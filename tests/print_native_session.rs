//! Print-host integration regressions for the builtin plugin surfaces:
//! native slash commands (PIR-BIN-001) and transcript replay on `--continue`
//! (PIR-BIN-002).
//!
//! Both tests drive the REAL `pir print` binary against a loopback
//! OpenAI-compatible mock provider, so the assertions cover the whole host:
//! CLI routing, plugin host dispatch, builtin `todo` state, session JSONL
//! persistence and replay.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
enum Step {
    Text(&'static str),
    Tool(&'static str, &'static str),
}

fn sse_chunk(delta: serde_json::Value, finish: serde_json::Value) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "mock-model",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        })
    )
}

/// Serve `script` steps in order (the last one repeats) over SSE.
fn spawn_scripted_mock(script: Vec<Step>) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let bodies2 = Arc::clone(&bodies);
    std::thread::spawn(move || {
        let mut cursor = 0usize;
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut request = Vec::new();
            let mut buf = [0u8; 8192];
            let header_end = loop {
                if let Some(i) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(i + 4);
                }
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break None,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            };
            let Some(header_end) = header_end else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
            let len = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while request.len() < header_end + len {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let body = String::from_utf8_lossy(&request[header_end..]).to_string();
            bodies2.lock().unwrap().push(body);

            let step = script
                .get(cursor.min(script.len().saturating_sub(1)))
                .cloned();
            cursor += 1;
            let events = match step {
                Some(Step::Tool(name, arguments)) => {
                    let mut events = String::new();
                    events.push_str(&sse_chunk(
                        serde_json::json!({"role": "assistant", "content": ""}),
                        serde_json::Value::Null,
                    ));
                    events.push_str(&sse_chunk(
                        serde_json::json!({
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_mock_0",
                                "type": "function",
                                "function": {"name": name, "arguments": arguments},
                            }],
                        }),
                        serde_json::Value::Null,
                    ));
                    let mut final_chunk: serde_json::Value = serde_json::from_str(
                        sse_chunk(serde_json::json!({}), serde_json::json!("tool_calls"))
                            .trim_start_matches("data: ")
                            .trim_end(),
                    )
                    .unwrap();
                    final_chunk["usage"] = serde_json::json!({
                        "prompt_tokens": 100, "completion_tokens": 5, "total_tokens": 105,
                    });
                    events.push_str(&format!("data: {final_chunk}\n\n"));
                    events.push_str("data: [DONE]\n\n");
                    events
                }
                Some(Step::Text(text)) => {
                    let mut events = String::new();
                    events.push_str(&sse_chunk(
                        serde_json::json!({"role": "assistant", "content": ""}),
                        serde_json::Value::Null,
                    ));
                    events.push_str(&sse_chunk(
                        serde_json::json!({"content": text}),
                        serde_json::Value::Null,
                    ));
                    let mut final_chunk: serde_json::Value = serde_json::from_str(
                        sse_chunk(serde_json::json!({}), serde_json::json!("stop"))
                            .trim_start_matches("data: ")
                            .trim_end(),
                    )
                    .unwrap();
                    final_chunk["usage"] = serde_json::json!({
                        "prompt_tokens": 100, "completion_tokens": 5, "total_tokens": 105,
                    });
                    events.push_str(&format!("data: {final_chunk}\n\n"));
                    events.push_str("data: [DONE]\n\n");
                    events
                }
                None => "data: [DONE]\n\n".to_owned(),
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                events.len(),
                events
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (port, bodies)
}

fn write_agent(dir: &Path, port: u16) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("models.json"),
        format!(
            r#"{{"providers":{{"mock":{{"name":"Mock","baseUrl":"http://127.0.0.1:{port}/v1","api":"openai-completions","apiKey":"k","models":[{{"id":"mock-model","name":"M","api":"openai-completions","baseUrl":"http://127.0.0.1:{port}/v1","reasoning":false,"input":["text"],"contextWindow":100000,"maxTokens":1024}}]}}}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(dir.join("settings.json"), "{}").unwrap();
}

fn temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-print-native-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_pir(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_pir"))
        .current_dir(dir)
        .env("PIR_AGENT_DIR", dir)
        .env("PIR_SESSION_DIR", dir.join("sessions"))
        .env("HOME", dir)
        .env("USERPROFILE", dir)
        .env("PI_OFFLINE", "1")
        .env("PIR_SKIP_VERSION_CHECK", "1")
        .env("PIR_TRUST_PROJECT", "1")
        .env("NO_COLOR", "1")
        .args(args)
        .output()
        .expect("run pir")
}

/// PIR-BIN-001: a leading native builtin command (`/todos`) must be
/// dispatched through the plugin host and consume the turn — zero provider
/// requests — instead of being sent to the model as a prompt.
#[test]
fn print_dispatches_native_todos_command_without_a_model_request() {
    let (port, bodies) = spawn_scripted_mock(vec![Step::Text("MODEL-CALLED")]);
    let dir = temp_dir("todos");
    write_agent(&dir, port);
    let output = run_pir(
        &dir,
        &[
            "print",
            "/todos",
            "--mode",
            "text",
            "--only-builtin",
            "-e",
            "builtin:todo",
            "--no-context-files",
            "--provider",
            "mock",
            "--model",
            "mock-model",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "print /todos failed: exit={:?} stderr={stderr}",
        output.status.code()
    );
    let request_count = bodies.lock().unwrap().len();
    assert!(
        request_count == 0,
        "the native command must consume the turn (zero model requests), got {request_count}"
    );
    assert!(
        stderr.contains("[ext] command:todos: /todos requires interactive mode"),
        "native command output missing from stderr: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// PIR-BIN-002: `print --continue` must replay the persisted transcript into
/// the plugin host's session-context snapshot, so the `todo` builtin rebuilds
/// its slot and `todo list` sees the previous run's tasks.
#[test]
fn print_continue_replays_todos_into_the_plugin_host() {
    let (port, _bodies) = spawn_scripted_mock(vec![
        Step::Tool("todo", r#"{"action":"create","subject":"Alpha"}"#),
        Step::Tool("todo", r#"{"action":"create","subject":"Beta"}"#),
        Step::Tool("todo", r#"{"action":"create","subject":"Gamma"}"#),
        Step::Text("Created three tasks."),
        Step::Tool("todo", r#"{"action":"list"}"#),
        Step::Text("LISTED"),
    ]);
    let dir = temp_dir("replay");
    write_agent(&dir, port);
    let base_args = [
        "print",
        "--mode",
        "json",
        "--only-builtin",
        "-e",
        "builtin:todo",
        "--no-context-files",
        "--provider",
        "mock",
        "--model",
        "mock-model",
    ];
    let mut first = vec![];
    first.extend_from_slice(&base_args);
    first.push("Create three tasks: Alpha, Beta, Gamma.");
    let run1 = run_pir(&dir, &first);
    assert!(
        run1.status.success(),
        "first print run failed: {}",
        String::from_utf8_lossy(&run1.stderr)
    );
    let mut second = vec![];
    second.extend_from_slice(&base_args);
    second.push("--continue");
    second.push("List todo state.");
    let run2 = run_pir(&dir, &second);
    let stdout = String::from_utf8_lossy(&run2.stdout);
    assert!(
        run2.status.success(),
        "second print run failed: {}\nstdout={stdout}",
        String::from_utf8_lossy(&run2.stderr)
    );
    let todo_result_line = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .flat_map(|event| {
            event
                .get("messages")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .find(|message| {
            message.get("role").and_then(serde_json::Value::as_str) == Some("toolResult")
                && message.get("toolName").and_then(serde_json::Value::as_str) == Some("todo")
        })
        .expect("the second run must contain a todo tool result");
    let tasks = todo_result_line
        .pointer("/details/tasks")
        .and_then(serde_json::Value::as_array)
        .expect("todo result details.tasks");
    let subjects: Vec<&str> = tasks
        .iter()
        .filter_map(|task| task.get("subject").and_then(serde_json::Value::as_str))
        .collect();
    assert_eq!(
        subjects,
        vec!["Alpha", "Beta", "Gamma"],
        "the resumed print run must replay the first run's tasks: {todo_result_line}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
