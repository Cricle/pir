//! `export_html` without the `export-html` feature: an `rpc`-only library
//! build must keep the command on the wire and answer the documented
//! missing-feature error BEFORE `attached()`. Driven in-process through
//! `RpcServer::serve` over a tokio duplex (the same harness shape as
//! `tests/rpc_bash_record_fix.rs`). The session factory panics if it is ever
//! reached, which proves the off-arm answers without building a session (and
//! therefore without a provider/model).

#![cfg(all(feature = "rpc", not(feature = "export-html")))]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use pir::coding_agent::rpc::{RpcServer, SessionFactory};

#[tokio::test]
async fn export_html_reports_missing_feature_before_attaching() {
    let factory: SessionFactory =
        Arc::new(|_| panic!("the export_html off-arm must answer before a session is built"));
    let server = RpcServer::new(PathBuf::from("."), factory);
    let (client, server_stream) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_stream);
    let serve = tokio::spawn(async move { server.serve(server_read, server_write).await });

    let (client_read, mut client_write) = tokio::io::split(client);
    client_write
        .write_all(b"{\"id\":\"export-off\",\"type\":\"export_html\"}\n")
        .await
        .expect("write command");
    client_write.flush().await.expect("flush command");

    let mut lines = tokio::io::BufReader::new(client_read).lines();
    let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
        .await
        .expect("response must not time out")
        .expect("read must succeed")
        .expect("one protocol line");
    let response: Value = serde_json::from_str(&line).expect("response is JSON");
    assert_eq!(response["type"], json!("response"));
    assert_eq!(response["command"], json!("export_html"));
    assert_eq!(response["id"], json!("export-off"));
    assert_eq!(response["success"], json!(false));
    assert_eq!(
        response["error"],
        json!(
            "`export_html` is unavailable: this build was built without the `export-html` feature"
        )
    );

    // EOF: dropping both client halves closes the duplex, so `serve` returns.
    drop(lines);
    drop(client_write);
    tokio::time::timeout(Duration::from_secs(30), serve)
        .await
        .expect("serve must end on EOF")
        .expect("serve task must not panic")
        .expect("serve must end cleanly");
}
