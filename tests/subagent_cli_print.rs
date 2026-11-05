//! End-to-end CLI regression: `pir print --tools subagent` calls the native
//! subagent builtin against a mock OpenAI-compatible endpoint and executes the
//! child session through the real binary.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn chunk(delta: serde_json::Value, finish: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "chatcmpl-1",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "mock-model",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        })
    )
}

fn spawn_mock() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls2 = calls.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
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
            let n = calls2.fetch_add(1, Ordering::SeqCst);
            eprintln!("MOCK CALL {} body={}", n + 1, &body[..body.len().min(1200)]);
            let events = if n == 0 {
                let args = serde_json::json!({"agent": "scout", "task": "delegate me"}).to_string();
                let mut s = chunk(
                    serde_json::json!({"role": "assistant", "tool_calls": [{
                        "index": 0, "id": "call_1", "type": "function",
                        "function": {"name": "subagent", "arguments": args}
                    }]}),
                    None,
                );
                s += &chunk(serde_json::json!({}), Some("tool_calls"));
                s
            } else {
                let text = if n == 1 {
                    "CHILD_VIA_CLI"
                } else {
                    "PARENT_DONE"
                };
                let mut s = chunk(
                    serde_json::json!({"role": "assistant", "content": text}),
                    None,
                );
                s += &chunk(serde_json::json!({}), Some("stop"));
                s
            } + "data: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                events.len(),
                events
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (port, calls)
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-zz-cli-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    dir
}

fn write_models(agent: &Path, port: u16) {
    std::fs::write(
        agent.join("models.json"),
        format!(
            r#"{{"providers":{{"mock":{{"name":"Mock","baseUrl":"http://127.0.0.1:{port}/v1","api":"openai-completions","apiKey":"test-key","models":[{{"id":"mock-model","name":"Mock","api":"openai-completions","baseUrl":"http://127.0.0.1:{port}/v1","reasoning":false,"input":["text"],"contextWindow":100000,"maxTokens":1024}}]}}}}}}"#
        ),
    )
    .unwrap();
}

#[test]
fn cli_print_runs_subagent_with_tools_flag() {
    let (port, calls) = spawn_mock();
    let dir = temp_dir("print");
    write_models(&dir, port);
    std::fs::write(
        dir.join("agents").join("scout.md"),
        "---\nname: scout\ndescription: d\n---\nbody",
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pir"))
        .current_dir(&dir)
        .env("PIR_AGENT_DIR", &dir)
        .env("HOME", &dir)
        .env("USERPROFILE", &dir)
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env_remove("PIR_TRUST_PROJECT")
        .args([
            "print",
            "kick off",
            "--provider",
            "mock",
            "--model",
            "mock-model",
            "--offline",
            "--no-session",
            "--no-context-files",
            "--tools",
            "subagent",
        ])
        .output()
        .expect("run pir");
    eprintln!(
        "STATUS {:?}\nSTDOUT:\n{}\nSTDERR:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "exit: {:?}", output.status);
    assert!(stdout.contains("PARENT_DONE"), "stdout: {stdout}");
    assert!(
        calls.load(Ordering::SeqCst) >= 3,
        "calls: {}",
        calls.load(Ordering::SeqCst)
    );
    let _ = std::fs::remove_dir_all(&dir);
}
