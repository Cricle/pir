//! `fs.watch` incremental-snapshot equivalence.
//!
//! The host watcher (`src/extensions/runtime/host_functions/fs_watch_fns.rs`)
//! replaced its per-tick "snapshot the whole tree, diff two maps" loop with a
//! persistent mark-and-sweep tree (`WatchTree::refresh`). The contract that
//! rewrite has to keep is the *event set*: for one fixture and one scripted
//! sequence of filesystem operations, the events the real watcher delivers must
//! be exactly the events the previous algorithm produces. That algorithm is
//! kept here as `reference` — a verbatim copy of the removed `snapshot` /
//! `walk_dir` plus the removed diff, including its root `rename`(null) arms for
//! the absent state (the pre-change file is archived at
//! `.pir-foreman-backup-20261014-1543/lane46-fswatch/fs_watch_fns.rs`).
//!
//! Determinism model: the watcher sweeps immediately on native events and at
//! the 200 ms fast fallback cadence while changes are flowing, so the scripted
//! sequence is built from single-syscall operations separated by `STEP_WINDOW`
//! (600 ms ≈ 3 fast ticks) windows in which the tree does not change. Every
//! operation is therefore diffed by its own tick, further ticks diff an
//! unchanged tree and report nothing, and the watcher's accumulated events
//! equal the reference's per-step diffs — the reference snapshots the same tree
//! right before and right after each step. Operations are single syscalls
//! (`File::create`, `remove_file`, `remove_dir`, `create_dir`, `rename`,
//! `write`): a create plus a data write is two syscalls, and a tick landing
//! between them reports `rename` + `change` where the reference predicts only
//! `rename`.
//!
//! The comparison is per-watcher set equality (the old queue order was never a
//! contract), plus two multiset checks a set comparison cannot make: the number
//! of root `rename`(null) events the absent-state watcher reports (exactly one
//! per transition, never a repeat per absent tick) and the single-file
//! watcher's basename override.
//!
//! Runtime: ~11 s (26 scripted steps at 400 ms + a 600 ms settle), inside the
//! runtime's 30 s hook budget; the JS side caps its own wait so the test cannot
//! hang.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// Quiet window after every scripted operation: 2 fast poll ticks
/// (`WATCH_POLL_FAST` is 200 ms) observe the operation's final state, and the
/// next operation starts only after the watcher had a full tick to diff the
/// unchanged tree. The old 600 ms (3 ticks) bought a third tick that added
/// wall time without changing the event set.
const STEP_WINDOW: Duration = Duration::from_millis(400);

/// Settle time before the first operation: the watchers are created inside the
/// `session_start` handler and their threads take the baseline snapshot in this
/// window (3 fast ticks).
const WATCH_SETTLE: Duration = Duration::from_millis(600);

/// One event as both sides express it: Node's kind plus `filename` (`None` for
/// the root-level `rename` of the absent state).
type Event = (&'static str, Option<String>);

// ---------------------------------------------------------------------------
// The pre-change algorithm, kept as the reference
// ---------------------------------------------------------------------------

/// The removed snapshot-plus-diff algorithm, copied verbatim (only the diff's
/// three pieces — `diff_snapshots`, the `(Some, None)` / `(None, Some)` arms of
/// the old poll loop — are folded into [`diff`] here).
mod reference {
    use std::collections::HashMap;
    use std::path::Path;
    use std::time::SystemTime;

    /// Relative path → (mtime nanos, byte length). A watched single file is the
    /// entry `""`.
    pub type Snapshot = HashMap<String, (u128, u64)>;

    pub fn snapshot(root: &Path, recursive: bool) -> Option<Snapshot> {
        let mut out = HashMap::new();
        if root.is_file() {
            let meta = std::fs::metadata(root).ok()?;
            out.insert(
                String::new(),
                (
                    meta.modified()
                        .ok()?
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .ok()?
                        .as_nanos(),
                    meta.len(),
                ),
            );
            return Some(out);
        }
        let entries = std::fs::read_dir(root).ok()?;
        walk_dir(entries, "", recursive, &mut out, 0);
        Some(out)
    }

    fn walk_dir(
        entries: std::fs::ReadDir,
        prefix: &str,
        recursive: bool,
        out: &mut Snapshot,
        depth: usize,
    ) {
        if depth > 16 {
            return; // bounded walk: pathological trees cannot hang the poll
        }
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            let raw_name = entry.file_name();
            let raw_name = raw_name.to_string_lossy();
            let mut name = String::with_capacity(prefix.len() + raw_name.len() + 1);
            name.push_str(prefix);
            name.push_str(&raw_name);
            if meta.is_dir() {
                out.insert(name.clone(), (0, 0));
                if recursive {
                    if let Ok(inner) = std::fs::read_dir(entry.path()) {
                        walk_dir(inner, &(name + "\\"), recursive, out, depth + 1);
                    }
                }
                continue;
            }
            let mtime = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            out.insert(name, (mtime, meta.len()));
        }
    }

    /// One tick of the old loop: `diff_snapshots` for two live snapshots, the
    /// root `rename`(null) for a transition into or out of the absent state,
    /// nothing while the root stays unreadable.
    pub fn diff(
        previous: Option<&Snapshot>,
        next: Option<&Snapshot>,
    ) -> Vec<(&'static str, Option<String>)> {
        let mut events = Vec::new();
        match (previous, next) {
            (Some(previous), Some(next)) => {
                for (name, current) in next {
                    match previous.get(name) {
                        None => events.push(("rename", Some(name.clone()))),
                        Some(prior) => {
                            if prior != current {
                                events.push(("change", Some(name.clone())));
                            }
                        }
                    }
                }
                for name in previous.keys() {
                    if !next.contains_key(name) {
                        events.push(("rename", Some(name.clone())));
                    }
                }
            }
            (Some(_), None) | (None, Some(_)) => events.push(("rename", None)),
            (None, None) => {}
        }
        events
    }
}

/// One watcher's reference state: the previous snapshot plus the accumulated
/// event set, with `push_event`'s filename rule mirrored (a single-file watcher
/// reports its own basename for every event, the root null included).
struct Reference {
    root: PathBuf,
    recursive: bool,
    file_name: Option<String>,
    previous: Option<reference::Snapshot>,
    events: BTreeSet<Event>,
}

impl Reference {
    /// Take the baseline (the watcher's own initial snapshot): no events yet.
    fn new(root: &Path, recursive: bool, file_name: Option<String>) -> Self {
        Self {
            previous: reference::snapshot(root, recursive),
            root: root.to_path_buf(),
            recursive,
            file_name,
            events: BTreeSet::new(),
        }
    }

    /// Snapshot after one scripted step and fold the old algorithm's events for
    /// it into the set.
    fn observe(&mut self) {
        let next = reference::snapshot(&self.root, self.recursive);
        for (kind, relative) in reference::diff(self.previous.as_ref(), next.as_ref()) {
            let filename = match (&self.file_name, relative) {
                (Some(name), _) => Some(name.clone()),
                (None, relative) => relative,
            };
            self.events.insert((kind, filename));
        }
        self.previous = next;
    }
}

// ---------------------------------------------------------------------------
// Fixture and scripted operations
// ---------------------------------------------------------------------------

/// Resolved fixture paths; the driver thread rebuilds them from the same root.
#[derive(Clone)]
struct Fixture {
    /// The recursive directory watch.
    dir_a: PathBuf,
    /// The directory watch whose root moves away and back (absent state).
    dir_b: PathBuf,
    /// Where `dir_b` parks while it is gone (outside every watched tree).
    parked_b: PathBuf,
    /// The single-file watch.
    file_c: PathBuf,
}

impl Fixture {
    fn from_root(root: &Path) -> Self {
        Self {
            dir_a: root.join("a"),
            dir_b: root.join("b"),
            parked_b: root.join("b-parked"),
            file_c: root.join("c.txt"),
        }
    }

    /// The sentinel the driver creates last; the JS handler waits for it and
    /// then drains one final window before resolving.
    fn sentinel(&self) -> PathBuf {
        self.dir_a.join("done.sentinel")
    }

    fn create(&self) {
        std::fs::create_dir_all(self.dir_a.join("sub").join("deep")).expect("create a/sub/deep");
        std::fs::create_dir_all(&self.dir_b).expect("create b");
        for (path, text) in [
            (self.dir_a.join("a.txt"), "alpha"),
            (self.dir_a.join("keep.txt"), "keep"),
            (self.dir_a.join("flip.txt"), "flip"),
            (self.dir_a.join("sub").join("child.txt"), "child"),
            (self.dir_a.join("sub").join("deep").join("deep.txt"), "deep"),
            (self.dir_b.join("child.txt"), "b-child"),
            (self.file_c.clone(), "c"),
        ] {
            std::fs::write(path, text).expect("fixture file");
        }
    }
}

/// One scripted fixture operation. Every variant is a single syscall except the
/// two ACL steps (file-system state changes either way, observed as a whole by
/// both sides).
#[derive(Debug)]
enum Op {
    /// Create an empty file (`File::create` then drop: one syscall).
    Create(PathBuf),
    /// Truncate and write an existing file. A tick between the two syscalls
    /// reports `change` twice, which the set comparison absorbs.
    Write(PathBuf, &'static str),
    RemoveFile(PathBuf),
    RemoveDir(PathBuf),
    MakeDir(PathBuf),
    Move(PathBuf, PathBuf),
    /// A quiet step: no filesystem operation at all.
    Noop,
    /// Deny the list-directory right on the watched `sub` directory, so
    /// `read_dir(sub)` fails while the directory still exists — the walk drops
    /// the whole subtree, which is the unreadable-subdirectory case.
    DenyListing,
    /// Lift that deny again.
    LiftListing,
}

/// The scripted sequence (see the module header for the ordering contract).
fn script(fixture: &Fixture) -> Vec<Op> {
    let dir_a = &fixture.dir_a;
    let dir_b = &fixture.dir_b;
    let file_c = &fixture.file_c;
    let new = dir_a.join("new.txt");
    let renamed = dir_a.join("renamed.txt");
    let nested = dir_a.join("sub").join("nested.txt");
    let sub2 = dir_a.join("sub2");
    vec![
        // The dirB and fileC transitions run FIRST: a watcher that sees no
        // change for five fast sweeps backs off to WATCH_POLL_IDLE (2 s). In
        // the old order their blocks sat ~10 s into the sequence, and the
        // 400 ms quiet window was only saved by the native backend usually
        // reporting the transition anyway (measured: 1/3 flake). Running every
        // non-dirA watcher while it is still on the fast cadence keeps the
        // 400 ms window sufficient for the same event set; the reference and
        // the watcher both run this one sequence, and set equality is
        // order-free.
        // dirB, the absent state machine: gone, still gone, back again.
        Op::Move(dir_b.clone(), fixture.parked_b.clone()),
        Op::Noop,
        Op::Move(fixture.parked_b.clone(), dir_b.clone()),
        Op::Noop,
        // fileC, the single-file watch: the basename override on every event.
        Op::Write(file_c.clone(), "cc"),
        Op::RemoveFile(file_c.clone()),
        Op::Noop,
        Op::Create(file_c.clone()),
        Op::Write(file_c.clone(), "ccc"),
        // dirA, the recursive directory watch: appearance, change,
        // disappearance, a path re-used by a later entry, and the same path
        // flipping type (the sweep has to unlearn the old tuple every time).
        Op::Create(new.clone()),
        Op::Write(new.clone(), "alpha"),
        Op::Create(nested.clone()),
        Op::RemoveFile(nested),
        Op::Move(new, renamed),
        Op::MakeDir(sub2.clone()),
        Op::Create(sub2.join("inner.txt")),
        Op::RemoveFile(sub2.join("inner.txt")),
        Op::RemoveDir(sub2),
        Op::RemoveFile(dir_a.join("flip.txt")),
        Op::MakeDir(dir_a.join("flip.txt")),
        Op::RemoveDir(dir_a.join("flip.txt")),
        Op::Create(dir_a.join("flip.txt")),
        // The subdirectory becomes unlistable, then readable again: the walk
        // drops its subtree and gets it back.
        Op::DenyListing,
        Op::LiftListing,
        // Same byte length, new mtime: still a `change` (the tuple compare is
        // not a length compare).
        Op::Write(dir_a.join("keep.txt"), "KEEP"),
        // The sentinel ends the JS side (and is dirA's last rename).
        Op::Create(fixture.sentinel()),
    ]
}

/// Perform one scripted operation; a failure is a broken script, not a watcher
/// finding, so it panics with the operation in the message.
fn perform(op: &Op, sub: &Path, denied: &mut Option<DeniedListing>, notes: &mut Vec<String>) {
    let outcome = match op {
        Op::Create(path) => std::fs::File::create(path).map(drop),
        Op::Write(path, text) => std::fs::write(path, text),
        Op::RemoveFile(path) => std::fs::remove_file(path),
        Op::RemoveDir(path) => std::fs::remove_dir(path),
        Op::MakeDir(path) => std::fs::create_dir(path),
        Op::Move(from, to) => std::fs::rename(from, to),
        Op::Noop => Ok(()),
        Op::DenyListing => {
            *denied = DeniedListing::apply(sub);
            if denied.is_none() {
                notes.push(format!(
                    "cannot deny the listing right on {}: the unreadable-subdirectory step is a no-op",
                    sub.display()
                ));
            }
            return;
        }
        Op::LiftListing => {
            match denied.take() {
                Some(mut guard) => {
                    if !guard.lift() {
                        notes.push("the listing deny could not be lifted".to_owned());
                    }
                }
                None => notes.push("listing lift without a deny".to_owned()),
            }
            return;
        }
    };
    outcome.unwrap_or_else(|error| panic!("scripted step {op:?} failed: {error}"));
}

/// A live deny on a directory's listing right, lifted explicitly (and on drop,
/// so a panicking step cannot leave an unreadable directory behind).
struct DeniedListing {
    dir: PathBuf,
    active: bool,
}

impl DeniedListing {
    /// Apply the deny; `None` when the platform could not, in which case the
    /// step changes nothing and both algorithms agree trivially.
    fn apply(dir: &Path) -> Option<Self> {
        set_listing_denied(dir, true).then(|| Self {
            dir: dir.to_path_buf(),
            active: true,
        })
    }

    fn lift(&mut self) -> bool {
        let lifted = set_listing_denied(&self.dir, false);
        self.active = !lifted;
        lifted
    }
}

impl Drop for DeniedListing {
    fn drop(&mut self) {
        if self.active {
            let _ = set_listing_denied(&self.dir, false);
        }
    }
}

/// Deny (`denied`) or restore the list-directory right on `dir`. Windows: the
/// ACL denies `RD` to Everyone, spelled as a SID so the command does not depend
/// on the display language. Unix: mode `0o000` (owner rights restored after).
#[cfg(windows)]
fn set_listing_denied(dir: &Path, denied: bool) -> bool {
    let mut command = std::process::Command::new("icacls");
    command.arg(dir);
    if denied {
        command.args(["/deny", "*S-1-1-0:(RD)"]);
    } else {
        command.args(["/remove:d", "*S-1-1-0"]);
    }
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn set_listing_denied(dir: &Path, denied: bool) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let mode = if denied { 0o000 } else { 0o700 };
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).is_ok()
}

#[cfg(not(any(windows, unix)))]
fn set_listing_denied(_dir: &Path, _denied: bool) -> bool {
    false
}

/// The old algorithm's event sets for the scripted sequence, one per watcher.
#[derive(Debug)]
struct ReferenceSets {
    a: BTreeSet<Event>,
    b: BTreeSet<Event>,
    c: BTreeSet<Event>,
}

/// Run the scripted operations on the fixture (own thread: the handler's
/// pending await keeps pumping the JS event loop while this runs).
fn run_steps(fixture: &Fixture) -> (ReferenceSets, Vec<String>) {
    let sub = fixture.dir_a.join("sub");
    let mut notes = Vec::new();
    std::thread::sleep(WATCH_SETTLE);
    let mut ref_a = Reference::new(&fixture.dir_a, true, None);
    let mut ref_b = Reference::new(&fixture.dir_b, true, None);
    let mut ref_c = Reference::new(
        &fixture.file_c,
        false,
        fixture
            .file_c
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
    );
    let mut denied: Option<DeniedListing> = None;
    for op in script(fixture) {
        perform(&op, &sub, &mut denied, &mut notes);
        std::thread::sleep(STEP_WINDOW);
        ref_a.observe();
        ref_b.observe();
        ref_c.observe();
    }
    (
        ReferenceSets {
            a: ref_a.events,
            b: ref_b.events,
            c: ref_c.events,
        },
        notes,
    )
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

fn test_config(cwd: PathBuf) -> ExtensionConfig {
    ExtensionConfig {
        cwd,
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

/// A JS string literal for `path` (`Debug` for `str` produces the same quoting
/// and backslash escapes a JS double-quoted string uses).
fn js_str(path: &Path) -> String {
    format!("{:?}", path.to_string_lossy())
}

/// The recorded event stream for one watcher bucket, in queue order.
fn events_of(records: &Value, bucket: &str) -> Vec<Event> {
    let entries = records[bucket]
        .as_array()
        .unwrap_or_else(|| panic!("no `{bucket}` bucket in {records}"));
    entries
        .iter()
        .map(|entry| {
            let kind = match entry["type"].as_str() {
                Some("rename") => "rename",
                Some("change") => "change",
                other => panic!("unexpected event type {other:?} in {entry}"),
            };
            (kind, entry["filename"].as_str().map(str::to_owned))
        })
        .collect()
}

const TS: &str = r#"
import fs from "node:fs";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const recs: any = { a: [], b: [], c: [] };
    (globalThis as any).__pirIncRecs = recs;
    const collector = (bucket: any) => (eventType: any, filename: any) => {
      bucket.push({ type: eventType, filename: filename === undefined ? null : filename });
    };
    // The placeholders stand bare: `js_str` substitutes a `Debug`-formatted,
    // already quoted and escaped string literal (paths carry backslashes).
    const wA = fs.watch(__DIR_A__, { recursive: true, persistent: false }, collector(recs.a));
    const wB = fs.watch(__DIR_B__, { recursive: true, persistent: false }, collector(recs.b));
    const wC = fs.watch(__FILE_C__, { persistent: false }, collector(recs.c));
    (globalThis as any).__pirIncWatchers = [wA, wB, wC];
    const done = __DONE__;
    await new Promise((resolve: any) => {
      const poll = setInterval(() => {
        if (!fs.existsSync(done)) return;
        clearInterval(poll);
        // One last drain window: the watcher report of the sentinel lands
        // within one 200 ms fast tick and the 30 ms pump delivers it before
        // this resolves; 450 ms covers two ticks with jitter margin.
        setTimeout(resolve, 450);
      }, 50);
      // Never outlive the host call budget: resolve bounded either way.
      setTimeout(() => { clearInterval(poll); resolve("timeout"); }, 15000);
    });
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn incremental_snapshot_events_match_the_reference_algorithm() {
    let root = std::env::temp_dir().join(format!("pir-fs-watch-inc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let fixture = Fixture::from_root(&root);
    fixture.create();

    let ts = TS
        .replace("__DIR_A__", &js_str(&fixture.dir_a))
        .replace("__DIR_B__", &js_str(&fixture.dir_b))
        .replace("__FILE_C__", &js_str(&fixture.file_c))
        .replace("__DONE__", &js_str(&fixture.sentinel()));

    let rt = ExtensionRuntime::new(test_config(root.clone()))
        .await
        .expect("runtime");
    rt.allow_root(root.clone());
    let module = "pir-ext:fs_watch_incremental.ts";
    rt.seed_module(module, &ts).expect("seed fixture");
    let loaded = rt.seeded_load(module).await;
    assert_eq!(loaded.expect("seeded load"), 1, "fixture must load");

    let driver_fixture = fixture.clone();
    let driver = std::thread::spawn(move || run_steps(&driver_fixture));

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors: {:?}",
        outcome.errors
    );

    let (expected, notes) = driver.join().expect("driver thread");
    for note in &notes {
        eprintln!("fs_watch_incremental note: {note}");
    }

    let records: Value = rt
        .eval_json("JSON.stringify(globalThis.__pirIncRecs || {})")
        .await
        .expect("read records");
    let observed_a = events_of(&records, "a");
    let observed_b = events_of(&records, "b");
    let observed_c = events_of(&records, "c");

    // A green comparison against empty sets would prove nothing: pin the events
    // the scripted sequence must always predict. The ACL step is deliberately
    // not part of this list (it is a no-op where the platform refuses the
    // deny), dirB's two transitions collapse to one set element, and dirC's
    // change plus its basename-overridden root rename are two.
    for required in [
        ("rename", Some("new.txt".to_owned())),
        ("change", Some("new.txt".to_owned())),
        ("rename", Some("renamed.txt".to_owned())),
        ("rename", Some("sub\\nested.txt".to_owned())),
        ("rename", Some("sub2".to_owned())),
        ("rename", Some("flip.txt".to_owned())),
        ("change", Some("keep.txt".to_owned())),
        ("rename", Some("done.sentinel".to_owned())),
    ] {
        assert!(
            expected.a.contains(&required),
            "reference must predict {required:?}: {expected:?}"
        );
    }
    assert_eq!(
        expected.b.len(),
        1,
        "reference must predict the dirB absent transition: {expected:?}"
    );
    assert_eq!(
        expected.c.len(),
        2,
        "reference must predict the dirC change and rename: {expected:?}"
    );

    let set_a: BTreeSet<Event> = observed_a.iter().cloned().collect();
    let set_b: BTreeSet<Event> = observed_b.iter().cloned().collect();
    let set_c: BTreeSet<Event> = observed_c.iter().cloned().collect();
    assert_eq!(
        set_a, expected.a,
        "dirA recursive watch: observed stream {observed_a:?}"
    );
    assert_eq!(
        set_b, expected.b,
        "dirB absent-state watch: observed stream {observed_b:?}"
    );
    assert_eq!(
        set_c, expected.c,
        "fileC single-file watch: observed stream {observed_c:?}"
    );

    // Set equality hides repeats, so the absent state gets its multiset check:
    // one root `rename`(null) per transition, nothing while it stays absent
    // (the run crosses several absent ticks).
    let root_rename: Event = ("rename", None);
    assert!(
        observed_b.len() == 2 && observed_b.iter().all(|event| *event == root_rename),
        "dirB must report exactly the two root renames: {observed_b:?}"
    );
    // dirA is never absent, so it must never report the root null.
    assert!(
        observed_a.iter().all(|(_, filename)| filename.is_some()),
        "dirA is never absent: {observed_a:?}"
    );
    // The single-file watch reports its basename for every event, the root
    // renames included.
    assert!(
        observed_c
            .iter()
            .all(|(_, filename)| filename.as_deref() == Some("c.txt")),
        "fileC reports the watched basename: {observed_c:?}"
    );

    let closed: Value = rt
        .eval_json("JSON.stringify(globalThis.__pirIncWatchers.map(w => w.close().closed))")
        .await
        .expect("close watchers");
    assert_eq!(
        closed,
        json!([true, true, true]),
        "watcher.close() marks every watcher closed"
    );

    let _ = std::fs::remove_dir_all(&root);
}
