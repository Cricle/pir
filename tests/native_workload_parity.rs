//! Fixed-workload native Rust vs `pir js` comparison (no Node/npm needed).
//!
//! `cargo test --release --no-default-features --features cli,extensions --test native_workload_parity -- --ignored --nocapture`
//!
//! `PIR_NATIVE_WORKLOAD_BIN` overrides Cargo's pir executable (use a compatible
//! release binary); `PIR_NATIVE_WORKLOAD_ONLY=fs|codec|sqlite` selects one case.
//! `PIR_NATIVE_WORKLOAD_SAMPLES=1` prints `NATIVE_WORKLOAD_SAMPLE <json>` records
//! with PIDs. `PIR_NATIVE_WORKLOAD_DIAGNOSTICS=1` adds the Windows child peak
//! commit and working set, and captures pool counters when enabled in the child.
//! Native runs in the libtest process; per-workload native memory is not reported.
//!
//! Fixture creation is untimed. The fs case uses warm OS caches, not cold disk.
//! Work times cover the same fixed batches on both sides, including checksumming
//! results. Pir wall time additionally includes process/runtime boot, script
//! load, untimed input setup, output and teardown. A no-work script invocation
//! gives a separate launch baseline; it is not subtracted to estimate work.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

const RUNS: usize = 7;
const FS_DIRS: usize = 4;
const FS_FILES_PER_DIR: usize = 16;
const INPUT_BYTES: usize = 512 * 1024;
const CODEC_LEVEL: u32 = 6;
const SQLITE_ROWS: usize = 384;
const BLOB_BYTES: usize = 1024;
const QUERY_BATCH: usize = 128;
const SCHEMA: &str =
    "CREATE TABLE bench(id INTEGER PRIMARY KEY, score INTEGER NOT NULL, payload BLOB NOT NULL)";
const INSERT: &str = "INSERT INTO bench(id, score, payload) VALUES (?, ?, ?)";
const SELECT: &str = "SELECT id, score, payload FROM bench WHERE id >= ? AND id < ? ORDER BY id";
const RESULT_PREFIX: &str = "NATIVE_WORKLOAD_RESULT ";

#[derive(Clone, Copy)]
enum Workload {
    Fs,
    Codec,
    Sqlite,
}

impl Workload {
    fn name(self) -> &'static str {
        match self {
            Self::Fs => "fs",
            Self::Codec => "codec",
            Self::Sqlite => "sqlite",
        }
    }

    fn batch(self) -> usize {
        match self {
            Self::Fs | Self::Codec => 8,
            Self::Sqlite => 4,
        }
    }

    fn run(self, fixture: &Fixture) -> Check {
        match self {
            Self::Fs => native_fs(&fixture.root.join("tree")),
            Self::Codec => native_codec(&fixture.input),
            Self::Sqlite => native_sqlite(&fixture.input),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Check {
    Fs {
        directories: usize,
        files: usize,
        bytes: usize,
        digest: String,
    },
    Codec {
        bytes: usize,
        digest: String,
    },
    Sqlite {
        writes: usize,
        rows: usize,
        blob_bytes: usize,
        digest: String,
    },
}

struct Fixture {
    root: PathBuf,
    input: Vec<u8>,
    fs_check: Check,
}

impl Fixture {
    fn new() -> Self {
        // tempfile isn't a direct dependency. Create a fresh, private-to-this-
        // run directory and let the fixture clean it up even on a failed check.
        let root =
            std::env::temp_dir().join(format!("pir-native-workload-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).expect("create fresh benchmark directory");
        let mut fixture = Self {
            root,
            input: fixture_bytes(INPUT_BYTES, 0x1083_7ac9),
            fs_check: fs_check(0, 0, 0, Sha256::new()),
        };
        let tree = fixture.root.join("tree");
        fs::create_dir(&tree).expect("create fs fixture root");
        let mut hash = Sha256::new();
        let mut bytes = 0;
        for dir in 0..FS_DIRS {
            let dir_name = format!("d{dir:02}");
            fs::create_dir(tree.join(&dir_name)).expect("create fs fixture subdirectory");
            for file in 0..FS_FILES_PER_DIR {
                let ordinal = dir * FS_FILES_PER_DIR + file;
                let data = fixture_bytes(4096 + (ordinal % 7) * 997, ordinal as u32 + 1);
                let rel = format!("{dir_name}/f{file:02}.bin");
                fs::write(tree.join(&rel), &data).expect("write fs fixture file");
                hash_file(&mut hash, &rel, &data);
                bytes += data.len();
            }
        }
        fixture.fs_check = fs_check(FS_DIRS, FS_DIRS * FS_FILES_PER_DIR, bytes, hash);
        fs::write(fixture.root.join("input.bin"), &fixture.input).expect("write binary input");
        fs::create_dir(fixture.root.join("agent")).expect("create isolated agent directory");
        let spec = json!({
            "codecLevel": CODEC_LEVEL, "rows": SQLITE_ROWS, "blobBytes": BLOB_BYTES,
            "queryBatch": QUERY_BATCH, "schema": SCHEMA, "insert": INSERT, "select": SELECT,
            "resultPrefix": RESULT_PREFIX,
        });
        fs::write(
            fixture.root.join("workload.cjs"),
            format!("const spec = {spec};\n{SCRIPT}"),
        )
        .expect("write benchmark script");
        fixture
    }

    fn expected(&self, workload: Workload) -> Check {
        match workload {
            Workload::Fs => self.fs_check.clone(),
            Workload::Codec => Check::Codec {
                bytes: self.input.len(),
                digest: format!("{:x}", Sha256::digest(&self.input)),
            },
            Workload::Sqlite => {
                // Reference derived from the intended rows, without querying SQLite.
                let mut hash = Sha256::new();
                for id in 0..SQLITE_ROWS as i64 {
                    hash_row(&mut hash, id, score(id), payload(&self.input, id as usize));
                }
                sqlite_check(SQLITE_ROWS, SQLITE_ROWS, SQLITE_ROWS * BLOB_BYTES, hash)
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A deterministic mix of repeated and pseudo-random binary bytes.
fn fixture_bytes(len: usize, mut state: u32) -> Vec<u8> {
    (0..len)
        .map(|offset| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            if offset % 256 < 160 {
                (offset % 32) as u8
            } else {
                state as u8
            }
        })
        .collect()
}

fn hash_file(hash: &mut Sha256, relative: &str, data: &[u8]) {
    hash.update(format!("{relative}\0{}\0", data.len()));
    hash.update(data);
}

fn fs_check(directories: usize, files: usize, bytes: usize, hash: Sha256) -> Check {
    Check::Fs {
        directories,
        files,
        bytes,
        digest: format!("{:x}", hash.finalize()),
    }
}

fn sorted_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .expect("scan fixture directory")
        .map(|entry| {
            entry
                .expect("read fixture entry")
                .file_name()
                .into_string()
                .expect("ASCII name")
        })
        .collect();
    names.sort_unstable();
    names
}

fn native_fs(tree: &Path) -> Check {
    let dirs = sorted_names(tree);
    let (mut files, mut bytes) = (0, 0);
    let mut hash = Sha256::new();
    for dir in &dirs {
        for file in sorted_names(&tree.join(dir)) {
            let rel = format!("{dir}/{file}");
            let data = fs::read(tree.join(&rel)).expect("read fixture file");
            hash_file(&mut hash, &rel, &data);
            files += 1;
            bytes += data.len();
        }
    }
    fs_check(dirs.len(), files, bytes, hash)
}

fn native_codec(input: &[u8]) -> Check {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(CODEC_LEVEL));
    encoder.write_all(input).expect("compress fixture");
    let encoded = encoder.finish().expect("finish compressed fixture");
    assert!(
        !encoded.is_empty() && encoded.len() < input.len(),
        "fixture must compress"
    );
    let mut decoded = Vec::new();
    ZlibDecoder::new(encoded.as_slice())
        .read_to_end(&mut decoded)
        .expect("decompress fixture");
    // The decoded bytes must match the source's digest, not another encoder's
    // wire representation (valid zlib streams need not be byte-identical).
    Check::Codec {
        bytes: decoded.len(),
        digest: format!("{:x}", Sha256::digest(&decoded)),
    }
}

fn score(id: i64) -> i64 {
    (id * 17) % 103 - 51
}

fn payload(input: &[u8], id: usize) -> &[u8] {
    &input[id * BLOB_BYTES..(id + 1) * BLOB_BYTES]
}

fn hash_row(hash: &mut Sha256, id: i64, row_score: i64, blob: &[u8]) {
    hash.update(format!("{id}:{row_score}:{}\n", blob.len()));
    hash.update(blob);
}

fn sqlite_check(writes: usize, rows: usize, blob_bytes: usize, hash: Sha256) -> Check {
    Check::Sqlite {
        writes,
        rows,
        blob_bytes,
        digest: format!("{:x}", hash.finalize()),
    }
}

fn native_sqlite(input: &[u8]) -> Check {
    let db = Connection::open_in_memory().expect("open in-memory SQLite database");
    db.execute_batch(SCHEMA).expect("create SQLite schema");
    db.execute_batch("BEGIN").expect("begin write transaction");
    let mut writes = 0;
    {
        let mut insert = db.prepare(INSERT).expect("prepare BLOB insert");
        for id in 0..SQLITE_ROWS as i64 {
            writes += insert
                .execute(params![id, score(id), payload(input, id as usize)])
                .expect("insert BLOB row");
        }
    }
    db.execute_batch("COMMIT")
        .expect("commit write transaction");
    let mut select = db.prepare(SELECT).expect("prepare ordered range query");
    let (mut rows, mut blob_bytes) = (0, 0);
    let mut hash = Sha256::new();
    for lo in (0..SQLITE_ROWS).step_by(QUERY_BATCH) {
        let hi = (lo + QUERY_BATCH).min(SQLITE_ROWS);
        // Materialize each range on both sides, matching DatabaseSync.all().
        let result = select
            .query_map(params![lo as i64, hi as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })
            .expect("run range query")
            .collect::<Result<Vec<_>, _>>()
            .expect("read range rows");
        for (id, row_score, blob) in result {
            hash_row(&mut hash, id, row_score, &blob);
            rows += 1;
            blob_bytes += blob.len();
        }
    }
    sqlite_check(writes, rows, blob_bytes, hash)
}

const SCRIPT: &str = r#"
const name = process.argv[2];
const batch = Number(process.argv[3]);
const checks = [];
let work_ms = null;
if (name !== "startup") {
  const fs = require("node:fs");
  const { createHash } = require("node:crypto");
  const input = name === "fs" ? null : fs.readFileSync("input.bin");
  let run;
  switch (name) {
    case "fs":
      run = () => {
        const dirs = fs.readdirSync("tree").sort();
        const hash = createHash("sha256");
        let files = 0, bytes = 0;
        for (const dir of dirs) {
          for (const file of fs.readdirSync("tree/" + dir).sort()) {
            const relative = dir + "/" + file;
            const data = fs.readFileSync("tree/" + relative);
            hash.update(relative + "\0" + data.length + "\0");
            hash.update(data);
            files++;
            bytes += data.length;
          }
        }
        return { kind: name, directories: dirs.length, files, bytes, digest: hash.digest("hex") };
      };
      break;
    case "codec": {
      const { deflateSync, inflateSync } = require("node:zlib");
      run = () => {
        const encoded = deflateSync(input, { level: spec.codecLevel });
        if (encoded.length === 0 || encoded.length >= input.length) throw new Error("fixture must compress");
        const decoded = inflateSync(encoded);
        return { kind: name, bytes: decoded.length, digest: createHash("sha256").update(decoded).digest("hex") };
      };
      break;
    }
    case "sqlite": {
      const { DatabaseSync } = require("node:sqlite");
      run = () => {
        const db = new DatabaseSync(":memory:");
        try {
          db.exec(spec.schema);
          db.exec("BEGIN");
          let writes = 0, rows = 0, blob_bytes = 0;
          const insert = db.prepare(spec.insert);
          for (let id = 0; id < spec.rows; id++) {
            const blob = input.subarray(id * spec.blobBytes, (id + 1) * spec.blobBytes);
            writes += insert.run(id, (id * 17) % 103 - 51, blob).changes;
          }
          db.exec("COMMIT");
          const select = db.prepare(spec.select);
          const hash = createHash("sha256");
          for (let lo = 0; lo < spec.rows; lo += spec.queryBatch) {
            const hi = Math.min(lo + spec.queryBatch, spec.rows);
            for (const row of select.all(lo, hi)) {
              if (!(row.payload instanceof Uint8Array)) throw new Error("expected SQLite BLOB");
              hash.update(row.id + ":" + row.score + ":" + row.payload.length + "\n");
              hash.update(row.payload);
              rows++;
              blob_bytes += row.payload.length;
            }
          }
          return { kind: name, writes, rows, blob_bytes, digest: hash.digest("hex") };
        } finally {
          db.close();
        }
      };
      break;
    }
    default: throw new Error("unknown benchmark case: " + name);
  }
  const start = performance.now();
  for (let i = 0; i < batch; i++) checks.push(run());
  work_ms = performance.now() - start;
}
console.log(spec.resultPrefix + JSON.stringify({ name, work_ms, checks }));
"#;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PirResult {
    name: String,
    work_ms: Option<f64>,
    checks: Vec<Check>,
}

struct Sample {
    pid: u32,
    work_ms: Option<f64>,
    wall_ms: Option<f64>,
    peak_commit_bytes: Option<usize>,
    peak_working_set_bytes: Option<usize>,
    pool_stats: Option<String>,
}

#[cfg(windows)]
fn process_peaks(handle: &std::os::windows::io::OwnedHandle) -> Option<(usize, usize)> {
    use std::os::windows::io::AsRawHandle;

    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }

    #[link(name = "kernel32")]
    #[allow(unsafe_code)]
    unsafe extern "system" {
        fn K32GetProcessMemoryInfo(
            process: std::os::windows::io::RawHandle,
            counters: *mut ProcessMemoryCounters,
            size: u32,
        ) -> i32;
    }

    let size = std::mem::size_of::<ProcessMemoryCounters>() as u32;
    let mut counters = std::mem::MaybeUninit::<ProcessMemoryCounters>::zeroed();
    // SAFETY: the owned handle remains live across this call, and `counters`
    // has the Windows PROCESS_MEMORY_COUNTERS_EX layout and full output size.
    #[allow(unsafe_code)]
    unsafe {
        if K32GetProcessMemoryInfo(handle.as_raw_handle(), counters.as_mut_ptr(), size) == 0 {
            return None;
        }
        let counters = counters.assume_init();
        Some((counters.peak_pagefile_usage, counters.peak_working_set_size))
    }
}

fn run_pir(
    bin: &Path,
    fixture: &Fixture,
    name: &str,
    batch: usize,
    expected: Option<&Check>,
) -> Sample {
    let mut command = Command::new(bin);
    command
        .current_dir(&fixture.root)
        .env("PIR_AGENT_DIR", fixture.root.join("agent"))
        .env_remove("PIR_JS_MEMORY_MB")
        .args(["js", "workload.cjs", name, &batch.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let start = Instant::now();
    let child = command
        .spawn()
        .unwrap_or_else(|err| panic!("launch {} ({name}): {err}", bin.display()));
    let pid = child.id();
    let diagnostics = std::env::var("PIR_NATIVE_WORKLOAD_DIAGNOSTICS").as_deref() == Ok("1");
    #[cfg(windows)]
    let handle = if diagnostics {
        use std::os::windows::io::AsHandle;
        child.as_handle().try_clone_to_owned().ok()
    } else {
        None
    };
    let output = child.wait_with_output().expect("collect pir child output");
    let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
    #[cfg(windows)]
    let peaks = handle.as_ref().and_then(process_peaks);
    #[cfg(not(windows))]
    let peaks: Option<(usize, usize)> = None;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let pool_stats = if diagnostics {
        stderr
            .lines()
            .rev()
            .find(|line| line.starts_with("PIR_QJS_ALLOC stats "))
            .map(str::to_owned)
    } else {
        None
    };
    assert!(
        output.status.success(),
        "pir {name} exited {}:\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    let mut records = stdout
        .lines()
        .filter_map(|line| line.strip_prefix(RESULT_PREFIX));
    let record = records
        .next()
        .unwrap_or_else(|| panic!("missing pir {name} result:\n{stdout}\n{stderr}"));
    assert!(
        records.next().is_none(),
        "duplicate pir {name} result:\n{stdout}"
    );
    let parsed: PirResult = serde_json::from_str(record)
        .unwrap_or_else(|err| panic!("invalid pir {name} result: {err}\n{record}"));
    assert_eq!(parsed.name, name, "pir reported the wrong workload");
    assert_eq!(parsed.checks.len(), batch, "pir {name}: wrong check count");
    if let Some(expected) = expected {
        assert!(
            matches!(parsed.work_ms, Some(ms) if ms.is_finite() && ms > 0.0),
            "pir {name}: invalid batch work time {:?}",
            parsed.work_ms
        );
        for (pass, check) in parsed.checks.iter().enumerate() {
            assert_eq!(check, expected, "pir {name} check on pass {}", pass + 1);
        }
    } else {
        assert!(
            parsed.work_ms.is_none(),
            "pir startup invocation reported work"
        );
    }
    Sample {
        pid,
        work_ms: parsed.work_ms,
        wall_ms: Some(wall_ms),
        peak_commit_bytes: peaks.map(|(commit, _)| commit),
        peak_working_set_bytes: peaks.map(|(_, ws)| ws),
        pool_stats,
    }
}

fn run_native(fixture: &Fixture, workload: Workload, expected: &Check) -> Sample {
    let mut checks = Vec::with_capacity(workload.batch());
    let start = Instant::now();
    for _ in 0..workload.batch() {
        checks.push(workload.run(fixture));
    }
    let work_ms = start.elapsed().as_secs_f64() * 1000.0;
    for (pass, check) in checks.iter().enumerate() {
        assert_eq!(
            check,
            expected,
            "native {} check on pass {}",
            workload.name(),
            pass + 1
        );
    }
    Sample {
        pid: std::process::id(),
        work_ms: Some(work_ms),
        wall_ms: None,
        peak_commit_bytes: None,
        peak_working_set_bytes: None,
        pool_stats: None,
    }
}

fn emit_sample(engine: &str, name: &str, trial: usize, batch: usize, sample: &Sample) {
    println!(
        "NATIVE_WORKLOAD_SAMPLE {}",
        json!({
            "engine": engine, "workload": name, "trial": trial, "batch": batch, "pid": sample.pid,
            "work_ms": sample.work_ms, "wall_ms": sample.wall_ms,
            "peak_commit_bytes": sample.peak_commit_bytes,
            "peak_working_set_bytes": sample.peak_working_set_bytes,
            "pool_stats": sample.pool_stats,
        })
    );
}

fn summarize(label: &str, mut values: Vec<f64>) -> f64 {
    values.sort_unstable_by(f64::total_cmp);
    let mid = values[values.len() / 2]; // RUNS is odd.
    println!(
        "  {label:<21} median {mid:9.3} ms   range [{:.3}, {:.3}] ms (n={})",
        values[0],
        values[values.len() - 1],
        values.len()
    );
    mid
}

#[test]
#[ignore = "release workload benchmark; run explicitly with --ignored --nocapture"]
fn native_workload_parity() {
    assert!(
        !cfg!(debug_assertions),
        "run native_workload_parity in a release profile"
    );
    let bin = std::env::var_os("PIR_NATIVE_WORKLOAD_BIN")
        .map_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pir")), PathBuf::from);
    // Resolve a relative override before changing the child's working directory.
    let bin = std::path::absolute(bin).expect("resolve pir executable path");
    assert!(bin.is_file(), "pir executable missing: {}", bin.display());
    let selection = std::env::var("PIR_NATIVE_WORKLOAD_ONLY").unwrap_or_default();
    assert!(
        matches!(selection.as_str(), "" | "fs" | "codec" | "sqlite"),
        "PIR_NATIVE_WORKLOAD_ONLY must be fs, codec, or sqlite"
    );
    let structured = std::env::var("PIR_NATIVE_WORKLOAD_SAMPLES").as_deref() == Ok("1");
    let fixture = Fixture::new();
    println!("pir executable: {}", bin.display());

    run_pir(&bin, &fixture, "startup", 0, None); // Discard first invocation.
    let startup = (1..=RUNS)
        .map(|trial| {
            let sample = run_pir(&bin, &fixture, "startup", 0, None);
            if structured {
                emit_sample("pir", "startup", trial, 0, &sample);
            }
            sample.wall_ms.expect("pir wall time")
        })
        .collect();
    summarize("pir no-work launch", startup);

    for workload in [Workload::Fs, Workload::Codec, Workload::Sqlite] {
        let name = workload.name();
        if !selection.is_empty() && selection != name {
            continue;
        }
        let batch = workload.batch();
        let expected = fixture.expected(workload);
        // Warm the filesystem cache and prime both code paths before sampling.
        run_native(&fixture, workload, &expected);
        run_pir(&bin, &fixture, name, batch, Some(&expected));
        let mut native_work = Vec::with_capacity(RUNS);
        let mut pir_work = Vec::with_capacity(RUNS);
        let mut pir_wall = Vec::with_capacity(RUNS);
        for trial in 1..=RUNS {
            let native = || run_native(&fixture, workload, &expected);
            let pir = || run_pir(&bin, &fixture, name, batch, Some(&expected));
            // Alternate order to avoid always giving one side the freshest cache.
            let (native, pir) = if trial % 2 == 1 {
                (native(), pir())
            } else {
                let p = pir();
                (native(), p)
            };
            if structured {
                emit_sample("native", name, trial, batch, &native);
                emit_sample("pir", name, trial, batch, &pir);
            }
            native_work.push(native.work_ms.expect("native work time"));
            pir_work.push(pir.work_ms.expect("pir work time"));
            pir_wall.push(pir.wall_ms.expect("pir wall time"));
        }
        println!(
            "{name}: {batch} passes per sample; verified {}",
            json!(expected)
        );
        let native_mid = summarize("native batch work", native_work);
        let pir_mid = summarize("pir batch work", pir_work);
        summarize("pir subprocess wall", pir_wall);
        println!(
            "  pir/native work: {:.2}x (ratio of medians)",
            pir_mid / native_mid
        );
    }
}
