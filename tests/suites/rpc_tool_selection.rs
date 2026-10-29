//! Q66: process-level RPC coverage for the global tool-selection flags.
//!
//! Spawns the real `pir rpc` binary against a loopback OpenAI-SSE mock provider
//! and asserts the names in the FIRST provider request's `tools` array. The
//! harness is hermetic: loopback only, a per-test temp agent/work directory
//! (`PIR_AGENT_DIR` + `HOME`/`USERPROFILE`), `PI_OFFLINE=1`, and no ambient
//! `~/.pi` state.
//!
//! `--only-builtin --no-mcp` keeps the registry deterministic (core tools +
//! the Rust builtins, no on-disk discovery/MCP), so `--tools read` must declare
//! exactly `read` and `--exclude-tools bash` must leave `read` in place.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Temp directories
// ---------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-q66-rpc-{label}-{}-{}",
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

// ---------------------------------------------------------------------------
// Loopback mock provider
// ---------------------------------------------------------------------------

struct MockProvider {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
    connections: Arc<std::sync::atomic::AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MockProvider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback mock provider");
        let address = listener.local_addr().expect("mock local addr");
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let recorded = Arc::clone(&requests);
        let accepted = Arc::clone(&connections);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        accepted.fetch_add(1, Ordering::SeqCst);
                        serve(stream, &recorded);
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            address,
            requests,
            connections,
            stop,
            thread: Some(thread),
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the blocking accept so the thread observes the stop flag.
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Read one HTTP request, record its JSON body, then answer with a complete
/// OpenAI SSE completion. The body is recorded BEFORE the response is written,
/// so a client that has read its prompt response can inspect the capture
/// without a settle wait.
fn serve(mut stream: TcpStream, recorded: &Mutex<Vec<Value>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(position) = find_subslice(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 1 << 20 {
            return;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(_) => return,
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let mut body = buffer[head_end..].to_vec();
    while body.len() < content_length {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => body.extend_from_slice(&chunk[..read]),
            Err(_) => break,
        }
    }
    let response = if head.starts_with("POST") {
        if let Ok(parsed) = serde_json::from_slice::<Value>(&body) {
            recorded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(parsed);
            sse_response()
        } else {
            http_response("text/plain", b"bad request".to_vec())
        }
    } else {
        http_response(
            "application/json",
            br#"{"object":"list","data":[{"id":"mock-model","object":"model"}]}"#.to_vec(),
        )
    };
    let _ = stream.write_all(&response);
    let _ = stream.flush();
}

fn http_response(content_type: &str, body: Vec<u8>) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncache-control: no-cache\r\n\
         connection: close\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    response
}

fn sse_chunk(delta: Value, finish_reason: Option<&str>, usage: Option<Value>) -> String {
    let mut payload = json!({
        "id": "mock-q66",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "mock-model",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }],
    });
    if let Some(usage) = usage {
        payload["usage"] = usage;
    }
    format!("data: {payload}\n\n")
}

fn sse_response() -> Vec<u8> {
    let mut body = String::new();
    body.push_str(&sse_chunk(
        json!({ "role": "assistant", "content": "" }),
        None,
        None,
    ));
    body.push_str(&sse_chunk(json!({ "content": "OK" }), None, None));
    body.push_str(&sse_chunk(
        json!({}),
        Some("stop"),
        Some(json!({ "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 })),
    ));
    body.push_str("data: [DONE]\n\n");
    http_response("text/event-stream", body.into_bytes())
}

// ---------------------------------------------------------------------------
// RPC child
// ---------------------------------------------------------------------------

struct RpcRun {
    child: Child,
    reader: BufReader<ChildStdout>,
    stdin: Option<std::process::ChildStdin>,
    provider: MockProvider,
    stderr_path: PathBuf,
    _root: TempDir,
}

impl RpcRun {
    fn spawn(label: &str, extra_args: &[&str]) -> Self {
        let provider = MockProvider::start();
        let root = TempDir::new(label);
        let agent = root.path().join("agent");
        let work = root.path().join("work");
        std::fs::create_dir_all(&agent).expect("create agent dir");
        std::fs::create_dir_all(&work).expect("create work dir");

        let models = json!({
            "providers": {
                "mock": {
                    "name": "Mock",
                    "baseUrl": provider.base_url(),
                    "api": "openai-completions",
                    "apiKey": "sk-mock-q66-not-a-real-credential",
                    "models": [{
                        "id": "mock-model",
                        "name": "Mock Model",
                        "reasoning": false,
                        "input": ["text"],
                        "contextWindow": 128000,
                        "maxTokens": 4096,
                    }],
                }
            }
        });
        std::fs::write(
            agent.join("models.json"),
            serde_json::to_string_pretty(&models).expect("serialize models.json"),
        )
        .expect("write models.json");
        let settings = json!({
            "defaultProvider": "mock",
            "defaultModel": "mock-model",
            "defaultThinkingLevel": "off",
        });
        std::fs::write(
            agent.join("settings.json"),
            serde_json::to_string_pretty(&settings).expect("serialize settings.json"),
        )
        .expect("write settings.json");

        let stderr_path = root.path().join("stderr.txt");
        let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr file");
        let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
        command
            .current_dir(&work)
            .env("PIR_AGENT_DIR", &agent)
            .env("PIR_CODING_AGENT_DIR", &agent)
            .env("PI_CODING_AGENT_DIR", &agent)
            .env("HOME", root.path())
            .env("USERPROFILE", root.path())
            .env(
                "PI_CODING_AGENT_SESSION_DIR",
                root.path().join("agent").join("sessions"),
            )
            .env(
                "PIR_SESSION_DIR",
                root.path().join("agent").join("sessions"),
            )
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .args(["rpc", "--only-builtin", "--no-mcp", "--offline"])
            .args(extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr_file));
        let mut child = command.spawn().expect("pir rpc should start");
        let stdout = child.stdout.take().expect("rpc stdout");
        let stdin = child.stdin.take().expect("rpc stdin");
        Self {
            child,
            reader: BufReader::new(stdout),
            stdin: Some(stdin),
            provider,
            stderr_path,
            _root: root,
        }
    }

    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("rpc stdin is open");
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .expect("write rpc line");
        stdin.flush().expect("flush rpc line");
    }

    fn read_line(&mut self) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read rpc line");
        assert!(
            !line.is_empty(),
            "rpc stdout closed before a response; stderr:\n{}",
            self.stderr_tail()
        );
        serde_json::from_str(line.trim()).expect("rpc line must be valid JSON")
    }

    fn read_response(&mut self, command: &str) -> Value {
        for _ in 0..500 {
            let value = self.read_line();
            if value["type"] == "response" && value["command"] == command {
                return value;
            }
        }
        panic!(
            "no {command} response arrived; stderr:\n{}",
            self.stderr_tail()
        )
    }

    fn stderr_tail(&self) -> String {
        let stderr = std::fs::read_to_string(&self.stderr_path).unwrap_or_default();
        let lines: Vec<&str> = stderr.lines().collect();
        lines[lines.len().saturating_sub(15)..].join("\n")
    }

    /// Send the first prompt of a fresh session and return the FIRST provider
    /// request body.
    fn first_request_for_prompt(&mut self) -> Value {
        self.send(r#"{"type":"hello"}"#);
        let hello = self.read_response("hello");
        assert_eq!(hello["success"], true, "{hello}");

        self.send(r#"{"type":"create","id":"c0"}"#);
        let created = self.read_response("create");
        assert_eq!(created["success"], true, "{created}");
        assert_eq!(
            created["data"]["model"], "mock-model",
            "the child must load the test models.json: {created}"
        );

        self.send(r#"{"type":"prompt","id":"p0","text":"Reply with OK."}"#);
        let prompt = self.read_response("prompt");
        assert_eq!(prompt["success"], true, "prompt failed: {prompt}");
        assert_eq!(prompt["data"]["disposition"], "started", "{prompt}");

        // The prompt response only ACKS the run (upstream RPC starts the run
        // asynchronously); wait for the run to settle before reading the
        // capture. Events stream bare at the top level.
        let mut settled = false;
        for _ in 0..500 {
            if self.read_line()["type"] == "agent_end" {
                settled = true;
                break;
            }
        }
        assert!(
            settled,
            "agent_end never arrived; stderr:\n{}",
            std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
        );

        let requests = self.provider.requests();
        assert!(
            !requests.is_empty(),
            "the provider was never reached ({} TCP connections); stderr:\n{}",
            self.provider
                .connections
                .load(std::sync::atomic::Ordering::SeqCst),
            std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
        );
        requests[0].clone()
    }

    fn finish(mut self) {
        drop(self.stdin.take());
        // Every response under test is already asserted. A prompt-initialized
        // child may spend the server's full 5 s writer-drain grace before
        // exiting, so cap the graceful window and reap; the dedicated
        // `rpc_extension_ui::extension_shutdown_*` tests own the clean-shutdown
        // contract.
        let deadline = Instant::now() + Duration::from_millis(250);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
            }
        }
    }
}

/// The names in a provider request's `tools` array (empty when absent).
fn declared_tools(body: &Value) -> Vec<String> {
    let mut names: Vec<String> = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str)
                        .or_else(|| tool.get("name").and_then(Value::as_str))
                        .map(str::to_owned)
                })
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// `pir rpc --tools read` declares exactly `read` to the provider.
#[cfg(feature = "tool-read")]
#[test]
fn rpc_tools_allowlist_is_declared_to_the_provider() {
    let mut run = RpcRun::spawn("allowlist", &["--tools", "read"]);
    let body = run.first_request_for_prompt();
    let names = declared_tools(&body);
    assert_eq!(
        names,
        vec!["read".to_owned()],
        "the allowlist is the RPC session's active set: {body}"
    );
    run.finish();
}

/// `pir rpc --no-tools` declares nothing (the request omits the array or
/// carries an empty one).
#[test]
fn rpc_no_tools_declares_nothing() {
    let mut run = RpcRun::spawn("no-tools", &["--no-tools"]);
    let body = run.first_request_for_prompt();
    assert!(
        declared_tools(&body).is_empty(),
        "`--no-tools` must declare no tools: {body}"
    );
    run.finish();
}

/// `pir rpc --exclude-tools bash` drops `bash` but keeps the rest of the
/// default base (`read` included).
#[cfg(feature = "tool-read")]
#[test]
fn rpc_exclude_tools_removes_a_core_tool() {
    let mut run = RpcRun::spawn("exclude", &["--exclude-tools", "bash"]);
    let body = run.first_request_for_prompt();
    let names = declared_tools(&body);
    assert!(
        names.iter().any(|name| name == "read"),
        "an unrelated core tool stays declared: {names:?}"
    );
    assert!(
        !names.iter().any(|name| name == "bash"),
        "the denylisted tool must not be declared: {names:?}"
    );
    run.finish();
}

/// A malformed selection fails before the protocol loop starts: exit 1,
/// empty stdout, the shared resolver's `--tools:` error on stderr.
#[test]
fn rpc_rejects_a_mixed_tool_list_before_serving() {
    let root = TempDir::new("mixed-list");
    let agent = root.path().join("agent");
    let work = root.path().join("work");
    std::fs::create_dir_all(&agent).expect("create agent dir");
    std::fs::create_dir_all(&work).expect("create work dir");
    let output = Command::new(env!("CARGO_BIN_EXE_pir"))
        .current_dir(&work)
        .env("PIR_AGENT_DIR", &agent)
        .env("HOME", root.path())
        .env("USERPROFILE", root.path())
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .args([
            "rpc",
            "--only-builtin",
            "--no-mcp",
            "--offline",
            "--tools",
            "read,+bash",
        ])
        .stdin(Stdio::null())
        .output()
        .expect("pir rpc should start");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a mixed --tools list must fail; stderr:\n{stderr}"
    );
    assert_eq!(output.status.code(), Some(1), "stderr:\n{stderr}");
    assert!(
        stderr.contains("--tools:"),
        "the shared resolver's validation error must reach stderr:\n{stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "no protocol frames may precede the failure: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}
