//! Process-level coverage for the extended RPC command surface (upstream
//! `rpc-mode.ts` command list): every session-control command answers with a
//! well-formed response, and unknown/invalid commands fail without killing
//! the server.
//!
//! Contract coverage:
//! - events stream BARE at top level (no `{"type":"event",...}` wrapper);
//! - unknown commands report `Unknown command: <type>` (capital U);
//! - steer/follow_up carry the REAL session disposition; a prompt that
//!   arrives while the agent streams is REFUSED without an explicit
//!   `streamingBehavior` (upstream agent-session.ts:2014-2022) and QUEUED
//!   ("steer"/"followUp") with one;
//! - `get_tree` answers with the upstream `SessionTreeNode` forest (nested
//!   `children` nodes carrying their own `entry`), `get_entries` stays a
//!   flat entry list;
//! - `abort`/`get-state` answer with no data, `set_thinking_level` with bare
//!   success, `switch_session`/`clone`/`new_session` with `{cancelled}` only,
//!   `fork` with `{text, cancelled}` and optional `entryId`;
//! - `bash` accepts `excludeFromContext`;
//! - the pir-only background tool-call members
//!   (`background_calls`/`abort_background`/`abort_all_background`,
//!   `TOOL-BACKGROUND-SCOPE.md` §2.5) go through `attached()` like their
//!   neighbours and answer with their own payload shapes: an empty array, a
//!   `{inFlight}` boolean (false for an unknown/settled id, true for a call
//!   that was in flight) and a cancelled count.
//!
//! The process runs against the shared instant-401 sandbox endpoint
//! (`common::sandbox_endpoint()`), so failed prompts are fast and
//! deterministic (never a stalled unreachable host). Tests that prompt wait
//! for the run to settle (bare `agent_end` event) before forking or reading
//! entries.

use crate::cli_entry_contract::common;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader as TokioBufReader};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::{AgentTool, BoxFuture, ToolContext, ToolRegistry};
use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::coding_agent::rpc::{RpcServer, SessionFactory};
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::session_manager::SessionManager;
use serde_json::json;

// ---------------------------------------------------------------------------
// Process-level helpers
// ---------------------------------------------------------------------------

/// Minimal models.json pointed at the shared instant-401 sandbox endpoint:
/// the session factory builds a real AgentSession and any prompt fails fast
/// with a non-retryable auth error.
fn sandbox_models_json() -> String {
    let base_url = common::sandbox_endpoint();
    format!(
        r#"{{"providers":{{"bench":{{"name":"rpc test","baseUrl":"{base_url}","apiKey":"x","api":"openai-completions","models":[{{"id":"bench-model","name":"bench","api":"openai-completions","baseUrl":"{base_url}","reasoning":false,"contextWindow":4096,"maxTokens":256,"input":["text"]}}]}}}}}}"#
    )
}

fn spawn_rpc() -> (
    std::process::Child,
    BufReader<std::process::ChildStdout>,
    std::process::ChildStdin,
) {
    // One home per spawned process: tests in this binary run in parallel, and
    // a shared `models.json` path could be observed truncated mid-write by a
    // sibling spawn (seen as "EOF while parsing a value" from create).
    static SPAWN_SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = SPAWN_SEQ.fetch_add(1, Ordering::SeqCst);
    let home = std::env::temp_dir().join(format!("pir-rpc-cmds-{}-{seq}", std::process::id()));
    let _ = std::fs::create_dir_all(&home);
    let agent_dir = home.join("agent");
    let _ = std::fs::create_dir_all(&agent_dir);
    let _ = std::fs::write(agent_dir.join("models.json"), sandbox_models_json());
    let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .arg("rpc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("PIR_AGENT_DIR", &agent_dir);
    for (_provider, keys) in pir::ai::auth::env_api_keys::all() {
        for key in keys {
            command.env_remove(key);
        }
    }
    let mut child = command.spawn().expect("pir rpc should start");
    let stdout = child.stdout.take().expect("rpc stdout");
    let stdin = child.stdin.take().expect("rpc stdin");
    // Drain the child's stderr on a side thread: the pipe buffer is bounded,
    // and a chatty child (tracing warnings, [ext] diagnostics) that nobody
    // drains blocks on its next stderr write — which wedges the command loop
    // and makes the pending command never answer.
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            use std::io::Read as _;
            let mut stderr = stderr;
            let mut buffer = [0u8; 4096];
            loop {
                match stderr.read(&mut buffer) {
                    Ok(0) | Err(_) => return,
                    Ok(read) => {
                        std::io::stderr().write_all(&buffer[..read]).ok();
                    }
                }
            }
        });
    }
    (child, BufReader::new(stdout), stdin)
}

fn send(stdin: &mut std::process::ChildStdin, line: &str) {
    stdin
        .write_all(format!("{line}\n").as_bytes())
        .expect("write rpc line");
    stdin.flush().expect("flush rpc line");
}

/// Reap the rpc child after its stdin was closed.
///
/// A child that never attached a session exits at once. After a prompt, the
/// server's bounded writer drain (src/coding_agent/rpc.rs) may legitimately
/// sit in its full 5 s grace window even though every protocol response has
/// already been delivered and asserted. Paying that window in every
/// command-shape test is pure wall-clock: give a short grace, then kill and
/// reap. The clean-shutdown contract is covered by the dedicated
/// `rpc_extension_ui::extension_shutdown_*` tests, and reaping keeps
/// `pir.exe` unlocked on Windows either way.
fn wait_or_kill(child: &mut std::process::Child) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}

/// Kill a wedged RPC child from the watchdog. Windows terminates the whole
/// tree with `taskkill`; Unix sends SIGKILL to the direct child, which closes
/// the stdout pipe the blocked reader is waiting on.
fn kill_wedged_child(pid: u32) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F", "/T"])
            .output();
    }
    #[cfg(unix)]
    {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
}

fn read_line(reader: &mut BufReader<std::process::ChildStdout>) -> serde_json::Value {
    let mut line = String::new();
    reader.read_line(&mut line).expect("read rpc line");
    assert!(!line.is_empty(), "rpc stdout closed before a response");
    serde_json::from_str(line.trim()).expect("rpc line must be valid JSON")
}

/// Drain the event stream until the run settles: the terminal `agent_end`
/// event proves the spawned run (including the user-message append, which
/// happens at the start of the run) has finished, so follow-up commands that
/// read the session file (fork/clone/get_entries) cannot race it.
fn drain_until_agent_end(reader: &mut BufReader<std::process::ChildStdout>) {
    for _ in 0..500 {
        let value = read_line(reader);
        if value["type"] == "agent_end" {
            return;
        }
    }
    panic!("agent_end event never arrived");
}

/// Round-trip a command and assert the response succeeded.
fn expect_ok(
    reader: &mut BufReader<std::process::ChildStdout>,
    stdin: &mut std::process::ChildStdin,
    command: &str,
) -> serde_json::Value {
    send(stdin, &format!(r#"{{"type":"{command}","id":"c1"}}"#));
    let response = read_response(reader, command);
    assert_eq!(response["success"], true, "{response}");
    response
}

/// Read lines until the response for `command` arrives (skipping any bare
/// event lines and stray late frames a failed run may still emit). Every
/// skipped line must be a BARE event (never the `{"type":"event",...}`
/// wrapper).
fn read_response(
    reader: &mut BufReader<std::process::ChildStdout>,
    command: &str,
) -> serde_json::Value {
    for _ in 0..200 {
        let value = read_line(reader);
        assert_ne!(
            value["type"], "event",
            "events must not be wrapped: {value}"
        );
        if value["type"] == "response" && value["command"] == command {
            return value;
        }
    }
    panic!("no {command} response arrived");
}

// ---------------------------------------------------------------------------
// Session-control commands that need no model (process level)
// ---------------------------------------------------------------------------

#[test]
fn rpc_session_control_commands_answer_without_a_model() {
    let (mut child, mut reader, mut stdin) = spawn_rpc();

    // Every round-trip below reads with `read_response` (frame matching), NOT
    // a bare `read_line`: the session-event bridge streams bare event frames
    // (`queue_update`, …) from its own forwarder thread, so they legitimately
    // interleave with responses at ANY point — even with nothing visibly
    // pending (an idle-session queue drain can surface frames several
    // commands later). Assuming frame order here is the flake this test used
    // to have.
    send(&mut stdin, r#"{"type":"hello"}"#);
    let _ = read_response(&mut reader, "hello");

    // Attach a real (persisted) session; the model is never called.
    send(&mut stdin, r#"{"type":"create","id":"c0"}"#);
    let created = read_response(&mut reader, "create");
    assert_eq!(created["success"], true, "{created}");

    // get_state carries the upstream RpcSessionState shape.
    send(&mut stdin, r#"{"type":"get_state","id":"s1"}"#);
    let state = read_response(&mut reader, "get_state");
    assert_eq!(state["success"], true, "{state}");
    assert!(state["data"].get("model").is_some(), "{state}");
    assert!(state["data"].get("thinkingLevel").is_some(), "{state}");
    assert!(state["data"].get("steeringMode").is_some(), "{state}");
    assert!(
        state["data"].get("pendingMessageCount").is_some(),
        "{state}"
    );

    // The legacy kebab-case `get-state` alias answers with bare success, NO
    // state snapshot payload (upstream rpc-mode.ts parity).
    send(&mut stdin, r#"{"type":"get-state","id":"gs0"}"#);
    let response = read_response(&mut reader, "get-state");
    assert_eq!(response["success"], true, "{response}");
    assert!(response.get("data").is_none(), "{response}");

    // `abort` answers with bare success, no data payload.
    send(&mut stdin, r#"{"type":"abort","id":"a0"}"#);
    let response = read_response(&mut reader, "abort");
    assert_eq!(response["success"], true, "{response}");
    assert!(response.get("data").is_none(), "{response}");

    // `set_thinking_level` answers with bare success (no {level} payload).
    send(
        &mut stdin,
        r#"{"type":"set_thinking_level","id":"t0","level":"medium"}"#,
    );
    let response = read_response(&mut reader, "set_thinking_level");
    assert_eq!(response["success"], true, "{response}");
    assert!(response.get("data").is_none(), "{response}");

    // steer/follow_up carry the REAL session disposition (idle: "handled").
    send(&mut stdin, r#"{"type":"steer","id":"s0","text":"later"}"#);
    // The steer enqueues through the session, whose queue_update event
    // streams BARE on stdout before the response.
    let response = read_response(&mut reader, "steer");
    assert_eq!(response["data"]["disposition"], "handled", "{response}");
    send(
        &mut stdin,
        r#"{"type":"follow_up","id":"f0","text":"after"}"#,
    );
    let response = read_response(&mut reader, "follow_up");
    assert_eq!(response["data"]["disposition"], "handled", "{response}");

    // get_fork_messages lists the user messages (empty for a fresh session).
    send(&mut stdin, r#"{"type":"get_fork_messages","id":"fm0"}"#);
    let response = read_response(&mut reader, "get_fork_messages");
    assert!(response["data"]["messages"].is_array(), "{response}");

    // get_commands enumerates extension/prompt/skill commands (never a
    // hardcoded empty vec: the arrays are always present).
    send(&mut stdin, r#"{"type":"get_commands","id":"gc0"}"#);
    let response = read_response(&mut reader, "get_commands");
    assert!(response["data"]["commands"].is_array(), "{response}");

    // Queue-mode setters: both spellings of the wire mode.
    send(
        &mut stdin,
        r#"{"type":"set_steering_mode","id":"m1","mode":"one-at-a-time"}"#,
    );
    let response = read_response(&mut reader, "set_steering_mode");
    assert_eq!(response["success"], true, "{response}");
    send(
        &mut stdin,
        r#"{"type":"set_follow_up_mode","id":"m2","mode":"all"}"#,
    );
    let response = read_response(&mut reader, "set_follow_up_mode");
    assert_eq!(response["success"], true, "{response}");
    // Invalid mode names are command errors, not crashes.
    send(
        &mut stdin,
        r#"{"type":"set_steering_mode","id":"m3","mode":"bogus"}"#,
    );
    let response = read_response(&mut reader, "set_steering_mode");
    assert_eq!(response["success"], false, "{response}");

    // Thinking-level listing is static and complete (upstream level set).
    let response = expect_ok(&mut reader, &mut stdin, "get_available_thinking_levels");
    assert!(
        response["data"]["levels"].as_array().is_some(),
        "{response}"
    );

    // Empty-session commands: clear_queue answers with both queue lists
    // (the emptied queue_update frame streams bare before the response).
    send(&mut stdin, r#"{"type":"clear_queue","id":"cq0"}"#);
    let response = read_response(&mut reader, "clear_queue");
    assert!(response["data"].get("steering").is_some(), "{response}");
    assert!(response["data"].get("followUp").is_some(), "{response}");

    // Feature toggles and their readbacks.
    send(
        &mut stdin,
        r#"{"type":"set_auto_compaction","id":"a1","enabled":false}"#,
    );
    let response = read_response(&mut reader, "set_auto_compaction");
    assert_eq!(response["success"], true, "{response}");
    send(
        &mut stdin,
        r#"{"type":"set_auto_retry","id":"a2","enabled":true}"#,
    );
    let response = read_response(&mut reader, "set_auto_retry");
    assert_eq!(response["success"], true, "{response}");
    let response = expect_ok(&mut reader, &mut stdin, "abort_retry");
    assert!(response.is_object(), "{response}");

    // Unknown commands carry the upstream capital-U text.
    send(
        &mut stdin,
        r#"{"type":"definitely-not-a-command","id":"u0"}"#,
    );
    let response = read_response(&mut reader, "definitely-not-a-command");
    assert_eq!(response["success"], false, "{response}");
    assert_eq!(
        response["error"], "Unknown command: definitely-not-a-command",
        "{response}"
    );

    drop(stdin);
    wait_or_kill(&mut child);
}

#[test]
fn rpc_set_model_and_bash_report_errors_not_crashes() {
    let (mut child, mut reader, mut stdin) = spawn_rpc();

    send(&mut stdin, r#"{"type":"hello"}"#);
    let _ = read_line(&mut reader);

    // An unknown model is a named error (upstream "Model not found: p/m").
    send(&mut stdin, r#"{"type":"create","id":"c0"}"#);
    let created = read_line(&mut reader);
    assert_eq!(created["success"], true, "{created}");
    send(
        &mut stdin,
        r#"{"type":"set_model","id":"sm1","provider":"nope","modelId":"none"}"#,
    );
    let response = read_line(&mut reader);
    assert_eq!(response["success"], false, "{response}");
    assert!(
        response["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Model not found"),
        "{response}"
    );

    // Out-of-loop bash executes a real command (no model needed); the
    // optional `excludeFromContext` flag is accepted.
    send(
        &mut stdin,
        r#"{"type":"bash","id":"b1","command":"echo RPC_BASH_OK","excludeFromContext":true}"#,
    );
    // The bash execution streams `bash_execution_update` frames (carrying
    // the request id) before the response; read past them.
    let response = read_response(&mut reader, "bash");
    assert_eq!(response["success"], true, "{response}");
    assert!(
        response["data"]["output"]
            .as_str()
            .unwrap_or_default()
            .contains("RPC_BASH_OK"),
        "{response}"
    );

    drop(stdin);
    wait_or_kill(&mut child);
}

// ---------------------------------------------------------------------------
// `bash` while the client's stdin stays OPEN (regression: the shell's stdin
// must never be the inherited JSON-RPC pipe — Git Bash blocks at startup on an
// inherited held-open pipe, so the command only runs once the client closes
// stdin; upstream bash.ts:116 spawns non-stdin transports with `stdio[0] =
// "ignore"`).
// ---------------------------------------------------------------------------

#[test]
fn rpc_bash_completes_while_stdin_stays_open() {
    let (mut child, mut reader, mut stdin) = spawn_rpc();

    // A session must be attached before `bash` (the command routes through the
    // session's out-of-loop shell execution). These round-trips happen BEFORE
    // the reader moves to its own thread.
    send(&mut stdin, r#"{"type":"hello"}"#);
    let _ = read_response(&mut reader, "hello");
    send(&mut stdin, r#"{"type":"create","id":"c0"}"#);
    let created = read_response(&mut reader, "create");
    assert_eq!(created["success"], true, "{created}");

    send(
        &mut stdin,
        r#"{"type":"bash","id":"b1","command":"echo RPC_BASH_STDIN_OPEN_OK"}"#,
    );
    // CRITICAL: `stdin` is deliberately NOT dropped/closed for the whole wait
    // below — holding the request pipe open is the regression condition. (The
    // original bash coverage read the response on this same thread with stdin
    // still open and passed only because the earlier round-trips never touched
    // a process that inherited it.)
    //
    // The read runs on a side thread so THIS thread can bound the wait: a
    // wedged child must fail as a clear timeout instead of blocking the suite
    // forever. The reader is moved, never cloned.
    let (line_tx, line_rx) = std::sync::mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let response = read_response(&mut reader, "bash");
        let _ = line_tx.send(response);
    });

    // 30s budget: a healthy run is Git Bash process creation plus one `echo`
    // (~0.3-3s), so exceeding it means a hang, not slowness. On timeout the
    // child is killed (our own child, never a foreign process) so the reader
    // thread unblocks and nothing leaks.
    let response = match line_rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(response) => response,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader_thread.join();
            panic!("bash response did not arrive while stdin stayed open (hang reproduced)");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader_thread.join();
            panic!("rpc stdout closed before the bash response arrived");
        }
    };
    assert_eq!(response["success"], true, "{response}");
    assert!(
        response["data"]["output"]
            .as_str()
            .unwrap_or_default()
            .contains("RPC_BASH_STDIN_OPEN_OK"),
        "{response}"
    );

    drop(stdin);
    wait_or_kill(&mut child);
    let _ = reader_thread.join();
}

// ---------------------------------------------------------------------------
// Session-swap payload shapes ({cancelled} only; fork {text, cancelled})
// ---------------------------------------------------------------------------

#[test]
fn rpc_control_command_shapes_match_upstream() {
    let (mut child, mut reader, mut stdin) = spawn_rpc();
    // Watchdog: a wedged rpc child must not hang the suite forever; killing
    // it closes stdout and the next read fails with a clear assertion. The
    // `done` flag lets the test cancel the watchdog the moment the child
    // exited on its own — otherwise join() below would wait out the full
    // 60s sleep even for a fast, healthy run.
    let watchdog_child = child.id();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done_for_watchdog = std::sync::Arc::clone(&done);
    let watchdog = std::thread::spawn(move || {
        for _ in 0..600 {
            if done_for_watchdog.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // The child is moved into wait_or_kill below on success; a PID reuse
        // within 60s in the test pool is vanishingly unlikely, accept the
        // risk for a bounded suite.
        kill_wedged_child(watchdog_child);
        eprintln!("[watchdog] killed wedged rpc child {watchdog_child}");
    });

    send(&mut stdin, r#"{"type":"hello"}"#);
    let _ = read_line(&mut reader);

    send(&mut stdin, r#"{"type":"create","id":"c0"}"#);
    eprintln!("[progress] create sent");
    let created = read_line(&mut reader);
    assert_eq!(created["success"], true, "{created}");
    let session_path = created["data"]["path"]
        .as_str()
        .expect("session path")
        .to_owned();

    // Give the session content (the prompt fails fast against the 401
    // sandbox endpoint, but the user message persists), so fork/clone have a
    // real transcript to duplicate. Wait for the run to settle first: the
    // user-message append happens inside the spawned run, and a fork racing
    // it would read an empty (lazily created) session file.
    send(
        &mut stdin,
        r#"{"type":"prompt","id":"p0","text":"hello fork"}"#,
    );
    eprintln!("[progress] prompt sent");
    let response = read_line(&mut reader);
    eprintln!("[progress] prompt acked: {response}");
    assert_eq!(response["success"], true, "{response}");
    assert!(response["data"]["disposition"].is_string(), "{response}");
    eprintln!("[progress] draining until agent_end");
    drain_until_agent_end(&mut reader);
    eprintln!("[progress] settled");

    send(&mut stdin, r#"{"type":"fork","id":"fk0"}"#);
    eprintln!("[progress] fork sent");
    let response = read_response(&mut reader, "fork");
    eprintln!("[progress] fork answered");
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(response["data"]["cancelled"], false, "{response}");
    // Forking at the current position reports an empty selected text.
    assert_eq!(response["data"]["text"], "", "{response}");
    assert!(response["data"].get("path").is_none(), "{response}");

    // `clone` returns {cancelled} only.
    send(&mut stdin, r#"{"type":"clone","id":"cl0"}"#);
    eprintln!("[progress] clone sent");
    let response = read_response(&mut reader, "clone");
    eprintln!("[progress] clone answered");
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(response["data"]["cancelled"], false, "{response}");
    assert!(response["data"].get("path").is_none(), "{response}");

    // `switch_session` returns {cancelled} only (no path). Build the frame
    // with `json!`: the session path carries Windows backslashes, which a
    // raw `format!` embeds unescaped (`\U`, `\A` are invalid JSON escapes)
    // — the child would answer with a "parse" error no `read_response`
    // matches, wedging the test until the watchdog.
    send(
        &mut stdin,
        &json!({
            "type": "switch_session",
            "id": "sw0",
            "sessionPath": session_path,
        })
        .to_string(),
    );
    eprintln!("[progress] switch_session sent");
    let response = read_response(&mut reader, "switch_session");
    eprintln!("[progress] switch_session answered");
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(response["data"]["cancelled"], false, "{response}");
    assert!(response["data"].get("path").is_none(), "{response}");

    // `new_session` accepts an optional parentSession and answers with
    // {cancelled} (json! for the same backslash-escaping reason).
    send(
        &mut stdin,
        &json!({
            "type": "new_session",
            "id": "ns0",
            "parentSession": session_path,
        })
        .to_string(),
    );
    eprintln!("[progress] new_session sent");
    let response = read_response(&mut reader, "new_session");
    eprintln!("[progress] new_session answered");
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(response["data"]["cancelled"], false, "{response}");

    drop(stdin);
    eprintln!("[progress] stdin dropped, waiting for exit");
    wait_or_kill(&mut child);
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = watchdog.join();
}

#[test]
fn rpc_fork_with_entry_id_branches_and_reports_the_entry_text() {
    let (mut child, mut reader, mut stdin) = spawn_rpc();

    send(&mut stdin, r#"{"type":"hello"}"#);
    let _ = read_line(&mut reader);

    send(&mut stdin, r#"{"type":"create","id":"c0"}"#);
    let _ = read_line(&mut reader);

    // Persist one user message (the run itself fails fast against the 401
    // sandbox endpoint, but the message is stored first). Wait for the run to
    // settle (its `agent_end` terminal event) before reading the entries: the
    // user-message append happens inside the spawned run.
    send(
        &mut stdin,
        r#"{"type":"prompt","id":"p0","text":"FORK_ME_TEXT"}"#,
    );
    let _ = read_line(&mut reader);
    drain_until_agent_end(&mut reader);
    send(&mut stdin, r#"{"type":"get_entries","id":"ge0"}"#);
    let response = read_response(&mut reader, "get_entries");
    assert_eq!(response["success"], true, "{response}");
    let entry_id = response["data"]["entries"]
        .as_array()
        .and_then(|entries| {
            entries
                .iter()
                .find(|entry| {
                    entry
                        .get("message")
                        .map(|message| message.to_string().contains("FORK_ME_TEXT"))
                        .unwrap_or(false)
                })
                .and_then(|entry| entry.get("id").and_then(|id| id.as_str()))
        })
        .expect("user message entry")
        .to_owned();

    // get_tree answers with the upstream `SessionTreeNode` forest
    // (session-manager.ts:199-207): every node carries its own `entry` (the
    // same JSON shape `get_entries` returns — `entry.id` is the node id),
    // recursively nested `children` nodes, and the resolved `label`; the
    // legacy top-level `id` stays for compatibility.
    send(&mut stdin, r#"{"type":"get_tree","id":"gt0"}"#);
    let response = read_response(&mut reader, "get_tree");
    assert_eq!(response["success"], true, "{response}");
    let tree = response["data"]["tree"].as_array().expect("tree array");
    let node = tree
        .iter()
        .find(|node| node["entry"]["id"] == entry_id.as_str())
        .expect("the user-message node is a root of the tree");
    assert_eq!(node["entry"]["type"], "message", "{response}");
    assert_eq!(node["id"], entry_id.as_str(), "{response}");
    assert!(node["label"].is_null(), "{response}");
    assert!(
        node.get("entry").is_some() && node["children"].is_array(),
        "the node nests its entry and child nodes: {response}"
    );

    // fork(entryId) with the default "before" position branches at the
    // entry's PARENT and returns its user text (upstream rpc-mode.ts fork →
    // runtimeHost.fork → fork(entryId, undefined)). The entry here is the
    // FIRST user message (no parent), so the fork starts a fresh session
    // (upstream `SessionManager.create` + `newSession({parentSession})`) —
    // the branched-session view has no fork messages yet. `position: "at"`
    // branches at the entry itself, keeping the transcript.
    send(
        &mut stdin,
        &format!(r#"{{"type":"fork","id":"fk1","entryId":"{entry_id}","position":"at"}}"#),
    );
    let response = read_response(&mut reader, "fork");
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(response["data"]["cancelled"], false, "{response}");
    assert!(response["data"].get("text").is_none(), "{response}");

    // get_fork_messages now reports the branched user message.
    send(&mut stdin, r#"{"type":"get_fork_messages","id":"fm1"}"#);
    let response = read_response(&mut reader, "get_fork_messages");
    assert_eq!(response["success"], true, "{response}");
    let messages = response["data"]["messages"].as_array().expect("messages");
    assert!(
        messages
            .iter()
            .any(|message| message["text"] == "FORK_ME_TEXT"),
        "{response}"
    );

    drop(stdin);
    wait_or_kill(&mut child);
}

// ---------------------------------------------------------------------------
// In-process streaming race: a prompt that arrives while the agent streams
// requires an explicit `streamingBehavior` — without one it is refused with
// the upstream error, with "steer"/"followUp" it is QUEUED. Uses a scripted
// provider whose first turn parks inside a gate tool, so "streaming" is
// deterministic.
// ---------------------------------------------------------------------------

fn fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake model".into(),
        api: "fake-api".into(),
        provider: "fake".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 4096,
        max_tokens: 256,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

fn text(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

fn assistant(content: Vec<ContentBlock>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content,
        stop_reason,
        timestamp: 1,
        ..Default::default()
    }
}

fn tool_call(id: &str, name: &str) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: BTreeMap::new(),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

/// Responder signature: (call index, request context) -> assistant message.
type Responder = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

/// A fake provider that answers every stream with a scripted message.
struct FakeProvider {
    calls: AtomicUsize,
    respond: Responder,
}

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let message = (self.respond)(index, &context);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            message,
        ));
        stream
    }
}

fn fake_registry(respond: Responder) -> Arc<ProviderRegistry> {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
        respond,
    }));
    Arc::new(registry)
}

/// A tool whose run blocks until the gate opens or the context aborts.
/// `entered` counts run starts so tests can await tool entry.
#[derive(Clone)]
struct GateTool {
    gate: tokio::sync::watch::Receiver<bool>,
    entered: Arc<AtomicUsize>,
}

impl AgentTool for GateTool {
    fn name(&self) -> &'static str {
        "gate"
    }
    fn description(&self) -> &'static str {
        "blocks until released"
    }
    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({"type": "object"}))
    }
    fn run(&self, _args: &serde_json::Value, ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let mut gate = self.gate.clone();
        let aborted = ctx.signal.clone();
        Box::pin(async move {
            loop {
                if aborted.as_ref().is_some_and(|signal| signal.is_aborted()) {
                    return ToolResult::error("aborted");
                }
                if *gate.borrow() {
                    return ToolResult::ok("gate open");
                }
                if tokio::time::timeout(std::time::Duration::from_millis(20), gate.changed())
                    .await
                    .is_err()
                {
                    continue;
                }
            }
        })
    }
}

fn tool_registry_with(
    gate: Option<(tokio::sync::watch::Receiver<bool>, Arc<AtomicUsize>)>,
) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    if let Some((gate, entered)) = gate {
        tools.register(Arc::new(GateTool { gate, entered }));
    }
    tools
}

/// Factory whose first model turn requests the gate tool, parking the run
/// (shared `entered` counter) while the test injects concurrent input.
fn gated_factory(
    gate_rx: tokio::sync::watch::Receiver<bool>,
    entered: Arc<AtomicUsize>,
) -> SessionFactory {
    Arc::new(move |session: Option<SessionManager>| {
        let gate_rx = gate_rx.clone();
        let registry = fake_registry(Arc::new(|index, _context| {
            if index == 0 {
                assistant(vec![tool_call("call-1", "gate")], StopReason::ToolUse)
            } else {
                assistant(vec![text("rpc reply")], StopReason::Stop)
            }
        }));
        Ok(Arc::new(AgentSession::new(AgentSessionConfig {
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: tool_registry_with(Some((gate_rx, Arc::clone(&entered)))),
            session,
            compaction: None,
            max_turns: 4,
            system_prompt: None,
            cwd: None,
            event_stream: None,
            extensions: None,
            tool_selection: None,
        })))
    })
}

/// Model-issued tool call carrying the scheduler's `background: true`
/// argument (preflight strips it before the tool ever sees it).
fn background_tool_call(id: &str, name: &str) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: BTreeMap::from([("background".to_owned(), json!(true))]),
        thought_signature: None,
        namespace: None,
        arguments_error: None,
    })
}

/// Factory whose first model turn requests the gate tool with
/// `background: true`: the run settles on the synthesized placeholder while
/// the detached call stays parked in the session's background registry (the
/// gate never opens).
fn background_gated_factory(
    gate_rx: tokio::sync::watch::Receiver<bool>,
    entered: Arc<AtomicUsize>,
) -> SessionFactory {
    Arc::new(move |session: Option<SessionManager>| {
        let gate_rx = gate_rx.clone();
        let registry = fake_registry(Arc::new(|index, _context| {
            if index == 0 {
                assistant(
                    vec![background_tool_call("call-bg", "gate")],
                    StopReason::ToolUse,
                )
            } else {
                assistant(vec![text("rpc reply")], StopReason::Stop)
            }
        }));
        Ok(Arc::new(AgentSession::new(AgentSessionConfig {
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: tool_registry_with(Some((gate_rx, Arc::clone(&entered)))),
            session,
            compaction: None,
            max_turns: 4,
            system_prompt: None,
            cwd: None,
            event_stream: None,
            extensions: None,
            tool_selection: None,
        })))
    })
}

/// Per-test temp session directory.
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-rpc-cmds-suite-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Client half of an in-memory stdio pair: write JSON lines, read JSON lines.
struct RpcClient {
    writer: tokio::sync::mpsc::UnboundedSender<String>,
    reader: tokio::sync::mpsc::UnboundedReceiver<String>,
}

impl RpcClient {
    fn send(&self, value: serde_json::Value) {
        self.writer
            .send(serde_json::to_string(&value).expect("serialize"))
            .expect("send command");
    }

    /// Read lines until `predicate` matches. Every skipped line must be a
    /// BARE event (never the `{"type":"event",...}` wrapper).
    async fn wait_for(
        &mut self,
        predicate: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        for _ in 0..2000 {
            let line = self.reader.recv().await.expect("protocol line");
            let value: serde_json::Value =
                serde_json::from_str(&line).expect("rpc line must be valid JSON");
            assert_ne!(
                value["type"], "event",
                "events must not be wrapped: {value}"
            );
            if predicate(&value) {
                return value;
            }
        }
        panic!("expected protocol line never arrived");
    }
}

async fn rpc_pair_with(label: &str, factory: SessionFactory) -> (RpcClient, TempDir) {
    let temp = TempDir::new(label);
    let server = RpcServer::new(temp.0.clone(), factory);
    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    tokio::spawn(async move { server.serve(server_read, server_write).await });

    let (client_read, mut client_write) = tokio::io::split(client_stream);

    // Reader: forward server output lines to the client receiver.
    let (line_tx, line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut lines = TokioBufReader::new(client_read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    // Writer: pull lines from the client sender and push them to the server.
    let (write_tx, mut write_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(mut line) = write_rx.recv().await {
            line.push('\n');
            if client_write.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = client_write.flush().await;
        }
    });

    (
        RpcClient {
            writer: write_tx,
            reader: line_rx,
        },
        temp,
    )
}

async fn wait_for_tool_entry(entered: &Arc<AtomicUsize>) {
    for _ in 0..2000 {
        if entered.load(Ordering::SeqCst) > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("gate tool never started");
}

#[tokio::test]
async fn rpc_prompt_while_streaming_requires_streaming_behavior() {
    let (gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let (mut client, _temp) =
        rpc_pair_with("streaming-queue", gated_factory(gate_rx, entered.clone())).await;

    client.send(serde_json::json!({ "type": "create" }));
    let _created = client
        .wait_for(|value| value["command"] == "create" && value["success"] == true)
        .await;

    // First prompt: accepted, its run parks inside the gate tool.
    client.send(serde_json::json!({ "id": "p1", "type": "prompt", "text": "start" }));
    let first = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p1")
        .await;
    assert_eq!(first["success"], serde_json::json!(true), "{first}");
    assert_eq!(first["data"]["disposition"], "started", "{first}");
    wait_for_tool_entry(&entered).await;

    // steer while streaming reports the real "queued" disposition.
    client.send(serde_json::json!({ "type": "steer", "text": "redirect" }));
    let steered = client.wait_for(|value| value["command"] == "steer").await;
    assert_eq!(steered["success"], serde_json::json!(true), "{steered}");
    assert_eq!(steered["data"]["disposition"], "queued", "{steered}");

    // follow_up while streaming is also "queued".
    client.send(serde_json::json!({ "type": "follow_up", "text": "afterwards" }));
    let followed = client
        .wait_for(|value| value["command"] == "follow_up")
        .await;
    assert_eq!(followed["success"], serde_json::json!(true), "{followed}");
    assert_eq!(followed["data"]["disposition"], "queued", "{followed}");

    // A second PROMPT while the agent streams REQUIRES an explicit
    // streamingBehavior (upstream agent-session.ts:2014-2022): without one
    // the prompt is REFUSED with the upstream error, never silently steered.
    client.send(serde_json::json!({ "id": "p2", "type": "prompt", "text": "second" }));
    let refused = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p2")
        .await;
    assert_eq!(refused["success"], serde_json::json!(false), "{refused}");
    assert_eq!(
        refused["error"],
        serde_json::json!(
            "Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message."
        ),
        "{refused}"
    );

    // With an explicit streamingBehavior the streaming prompt queues:
    // "steer" routes into the steering queue, "followUp" into the follow-up
    // queue; both report disposition "queued" and are acknowledged.
    client.send(serde_json::json!({
        "id": "p2s",
        "type": "prompt",
        "text": "second",
        "streamingBehavior": "steer"
    }));
    let steered_prompt = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p2s")
        .await;
    assert_eq!(
        steered_prompt["success"],
        serde_json::json!(true),
        "{steered_prompt}"
    );
    assert_eq!(
        steered_prompt["data"]["disposition"], "queued",
        "{steered_prompt}"
    );

    client.send(serde_json::json!({
        "id": "p2f",
        "type": "prompt",
        "text": "later",
        "streamingBehavior": "followUp"
    }));
    let followed_prompt = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p2f")
        .await;
    assert_eq!(
        followed_prompt["success"],
        serde_json::json!(true),
        "{followed_prompt}"
    );
    assert_eq!(
        followed_prompt["data"]["disposition"], "queued",
        "{followed_prompt}"
    );

    // Release the gate; the run settles. `agent_end` precedes the session's
    // idle transition, so wait for `agent_settled` (the idle edge) before
    // sending the next prompt: a prompt in that window is correctly refused
    // as "agent is streaming".
    let _ = gate_tx.send(true);
    client
        .wait_for(|value| value["type"] == "agent_settled")
        .await;

    // After the run settles the prompt claim is free again.
    client.send(serde_json::json!({ "id": "p3", "type": "prompt", "text": "third" }));
    let third = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p3")
        .await;
    assert_eq!(third["success"], serde_json::json!(true), "{third}");
    assert_eq!(third["data"]["disposition"], "started", "{third}");
}

// ---------------------------------------------------------------------------
// Background tool-call members (pir extension, TOOL-BACKGROUND-SCOPE.md §2.5)
// ---------------------------------------------------------------------------

#[test]
fn rpc_background_members_require_attachment_and_keep_their_shapes() {
    let (mut child, mut reader, mut stdin) = spawn_rpc();

    send(&mut stdin, r#"{"type":"hello"}"#);
    let _ = read_response(&mut reader, "hello");

    // Every member goes through `attached()`. The CLI's startup session is
    // attached before the first line is read (§2 #4), so each command answers
    // with its typed payload instead of the no-session error.
    send(&mut stdin, r#"{"type":"background_calls","id":"bg0"}"#);
    let response = read_response(&mut reader, "background_calls");
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(response["data"], json!([]), "{response}");

    send(
        &mut stdin,
        r#"{"type":"abort_background","id":"bg1","callId":"call-x"}"#,
    );
    let response = read_response(&mut reader, "abort_background");
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(response["data"]["inFlight"], false, "{response}");

    send(&mut stdin, r#"{"type":"abort_all_background","id":"bg2"}"#);
    let response = read_response(&mut reader, "abort_all_background");
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(response["data"], json!(0), "{response}");

    // Attach a real (persisted) session; the model is never called.
    send(&mut stdin, r#"{"type":"create","id":"c0"}"#);
    let created = read_response(&mut reader, "create");
    assert_eq!(created["success"], true, "{created}");

    // Nothing in flight: an EMPTY ARRAY (not null, not an object wrapper).
    send(&mut stdin, r#"{"type":"background_calls","id":"bg3"}"#);
    let response = read_response(&mut reader, "background_calls");
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(response["data"], json!([]), "{response}");

    // An unknown call id is a successful no-op: `inFlight: false`, never an
    // error (the call may simply have settled already).
    send(
        &mut stdin,
        r#"{"type":"abort_background","id":"bg4","callId":"call-missing"}"#,
    );
    let response = read_response(&mut reader, "abort_background");
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(response["data"]["inFlight"], false, "{response}");

    // A missing `callId` is a named command error, not a crash.
    send(&mut stdin, r#"{"type":"abort_background","id":"bg5"}"#);
    let response = read_response(&mut reader, "abort_background");
    assert_eq!(response["success"], false, "{response}");
    assert!(
        response["error"]
            .as_str()
            .unwrap_or_default()
            .contains("callId"),
        "{response}"
    );

    // Nothing in flight: zero cancelled.
    send(&mut stdin, r#"{"type":"abort_all_background","id":"bg6"}"#);
    let response = read_response(&mut reader, "abort_all_background");
    assert_eq!(response["success"], true, "{response}");
    assert_eq!(response["data"], json!(0), "{response}");

    // The process must survive and keep serving.
    send(&mut stdin, r#"{"type":"hello"}"#);
    let hello = read_response(&mut reader, "hello");
    assert_eq!(hello["command"], "hello", "{hello}");

    drop(stdin);
    wait_or_kill(&mut child);
}

#[tokio::test]
async fn rpc_background_calls_reports_a_detached_call_and_aborts_it() {
    // The gate never opens, so the detached call stays in flight for the
    // whole test (the run itself settles on the placeholder result).
    let (_gate_tx, gate_rx) = tokio::sync::watch::channel(false);
    let entered = Arc::new(AtomicUsize::new(0));
    let (mut client, _temp) = rpc_pair_with(
        "background-members",
        background_gated_factory(gate_rx, Arc::clone(&entered)),
    )
    .await;

    client.send(json!({ "type": "create" }));
    let _created = client
        .wait_for(|value| value["command"] == "create" && value["success"] == true)
        .await;

    client.send(json!({ "id": "p1", "type": "prompt", "text": "start the background call" }));
    let started = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p1")
        .await;
    assert_eq!(started["success"], json!(true), "{started}");
    // The run completes on the placeholder while the real call keeps running:
    // a failure here means the session never installed its detach sink.
    client.wait_for(|value| value["type"] == "agent_end").await;
    wait_for_tool_entry(&entered).await;

    // The snapshot reports the in-flight call with the same camelCase keys
    // the `background_tool_end` event uses; a model-issued call has no
    // parent, so `parentToolCallId` is omitted.
    client.send(json!({ "type": "background_calls", "id": "bg0" }));
    let snapshot = client
        .wait_for(|value| value["command"] == "background_calls" && value["id"] == "bg0")
        .await;
    assert_eq!(snapshot["success"], json!(true), "{snapshot}");
    let calls = snapshot["data"].as_array().expect("snapshot array");
    assert_eq!(calls.len(), 1, "{snapshot}");
    assert_eq!(calls[0]["callId"], "call-bg", "{snapshot}");
    assert_eq!(calls[0]["toolName"], "gate", "{snapshot}");
    assert!(
        calls[0].get("parentToolCallId").is_none(),
        "a model-issued call carries no parentToolCallId: {snapshot}"
    );

    // Aborting the in-flight call reports `inFlight: true` and drops it from
    // the registry synchronously (a second snapshot is empty).
    client.send(json!({ "type": "abort_background", "id": "bg1", "callId": "call-bg" }));
    let aborted = client
        .wait_for(|value| value["command"] == "abort_background" && value["id"] == "bg1")
        .await;
    assert_eq!(aborted["success"], json!(true), "{aborted}");
    assert_eq!(aborted["data"]["inFlight"], json!(true), "{aborted}");

    client.send(json!({ "type": "background_calls", "id": "bg2" }));
    let empty = client
        .wait_for(|value| value["command"] == "background_calls" && value["id"] == "bg2")
        .await;
    assert_eq!(empty["data"], json!([]), "{empty}");

    // Re-aborting the now settled id is a no-op `false`, and nothing is left
    // to cancel-all.
    client.send(json!({ "type": "abort_background", "id": "bg3", "callId": "call-bg" }));
    let noop = client
        .wait_for(|value| value["command"] == "abort_background" && value["id"] == "bg3")
        .await;
    assert_eq!(noop["success"], json!(true), "{noop}");
    assert_eq!(noop["data"]["inFlight"], json!(false), "{noop}");

    client.send(json!({ "type": "abort_all_background", "id": "bg4" }));
    let none = client
        .wait_for(|value| value["command"] == "abort_all_background" && value["id"] == "bg4")
        .await;
    assert_eq!(none["success"], json!(true), "{none}");
    assert_eq!(none["data"], json!(0), "{none}");
}

// ---------------------------------------------------------------------------
// RPC prompt `images` (upstream `RpcCommand.prompt.images`) and the run
// settle event (`agent_settled`).
// ---------------------------------------------------------------------------

/// Factory that records the request context the model receives and answers
/// every call with plain text. Shared by the image-prompt and settle tests.
fn recording_factory(captured: Arc<std::sync::Mutex<Option<Context>>>) -> SessionFactory {
    Arc::new(move |session: Option<SessionManager>| {
        let captured = Arc::clone(&captured);
        let registry = fake_registry(Arc::new(move |_index, context| {
            *captured
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(context.clone());
            assistant(vec![text("saw images")], StopReason::Stop)
        }));
        Ok(Arc::new(AgentSession::new(AgentSessionConfig {
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session,
            compaction: None,
            max_turns: 2,
            system_prompt: None,
            cwd: None,
            event_stream: None,
            extensions: None,
            tool_selection: None,
        })))
    })
}

/// The RPC prompt `images` array (wire shape
/// `{"type":"image","data":<base64>,"mimeType":<mime>}`) joins the user
/// message: prompt text first, images in order after it.
#[tokio::test]
async fn rpc_prompt_images_ride_the_user_message() {
    let captured: Arc<std::sync::Mutex<Option<Context>>> = Arc::new(std::sync::Mutex::new(None));
    let (mut client, _temp) =
        rpc_pair_with("prompt-images", recording_factory(Arc::clone(&captured))).await;

    client.send(json!({ "type": "create" }));
    let created = client
        .wait_for(|value| value["command"] == "create" && value["success"] == true)
        .await;
    assert_eq!(created["success"], json!(true), "{created}");

    client.send(json!({
        "id": "p1",
        "type": "prompt",
        "text": "look",
        "images": [{ "type": "image", "data": "aGVsbG8=", "mimeType": "image/png" }]
    }));
    let started = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p1")
        .await;
    assert_eq!(started["success"], json!(true), "{started}");
    assert_eq!(started["data"]["disposition"], "started", "{started}");
    client.wait_for(|value| value["type"] == "agent_end").await;

    let context = captured
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
        .expect("the fake provider must receive the run context");
    let user = context
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::User(user) => Some(user),
            _ => None,
        })
        .expect("the request context must carry the starting user message");

    // The starting `user_message` is the prompt text followed by the ordered
    // image blocks (src/agent_session.rs `user_message`).
    assert_eq!(
        user.content,
        vec![
            text("look"),
            ContentBlock::Image(ImageContent {
                data: "aGVsbG8=".into(),
                mime_type: "image/png".into(),
            }),
        ],
        "the RPC images must ride the user message after the prompt text"
    );
}

/// A successful run settles with `agent_settled { aborted: false }` on the
/// RPC event stream. The timeout turns a missing event (lane B not merged
/// yet) into a clear failure instead of an indefinitely parked test.
/// RPC attach mirrors upstream `assertSessionCwdExists`: a persisted session
/// whose stored cwd no longer exists is refused instead of failing later at
/// the first tool call (upstream `session-cwd.ts:54-59`).
#[tokio::test]
async fn rpc_attach_rejects_a_session_whose_cwd_is_gone() {
    let captured: Arc<std::sync::Mutex<Option<Context>>> = Arc::new(std::sync::Mutex::new(None));
    let (mut client, temp) =
        rpc_pair_with("missing-cwd", recording_factory(Arc::clone(&captured))).await;

    let missing = temp.0.join("gone-project");
    let mut seeded =
        SessionManager::create(missing.to_string_lossy().as_ref(), &temp.0).expect("create");
    seeded
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text("hello")],
            tools_added: None,
            tools_removed: None,
            timestamp: 1,
            sections: None,
        }))
        .expect("append");
    let path = seeded.path().to_path_buf();
    drop(seeded);

    client.send(json!({
        "id": "a1",
        "type": "attach",
        "session": path.to_string_lossy(),
    }));
    let response = client
        .wait_for(|value| value["command"] == "attach" && value["id"] == "a1")
        .await;
    assert_eq!(response["success"], json!(false), "{response}");
    let error = response["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("Stored session working directory does not exist"),
        "{response}"
    );
}

#[tokio::test]
async fn rpc_agent_settled_follows_the_run_with_aborted_false() {
    let captured: Arc<std::sync::Mutex<Option<Context>>> = Arc::new(std::sync::Mutex::new(None));
    let (mut client, _temp) = rpc_pair_with("agent-settled", recording_factory(captured)).await;

    client.send(json!({ "type": "create" }));
    let created = client
        .wait_for(|value| value["command"] == "create" && value["success"] == true)
        .await;
    assert_eq!(created["success"], json!(true), "{created}");

    client.send(json!({ "id": "p1", "type": "prompt", "text": "settle" }));
    let started = client
        .wait_for(|value| value["command"] == "prompt" && value["id"] == "p1")
        .await;
    assert_eq!(started["success"], json!(true), "{started}");
    assert_eq!(started["data"]["disposition"], "started", "{started}");

    let settled = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.wait_for(|value| value["type"] == "agent_settled"),
    )
    .await
    .expect("agent_settled must reach the RPC event stream (lane B)");
    assert_eq!(settled["aborted"], json!(false), "{settled}");
}

// ---------------------------------------------------------------------------
// §6 #13-#15: compact wire shape, get_commands sourceInfo, full get_messages
// ---------------------------------------------------------------------------

/// A session-manager fixture on disk (the file exists after the first append)
/// that the RPC host attaches by path.
fn seeded_session(
    label: &str,
    seed: impl FnOnce(&mut SessionManager),
) -> (TempDir, std::path::PathBuf) {
    let temp = TempDir::new(label);
    // The RPC `attach` path rejects a stored cwd that no longer exists
    // (`assertSessionCwdExists`), so the fixture's cwd must be the live temp
    // directory.
    let cwd = temp.0.to_string_lossy().into_owned();
    let mut manager = SessionManager::create(&cwd, &temp.0).expect("create session");
    seed(&mut manager);
    let path = manager.path().to_path_buf();
    assert!(
        path.exists(),
        "the seeded session file must exist: {path:?}"
    );
    (temp, path)
}

fn seed_user(manager: &mut SessionManager, body: &str) -> String {
    manager
        .append_message(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![text(body)],
            tools_added: None,
            tools_removed: None,
            sections: None,
            timestamp: 1,
        }))
        .expect("append user")
}

fn seed_assistant(manager: &mut SessionManager, body: &str) -> String {
    manager
        .append_message(Message::Assistant(assistant(
            vec![text(body)],
            StopReason::Stop,
        )))
        .expect("append assistant")
}

async fn attach_path(client: &mut RpcClient, path: &std::path::Path) {
    client.send(json!({
        "type": "attach",
        "session": path.display().to_string(),
    }));
    // Match the response whatever its outcome (asserting on success only would
    // hide a failed attach behind the helper's next read).
    let attached = client.wait_for(|value| value["command"] == "attach").await;
    assert_eq!(attached["success"], json!(true), "{attached}");
}

/// `get_messages` defaults to the COMPLETE upstream `session.messages` view:
/// extension `custom_message` records keep their `custom` role (customType,
/// display, details, timestamp) instead of being flattened into user text.
/// `view: "llm"` still answers the old LLM projection.
#[tokio::test]
async fn rpc_get_messages_defaults_to_the_full_session_view() {
    let (temp, path) = seeded_session("full-messages", |manager| {
        seed_user(manager, "HELLO_FROM_USER");
        manager
            .append_custom_message_entry(
                "test-note",
                json!([{ "type": "text", "text": "CUSTOM_UI_TEXT" }]),
                true,
                Some(json!({ "kind": "ui" })),
            )
            .expect("append custom message");
        seed_assistant(manager, "HELLO_FROM_ASSISTANT");
    });
    let (mut client, _server_temp) = rpc_pair_with(
        "full-messages",
        recording_factory(Arc::new(std::sync::Mutex::new(None))),
    )
    .await;
    attach_path(&mut client, &path).await;

    client.send(json!({ "id": "gm-full", "type": "get_messages" }));
    let response = client
        .wait_for(|value| value["command"] == "get_messages" && value["id"] == "gm-full")
        .await;
    assert_eq!(response["success"], json!(true), "{response}");
    let messages = response["data"]["messages"]
        .as_array()
        .unwrap_or_else(|| panic!("{response}"));
    let custom = messages
        .iter()
        .find(|message| message["role"] == "custom")
        .unwrap_or_else(|| panic!("missing custom record: {response}"));
    assert_eq!(custom["customType"], json!("test-note"), "{custom}");
    assert_eq!(custom["display"], json!(true), "{custom}");
    assert_eq!(custom["details"], json!({ "kind": "ui" }), "{custom}");
    assert!(custom["timestamp"].is_number(), "{custom}");
    assert!(
        custom["content"].to_string().contains("CUSTOM_UI_TEXT"),
        "{custom}"
    );
    assert!(
        messages.iter().any(|message| message["role"] == "user"),
        "{response}"
    );
    assert!(
        messages
            .iter()
            .any(|message| message["role"] == "assistant"),
        "{response}"
    );

    // The projection stays reachable explicitly.
    client.send(json!({ "id": "gm-llm", "type": "get_messages", "view": "llm" }));
    let response = client
        .wait_for(|value| value["command"] == "get_messages" && value["id"] == "gm-llm")
        .await;
    assert_eq!(response["success"], json!(true), "{response}");
    let messages = response["data"]["messages"]
        .as_array()
        .unwrap_or_else(|| panic!("{response}"));
    assert!(
        messages.iter().all(|message| message["role"] != "custom"),
        "the LLM projection must not carry custom roles: {response}"
    );
    assert!(
        response["data"]["messages"]
            .to_string()
            .contains("CUSTOM_UI_TEXT"),
        "the projection renders the custom message as user text: {response}"
    );

    // An unknown view is a named error, not a silent fallback.
    client.send(json!({ "id": "gm-bad", "type": "get_messages", "view": "bogus" }));
    let response = client
        .wait_for(|value| value["command"] == "get_messages" && value["id"] == "gm-bad")
        .await;
    assert_eq!(response["success"], json!(false), "{response}");
    assert!(
        response["error"]
            .as_str()
            .unwrap_or_default()
            .contains("view"),
        "{response}"
    );

    drop(client);
    drop(_server_temp);
    drop(temp);
}

/// Factory whose provider answers every call (the summarizer included) with
/// one text response carrying a non-zero usage.
fn compact_factory() -> SessionFactory {
    Arc::new(move |session: Option<SessionManager>| {
        let registry = fake_registry(Arc::new(|_index, _context| {
            let mut message = assistant(vec![text("COMPACTED SUMMARY")], StopReason::Stop);
            message.usage = Usage {
                input: 111,
                output: 22,
                total_tokens: 133,
                ..Default::default()
            };
            message
        }));
        Ok(Arc::new(AgentSession::new(AgentSessionConfig {
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session,
            compaction: Some(pir::compaction::CompactionConfig {
                enabled: true,
                reserve_tokens: 1,
                keep_recent_tokens: 1,
            }),
            max_turns: 4,
            system_prompt: None,
            cwd: None,
            event_stream: None,
            extensions: None,
            tool_selection: None,
        })))
    })
}

/// `compact` answers with the full upstream `CompactionResult`:
/// `usage` from the summarizer response and `details` (the persisted
/// `CompactionDetails` file lists), alongside the existing summary/token/cut
/// fields.
#[tokio::test]
async fn rpc_compact_returns_usage_and_details() {
    let (temp, path) = seeded_session("compact-wire", |manager| {
        for index in 0..4 {
            seed_user(
                manager,
                &format!("user turn {index} with some words to summarize"),
            );
            seed_assistant(manager, &format!("assistant reply {index}"));
        }
    });
    let (mut client, _server_temp) = rpc_pair_with("compact-wire", compact_factory()).await;
    attach_path(&mut client, &path).await;

    client.send(json!({
        "id": "compact-1",
        "type": "compact",
        "customInstructions": "keep it short",
    }));
    let response = client
        .wait_for(|value| value["command"] == "compact" && value["id"] == "compact-1")
        .await;
    assert_eq!(response["success"], json!(true), "{response}");
    let data = &response["data"];
    // The fixture forces a split turn, so the summarizer runs twice (history +
    // turn prefix) and the combined summary repeats the scripted text.
    assert!(
        data["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("COMPACTED SUMMARY"),
        "{data}"
    );
    assert!(data["tokensBefore"].is_number(), "{data}");
    assert!(data["estimatedTokensAfter"].is_number(), "{data}");
    assert!(data["firstKeptIndex"].is_number(), "{data}");
    assert!(
        data.get("firstKeptEntryId").is_some(),
        "firstKeptEntryId must be present (null when unpersisted): {data}"
    );
    let usage = data
        .get("usage")
        .and_then(|usage| usage.as_object())
        .unwrap_or_else(|| panic!("usage must be an object: {data}"));
    assert!(
        usage
            .get("input")
            .and_then(|input| input.as_u64())
            .is_some_and(|input| input >= 111),
        "usage must carry the summarizer response's input tokens: {data}"
    );
    let details = data
        .get("details")
        .and_then(|details| details.as_object())
        .unwrap_or_else(|| panic!("details must be an object: {data}"));
    assert!(details["readFiles"].is_array(), "{data}");
    assert!(details["modifiedFiles"].is_array(), "{data}");

    drop(client);
    drop(_server_temp);
    drop(temp);
}

/// `SourceInfo` provenance matches upstream `getDefaultSourceInfoForPath`
/// (user/project roots, temporary otherwise, synthetic prefixes) and only
/// fills fields the producer did not send.
#[test]
#[cfg_attr(
    target_os = "linux",
    ignore = "asserts Windows source provenance paths"
)]
fn command_source_info_reports_upstream_provenance() {
    use pir::extensions::SourceInfo;
    use std::path::Path;

    let cwd = Path::new("C:\\repo");
    let agent_dir = Path::new("C:\\home\\.pi\\agent");

    let user =
        SourceInfo::for_resource_path(agent_dir.join("prompts").join("deploy.md"), cwd, agent_dir);
    assert_eq!(user.source, "local");
    assert_eq!(user.scope, "user");
    assert_eq!(user.origin, "top-level");
    assert!(user.path.ends_with("deploy.md"), "{user:?}");
    assert!(
        user.base_dir
            .as_deref()
            .is_some_and(|dir| dir.ends_with("prompts")),
        "{user:?}"
    );

    let project = SourceInfo::for_resource_path(
        cwd.join(".pi").join("skills").join("foo").join("SKILL.md"),
        cwd,
        agent_dir,
    );
    assert_eq!(project.scope, "project", "{project:?}");
    assert_eq!(project.source, "local", "{project:?}");
    assert!(
        project
            .base_dir
            .as_deref()
            .is_some_and(|dir| dir.ends_with("skills")),
        "{project:?}"
    );

    let elsewhere = SourceInfo::for_resource_path("C:\\elsewhere\\notes\\x.md", cwd, agent_dir);
    assert_eq!(elsewhere.scope, "temporary", "{elsewhere:?}");
    assert_eq!(elsewhere.source, "local", "{elsewhere:?}");
    assert!(
        elsewhere
            .base_dir
            .as_deref()
            .is_some_and(|dir| dir.ends_with("notes")),
        "{elsewhere:?}"
    );

    let builtin = SourceInfo::for_resource_path("builtin:mcp", cwd, agent_dir);
    assert_eq!(builtin.source, "builtin", "{builtin:?}");
    assert_eq!(builtin.scope, "temporary", "{builtin:?}");
    assert!(builtin.base_dir.is_none(), "{builtin:?}");

    let inline = SourceInfo::for_resource_path("<inline:demo>", cwd, agent_dir);
    assert_eq!(inline.source, "inline", "{inline:?}");

    // A producer that sent richer metadata keeps every value it sent.
    let sent = SourceInfo {
        path: agent_dir
            .join("prompts")
            .join("deploy.md")
            .to_string_lossy()
            .into_owned(),
        source: "package".into(),
        scope: "project".into(),
        origin: "package".into(),
        base_dir: Some("C:\\pkg".into()),
    };
    let filled = sent.clone().filled_from_path(cwd, agent_dir);
    assert_eq!(filled, sent, "known fields must never be overwritten");

    // Unknown fields are omitted on the wire (never sent as empty strings).
    let value = serde_json::to_value(SourceInfo::from_path("x.md")).expect("serialize");
    assert_eq!(value, json!({ "path": "x.md" }), "{value}");
}

/// A `user_bash` handler that returns custom `BashOperations`: the RPC `bash`
/// command executes them through the session
/// (`execute_user_bash_operations`, upstream rpc-mode.ts:579-583) and answers
/// with the resulting `BashResult`, recorded exactly once.
const CUSTOM_OPERATIONS_EXTENSION: &str = r#"
export default function (pi) {
    pi.on("user_bash", async () => ({
        operations: {
            async exec(command, cwd, options) {
                options.onData(`out:${command}@${cwd}`);
                options.onData("!");
                return { exitCode: 7 };
            },
        },
    }));
}
"#;

async fn operations_factory(temp: &TempDir) -> SessionFactory {
    let extension_path = temp.0.join("custom-operations.ts");
    std::fs::write(&extension_path, CUSTOM_OPERATIONS_EXTENSION).expect("write extension");
    let handle = ExtensionHandle::start(ExtensionConfig {
        cwd: temp.0.clone(),
        mode: "rpc".to_string(),
        has_ui: false,
        limits: Some(Default::default()),
        ..Default::default()
    })
    .expect("extension runtime should start");
    let loaded = handle
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("extension load should not fail at the transport level");
    assert!(
        loaded.errors.is_empty(),
        "extension load errors: {:?}",
        loaded.errors
    );
    Arc::new(move |session: Option<SessionManager>| {
        Ok(Arc::new(AgentSession::new(AgentSessionConfig {
            provider_registry: fake_registry(Arc::new(|_index, _context| {
                assistant(vec![text("unused")], StopReason::Stop)
            })),
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session,
            compaction: None,
            max_turns: 2,
            system_prompt: None,
            cwd: None,
            event_stream: None,
            extensions: Some(handle.clone()),
            tool_selection: None,
        })))
    })
}

#[tokio::test]
async fn rpc_bash_executes_extension_custom_operations_once() {
    let temp = TempDir::new("bash-operations");
    let factory = operations_factory(&temp).await;
    let (mut client, _server_temp) = rpc_pair_with("bash-operations", factory).await;

    client.send(json!({ "type": "create" }));
    let created = client
        .wait_for(|value| value["command"] == "create" && value["success"] == true)
        .await;
    assert_eq!(created["success"], json!(true), "{created}");

    client.send(json!({
        "id": "ops-1",
        "type": "bash",
        "command": "echo hi",
        "excludeFromContext": false,
    }));
    let response = client
        .wait_for(|value| value["command"] == "bash" && value["id"] == "ops-1")
        .await;
    assert_eq!(response["success"], json!(true), "{response}");
    let data = &response["data"];
    let output = data["output"].as_str().unwrap_or_default();
    assert!(output.starts_with("out:echo hi@"), "{data}");
    assert!(output.contains('!'), "{data}");
    assert_eq!(data["exitCode"], json!(7), "{data}");
    assert_eq!(data["cancelled"], json!(false), "{data}");

    // The executor records the result itself: exactly ONE bashExecution entry,
    // visible in the full `get_messages` view.
    client.send(json!({ "id": "gm-ops", "type": "get_messages" }));
    let messages = client
        .wait_for(|value| value["command"] == "get_messages" && value["id"] == "gm-ops")
        .await;
    assert_eq!(messages["success"], json!(true), "{messages}");
    let records: Vec<&serde_json::Value> = messages["data"]["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|message| message["role"] == "bashExecution")
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(records.len(), 1, "recorded exactly once: {messages}");
    assert_eq!(records[0]["command"], json!("echo hi"), "{messages}");

    drop(client);
    drop(_server_temp);
    drop(temp);
}

/// The `session_compact` extension payload carries the full
/// `CompactionResult` under `compaction` — the field the prelude's
/// `ctx.compact({ onComplete })` settler reads (`payload.compaction ??
/// payload.result`). `onError` already carried `errorMessage`.
const COMPACT_PAYLOAD_EXTENSION: &str = r#"
export default function (pi) {
    pi.on("session_compact", (event) => {
        globalThis.__seenCompact = event;
    });
}
"#;

async fn compact_payload_factory(temp: &TempDir) -> (SessionFactory, ExtensionHandle) {
    let extension_path = temp.0.join("compact-payload.ts");
    std::fs::write(&extension_path, COMPACT_PAYLOAD_EXTENSION).expect("write extension");
    let handle = ExtensionHandle::start(ExtensionConfig {
        cwd: temp.0.clone(),
        mode: "rpc".to_string(),
        has_ui: false,
        limits: Some(Default::default()),
        ..Default::default()
    })
    .expect("extension runtime should start");
    let loaded = handle
        .load(std::slice::from_ref(&extension_path))
        .await
        .expect("extension load should not fail at the transport level");
    assert!(
        loaded.errors.is_empty(),
        "extension load errors: {:?}",
        loaded.errors
    );
    let session_handle = handle.clone();
    let factory: SessionFactory = Arc::new(move |session: Option<SessionManager>| {
        let registry = fake_registry(Arc::new(|_index, _context| {
            assistant(vec![text("COMPACTED SUMMARY")], StopReason::Stop)
        }));
        Ok(Arc::new(AgentSession::new(AgentSessionConfig {
            provider_registry: registry,
            model: fake_model(),
            api_key: None,
            thinking: ThinkingLevel::Off,
            tools: ToolRegistry::new(),
            session,
            compaction: Some(pir::compaction::CompactionConfig {
                enabled: true,
                reserve_tokens: 1,
                keep_recent_tokens: 1,
            }),
            max_turns: 4,
            system_prompt: None,
            cwd: None,
            event_stream: None,
            extensions: Some(session_handle.clone()),
            tool_selection: None,
        })))
    });
    (factory, handle)
}

#[tokio::test]
async fn rpc_compact_event_carries_the_full_result_to_extensions() {
    let (temp, path) = seeded_session("compact-payload", |manager| {
        for index in 0..3 {
            seed_user(manager, &format!("user turn {index}"));
            seed_assistant(manager, &format!("assistant reply {index}"));
        }
    });
    let (factory, handle) = compact_payload_factory(&temp).await;
    assert!(
        handle.has_handlers("session_compact"),
        "the pi.on(\"session_compact\") handler must be registered"
    );
    // Sanity: the same handle dispatches the event and the probe sees it.
    let mut direct = json!({
        "type": "session_compact",
        "compaction": { "summary": "DIRECT_PROBE" },
    });
    handle
        .emit("session_compact", &mut direct)
        .await
        .expect("direct dispatch");
    let direct_probe = handle
        .eval_json("JSON.stringify(globalThis.__seenCompact ?? null)")
        .await
        .expect("probe direct dispatch");
    // `eval_json` parses the `JSON.stringify` result, so the probe arrives as
    // an object — not as raw JSON text.
    assert_eq!(
        direct_probe["compaction"]["summary"],
        json!("DIRECT_PROBE"),
        "direct dispatch must reach the handler: {direct_probe}"
    );
    let (mut client, _server_temp) = rpc_pair_with("compact-payload", factory).await;
    attach_path(&mut client, &path).await;

    client.send(json!({ "id": "compact-p", "type": "compact" }));
    let response = client
        .wait_for(|value| value["command"] == "compact" && value["id"] == "compact-p")
        .await;
    assert_eq!(response["success"], json!(true), "{response}");

    let seen = handle
        .eval_json("JSON.stringify(globalThis.__seenCompact ?? null)")
        .await
        .expect("probe the captured session_compact event");
    assert_eq!(seen["type"], json!("session_compact"), "{seen}");
    let compaction = &seen["compaction"];
    assert!(
        compaction["summary"]
            .as_str()
            .unwrap_or_default()
            .contains("COMPACTED SUMMARY"),
        "the full result rides the event: {seen}"
    );
    assert!(compaction["usage"].is_object(), "{seen}");
    assert!(compaction["details"]["readFiles"].is_array(), "{seen}");
    assert!(compaction["estimatedTokensAfter"].is_number(), "{seen}");
    assert!(compaction.get("firstKeptEntryId").is_some(), "{seen}");

    drop(client);
    drop(_server_temp);
    drop(temp);
}
