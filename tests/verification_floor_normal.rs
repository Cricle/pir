//! D1: the declared-check verification floor for a plain `normal` run.
//!
//! Before this item the execution contract had exactly one producer — plan
//! approval. A `normal` run therefore had **no** correctness guarantee at all:
//! a tool error was a result and the loop continued (`mod.rs` `contract_gate_
//! continuation` returning `None` at `active_contract()?`).
//!
//! These tests pin the floor and, just as importantly, the four ways a naive
//! implementation ships a gate that looks right and cannot be satisfied:
//!
//! 1. `evidence_baseline` left `None` fails closed
//!    (`pir-builtin-mode/src/journal.rs:86-91`), so every run would end
//!    UNVERIFIED no matter what the model did.
//! 2. Re-arming through `set_active_contract` on every read clears the pass
//!    markers (`mod.rs:608-621`), so a satisfied contract never settles.
//! 3. `complete_contract` retires a satisfied contract (`mod.rs`), so a second
//!    prompt in the same session silently loses the floor unless it re-arms.
//! 4. The `before_agent_start` arm only injects the verification text when the
//!    plan is non-empty, and a declared check has no plan — so the model would
//!    be gated while never being told the exact command to run.
//!
//! Checks are injected by writing the session's `.checks.json` sidecar rather
//! than through `PIR_CHECK`: the environment is process-global, and two of
//! these tests would otherwise race each other.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde_json::{json, Value};

use pir::plugins::services::{
    PluginServices, ServiceResult, SessionService, StatusService, ToolInfo,
};
use pir::plugins::{instantiate_builtin, PluginHost};

/// The declared check every test in this file declares.
const CHECK: &str = "python -m unittest";

struct FakeStatus {
    cells: Mutex<Vec<(String, String)>>,
}

impl Default for FakeStatus {
    fn default() -> Self {
        Self {
            cells: Mutex::new(Vec::new()),
        }
    }
}

impl FakeStatus {
    fn text(&self, key: &str) -> Option<String> {
        self.cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .rev()
            .find(|(cell, _)| cell == key)
            .map(|(_, text)| text.clone())
    }
}

impl StatusService for FakeStatus {
    fn set_status(&self, key: &str, text: Option<&str>) {
        let mut cells = self
            .cells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match text {
            Some(text) => cells.push((key.to_owned(), text.to_owned())),
            None => cells.retain(|(cell, _)| cell != key),
        }
    }
}

struct FakeSession {
    idle: AtomicBool,
    active: Vec<String>,
    all: Vec<ToolInfo>,
    path: Option<PathBuf>,
    entries: RwLock<Option<Value>>,
}

impl FakeSession {
    fn with_path(path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            idle: AtomicBool::new(true),
            active: ["read", "bash", "edit", "write"]
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            all: ["read", "bash", "edit", "write"]
                .iter()
                .map(|name| {
                    serde_json::from_value(json!({ "name": name })).expect("tool info fixture")
                })
                .collect(),
            path: Some(path),
            entries: RwLock::new(None),
        })
    }

    /// The transcript the baseline is computed from. The host exposes it as a
    /// list of *views*, each holding `messages` (or a single `entry.message`);
    /// `shell_call_baseline` walks that shape, so a flat message array would
    /// silently produce an empty baseline. `None` makes the function return
    /// `None`, which the gate treats as "unknown history" and fails closed on.
    fn set_entries(&self, entries: Option<Value>) {
        *self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = entries;
    }

    /// Wrap a flat message list the way the real host does.
    fn set_message_views(&self, messages: &[Value]) {
        self.set_entries(Some(json!([{ "messages": messages }])));
    }
}

impl SessionService for FakeSession {
    fn is_idle(&self) -> bool {
        self.idle.load(Ordering::SeqCst)
    }

    fn active_tools(&self) -> Arc<[Arc<str>]> {
        self.active
            .iter()
            .map(|name| Arc::<str>::from(name.as_str()))
            .collect::<Vec<_>>()
            .into()
    }

    fn all_tools(&self) -> Arc<[ToolInfo]> {
        self.all.clone().into()
    }

    fn session_path(&self) -> Option<Arc<std::path::Path>> {
        self.path.clone().map(Arc::from)
    }

    fn entries(&self) -> Option<Arc<Value>> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .map(Arc::new)
    }
}

/// A message service stub. The gate nudge is returned by the plugin as a
/// `continue: true` payload rather than sent through this service, so it stays
/// unused here — but the host requires one to be bound.
struct FakeMessages;

impl pir::plugins::services::MessageService for FakeMessages {}

fn dir_for(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-floor-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Write the session's declared checks the way the CLI does (`--check` writes
/// this sidecar at `session_start`).
fn write_checks(dir: &std::path::Path, checks: &[&str]) {
    let path = dir.join("session.jsonl.checks.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({ "version": 1, "checks": checks })).expect("encode checks"),
    )
    .expect("write checks sidecar");
}

struct Floor {
    dir: PathBuf,
    host: PluginHost,
    status: Arc<FakeStatus>,
    session: Arc<FakeSession>,
    /// `false` for a `Floor` that shares its directory with another one (a
    /// resume test), where dropping the first must not delete the files the
    /// second is about to reopen.
    cleanup: bool,
}

impl Drop for Floor {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

impl Floor {
    fn set_cleanup(&mut self, cleanup: bool) {
        self.cleanup = cleanup;
    }
}

/// Build a `normal` session over a temp dir with `checks` declared, and drive
/// the `session_start` that loads them.
async fn open_floor(label: &str, checks: Option<&[&str]>) -> Floor {
    let dir = dir_for(label);
    if let Some(checks) = checks {
        write_checks(&dir, checks);
    }
    open_floor_in(dir, label).await
}

/// Same, over a directory the caller owns (so a reopen can target it).
async fn open_floor_in(dir: PathBuf, label: &str) -> Floor {
    let _ = label;
    let session = FakeSession::with_path(dir.join("session.jsonl"));
    // An enumerable (empty) transcript: the baseline is a complete empty set
    // rather than "unknown", which is what a real host provides.
    session.set_message_views(&[]);
    let status = Arc::new(FakeStatus::default());
    let plugin = instantiate_builtin("mode").expect("mode builtin is compiled in");
    let host = PluginHost::build(None, vec![Arc::clone(&plugin)]);
    host.bind_context(
        PluginServices::new()
            .with_session(Arc::clone(&session) as Arc<dyn SessionService>)
            .with_status(Arc::clone(&status) as Arc<dyn StatusService>)
            .with_messages(Arc::new(FakeMessages))
            .build(),
    );
    let mut start = json!({ "type": "session_start", "reason": "new" });
    host.emit("session_start", &mut start)
        .await
        .expect("emit session_start");
    Floor {
        dir,
        host,
        status,
        session,
        cleanup: true,
    }
}

async fn emit_prompt(floor: &Floor) -> Value {
    let mut payload = json!({
        "type": "before_agent_start",
        "messageType": "user",
        "prompt": "do the task",
        "systemPrompt": "base prompt",
        "systemPromptOptions": { "sections": {} }
    });
    let outcome = floor
        .host
        .emit("before_agent_start", &mut payload)
        .await
        .expect("emit before_agent_start");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    // The plugin mutates `payload` in place (sections / systemPrompt); the
    // returned `result` is the same shape for the caller. Serialize both so a
    // section written either way is visible to the assertions.
    let mut merged = payload;
    merged["pluginResult"] = outcome.result;
    merged
}

async fn settle(floor: &Floor, messages: Vec<Value>) -> Value {
    let mut payload = json!({
        "type": "agent_before_settle",
        "outcome": "completed",
        "entries": [],
        "continue": false,
        "context": {
            "contextEntries": [],
            "contextMessages": messages.clone(),
            "llmMessages": messages,
            "pendingMessages": [],
            "canContinue": false
        }
    });
    let outcome = floor
        .host
        .emit("agent_before_settle", &mut payload)
        .await
        .expect("emit agent_before_settle");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
    let mut merged = payload;
    merged["pluginResult"] = outcome.result;
    merged
}

/// A settled turn whose model claims success without running the check.
fn claimed_done_turn() -> Vec<Value> {
    vec![
        json!({
            "role": "user",
            "content": [{ "type": "text", "text": "do the task" }]
        }),
        json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": "done, verified by inspection" }],
            "stopReason": "stop"
        }),
    ]
}

/// A turn that ran `command` with the given exit code.
fn ran_command_turn(call_id: &str, command: &str, exit_code: i64) -> Vec<Value> {
    let text = if exit_code == 0 {
        "Ran 3 tests OK\nCommand exited with code 0".to_owned()
    } else {
        "FAILED (failures=1)\nCommand exited with code 1".to_owned()
    };
    vec![
        json!({
            "role": "user",
            "content": [{ "type": "text", "text": "do the task" }]
        }),
        json!({
            "role": "assistant",
            "content": [{
                "type": "toolCall",
                "id": call_id,
                "name": "bash",
                "arguments": { "command": command }
            }],
            "stopReason": "toolUse"
        }),
        json!({
            "role": "toolResult",
            "toolCallId": call_id,
            "toolName": "bash",
            "isError": exit_code != 0,
            "content": [{ "type": "text", "text": text }],
            "details": { "exitCode": exit_code }
        }),
        json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": "done" }],
            "stopReason": "stop"
        }),
    ]
}

fn was_gated(result: &Value) -> bool {
    if result["continue"].as_bool() == Some(true) {
        return true;
    }
    serde_json::to_string(result)
        .map(|text| text.contains("verification_gate"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// The floor
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn a_claimed_success_without_the_check_is_not_complete() {
    let floor = open_floor("claimed", Some(&[CHECK])).await;
    assert_eq!(floor.status.text("mode"), None, "stays a plain normal run");
    let _ = emit_prompt(&floor).await;

    let result = settle(&floor, claimed_done_turn()).await;
    assert!(
        was_gated(&result),
        "a claim of success without the declared check must not settle: {result}"
    );
    let serialized = serde_json::to_string(&result).expect("serialize gate result");
    assert!(
        serialized.contains("verification_gate"),
        "the run is continued by the verification gate: {serialized}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn running_the_declared_check_settles() {
    let floor = open_floor("pass", Some(&[CHECK])).await;
    let _ = emit_prompt(&floor).await;

    let result = settle(&floor, ran_command_turn("call-ok", CHECK, 0)).await;
    assert!(
        !was_gated(&result),
        "a successful declared check settles: {result}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_failing_check_does_not_satisfy_the_floor() {
    let floor = open_floor("failing", Some(&[CHECK])).await;
    let _ = emit_prompt(&floor).await;

    let result = settle(&floor, ran_command_turn("call-bad", CHECK, 1)).await;
    assert!(
        was_gated(&result),
        "a non-zero check keeps the floor: {result}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_different_command_does_not_satisfy_the_floor() {
    let floor = open_floor("wrong-cmd", Some(&[CHECK])).await;
    let _ = emit_prompt(&floor).await;

    // Evidence matching is verbatim by design; a lookalike is not the check.
    let result = settle(
        &floor,
        ran_command_turn("call-other", "python -m pytest", 0),
    )
    .await;
    assert!(
        was_gated(&result),
        "only the exact declared command counts: {result}"
    );
}

// ---------------------------------------------------------------------------
// Trap 4: the model must be told which command satisfies the gate
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn the_prompt_names_the_declared_check() {
    let floor = open_floor("prompt", Some(&[CHECK])).await;

    let payload = emit_prompt(&floor).await;
    let serialized = serde_json::to_string(&payload).expect("serialize prompt payload");
    assert!(
        serialized.contains(CHECK),
        "the run is gated, so the prompt must name the exact command: {serialized}"
    );
    assert!(
        serialized.contains("EXACTLY"),
        "the model must be told the command is matched verbatim: {serialized}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn no_declared_check_changes_nothing() {
    let floor = open_floor("none", None).await;

    let payload = emit_prompt(&floor).await;
    let serialized = serde_json::to_string(&payload).expect("serialize prompt payload");
    assert!(
        !serialized.contains("declared_verification"),
        "no declared check must add no prompt section: {serialized}"
    );
    let result = settle(&floor, claimed_done_turn()).await;
    assert!(
        !was_gated(&result),
        "without a declared check the run settles exactly as before: {result}"
    );
}

// ---------------------------------------------------------------------------
// Trap 1: history before the task cannot satisfy it
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn a_check_run_before_the_prompt_does_not_count() {
    let floor = open_floor("baseline", Some(&[CHECK])).await;

    // The transcript already contains a successful run of the check from an
    // earlier prompt. The baseline is computed over it, so it is investigation
    // evidence, not this run's verification.
    let earlier = ran_command_turn("call-earlier", CHECK, 0);
    floor.session.set_message_views(&earlier);
    // A prompt boundary arms a fresh contract with a fresh baseline.
    let _ = emit_prompt(&floor).await;
    // Re-set so the *gate* also sees the old turn in the transcript.
    floor.session.set_message_views(&earlier);

    let result = settle(&floor, earlier).await;
    assert!(
        was_gated(&result),
        "evidence from before the prompt cannot satisfy the check: {result}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn an_unknown_transcript_fails_closed() {
    let floor = open_floor("unknown", Some(&[CHECK])).await;
    // A host that cannot enumerate the transcript yields `None`, which the
    // gate must treat as unknown history rather than as "nothing happened".
    floor.session.set_entries(None);
    let _ = emit_prompt(&floor).await;

    let result = settle(&floor, ran_command_turn("call-ok", CHECK, 0)).await;
    assert!(
        was_gated(&result),
        "an unknown baseline must fail closed: {result}"
    );
}

// ---------------------------------------------------------------------------
// Traps 2 and 3: one prompt must not break the next
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn a_settled_check_does_not_re_arm_itself_within_the_same_run() {
    let floor = open_floor("settle-once", Some(&[CHECK])).await;
    let _ = emit_prompt(&floor).await;

    let passed = settle(&floor, ran_command_turn("call-ok", CHECK, 0)).await;
    assert!(!was_gated(&passed), "first settle passes: {passed}");

    // The same run settling again (no new prompt) must stay settled: the
    // retired contract must not be re-installed by the gate's own reads.
    let again = settle(&floor, ran_command_turn("call-ok", CHECK, 0)).await;
    assert!(
        !was_gated(&again),
        "a retired contract must not re-arm within the run: {again}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_second_prompt_is_gated_again() {
    let floor = open_floor("second-prompt", Some(&[CHECK])).await;

    let _ = emit_prompt(&floor).await;
    let passed = settle(&floor, ran_command_turn("call-1", CHECK, 0)).await;
    assert!(!was_gated(&passed), "first prompt passes: {passed}");

    // Second prompt in the same session: `complete_contract` retired the
    // contract, so the prompt boundary must arm a fresh one.
    let _ = emit_prompt(&floor).await;
    let result = settle(&floor, claimed_done_turn()).await;
    assert!(
        was_gated(&result),
        "a second prompt must regain the floor: {result}"
    );
}

// ---------------------------------------------------------------------------
// The CLI handoff
// ---------------------------------------------------------------------------

/// `--check` reaches the plugin as `PIR_CHECK` (the same process-local handoff
/// `--perf-profile` uses), the plugin persists it to the session sidecar, and
/// the floor arms from it.
///
/// This pins a process-global variable, so it needs the same exclusivity
/// guard the other env-pinning suites use (`crate::common`): under nextest
/// (CI) the test is alone in its process and runs; under plain `cargo test`
/// the merged harness shares the process with every other suite and this
/// reports a SKIP rather than passing on a value it may have raced for.
#[tokio::test(flavor = "current_thread")]
async fn the_cli_check_handoff_reaches_the_floor_and_persists() {
    if crate::common::skip_without_nextest("the_cli_check_handoff_reaches_the_floor_and_persists") {
        return;
    }
    // No sidecar: the check must come from the environment, exactly as the CLI
    // sets it.
    std::env::set_var("PIR_CHECK", format!("{CHECK}\ncargo clippy"));
    let floor = open_floor("env-handoff", None).await;
    std::env::remove_var("PIR_CHECK");

    // Persisted for the resume path.
    let sidecar = floor.dir.join("session.jsonl.checks.json");
    let text = std::fs::read_to_string(&sidecar).expect("checks sidecar written");
    let stored: Value = serde_json::from_str(&text).expect("sidecar is JSON");
    let checks = stored["checks"].as_array().expect("checks array");
    assert_eq!(checks.len(), 2, "{stored}");
    assert_eq!(checks[0].as_str(), Some(CHECK), "{stored}");
    assert_eq!(checks[1].as_str(), Some("cargo clippy"), "{stored}");

    // And it is armed, not merely stored.
    let _ = emit_prompt(&floor).await;
    let gated = settle(&floor, claimed_done_turn()).await;
    assert!(
        was_gated(&gated),
        "the CLI-declared check gates the run: {gated}"
    );
}

/// The session's own sidecar is what a resumed task reloads, and it wins when
/// the CLI passes no `--check` on the new invocation. This is the resume half
/// of the handoff, tested without touching the process environment (which
/// would race every other test that parses a CLI).
#[tokio::test(flavor = "current_thread")]
async fn a_resumed_session_reloads_its_persisted_check() {
    let dir = dir_for("resume");
    write_checks(&dir, &[CHECK]);

    // First open: the sidecar is read and armed. It must not delete the dir on
    // drop, because the resume below reopens it.
    let mut first = open_floor_in(dir.clone(), "resume-first").await;
    first.set_cleanup(false);
    let _ = emit_prompt(&first).await;
    assert!(
        was_gated(&settle(&first, claimed_done_turn()).await),
        "the persisted check gates the first run"
    );
    drop(first);

    // Reopen the same directory (a `--continue`): the check comes back from
    // disk, with no `--check` on the command line at all.
    let resumed = open_floor_in(dir.clone(), "resume-second").await;
    let _ = emit_prompt(&resumed).await;
    assert!(
        was_gated(&settle(&resumed, claimed_done_turn()).await),
        "a resumed session keeps the check it was started with"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// The floor never overrides a real plan contract
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn a_declared_check_does_not_replace_an_approved_plan() {
    let floor = open_floor("plan-wins", Some(&[CHECK])).await;
    assert_eq!(floor.status.text("mode"), None);

    // Enter plan mode: the floor must not arm there. Plan installs the real
    // contract on approval, and that contract is strictly stronger.
    floor
        .host
        .dispatch_command("mode", "plan")
        .await
        .expect("enter plan mode");
    assert_eq!(
        floor.status.text("mode").as_deref(),
        Some("MODE: PLAN"),
        "plan mode is entered"
    );
    let payload = emit_prompt(&floor).await;
    let serialized = serde_json::to_string(&payload).expect("serialize prompt payload");
    assert!(
        !serialized.contains("declared_verification"),
        "the declared-check floor must not arm in plan mode: {serialized}"
    );
}
