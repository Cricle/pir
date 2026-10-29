//! W14-G: end-to-end print-mode tool exposure, asserted on the WIRE.
//!
//! Three tool-exposure defects were fixed today and none of them had an
//! end-to-end regression test:
//!
//! 1. `pir print` never called `merge_plugin_tools`, so only the core tools
//!    reached the model (a real capture showed `tools=9`, then 21 after the
//!    fix);
//! 2. the session/interactive `--tools` resolution matched only core names, so
//!    `--tools ctx_execute` produced an EMPTY tool set;
//! 3. `print` hard-filtered its registry while a session applies an active
//!    override, so plugin tools were un-callable under `--exclude-tools`.
//!
//! The helper unit tests pin the pieces. This suite pins the WIRING: it spawns
//! the real `pir` binary against a loopback mock provider (the
//! `scripts/perf/body_probe.py` shape: an `openai-completions` provider pointed
//! at `127.0.0.1:<port>/v1`, a temp agent dir holding `models.json` +
//! `settings.json`) and asserts the names in the FIRST provider request's
//! `tools` array.
//!
//! Hermetic: loopback only, per-test temp agent/work directories, `PI_OFFLINE=1`
//! so no catalog/version network hop, no `~/.pi`, no credentials copied. The
//! child's stdout/stderr are redirected to files (never pipes) so a chatty run
//! cannot dead-lock the test, and every run has a bounded watchdog.
//!
//! Each test FAILS LOUDLY when the request is never captured: the capture
//! assertion runs first and prints the child's stderr tail.
//!
//! Case 4 note (read before changing the assertion): under
//! `--exclude-tools bash` alone the DECLARED set is every tool that activates
//! on registration in the host's available set, minus `bash` — in print AND in
//! a session. The exclude-only base in `resolve_tool_selection` is
//! `_isActivatedOnRegistration` over that set, not a fixed core list
//! (`src/coding_agent/tool_selection.rs:318-339`); print passes its MERGED
//! registry, so the core tools AND the registration-active plugin tools
//! (`ctx_*`, `python`, `todo`, ...) are declared, while `powershell`
//! (`defaultActive: false`) and the `codemode`/`tool_search` model-only tools
//! are not. `tests/suites/print_callability_w13.rs` pins the same set against
//! a real session. The bug-3 fix is about the REGISTRY, not the declaration:
//! plugin tools stay registered and callable. That half is asserted here the
//! only way a request capture can prove it — the mock answers the first
//! request with a call to `todo`, a plugin tool the old core-only base pruned
//! from the registry, and the second request must carry a real tool result
//! instead of the batch's "Tool todo not found"
//! (`src/agent/tool_scheduler/prepare.rs`).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// How long a spawned `pir print` may take before the watchdog kills it.
const RUN_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the watchdog waits for a KILLED child to be reaped. `Child::wait`
/// after a failed `kill` (already gone, access denied) would otherwise block
/// the test forever, so the post-kill wait is polled with this bound too.
const KILL_GRACE: Duration = Duration::from_secs(10);

/// How long the recorded-request list may lag the child's exit. The mock
/// records a body only after it has written the response, so the push can
/// trail the child's last read by a scheduling tick; the wait is bounded.
const REQUEST_SETTLE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Temp directories
// ---------------------------------------------------------------------------

/// Per-test temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-w14g-{label}-{}-{}",
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
//
// A deliberately tiny blocking HTTP/1.1 server: one request per connection,
// `Connection: close`, a `Content-Length`-delimited SSE body. It records every
// JSON POST body and answers the first `tool_call_turns` POSTs with a `todo`
// tool call so a follow-up request exists (the callability probe of case 4).

struct MockProvider {
    base_url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MockProvider {
    fn start(tool_call_turns: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback mock provider");
        listener
            .set_nonblocking(true)
            .expect("mock listener non-blocking");
        let addr = listener.local_addr().expect("mock local addr");
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let recorded = Arc::clone(&requests);
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut posts = 0usize;
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Some(body) = serve(stream, posts < tool_call_turns) {
                            recorded
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push(body);
                            posts += 1;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            base_url: format!("http://{addr}/v1"),
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Wait (bounded by [`REQUEST_SETTLE`]) until at least `count` request
    /// bodies are recorded, then return the snapshot. The mock records a body
    /// AFTER writing its response, so a child that has already exited can
    /// still be one push behind; without this the "never reached the mock"
    /// assertion could fire on a run that did reach it.
    fn wait_for_requests(&self, count: usize) -> Vec<Value> {
        let deadline = Instant::now() + REQUEST_SETTLE;
        loop {
            let requests = self.requests();
            if requests.len() >= count || Instant::now() >= deadline {
                return requests;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
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

/// Read one HTTP request. Returns the parsed JSON body for a POST with a body.
fn serve(mut stream: TcpStream, answer_with_tool_call: bool) -> Option<Value> {
    // The listener is non-blocking so the accept loop can poll its stop flag.
    // Winsock may hand the accepted socket the listener's non-blocking mode;
    // a non-blocking `read` would then fail with `WouldBlock` here and the
    // mock would answer NOTHING. Make blocking explicit, then bound both
    // directions: every read/write has a deadline, so neither this thread nor
    // the `join()` in `Drop` can wedge on a stalled peer.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(position) = find_subslice(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        if buffer.len() > 1 << 20 {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(_) => return None,
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
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
    let is_post = head.starts_with("POST");
    let parsed = if is_post && !body.is_empty() {
        serde_json::from_slice::<Value>(&body).ok()
    } else {
        None
    };
    let response = if is_post {
        sse_response(answer_with_tool_call)
    } else {
        // Any probe (e.g. `/v1/models`) gets a harmless catalog answer.
        http_response(
            "application/json",
            br#"{"object":"list","data":[{"id":"mock-model","object":"model"}]}"#.to_vec(),
        )
    };
    let _ = stream.write_all(&response);
    let _ = stream.flush();
    parsed
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

/// One OpenAI `chat.completion.chunk` SSE frame, built with `json!` so nothing
/// is hand-escaped.
fn chunk(delta: Value, finish_reason: Option<&str>, usage: Option<Value>) -> String {
    let mut payload = json!({
        "id": "mock-w14g",
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

fn sse_response(tool_call: bool) -> Vec<u8> {
    let mut body = String::new();
    if tool_call {
        // The same frame shape `scripts/perf/body_probe.py --multi-turn`
        // uses: a role delta first, then the tool call.
        body.push_str(&chunk(
            json!({ "role": "assistant", "content": "" }),
            None,
            None,
        ));
        body.push_str(&chunk(
            json!({
                "tool_calls": [{
                    "index": 0,
                    "id": "call_w14g_todo",
                    "type": "function",
                    "function": { "name": "todo", "arguments": "{\"action\":\"list\"}" },
                }]
            }),
            None,
            None,
        ));
        body.push_str(&chunk(
            json!({}),
            Some("tool_calls"),
            Some(json!({ "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 })),
        ));
    } else {
        body.push_str(&chunk(
            json!({ "role": "assistant", "content": "" }),
            None,
            None,
        ));
        body.push_str(&chunk(json!({ "content": "OK" }), None, None));
        body.push_str(&chunk(
            json!({}),
            Some("stop"),
            Some(json!({ "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 })),
        ));
    }
    body.push_str("data: [DONE]\n\n");
    http_response("text/event-stream", body.into_bytes())
}

// ---------------------------------------------------------------------------
// The print run
// ---------------------------------------------------------------------------

struct PrintRun {
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: String,
    stderr: String,
}

impl PrintRun {
    /// The child's stderr tail for assertion messages (bounded).
    fn stderr_tail(&self) -> String {
        let lines: Vec<&str> = self.stderr.lines().collect();
        let start = lines.len().saturating_sub(12);
        lines[start..].join("\n")
    }
}

/// Everything a case needs: the running mock plus the finished child.
struct Case {
    provider: MockProvider,
    run: PrintRun,
}

/// Start the mock, write an isolated agent dir, and run one `pir print` turn.
///
/// `settings_extra` is merged over the base settings; `extra_args` are appended
/// before the prompt.
fn run_case(
    label: &str,
    tool_call_turns: usize,
    settings_extra: Value,
    extra_args: &[&str],
) -> Case {
    let provider = MockProvider::start(tool_call_turns);
    let root = TempDir::new(label);
    let agent = root.path().join("agent");
    let work = root.path().join("work");
    std::fs::create_dir_all(&agent).expect("create agent dir");
    std::fs::create_dir_all(&work).expect("create work dir");

    // models.json: the `body_probe.py` shape — an openai-completions provider
    // pointed at the loopback mock, with an inline throwaway key.
    let models = json!({
        "providers": {
            "mock": {
                "name": "Mock",
                "baseUrl": provider.base_url.clone(),
                "api": "openai-completions",
                "apiKey": "sk-mock-w14g-not-a-real-credential",
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

    let mut settings = json!({
        "defaultProvider": "mock",
        "defaultModel": "mock-model",
        "defaultThinkingLevel": "off",
    });
    if let (Some(base), Some(extra)) = (settings.as_object_mut(), settings_extra.as_object()) {
        for (key, value) in extra {
            base.insert(key.clone(), value.clone());
        }
    }
    std::fs::write(
        agent.join("settings.json"),
        serde_json::to_string_pretty(&settings).expect("serialize settings.json"),
    )
    .expect("write settings.json");

    let stdout_path = root.path().join("stdout.txt");
    let stderr_path = root.path().join("stderr.txt");
    let stdout_file = std::fs::File::create(&stdout_path).expect("create stdout file");
    let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr file");

    let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .current_dir(&work)
        // No `~/.pi`: the agent dir carries models.json + settings.json only.
        .env("PIR_AGENT_DIR", &agent)
        .env("PIR_CODING_AGENT_DIR", &agent)
        .env("PI_CODING_AGENT_DIR", &agent)
        .env("HOME", root.path())
        .env("USERPROFILE", root.path())
        // No startup network hop (remote catalog overlays, version check).
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        // Loopback must never go through an ambient proxy.
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env("RUST_MIN_STACK", "8388608")
        .args([
            "print",
            "--no-session",
            "--no-context-files",
            "--no-skills",
            "--no-prompt-templates",
            "--provider",
            "mock",
            "--model",
            "mock-model",
        ])
        .args(extra_args)
        .arg("Reply with the single word OK.")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));

    let mut child = command.spawn().expect("spawn the pir binary");
    let deadline = Instant::now() + RUN_TIMEOUT;
    let mut timed_out = false;
    let mut reaped = true;
    let status = loop {
        match child.try_wait().expect("poll the pir child") {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                // `kill` is best-effort; a plain `wait()` afterwards is an
                // UNBOUNDED wait when the kill failed (the child vanished, or
                // the OS denied it). Poll the reap with its own bound instead.
                let _ = child.kill();
                let reap_deadline = Instant::now() + KILL_GRACE;
                reaped = loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break true,
                        Ok(None) if Instant::now() >= reap_deadline => break false,
                        Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                        Err(_) => break false,
                    }
                };
                timed_out = true;
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };

    let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    let run = PrintRun {
        exit_code: status.and_then(|status| status.code()),
        timed_out,
        stdout,
        stderr,
    };
    assert!(
        !run.timed_out && reaped,
        "pir print did not exit within {RUN_TIMEOUT:?} (case {label}, reaped={reaped}); \
         stderr tail:\n{}",
        run.stderr_tail()
    );
    Case { provider, run }
}

/// The last `limit` characters of a string (assertion messages only).
fn tail(text: &str, limit: usize) -> String {
    let start = text
        .char_indices()
        .rev()
        .nth(limit)
        .map(|(index, _)| index)
        .unwrap_or(0);
    text[start..].to_owned()
}

/// Succeed, or panic with the child's stderr so a startup failure is never
/// reported as a tool-set failure.
fn assert_success(case: &Case) {
    assert_eq!(
        case.run.exit_code,
        Some(0),
        "pir print failed (stdout tail:\n{}\nstderr tail:\n{})",
        tail(&case.run.stdout, 600),
        case.run.stderr_tail()
    );
}

/// The names in the FIRST captured request's `tools` array.
///
/// Fails loudly (with the child's stderr) when the CLI never reached the mock,
/// or reached it without a `tools` array — both are the regression's signature.
fn declared_tools(case: &Case) -> Vec<String> {
    // Bounded settle: the mock records the body after it answers, so the push
    // can lag the child's exit by one scheduling tick.
    let requests = case.provider.wait_for_requests(1);
    assert!(
        !requests.is_empty(),
        "the CLI never reached the mock provider: no provider request was captured\n\
         stdout:\n{}\nstderr tail:\n{}",
        case.run.stdout,
        case.run.stderr_tail()
    );
    let body = &requests[0];
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .unwrap_or_else(|| {
            panic!(
                "the first provider request carries no `tools` array: {body}\nstderr tail:\n{}",
                case.run.stderr_tail()
            )
        });
    let mut names: Vec<String> = tools
        .iter()
        .filter_map(|tool| {
            tool.get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .or_else(|| tool.get("name").and_then(Value::as_str))
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    names.dedup();
    assert!(
        !names.is_empty(),
        "the first provider request declares an EMPTY tool set: {body}"
    );
    names
}

fn assert_declares(names: &[String], expected: &str) {
    assert!(
        names.iter().any(|name| name == expected),
        "the request must declare `{expected}`; declared: {names:?}"
    );
}

/// The core tools every default run declares (the pre-fix body carried exactly
/// these nine, `powershell` being `default_active: false`).
fn assert_core_declared(names: &[String]) {
    for core in [
        "read",
        "bash",
        "edit",
        "write",
        "ls",
        "background_status",
        "background_abort",
    ] {
        assert_declares(names, core);
    }
    #[cfg(feature = "grep-find")]
    for core in ["grep", "find"] {
        assert_declares(names, core);
    }
}

/// The builtin-plugin tools the task names as the minimum end-to-end set,
/// each gated on the cargo feature that compiles it in.
fn assert_plugin_tools_declared(names: &[String]) {
    #[cfg(feature = "context-mode")]
    for tool in [
        "ctx_execute",
        "ctx_execute_file",
        "ctx_batch_execute",
        "ctx_index",
        "ctx_search",
        "ctx_fetch_and_index",
        "ctx_stats",
        "ctx_purge",
        "ctx_doctor",
    ] {
        assert_declares(names, tool);
    }
    #[cfg(feature = "python")]
    assert_declares(names, "python");
    #[cfg(feature = "todo")]
    assert_declares(names, "todo");
    #[cfg(feature = "subagent")]
    assert_declares(names, "subagent");
}

// ---------------------------------------------------------------------------
// 1. Default `pir print`
// ---------------------------------------------------------------------------

/// Regression for defect 1: print must declare the core tools AND the builtin
/// plugin tools. Before the `merge_plugin_tools` call the captured body carried
/// exactly the nine default-active core tools.
#[test]
fn default_print_declares_core_and_builtin_plugin_tools() {
    let case = run_case("default", 0, json!({}), &[]);
    assert_success(&case);
    let names = declared_tools(&case);
    assert_core_declared(&names);
    assert_plugin_tools_declared(&names);

    // The pre-fix body was exactly the nine core names; a run that never merged
    // the plugin host can never reach this count.
    let plugin_count = {
        let mut count = 0;
        #[cfg(feature = "context-mode")]
        {
            count += 9;
        }
        #[cfg(feature = "python")]
        {
            count += 1;
        }
        #[cfg(feature = "todo")]
        {
            count += 1;
        }
        #[cfg(feature = "subagent")]
        {
            count += 1;
        }
        count
    };
    assert!(
        names.len() >= 9 + plugin_count,
        "the merged plugin tools are missing: {} declared, at least {} expected ({names:?})",
        names.len(),
        9 + plugin_count
    );
}

// ---------------------------------------------------------------------------
// 2. `--tools ctx_execute`
// ---------------------------------------------------------------------------

/// Regression for defect 2: the `--tools` allowlist must be matched against the
/// MERGED registry and declare exactly the named tool. A core-only available
/// list matched nothing and produced an empty tool set.
#[cfg(feature = "context-mode")]
#[test]
fn tools_allowlist_declares_exactly_the_named_tool() {
    let case = run_case("allowlist", 0, json!({}), &["--tools", "ctx_execute"]);
    assert_success(&case);
    let names = declared_tools(&case);
    assert_eq!(
        names,
        vec!["ctx_execute".to_owned()],
        "`--tools ctx_execute` must declare exactly that tool"
    );
}

// ---------------------------------------------------------------------------
// 3. `--no-builtin-tools`
// ---------------------------------------------------------------------------

/// `--no-builtin-tools` keeps extension/custom tools and drops every built-in
/// (core) tool. Before plugin tools reached print's registry at all, this flag
/// left nothing.
#[test]
fn no_builtin_tools_keeps_plugin_tools_and_drops_core() {
    let case = run_case("no-builtin", 0, json!({}), &["--no-builtin-tools"]);
    assert_success(&case);
    let names = declared_tools(&case);

    for core in [
        "read",
        "bash",
        "edit",
        "write",
        "ls",
        "powershell",
        "background_status",
        "background_abort",
    ] {
        assert!(
            !names.iter().any(|name| name == core),
            "`--no-builtin-tools` must drop the core tool `{core}`: {names:?}"
        );
    }
    #[cfg(feature = "grep-find")]
    for core in ["grep", "find"] {
        assert!(
            !names.iter().any(|name| name == core),
            "`--no-builtin-tools` must drop the core tool `{core}`: {names:?}"
        );
    }
    assert_plugin_tools_declared(&names);
}

// ---------------------------------------------------------------------------
// 4. `--exclude-tools bash`
// ---------------------------------------------------------------------------

/// Regression for defect 3 (the registry half) plus the declared-set contract.
///
/// Declared set: the shared selection rule's exclude-only base is every tool
/// that activates on registration in the host's available set, so `bash` is
/// gone, the other registration-active core tools survive, and the
/// registration-active plugin tools (`ctx_*`, `python`, `todo`, ...) are
/// declared too (`src/coding_agent/tool_selection.rs:318-339`) — the same set
/// a session declares, pinned by `tests/suites/print_callability_w13.rs`.
///
/// Registry: the fix is that plugin tools stay REGISTERED. A request capture
/// cannot read a registry, so the mock answers the first request with a call to
/// `todo` — a plugin tool the old core-only base pruned from the registry and
/// made un-callable. (The corrected base also declares `todo`, since it
/// activates on registration; the call pins the merged plugin tool's
/// end-to-end callability.) If a host still filtered its registry, the batch
/// would answer "Tool todo not found"
/// (`src/agent/tool_scheduler/prepare.rs`); with the fix the second request
/// carries a real todo tool result.
#[test]
fn exclude_tools_bash_keeps_plugin_tools_registered() {
    let case = run_case("exclude", 1, json!({}), &["--exclude-tools", "bash"]);
    assert_success(&case);
    let names = declared_tools(&case);

    assert!(
        !names.iter().any(|name| name == "bash"),
        "`--exclude-tools bash` must not declare `bash`: {names:?}"
    );
    for core in ["read", "edit", "write", "ls"] {
        assert_declares(&names, core);
    }

    // The callability probe: the todo call the mock issued reached a registry
    // that still holds the plugin tool. Wait (bounded) for the follow-up turn
    // the tool result causes.
    let requests = case.provider.wait_for_requests(2);
    assert!(
        requests.len() >= 2,
        "the todo tool-call turn must be followed by a second provider request; \
         captured {} request(s)\nstderr tail:\n{}",
        requests.len(),
        case.run.stderr_tail()
    );
    let second = serde_json::to_string(&requests[1]).expect("serialize the second request");
    assert!(
        !second.contains("Tool todo not found"),
        "print's registry must still hold plugin tools under --exclude-tools: {second}"
    );
    let tool_messages: Vec<&Value> = requests[1]
        .get("messages")
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        !tool_messages.is_empty(),
        "the second request must carry a tool result: {second}"
    );
    assert!(
        tool_messages.iter().any(|message| {
            message.get("tool_call_id").and_then(Value::as_str) == Some("call_w14g_todo")
                && message
                    .get("content")
                    .and_then(Value::as_str)
                    .is_some_and(|content| content == "No tasks")
        }),
        "the undeclared but REGISTERED todo tool must have executed (its real \
         `No tasks` envelope, not a registry miss): {second}"
    );
}

// ---------------------------------------------------------------------------
// 5. `--no-extensions`
// ---------------------------------------------------------------------------

/// `--no-extensions` loads no builtin plugin host, so no plugin tool can be
/// declared; the core registry is unaffected.
///
/// The CLI still validates a non-empty extension plan strictly, so the mock
/// provider check runs after it; an EMPTY plan skips the runtime entirely.
#[test]
fn no_extensions_declares_no_plugin_tools() {
    let case = run_case("no-extensions", 0, json!({}), &["--no-extensions"]);
    assert_success(&case);
    let names = declared_tools(&case);

    for name in &names {
        assert!(
            !name.starts_with("ctx_")
                && !name.starts_with("mcp__")
                && name != "python"
                && name != "todo"
                && name != "subagent"
                && name != "tool_search"
                && name != "codemode",
            "`--no-extensions` must declare no plugin tool, found `{name}`: {names:?}"
        );
    }
    assert_core_declared(&names);
}

// ---------------------------------------------------------------------------
// 6. `disabledBuiltinExtensions: ["context-mode"]`
// ---------------------------------------------------------------------------

/// The settings-driven builtin gate reaches print: `print_plugin_host` applies
/// `restrict_plan_for_builtins` (the same helper the session host uses), so the
/// `builtin:context-mode` entry is dropped before its tools register.
#[cfg(feature = "context-mode")]
#[test]
fn disabled_builtin_extension_context_mode_declares_no_ctx_tools() {
    let case = run_case(
        "disabled-context-mode",
        0,
        json!({ "disabledBuiltinExtensions": ["context-mode"] }),
        &[],
    );
    assert_success(&case);
    let names = declared_tools(&case);

    assert!(
        !names.iter().any(|name| name.starts_with("ctx_")),
        "`disabledBuiltinExtensions: [\"context-mode\"]` must remove every ctx_* tool: {names:?}"
    );
    // The rest of the plan is untouched.
    #[cfg(feature = "todo")]
    assert_declares(&names, "todo");
    #[cfg(feature = "python")]
    assert_declares(&names, "python");
    #[cfg(feature = "subagent")]
    assert_declares(&names, "subagent");
}

// ---------------------------------------------------------------------------
// 7. `settings.pythonTool: false`
// ---------------------------------------------------------------------------

/// `pythonTool: false` folds `python` into `SettingsManager::disabled_builtins`,
/// so `builtin:python` is dropped from the plan before the host instantiates
/// it. Print used to build its host directly and skip that gate.
#[cfg(feature = "python")]
#[test]
fn python_tool_disabled_setting_declares_no_python_tool() {
    let case = run_case("python-tool-off", 0, json!({ "pythonTool": false }), &[]);
    assert_success(&case);
    let names = declared_tools(&case);

    assert!(
        !names.iter().any(|name| name == "python"),
        "`pythonTool: false` must remove the python tool: {names:?}"
    );
    // The other builtins stay on the plan.
    #[cfg(feature = "context-mode")]
    assert_declares(&names, "ctx_execute");
    #[cfg(feature = "todo")]
    assert_declares(&names, "todo");
}
