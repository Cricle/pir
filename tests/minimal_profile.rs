//! End-to-end regression for the zero-optional-feature profile: the
//! `pir::minimal` agent must build from core modules and drive one real
//! OpenAI-compatible `/chat/completions` request against a local mock server.
//!
//! Run with no features at all:
//!
//! ```text
//! cargo test --no-default-features --test minimal_profile
//! ```

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use pir::ai::types::ThinkingLevel;
use pir::minimal::{message_text, MinimalAgent, MinimalConfig};

#[test]
fn minimal_agent_runs_against_openai_compatible_endpoint() {
    // The agent-session build is deep enough to overflow a small test thread
    // stack in debug builds (same reason `src/main.rs` uses a big-stack
    // worker); give this regression the same headroom.
    let worker = std::thread::Builder::new()
        .name("minimal-profile-test".to_owned())
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            runtime.block_on(run_case());
        })
        .expect("spawn test worker");
    worker.join().expect("test worker panicked");
}

async fn run_case() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let address = listener.local_addr().expect("mock address");

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let read = socket.read(&mut chunk).await.expect("read request");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            if let Some(headers_end) = find_headers_end(&request) {
                let headers = String::from_utf8_lossy(&request[..headers_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(str::trim)
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                if request.len() >= headers_end + 4 + content_length {
                    break;
                }
            }
        }

        let body = concat!(
            "data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",",
            "\"created\":1,\"model\":\"mock\",\"choices\":[{\"index\":0,",
            "\"delta\":{\"role\":\"assistant\",\"content\":\"Hello from the minimal agent.\"},",
            "\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",",
            "\"created\":1,\"model\":\"mock\",\"choices\":[{\"index\":0,\"delta\":{},",
            "\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,",
            "\"completion_tokens\":6,\"total_tokens\":9}}\n\n",
            "data: [DONE]\n\n",
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write response");
        socket.flush().await.expect("flush response");
    });

    let config = MinimalConfig {
        base_url: format!("http://{address}/v1"),
        api_key: Some("test-key".to_owned()),
        model: "mock-model".to_owned(),
        system_prompt: "You are a test agent.".to_owned(),
        max_turns: 4,
        cwd: std::env::current_dir().expect("cwd"),
        thinking: ThinkingLevel::Off,
    };
    let agent = MinimalAgent::build(config).await.expect("build agent");
    assert_eq!(agent.model().id, "mock-model");
    let message = agent.prompt("Say hello").await.expect("prompt");
    assert_eq!(message_text(&message), "Hello from the minimal agent.");
    agent.shutdown().await;
    server.await.expect("mock server");
}

fn find_headers_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}
