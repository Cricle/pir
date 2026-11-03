//! Host-backed SDK `SessionManager` + `createAgentSession` (P1 collection).
//!
//! Two boundaries are covered:
//!
//! 1. The extension-facing `SessionManager` class over the
//!    `__pir_host.pirSession*` ops: opening ANY session file (not just the
//!    live one), the read face (`getBranch`/`getTree`/`getLeafId`/`getEntry`/
//!    `getChildren`/`getLabel`/`getEntries`/...), `createBranchedSession`
//!    forking root-to-leaf into a NEW session file, discovery (`findById`,
//!    `list`, `listAll`), and the locked write face (standard error).
//!    Consumers: pi-phone's session tree/branch panel, pi-subagents
//!    fork-context.
//!
//! 2. `createAgentSession`: the pir counterpart spawns `pir print --mode
//!    json` child processes (upstream's compiled-binary-host launch shape).
//!    A live round trip runs against a loopback OpenAI-completions SSE
//!    server: session → prompt → streamed events + reply → second prompt
//!    continuing the same session file → dispose.
//!
//! Fixtures are generated at runtime into per-test temp directories (the
//! hygiene gate forbids tracked `.ts`/`.js` files); environment variables are
//! serialized through a mutex because the spawned child inherits the process
//! environment.

use std::path::{Path, PathBuf};

use pir::ai::types::{
    AssistantMessage, ContentBlock, Message, StopReason, TextContent, ToolResultMessage,
    ToolResultRole, UserMessage, UserRole,
};
use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use pir::session_manager::search::{
    fuzzy_match, match_session_search, normalize_whitespace_lower,
    reset_session_search_corpus_builds, session_search_corpus_builds, session_search_corpus_bytes,
    session_search_corpus_cache_stats, session_search_corpus_fingerprint_bytes,
    session_search_corpus_fingerprints, session_search_lazy_corpus_cache_stats,
    session_search_lazy_corpus_loads, SessionSearchQuery,
};
use pir::session_manager::{
    reset_session_filter_corpus_bytes, reset_session_line_scan_bytes,
    reset_session_listing_corpus_bytes, session_filter_corpus_bytes, session_line_scan_bytes,
    session_listing_corpus_bytes, FileEntry, SessionManager, SessionSummary, StoredMessage,
};
use serde_json::{json, Value};

/// Per-test temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-session-ext-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The script-mode module runner boot (`pir js` defines the same shape; see
/// `main_cli/js_mode.rs` + `script_boot.js.txt`).
const SCRIPT_RUN_BOOT: &str = r#"
globalThis.__pirScriptRun = async (moduleName) => {
  try {
    await import(moduleName);
    return JSON.stringify({ ok: true });
  } catch (error) {
    const message = error && error.message !== undefined ? String(error.message) : String(error);
    const rawStack = error && error.stack ? String(error.stack) : "";
    const stack = rawStack.indexOf(message) >= 0 ? rawStack : message + (rawStack ? "\n" + rawStack : "");
    return JSON.stringify({ ok: false, message: message, stack: stack });
  }
};
"#;

fn script_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "script".to_string(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: pir::ai::util::now_ms(),
        sections: None,
    })
}

fn assistant_message(text: &str) -> Message {
    Message::Assistant(assistant_message_at(text, pir::ai::util::now_ms()))
}

fn assistant_message_at(text: &str, timestamp: u64) -> AssistantMessage {
    AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        timestamp,
        ..Default::default()
    }
}

fn tool_result_message(call_id: &str, tool_name: &str, timestamp: u64) -> ToolResultMessage {
    ToolResultMessage {
        role: ToolResultRole::ToolResult,
        tool_call_id: call_id.into(),
        tool_name: tool_name.into(),
        content: vec![ContentBlock::Text(TextContent {
            text: format!("result {call_id}"),
            text_signature: None,
        })],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        structured_content: None,
        nested_calls: None,
        is_error: false,
        duration_ms: None,
        timestamp,
    }
}

/// A branched fixture session: m1(user) ← m2(assistant) on the trunk and
/// m3(assistant) on a branch from m1; the leaf is m3.
fn branched_fixture(dir: &Path, file_name: &str) -> (PathBuf, String, String, String) {
    let mut session = SessionManager::create(dir.to_str().expect("utf8 temp dir"), dir)
        .expect("fixture session create");
    let m1 = session
        .append_message(user_message("hello"))
        .expect("append user");
    let m2 = session
        .append_message(assistant_message("trunk reply"))
        .expect("append assistant");
    session.branch(&m1).expect("branch to m1");
    let m3 = session
        .append_message(assistant_message("branch reply"))
        .expect("append branch assistant");
    let file = session
        .session_file()
        .expect("fixture file planned")
        .to_path_buf();
    std::fs::rename(&file, dir.join(file_name)).expect("rename fixture");
    let file = dir.join(file_name);
    // Reopen so the fixture manager points at the stable file name.
    let reopened = SessionManager::open(&file).expect("reopen fixture");
    assert_eq!(reopened.leaf_id(), Some(m3.as_str()));
    (file, m1, m2, m3)
}

// ---------------------------------------------------------------------------
// 1. Host-backed SessionManager (pi-phone / pi-subagents consumption surface)
// ---------------------------------------------------------------------------

const SESSION_MANAGER_PROBE: &str = r#"
import { SessionManager } from "@earendil-works/pi-coding-agent";

export default (pi) => {};

const dir = __PIR_SESSION_DIR__;
const file = __PIR_SESSION_FILE__;
const m1 = __PIR_M1__;
const m3 = __PIR_M3__;
const out = {};

// Open an arbitrary session file (not the live session).
const opened = SessionManager.open(file);
out.sessionId = opened.getSessionId();
out.leafId = opened.getLeafId();
out.entryCount = opened.getEntries().length;
out.leafEntryText = opened.getLeafEntry()?.message?.content?.[0]?.text;
out.branchTexts = opened.getBranch().map((entry) => entry?.message?.content?.[0]?.text);
out.branchFromM1Texts = opened.getBranch(m1).map((entry) => entry?.id);
out.treeRoots = opened.getTree().length;
out.treeLeafIds = opened.getTree().flatMap((root) => [root.entry.id, ...root.children.map((c) => c.entry.id)]);
out.entryM1Role = opened.getEntry(m1)?.message?.role;
out.childrenOfM1 = opened.getChildren(m1).map((entry) => entry.id);
out.hasConversation = opened.hasConversation();
out.isPersisted = opened.isPersisted();
out.cwd = typeof opened.getCwd();
out.sessionDir = opened.getSessionDir();
out.headerType = opened.getHeader()?.type;
out.entryCountMethod = opened.getEntryCount();

// createBranchedSession forks root-to-m3 into a NEW file and continues there.
const branchedPath = opened.createBranchedSession(m3);
out.branchedPath = typeof branchedPath === "string";
out.branchedSameManager = opened.getSessionFile() === branchedPath;
out.branchedEntries = opened.getEntries().length;
out.branchedLeafText = opened.getLeafEntry()?.message?.content?.[0]?.text;

// The forked file is a real session on disk that reopens with the same path.
const reopened = SessionManager.open(branchedPath);
out.reopenedEntries = reopened.getEntries().length;
out.reopenedBranchTexts = reopened.getBranch().map((entry) => entry?.message?.content?.[0]?.text);

// pi-subagents fork-context face: open with a sessionDir override; the fork
// file must land in THAT directory.
const forksDir = dir + "/forks";
const withDir = SessionManager.open(file, forksDir);
const forkPath = withDir.createBranchedSession(m3);
out.forkInOverrideDir = typeof forkPath === "string"
  && String(forkPath).replace(/\\/g, "/").indexOf("/forks/") !== -1;

// Discovery.
const foundPath = SessionManager.findById(dir, withDir.getSessionId(), forksDir);
out.findById = typeof foundPath === "string";
out.foundIsFork = typeof foundPath === "string" && String(foundPath).replace(/\\/g, "/").indexOf("/forks/") !== -1;
out.listMissingDir = Array.isArray(await SessionManager.list(dir + "/no-such-dir"));

// Write face stays locked.
try {
  opened.appendMessage({ role: "user", content: "x" });
  out.writeFace = "no-throw";
} catch (error) {
  out.writeFace = error.message;
}

// In-memory face (pi-subagents storage kind "memory").
const memory = SessionManager.inMemory(dir);
out.memoryIsPersisted = memory.isPersisted();

globalThis.__smResult = out;
"#;

#[tokio::test(flavor = "current_thread")]
async fn session_manager_host_backed_surface() {
    let dir = TempDir::new("sm-surface");
    let (file, m1, m2, m3) = branched_fixture(dir.path(), "fixture.jsonl");

    let source = SESSION_MANAGER_PROBE
        .replace(
            "__PIR_SESSION_DIR__",
            &format!("{:?}", dir.path().to_string_lossy().replace('\\', "/")),
        )
        .replace(
            "__PIR_SESSION_FILE__",
            &format!("{:?}", file.to_string_lossy().replace('\\', "/")),
        )
        .replace("__PIR_M1__", &format!("{m1:?}"))
        .replace("__PIR_M3__", &format!("{m3:?}"));

    let rt = ExtensionRuntime::new(script_config_at(dir.path()))
        .await
        .expect("runtime construction");
    rt.eval_boot(SCRIPT_RUN_BOOT).await.expect("script boot");
    rt.seed_module("pir-ext:session_manager_probe.js", &source)
        .expect("seed module");
    let run = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        rt.run_script("pir-ext:session_manager_probe.js"),
    )
    .await
    .expect("probe timed out");
    match run {
        Ok(Ok(())) => {}
        Ok(Err(stack)) => panic!("probe module failed: {stack}"),
        Err(error) => panic!("script run failed: {error}"),
    }
    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__smResult ?? null)")
        .await
        .expect("read probe results");

    assert!(v["sessionId"].is_string(), "{v}");
    assert_eq!(v["entryCount"], 3, "{v}");
    assert_eq!(v["leafId"], json!(m3), "{v}");
    assert_eq!(v["leafEntryText"], "branch reply", "{v}");
    assert_eq!(v["branchTexts"], json!(["hello", "branch reply"]), "{v}");
    assert_eq!(v["branchFromM1Texts"], json!([m1]), "{v}");
    assert_eq!(v["treeRoots"], 1, "{v}");
    assert_eq!(v["treeLeafIds"], json!([m1, m2, m3]), "{v}");
    assert_eq!(v["entryM1Role"], "user", "{v}");
    assert_eq!(v["childrenOfM1"].as_array().map(Vec::len), Some(2), "{v}");
    assert_eq!(v["hasConversation"], true, "{v}");
    assert_eq!(v["isPersisted"], true, "{v}");
    assert_eq!(v["cwd"], "string", "{v}");
    assert_eq!(
        v["sessionDir"],
        dir.path().to_string_lossy().replace('\\', "/"),
        "{v}"
    );
    assert_eq!(v["headerType"], "session", "{v}");
    assert_eq!(v["entryCountMethod"], 3, "{v}");

    assert_eq!(v["branchedPath"], true, "{v}");
    assert_eq!(v["branchedSameManager"], true, "{v}");
    assert_eq!(v["branchedEntries"], 2, "{v}");
    assert_eq!(v["branchedLeafText"], "branch reply", "{v}");
    assert_eq!(v["reopenedEntries"], 2, "{v}");
    assert_eq!(
        v["reopenedBranchTexts"],
        json!(["hello", "branch reply"]),
        "{v}"
    );

    assert_eq!(v["forkInOverrideDir"], true, "{v}");
    assert_eq!(v["findById"], true, "{v}");
    assert_eq!(v["foundIsFork"], true, "{v}");
    assert_eq!(v["listMissingDir"], true, "{v}");
    assert!(
        v["writeFace"]
            .as_str()
            .unwrap_or_default()
            .contains("SessionManager.appendMessage is not available in the pir extension runtime"),
        "{v}"
    );
    assert_eq!(v["memoryIsPersisted"], false, "{v}");
}

// ---------------------------------------------------------------------------
// 2. createAgentSession (pir print --mode json child-process round trip)
// ---------------------------------------------------------------------------

const CREATE_AGENT_SESSION_PROBE: &str = r#"
import { SessionManager, createAgentSession } from "@earendil-works/pi-coding-agent";

export default (pi) => {};

const cwd = __PIR_CWD__;
const sessionDir = __PIR_SESSION_DIR__;
const out = {};

const sm = SessionManager.create(cwd, sessionDir);
const { session } = await createAgentSession({
  cwd: cwd,
  sessionManager: sm,
  model: { provider: "testprov", id: "test-model" },
});

const events = [];
// DIAGNOSTIC: keep the raw turn-2 event trail. A `--mode json` child exits 0
// even for an errored turn (the failure is reported in-band), so the terminal
// `message_end` payload is the only place the stopReason/errorMessage of a
// failed second turn is visible.
const rawEvents = [];
session.subscribe((event) => {
  events.push(event.type);
  rawEvents.push(event);
});

const reply1 = await session.prompt("Say hello");
out.reply1 = reply1;
out.sessionId = session.sessionId;
out.sessionFile = typeof session.sessionFile === "string" && session.sessionFile.endsWith(".jsonl");
out.messageCount1 = session.messages.length;
out.hasHeaderEvent = events.some((type) => type === "agent_start" || type === "message_start");
out.hasEndEvent = events.includes("message_end");

const eventsBefore = events.length;
const reply2 = await session.prompt("Continue");
out.reply2 = reply2;
out.messageCount2 = session.messages.length;
out.eventsGrew = events.length > eventsBefore;
{
  const turn2 = rawEvents.slice(eventsBefore);
  const assistantEnds = turn2.filter(
    (event) => event.type === "message_end" && event.message && event.message.role === "assistant",
  );
  out.turn2 = {
    eventTypes: turn2.map((event) => event.type),
    assistantEnds: assistantEnds.map((event) => ({
      stopReason: event.message.stopReason,
      errorMessage: event.message.errorMessage,
      contentTypes: Array.isArray(event.message.content)
        ? event.message.content.map((block) => (block && block.type) || "unknown")
        : null,
      raw: JSON.stringify(event.message).slice(0, 2000),
    })),
  };
}

out.activeTools = Array.isArray(session.getActiveToolNames());
out.bindExtensions = typeof session.bindExtensions === "function";
out.extensionRunnerHandlers = session.extensionRunner.hasHandlers("session_shutdown") === false;

try {
  session.steer("x");
  out.steerFace = "no-throw";
} catch (error) {
  out.steerFace = error.message;
}

session.dispose();
out.disposedPrompt = await session.prompt("after dispose").then(
  () => "resolved",
  (error) => (String(error.message).indexOf("disposed") !== -1 ? "rejected-disposed" : error.message),
);

globalThis.__casResult = out;
"#;

#[tokio::test(flavor = "current_thread")]
async fn create_agent_session_child_round_trip() {
    let dir = TempDir::new("cas");
    let agent_dir = TempDir::new("cas-agent");
    let session_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&session_dir).expect("session dir");

    // Loopback OpenAI-completions SSE server: one response per connection,
    // accepting until dropped (the child makes one request per prompt run).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback bind");
    let port = listener.local_addr().expect("local addr").port();
    // The handle is held for the whole test (`_server`): dropping it would not
    // stop the thread, but the diagnostic re-run below needs the listener
    // still accepting, so the binding stays alive until the test ends.
    let _server = std::thread::spawn(move || {
        let sse = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"mock reply\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n",
            "data: [DONE]\n\n",
        );
        for stream in listener.incoming() {
            // A transient accept error (an aborted keep-alive connection) must
            // not end the fixture: the round-trip test drives SEVERAL turns, and
            // a `break` here refused the second one (observed as an empty
            // assistant reply with exit 0).
            let Ok(mut stream) = stream else { continue };
            use std::io::Write;
            // Read the WHOLE request before answering: responding (and
            // closing) while the client is still writing its body is a reset
            // race, and the second turn's request is much bigger (the replayed
            // transcript + the tool-loadout declaration) than the first's —
            // the client then aborts the write and the run ends with an empty
            // assistant while the child still exits 0.
            read_full_request(&mut stream);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                sse.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });

    // Isolated agent dir: models.json defines the test provider; the spawned
    // child inherits these environment settings.
    let models = json!({
        "providers": {
            "testprov": {
                "name": "Test Provider",
                "baseUrl": format!("http://127.0.0.1:{port}/v1"),
                "apiKey": "test-key",
                "api": "openai-completions",
                "models": [{ "id": "test-model", "name": "Test Model" }],
            }
        }
    });
    std::fs::create_dir_all(agent_dir.path()).ok();
    std::fs::write(agent_dir.path().join("models.json"), models.to_string())
        .expect("write models.json");
    let exe = env!("CARGO_BIN_EXE_pir").to_string();
    std::env::set_var("PIR_EXECUTABLE", &exe);
    std::env::set_var("PIR_AGENT_DIR", agent_dir.path());
    std::env::set_var("TESTPROV_API_KEY", "test-key");

    let source = CREATE_AGENT_SESSION_PROBE
        .replace(
            "__PIR_CWD__",
            &format!("{:?}", dir.path().to_string_lossy().replace('\\', "/")),
        )
        .replace(
            "__PIR_SESSION_DIR__",
            &format!("{:?}", session_dir.to_string_lossy().replace('\\', "/")),
        );

    let rt = ExtensionRuntime::new(script_config_at(dir.path()))
        .await
        .expect("runtime construction");
    rt.eval_boot(SCRIPT_RUN_BOOT).await.expect("script boot");
    rt.seed_module("pir-ext:create_agent_session_probe.js", &source)
        .expect("seed module");
    let run = tokio::time::timeout(
        std::time::Duration::from_secs(240),
        rt.run_script("pir-ext:create_agent_session_probe.js"),
    )
    .await
    .expect("createAgentSession round trip timed out");
    match run {
        Ok(Ok(())) => {}
        Ok(Err(stack)) => panic!("probe module failed: {stack}"),
        Err(error) => panic!("script run failed: {error}"),
    }
    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__casResult ?? null)")
        .await
        .expect("read probe results");

    // DIAGNOSTIC (red-run aid; see the lane report): the SDK shim spawns one
    // `pir print --mode json` child per prompt and swallows that child's
    // stderr whenever it exits 0 — and `--mode json` deliberately exits 0 with
    // an errored/aborted turn reported IN-BAND as events (print-mode.ts:139-156,
    // pinned by `suites/cli_entry_contract.rs`), so a failed second turn
    // reaches the probe as an empty reply with no stderr anywhere. When the
    // second reply is not the mock text, re-run the exact command the shim
    // builds for turn 2 and put the child's own stdout/stderr (plus the
    // probe's raw turn-2 event trail) into the assertion message. The re-run
    // only happens on the failing path.
    let evidence = if v["reply2"].as_str() == Some("mock reply") {
        String::new()
    } else {
        format!(
            "\n--- child turn-2 evidence ---\n{}\n--- probe turn-2 trail ---\n{}",
            child_turn_diagnostic(&session_dir, "Continue"),
            serde_json::to_string_pretty(&v["turn2"])
                .unwrap_or_else(|error| format!("<turn2 trail unavailable: {error}>"))
        )
    };

    assert_eq!(v["reply1"], "mock reply", "{v}");
    assert!(v["sessionId"].is_string(), "{v}");
    assert!(v["sessionFile"] == true, "{v}");
    // The child now emits the upstream message lifecycle: the initial user
    // message and the tool-loadout declaration are each announced with
    // `message_start`/`message_end`, and the shim accumulates those `message_end`
    // frames. Turn 1 therefore holds user + system declaration + assistant;
    // turn 2 adds only user + assistant (the declaration is unchanged).
    assert_eq!(v["messageCount1"], 3, "{v}");
    assert_eq!(v["hasHeaderEvent"], true, "{v}");
    assert_eq!(v["hasEndEvent"], true, "{v}");
    assert_eq!(v["reply2"], "mock reply", "{v}{evidence}");
    assert_eq!(v["messageCount2"], 5, "{v}");
    assert_eq!(v["eventsGrew"], true, "{v}");
    assert_eq!(v["activeTools"], true, "{v}");
    assert_eq!(v["bindExtensions"], true, "{v}");
    assert_eq!(v["extensionRunnerHandlers"], true, "{v}");
    assert_eq!(
        v["steerFace"], "AgentSession.steer is not available in the pir extension runtime",
        "{v}"
    );
    assert_eq!(v["disposedPrompt"], "rejected-disposed", "{v}");

    // The child actually persisted the transcript it streamed. The second turn
    // declares no new loadout: the first turn's `toolsAdded` declaration is
    // recorded now and replayed, so `declareToolChanges` sees an unchanged set
    // and injects nothing (upstream `agent-loop.ts:333-375`, the delta rule
    // `tool_loadout_changes_are_declared_once` locks down; D32 made the
    // declaration survive the store so a resume can restore the loadout).
    let files: Vec<_> = std::fs::read_dir(&session_dir)
        .expect("session dir")
        .flatten()
        .map(|entry| entry.path())
        .collect();
    assert_eq!(files.len(), 1, "one session file expected: {files:?}");
    let session = SessionManager::open(&files[0]).expect("open child session");
    let roles: Vec<String> = session
        .entries()
        .into_iter()
        .filter_map(|entry| match entry {
            pir::session_manager::FileEntry::Message(message) => {
                Some(message.message.role().to_string())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        roles,
        vec!["user", "system", "assistant", "user", "assistant"],
        "child transcript must carry both turns: {roles:?}"
    );
}

// ---------------------------------------------------------------------------
// Fixture helpers + the red-run diagnostic
// ---------------------------------------------------------------------------

/// Read one HTTP request in full (headers, then the `Content-Length` body)
/// before the caller answers it. A server that responds and closes while the
/// client is still writing aborts that write; the fixture's SECOND turn sends
/// a much larger request (the replayed transcript + the tool-loadout
/// declaration), which is exactly the shape that trips the race. Bounded: a
/// read timeout and a size cap keep a stalled or hostile client from wedging
/// the single mock thread, and a request without a usable `Content-Length`
/// falls through to answering after the headers (the old behavior).
fn read_full_request(stream: &mut std::net::TcpStream) {
    use std::io::Read as _;
    use std::time::Duration;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut received: Vec<u8> = Vec::new();
    let mut scratch = [0u8; 8192];
    let mut header_end: Option<usize> = None;
    loop {
        match stream.read(&mut scratch) {
            Ok(0) | Err(_) => return,
            Ok(read) => received.extend_from_slice(&scratch[..read]),
        }
        if header_end.is_none() {
            header_end = received
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| index + 4);
        }
        let Some(head) = header_end else {
            if received.len() > 1024 * 1024 {
                return;
            }
            continue;
        };
        let headers = String::from_utf8_lossy(&received[..head]).to_ascii_lowercase();
        let content_length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if received.len() >= head + content_length || received.len() > 64 * 1024 * 1024 {
            return;
        }
    }
}

/// Re-run the exact `pir print --mode json` child the SDK shim spawns for one
/// prompt turn and return its stderr plus the stdout tail. Used ONLY by the
/// failing path of [`create_agent_session_child_round_trip`]: the shim keeps
/// the child's stderr private unless the exit code is non-zero, and JSON mode
/// exits 0 for an errored turn (the failure is in-band), so this is the only
/// way the assertion message can carry the child's own evidence.
fn child_turn_diagnostic(session_dir: &Path, text: &str) -> String {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    let Some(session_file) = std::fs::read_dir(session_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .find(|path| path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
    else {
        return "no session file found for the diagnostic re-run".to_owned();
    };
    // The shim's `PIR_CHILD_SESSION_BASE_ARGS` + per-turn flags; the provider
    // and model match the probe's `createAgentSession` options.
    let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .args([
            "print",
            "--mode",
            "json",
            "--no-extensions",
            "--no-skills",
            "--no-prompt-templates",
        ])
        .arg("--session")
        .arg(&session_file)
        .args(["--provider", "testprov", "--model", "test-model", "--"])
        .arg(text)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Ok(mut child) = command.spawn() else {
        return "diagnostic child could not be spawned".to_owned();
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = stdout {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut pipe) = stderr {
            let _ = pipe.read_to_end(&mut buffer);
        }
        buffer
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    format!(
        "exit: {status:?}\nstderr tail:\n{}\nstdout tail:\n{}",
        tail_chars(&String::from_utf8_lossy(&stderr), 4000),
        tail_chars(&String::from_utf8_lossy(&stdout), 4000)
    )
}

/// The last `max` characters of `text` (the evidence stays bounded).
fn tail_chars(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    chars[chars.len().saturating_sub(max)..].iter().collect()
}

/// The text of a stored user message, when `entry` is one.
fn user_text(entry: &FileEntry) -> Option<&str> {
    match entry {
        FileEntry::Message(message) => match &message.message {
            StoredMessage::User(user) => user.content.iter().find_map(|block| match block {
                ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            }),
            _ => None,
        },
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Long-line streaming (R1-L3-016): the loader and the paged prefix scan must
// scan each byte of a long line once and keep the following lines intact.
// ---------------------------------------------------------------------------

#[test]
fn a_three_mib_single_line_session_loads_with_later_lines_intact() {
    let dir = TempDir::new("long-line");
    let mut session =
        SessionManager::create(dir.path().to_str().expect("utf8 temp dir"), dir.path())
            .expect("create session");
    let big = "x".repeat(3 * 1024 * 1024);
    session
        .append_message(user_message(&big))
        .expect("append the 3 MiB message");
    session
        .append_message(user_message("after the big line"))
        .expect("append a later message");
    let path = session.session_file().expect("session file").to_path_buf();
    drop(session);

    reset_session_line_scan_bytes();
    let reopened = SessionManager::open(&path).expect("reopen the long-line session");
    let scanned = session_line_scan_bytes();
    let entries = reopened.entries();
    let texts: Vec<&str> = entries.iter().filter_map(user_text).collect();
    assert_eq!(texts.len(), 2, "both messages parse");
    assert_eq!(texts[0].len(), big.len(), "the 3 MiB line is intact");
    assert_eq!(
        texts[1], "after the big line",
        "the line after the long one is not joined or lost"
    );
    // The old shape restarted its newline search at `pending[0]` after every
    // 1 MiB read and scanned ~2× the line; the cursor shape scans it once.
    assert!(
        scanned < (big.len() as u64 * 3) / 2,
        "loader scanned {scanned} bytes for a {} byte line (old rescan ≈ 2x)",
        big.len()
    );
}

#[test]
fn the_paged_prefix_scan_walks_a_three_mib_line_once() {
    let dir = TempDir::new("long-prefix");
    let mut session =
        SessionManager::create(dir.path().to_str().expect("utf8 temp dir"), dir.path())
            .expect("create session");
    let big = "y".repeat(3 * 1024 * 1024);
    session
        .append_message(user_message(&big))
        .expect("append the 3 MiB prefix message");
    for index in 0..4 {
        session
            .append_message(user_message(&format!("tail {index}")))
            .expect("append a tail message");
    }
    let path = session.session_file().expect("session file").to_path_buf();
    drop(session);

    let paged = SessionManager::open_paged(&path, 4 * 1024, 2).expect("paged open");
    assert!(paged.is_paged(), "the fixture must exceed the tail window");
    let mut longest = 0usize;
    reset_session_line_scan_bytes();
    let visited = paged
        .for_each_prefix_entry(|entry| {
            if let Some(text) = user_text(&entry) {
                longest = longest.max(text.len());
            }
        })
        .expect("prefix scan");
    let scanned = session_line_scan_bytes();
    assert!(visited >= 2, "header + the 3 MiB message, got {visited}");
    assert_eq!(longest, big.len(), "the 3 MiB prefix line parses intact");
    assert!(
        scanned < (big.len() as u64 * 3) / 2,
        "prefix scan scanned {scanned} bytes for a {} byte line (old rescan ≈ 2x)",
        big.len()
    );
}

// Scale check for R1-L3-016: with 1 MiB reads the old rescan shape performs
// ~L²/(2 MiB) comparisons (a 32 MiB line ≈ 528 MiB scanned), while the cursor
// shape scans it once. Ignored by default so the normal gate stays fast.
// ---------------------------------------------------------------------------
// Turn-end persisted entry ids (R1-L1-009): one branch-path build + one
// index build, NOT a whole-path rescan per tool result.
// ---------------------------------------------------------------------------

/// The pre-index linear shape the indexed lookup must agree with
/// (newest-first branch walk over the exact same predicate).
fn linear_turn_entry_id(
    branch: &[FileEntry],
    matches: impl Fn(&StoredMessage) -> bool,
) -> Option<String> {
    branch.iter().rev().find_map(|entry| match entry {
        FileEntry::Message(entry) if !entry.id.is_empty() && matches(&entry.message) => {
            Some(entry.id.clone())
        }
        _ => None,
    })
}

#[test]
fn turn_end_entry_ids_are_indexed_not_rescanned_per_tool_result() {
    let mut session = SessionManager::in_memory(".", None).expect("in-memory session");
    let mut clock = 1_700_000_000_000u64;

    // 60 settled turns, each assistant + 3 tool results: 240 branch entries.
    for turn in 0..60 {
        clock += 1;
        session
            .append_message(Message::Assistant(assistant_message_at(
                &format!("turn {turn}"),
                clock,
            )))
            .expect("append assistant");
        for result in 0..3 {
            clock += 1;
            session
                .append_message(Message::ToolResult(tool_result_message(
                    &format!("old-{turn}-{result}"),
                    "echo",
                    clock,
                )))
                .expect("append old tool result");
        }
    }

    // The turn under test: assistant + 120 tool results (361-entry branch).
    clock += 1;
    let message = assistant_message_at("turn under test", clock);
    let assistant_entry_id = session
        .append_message(Message::Assistant(message.clone()))
        .expect("append turn assistant");
    let mut tool_results = Vec::new();
    for index in 0..120 {
        clock += 1;
        tool_results.push(tool_result_message(&format!("call-{index}"), "echo", clock));
        let result = tool_results.last().expect("just pushed");
        session
            .append_message(Message::ToolResult(result.clone()))
            .expect("append tool result");
    }

    // Independently resolve the same ids with the old linear scan.
    let branch = session.branch_entries(None).expect("branch entries");
    let expected_message_id = linear_turn_entry_id(&branch, |stored| match stored {
        StoredMessage::Assistant(stored) => {
            stored.content == message.content
                && stored.timestamp == message.timestamp
                && stored.stop_reason == message.stop_reason
        }
        _ => false,
    });
    let expected_result_ids: Vec<String> = tool_results
        .iter()
        .filter_map(|result| {
            linear_turn_entry_id(&branch, |stored| match stored {
                StoredMessage::ToolResult(stored) => {
                    stored.tool_call_id == result.tool_call_id
                        && stored.tool_name == result.tool_name
                        && stored.timestamp == result.timestamp
                }
                _ => false,
            })
        })
        .collect();
    assert_eq!(expected_message_id, Some(assistant_entry_id));
    assert_eq!(expected_result_ids.len(), tool_results.len());

    let (message_entry_id, tool_result_entry_ids, entries_visited, path_builds) =
        session.find_persisted_turn_entry_ids_for_test(&message, &tool_results);

    // Exact old-implementation results.
    assert_eq!(message_entry_id, expected_message_id);
    assert_eq!(tool_result_entry_ids, expected_result_ids);

    // ONE path build; visited entries stay linear in the branch length rather
    // than `branch * tool_results` (the old shape: ~361 * 120 = 43,320).
    assert_eq!(path_builds, 1, "the branch path must be built once");
    let path_len = branch.len();
    assert!(
        entries_visited <= 2 * path_len + tool_results.len() + 2,
        "turn-end lookup visited {entries_visited} entries over a {path_len}-entry \
         branch with {} tool results (old linear shape ~= {}); the lookup is not indexed",
        tool_results.len(),
        path_len * tool_results.len(),
    );
}

#[test]
#[ignore = "32 MiB single-line scan; run explicitly for the R1-L3-016 regression"]
fn a_32_mib_single_line_is_scanned_once() {
    let dir = TempDir::new("long-line-32m");
    let mut session =
        SessionManager::create(dir.path().to_str().expect("utf8 temp dir"), dir.path())
            .expect("create session");
    let big = "z".repeat(32 * 1024 * 1024);
    session
        .append_message(user_message(&big))
        .expect("append the 32 MiB message");
    session
        .append_message(user_message("after the big line"))
        .expect("append a later message");
    let path = session.session_file().expect("session file").to_path_buf();
    drop(session);

    reset_session_line_scan_bytes();
    let reopened = SessionManager::open(&path).expect("reopen the 32 MiB session");
    let scanned = session_line_scan_bytes();
    let entries = reopened.entries();
    let texts: Vec<&str> = entries.iter().filter_map(user_text).collect();
    assert_eq!(texts.len(), 2, "both messages parse");
    assert_eq!(texts[0].len(), big.len(), "the 32 MiB line is intact");
    assert!(
        scanned < 2 * big.len() as u64,
        "loader scanned {scanned} bytes for a {} byte line (old rescan ≈ 16x)",
        big.len()
    );
}

// ---------------------------------------------------------------------------
// Session search (R1-L3-012): cached corpus normalization + ASCII fuzzy path
// ---------------------------------------------------------------------------

/// Build a `SessionSummary` fixture without touching the filesystem.
fn search_summary(id: &str, name: Option<&str>, cwd: &str, corpus: &str) -> SessionSummary {
    SessionSummary {
        path: PathBuf::from(format!("C:\\sessions\\{id}.jsonl")),
        id: id.to_owned(),
        cwd: cwd.to_owned(),
        modified: std::time::UNIX_EPOCH,
        name: name.map(str::to_owned),
        first_message: String::new(),
        all_messages_text: corpus.to_owned(),
        message_count: 0,
        parent_session_path: None,
    }
}

/// The pre-fix reference fuzzy matcher (char vectors over `to_lowercase`):
/// the ASCII byte fast path must produce identical match decisions AND scores.
fn reference_fuzzy_match(query: &str, text: &str) -> (bool, f64) {
    fn is_word_boundary(previous: char) -> bool {
        previous.is_whitespace() || matches!(previous, '-' | '_' | '.' | '/' | ':')
    }
    fn score_positions(text: &[char], positions: &[usize]) -> f64 {
        let mut score = 0.0;
        let mut last_match: Option<usize> = None;
        let mut consecutive = 0i64;
        for &index in positions {
            match last_match {
                Some(previous) if previous + 1 == index => {
                    consecutive += 1;
                    score -= (consecutive * 5) as f64;
                }
                _ => {
                    consecutive = 0;
                    if let Some(previous) = last_match {
                        score += ((index - previous - 1) * 2) as f64;
                    }
                }
            }
            if index == 0 || is_word_boundary(text[index - 1]) {
                score -= 10.0;
            }
            score += index as f64 * 0.1;
            last_match = Some(index);
        }
        score
    }
    fn greedy_positions(query: &[char], text: &[char]) -> Option<Vec<usize>> {
        let mut positions = Vec::with_capacity(query.len());
        let mut cursor = 0usize;
        for &wanted in query {
            let found = text[cursor..]
                .iter()
                .position(|&candidate| candidate == wanted)?
                + cursor;
            positions.push(found);
            cursor = found + 1;
        }
        Some(positions)
    }
    fn match_query(query: &[char], text: &[char]) -> (bool, f64) {
        if query.is_empty() {
            return (true, 0.0);
        }
        if query.len() > text.len() {
            return (false, 0.0);
        }
        let Some(mut positions) = greedy_positions(query, text) else {
            return (false, 0.0);
        };
        let mut score = score_positions(text, &positions);
        let mut start = 0usize;
        while start + query.len() <= text.len() {
            if text[start..start + query.len()] == *query {
                for (offset, position) in positions.iter_mut().enumerate() {
                    *position = start + offset;
                }
                score = score.min(score_positions(text, &positions));
            }
            start += 1;
        }
        if query == text {
            score -= 100.0;
        }
        (true, score)
    }
    fn swap_alpha_numeric(query: &[char]) -> Vec<char> {
        if query.is_empty()
            || !query
                .iter()
                .all(|character| character.is_ascii_alphanumeric())
        {
            return Vec::new();
        }
        let Some(split) = (1..query.len()).find(|split| {
            let (head, tail) = query.split_at(*split);
            let head_letters = head[0].is_ascii_lowercase();
            let head_uniform = head.iter().all(|c| c.is_ascii_lowercase() == head_letters);
            let tail_letters = tail[0].is_ascii_lowercase();
            let tail_uniform = tail.iter().all(|c| c.is_ascii_lowercase() == tail_letters);
            head_uniform && tail_uniform && head_letters != tail_letters
        }) else {
            return Vec::new();
        };
        let (head, tail) = query.split_at(split);
        tail.iter().chain(head.iter()).copied().collect()
    }

    let query_lower: Vec<char> = query.to_lowercase().chars().collect();
    let text_lower: Vec<char> = text.to_lowercase().chars().collect();
    let primary = match_query(&query_lower, &text_lower);
    if primary.0 {
        return primary;
    }
    let swapped = swap_alpha_numeric(&query_lower);
    if swapped.is_empty() {
        return primary;
    }
    let swapped_match = match_query(&swapped, &text_lower);
    if !swapped_match.0 {
        return primary;
    }
    (true, swapped_match.1 + 5.0)
}

/// The ASCII byte path must be bit-for-bit equivalent to the old char path;
/// non-ASCII input keeps the Unicode path and stays correct.
#[test]
fn fuzzy_match_ascii_fast_path_matches_reference_scores() {
    let cases = [
        ("", "anything"),
        ("abc", "abc"),
        ("ABC", "abc"),
        ("abc", "xxabcxx"),
        ("fb", "foobar"),
        ("fz", "foobar"),
        ("f", "foo_bar"),
        ("_", "foo_bar"),
        ("ab", "abc"),
        ("ac", "abc"),
        ("foo", "foo"),
        ("foo", "foobar"),
        ("FOO", "foobar"),
        ("ab12", "foobar 12ab"),
        ("12ab", "foobar 12ab"),
        ("rpln", "Release Planning"),
        ("WRSP", "C:\\Work\\Rust_Projects"),
        ("abcdef", "abc"),
        ("z", "abc"),
        ("a-b", "a-b-c"),
        ("über", "Ärger Über"),
        ("ÜB", "ärger über"),
        ("ärger", "ÄRGER"),
        ("é", "café"),
        ("cafe", "café"),
        ("日本語", "これは日本語のテキスト"),
        ("本語", "日本語"),
    ];
    for (query, text) in cases {
        let expected = reference_fuzzy_match(query, text);
        let actual = fuzzy_match(query, text);
        assert_eq!(
            actual.matches, expected.0,
            "match parity for {query:?} against {text:?}"
        );
        assert_eq!(
            actual.score, expected.1,
            "score parity for {query:?} against {text:?}"
        );
    }

    // Empty query is a neutral match.
    assert_eq!(
        fuzzy_match("", "anything"),
        pir::session_manager::search::FuzzyMatch {
            matches: true,
            score: 0.0
        }
    );
    // Non-ASCII whitespace/case normalization used by phrase tokens.
    assert_eq!(normalize_whitespace_lower("  ÄRGER\tÜber  "), "ärger über");
}

/// Original selector semantics are unchanged: case-insensitive tokens, AND
/// across tokens, quoted phrases with whitespace normalization and no
/// cross-field hits, regex mode, tiered scoring, and hits on non-first
/// messages.
#[test]
fn session_search_preserves_selector_semantics_with_cached_corpus() {
    let summary = search_summary(
        "sess-abc-123",
        Some("Release Planning"),
        "C:\\Work\\Rust_Projects",
        "Alpha   first\tquestion\nreply about BETA topic  second   user Gamma",
    );
    let fields = summary.search_fields();
    let matched = |query: &str| match_session_search(&fields, &SessionSearchQuery::parse(query));
    let yes = |query: &str| {
        assert!(
            matched(query).is_some(),
            "query {query:?} must match the session"
        );
    };

    // Empty queries match everything with a neutral score.
    assert_eq!(matched(""), Some(0.0));
    assert_eq!(matched("   "), Some(0.0));

    // Case-insensitivity and a hit on a NON-first message.
    yes("alpha");
    yes("ALPHA");
    yes("beta");
    yes("GAMMA");

    // Multi-word queries are AND over tokens; tokens may hit different
    // messages/fields.
    yes("alpha gamma");
    yes("first beta gamma");
    assert!(matched("alpha delta").is_none());

    // Quoted phrase: whitespace is normalized, and a phrase cannot span a
    // field boundary.
    yes("\"beta   topic\"");
    assert!(matched("\"beta gamma\"").is_none());
    assert!(matched("\"planning alpha\"").is_none());

    // Regex mode is case-insensitive; an invalid pattern matches nothing.
    yes("re:^alpha");
    yes("re:planning$");
    assert!(matched("re:[").is_none());

    // Scattered fuzzy matches work in the name and the cwd (and only there).
    yes("rpln");
    yes("wrsp");
    assert!(matched("nonexistent").is_none());

    // Tiered scoring: name exact > message-text exact > fuzzy name/cwd.
    let name_score = matched("release").expect("name hit");
    let text_score = matched("beta").expect("text hit");
    let fuzzy_score = matched("rpln").expect("fuzzy hit");
    assert!(
        name_score < text_score,
        "name hit {name_score} must outrank text hit {text_score}"
    );
    assert!(
        text_score < fuzzy_score,
        "text hit {text_score} must outrank fuzzy hit {fuzzy_score}"
    );

    // Repeating a query returns the exact same score through the corpus cache.
    assert_eq!(matched("beta"), Some(text_score));
}

/// Counter seam (R1-L3-012): a summary matched with N tokens normalizes its
/// corpus ONE time, and later queries against the same summary reuse it. The
/// old per-token shape normalized the full corpus N times per query (and once
/// per token on every later keystroke).
#[test]
fn session_search_normalizes_each_corpus_once_per_summary() {
    let corpus = "lorem ipsum dolor sit amet consectetur adipiscing elit ".repeat(2048);
    let summary = search_summary("sess-perf", Some("Perf Corpus"), "C:\\perf", &corpus);
    let fields = summary.search_fields();

    reset_session_search_corpus_builds();
    assert_eq!(session_search_corpus_builds(), 0);

    // Empty and regex queries never need the token corpus.
    assert_eq!(
        match_session_search(&fields, &SessionSearchQuery::parse("")),
        Some(0.0)
    );
    assert!(match_session_search(&fields, &SessionSearchQuery::parse("re:lorem")).is_some());
    assert_eq!(session_search_corpus_builds(), 0);

    // Exact name and id hits resolve before the message corpus and must not
    // hash or lowercase it. The previous eager `PreparedFields` built the
    // corpus for every non-empty token query and fails these assertions.
    assert!(match_session_search(&fields, &SessionSearchQuery::parse("perf")).is_some());
    assert!(match_session_search(&fields, &SessionSearchQuery::parse("\"perf corpus\"")).is_some());
    assert!(match_session_search(&fields, &SessionSearchQuery::parse("sess-perf")).is_some());
    assert_eq!(
        session_search_corpus_builds(),
        0,
        "exact name/id hits must not build the corpus"
    );
    assert_eq!(
        session_search_corpus_bytes(),
        0,
        "exact name/id hits must not normalize corpus bytes"
    );
    assert_eq!(
        session_search_corpus_fingerprints(),
        0,
        "exact name/id hits must not fingerprint the corpus"
    );
    assert_eq!(
        session_search_corpus_fingerprint_bytes(),
        0,
        "exact name/id hits must not scan corpus bytes for a key"
    );

    // Five tokens, one normalization of the whole corpus (not five).
    let multi = SessionSearchQuery::parse("lorem ipsum dolor sit amet");
    assert_eq!(multi.tokens.len(), 5);
    assert!(match_session_search(&fields, &multi).is_some());
    assert_eq!(
        session_search_corpus_builds(),
        1,
        "5 tokens must normalize the corpus once"
    );
    assert_eq!(
        session_search_corpus_fingerprints(),
        1,
        "one corpus-scanner token set hashes the literal corpus once"
    );
    assert_eq!(
        session_search_corpus_bytes(),
        corpus.len() as u64,
        "bytes normalized must be independent of the token count \
         (old shape: 5 x {} bytes)",
        corpus.len()
    );

    // A later, different query against the same summary reuses the cache.
    let second = SessionSearchQuery::parse("ipsum consectetur");
    assert!(match_session_search(&fields, &second).is_some());
    assert_eq!(
        session_search_corpus_builds(),
        1,
        "a later query must not re-normalize the same summary"
    );
    assert_eq!(session_search_corpus_bytes(), corpus.len() as u64);

    // Phrase matching borrows the same cached corpus (its normalized view is
    // derived lazily from the cached lowercase text).
    let phrase = SessionSearchQuery::parse("\"lorem   ipsum\"");
    assert!(match_session_search(&fields, &phrase).is_some());
    assert_eq!(session_search_corpus_builds(), 1);

    // The picker clones summaries into display rows; the content-keyed cache
    // still hits for the clone.
    let cloned = summary.clone();
    assert!(match_session_search(&cloned.search_fields(), &second).is_some());
    assert_eq!(
        session_search_corpus_builds(),
        1,
        "a cloned summary with the same corpus must reuse the cache"
    );

    // A genuinely different corpus is normalized once of its own.
    let other = search_summary("sess-other", None, "C:\\perf", "unique tokencorpus");
    assert!(match_session_search(
        &other.search_fields(),
        &SessionSearchQuery::parse("tokencorpus")
    )
    .is_some());
    assert_eq!(session_search_corpus_builds(), 2);
}

/// A corpus that cannot fit in an empty cache is still normalized for the
/// match but must never enter the cache, so the next identical query rebuilds
/// it instead of bypassing the byte budget with one huge entry. The match
/// result is identical whether or not the cache keeps the entry.
#[test]
fn session_search_oversized_corpus_is_not_cached_and_result_is_stable() {
    let max_bytes = session_search_corpus_cache_stats().max_bytes;
    assert_eq!(max_bytes, 128 * 1024 * 1024);
    // A single entry is charged its actual lowercase length (1x), so a corpus
    // larger than the whole budget is oversized. Keep a needle at the end to
    // prove matching is unchanged.
    let needle = " oversizedneedle";
    let mut corpus = "a".repeat(max_bytes + 1);
    let needle_start = corpus.len() - needle.len();
    corpus.replace_range(needle_start.., needle);
    let summary = search_summary("sess-huge", None, "C:\\perf", &corpus);
    drop(corpus);
    let fields = summary.search_fields();

    reset_session_search_corpus_builds();
    let first = match_session_search(&fields, &SessionSearchQuery::parse("oversizedneedle"));
    assert!(
        first.is_some(),
        "the oversized corpus must still be matched"
    );
    let second = match_session_search(&fields, &SessionSearchQuery::parse("oversizedneedle"));
    assert_eq!(first, second, "matching must not depend on cache residency");
    assert_eq!(
        session_search_corpus_builds(),
        2,
        "an oversized corpus must be rebuilt instead of being cached"
    );
    assert_eq!(
        session_search_corpus_bytes(),
        2 * (max_bytes as u64 + 1),
        "both matches must normalize the full oversized corpus"
    );
    let stats = session_search_corpus_cache_stats();
    assert_eq!(
        stats.entries, 0,
        "oversized corpus must not enter the cache"
    );
    assert_eq!(stats.bytes, 0);
    assert_eq!(stats.rejected, 2, "both insert attempts must be rejected");
}

/// Byte-budget eviction: with a 1x lowercase charge, two corpora each larger
/// than half the budget cannot stay resident together, so the older entry must
/// be evicted FIFO and rebuilt on its next query while the newer one hits.
#[test]
fn session_search_corpus_cache_enforces_byte_budget() {
    let max_bytes = session_search_corpus_cache_stats().max_bytes;
    let corpus_bytes = max_bytes / 2 + 1;
    let first_source = "a".repeat(corpus_bytes);
    let second_source = "b".repeat(corpus_bytes);
    let first = search_summary("sess-budget-a", None, "C:\\budget", &first_source);
    let second = search_summary("sess-budget-b", None, "C:\\budget", &second_source);
    drop(first_source);
    drop(second_source);

    reset_session_search_corpus_builds();
    assert!(
        match_session_search(&first.search_fields(), &SessionSearchQuery::parse("aaaa")).is_some()
    );
    assert_eq!(session_search_corpus_builds(), 1);
    let first_stats = session_search_corpus_cache_stats();
    assert_eq!(first_stats.entries, 1);
    assert_eq!(
        first_stats.bytes, corpus_bytes,
        "lowercase text is charged 1x, not reserved for a normalized view"
    );
    assert!(first_stats.bytes <= first_stats.max_bytes);

    assert!(
        match_session_search(&second.search_fields(), &SessionSearchQuery::parse("bbbb")).is_some()
    );
    assert_eq!(session_search_corpus_builds(), 2);
    let second_stats = session_search_corpus_cache_stats();
    assert_eq!(
        second_stats.entries, 1,
        "the first corpus must have been evicted"
    );
    assert_eq!(
        second_stats.bytes, corpus_bytes,
        "eviction must subtract the evicted entry's charged bytes"
    );
    assert_eq!(second_stats.evictions, 1);
    assert!(second_stats.bytes <= second_stats.max_bytes);

    // The newest entry still fits the budget; the first entry was evicted.
    assert!(
        match_session_search(&second.search_fields(), &SessionSearchQuery::parse("bbbb")).is_some()
    );
    assert_eq!(session_search_corpus_builds(), 2);
    assert_eq!(session_search_corpus_cache_stats(), second_stats);
    assert!(
        match_session_search(&first.search_fields(), &SessionSearchQuery::parse("aaaa")).is_some()
    );
    assert_eq!(
        session_search_corpus_builds(),
        3,
        "the evicted corpus must be normalized again"
    );
    let final_stats = session_search_corpus_cache_stats();
    assert_eq!(final_stats.entries, 1);
    assert_eq!(final_stats.evictions, 2);
    assert_eq!(final_stats.bytes, corpus_bytes);
    assert!(final_stats.bytes <= final_stats.max_bytes);
}

/// The cache charges the lowercase text (1x) on insert and adds the
/// whitespace-normalized view only when a phrase query actually builds it;
/// the lazy build must update the byte accounting without re-charging an
/// already built view.
#[test]
fn session_search_cache_bytes_include_normalized() {
    let corpus = "Alpha   BETA\tgamma\nDelta  ".repeat(128);
    let summary = search_summary("sess-bytes", None, "C:\\bytes", &corpus);
    let fields = summary.search_fields();
    let lower_len = corpus.to_lowercase().len();
    let normalized_len = normalize_whitespace_lower(&corpus).len();
    assert!(
        normalized_len < lower_len,
        "fixture must exercise whitespace collapsing"
    );

    reset_session_search_corpus_builds();
    assert!(match_session_search(&fields, &SessionSearchQuery::parse("absenttoken")).is_none());
    let stats = session_search_corpus_cache_stats();
    assert_eq!(stats.entries, 1);
    assert_eq!(
        stats.bytes, lower_len,
        "a literal token only pays for the lowercase view"
    );
    assert!(stats.bytes <= stats.max_bytes);

    // A phrase query fills the lazy normalized view; the charge grows by its
    // actual size and the entry stays resident.
    assert!(match_session_search(&fields, &SessionSearchQuery::parse("\"alpha beta\"")).is_some());
    let after = session_search_corpus_cache_stats();
    assert_eq!(after.entries, stats.entries);
    assert_eq!(after.evictions, stats.evictions);
    assert_eq!(
        after.bytes,
        lower_len + normalized_len,
        "the lazily built normalized view must be charged to the budget"
    );
    assert!(after.bytes <= after.max_bytes);

    // Repeating the phrase (and literal) query must not charge the view twice.
    assert!(match_session_search(&fields, &SessionSearchQuery::parse("\"alpha beta\"")).is_some());
    assert!(match_session_search(&fields, &SessionSearchQuery::parse("absenttoken")).is_none());
    assert_eq!(session_search_corpus_cache_stats(), after);
}

/// Consecutive queries over more distinct corpora than the entry cap must
/// evict exactly one FIFO entry per insertion. `VecDeque` + `HashMap` keeps
/// that O(1); a linear "find the oldest entry" scan would not add evictions,
/// but the stats still pin the exact insertion-order bookkeeping and prove the
/// cache never over-evicts or leaks bytes.
#[test]
fn session_search_corpus_cache_evicts_fifo_entries_in_insertion_order() {
    let max_entries = session_search_corpus_cache_stats().max_entries;
    assert_eq!(max_entries, 1024);
    let summaries: Vec<SessionSummary> = (0..max_entries + 1)
        .map(|index| {
            search_summary(
                &format!("sess-fifo-{index}"),
                None,
                "C:\\fifo",
                &format!("payload-{index}"),
            )
        })
        .collect();
    reset_session_search_corpus_builds();
    let query = SessionSearchQuery::parse("absent-token");
    for summary in &summaries {
        assert!(match_session_search(&summary.search_fields(), &query).is_none());
    }
    let stats = session_search_corpus_cache_stats();
    assert_eq!(stats.entries, max_entries);
    assert_eq!(stats.evictions, 1);
    assert_eq!(session_search_corpus_builds(), max_entries as u64 + 1);
    assert!(stats.bytes <= stats.max_bytes);

    // The oldest entry was evicted; touching it again evicts exactly one more.
    assert!(match_session_search(&summaries[0].search_fields(), &query).is_none());
    let after = session_search_corpus_cache_stats();
    assert_eq!(after.entries, max_entries);
    assert_eq!(after.evictions, 2);
    assert_eq!(session_search_corpus_builds(), max_entries as u64 + 2);
}

/// P2-01: exact name/id/cwd literal hits and regex name/id/cwd hits resolve
/// before the lazy transcript is resolved, hashed or lowercased.
#[test]
fn session_search_exact_name_id_cwd_and_regex_hits_skip_lazy_corpus() {
    let dir = TempDir::new("search-fast-path");
    let mut session = SessionManager::create(r"C:\workspace\selector", dir.path())
        .expect("create fast-path session");
    session
        .append_message(user_message("message body has bodynedle"))
        .expect("append fast-path message");
    session
        .append_session_info("Fast Path Session")
        .expect("append session name");
    let id = session.id().to_owned();

    let sessions = SessionManager::list_sessions(dir.path(), None);
    assert_eq!(sessions.len(), 1);
    let summary = &sessions[0];
    assert_eq!(summary.name.as_deref(), Some("Fast Path Session"));
    let fields = summary.search_fields();

    reset_session_search_corpus_builds();
    let queries = [
        "fast",
        "\"fast path\"",
        id.as_str(),
        "selector",
        "re:^Fast Path Session$",
        "re:^C:\\\\workspace\\\\selector$",
    ];
    for query in queries {
        assert!(
            match_session_search(&fields, &SessionSearchQuery::parse(query)).is_some(),
            "query {query:?} must hit a small field"
        );
    }
    assert_eq!(
        session_search_corpus_builds(),
        0,
        "small-field hits must not normalize the corpus"
    );
    assert_eq!(session_search_corpus_bytes(), 0);
    assert_eq!(
        session_search_lazy_corpus_loads(),
        0,
        "small-field hits must not read the transcript"
    );
    assert_eq!(
        session_search_corpus_fingerprints(),
        0,
        "small-field hits must not fingerprint the corpus"
    );
    assert_eq!(session_search_corpus_fingerprint_bytes(), 0);

    // The transcript is still searched on the first corpus-scanning query.
    assert!(match_session_search(&fields, &SessionSearchQuery::parse("bodynedle")).is_some());
    assert_eq!(session_search_lazy_corpus_loads(), 1);
    assert_eq!(session_search_corpus_builds(), 1);
    assert_eq!(
        session_search_corpus_fingerprints(),
        1,
        "a lazy corpus hit fingerprints only the small descriptor"
    );
    assert!(
        session_search_corpus_fingerprint_bytes() <= summary.all_messages_text.len() as u64,
        "the transcript itself must never be hashed for a descriptor-keyed entry"
    );
}

/// P2-02 / R1-L3-013: a 260-session listing exceeds the old 256-entry lazy
/// cache cap, which made every keystroke re-read all files (FIFO sweeping just
/// ahead of the stable scan order). With the entry cap aligned to the
/// normalized cache, a second identical keystroke performs zero re-reads.
#[test]
fn lazy_corpus_cache_handles_repeated_keystrokes_over_260_sessions() {
    let dir = TempDir::new("lazy-many");
    for index in 0..260 {
        let mut session =
            SessionManager::create(r"C:\many", dir.path()).expect("create many-session fixture");
        session
            .append_message(user_message(&format!(
                "session {index} has searchablepayload"
            )))
            .expect("append many-session message");
    }
    let sessions = SessionManager::list_sessions(dir.path(), None);
    assert_eq!(sessions.len(), 260);

    reset_session_search_corpus_builds();
    let query = SessionSearchQuery::parse("searchablepayload");
    for summary in &sessions {
        assert!(match_session_search(&summary.search_fields(), &query).is_some());
    }
    assert_eq!(session_search_lazy_corpus_loads(), 260);
    let first_stats = session_search_lazy_corpus_cache_stats();
    assert_eq!(first_stats.entries, 260);
    assert_eq!(first_stats.evictions, 0);
    assert!(first_stats.bytes <= first_stats.max_bytes);
    assert_eq!(session_search_corpus_builds(), 260);

    // A second keystroke over the same listing must reuse every entry.
    for summary in &sessions {
        assert!(match_session_search(&summary.search_fields(), &query).is_some());
    }
    assert_eq!(
        session_search_lazy_corpus_loads(),
        260,
        "260 cached sessions must not thrash the lazy corpus cache"
    );
    assert_eq!(session_search_lazy_corpus_cache_stats(), first_stats);
    assert_eq!(session_search_corpus_builds(), 260);
}

/// P2-SEARCH3-01: a literal corpus is fingerprinted once per backing
/// allocation. Later keystrokes hit the allocation memo in O(1); a clone is a
/// new allocation and is deduplicated by content, not by pointer guessing.
#[test]
fn session_search_literal_fingerprint_is_memoized_per_allocation() {
    let corpus = "lorem ipsum dolor sit amet consectetur adipiscing elit ".repeat(2048);
    let summary = search_summary("sess-fp-memo", None, "C:\\fp", &corpus);
    let fields = summary.search_fields();

    reset_session_search_corpus_builds();
    assert_eq!(session_search_corpus_fingerprints(), 0);

    let first = SessionSearchQuery::parse("lorem ipsum");
    assert!(match_session_search(&fields, &first).is_some());
    assert_eq!(session_search_corpus_fingerprints(), 1);
    assert_eq!(
        session_search_corpus_fingerprint_bytes(),
        corpus.len() as u64,
        "the first corpus-scanning query hashes the literal corpus once"
    );
    assert_eq!(session_search_corpus_builds(), 1);

    // Same allocation, different keystroke: no second full-corpus hash.
    let second = SessionSearchQuery::parse("consectetur adipiscing");
    assert!(match_session_search(&fields, &second).is_some());
    assert_eq!(
        session_search_corpus_fingerprints(),
        1,
        "consecutive keystrokes over one corpus allocation must not re-hash it"
    );
    assert_eq!(
        session_search_corpus_fingerprint_bytes(),
        corpus.len() as u64,
        "no additional whole-corpus bytes may be scanned for the cache key"
    );
    assert_eq!(session_search_corpus_builds(), 1);

    // A clone is a distinct allocation: one new fingerprint, but the
    // content-addressed normalized entry is still reused (builds stays 1).
    let cloned = summary.clone();
    assert!(match_session_search(&cloned.search_fields(), &second).is_some());
    assert_eq!(
        session_search_corpus_fingerprints(),
        2,
        "a cloned allocation fingerprints once, not once per keystroke"
    );
    assert_eq!(session_search_corpus_builds(), 1);
}

/// P2-SEARCH4-01 at B8 scale: 1,000 x ~100 KB distinct literal corpora fit the
/// normalized tier at a 1x lowercase charge with a 128 MiB budget, so a second
/// complete keystroke round performs zero fingerprints, zero normalizations,
/// zero file reads and no evictions. The old 64 MiB / 2x design only held
/// ~335 entries and rebuilt ~665 lower views per keystroke.
#[test]
fn session_search_literal_working_set_hashes_once_across_rounds() {
    const SESSIONS: usize = 1_000;
    const BODY_BYTES: usize = 100 * 1024;
    let filler = "x".repeat(BODY_BYTES);
    let summaries: Vec<SessionSummary> = (0..SESSIONS)
        .map(|index| {
            let corpus = format!("{filler} session-{index} literalneedle-{index}");
            search_summary(&format!("sess-lit-{index}"), None, r"C:\literal", &corpus)
        })
        .collect();
    let total_bytes: u64 = summaries
        .iter()
        .map(|summary| summary.all_messages_text.len() as u64)
        .sum();
    assert!(
        total_bytes > 64 * 1024 * 1024,
        "fixture must exceed the old 64 MiB tier, got {total_bytes} bytes"
    );
    let max_bytes = session_search_corpus_cache_stats().max_bytes;
    assert_eq!(max_bytes, 128 * 1024 * 1024);
    assert!(
        total_bytes <= max_bytes as u64,
        "1,000 x 100 KB must fit the 128 MiB normalized tier at a 1x charge"
    );
    assert!(SESSIONS < session_search_corpus_cache_stats().max_entries);

    reset_session_search_corpus_builds();
    let query = SessionSearchQuery::parse("literalneedle");
    for summary in &summaries {
        assert!(match_session_search(&summary.search_fields(), &query).is_some());
    }
    assert_eq!(session_search_corpus_fingerprints(), SESSIONS as u64);
    assert_eq!(
        session_search_corpus_fingerprint_bytes(),
        total_bytes,
        "each distinct literal corpus is hashed once"
    );
    assert_eq!(session_search_corpus_builds(), SESSIONS as u64);
    assert_eq!(
        session_search_lazy_corpus_loads(),
        0,
        "literal corpora never read files"
    );
    let first = session_search_corpus_cache_stats();
    assert_eq!(first.entries, SESSIONS);
    assert_eq!(first.evictions, 0);
    assert_eq!(
        first.bytes as u64, total_bytes,
        "the whole lowercase working set must fit at a 1x charge"
    );
    assert!(first.bytes <= first.max_bytes);

    // Second keystroke round: all allocations are already memoized and the
    // lowercase entries are still resident. Nothing may be rebuilt.
    for summary in &summaries {
        assert!(match_session_search(&summary.search_fields(), &query).is_some());
    }
    assert_eq!(
        session_search_corpus_fingerprints(),
        SESSIONS as u64,
        "a second round over 1,000 x 100 KB literal corpora must not re-hash any"
    );
    assert_eq!(session_search_corpus_fingerprint_bytes(), total_bytes);
    assert_eq!(
        session_search_corpus_builds(),
        SESSIONS as u64,
        "a second round must not rebuild any lowercase corpus"
    );
    assert_eq!(
        session_search_lazy_corpus_loads(),
        0,
        "a second round must not read any file"
    );
    assert_eq!(session_search_corpus_cache_stats(), first);
}

/// P2-SEARCH3-02 / R1-L3-013: 700 sessions x ~100 KB (~70 MB raw) exceed the
/// old 64 MiB lazy budget but fit the 128 MiB one. The first round reads each
/// transcript once; the second round must be completely read-free. Linear
/// extrapolation to B8's 1,000 x 100 KB: ~102 MB < 128 MiB, same per-session
/// cost; a single transcript above 128 MiB is still never cached.
#[test]
fn lazy_corpus_cache_keeps_100kb_working_set_across_two_query_rounds() {
    const SESSIONS: usize = 700;
    const BODY_BYTES: usize = 100 * 1024;
    let dir = TempDir::new("lazy-100kb");

    // Build one valid session file, then copy it 700 times. Copies have unique
    // paths (hence unique descriptors); the header id is irrelevant here.
    let mut template = SessionManager::create(r"C:\b8", dir.path()).expect("create b8 template");
    // Ten messages keep `first_message` small (the listing preview) while the
    // joined search corpus is still ~100 KB.
    for index in 0..10 {
        let mut text = format!("message {index} ");
        text.push_str(&"x".repeat(BODY_BYTES / 10));
        if index == 9 {
            text.push_str(" b8needle");
        }
        template
            .append_message(user_message(&text))
            .expect("append b8 template message");
    }
    let template_path = template.path().to_owned();
    drop(template);
    let template_bytes = std::fs::read(&template_path).expect("read b8 template");
    for index in 1..SESSIONS {
        std::fs::write(
            dir.path().join(format!("b8-{index}.jsonl")),
            &template_bytes,
        )
        .expect("write b8 session copy");
    }

    let sessions = SessionManager::list_sessions(dir.path(), None);
    assert_eq!(sessions.len(), SESSIONS);
    assert!(
        sessions
            .iter()
            .all(|summary| summary.all_messages_text.len() < 1024),
        "LIST1 listing must still carry small descriptors, not 100 KB corpora"
    );

    reset_session_search_corpus_builds();
    let query = SessionSearchQuery::parse("b8needle");
    for summary in &sessions {
        assert!(match_session_search(&summary.search_fields(), &query).is_some());
    }
    assert_eq!(session_search_lazy_corpus_loads(), SESSIONS as u64);
    let first = session_search_lazy_corpus_cache_stats();
    assert_eq!(first.max_bytes, 128 * 1024 * 1024);
    assert_eq!(first.entries, SESSIONS);
    assert_eq!(first.evictions, 0);
    assert!(
        first.bytes > 64 * 1024 * 1024,
        "fixture must exceed the old 64 MiB lazy budget, got {} bytes",
        first.bytes
    );
    assert!(first.bytes <= first.max_bytes);

    // A second complete keystroke round over the same listing must not touch
    // any session file again (and must not evict the working set).
    for summary in &sessions {
        assert!(match_session_search(&summary.search_fields(), &query).is_some());
    }
    assert_eq!(
        session_search_lazy_corpus_loads(),
        SESSIONS as u64,
        "the 100 KB working set must stay resident across keystroke rounds"
    );
    assert_eq!(session_search_lazy_corpus_cache_stats(), first);
}

/// A deleted transcript must degrade to the descriptor fallback without a
/// panic, without counting a load, and without re-reading the file per
/// keystroke (the fallback literal descriptor is cached too).
#[test]
fn lazy_corpus_deleted_transcript_falls_back_and_stays_stable() {
    let dir = TempDir::new("lazy-deleted");
    let mut session =
        SessionManager::create(r"C:\deleted", dir.path()).expect("create deleted session");
    session
        .append_message(user_message("deletedbodyneedle"))
        .expect("append deleted message");
    let sessions = SessionManager::list_sessions(dir.path(), None);
    assert_eq!(sessions.len(), 1);
    // Close the writer before deleting: Windows refuses to unlink a file with
    // an open append handle.
    drop(session);
    std::fs::remove_file(&sessions[0].path).expect("delete session file");
    let fields = sessions[0].search_fields();

    reset_session_search_corpus_builds();
    let query = SessionSearchQuery::parse("deletedbodyneedle");
    assert!(match_session_search(&fields, &query).is_none());
    assert_eq!(
        session_search_lazy_corpus_loads(),
        0,
        "a failed read must not be counted as a loaded corpus"
    );
    assert!(match_session_search(&fields, &query).is_none());
    assert_eq!(session_search_lazy_corpus_loads(), 0);
    assert_eq!(
        session_search_corpus_builds(),
        1,
        "descriptor fallback is cached"
    );
    assert_eq!(session_search_corpus_fingerprints(), 1);
}

/// A descriptor naming a path that cannot round-trip through UTF-8 (the
/// listing stores `to_string_lossy`) must fall back like any unreadable file:
/// no panic, no lazy load, unique transcript text still absent. The test is
/// skipped only when the host filesystem refuses to create such a name.
#[test]
fn lazy_corpus_non_utf8_descriptor_path_falls_back() {
    #[cfg(windows)]
    fn weird_name() -> std::ffi::OsString {
        use std::os::windows::ffi::OsStringExt;
        let mut name: Vec<u16> = "weird-".encode_utf16().collect();
        name.push(0xD800); // unpaired surrogate: not representable in UTF-8
        name.extend(".jsonl".encode_utf16());
        std::ffi::OsString::from_wide(&name)
    }
    #[cfg(unix)]
    fn weird_name() -> std::ffi::OsString {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(b"weird-\xff.jsonl".to_vec())
    }
    #[cfg(not(any(unix, windows)))]
    fn weird_name() -> std::ffi::OsString {
        std::ffi::OsString::from("weird-unknown.jsonl")
    }

    let dir = TempDir::new("lazy-non-utf8");
    let mut session =
        SessionManager::create(r"C:\nonutf8", dir.path()).expect("create non-utf8 session");
    session
        .append_message(user_message("nonutf8bodyneedle"))
        .expect("append non-utf8 message");
    let source_path = session.path().to_owned();
    drop(session);

    let weird = dir.path().join(weird_name());
    if std::fs::copy(&source_path, &weird).is_err() {
        // e.g. a filesystem that rejects unpaired surrogates; the deleted-file
        // test already covers the same best-effort fallback shape.
        return;
    }

    let sessions = SessionManager::list_sessions(dir.path(), None);
    let weird_summary = sessions
        .iter()
        .find(|summary| summary.path == weird)
        .expect("listing must include the non-UTF-8 path");
    assert!(
        weird_summary.all_messages_text.contains('\u{FFFD}'),
        "to_string_lossy must produce the replacement character in the descriptor"
    );
    let fields = weird_summary.search_fields();
    reset_session_search_corpus_builds();
    assert!(
        match_session_search(&fields, &SessionSearchQuery::parse("nonutf8bodyneedle")).is_none(),
        "the lossy path cannot reopen the transcript; fallback must not invent a hit"
    );
    assert_eq!(
        session_search_lazy_corpus_loads(),
        0,
        "an unreadable non-UTF-8 path must not be counted as a load"
    );
}

/// LIST1 descriptor semantics: an append changes the file length (and mtime),
/// so the re-listed descriptor is a new key. The next query reads once, and a
/// repeated keystroke reuses the new entry.
#[test]
fn lazy_corpus_descriptor_rekeys_after_append() {
    let dir = TempDir::new("lazy-rekey");
    let mut session =
        SessionManager::create(r"C:\rekey", dir.path()).expect("create rekey session");
    session
        .append_message(user_message("firstpartneedle"))
        .expect("append first rekey message");
    let id = session.id().to_owned();
    let first = SessionManager::list_sessions(dir.path(), None);
    assert_eq!(first.len(), 1);

    reset_session_search_corpus_builds();
    let first_query = SessionSearchQuery::parse("firstpartneedle");
    assert!(match_session_search(&first[0].search_fields(), &first_query).is_some());
    assert_eq!(session_search_lazy_corpus_loads(), 1);
    assert!(match_session_search(&first[0].search_fields(), &first_query).is_some());
    assert_eq!(session_search_lazy_corpus_loads(), 1);

    session
        .append_message(user_message("secondpartneedle"))
        .expect("append second rekey message");
    let second = SessionManager::list_sessions(dir.path(), None);
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].id, id);
    assert_ne!(
        second[0].all_messages_text, first[0].all_messages_text,
        "a re-listing after an append must produce a new descriptor"
    );

    let second_query = SessionSearchQuery::parse("secondpartneedle");
    assert!(match_session_search(&second[0].search_fields(), &second_query).is_some());
    assert_eq!(session_search_lazy_corpus_loads(), 2);
    assert!(match_session_search(&second[0].search_fields(), &second_query).is_some());
    assert_eq!(
        session_search_lazy_corpus_loads(),
        2,
        "the re-listed descriptor must be cached under its new key"
    );
}

// ---------------------------------------------------------------------------
// R1-L3-013: listing keeps lazy corpus descriptors, not full transcripts
// ---------------------------------------------------------------------------

/// Append `message_count` user messages of roughly `body_bytes` each. Only the
/// LAST message gains `needle` (so a preview-only or truncated corpus cannot
/// satisfy the search assertions). Returns the session id and the exact byte
/// length of the joined search corpus (messages joined with one space).
fn seed_large_session(
    dir: &Path,
    cwd: &str,
    message_count: usize,
    body_bytes: usize,
    needle: Option<&str>,
) -> (String, usize) {
    let mut session = SessionManager::create(cwd, dir).expect("create large session");
    let mut corpus_len = 0usize;
    for index in 0..message_count {
        let mut text = format!("message {index} ");
        text.push_str(&"x".repeat(body_bytes));
        if index + 1 == message_count {
            if let Some(needle) = needle {
                text.push(' ');
                text.push_str(needle);
            }
        }
        corpus_len += text.len();
        session
            .append_message(user_message(&text))
            .expect("append large message");
    }
    corpus_len += message_count.saturating_sub(1);
    (session.id().to_owned(), corpus_len)
}

#[test]
fn listing_large_sessions_stores_lazy_corpus_descriptors() {
    let dir = TempDir::new("lazy-listing");
    let (newest, newest_corpus_len) =
        seed_large_session(dir.path(), r"C:\lazy", 8, 4096, Some("tailneedle"));
    std::thread::sleep(std::time::Duration::from_millis(20));
    let (middle, _) = seed_large_session(dir.path(), r"C:\lazy", 8, 4096, None);
    std::thread::sleep(std::time::Duration::from_millis(20));
    let (oldest, _) = seed_large_session(dir.path(), r"C:\lazy", 8, 4096, None);

    reset_session_listing_corpus_bytes();
    let sessions = SessionManager::list_sessions(dir.path(), None);
    assert_eq!(sessions.len(), 3);
    assert_eq!(
        session_listing_corpus_bytes(),
        0,
        "listing must not materialize any message text into all_messages_text"
    );

    let descriptor_bytes: usize = sessions
        .iter()
        .map(|summary| summary.all_messages_text.len())
        .sum();
    assert!(
        descriptor_bytes < 4 * 1024,
        "a 3 x ~36 KiB listing must carry only lazy descriptors, got {descriptor_bytes} bytes"
    );
    assert!(
        sessions
            .iter()
            .all(|summary| !summary.all_messages_text.contains("xxxx")),
        "no transcript text may be retained in the listing"
    );

    // The display preview (first user message) is unchanged.
    let newest_summary = sessions
        .iter()
        .find(|summary| summary.id == newest)
        .expect("newest summary");
    assert!(
        newest_summary.first_message.starts_with("message 0 "),
        "first_message stays the full first user message"
    );
    assert_eq!(newest_summary.message_count, 8);

    let fields = newest_summary.search_fields();
    reset_session_search_corpus_builds();

    // An exact id hit never reads the transcript.
    assert!(match_session_search(&fields, &SessionSearchQuery::parse(&newest)).is_some());
    assert_eq!(
        session_search_lazy_corpus_loads(),
        0,
        "an id hit must not resolve the lazy corpus"
    );

    // A hit that only exists in the LAST message streams the file once.
    assert!(match_session_search(&fields, &SessionSearchQuery::parse("tailneedle")).is_some());
    assert_eq!(session_search_lazy_corpus_loads(), 1);
    assert_eq!(
        session_search_corpus_bytes(),
        newest_corpus_len as u64,
        "the lazily scanned corpus must cover every user/assistant message"
    );

    // Regex mode resolves the same lazy corpus (cached now: no second load).
    assert!(match_session_search(&fields, &SessionSearchQuery::parse("re:tailneedle$")).is_some());
    assert_eq!(session_search_lazy_corpus_loads(), 1);

    // A later keystroke reuses the streamed corpus: no re-read.
    assert!(match_session_search(&fields, &SessionSearchQuery::parse("message")).is_some());
    assert_eq!(
        session_search_lazy_corpus_loads(),
        1,
        "repeated keystrokes must not re-read the session file"
    );

    // Other sessions still list in mtime order and stay searchable.
    assert_eq!(
        sessions
            .iter()
            .map(|summary| summary.id.as_str())
            .collect::<Vec<_>>(),
        vec![oldest.as_str(), middle.as_str(), newest.as_str()]
    );
}

#[test]
fn filter_sessions_by_cwd_copies_only_lazy_descriptors() {
    let dir = TempDir::new("lazy-filter");
    seed_large_session(dir.path(), r"C:\lazy", 8, 4096, Some("tailneedle"));
    seed_large_session(dir.path(), r"C:\lazy", 8, 4096, None);
    seed_large_session(dir.path(), r"C:\other", 8, 4096, None);

    let all = SessionManager::list_sessions(dir.path(), None);
    assert_eq!(all.len(), 3);

    reset_session_filter_corpus_bytes();
    let filtered = SessionManager::filter_sessions_by_cwd(&all, r"C:\lazy");
    assert_eq!(filtered.len(), 2);
    let copied = session_filter_corpus_bytes();
    let descriptor_bytes: u64 = filtered
        .iter()
        .map(|summary| summary.all_messages_text.len() as u64)
        .sum();
    assert_eq!(
        copied, descriptor_bytes,
        "the seam counts exactly the all_messages_text bytes the filter copied"
    );
    assert!(
        copied < 4 * 1024,
        "filtering must copy only lazy descriptors, got {copied} bytes"
    );

    // Same result as a cwd-filtered listing (both carry lazy descriptors).
    assert_eq!(
        filtered,
        SessionManager::list_sessions(dir.path(), Some(r"C:\lazy"))
    );

    // The filtered summaries still search the full transcript.
    reset_session_search_corpus_builds();
    let matched = filtered.iter().any(|summary| {
        match_session_search(
            &summary.search_fields(),
            &SessionSearchQuery::parse("tailneedle"),
        )
        .is_some()
    });
    assert!(
        matched,
        "the seeded tail needle must be searchable after filtering"
    );
    assert!(session_search_lazy_corpus_loads() >= 1);
}
