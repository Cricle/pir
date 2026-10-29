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

use pir::ai::types::{ContentBlock, Message, StopReason, TextContent, UserMessage, UserRole};
use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use pir::session_manager::SessionManager;
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
    Message::Assistant(pir::ai::types::AssistantMessage {
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        stop_reason: StopReason::Stop,
        timestamp: pir::ai::util::now_ms(),
        ..Default::default()
    })
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
