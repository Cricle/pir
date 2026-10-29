//! Host file-watch backend for `fs.watch`: a fourth generic entry
//! `__pir_host.fsWatch(op, payloadJson) -> String` with the same
//! `{ok,value}` / `{ok:false,code,message}` JSON envelope as the other host
//! tables (see [`super::http_fns`] for the pattern).
//!
//! Implementation: a detached thread per watcher keeps a persistent
//! mark-and-sweep tree ([`WatchTree`]) across ticks: each sweep walks the
//! watched path (single file, or a directory walk — recursive when the option
//! asks), stamps a generation on every path it visits and compares
//! `(mtime, size)` against the stored entry, so an unchanged tree costs no
//! per-entry allocation and no second full snapshot. The emitted event set is
//! exactly the set the previous snapshot-then-diff produced:
//! appearances/disappearances report as `rename`, mtime/size drift reports as
//! `change` (Node's two event types). Filenames arrive JS-side relative to
//! the watch root, `null` when unknown (documented Node behavior). JS drains
//! the queue through `watchPoll` on a timer pump, like the fetch/server
//! bridges.
//!
//! Scheduling: the `extensions` feature now enables the native `notify`
//! backend (as `tui`/`env` already did), so the thread waits on the OS watcher
//! and sweeps only when it reports a change; an idle tree is not re-stat'd
//! five times a second. A quiet fallback sweep still runs every
//! [`WATCH_POLL_IDLE`] to catch events a platform watcher lost, and if the
//! watcher cannot be created the fallback poll drives the watcher, starting at
//! [`WATCH_POLL_FAST`] and backing off to [`WATCH_POLL_IDLE`] after
//! [`WATCH_IDLE_SWEEPS_BEFORE_BACKOFF`] quiet sweeps; any observed change
//! resets it to the fast cadence.
//!
//! Permissions: read-only over the fs surface — the watch roots are the
//! extension's own read surface (reads are globally allowed by the fs
//! permission model), so no new fence is installed.
//!
//! Lifecycle: a watcher's thread ends on `watchStop` (or a JS `close()`), on
//! the owning runtime's generation release (`/reload` / shutdown: every watcher
//! the generation started is stopped and unregistered), and when the runtime
//! that started it is dropped — the handle only holds a `Weak` to the shared
//! `RuntimeState`, so the thread also reaps itself once that owner is gone and
//! the thread can never outlive its runtime. A watcher that
//! is alive but not polled (a JS event loop blocked in long synchronous work,
//! e.g. a multi-minute `execFileSync` scan) is NOT stale: `fs.watch` in Node
//! keeps reporting changes no matter how long the poller was blocked, so
//! reaping on poll silence would silently kill a live watcher.
//!
//! Queue bound: the event backlog is bounded at [`MAX_WATCH_EVENTS`] and
//! overflow drops the OLDEST notifications — see that constant for why the
//! bound and the drop are Node-shaped here (a watcher is a change signal, not
//! a transaction log; Node's watcher coalesces/elides unread notifications).
//! Every drop is counted on the handle, reported through `ext_diag!` on the
//! [`WATCH_DROP_REPORT_EVERY`] cadence, and the running total rides every
//! `watchPoll` answer as `dropped` — never silent, never unbounded.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, PoisonError, Weak};
use std::time::{Duration, SystemTime};

use rquickjs::prelude::Func;
use rquickjs::Object;
use serde_json::Value as JsonValue;

use super::*;

/// Fast fallback cadence of the watch threads: how often the tree is diffed
/// while changes are being observed (and the initial cadence before a quiet
/// run backs off).
const WATCH_POLL_FAST: Duration = Duration::from_millis(200);

/// Backed-off cadence after [`WATCH_IDLE_SWEEPS_BEFORE_BACKOFF`] sweeps with
/// no change: an idle watcher costs one walk every two seconds instead of
/// five per second. With the native backend this is also the safety-net
/// sweep cadence (native events sweep immediately).
const WATCH_POLL_IDLE: Duration = Duration::from_secs(2);

/// Quiet sweeps (one second at [`WATCH_POLL_FAST`]) before backing off.
const WATCH_IDLE_SWEEPS_BEFORE_BACKOFF: u32 = 5;

/// The fallback cadence for a run of `idle_sweeps` quiet sweeps (a pure
/// function so the backoff shape is testable; `watchStats` reports the live
/// value through the host bridge).
fn watch_poll_interval(idle_sweeps: u32) -> Duration {
    if idle_sweeps >= WATCH_IDLE_SWEEPS_BEFORE_BACKOFF {
        WATCH_POLL_IDLE
    } else {
        WATCH_POLL_FAST
    }
}

/// Bounded event backlog per watcher: the JS pump drains every 30 ms tick, so
/// 1024 events is ~30 s of a 30-events/tick stream — enough headroom for a JS
/// event loop blocked in long synchronous work (`fs.watch` keeps reporting
/// while the poller is blocked, see the module header) without letting a
/// stalled consumer grow the queue. Overflow drops the OLDEST events: a
/// watcher's notifications are a "what changed lately" signal, not a
/// transaction log, and Node's own watcher coalesces/elides notifications
/// (one `rename` per path per tick, no replay of an unread backlog), so
/// dropping the oldest is the closest Node-shaped degradation. The drop is
/// never silent: [`FsWatchHandle::dropped`] counts it, [`push_event`] reports
/// it through `ext_diag!` on a bounded cadence, and every `watchPoll` answer
/// carries the running total.
const MAX_WATCH_EVENTS: usize = 1024;

/// Drop-report cadence: the first dropped event is reported, then one
/// diagnostic per this many further drops (a busy directory under a stalled
/// poller must not turn the log into the new unbounded queue).
const WATCH_DROP_REPORT_EVERY: u64 = 1024;

/// Hard cap on live `fs.watch` watcher threads for the whole process. Each
/// watcher owns an OS thread; a JS loop calling `fs.watch` without closing
/// would otherwise spawn threads without bound. 64 concurrent watched roots
/// is well above realistic editor/agent use.
const MAX_WATCHERS: usize = 64;

/// One queued filesystem event: Node's `eventType` plus the relative filename
/// (`None` when unknown).
struct WatchEvent {
    kind: &'static str,
    filename: Option<String>,
}

struct FsWatchHandle {
    root: PathBuf,
    recursive: bool,
    /// Basename reported for single-file watches (the root is not a dir).
    file_name: Option<String>,
    /// Owning runtime generation (see [`release_generation`]): a reload REUSES
    /// the shared `RuntimeState`, so the weak-owner reap below cannot see the
    /// old generation's death; the generation release stops it explicitly.
    generation: u64,
    events: StdMutex<VecDeque<WatchEvent>>,
    /// Cumulative events dropped because `events` was at [`MAX_WATCH_EVENTS`]
    /// (observability, and the `dropped` field of every `watchPoll` answer).
    dropped: AtomicU64,
    closed: StdMutex<bool>,
    /// The runtime that started this watcher. The thread reaps itself once the
    /// last strong reference is gone (see the module header).
    owner: Weak<StdMutex<RuntimeState>>,
    reaped: AtomicBool,
    /// Diagnostic counters (`watchStats`, and the test seam for the adaptive
    /// cadence): sweeps performed, filesystem stats performed by them, and
    /// the interval the thread is currently sleeping/blocking for.
    sweeps: AtomicU64,
    stat_calls: AtomicU64,
    poll_interval_ms: AtomicU64,
}

static WATCHERS: OnceLock<StdMutex<HashMap<u64, Arc<FsWatchHandle>>>> = OnceLock::new();
static NEXT_WATCH_ID: AtomicU64 = AtomicU64::new(1);

fn watchers() -> &'static StdMutex<HashMap<u64, Arc<FsWatchHandle>>> {
    WATCHERS.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Install `__pir_host.fsWatch(op, payloadJson)` on the shared `__pir_host`
/// object. The registration captures only a `Weak` handle on the runtime
/// state: a watcher never keeps its runtime alive. `generation` scopes the
/// watcher table for the runtime teardown/reload release.
pub(super) fn register_fs_watch_host_fns(
    host: &Object,
    state: &Arc<StdMutex<RuntimeState>>,
    generation: u64,
) -> rquickjs::Result<()> {
    let owner = Arc::downgrade(state);
    host.set(
        "fsWatch",
        Func::from(move |op: String, payload: String| -> String {
            fs_watch_call(&op, &payload, &owner, generation)
        }),
    )?;
    Ok(())
}

fn fs_watch_call(
    op: &str,
    payload: &str,
    owner: &Weak<StdMutex<RuntimeState>>,
    generation: u64,
) -> String {
    let payload: JsonValue = serde_json::from_str(payload).unwrap_or(JsonValue::Null);
    let result = match op {
        "watchStart" => watch_start(&payload, owner, generation),
        "watchPoll" => watch_poll(&payload),
        "watchStop" => watch_stop(&payload),
        // Diagnostic only: never part of the Node `fs.watch` surface. Exposes
        // the sweep/stat counters and the live fallback interval so the
        // adaptive-cadence behaviour is observable (tests use it as the
        // counter seam).
        "watchStats" => watch_stats(&payload),
        other => Err(FnError::new(
            "ENOSYS",
            format!("unknown fsWatch op `{other}`"),
        )),
    };
    match result {
        // `Display for Value` is the compact serialization (`to_writer`), the
        // same bytes `to_string` produces, so the op's own value is written
        // straight into the envelope text. `json!({ "ok": true, "value": v })`
        // would instead allocate the envelope `Map` plus its two key strings
        // AND rebuild the value through `to_value(&v)` — for `watchPoll`, a
        // full recursive copy of the drained batch.
        Ok(value) => format!(r#"{{"ok":true,"value":{value}}}"#),
        Err(error) => error.to_json().to_string(),
    }
}

struct FnError {
    code: &'static str,
    message: String,
}

impl FnError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn to_json(&self) -> JsonValue {
        serde_json::json!({
            "ok": false,
            "code": self.code,
            "message": self.message,
        })
    }
}

fn payload_id(payload: &JsonValue) -> Result<u64, FnError> {
    payload
        .get("id")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| FnError::new("EINVAL", "missing field `id`"))
}

fn watch_start(
    payload: &JsonValue,
    owner: &Weak<StdMutex<RuntimeState>>,
    generation: u64,
) -> Result<JsonValue, FnError> {
    if watchers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len()
        >= MAX_WATCHERS
    {
        return Err(FnError::new(
            "EMFILE",
            format!("too many fs.watch watchers: the process limit is {MAX_WATCHERS}"),
        ));
    }
    let path = payload
        .get("path")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| FnError::new("EINVAL", "missing field `path`"))?;
    let recursive = payload
        .get("recursive")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);
    let root = PathBuf::from(path);
    let file_name = if root.is_dir() {
        None
    } else {
        root.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    };
    let handle = Arc::new(FsWatchHandle {
        root,
        recursive,
        file_name,
        generation,
        events: StdMutex::new(VecDeque::new()),
        dropped: AtomicU64::new(0),
        closed: StdMutex::new(false),
        owner: owner.clone(),
        reaped: AtomicBool::new(false),
        sweeps: AtomicU64::new(0),
        stat_calls: AtomicU64::new(0),
        poll_interval_ms: AtomicU64::new(0),
    });
    let id = NEXT_WATCH_ID.fetch_add(1, Ordering::SeqCst);
    watchers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, Arc::clone(&handle));
    let thread_handle = Arc::clone(&handle);
    std::thread::Builder::new()
        .name(format!("pir-fs-watch-{id}"))
        .spawn(move || watch_thread(thread_handle))
        .map_err(|error| FnError::new("EIO", format!("watch thread spawn failed: {error}")))?;
    Ok(serde_json::json!({ "id": id }))
}

/// One entry of the persistent watch tree: the `(mtime nanos, byte length)`
/// pair the old snapshot held, plus the sweep generation that last saw it.
struct Entry {
    mtime: u128,
    len: u64,
    /// Mark bit: generation stamp of the sweep that last visited this path.
    /// An entry still holding an older stamp at the end of a sweep is gone.
    seen: u32,
}

/// One reported change: Node's event kind plus the root-relative filename
/// (`None` for the root-level `rename` of the absent state).
type SweepEvent = (&'static str, Option<String>);

/// The root-level event of a sweep that could not read the watched path (or
/// could no longer, after a success): Node reports `rename` with a null
/// filename.
const ROOT_RENAME: SweepEvent = ("rename", None);

/// The persistent mark-and-sweep watch tree: one map that survives across
/// ticks, swept per poll instead of rebuilt. A sweep walks the root, stamps
/// `seen = gen` on every path it visits and compares the stored `(mtime,
/// len)`; a hit allocates nothing (the relative path is looked up as `&str`
/// out of one reused buffer), a miss boxes the path once, and a path the sweep
/// did not visit is swept out as disappeared with its key's own allocation
/// moving into the `rename` event (no copy). Directories are stored as
/// `(0, 0)`, so a directory whose mtime moves never reports a `change` (the
/// old snapshot stored exactly that). The reported event multiset is the one
/// the previous snapshot-then-diff produced; only the order differs (walk
/// order here, hash order there — never a contract).
struct WatchTree {
    /// Root-relative path (backslash-separated: the bytes JS receives as
    /// `filename`) → entry.
    entries: HashMap<Box<str>, Entry>,
    /// Generation stamp of the current sweep, bumped once per sweep. Wrapping
    /// is fine: a mark is only ever compared against the current sweep, and
    /// 2^32 sweeps is decades even at the fast cadence.
    gen: u32,
    /// The last sweep failed to read the root, so `entries` is stale: the
    /// next successful sweep reports only the root `rename` (null) — the old
    /// loop's `(None, Some(_))` arm — and re-baselines silently.
    absent: bool,
    /// The one reusable path buffer: the walk pushes a name, looks the `&str`
    /// up, and truncates on unwind — one live allocation for the whole tree
    /// instead of one `String` per entry per tick.
    path: String,
}

impl WatchTree {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            gen: 0,
            absent: false,
            path: String::new(),
        }
    }

    /// One sweep over `root`, reporting the changes since the previous sweep.
    /// `root` is the watched path itself: a file is the single entry `""`
    /// (exactly the old snapshot's shape), a directory is walked (recursively
    /// when the option asks, depth-bounded). A root that cannot be read
    /// reports only the absent-state transition and keeps the tree.
    fn refresh(&mut self, root: &Path, recursive: bool, stat_calls: &AtomicU64) -> Vec<SweepEvent> {
        // The previous sweep failed: this one re-baselines, so its per-entry
        // changes are not events (the old loop skipped the diff entirely on
        // its `(None, Some(_))` arm).
        let rebuild = self.absent;
        let mut events = Vec::new();
        self.gen = self.gen.wrapping_add(1);
        if !self.sweep(root, recursive, &mut events, stat_calls) {
            self.path.clear();
            if self.absent {
                // Still unreadable: the old loop's `(None, None)` arm.
                return Vec::new();
            }
            // The root disappeared (or became unreadable): the old loop's
            // `(Some(_), None)` arm.
            self.absent = true;
            return vec![ROOT_RENAME];
        }
        self.absent = false;
        if rebuild {
            // Drop what did not come back, so a later re-appearance reports a
            // fresh `rename` instead of matching a stale `(mtime, len)` (which
            // would silently swallow the event).
            let gen = self.gen;
            self.entries.retain(|_, entry| entry.seen == gen);
            self.shrink_sparse();
            return vec![ROOT_RENAME];
        }
        // Sweep out the paths this walk did not visit: they disappeared.
        // `extract_if` hands the stale key over by value, so the path's own
        // `Box<str>` allocation becomes the event's `String` (`Box<str>` →
        // `String` reuses the buffer, while the old `retain` + `to_string`
        // copied the bytes into a fresh allocation and then freed the key's);
        // the append happens in the same place, after the walk's events. The
        // predicate is `retain`'s and the removals are appended in the same
        // place, so the removal set (the contract; the order never was one) is
        // unchanged.
        let gen = self.gen;
        events.extend(
            self.entries
                .extract_if(|_, entry| entry.seen != gen)
                .map(|(name, _)| ("rename", Some(String::from(name)))),
        );
        self.shrink_sparse();
        events
    }

    /// Release the table's excess capacity after a sweep that removed entries.
    /// `retain`/`extract_if` never shrink the table, so a tree that shrank by
    /// orders of magnitude would keep its peak capacity (tens of MB at a
    /// million paths) for the watcher's lifetime. The 4× guard self-limits:
    /// after the shrink `capacity ≈ len`, so it re-arms only after another
    /// mass removal. Capacity is unobservable, so this is behavior-neutral.
    fn shrink_sparse(&mut self) {
        if self.entries.capacity() / 4 > self.entries.len() {
            self.entries.shrink_to_fit();
        }
    }

    /// Walk `root` into the tree under the current generation, reporting each
    /// new/changed entry. Returns whether the root could be read at all.
    fn sweep(
        &mut self,
        root: &Path,
        recursive: bool,
        events: &mut Vec<SweepEvent>,
        stat_calls: &AtomicU64,
    ) -> bool {
        stat_calls.fetch_add(1, Ordering::Relaxed);
        if root.is_file() {
            let Ok(meta) = std::fs::metadata(root) else {
                return false;
            };
            // The single-file watch's one entry used the empty relative name,
            // and unlike a walked entry it reported absent when mtime was
            // unavailable (the old snapshot's `?`).
            let Some(mtime) = entry_mtime(&meta) else {
                return false;
            };
            self.path.clear();
            self.mark(mtime, meta.len(), events);
            return true;
        }
        let Ok(entries) = std::fs::read_dir(root) else {
            return false;
        };
        self.path.clear();
        self.sweep_dir(entries, recursive, 0, events, stat_calls);
        true
    }

    /// The recursive walk (the old `walk_dir`: same depth bound, same silent
    /// skips of unreadable entries). `self.path` carries the root-relative
    /// key and is truncated back to `base` on unwind.
    fn sweep_dir(
        &mut self,
        entries: std::fs::ReadDir,
        recursive: bool,
        depth: usize,
        events: &mut Vec<SweepEvent>,
        stat_calls: &AtomicU64,
    ) {
        if depth > 16 {
            return; // bounded walk: pathological trees cannot hang the poll
        }
        let base = self.path.len();
        for entry in entries.flatten() {
            stat_calls.fetch_add(1, Ordering::Relaxed);
            let Ok(meta) = entry.metadata() else { continue };
            let raw_name = entry.file_name();
            let raw_name = raw_name.to_string_lossy();
            self.path.truncate(base);
            self.path.push_str(&raw_name);
            if meta.is_dir() {
                self.mark(0, 0, events);
                if recursive {
                    if let Ok(inner) = std::fs::read_dir(entry.path()) {
                        self.path.push('\\');
                        self.sweep_dir(inner, recursive, depth + 1, events, stat_calls);
                    }
                }
                continue;
            }
            // A walked entry that cannot report mtime was `(0, len)` in the
            // old snapshot (`unwrap_or(0)`), not absent.
            let mtime = entry_mtime(&meta).unwrap_or(0);
            self.mark(mtime, meta.len(), events);
        }
        self.path.truncate(base);
    }

    /// Stamp one visited path: an existing key is updated in place (the
    /// `(mtime, len)` comparison is the old diff's `prior != current`), a new
    /// key is boxed once, and the event it implies is pushed.
    fn mark(&mut self, mtime: u128, len: u64, events: &mut Vec<SweepEvent>) {
        let gen = self.gen;
        let stamped = match self.entries.get_mut(self.path.as_str()) {
            Some(entry) => {
                let changed = entry.mtime != mtime || entry.len != len;
                entry.mtime = mtime;
                entry.len = len;
                entry.seen = gen;
                Some(changed)
            }
            None => None,
        };
        match stamped {
            Some(true) => events.push(("change", Some(self.path.clone()))),
            Some(false) => {}
            // First sight of this path: the old diff's
            // `previous.get(name) == None` arm.
            None => {
                self.entries.insert(
                    Box::from(self.path.as_str()),
                    Entry {
                        mtime,
                        len,
                        seen: gen,
                    },
                );
                events.push(("rename", Some(self.path.clone())));
            }
        }
    }
}

/// An entry's mtime as nanoseconds since the Unix epoch, `None` when the
/// platform cannot report it (the old snapshot's
/// `modified().ok()?.duration_since(UNIX_EPOCH).ok()?`).
fn entry_mtime(meta: &std::fs::Metadata) -> Option<u128> {
    meta.modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos())
}

/// The native change signal. `notify` is guaranteed here: this module is only
/// compiled with the `extensions` feature, which enables `dep:notify`.
///
/// The watcher is only a *wake signal*: the sweep below is still the diff
/// engine, so the delivered event set is unchanged. Notifications are
/// coalesced to one pending wake-up (a bounded sync channel; a burst cannot
/// grow memory), so a flood of OS events costs one sweep, not one per event.
struct ChangeSignal {
    _watcher: notify::RecommendedWatcher,
    rx: std::sync::mpsc::Receiver<()>,
}

impl ChangeSignal {
    fn start(root: &Path, recursive: bool) -> Option<Self> {
        use notify::Watcher as _;

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let mut watcher =
            notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
                // The sweep itself calls read_dir on the watched directory.
                // inotify reports that read as `Access(Read)` on the watched
                // root, which would wake the next sweep immediately and turn
                // the fallback into a busy loop (Linux CI: ~2.5k sweeps/s).
                // Only changes are wake signals; errors still mean "sweep
                // now" (the diff is the source of truth).
                if matches!(
                    &event,
                    Ok(event) if matches!(event.kind, notify::EventKind::Access(_))
                ) {
                    return;
                }
                let _ = tx.try_send(());
            })
            .ok()?;
        let mode = if recursive {
            notify::RecursiveMode::Recursive
        } else {
            notify::RecursiveMode::NonRecursive
        };
        watcher.watch(root, mode).ok()?;
        Some(Self {
            _watcher: watcher,
            rx,
        })
    }

    /// Wait up to `timeout` for a native event (all already-queued events are
    /// coalesced into this one wake-up). `true` when an event arrived.
    fn wait(&self, timeout: Duration) -> bool {
        match self.rx.recv_timeout(timeout) {
            Ok(()) => {
                while self.rx.try_recv().is_ok() {}
                true
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(timeout);
                false
            }
        }
    }
}

/// The watch loop: a native event wakes an immediate mark-and-sweep pass; the
/// adaptive fallback interval bounds how long an event the platform watcher
/// missed can stay unseen, and drives the watcher when no native backend is
/// compiled in.
fn watch_thread(handle: Arc<FsWatchHandle>) {
    let mut tree = WatchTree::new();
    // Register the native watch BEFORE the baseline sweep, so a change that
    // lands between the two is queued and swept immediately (the baseline
    // could otherwise absorb it without an event).
    let signal = ChangeSignal::start(&handle.root, handle.recursive);
    // The first sweep is the baseline only, exactly like the old loop's
    // `previous = snapshot(..)`: whatever it reports is not an event.
    tree.refresh(&handle.root, handle.recursive, &handle.stat_calls);
    handle.sweeps.fetch_add(1, Ordering::Relaxed);
    let mut idle_sweeps: u32 = 0;
    loop {
        if *lock(&handle.closed) {
            break;
        }
        if handle.reaped.load(Ordering::SeqCst) {
            break;
        }
        let interval = watch_poll_interval(idle_sweeps);
        handle
            .poll_interval_ms
            .store(interval.as_millis() as u64, Ordering::Relaxed);
        let woke = match &signal {
            Some(signal) => signal.wait(interval),
            None => {
                std::thread::sleep(interval);
                false
            }
        };
        if *lock(&handle.closed) {
            break;
        }
        // Reap when the owning runtime is gone (the thread must not outlive
        // it). A live-but-unpolled watcher keeps running: Node's fs.watch
        // reports changes regardless of how long the JS poller was blocked.
        if handle.owner.upgrade().is_none() {
            handle.reaped.store(true, Ordering::SeqCst);
            break;
        }
        let events = tree.refresh(&handle.root, handle.recursive, &handle.stat_calls);
        handle.sweeps.fetch_add(1, Ordering::Relaxed);
        // Any change — or any native wake-up, even one the diff absorbed —
        // keeps the fast cadence; only a run of quiet sweeps backs off.
        if events.is_empty() && !woke {
            idle_sweeps = idle_sweeps.saturating_add(1);
        } else {
            idle_sweeps = 0;
        }
        for (kind, relative) in events {
            push_event(&handle, kind, relative);
        }
    }
    // Self-cleanup once the watcher ends (closed or reaped): drop the handle
    // from the global registry. The registry mutex is never re-entered — the
    // lock is taken once for the lookup and released before the removal (a
    // second `lock(watchers())` while the iterator guard is alive would
    // deadlock this thread *holding* the registry, hanging every later
    // `fsWatch` op — including the JS pump's `watchPoll`, i.e. the whole
    // script runtime).
    let stale = {
        let guard = lock(watchers());
        guard
            .iter()
            .find(|(_, entry)| Arc::ptr_eq(entry, &handle))
            .map(|(id, _)| *id)
    };
    if let Some(id) = stale {
        lock(watchers()).remove(&id);
    }
}

fn push_event(handle: &FsWatchHandle, kind: &'static str, relative: Option<String>) {
    // Single-file watches report the watched basename (Node reports the
    // watched path's own name on Windows); directory renames report null.
    let filename = match (&handle.file_name, relative) {
        (Some(name), _) => Some(name.clone()),
        (None, relative) => relative,
    };
    let mut events = lock(&handle.events);
    let overflowed = events.len() >= MAX_WATCH_EVENTS;
    if overflowed {
        events.pop_front();
    }
    events.push_back(WatchEvent { kind, filename });
    if !overflowed {
        return;
    }
    let dropped = handle.dropped.fetch_add(1, Ordering::SeqCst) + 1;
    // Bounded-cadence diagnostic: the first drop plus one per
    // WATCH_DROP_REPORT_EVERY (identical-text repeats are also collapsed by
    // the logging router). A watcher that is never polled is an operator
    // problem, not a silent one.
    if dropped == 1 || dropped.is_multiple_of(WATCH_DROP_REPORT_EVERY) {
        crate::ext_diag!(
            "[ext] fs.watch backlog full for `{}` ({} queued events, poller not draining): dropped {} of the oldest notifications",
            handle.root.display(),
            MAX_WATCH_EVENTS,
            dropped
        );
    }
}

fn watch_poll(payload: &JsonValue) -> Result<JsonValue, FnError> {
    let id = payload_id(payload)?;
    let handle = lock(watchers())
        .get(&id)
        .cloned()
        .ok_or_else(|| FnError::new("ESRCH", format!("no fs watcher with id {id}")))?;
    let mut events = lock(&handle.events);
    // Assembled by hand instead of `json!`: the macro serializes every
    // expression by reference (`to_value(&x)`), so a `Value` input is rebuilt
    // element by element — `json!({ "events": drained })` copied the whole
    // batch into a second array (and, inside each event, copied the filename
    // again). Here each event's own strings move into its object and the
    // drained array moves into the envelope.
    let mut drained = Vec::with_capacity(events.len());
    for event in events.drain(..) {
        let mut entry = serde_json::Map::new();
        entry.insert("type".to_owned(), JsonValue::String(event.kind.to_owned()));
        entry.insert(
            "filename".to_owned(),
            match event.filename {
                Some(filename) => JsonValue::String(filename),
                None => JsonValue::Null,
            },
        );
        drained.push(JsonValue::Object(entry));
    }
    // `dropped` rides every answer so an observer can detect the bounded
    // queue's overflow without reading the diagnostic log (the JS pump reads
    // `events` and ignores the extra key — Node's `FSWatcher` has no counter
    // to expose, and the count is cumulative, so a consumer can diff it).
    let dropped = handle.dropped.load(Ordering::SeqCst);
    let mut batch = serde_json::Map::new();
    batch.insert("events".to_owned(), JsonValue::Array(drained));
    if dropped > 0 {
        batch.insert("dropped".to_owned(), JsonValue::from(dropped));
    }
    Ok(JsonValue::Object(batch))
}

/// Diagnostic snapshot of one watcher (see the `watchStats` op comment in
/// [`fs_watch_call`]): sweep/stat counters, the live fallback interval, and
/// the current queue depth. Read-only.
fn watch_stats(payload: &JsonValue) -> Result<JsonValue, FnError> {
    let id = payload_id(payload)?;
    let handle = lock(watchers())
        .get(&id)
        .cloned()
        .ok_or_else(|| FnError::new("ESRCH", format!("no fs watcher with id {id}")))?;
    let mut stats = serde_json::Map::new();
    stats.insert(
        "sweeps".to_owned(),
        JsonValue::from(handle.sweeps.load(Ordering::Relaxed)),
    );
    stats.insert(
        "statCalls".to_owned(),
        JsonValue::from(handle.stat_calls.load(Ordering::Relaxed)),
    );
    stats.insert(
        "pollIntervalMs".to_owned(),
        JsonValue::from(handle.poll_interval_ms.load(Ordering::Relaxed)),
    );
    stats.insert(
        "queued".to_owned(),
        JsonValue::from(lock(&handle.events).len() as u64),
    );
    Ok(JsonValue::Object(stats))
}

fn watch_stop(payload: &JsonValue) -> Result<JsonValue, FnError> {
    let id = payload_id(payload)?;
    if let Some(handle) = watchers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&id)
    {
        *lock(&handle.closed) = true;
    }
    Ok(JsonValue::Bool(true))
}

/// Stop every watcher started by `generation` (runtime teardown or `/reload`).
///
/// The weak-owner reap in [`watch_thread`] cannot serve a reload: the
/// successor runtime shares the same `RuntimeState`, so the weak reference
/// stays alive and a previous generation's OS watcher (thread + native watch
/// handle) would live on with no JS pump to drain it. The generation release
/// removes the entries and sets `closed`, so each thread leaves at its next
/// poll slice (at most [`WATCH_POLL_IDLE`]). Watchers of a successor generation
/// are untouched.
pub(super) fn release_generation(generation: u64) {
    let stale: Vec<Arc<FsWatchHandle>> = {
        let mut table = watchers().lock().unwrap_or_else(PoisonError::into_inner);
        let ids: Vec<u64> = table
            .iter()
            .filter(|(_, handle)| handle.generation == generation)
            .map(|(id, _)| *id)
            .collect();
        ids.iter().filter_map(|id| table.remove(id)).collect()
    };
    for handle in stale {
        *lock(&handle.closed) = true;
    }
}

/// Number of live watcher handles (test seam for the generation release).
#[doc(hidden)]
pub fn watch_count_for_tests() -> usize {
    watchers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len()
}
