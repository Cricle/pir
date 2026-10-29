//! `beforeExit` re-emission semantics, as implemented by the process-global
//! shim (`src/extensions/api/js/process_global_stub.js.txt`).
//!
//! Node emits `beforeExit` once per loop exhaustion. A handler that keeps the
//! loop alive (arms a timer/immediate — directly, or from a `process.nextTick`
//! callback that arms one) is followed by a *second* emission once that work
//! has run out; a handler that only queues a `nextTick` does not keep the loop
//! alive, so the process exits instead of emitting again. The vendored
//! `test-process-beforeexit.js` exercises the chain end to end but is not run
//! by `cargo test`; this pins the rule directly.
//!
//! The expected numbers were produced by the same probe under node v24.19.0
//! (the tag `scripts/node_tests/vendored` is fetched from) and match the
//! engine line for line. Fixtures are generated at runtime into a temp dir:
//! the hygiene gate forbids `.js` sources in the repository.
//!
//! Standalone binary (the `g_*.rs` / `tests/suites/` files belong to other
//! streams).

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const PROBE: &str = r#"
'use strict';
const CASE = process.env.BE_CASE;
const state = { emissions: 0, ticks: 0, timers: 0 };
process.on('exit', () => {
  console.log('RESULT ' + CASE + ' ' + state.emissions + ' ' + state.ticks + ' ' + state.timers);
});
const emit = () => { state.emissions += 1; };

switch (CASE) {
  case 'once-does-nothing':
    // A listener that schedules nothing: exactly one emission.
    process.on('beforeExit', emit);
    break;
  case 'on-timer-chain': {
    // Three emissions: the first two handlers arm a timer, the third does not.
    process.on('beforeExit', () => {
      emit();
      if (state.emissions < 3) { state.timers += 1; setTimeout(() => {}, 1); }
    });
    break;
  }
  case 'nexttick-only':
    // A bare nextTick runs inside the emission and does NOT keep the loop
    // alive: one emission, one tick.
    process.on('beforeExit', () => {
      emit();
      process.nextTick(() => { state.ticks += 1; });
    });
    break;
  case 'nexttick-arms-timer':
    // The nextTick callback arms a timer: the loop is alive again, so the
    // next exhaustion emits a second time (the re-emission is decided after
    // the handler's own nextTick/microtask drain has run).
    process.on('beforeExit', () => {
      emit();
      if (state.emissions === 1) {
        process.nextTick(() => {
          state.ticks += 1;
          state.timers += 1;
          setTimeout(() => {}, 1);
        });
      }
    });
    break;
  case 'await-arms-timer':
    // Same, one microtask generation deeper (async handler body).
    process.on('beforeExit', async () => {
      emit();
      if (state.emissions === 1) {
        await null;
        state.timers += 1;
        setTimeout(() => {}, 1);
      }
    });
    break;
  case 'await-deep-arms-timer':
    // Two microtask generations deeper: still a live loop.
    process.on('beforeExit', async () => {
      emit();
      if (state.emissions === 1) {
        await null;
        await null;
        state.timers += 1;
        setTimeout(() => {}, 1);
      }
    });
    break;
  case 'immediate-chain': {
    // setImmediate from the handler: re-emission.
    process.on('beforeExit', () => {
      emit();
      if (state.emissions < 3) { state.timers += 1; setImmediate(() => {}); }
    });
    break;
  }
  default:
    throw new Error('unknown case ' + CASE);
}
"#;

/// `case` → (emissions, ticks, timers) observed under node v24.19.0.
const NODE_RESULTS: &[(&str, u32, u32, u32)] = &[
    ("once-does-nothing", 1, 0, 0),
    ("on-timer-chain", 3, 0, 2),
    ("nexttick-only", 1, 1, 0),
    ("nexttick-arms-timer", 2, 1, 1),
    ("await-arms-timer", 2, 0, 1),
    ("await-deep-arms-timer", 2, 0, 1),
    ("immediate-chain", 3, 0, 2),
];

struct Workspace {
    dir: PathBuf,
    agent_dir: PathBuf,
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Workspace {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "pir-beforeexit-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_millis()
        ));
        let agent_dir = dir.join(".pir-agent");
        std::fs::create_dir_all(&agent_dir).expect("workspace create");
        Self { dir, agent_dir }
    }

    fn write(&self, name: &str, source: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, source).expect("fixture write");
        path
    }

    /// Run one probe case with a hard timeout so a hung child fails the test
    /// instead of hanging the suite.
    fn run_case(&self, case: &str) -> (bool, String, String) {
        let script = self.dir.join("probe.js");
        let mut child = Command::new(env!("CARGO_BIN_EXE_pir"))
            .env("PIR_AGENT_DIR", &self.agent_dir)
            .env("BE_CASE", case)
            .current_dir(&self.dir)
            .arg("js")
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("pir binary should start");
        let status = wait_bounded(&mut child, Duration::from_secs(30));
        let (stdout, stderr) = drain(&mut child);
        let ok = status.is_some_and(|s| s.success());
        (ok, stdout, stderr)
    }
}

fn wait_bounded(child: &mut Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait().expect("wait on pir child") {
            Some(status) => return Some(status),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn drain(child: &mut Child) -> (String, String) {
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    (stdout, stderr)
}

fn parse_result(case: &str, stdout: &str) -> Option<(u32, u32, u32)> {
    let line = stdout.lines().find(|line| line.starts_with("RESULT "))?;
    let mut parts = line.split_whitespace();
    assert_eq!(parts.next(), Some("RESULT"));
    assert_eq!(parts.next(), Some(case), "RESULT case mismatch: {line}");
    let mut number = |field: &str| -> u32 {
        parts
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("RESULT {case}: bad {field} in {line:?}"))
    };
    Some((number("emissions"), number("ticks"), number("timers")))
}

#[test]
fn before_exit_reemission_matches_node() {
    let ws = Workspace::new("semantics");
    ws.write("probe.js", PROBE);
    let mut problems = Vec::new();
    for (case, emissions, ticks, timers) in NODE_RESULTS {
        let (ok, stdout, stderr) = ws.run_case(case);
        if !ok {
            problems.push(format!(
                "{case}: pir exited non-zero or timed out\nstdout: {stdout}\nstderr: {stderr}"
            ));
            continue;
        }
        match parse_result(case, &stdout) {
            Some(actual) if actual == (*emissions, *ticks, *timers) => {}
            Some(actual) => problems.push(format!(
                "{case}: expected (emissions, ticks, timers) = {:?}, got {:?}",
                (emissions, ticks, timers),
                actual
            )),
            None => problems.push(format!("{case}: no RESULT line\nstdout: {stdout}")),
        }
    }
    assert!(
        problems.is_empty(),
        "beforeExit semantics diverged from node v24.19.0:\n{}",
        problems.join("\n")
    );
}
