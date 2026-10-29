//! Small mock MCP stdio server used by the integration tests.
//!
//! The tests used to re-execute their own ~330 MB test binary as the mock
//! child (`current_exe()` + `--exact <child test>`), which made every MCP
//! integration test pay a 338 MB image load. This binary is built separately
//! (via a `[[bin]]` entry in Cargo.toml) and links only `serde_json`, so the
//! mock child is an order of magnitude smaller.
//!
//! It is also compiled as a module by `tests/mcp_integration.rs` (so the old
//! in-process child entries and the bin share one implementation). Keep it
//! self-contained: std + serde_json only, no `pir`/`crate::` imports.
//!
//! Modes (selected by env, exactly the switches the tests already set):
//! - `PIR_MOCK_MCP_SERVER=1`  — the standard mock (mcp_integration)
//! - `PIR_MOCK_MCP_RAW=1`     — garbage-then-good stdout, then standard mock
//! - `PIR_MOCK_MCP_SILENT=1`  — never answers, holds stdin until EOF
//! - `PIR_W13_MOCK=<mode>`    — the W13-B mock used by g_agent's
//!   `mcp_residuals_w13` / `mcp_prune_w18` / `mcp_toggle_refresh_w18`
//!
//! `main` is dead code when this file is compiled as a module; the tests call
//! the `serve_*` functions directly from their child-entry tests.

use std::io::{BufRead, Read, Write};

use serde_json::{json, Value};

fn main() {
    if std::env::var("PIR_W13_MOCK").is_ok() {
        let mode = std::env::var("PIR_W13_MOCK").unwrap_or_default();
        if let Ok(path) = std::env::var("PIR_W13_PID_FILE") {
            let _ = std::fs::write(path, std::process::id().to_string());
        }
        let delay_ms: u64 = std::env::var("PIR_W13_DELAY_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let init_delay_ms: u64 = std::env::var("PIR_W13_INIT_DELAY_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        w13::serve_mock(&mode, delay_ms, init_delay_ms);
        return;
    }
    if std::env::var("PIR_MOCK_MCP_SILENT").as_deref() == Ok("1") {
        serve_silent_child();
        return;
    }
    if std::env::var("PIR_MOCK_MCP_RAW").as_deref() == Ok("1") {
        serve_raw_child();
        return;
    }
    if std::env::var("PIR_MOCK_MCP_SERVER").as_deref() == Ok("1") {
        serve_mock_requests();
        return;
    }
    // No mode: a test binary compiled from this file would just exit. The
    // `[[bin]]` target is only spawned with one of the switches above.
    std::process::exit(2);
}

// ---------------------------------------------------------------------------
// mcp_integration mock (the old `serve_mock_requests`)
// ---------------------------------------------------------------------------

/// The request loop shared by the mock children: answers the MCP methods
/// over line-delimited JSON-RPC on stdin/stdout. Non-JSON lines (the libtest
/// banner, garbage) are skipped by the rmcp codec.
pub fn serve_mock_requests() {
    let stdin = std::io::stdin();
    // Optional behaviors driven by env (test scenarios):
    let delay_ms: u64 = std::env::var("PIR_MOCK_DELAY_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let protocol_version =
        std::env::var("PIR_MOCK_PROTOCOL_VERSION").unwrap_or_else(|_| "2025-06-18".to_owned());
    let paged = std::env::var("PIR_MOCK_PAGED").ok();
    let notify_tools_list_changed = std::env::var("PIR_MOCK_NOTIFY_TOOLS").as_deref() == Ok("1");
    let no_resource_name = std::env::var("PIR_MOCK_NO_RESOURCE_NAME").as_deref() == Ok("1");
    let list_counter = std::env::var("PIR_MOCK_LIST_COUNTER").ok();
    if let Ok(trigger) = std::env::var("PIR_MOCK_NOTIFY_FILE") {
        let sent = std::env::var("PIR_MOCK_NOTIFY_SENT").ok();
        std::thread::spawn(move || {
            while !std::path::Path::new(&trigger).exists() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let _ = std::fs::remove_file(&trigger);
            write_mock_line(
                &std::io::stdout(),
                &json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }),
            );
            if let Some(path) = sent {
                let _ = std::fs::write(path, b"sent");
            }
        });
    }
    let stderr_mode = std::env::var("PIR_MOCK_STDERR").as_deref() == Ok("1");
    if stderr_mode {
        let _ = emit_mock_stderr("mock-stderr:startup");
    }
    let stall_after_init = std::env::var("PIR_MOCK_STALL_AFTER_INIT").as_deref() == Ok("1");
    let mut requests = 0usize;
    let probe_enabled = std::env::var("PIR_MOCK_PROBE_METHOD").is_ok();
    let progress_enabled = std::env::var("PIR_MOCK_PROGRESS_MS").is_ok()
        || std::env::var("PIR_MOCK_PROGRESS_COUNT").is_ok();
    let mut reader = stdin.lock();
    let mut line = String::new();
    loop {
        line.clear();
        let Ok(read) = reader.read_line(&mut line) else {
            break;
        };
        if read == 0 {
            break;
        }
        requests += 1;
        if requests > 10_000 {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = value.get("id").cloned() else {
            continue; // notification: nothing to answer
        };
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let delayed = delay_ms > 0
            && method == "tools/call"
            && params.get("name").and_then(Value::as_str) == Some("slow");
        let reply = match method.as_str() {
            "initialize" => json!({
                "protocolVersion": protocol_version,
                "capabilities": { "tools": {}, "resources": {}, "prompts": {} },
                "serverInfo": { "name": "mock", "version": "1.0.0" },
                "instructions": "Mock server instructions",
            }),
            "ping" => json!({}),
            "tools/list" => {
                if let Some(path) = &list_counter {
                    if let Ok(mut file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                    {
                        let _ = writeln!(file, "tools/list");
                    }
                }
                if notify_tools_list_changed {
                    write_mock_line(
                        &std::io::stdout(),
                        &json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }),
                    );
                }
                if let Some(mode) = &paged {
                    let next = match mode.as_str() {
                        "dup" => Some("dup".to_owned()),
                        "cap" => {
                            let page = params
                                .get("cursor")
                                .and_then(Value::as_str)
                                .and_then(|cursor| cursor.strip_prefix('p'))
                                .and_then(|number| number.parse::<u64>().ok())
                                .unwrap_or(0);
                            Some(format!("p{}", page + 1))
                        }
                        _ => None,
                    };
                    let mut page = json!({ "tools": [{ "name": "paged", "inputSchema": {} }] });
                    if let Some(next) = next {
                        page["nextCursor"] = json!(next);
                    }
                    page
                } else {
                    let mut tools = vec![
                        json!({
                            "name": "echo",
                            "description": "Echo the input text",
                            "inputSchema": {
                                "type": "object",
                                "properties": { "text": { "type": "string" } },
                                "required": ["text"],
                            },
                        }),
                        json!({
                            "name": "fail",
                            "description": "Always errors",
                            "inputSchema": { "type": "object" },
                        }),
                        json!({
                            "name": "slow",
                            "description": "Echoes after a delay",
                            "inputSchema": { "type": "object" },
                        }),
                    ];
                    if probe_enabled {
                        tools.push(json!({
                            "name": "probe",
                            "description": "Asks the client a server-initiated request",
                            "inputSchema": { "type": "object" },
                        }));
                    }
                    if progress_enabled {
                        tools.push(json!({
                            "name": "progress",
                            "description": "Reports progress before answering",
                            "inputSchema": { "type": "object" },
                        }));
                    }
                    if stderr_mode {
                        tools.push(json!({
                            "name": "noisy",
                            "description": "Writes stderr, then answers",
                            "inputSchema": { "type": "object" },
                        }));
                    }
                    json!({ "tools": tools })
                }
            }
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                if name == "fail" {
                    json!({ "content": [{ "type": "text", "text": "boom" }], "isError": true })
                } else if name == "probe" {
                    let method = std::env::var("PIR_MOCK_PROBE_METHOD")
                        .unwrap_or_else(|_| "roots/list".to_owned());
                    let probe_params = std::env::var("PIR_MOCK_PROBE_PARAMS")
                        .ok()
                        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
                    let answer = probe_client(&mut reader, &method, probe_params);
                    json!({
                        "content": [{ "type": "text", "text": answer.to_string() }],
                    })
                } else if name == "progress" {
                    let interval: u64 = std::env::var("PIR_MOCK_PROGRESS_MS")
                        .ok()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(40);
                    let count: u64 = std::env::var("PIR_MOCK_PROGRESS_COUNT")
                        .ok()
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(3);
                    if let Some(token) = params.pointer("/_meta/progressToken") {
                        for step in 1..=count {
                            write_mock_line(
                                &std::io::stdout(),
                                &json!({
                                    "jsonrpc": "2.0",
                                    "method": "notifications/progress",
                                    "params": {
                                        "progressToken": token,
                                        "progress": step,
                                        "total": count,
                                        "message": format!("step {step}"),
                                    },
                                }),
                            );
                            std::thread::sleep(std::time::Duration::from_millis(interval));
                        }
                    }
                    json!({
                        "content": [{ "type": "text", "text": format!("progress:{count}") }],
                    })
                } else if name == "noisy" {
                    if !emit_mock_stderr("mock-stderr:call") {
                        std::process::exit(3);
                    }
                    json!({ "content": [{ "type": "text", "text": "noisy-ok" }] })
                } else {
                    let args = params.get("arguments").cloned().unwrap_or(json!({}));
                    json!({
                        "content": [{
                            "type": "text",
                            "text": format!("echo:{}", args.get("text").and_then(Value::as_str).unwrap_or("")),
                        }],
                    })
                }
            }
            "resources/list" => {
                if no_resource_name {
                    json!({
                        "resources": [{ "uri": "mock://res/1", "mimeType": "text/plain" }],
                    })
                } else {
                    json!({
                        "resources": [{ "uri": "mock://res/1", "name": "one", "mimeType": "text/plain" }],
                    })
                }
            }
            "resources/templates/list" => {
                if std::env::var("PIR_MOCK_NO_TEMPLATES").as_deref() == Ok("1") {
                    write_mock_line(
                        &std::io::stdout(),
                        &json!({ "jsonrpc": "2.0", "id": id.clone(), "error": { "code": -32601, "message": "method not found" } }),
                    );
                    continue;
                }
                let mut template = json!({
                    "uriTemplate": "mock://res/{id}",
                    "title": "One",
                    "description": "A mock template",
                    "mimeType": "text/plain",
                });
                if !no_resource_name {
                    template["name"] = json!("one");
                }
                json!({ "resourceTemplates": [template] })
            }
            "resources/read" => json!({
                "contents": [{ "uri": "mock://res/1", "mimeType": "text/plain", "text": "hello" }],
            }),
            "prompts/list" => json!({ "prompts": [{ "name": "greet" }] }),
            "prompts/get" => json!({
                "messages": [{ "role": "user", "content": { "type": "text", "text": "hi" } }],
            }),
            _ => {
                write_mock_line(
                    &std::io::stdout(),
                    &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "method not found" } }),
                );
                continue;
            }
        };
        let response = json!({ "jsonrpc": "2.0", "id": id, "result": reply });
        if delayed {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                write_mock_line(&std::io::stdout(), &response);
            });
        } else {
            write_mock_line(&std::io::stdout(), &response);
        }
        if stall_after_init && method == "initialize" {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
    }
}

/// Garbage-then-good stdout child: writes an unterminated run of
/// `PIR_MOCK_RAW_LEN` bytes and an invalid JSON line before serving.
pub fn serve_raw_child() {
    let length: usize = std::env::var("PIR_MOCK_RAW_LEN")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2048);
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(&vec![b'x'; length]);
    let _ = out.write_all(b"\n");
    let _ = out.write_all(b"this is not json\n");
    let _ = out.flush();
    drop(out);
    serve_mock_requests();
}

/// Never answers the handshake; holds stdin open until EOF, writing a
/// sentinel/pid file when configured (the silent-child contract).
pub fn serve_silent_child() {
    let sentinel = std::env::var("PIR_MOCK_SENTINEL").unwrap_or_default();
    if let Ok(pid_file) = std::env::var("PIR_MOCK_PID") {
        let _ = std::fs::write(pid_file, std::process::id().to_string());
    }
    let mut sink = String::new();
    let _ = std::io::stdin().read_to_string(&mut sink);
    if !sentinel.is_empty() {
        let _ = std::fs::write(&sentinel, b"exited");
    }
}

fn write_mock_line(stdout: &std::io::Stdout, reply: &Value) {
    let mut out = stdout.lock();
    let _ = writeln!(out, "{reply}");
    let _ = out.flush();
}

/// Send one server-initiated request to the client and wait for its response.
fn probe_client(reader: &mut impl BufRead, method: &str, params: Option<Value>) -> Value {
    let id = format!("probe-{method}");
    write_mock_line(
        &std::io::stdout(),
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }),
    );
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return json!({ "error": "client closed" }),
            Ok(_) => {}
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("id").and_then(Value::as_str) == Some(id.as_str()) {
            return value;
        }
        if value.get("method").is_some() {
            if let Some(other_id) = value.get("id") {
                write_mock_line(
                    &std::io::stdout(),
                    &json!({
                        "jsonrpc": "2.0",
                        "id": other_id,
                        "error": { "code": -32601, "message": "method not found" },
                    }),
                );
            }
        }
    }
}

fn emit_mock_stderr(line: &str) -> bool {
    let mut stderr = std::io::stderr();
    writeln!(stderr, "{line}")
        .and_then(|()| stderr.flush())
        .is_ok()
}

// ---------------------------------------------------------------------------
// W13-B mock (the old `mcp_residuals_w13::serve_mock`)
// ---------------------------------------------------------------------------

pub mod w13 {
    use super::{write_file_atomically, write_line};
    use serde_json::{json, Value};

    /// Server behaviour, selected by `PIR_W13_MOCK`.
    pub const MOCK_SERVER_ERROR: &str = "server-error";
    pub const MOCK_SLOW: &str = "slow";
    pub const MOCK_PROGRESS: &str = "progress";
    /// A server that accepts the transport but NEVER answers a request.
    pub const MOCK_SILENT: &str = "silent";
    /// A well-behaved server.
    pub const MOCK_HEALTHY: &str = "healthy";

    pub const CANCEL_LOG_ENV: &str = "PIR_W13_CANCEL_LOG";
    pub const COUNT_FILE_ENV: &str = "PIR_W13_COUNT_FILE";
    pub const INIT_DELAY_ENV: &str = "PIR_W13_INIT_DELAY_MS";
    pub const PID_FILE_ENV: &str = "PIR_W13_PID_FILE";

    /// The line-delimited JSON-RPC loop of the W13 mock server.
    pub fn serve_mock(mode: &str, delay_ms: u64, init_delay_ms: u64) {
        use std::io::BufRead;
        use std::sync::{Arc, Mutex};

        let stdin = std::io::stdin();
        let stdout = Arc::new(Mutex::new(std::io::stdout()));
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let method = value.get("method").and_then(Value::as_str).unwrap_or("");
            let Some(id) = value.get("id").cloned() else {
                if method == "notifications/cancelled" {
                    if let Ok(path) = std::env::var(CANCEL_LOG_ENV) {
                        write_file_atomically(path, &value.to_string());
                    }
                }
                continue;
            };
            if method == "tools/call" {
                let count = std::env::var(COUNT_FILE_ENV)
                    .ok()
                    .and_then(|path| std::fs::read_to_string(path).ok())
                    .and_then(|text| text.trim().parse::<u64>().ok())
                    .unwrap_or(0)
                    + 1;
                if let Ok(path) = std::env::var(COUNT_FILE_ENV) {
                    write_file_atomically(path, &count.to_string());
                }
            }
            let reply = match method {
                "initialize" => {
                    if mode == MOCK_SILENT {
                        continue;
                    }
                    if init_delay_ms > 0 {
                        std::thread::sleep(std::time::Duration::from_millis(init_delay_ms));
                    }
                    Some(json!({
                        "protocolVersion": "2025-06-18",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "w13b-mock", "version": "1.0.0" },
                    }))
                }
                "tools/list" => Some(json!({
                    "tools": [{ "name": "work", "inputSchema": { "type": "object" } }],
                })),
                "tools/call" => match mode {
                    MOCK_SERVER_ERROR => {
                        let line = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32000, "message": "tool exploded" },
                        });
                        write_line(&stdout, &line);
                        continue;
                    }
                    MOCK_SLOW => {
                        let stdout = Arc::clone(&stdout);
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                            write_line(
                                &stdout,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": { "content": [{ "type": "text", "text": "late" }] },
                                }),
                            );
                        });
                        continue;
                    }
                    MOCK_PROGRESS => {
                        let token = value
                            .get("params")
                            .and_then(|params| params.get("_meta"))
                            .and_then(|meta| meta.get("progressToken"))
                            .cloned();
                        let stdout = Arc::clone(&stdout);
                        std::thread::spawn(move || {
                            let steps = 6u64;
                            let step = delay_ms / steps.max(1);
                            for index in 0..steps {
                                std::thread::sleep(std::time::Duration::from_millis(step));
                                if let Some(token) = &token {
                                    write_line(
                                        &stdout,
                                        &json!({
                                            "jsonrpc": "2.0",
                                            "method": "notifications/progress",
                                            "params": {
                                                "progressToken": token,
                                                "progress": index + 1,
                                            },
                                        }),
                                    );
                                }
                            }
                            write_line(
                                &stdout,
                                &json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": { "content": [{ "type": "text", "text": "done" }] },
                                }),
                            );
                        });
                        continue;
                    }
                    _ => continue,
                },
                _ => Some(json!({})),
            };
            if let Some(reply) = reply {
                write_line(
                    &stdout,
                    &json!({ "jsonrpc": "2.0", "id": id, "result": reply }),
                );
            }
        }
    }
}

fn write_line(stdout: &std::sync::Mutex<std::io::Stdout>, line: &Value) {
    let Ok(mut stdout) = stdout.lock() else {
        return;
    };
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

/// Atomic publish: the parent polls the file and may parse it immediately.
pub fn write_file_atomically(path: impl AsRef<std::path::Path>, text: &str) {
    let path = path.as_ref();
    let temp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    if std::fs::write(&temp, text).is_ok() {
        let _ = std::fs::rename(&temp, path);
    }
}
