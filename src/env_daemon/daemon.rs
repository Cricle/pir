//! The daemon half of `pir env-daemon`: the wire server, its operation set
//! (`exec`/`pty`/the file family/`listFiles`/`watch`/`scanLines`) and the
//! platform layer. Split out of `mod.rs` so the shared data plane
//! (`frame`/`decode`, used by `env_client`) and [`DAEMON_SUBCOMMAND`] stay core
//! while everything here compiles only under the `env-daemon` feature.
//!
//! [`DAEMON_SUBCOMMAND`]: super::DAEMON_SUBCOMMAND

use super::errors::Failure;
use super::exec::{self, Control};
use super::frame::{self, Frame, CANCEL, ERROR, PING, REQUEST, RESULT};
use super::output::Output;
use super::{fs, list, pty, scan, sys, watch};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::env;
use std::fs::{File, ReadDir};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const PROTOCOL: u64 = 1;
/// Operations added after protocol 1's first release; clients check `hello.features` before using them.
const FEATURES: &[&str] = &["listFiles"];
/// The version this daemon reports in `hello`. Upstream bakes the npm package
/// version in with a `build.rs`; pir reports its own crate version instead
/// (the protocol contract is `protocol: 1` + `features`, both unchanged).
const VERSION: &str = env!("CARGO_PKG_VERSION");
const PING_INTERVAL: Duration = Duration::from_secs(5);
const SILENCE_LIMIT_MS: u64 = 30_000;
const SCAN_CHUNK: usize = 64 * 1024;
/// Largest `readdir` page the daemon builds, whatever the client asks for.
/// The client's own `openDirReader` pages at 1000; a larger `max` would build
/// one JSON reply (with an `lstat` record per entry) large enough to risk the
/// 16 MiB frame cap, and would buffer the whole directory in memory on a
/// small device. Larger requests are served as consecutive pages (`done`
/// stays false), which is what the paged protocol asks for anyway.
const MAX_READDIR_PAGE: u64 = 1000;
/// Threads for file operations; commands run on their own threads.
///
/// The pool is sized from `available_parallelism()` and capped, because the
/// daemon also runs on 1-2 core / 512 MB mini devices: the upstream constant
/// (16) parked 16 idle threads (16 x 2 MiB of reserved stack) for a pool that
/// never needs more than a handful of concurrent file ops.
/// [`WORKERS_ENV`] overrides the decision (1..=64) for diagnostics and
/// embedded use.
const MIN_WORKERS: usize = 2;
const MAX_WORKERS: usize = 8;
/// Environment override for the file-op worker pool size.
const WORKERS_ENV: &str = "PIR_ENV_DAEMON_WORKERS";
/// Stack for a long-lived exec/pty request thread. The upstream 16 MiB was
/// sized for the recursive watch scan, which still gets it (see
/// [`request_stack_bytes`]); exec/pty only stream and decode, so the Rust
/// default 2 MiB is ample and each concurrent request reserves 8x less.
const REQUEST_STACK_BYTES: usize = 2 * 1024 * 1024;
/// The recursive watch scan walks the tree on its request thread, so it keeps
/// the upstream stack headroom (a deep session tree is bounded by
/// `maxDirectories`, but the per-directory frame is not tiny).
const WATCH_STACK_BYTES: usize = 16 * 1024 * 1024;
/// Open handles per connection, beyond which opening fails with `EMFILE`.
const MAX_HANDLES: usize = 4096;
/// Replies with larger payloads queue behind command output instead of going first.
const CONTROL_PAYLOAD: usize = 64 * 1024;
/// Bound on the file-operation job queue. The reader thread produces these
/// jobs; past this depth the queue (not the process's memory) applies
/// backpressure to the client, exactly like a full pipe.
const FILE_JOB_QUEUE_DEPTH: usize = 1024;
/// Long-running requests (`exec`/`watch`/`pty`) that may run at once. Each
/// owns a thread and possibly a child process tree; a client that asks for
/// more gets a structured `EAGAIN` instead of spawning without limit.
const DEFAULT_MAX_LONG_REQUESTS: usize = 32;
/// Environment override for [`DEFAULT_MAX_LONG_REQUESTS`] (1..=256), for
/// embedded use and the admission tests.
const MAX_LONG_REQUESTS_ENV: &str = "PIR_ENV_DAEMON_MAX_LONG_REQUESTS";

struct FileHandle {
    file: File,
    path: String,
    /// A failed chunk write poisons a write handle, so later chunks cannot leave a gap.
    failed: AtomicBool,
    /// Chunk writes run one at a time in arrival order, though the client sends several at once.
    serial: Mutex<Serial>,
}

#[derive(Default)]
struct Serial {
    jobs: VecDeque<Job>,
    running: bool,
}

struct DirHandle {
    entries: ReadDir,
    path: String,
}

enum Handle {
    File(Arc<FileHandle>),
    Dir(Arc<Mutex<DirHandle>>),
}

type Job = Box<dyn FnOnce() + Send>;

struct Server {
    output: Arc<Output>,
    tmpdir: String,
    handles: Mutex<HashMap<u64, Handle>>,
    next_handle: AtomicU64,
    controls: Mutex<HashMap<u32, Arc<Control>>>,
    groups: exec::Groups,
    terminals: pty::Terminals,
    /// In-flight `exec`/`watch`/`pty` requests (admission for
    /// [`max_long_requests`]).
    long_requests: Arc<AtomicU64>,
}

fn field<'a>(json: &'a Value, key: &str) -> Result<&'a str, Failure> {
    json[key]
        .as_str()
        .ok_or_else(|| Failure::new("EINVAL", format!("missing field {key}")))
}

fn number(json: &Value, key: &str) -> Result<u64, Failure> {
    json[key]
        .as_u64()
        .ok_or_else(|| Failure::new("EINVAL", format!("missing or invalid field {key}")))
}

fn flag(json: &Value, key: &str) -> bool {
    json[key].as_bool().unwrap_or(false)
}

impl Server {
    fn file(&self, json: &Value) -> Result<Arc<FileHandle>, Failure> {
        match self
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&number(json, "handle")?)
        {
            Some(Handle::File(file)) => Ok(file.clone()),
            Some(Handle::Dir(_)) => Err(Failure::new("EBADF", "not a file handle")),
            None => Err(Failure::new("EBADF", "unknown handle")),
        }
    }

    fn insert(&self, handle: Handle) -> Result<u64, Failure> {
        let mut handles = self
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if handles.len() >= MAX_HANDLES {
            return Err(Failure::new("EMFILE", "EMFILE: too many open files"));
        }
        let id = self.next_handle.fetch_add(1, Ordering::Relaxed);
        handles.insert(id, handle);
        Ok(id)
    }

    fn insert_file(&self, file: File, path: &str) -> Result<u64, Failure> {
        self.insert(Handle::File(Arc::new(FileHandle {
            file,
            path: path.to_string(),
            failed: AtomicBool::new(false),
            serial: Mutex::new(Serial::default()),
        })))
    }

    fn dispatch(
        &self,
        id: u32,
        json: &Value,
        payload: Vec<u8>,
        control: &Control,
    ) -> Result<(Value, Vec<u8>), Failure> {
        if json.is_null() {
            return Err(Failure::new("EINVAL", "invalid JSON"));
        }
        let op = json["op"].as_str().unwrap_or_default();
        let plain = |value: Result<Value, Failure>| value.map(|value| (value, Vec::new()));
        match op {
            "hello" => plain(Ok(json!({
                "protocol": PROTOCOL,
                "version": VERSION,
                "os": env::consts::OS,
                "arch": env::consts::ARCH,
                "home": sys::home(),
                "separator": sys::PATH_SEPARATOR,
                "tmpdir": self.tmpdir,
                "cwd": sys::cwd(),
                "driveCwds": sys::drive_cwds(),
                "pid": std::process::id(),
                "features": FEATURES,
            }))),
            "lstat" => plain(fs::lstat(field(json, "path")?)),
            "realpath" => plain(fs::realpath(field(json, "path")?)),
            "write" => {
                let path = field(json, "path")?;
                let parents = json["parents"].as_bool().unwrap_or(true);
                let file = fs::write(path, flag(json, "append"), parents, &payload)?;
                if !flag(json, "keep") {
                    return plain(Ok(json!({})));
                }
                plain(Ok(json!({ "handle": self.insert_file(file, path)? })))
            }
            "writeChunk" => {
                let handle = self.file(json)?;
                if handle.failed.load(Ordering::SeqCst) {
                    return Err(Failure::new("EBADF", "an earlier write failed"));
                }
                (&handle.file).write_all(&payload).map_err(|error| {
                    handle.failed.store(true, Ordering::SeqCst);
                    Failure::io(&error, "write", &handle.path)
                })?;
                plain(Ok(json!({})))
            }
            "truncate" => plain(fs::truncate(field(json, "path")?, number(json, "size")?)),
            "fsync" => plain(fs::fsync(field(json, "path")?)),
            "rename" => plain(fs::rename(field(json, "path")?, field(json, "to")?)),
            "mkdir" => plain(fs::mkdir(field(json, "path")?, flag(json, "recursive"))),
            "rm" => plain(fs::rm(
                field(json, "path")?,
                flag(json, "recursive"),
                flag(json, "force"),
            )),
            "mkdtemp" => plain(fs::mkdtemp(field(json, "path")?)),
            "open" => {
                let path = field(json, "path")?;
                if json["mode"].as_str() == Some("read") {
                    // Node's `open(path, "r")`: any kind of file, blocking; reads report what the file is.
                    let file = sys::open(path, sys::OpenMode::Read)
                        .map_err(|error| Failure::io(&error, "open", path))?;
                    let stat = match file.metadata() {
                        Ok(metadata) => json!({ "stat": fs::info(path, &metadata) }),
                        Err(error) => {
                            json!({ "statError": Failure::io(&error, "fstat", path).to_json() })
                        }
                    };
                    let mut result = stat;
                    result["handle"] = json!(self.insert_file(file, path)?);
                    return plain(Ok(result));
                }
                let (file, info) = fs::open_reader(path, flag(json, "noFollow"))?;
                plain(Ok(
                    json!({ "handle": self.insert_file(file, path)?, "info": info }),
                ))
            }
            "pread" => {
                let handle = self.file(json)?;
                // A length larger than the address space would wrap when cast;
                // clamp before the conversion and again to the frame budget.
                let length = usize::try_from(number(json, "length")?)
                    .unwrap_or(usize::MAX)
                    .min(frame::MAX_PAYLOAD);
                let bytes = match json["offset"].as_u64() {
                    Some(offset) => fs::pread(&handle.file, &handle.path, offset, length)?,
                    None => fs::read(&handle.file, &handle.path, length)?,
                };
                Ok((json!({}), bytes))
            }
            "fstat" => {
                let handle = self.file(json)?;
                let metadata = handle
                    .file
                    .metadata()
                    .map_err(|error| Failure::io(&error, "fstat", &handle.path))?;
                plain(Ok(fs::info(&handle.path, &metadata)))
            }
            "scanLines" => {
                let handle = self.file(json)?;
                plain(scan_lines(&handle.file, &handle.path, json, control))
            }
            "opendir" => {
                let path = field(json, "path")?;
                let entries =
                    sys::read_dir(path).map_err(|error| Failure::io(&error, "opendir", path))?;
                let handle = self.insert(Handle::Dir(Arc::new(Mutex::new(DirHandle {
                    entries,
                    path: path.to_string(),
                }))))?;
                plain(Ok(json!({ "handle": handle })))
            }
            "readdir" => plain(self.read_dir(json)),
            "listFiles" => list::run(json, &|| control.aborted.load(Ordering::SeqCst)),
            "close" => {
                self.handles
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&number(json, "handle")?);
                plain(Ok(json!({})))
            }
            "exec" => plain(exec::run(
                id,
                exec::ExecRequest::from_json(json)?,
                &self.tmpdir,
                &self.output,
                control,
                &self.groups,
            )),
            "watch" => plain(watch::run(id, json, &self.output, control)),
            "pty" => plain(pty::run(
                id,
                pty::PtyRequest::from_json(json)?,
                &self.output,
                control,
                &self.groups,
                &self.terminals,
            )),
            _ => Err(Failure::new("EINVAL", format!("unknown operation {op}"))),
        }
    }

    fn read_dir(&self, json: &Value) -> Result<Value, Failure> {
        let max = number(json, "max")?.min(MAX_READDIR_PAGE);
        // `max: 0` is a valid page request for "nothing"; it must not consume
        // an entry (the earlier `max(1)` returned one entry instead of zero).
        if max == 0 {
            return Ok(json!({ "entries": [], "done": false }));
        }
        let max = max as usize;
        // Lock only this directory, so other requests are not held up by a slow listing.
        let dir = match self
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&number(json, "handle")?)
        {
            Some(Handle::Dir(dir)) => dir.clone(),
            _ => return Err(Failure::new("EBADF", "not a directory handle")),
        };
        let mut dir = dir
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut entries = Vec::new();
        let mut done = false;
        while entries.len() < max {
            match dir.entries.next() {
                None => {
                    done = true;
                    break;
                }
                Some(Err(error)) => return Err(Failure::io(&error, "readdir", &dir.path)),
                Some(Ok(entry)) => entries.push(fs::dir_entry(&dir.path, &entry.file_name())),
            }
        }
        Ok(json!({ "entries": entries, "done": done }))
    }
}

fn scan_lines(file: &File, path: &str, json: &Value, control: &Control) -> Result<Value, Failure> {
    let start_line = number(json, "startLine")?;
    let end_line = json["endLine"].as_u64();
    if end_line.is_some_and(|end| end <= start_line) {
        return Err(Failure::new("EINVAL", "Invalid line range"));
    }
    let mut scanner = scan::LineScanner::new(start_line, end_line);
    let mut buffer = vec![0u8; SCAN_CHUNK];
    let mut position = 0u64;
    loop {
        if control.aborted.load(Ordering::SeqCst) {
            return Err(Failure::new("aborted", "aborted"));
        }
        let read = sys::read_at(file, &mut buffer, position)
            .map_err(|error| Failure::io(&error, "read", path))?;
        if read == 0 {
            return Ok(scanner.finish());
        }
        scanner.push(&buffer[..read]);
        position += read as u64;
    }
}

fn kill_all(groups: &exec::Groups) {
    for pid in groups
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
    {
        exec::kill_group(*pid);
    }
}

/// Stdin that records when bytes last arrived, so a large frame arriving slowly counts as a live client.
struct Seen<R> {
    inner: R,
    last_seen: Arc<AtomicU64>,
}

impl<R: Read> Read for Seen<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read > 0 {
            self.last_seen
                .store(crate::ai::util::now_ms(), Ordering::Relaxed);
        }
        Ok(read)
    }
}

fn start_workers() -> SyncSender<Job> {
    // Bounded: the reader thread applies backpressure instead of queueing
    // file jobs without limit when the client floods the daemon.
    let (jobs, queue) = mpsc::sync_channel::<Job>(FILE_JOB_QUEUE_DEPTH);
    let queue = Arc::new(Mutex::new(queue));
    for _ in 0..daemon_worker_count() {
        let queue: Arc<Mutex<Receiver<Job>>> = queue.clone();
        thread::spawn(move || loop {
            let job = queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recv();
            match job {
                Ok(job) => job(),
                Err(_) => return,
            }
        });
    }
    jobs
}

/// The file-op worker-pool size for this process: [`WORKERS_ENV`] when it
/// parses, else the detected parallelism clamped to
/// `[MIN_WORKERS, MAX_WORKERS]`.
///
/// Pure decision seam (see [`daemon_worker_count_from`]) so the tests can pin
/// the policy without setting process environment.
#[doc(hidden)]
pub fn daemon_worker_count() -> usize {
    daemon_worker_count_from(
        std::env::var(WORKERS_ENV).ok().as_deref(),
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1),
    )
}

/// The policy behind [`daemon_worker_count`]: an explicit `override_value`
/// (1..=64; unparseable/zero values fall back to the adaptive decision),
/// otherwise the detected parallelism clamped to the pool bounds.
///
/// Whether the pool is 1 or 8 never changes semantics: `writeChunk` order per
/// handle is serialized by `run_in_order` and pty writes are answered on the
/// reader thread in arrival order, so a smaller pool only reduces concurrency.
#[doc(hidden)]
pub fn daemon_worker_count_from(override_value: Option<&str>, parallelism: usize) -> usize {
    if let Some(count) = override_value
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|count| *count > 0)
    {
        return count.clamp(1, 64);
    }
    parallelism.clamp(MIN_WORKERS, MAX_WORKERS)
}

/// Stack bytes for the dedicated thread of one long-lived request `op`.
///
/// The recursive watch scan keeps the upstream 16 MiB headroom; exec and pty
/// only stream/decode on their own thread, so they run on the Rust default
/// stack instead of reserving 16 MiB per concurrent request (the per-request
/// cost that dominated a small device's address space).
#[doc(hidden)]
pub fn request_stack_bytes(op: &str) -> usize {
    if op == "watch" {
        WATCH_STACK_BYTES
    } else {
        REQUEST_STACK_BYTES
    }
}

/// The concurrent `exec`/`watch`/`pty` admission cap for this process
/// ([`MAX_LONG_REQUESTS_ENV`] when it parses, else
/// [`DEFAULT_MAX_LONG_REQUESTS`]).
#[doc(hidden)]
pub fn max_long_requests() -> usize {
    max_long_requests_from(std::env::var(MAX_LONG_REQUESTS_ENV).ok().as_deref())
}

/// The policy behind [`max_long_requests`]: an override in 1..=256, otherwise
/// the default.
#[doc(hidden)]
pub fn max_long_requests_from(override_value: Option<&str>) -> usize {
    override_value
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|count| *count > 0)
        .map(|count| count.clamp(1, 256))
        .unwrap_or(DEFAULT_MAX_LONG_REQUESTS)
}

/// One admitted long request: releases its slot when the request thread ends
/// (also when it panics, thanks to `Drop`).
struct LongRequestSlot(Arc<AtomicU64>);

impl Drop for LongRequestSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Server {
    /// Admit one `exec`/`watch`/`pty` request, or `None` past the cap.
    fn try_begin_long_request(&self) -> Option<LongRequestSlot> {
        let previous = self.long_requests.fetch_add(1, Ordering::SeqCst);
        if previous as usize >= max_long_requests() {
            self.long_requests.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(LongRequestSlot(Arc::clone(&self.long_requests)))
    }
}

/// Never set: bulk replies park until the writer drains, and `Output::close`
/// (EOF or a failed write) is what releases them. Kept as a value because
/// [`Output::wait_for_room`] takes the request's own stop flag for exec/pty.
static BULK_NEVER_STOP: AtomicBool = AtomicBool::new(false);

fn respond(server: &Server, id: u32, request: Frame, control: Arc<Control>) {
    let is_exec = matches!(request.json["op"].as_str(), Some("exec" | "pty"));
    let reply = match server.dispatch(id, &request.json, request.payload, &control) {
        Ok((json, payload)) => Frame::with_payload(RESULT, id, json, payload),
        Err(failure) => Frame::new(ERROR, id, failure.to_json()),
    };
    server
        .controls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&id);
    // A command's result follows its output; large payloads do not hold up pings and small replies.
    if is_exec || reply.payload.len() > CONTROL_PAYLOAD {
        // The bulk lane's cap is a promise to every producer, not only to
        // command readers: a client that issues large `pread`/`listFiles`
        // replies without draining its stdout must park the worker pool here
        // instead of growing the unsent queue by one frame per request.
        server.output.wait_for_room(&BULK_NEVER_STOP);
        server.output.bulk(reply, None);
    } else {
        server.output.control(reply);
    }
}

/// Queue `job` behind the handle's earlier chunk writes; one pool job drains the queue.
fn run_in_order(jobs: &SyncSender<Job>, handle: Arc<FileHandle>, job: Job) {
    let mut serial = handle
        .serial
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    serial.jobs.push_back(job);
    if serial.running {
        return;
    }
    serial.running = true;
    drop(serial);
    let _ = jobs.send(Box::new(move || loop {
        let next = {
            let mut serial = handle
                .serial
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let next = serial.jobs.pop_front();
            if next.is_none() {
                serial.running = false;
            }
            next
        };
        match next {
            Some(job) => job(),
            None => return,
        }
    }));
}

fn serve_frames(server: &Arc<Server>, input: &mut impl Read) -> io::Result<()> {
    let jobs = start_workers();
    while let Some(request) = frame::read_frame(input)? {
        match request.kind {
            REQUEST
                if matches!(
                    request.json["op"].as_str(),
                    Some("ptyWrite" | "ptyResize" | "ptyForeground")
                ) =>
            {
                // Answered here, in arrival order: through the worker pool, two writes could swap.
                let reply = match request.json["op"].as_str() {
                    Some("ptyWrite") => {
                        pty::write(&server.terminals, &request.json, request.payload)
                    }
                    Some("ptyResize") => pty::resize(&server.terminals, &request.json),
                    _ => pty::foreground(&server.terminals, &request.json),
                };
                server.output.control(match reply {
                    Ok(json) => Frame::new(RESULT, request.id, json),
                    Err(failure) => Frame::new(ERROR, request.id, failure.to_json()),
                });
            }
            REQUEST => {
                let id = request.id;
                // Registered before the request runs, so a cancel right behind it is not lost.
                let control = Arc::new(Control::default());
                server
                    .controls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(id, control.clone());
                // Commands and watchers last; they get their own threads (watchers recurse through trees).
                if matches!(request.json["op"].as_str(), Some("exec" | "watch" | "pty")) {
                    let op = request.json["op"].as_str().unwrap_or_default();
                    // Admission: each long request owns a thread and possibly
                    // a process tree. Past the cap the client gets a
                    // structured `EAGAIN` (never a silently dropped request).
                    let Some(slot) = server.try_begin_long_request() else {
                        server
                            .controls
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&id);
                        let failure = Failure::new(
                            "EAGAIN",
                            format!(
                                "EAGAIN: too many concurrent exec/watch/pty requests (limit {})",
                                max_long_requests()
                            ),
                        );
                        server
                            .output
                            .control(Frame::new(ERROR, id, failure.to_json()));
                        continue;
                    };
                    let worker = server.clone();
                    let spawned = thread::Builder::new()
                        .stack_size(request_stack_bytes(op))
                        .spawn(move || {
                            let _slot = slot;
                            respond(&worker, id, request, control)
                        });
                    if let Err(error) = spawned {
                        server
                            .controls
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&id);
                        let failure = Failure::new("spawn_error", error.to_string());
                        server
                            .output
                            .bulk(Frame::new(ERROR, id, failure.to_json()), None);
                    }
                } else {
                    let worker = server.clone();
                    let serial = match request.json["op"].as_str() {
                        Some("writeChunk") => server.file(&request.json).ok(),
                        _ => None,
                    };
                    let job: Job = Box::new(move || respond(&worker, id, request, control));
                    match serial {
                        Some(handle) => run_in_order(&jobs, handle, job),
                        None => {
                            let _ = jobs.send(job);
                        }
                    }
                }
            }
            CANCEL => {
                let control = server
                    .controls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&request.id)
                    .cloned();
                if let Some(control) = control {
                    control.cancel(request.json["mode"].as_str() == Some("kill"));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Why a `serve` run could not start; every failure prints one diagnostic line
/// on stderr and exits non-zero, as upstream does.
pub enum ServeError {
    /// Missing `--token <hex>`: usage error, exit 2.
    Usage,
    /// The stdio stream failed: exit 1.
    Io(io::Error),
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServeError::Usage => formatter.write_str("usage: pir env-daemon serve --token <hex>"),
            ServeError::Io(error) => write!(formatter, "pir env-daemon: {error}"),
        }
    }
}

/// `pir env-daemon [serve --token <hex> | --version]` — the process entry, with
/// upstream's exit codes (0 on clean end, 1 on an I/O failure, 2 on usage).
/// The caller (`main_cli/run_mode.rs`) turns this into a process exit.
pub fn run_main(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("serve") => {
            let token = args
                .iter()
                .position(|arg| arg == "--token")
                .and_then(|index| args.get(index + 1));
            let Some(token) = token else {
                eprintln!("{}", ServeError::Usage);
                return 2;
            };
            if let Err(error) = serve(token) {
                eprintln!("{error}");
                return 1;
            }
            // Request threads may still be blocked (a FIFO open); nothing is left to answer them.
            0
        }
        Some("--version") => {
            println!("pi-env {VERSION}");
            0
        }
        _ => {
            eprintln!("{}", ServeError::Usage);
            2
        }
    }
}

/// Serve protocol v1 on stdin/stdout until stdin ends (upstream `serve`).
///
/// # Errors
///
/// The first sync line or a frame I/O failure (`io::Error`); a client that
/// vanishes ends the loop cleanly instead.
pub fn serve(token: &str) -> io::Result<()> {
    {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "PI-ENV {token}")?;
        stdout.flush()?;
    }
    let output = Arc::new(Output::default());
    let writer = {
        let output = output.clone();
        thread::spawn(move || {
            output.run(&mut BufWriter::with_capacity(1 << 20, io::stdout().lock()))
        })
    };
    let server = Arc::new(Server {
        output: output.clone(),
        tmpdir: sys::tmpdir(),
        handles: Mutex::new(HashMap::new()),
        next_handle: AtomicU64::new(1),
        controls: Mutex::new(HashMap::new()),
        groups: Arc::new(Mutex::new(Default::default())),
        terminals: Arc::new(Mutex::new(HashMap::new())),
        long_requests: Arc::new(AtomicU64::new(0)),
    });
    let last_seen = Arc::new(AtomicU64::new(crate::ai::util::now_ms()));
    {
        let output = output.clone();
        let last_seen = last_seen.clone();
        let groups = server.groups.clone();
        thread::spawn(move || {
            loop {
                thread::sleep(PING_INTERVAL);
                output.control(Frame::new(PING, 0, json!({})));
                // A client that went silent (a phone that lost its network) leaves nothing running.
                if crate::ai::util::now_ms().saturating_sub(last_seen.load(Ordering::Relaxed))
                    > SILENCE_LIMIT_MS
                {
                    kill_all(&groups);
                    std::process::exit(0);
                }
            }
        });
    }
    let mut input = BufReader::with_capacity(
        1 << 20,
        Seen {
            inner: io::stdin().lock(),
            last_seen,
        },
    );
    let result = serve_frames(&server, &mut input);
    // However input ended, the client is gone: stop everything it started.
    // Cancelling the registered controls (not only killing process groups)
    // also ends watchers, which own no child process and would otherwise keep
    // scanning after the connection is gone.
    let controls: Vec<Arc<Control>> = server
        .controls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .cloned()
        .collect();
    for control in controls {
        control.cancel(false);
    }
    kill_all(&server.groups);
    output.close();
    let _ = writer.join();
    result
}
