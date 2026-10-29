//! P1 regression: the MCP streamable-HTTP client bounds plain-JSON response
//! bodies.
//!
//! rmcp 3.5's reqwest backend reads a non-SSE POST response with
//! `response.bytes()` (unbounded). `pir::mcp::http::BoundedHttpClient` checks
//! the declared `Content-Length` before reading and then applies a streaming
//! cap, recording the violation in the connection's shared cap state so the
//! client request path reports `McpError::MessageTooLarge`.
//!
//! Each test runs a one-shot loopback HTTP server and drives the client
//! directly (`post_message_with_max_sse_event_size`), so no MCP handshake is
//! needed.

use std::collections::HashMap;

use rmcp::model::{ClientJsonRpcMessage, ClientRequest, PingRequest, RequestId};
use rmcp::transport::streamable_http_client::{StreamableHttpClient, StreamableHttpPostResponse};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use pir::mcp::http::bounded_http_client_for_tests;

const CAP: usize = 1024 * 1024;

/// Serve one HTTP request and write `response_head` + `body` back.
async fn serve_once(response_head: Vec<u8>, body: Vec<u8>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let address = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = socket.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
            let Some(head_end) = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|position| position + 4)
            else {
                continue;
            };
            let head = String::from_utf8_lossy(&request[..head_end]).to_ascii_lowercase();
            let content_length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while request.len() < head_end + content_length {
                let read = socket.read(&mut chunk).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            break;
        }
        let _ = socket.write_all(&response_head).await;
        let _ = socket.write_all(&body).await;
        let _ = socket.shutdown().await;
    });
    format!("http://{address}/mcp")
}

fn ping_request() -> ClientJsonRpcMessage {
    ClientJsonRpcMessage::request(
        ClientRequest::PingRequest(PingRequest::default()),
        RequestId::Number(1),
    )
}

#[tokio::test]
async fn declared_content_length_over_cap_fails_before_reading() {
    let uri = serve_once(
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2097152\r\n\r\n"
            .to_vec(),
        Vec::new(),
    )
    .await;
    let (client, cap) = bounded_http_client_for_tests(CAP);
    let result = client
        .post_message_with_max_sse_event_size(
            uri.into(),
            ping_request(),
            None,
            None,
            HashMap::new(),
            CAP,
        )
        .await;
    let message = match result {
        Err(
            rmcp::transport::streamable_http_client::StreamableHttpError::UnexpectedServerResponse(
                message,
            ),
        ) => message.into_owned(),
        other => panic!("expected an oversized-body error, got {other:?}"),
    };
    assert!(
        message.contains("Content-Length") && message.contains("cap"),
        "unexpected error: {message}"
    );
    assert!(cap.overflowed(), "the cap state must record the violation");
}

#[tokio::test]
async fn chunked_body_over_cap_stops_at_the_streaming_limit() {
    let mut body = Vec::new();
    let chunk = vec![b'x'; 64 * 1024];
    for _ in 0..20 {
        body.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        body.extend_from_slice(&chunk);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"0\r\n\r\n");
    let uri = serve_once(
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\n\r\n"
            .to_vec(),
        body,
    )
    .await;
    let (client, cap) = bounded_http_client_for_tests(CAP);
    let result = client
        .post_message_with_max_sse_event_size(
            uri.into(),
            ping_request(),
            None,
            None,
            HashMap::new(),
            CAP,
        )
        .await;
    assert!(
        matches!(
            result,
            Err(rmcp::transport::streamable_http_client::StreamableHttpError::UnexpectedServerResponse(_))
        ),
        "expected an oversized-body error, got {result:?}"
    );
    assert!(cap.overflowed(), "the cap state must record the violation");
}

#[tokio::test]
async fn body_at_or_below_the_cap_still_parses() {
    let response = br#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        response.len()
    )
    .into_bytes();
    let uri = serve_once(head, response.to_vec()).await;
    let (client, cap) = bounded_http_client_for_tests(CAP);
    let result = client
        .post_message_with_max_sse_event_size(
            uri.into(),
            ping_request(),
            None,
            None,
            HashMap::new(),
            CAP,
        )
        .await;
    assert!(
        matches!(result, Ok(StreamableHttpPostResponse::Json(_, _))),
        "expected a parsed JSON response, got {result:?}"
    );
    assert!(!cap.overflowed());
}
