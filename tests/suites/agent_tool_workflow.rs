//! Deterministic provider-backed agent tool workflow coverage.

use pir::agent::agent_loop::{run, LoopDeps};
use pir::agent::tool_execution::ToolRegistry;
use pir::agent::tool_scheduler::{ToolScheduler, ToolSchedulerConfig};
use pir::ai::api::openai::OpenAiCompatibleProvider;
use pir::ai::provider::ProviderRegistry;
use pir::ai::transport::{ProviderRequestClient, ProviderRequestConfig};
use pir::ai::types::*;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn model(base_url: &str) -> Model {
    Model {
        id: "workflow-model".into(),
        name: "Workflow model".into(),
        api: "openai-chat".into(),
        provider: "mock-workflow".into(),
        base_url: base_url.into(),
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
        context_window: 16_384,
        max_tokens: 1_024,
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

fn sse(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn response_for(request_number: usize) -> String {
    let body = match request_number {
        0 => concat!(
            "data: {\"id\":\"resp-1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call-write\",\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"workflow.txt\\\",\\\"content\\\":\\\"WORKFLOW_FILE\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
            "data: [DONE]\n\n"
        ),
        1 => concat!(
            "data: {\"id\":\"resp-2\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call-bash\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"cat workflow.txt; printf WORKFLOW_OK\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n"
        ),
        _ => concat!(
            "data: {\"id\":\"resp-3\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"workflow complete\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        ),
    };
    sse(body)
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0_u8; 1024];
        let count = socket.read(&mut chunk).await.unwrap();
        if count == 0 {
            return bytes;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let length = headers
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length:")
                .or_else(|| line.strip_prefix("Content-Length:"))
        })
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let mut chunk = [0_u8; 1024];
        let count = socket.read(&mut chunk).await.unwrap();
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    bytes[header_end..header_end + length].to_vec()
}

fn text_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
        sections: None,
    })
}

#[tokio::test]
async fn provider_registry_runs_real_multi_turn_tool_workflow() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let requests_for_server = requests.clone();
    let server = tokio::spawn(async move {
        for request_number in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let body = read_http_request(&mut socket).await;
            requests_for_server
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&body).unwrap());
            socket
                .write_all(response_for(request_number).as_bytes())
                .await
                .unwrap();
        }
    });

    let root = std::env::temp_dir().join(format!(
        "pir-agent-workflow-{}-{}",
        std::process::id(),
        pir::ai::util::now_ms()
    ));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let workflow_model = model(&base_url);
    let client = ProviderRequestClient::new(ProviderRequestConfig {
        timeout: std::time::Duration::from_secs(5),
        max_response_bytes: 1024 * 1024,
        max_retries: 0,
        max_retry_delay: std::time::Duration::from_millis(1),
        proxy_url: None,
        http_idle_timeout: None,
    })
    .unwrap();
    let provider =
        OpenAiCompatibleProvider::new("mock-workflow", &base_url, vec![workflow_model.clone()])
            .unwrap()
            .with_client(client);
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(provider));

    let mut tools = ToolRegistry::new();
    pir::coding_agent::tools::register_core_tools(&mut tools);
    let definitions = pir::coding_agent::tools::tool_definitions(&tools);
    let context = Context {
        messages: vec![text_message("complete the workflow")],
        tools: Some(definitions.clone()),
        ..Context::default()
    };
    let deps = LoopDeps {
        tools: Arc::new(tools),
        scheduler: ToolScheduler::new(ToolSchedulerConfig {
            mode: pir::agent::tool_batch::ToolExecutionMode::Sequential,
            ..ToolSchedulerConfig::default()
        }),
        cwd: root.clone(),
        ..LoopDeps::default()
    }
    .with_provider_unchecked(Arc::new(registry), workflow_model, StreamOptions::default());

    let result = run(deps, context, None).await;
    server.await.unwrap();

    assert_eq!(result.turns, 3);
    assert_eq!(result.tool_results.len(), 2);
    assert_eq!(result.terminal.stop_reason, StopReason::Stop);
    assert_eq!(
        result.terminal.content,
        vec![ContentBlock::Text(TextContent {
            text: "workflow complete".into(),
            text_signature: None,
        })]
    );
    assert_eq!(
        tokio::fs::read_to_string(root.join("workflow.txt"))
            .await
            .unwrap(),
        "WORKFLOW_FILE"
    );
    assert_eq!(result.tool_results[0].tool_name, "write");
    assert_eq!(result.tool_results[1].tool_name, "bash");
    assert!(result.tool_results[1].content.iter().any(
        |block| matches!(block, ContentBlock::Text(text) if text.text.contains("WORKFLOW_OK"))
    ));

    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        let schema = requests[0]["tools"].as_array().unwrap();
        // Core tools: read/bash/edit/write/grep/find/ls + powershell, plus the
        // two model-facing background-call controls (background_status /
        // background_abort) registered by `register_core_tools`.
        let mut names: Vec<&str> = schema
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "background_abort",
                "background_status",
                "bash",
                "edit",
                "find",
                "grep",
                "ls",
                "powershell",
                "read",
                "write",
            ]
        );
        assert!(schema
            .iter()
            .any(|tool| tool["function"]["name"] == "write"));
        assert!(schema.iter().any(|tool| tool["function"]["name"] == "bash"));
        let second_messages = requests[1]["messages"].as_array().unwrap();
        assert!(second_messages
            .iter()
            .any(|message| message["role"] == "assistant" && message["tool_calls"].is_array()));
        assert!(second_messages
            .iter()
            .any(|message| message["role"] == "tool" && message["tool_call_id"] == "call-write"));
        let third_messages = requests[2]["messages"].as_array().unwrap();
        assert!(third_messages
            .iter()
            .any(|message| message["role"] == "tool" && message["tool_call_id"] == "call-bash"));
    }
    assert!(result.context.messages.iter().any(|message| matches!(message, Message::Assistant(assistant) if assistant.content.iter().any(|block| matches!(block, ContentBlock::ToolCall(call) if call.id == "call-write")))));
    assert!(result.context.messages.iter().any(|message| matches!(message, Message::ToolResult(result) if result.tool_call_id == "call-bash")));

    let _ = tokio::fs::remove_dir_all(root).await;
}
