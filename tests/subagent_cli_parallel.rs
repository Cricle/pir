//! End-to-end CLI regression: the subagent builtin's parallel mode runs two
//! child sessions through the real `pir print` binary and a mock provider.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn chunk(delta: serde_json::Value, finish: Option<&str>) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"mock-model",
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    )
}

#[test]
fn cli_parallel() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let thread_bodies = Arc::clone(&bodies);
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
            thread_bodies.lock().unwrap().push(body.clone());
            eprintln!("CALL body_snippet={}", &body[..body.len().min(500)]);
            // Inspect the conversation, not the raw body: the subagent tool
            // schema itself contains the word "tasks", so a body-wide search
            // makes the parent re-dispatch forever.
            let json: serde_json::Value = serde_json::from_str(&body).expect("request json");
            let last_role = json["messages"]
                .as_array()
                .and_then(|messages| messages.last())
                .and_then(|message| message["role"].as_str())
                .unwrap_or("");
            let last_text = json["messages"]
                .as_array()
                .and_then(|messages| messages.last())
                .and_then(|message| message["content"].as_str())
                .unwrap_or("");
            let events = if last_role == "tool" {
                // The parent already received both child results.
                let mut s = chunk(
                    serde_json::json!({"role":"assistant","content":"PARENT_DONE"}),
                    None,
                );
                s += &chunk(serde_json::json!({}), Some("stop"));
                s
            } else if last_text == "A" {
                let mut s = chunk(
                    serde_json::json!({"role":"assistant","content":"OUT:A"}),
                    None,
                );
                s += &chunk(serde_json::json!({}), Some("stop"));
                s
            } else if last_text == "B" {
                let mut s = chunk(
                    serde_json::json!({"role":"assistant","content":"OUT:B"}),
                    None,
                );
                s += &chunk(serde_json::json!({}), Some("stop"));
                s
            } else {
                let args = serde_json::json!({"tasks":[
                    {"agent":"scout","task":"A"},
                    {"agent":"scout","task":"B"}
                ]})
                .to_string();
                let mut s = chunk(
                    serde_json::json!({"role":"assistant","tool_calls":[{
                        "index":0,"id":"call_1","type":"function",
                        "function":{"name":"subagent","arguments":args}}]}),
                    None,
                );
                s += &chunk(serde_json::json!({}), Some("tool_calls"));
                s
            } + "data: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                events.len(), events
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    let dir: PathBuf = std::env::temp_dir().join(format!("pir-zz-cli-par-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("agents")).unwrap();
    std::fs::write(
        dir.join("agents").join("scout.md"),
        "---\nname: scout\ndescription: d\n---\nbody",
    )
    .unwrap();
    std::fs::write(
        dir.join("models.json"),
        format!(
            r#"{{"providers":{{"mock":{{"name":"Mock","baseUrl":"http://127.0.0.1:{port}/v1","api":"openai-completions","apiKey":"k","models":[{{"id":"mock-model","name":"M","api":"openai-completions","baseUrl":"http://127.0.0.1:{port}/v1","reasoning":false,"input":["text"],"contextWindow":100000,"maxTokens":1024}}]}}}}}}"#
        ),
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pir"))
        .current_dir(&dir)
        .env("PIR_AGENT_DIR", &dir)
        .env("HOME", &dir)
        .env("USERPROFILE", &dir)
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
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
        .unwrap();
    eprintln!(
        "STATUS {:?}\nSTDOUT:\n{}\nSTDERR:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "exit {:?}", output.status);
    // The child outputs live in the parent's tool-result message, not in the
    // parent's final answer; assert the model actually received both.
    let saw_both = bodies.lock().unwrap().iter().any(|body| {
        body.contains("OUT:A") && body.contains("OUT:B") && body.contains("\"role\":\"tool\"")
    });
    assert!(saw_both, "parent never received both child results");
    let _ = std::fs::remove_dir_all(&dir);
}
