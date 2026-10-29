//! Fixed, reproducible agent-efficiency benchmark: `pir` vs upstream `pi`
//! driven by the SAME local model through the SAME OpenAI-compatible endpoint.
//!
//! Design (see the branch docs):
//! - **Model source**: an OpenAI-compatible endpoint, model id from
//!   `PIR_BENCH_MODEL`. By default that endpoint is a local ollama
//!   (`http://localhost:11434/v1`); setting `PIR_BENCH_BASE_URL` (+ optional
//!   `PIR_BENCH_API_KEY`, `PIR_BENCH_PROVIDER`) points both tools at any
//!   remote gateway instead. Both tools read the same `models.json` — a temp
//!   agent dir is exported as `PI_CODING_AGENT_DIR`
//!   (upstream `getAgentDir`) and `PIR_AGENT_DIR` (pir's override), so both
//!   select the identical provider/model with `api:
//!   "openai-completions"` (the upstream `KnownApi` spelling; pir accepts it
//!   plus its `openai-chat` alias). `--provider <id> --model <id>
//!   --thinking off` pins the same model and thinking level on both sides.
//! - **Tasks**: five fixed agent-flavored tasks with machine-checkable
//!   outcomes, fixtures generated into a temp dir by this harness.
//! - **Runner**: both CLIs run non-interactive JSON event mode
//!   (`pir print --mode json` / `pi --mode json -p`, both `--no-session`),
//!   NDJSON events parsed for wall time, turns, tool calls, token usage, and
//!   final text; the task outcome is verified against the filesystem / answer.
//! - **Gate**: both tools must complete >= 4 of 5 tasks (parity). Timing is
//!   reported, never asserted, except a coarse 10 min/task sanity bound.
//!
//! The test is `#[ignore]`d (it is opt-in) and, when run with `--ignored`,
//! skips fast unless the endpoint is reachable AND `PIR_RUN_AGENT_BENCH=1` is
//! set. `scripts/agent-benchmark.cmd` wires that up (checks the endpoint,
//! pulls the model for local ollama, runs this test with `--nocapture
//! --ignored`).

use std::io::{BufRead, Read};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Default model (override with `PIR_BENCH_MODEL`, e.g. `qwen3:0.6b` for a
/// fast smoke run).
const DEFAULT_MODEL: &str = "qwen3:8b";

/// Coarse per-task sanity bound (also the hard kill timeout). Generous for
/// small local models; timing below it is reported, not asserted.
const DEFAULT_TASK_TIMEOUT_SECS: u64 = 600;

/// Tool-side run budget: the wall-clock kill timeout for one CLI invocation.
fn task_timeout() -> Duration {
    let secs = std::env::var("PIR_BENCH_TASK_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_TASK_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

fn bench_model() -> String {
    std::env::var("PIR_BENCH_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

/// Custom OpenAI-compatible base URL (trailing slash trimmed). `None` = the
/// default local ollama endpoint.
fn bench_base_url() -> Option<String> {
    std::env::var("PIR_BENCH_BASE_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
}

/// API key for the endpoint (the value is ignored by local ollama, but a
/// non-empty `apiKey` must be present in models.json for upstream
/// `composeModelProvider` to accept the provider).
fn bench_api_key() -> String {
    std::env::var("PIR_BENCH_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "ollama".to_string())
}

/// Provider id used in models.json and `--provider` (arbitrary on both sides,
/// must only match between the file and the CLI flag).
fn bench_provider() -> String {
    std::env::var("PIR_BENCH_PROVIDER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "ollama".to_string())
}

fn bench_enabled() -> bool {
    std::env::var("PIR_RUN_AGENT_BENCH").as_deref() == Ok("1")
}

/// Fast port probe (same approach as `tests/e2e_ollama.rs`): when the
/// endpoint is absent the default transport would burn seconds on connect
/// timeouts.
fn endpoint_available() -> bool {
    match bench_base_url() {
        Some(base) => tcp_probe_url(&base),
        None => ollama_available(),
    }
}

/// TCP probe of the default local ollama port.
fn ollama_available() -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], 11434));
    TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok()
}

/// TCP probe of the `host:port` inside an `http(s)://...` base URL.
fn tcp_probe_url(base: &str) -> bool {
    use std::net::ToSocketAddrs;
    let rest = base
        .strip_prefix("https://")
        .or_else(|| base.strip_prefix("http://"))
        .unwrap_or(base);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let default_port: u16 = if base.starts_with("https://") {
        443
    } else {
        80
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, digits))
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) =>
        {
            (
                host.to_string(),
                digits.parse::<u16>().unwrap_or(default_port),
            )
        }
        _ => (authority.to_string(), default_port),
    };
    match (host.as_str(), port).to_socket_addrs() {
        Ok(mut addrs) => {
            addrs.any(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok())
        }
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Tool identities
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    Pir,
    Pi,
}

impl Tool {
    fn label(self) -> &'static str {
        match self {
            Tool::Pir => "pir",
            Tool::Pi => "pi",
        }
    }
}

/// The `pi` program: `PIR_BENCH_PI_BIN` first, then the npm global shim
/// (`pi.cmd` on Windows — std spawns `.cmd`/`.bat` through `cmd.exe`; plain
/// `pi` elsewhere).
fn pi_program() -> String {
    std::env::var("PIR_BENCH_PI_BIN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "pi.cmd".to_string()
            } else {
                "pi".to_string()
            }
        })
}

fn pi_available() -> bool {
    Command::new(pi_program())
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Per-run metrics from the NDJSON event stream
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct Metrics {
    wall_ms: u128,
    /// `turn_start` events (both tools emit them identically).
    assistant_turns: u32,
    /// `toolcall_start` stream events (model requested tool calls).
    tool_call_events: u32,
    /// `tool_execution_start` events (harness actually executed tools).
    tool_executions: u32,
    /// Whether any `write`/`edit` tool executed (fs-effect evidence).
    write_or_edit_used: bool,
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    /// Text accumulated from `text_delta` stream events.
    delta_text: String,
    /// Final assistant message text from `message_end`/`turn_end`/
    /// `stream_end` lines (authoritative when present).
    message_text: String,
}

impl Metrics {
    /// Final response text: prefer the authoritative assistant message, fall
    /// back to the accumulated deltas.
    fn final_text(&self) -> &str {
        let message = self.message_text.trim();
        if message.is_empty() {
            self.delta_text.trim()
        } else {
            message
        }
    }
}

/// Pull the text out of an assistant message object across the two wire
/// shapes (`content` is a block array in both; tolerate a plain string).
fn assistant_message_text(message: &serde_json::Value) -> String {
    if let Some(text) = message.get("content").and_then(serde_json::Value::as_str) {
        return text.to_string();
    }
    let mut text = String::new();
    if let Some(blocks) = message.get("content").and_then(serde_json::Value::as_array) {
        for block in blocks {
            if block.get("type").and_then(serde_json::Value::as_str) == Some("text") {
                if let Some(part) = block.get("text").and_then(serde_json::Value::as_str) {
                    text.push_str(part);
                }
            }
        }
    }
    text
}

fn capture_usage(metrics: &mut Metrics, usage: &serde_json::Value) {
    metrics.input_tokens = usage
        .get("input")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(metrics.input_tokens);
    metrics.output_tokens = usage
        .get("output")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(metrics.output_tokens);
    metrics.total_tokens = usage
        .get("totalTokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(metrics.total_tokens);
}

/// Fold one NDJSON line into the metrics. Both wire contracts are covered:
/// - upstream (`modes/json-event.ts`): `message_update` with hoisted
///   top-level `usage` + `assistantMessageEvent` (snapshot stripped),
///   `message_end`/`turn_end`/`agent_end` with full messages;
/// - pir (`coding_agent/json.rs`): the same `message_update` shape plus
///   pir's `stream_end`/`turn_end` lifecycle lines.
fn absorb_line(metrics: &mut Metrics, line: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return; // non-JSON noise (banners, warnings) is ignored
    };
    if value.get("type").and_then(serde_json::Value::as_str) == Some("session") {
        return; // persisted-session header line
    }
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("turn_start") => metrics.assistant_turns += 1,
        Some("message_update") => {
            if let Some(usage) = value.get("usage") {
                capture_usage(metrics, usage);
            }
            if let Some(event) = value.get("assistantMessageEvent") {
                match event.get("type").and_then(serde_json::Value::as_str) {
                    Some("text_delta") => {
                        if let Some(delta) = event.get("delta").and_then(serde_json::Value::as_str)
                        {
                            metrics.delta_text.push_str(delta);
                        }
                    }
                    Some("toolcall_start") => metrics.tool_call_events += 1,
                    _ => {}
                }
            }
        }
        Some("message_end") | Some("turn_end") | Some("stream_end") => {
            if let Some(message) = value.get("message") {
                if message.get("role").and_then(serde_json::Value::as_str) == Some("assistant") {
                    let text = assistant_message_text(message);
                    if !text.trim().is_empty() {
                        metrics.message_text = text;
                    }
                    if let Some(usage) = message.get("usage") {
                        capture_usage(metrics, usage);
                    }
                }
            }
        }
        Some("tool_execution_start") => {
            metrics.tool_executions += 1;
            if let Some(name) = value.get("toolName").and_then(serde_json::Value::as_str) {
                if name == "write" || name == "edit" {
                    metrics.write_or_edit_used = true;
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Task definitions
// ---------------------------------------------------------------------------

struct TaskSpec {
    id: &'static str,
    title: &'static str,
    /// Task (e) runs with `--no-tools` as the plain-Q&A latency baseline.
    use_tools: bool,
    /// Fixture files written into the task dir before the run.
    files: &'static [(&'static str, &'static str)],
    prompt: fn(&Path) -> String,
    verify: fn(&Path, &Metrics) -> Result<(), String>,
}

const TODO_FIXTURE: &str = "\
// build checklist for the fixture generator
line 01: nothing to see here
line 02: TODO wire the retry loop
line 03: done items stay done
line 04: TODO document the deploy key rotation
line 05: nothing to see here
line 06: TODO bump the context window
line 07: keep this line clean
line 08: TODO split the scheduler tick
line 09: historical note, no marker
line 10: TODO remove the dead flag
line 11: clean
line 12: TODO rename the adapter module
line 13: clean
line 14: ship it
";

const FIXTURE_RS: &str = "\
// Agent benchmark fixture: `answer` must return 4.
// The value below is wrong; the agent under test should fix it.
pub fn answer() -> i32 {
    3
}

pub fn main() {
    println!(\"{}\", answer());
}
";

const NOTES_ALPHA: &str = "\
# Sprint notes (alpha)

- API key rotation moved to the platform team (API-KEY-7719).
- The staging certificate expires next quarter.
- Reminder: the telemetry endpoint moved to telemetry.internal.
- No deploy changes in this cycle.
";

const NOTES_BETA: &str = "\
# Sprint notes (beta)

- The deploy key is DEPLOY-KEY-9f3a; rotate it quarterly.
- Rollback window is 30 minutes after each deploy.
- Build cache was moved to the regional mirror.
";

fn count_todo_lines(contents: &str) -> usize {
    contents
        .lines()
        .filter(|line| line.contains("TODO"))
        .count()
}

fn task_prompt_hello(_dir: &Path) -> String {
    "Use your tools: create a file named hello.txt in the current directory with \
exactly the content `hello world`, then delete that file again so it no longer \
exists. Confirm when done."
        .to_string()
}

fn task_prompt_todo(dir: &Path) -> String {
    let path = dir.join("todos.txt");
    format!(
        "Read the file {} and count how many lines contain the marker TODO. \
Reply with only the number of TODO lines.",
        path.display()
    )
}

fn task_prompt_fixture(_dir: &Path) -> String {
    "In the current directory there is a file fixture.rs. Its function `answer` \
must return 4 but currently returns the wrong value. Edit fixture.rs so that \
`answer` returns 4. Keep the same function signature and do not touch `main`."
        .to_string()
}

fn task_prompt_deploy(_dir: &Path) -> String {
    "Two files sit in the current directory: notes-alpha.md and notes-beta.md. \
Exactly one of them mentions the deploy key. Read both files and reply with \
only the name of the file that mentions the deploy key."
        .to_string()
}

fn task_prompt_plain(_dir: &Path) -> String {
    "Reply with exactly the token BENCH-OK-42 and nothing else.".to_string()
}

/// `true` when `needle` appears as a standalone token (delimited by
/// non-alphanumerics), so "7" matches "7 TODO markers" but not "17".
fn contains_word(text: &str, needle: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
        .any(|token| token == needle)
}

fn verify_hello(dir: &Path, metrics: &Metrics) -> Result<(), String> {
    let path = dir.join("hello.txt");
    if path.exists() {
        return Err("hello.txt still exists after the run (delete step failed)".to_string());
    }
    if metrics.tool_executions == 0 {
        return Err(
            "hello.txt is absent but no tool ran at all (vacuous pass, agent likely \
answered without acting)"
                .to_string(),
        );
    }
    Ok(())
}

fn verify_todo(dir: &Path, metrics: &Metrics) -> Result<(), String> {
    let expected = count_todo_lines(TODO_FIXTURE);
    let text = metrics.final_text();
    if contains_word(text, &expected.to_string()) {
        Ok(())
    } else {
        Err(format!(
            "answer does not contain the expected count {expected} (todos.txt: {})",
            dir.join("todos.txt").display()
        ))
    }
}

/// Evaluate the fixed `answer` body: a single integer literal, or a simple
/// `a + b` sum, must equal 4.
fn verify_fixture_rs(dir: &Path, _metrics: &Metrics) -> Result<(), String> {
    let path = dir.join("fixture.rs");
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read edited fixture.rs: {error}"))?;
    let start = text
        .find("fn answer")
        .ok_or_else(|| "fixture.rs no longer contains `fn answer`".to_string())?;
    let rest = &text[start..];
    let open = rest
        .find('{')
        .ok_or_else(|| "cannot find the body of `answer`".to_string())?;
    let mut depth = 0usize;
    let mut end = None;
    for (index, ch) in rest[open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(open + index);
                    break;
                }
            }
            _ => {}
        }
    }
    let end = end.ok_or_else(|| "unbalanced braces in `answer`".to_string())?;
    // Strip line comments and whitespace so `4 // fixed` verifies like `4`.
    let body: String = rest[open + 1..end]
        .lines()
        .map(|line| line.split("//").next().unwrap_or(line))
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let value = if let Ok(literal) = body.parse::<i64>() {
        literal
    } else if let Some((left, right)) = body.split_once('+') {
        let left: i64 = left
            .trim()
            .parse()
            .map_err(|_| format!("unrecognized `answer` body: {body:?}"))?;
        let right: i64 = right
            .trim()
            .parse()
            .map_err(|_| format!("unrecognized `answer` body: {body:?}"))?;
        left + right
    } else {
        return Err(format!("unrecognized `answer` body: {body:?}"));
    };
    if value == 4 {
        Ok(())
    } else {
        Err(format!("`answer` still evaluates to {value}, expected 4"))
    }
}

fn verify_deploy(_dir: &Path, metrics: &Metrics) -> Result<(), String> {
    let text = metrics.final_text().to_lowercase();
    if text.contains("notes-beta") {
        Ok(())
    } else {
        Err("answer does not name notes-beta.md as the deploy-key file".to_string())
    }
}

fn verify_plain(_dir: &Path, metrics: &Metrics) -> Result<(), String> {
    let text = metrics.final_text();
    if contains_word(text, "BENCH-OK-42") {
        Ok(())
    } else {
        Err("reply does not contain the exact token BENCH-OK-42".to_string())
    }
}

fn task_specs() -> Vec<TaskSpec> {
    vec![
        TaskSpec {
            id: "create-delete",
            title: "create + delete hello.txt",
            use_tools: true,
            files: &[],
            prompt: task_prompt_hello,
            verify: verify_hello,
        },
        TaskSpec {
            id: "count-todos",
            title: "count TODO markers in todos.txt",
            use_tools: true,
            files: &[("todos.txt", TODO_FIXTURE)],
            prompt: task_prompt_todo,
            verify: verify_todo,
        },
        TaskSpec {
            id: "fix-fixture",
            title: "fix fixture.rs so answer() returns 4",
            use_tools: true,
            files: &[("fixture.rs", FIXTURE_RS)],
            prompt: task_prompt_fixture,
            verify: verify_fixture_rs,
        },
        TaskSpec {
            id: "two-hop-deploy",
            title: "two-hop research: which file has the deploy key",
            use_tools: true,
            files: &[
                ("notes-alpha.md", NOTES_ALPHA),
                ("notes-beta.md", NOTES_BETA),
            ],
            prompt: task_prompt_deploy,
            verify: verify_deploy,
        },
        TaskSpec {
            id: "no-tools-qa",
            title: "no-tools Q&A latency baseline",
            use_tools: false,
            files: &[],
            prompt: task_prompt_plain,
            verify: verify_plain,
        },
    ]
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn unique_temp_root() -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("pir-agent-bench-{}-{nonce}", std::process::id()))
}

/// The models.json BOTH tools read (single source of truth for provider +
/// model). `apiKey` must be present: upstream `composeModelProvider` refuses
/// providers with no auth method at all, and ollama ignores the value.
fn models_json(model_id: &str) -> String {
    let base_url = bench_base_url().unwrap_or_else(|| "http://localhost:11434/v1".to_string());
    let provider = bench_provider();
    let provider_name = format!("Agent benchmark ({provider})");
    serde_json::to_string_pretty(&serde_json::json!({
        "providers": {
            provider: {
                "name": provider_name,
                "baseUrl": base_url,
                "apiKey": bench_api_key(),
                "api": "openai-completions",
                "models": [{
                    "id": model_id,
                    "name": format!("bench {model_id}"),
                    "api": "openai-completions",
                    "baseUrl": base_url,
                    "reasoning": false,
                    "contextWindow": 32768,
                    "maxTokens": 8192,
                    "input": ["text"]
                }]
            }
        }
    }))
    .unwrap_or_default()
}

struct BenchEnv {
    root: PathBuf,
    agent_dir: PathBuf,
}

impl BenchEnv {
    fn create(model_id: &str) -> Result<BenchEnv, String> {
        let root = unique_temp_root();
        let agent_dir = root.join("agent");
        std::fs::create_dir_all(&agent_dir)
            .map_err(|error| format!("cannot create bench root {}: {error}", root.display()))?;
        std::fs::write(agent_dir.join("models.json"), models_json(model_id))
            .map_err(|error| format!("cannot write models.json: {error}"))?;
        Ok(BenchEnv { root, agent_dir })
    }

    fn task_dir(&self, id: &str, files: &[(&str, &str)]) -> Result<PathBuf, String> {
        let dir = self.root.join("tasks").join(id);
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("cannot create task dir {}: {error}", dir.display()))?;
        for (name, contents) in files {
            std::fs::write(dir.join(name), contents)
                .map_err(|error| format!("cannot write fixture {name}: {error}"))?;
        }
        Ok(dir)
    }
}

impl Drop for BenchEnv {
    fn drop(&mut self) {
        // Best effort: a lingering temp tree must not fail the benchmark.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// One completed CLI run (possibly failed / killed on timeout).
#[derive(Debug)]
struct RunResult {
    metrics: Metrics,
    timed_out: bool,
    exit_success: bool,
    stderr_tail: String,
}

/// Spawn the tool's JSON mode, stream-parse stdout with a timeout, kill on
/// overrun, and collect metrics.
fn run_json_mode(
    tool: Tool,
    model: &str,
    prompt: &str,
    cwd: &Path,
    use_tools: bool,
    env: &BenchEnv,
) -> RunResult {
    let provider = bench_provider();
    let mut command = match tool {
        Tool::Pir => {
            let pir_exe = std::env::var_os("PIR_BENCH_PIR_EXE")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_pir")));
            let mut command = Command::new(pir_exe);
            command.arg("print");
            command
                .args(["--mode", "json", "--no-session", "--no-context-files"])
                .args(["--no-extensions", "--approve"])
                .args([
                    "--provider",
                    provider.as_str(),
                    "--model",
                    model,
                    "--thinking",
                    "off",
                ]);
            if !use_tools {
                command.arg("--no-tools");
            }
            command.arg(prompt);
            command
        }
        Tool::Pi => {
            let mut command = Command::new(pi_program());
            command
                .args(["--mode", "json", "--no-session", "--no-context-files"])
                .args(["--no-extensions"])
                .args([
                    "--provider",
                    provider.as_str(),
                    "--model",
                    model,
                    "--thinking",
                    "off",
                ]);
            if !use_tools {
                command.arg("--no-tools");
            }
            // Upstream print mode: `-p` takes the message as its next arg.
            command.arg("-p").arg(prompt);
            command
        }
    };
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Both tools resolve the same agent dir -> same models.json.
        .env("PI_CODING_AGENT_DIR", &env.agent_dir)
        .env("PIR_AGENT_DIR", &env.agent_dir);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return RunResult {
                metrics: Metrics::default(),
                timed_out: false,
                exit_success: false,
                stderr_tail: format!("spawn failed: {error}"),
            }
        }
    };
    let started = Instant::now();

    // Both pipes were requested as `piped`, so they are always present.
    let Some(stdout_pipe) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return RunResult {
            metrics: Metrics::default(),
            timed_out: false,
            exit_success: false,
            stderr_tail: "spawn failed: stdout pipe missing".to_string(),
        };
    };
    let Some(mut stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return RunResult {
            metrics: Metrics::default(),
            timed_out: false,
            exit_success: false,
            stderr_tail: "spawn failed: stderr pipe missing".to_string(),
        };
    };

    // BufReader so `read_line` is available on the piped child stdout.
    let mut stdout = std::io::BufReader::new(stdout_pipe);

    let reader = std::thread::spawn(move || {
        let mut metrics = Metrics::default();
        let mut line = String::new();
        loop {
            line.clear();
            match stdout.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => absorb_line(&mut metrics, line.trim_end()),
            }
        }
        metrics
    });
    let err_reader = std::thread::spawn(move || {
        let mut collected = String::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if collected.len() < 8192 {
                        collected.push_str(&String::from_utf8_lossy(&chunk[..read]));
                    }
                }
            }
        }
        collected
    });

    let timeout = task_timeout();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if started.elapsed() >= timeout {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => break None,
        }
    };
    let wall_ms = started.elapsed().as_millis();

    let mut metrics = reader.join().unwrap_or_default();
    metrics.wall_ms = wall_ms;
    let stderr_tail: String = err_reader.join().unwrap_or_default();
    let mut tail = stderr_tail.trim().to_string();
    if tail.len() > 2000 {
        tail = tail[tail.len() - 2000..].to_string();
    }

    RunResult {
        metrics,
        timed_out,
        exit_success: status.is_some_and(|status| status.success()),
        stderr_tail: tail,
    }
}

// ---------------------------------------------------------------------------
// The benchmark entry point
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Attempt {
    ok: bool,
    detail: String,
    metrics: Metrics,
}

#[test]
#[ignore = "requires PIR_RUN_AGENT_BENCH=1, a reachable OpenAI-compatible endpoint, and the upstream pi CLI"]
fn agent_benchmark_pir_vs_pi() {
    if !bench_enabled() {
        eprintln!(
            "skipping agent benchmark: set PIR_RUN_AGENT_BENCH=1 (and start the endpoint) to run it"
        );
        return;
    }
    if !endpoint_available() {
        eprintln!(
            "skipping agent benchmark: endpoint not reachable (ollama 127.0.0.1:11434 or PIR_BENCH_BASE_URL)"
        );
        return;
    }
    if !pi_available() {
        eprintln!("skipping agent benchmark: upstream pi CLI not found (set PIR_BENCH_PI_BIN)");
        return;
    }
    let model = bench_model();
    let timeout = task_timeout();
    let env = match BenchEnv::create(&model) {
        Ok(env) => env,
        Err(error) => panic!("agent benchmark setup failed: {error}"),
    };

    println!("=== agent benchmark: pir vs pi ===");
    match bench_base_url() {
        Some(base) => println!("model: {model} (remote endpoint {base}, openai-completions)"),
        None => println!("model: {model} (ollama http://localhost:11434/v1, openai-completions)"),
    }
    println!("per-task timeout: {}s", timeout.as_secs());
    println!();

    let specs = task_specs();
    let tools = [Tool::Pir, Tool::Pi];
    let mut results: Vec<(String, Tool, Attempt)> = Vec::new();

    for spec in &specs {
        println!("--- {} : {} ---", spec.id, spec.title);
        let dir = match env.task_dir(spec.id, spec.files) {
            Ok(dir) => dir,
            Err(error) => panic!("fixture setup failed for {}: {error}", spec.id),
        };
        let prompt = (spec.prompt)(&dir);
        for tool in tools {
            print!("  {:<3} ... ", tool.label());
            let run = run_json_mode(tool, &model, &prompt, &dir, spec.use_tools, &env);
            let attempt = if run.timed_out {
                Attempt {
                    ok: false,
                    detail: format!("timed out after {}s", timeout.as_secs()),
                    metrics: run.metrics,
                }
            } else if !run.exit_success {
                let last_error = run.stderr_tail.lines().last().unwrap_or("").to_string();
                Attempt {
                    ok: false,
                    detail: format!("process failed (non-zero exit): {last_error}"),
                    metrics: run.metrics,
                }
            } else {
                match (spec.verify)(&dir, &run.metrics) {
                    Ok(()) => Attempt {
                        ok: true,
                        detail: "ok".to_string(),
                        metrics: run.metrics,
                    },
                    Err(reason) => Attempt {
                        ok: false,
                        detail: reason,
                        metrics: run.metrics,
                    },
                }
            };
            println!(
                "{} ({:.1}s)",
                if attempt.ok { "PASS" } else { "FAIL" },
                attempt.metrics.wall_ms as f64 / 1000.0
            );
            if !attempt.ok {
                println!("    reason: {}", attempt.detail);
            }
            results.push((spec.id.to_string(), tool, attempt));
        }
    }

    // Comparison table: timing and usage are REPORTED, never asserted.
    println!();
    println!(
        "{:<16} {:<4} {:<5} {:>9} {:>6} {:>7} {:>6} {:>8} {:>8} {:>8}",
        "task", "tool", "ok", "wall_s", "turns", "calls", "execs", "in_tok", "out_tok", "tot_tok"
    );
    for (id, tool, attempt) in &results {
        println!(
            "{:<16} {:<4} {:<5} {:>9.1} {:>6} {:>7} {:>6} {:>8} {:>8} {:>8}",
            id,
            tool.label(),
            if attempt.ok { "yes" } else { "no" },
            attempt.metrics.wall_ms as f64 / 1000.0,
            attempt.metrics.assistant_turns,
            attempt.metrics.tool_call_events,
            attempt.metrics.tool_executions,
            attempt.metrics.input_tokens,
            attempt.metrics.output_tokens,
            attempt.metrics.total_tokens,
        );
    }

    let score = |tool: Tool| -> usize {
        results
            .iter()
            .filter(|(_, candidate, attempt)| *candidate == tool && attempt.ok)
            .count()
    };
    let (pir_score, pi_score) = (score(Tool::Pir), score(Tool::Pi));
    let total = specs.len();
    println!();
    println!("parity gate: pir {pir_score}/{total}, pi {pi_score}/{total} (need >=4/{total} each)");
    println!(
        "compare: same model ({model}), same endpoint, same thinking level; \
timing is informational, correctness is the gate."
    );

    let failures: Vec<String> = results
        .iter()
        .filter(|(_, _, attempt)| !attempt.ok)
        .map(|(id, tool, attempt)| format!("{}[{}]: {}", id, tool.label(), attempt.detail))
        .collect();
    assert!(
        pir_score >= 4 && pi_score >= 4,
        "agent benchmark parity gate failed (both tools must complete >=4/{total} tasks):\n{}",
        failures.join("\n")
    );

    // Coarse sanity bound only: nothing here asserts on exact timing.
    for (_, tool, attempt) in &results {
        assert!(
            attempt.metrics.wall_ms as u64 <= timeout.as_millis() as u64 + 5_000,
            "{} run exceeded the {}s sanity bound",
            tool.label(),
            timeout.as_secs()
        );
    }
}
