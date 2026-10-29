//! Runtime contract for feature-slim `pir` binaries.
//!
//! Standalone target: `std` only (no `pir::` imports), so it compiles for every
//! slim profile. `#![cfg(feature = "cli")]` keeps it empty where the binary
//! target does not exist (`[[bin]] required-features = ["cli"]`).
//!
//! `scripts/feature_matrix.sh` runs it as the smoke phase for the seven bin
//! profiles: `cli` (the real toolless binary), `cli,tool-read` (the partial
//! build), `cli,tools`, `cli,mcp`, `cli,provider-openai`,
//! `cli,tui,provider-openai`, `full`. Each run pins:
//!
//! - `--version` is exactly `CARGO_PKG_VERSION` (no build metadata, no suffix);
//! - the exact missing-feature text for every surface this profile lacks (the
//!   `full` row lacks none, so it only runs the version + print checks);
//! - a print-mode turn against a local OpenAI-SSE mock when a wire adapter is
//!   compiled in, or the documented missing-provider error for `cli` alone;
//! - the explicit `--tools <compiled-out>` diagnostic: `grep` without
//!   `grep-find`, and every core tool whose per-tool leaf (`tool-read`,
//!   `tool-bash`, …) is off reports that leaf (the slim replacement for the
//!   old `g_tools`-based matrix row, which cannot compile without
//!   `tui`/`extensions`). The `cli,tool-read` profile also pins the other
//!   direction: an enabled leaf is never diagnosed.
//!
//! Everything runs in a temp `PIR_AGENT_DIR` / session dir / cwd with offline
//! mode on, so the host's `~/.pi` state is never read or written.

#![cfg(feature = "cli")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Per-test temp agent/cwd directory.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-feature-off-{label}-{}-{}",
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
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The `pir` binary under test, isolated from the host's real agent state.
fn pir(agent_dir: &Path, cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .env("PIR_AGENT_DIR", agent_dir)
        // The upstream name wins over `PIR_SESSION_DIR` in the config resolver,
        // so set both: an inherited value must not let sessions escape the
        // per-test temp dir.
        .env("PI_CODING_AGENT_SESSION_DIR", agent_dir.join("sessions"))
        .env("PIR_SESSION_DIR", agent_dir.join("sessions"))
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env_remove("PIR_TRUST_PROJECT")
        .current_dir(cwd);
    command
}

fn run(agent: &TempDir, cwd: &TempDir, args: &[&str]) -> Output {
    pir(&agent.0, &cwd.0)
        .args(args)
        .output()
        .expect("pir binary should start")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Pin the exact "this build was built without the `X` feature" diagnostic.
/// (Absent in `full`, where every surface below is compiled in.)
#[cfg(any(
    not(feature = "tui"),
    not(feature = "extensions"),
    not(feature = "server"),
    not(feature = "mcp"),
    not(feature = "mcp-sd"),
    not(feature = "client"),
    not(feature = "env-daemon")
))]
fn assert_missing_feature(output: &Output, command: &str, feature: &str) {
    let stderr = text(&output.stderr);
    assert!(
        !output.status.success(),
        "`{command}` must fail in a profile without `{feature}`; it succeeded"
    );
    assert_eq!(
        output.status.code(),
        Some(1),
        "missing-feature errors exit 1 (`{command}`); stderr:\n{stderr}"
    );
    let expected =
        format!("`{command}` is unavailable: this build was built without the `{feature}` feature");
    assert!(
        stderr.contains(&expected),
        "expected `{expected}` for `{command}`; stderr:\n{stderr}"
    );
}

#[test]
fn version_is_exactly_the_package_version() {
    let agent = TempDir::new("version-agent");
    let cwd = TempDir::new("version-cwd");
    let output = run(&agent, &cwd, &["--version"]);
    assert!(
        output.status.success(),
        "--version failed: {}\n{}",
        output.status,
        text(&output.stderr)
    );
    assert_eq!(
        text(&output.stdout).trim(),
        env!("CARGO_PKG_VERSION"),
        "--version prints exactly the crate version"
    );
}

// ---------------------------------------------------------------------------
// Off-surface diagnostics: one process test per compiled-out surface.
// ---------------------------------------------------------------------------

#[cfg(not(feature = "tui"))]
#[test]
fn interactive_reports_missing_tui() {
    let agent = TempDir::new("tui-agent");
    let cwd = TempDir::new("tui-cwd");
    let output = run(&agent, &cwd, &["interactive"]);
    assert_missing_feature(&output, "interactive mode", "tui");
}

#[cfg(not(feature = "extensions"))]
#[test]
fn js_reports_missing_extensions() {
    let agent = TempDir::new("ext-agent");
    let cwd = TempDir::new("ext-cwd");
    let output = run(&agent, &cwd, &["js", "probe.js"]);
    assert_missing_feature(&output, "pir js", "extensions");
}

#[cfg(not(feature = "server"))]
#[test]
fn server_reports_missing_server() {
    let agent = TempDir::new("server-agent");
    let cwd = TempDir::new("server-cwd");
    let output = run(&agent, &cwd, &["server"]);
    assert_missing_feature(&output, "pir server", "server");
}

/// The `client` feature gates `pir::client`; without it the command must parse
/// and report the missing feature instead of failing to compile.
#[cfg(not(feature = "client"))]
#[test]
fn client_reports_missing_client() {
    let agent = TempDir::new("client-agent");
    let cwd = TempDir::new("client-cwd");
    let output = run(&agent, &cwd, &["client"]);
    assert_missing_feature(&output, "pir client", "client");
}

#[cfg(not(feature = "env-daemon"))]
#[test]
fn env_daemon_reports_missing_env_daemon() {
    let agent = TempDir::new("env-daemon-off-agent");
    let cwd = TempDir::new("env-daemon-off-cwd");
    let output = run(&agent, &cwd, &["env-daemon", "--version"]);
    assert_missing_feature(&output, "pir env-daemon", "env-daemon");
}

#[cfg(not(feature = "mcp"))]
#[test]
fn mcp_sd_reports_missing_mcp() {
    let agent = TempDir::new("mcp-agent");
    let cwd = TempDir::new("mcp-cwd");
    let output = run(&agent, &cwd, &["mcp-sd"]);
    assert_missing_feature(&output, "pir mcp-sd", "mcp");
}

/// The `mcp`-without-`mcp-sd` arm: `src/main_cli/run_mode.rs` must not reference
/// `pir::mcp::server` (gated on `mcp-sd`) from a `mcp`-only gate.
#[cfg(all(feature = "mcp", not(feature = "mcp-sd")))]
#[test]
fn mcp_sd_reports_missing_mcp_sd() {
    let agent = TempDir::new("mcp-sd-agent");
    let cwd = TempDir::new("mcp-sd-cwd");
    let output = run(&agent, &cwd, &["mcp-sd"]);
    assert_missing_feature(&output, "pir mcp-sd", "mcp-sd");
}

// ---------------------------------------------------------------------------
// `env-daemon` on-arm: the feature build answers `--version` and serves its
// sync line (the matrix `bin(cli,env-daemon)` row compile-checks the arm; the
// `smoke(full)` row runs this test because `full` carries `env-daemon`).
// ---------------------------------------------------------------------------

#[cfg(feature = "env-daemon")]
#[test]
fn env_daemon_serve_starts_and_ends_cleanly() {
    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    let agent = TempDir::new("env-daemon-agent");
    let cwd = TempDir::new("env-daemon-cwd");

    let version = run(&agent, &cwd, &["env-daemon", "--version"]);
    assert!(
        version.status.success(),
        "`pir env-daemon --version` failed: {}\n{}",
        version.status,
        text(&version.stderr)
    );
    assert_eq!(
        text(&version.stdout).trim(),
        format!("pi-env {}", env!("CARGO_PKG_VERSION")),
        "the daemon reports its package version on the documented line"
    );

    let mut child = pir(&agent.0, &cwd.0)
        .args(["env-daemon", "serve", "--token", TOKEN])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn pir env-daemon serve");
    let stdin = child.stdin.take().expect("daemon stdin");
    let mut stdout = child.stdout.take().expect("daemon stdout");
    // The first protocol line is `PI-ENV <token>`; tolerate transport noise
    // before it exactly as `tests/env_daemon_protocol.rs` does.
    let marker = format!("PI-ENV {TOKEN}\n");
    let mut seen = String::new();
    let mut byte = [0u8; 1];
    while !seen.ends_with(marker.as_str()) {
        let read = stdout.read(&mut byte).expect("read the sync line");
        assert_ne!(read, 0, "daemon ended before its sync line: {seen:?}");
        seen.push(byte[0] as char);
        assert!(seen.len() < 4096, "no sync line from the daemon: {seen:?}");
    }
    // A clean stdin end is a clean daemon end (exit 0); no frames required.
    drop(stdin);
    let status = child.wait().expect("wait for the daemon");
    assert!(
        status.success(),
        "a clean stdin end must exit 0, got {status}"
    );
}

// ---------------------------------------------------------------------------
// Explicit `--tools <compiled-out>` diagnostics (Q2b, slim replacement for the
// old `g_tools`-based matrix row that cannot compile without `tui`/
// `extensions`).
// ---------------------------------------------------------------------------

/// A print run whose selection/loadout is reached, but whose wire adapter is
/// compiled out in every profile that lacks `grep-find` (`cli`, `cli,mcp`,
/// `cli,provider-openai`, `cli,tui,provider-openai`). The built-in catalog
/// model plus an inline `--api-key` avoids `models.json`: a custom provider
/// would fail construction inside `load_pi_environment` before the loadout,
/// and an anthropic model has no adapter in any of those profiles, so the
/// process stops without a network request.
#[cfg(not(feature = "grep-find"))]
fn run_with_compiled_out_tool(agent: &TempDir, cwd: &TempDir, tool: &str) -> Output {
    pir(&agent.0, &cwd.0)
        .args([
            "--tools",
            tool,
            "--print",
            "hi",
            "--only-builtin",
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet-4-5",
            "--api-key",
            "test-key",
            "--offline",
        ])
        .output()
        .expect("pir binary should start")
}

/// An explicit `--tools grep` in a build without `grep-find` must name the
/// missing surface instead of silently resolving to an empty selection.
#[cfg(not(feature = "grep-find"))]
#[test]
fn explicit_compiled_out_tool_names_are_diagnosed() {
    let agent = TempDir::new("tool-diag-agent");
    let cwd = TempDir::new("tool-diag-cwd");
    let output = run_with_compiled_out_tool(&agent, &cwd, "grep");
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains(
            "pir: tool `grep` is unavailable: this build was compiled without the `grep-find` feature"
        ),
        "expected the compiled-out `grep` diagnostic; stderr:\n{stderr}"
    );
}

/// An explicit `--tools <core-name>` in a build whose per-tool leaf is off
/// must name that leaf, not the aggregate `tools` feature. The test runs in
/// the no-`grep-find` profile shapes, where the helper above (which expects
/// the provider adapter to be compiled out) is available.
#[cfg(all(
    not(feature = "grep-find"),
    any(
        not(feature = "tool-read"),
        not(feature = "tool-bash"),
        not(feature = "tool-powershell"),
        not(feature = "tool-edit"),
        not(feature = "tool-write"),
        not(feature = "tool-ls"),
        not(feature = "tool-background")
    )
))]
#[test]
fn explicit_core_tools_name_their_leaf_feature() {
    // (tool name, leaf feature, compiled in?) — the `cli` row has all seven
    // off; `cli,tool-read` has exactly `read` on.
    let cases: [(&str, &str, bool); 8] = [
        ("read", "tool-read", cfg!(feature = "tool-read")),
        ("bash", "tool-bash", cfg!(feature = "tool-bash")),
        (
            "powershell",
            "tool-powershell",
            cfg!(feature = "tool-powershell"),
        ),
        ("edit", "tool-edit", cfg!(feature = "tool-edit")),
        ("write", "tool-write", cfg!(feature = "tool-write")),
        ("ls", "tool-ls", cfg!(feature = "tool-ls")),
        (
            "background_status",
            "tool-background",
            cfg!(feature = "tool-background"),
        ),
        (
            "background_abort",
            "tool-background",
            cfg!(feature = "tool-background"),
        ),
    ];
    for (tool, leaf, enabled) in cases {
        if enabled {
            continue;
        }
        let agent = TempDir::new("tool-leaf-diag-agent");
        let cwd = TempDir::new("tool-leaf-diag-cwd");
        let output = run_with_compiled_out_tool(&agent, &cwd, tool);
        let stderr = text(&output.stderr);
        let expected = format!(
            "pir: tool `{tool}` is unavailable: this build was compiled without the `{leaf}` feature"
        );
        assert!(
            stderr.contains(&expected),
            "expected `{expected}`; stderr:\n{stderr}"
        );
    }
}

/// The partial-build counterpart: an enabled leaf is available, so `--tools
/// read` in `cli,tool-read` reaches the wire-adapter construction with no
/// tool diagnostic at all.
#[cfg(all(
    feature = "tool-read",
    not(feature = "grep-find"),
    not(feature = "provider-anthropic")
))]
#[test]
fn enabled_leaf_is_not_diagnosed() {
    let agent = TempDir::new("tool-leaf-on-agent");
    let cwd = TempDir::new("tool-leaf-on-cwd");
    let output = run_with_compiled_out_tool(&agent, &cwd, "read");
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("compiled without the `provider-anthropic` feature"),
        "the run must reach provider construction after the loadout; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("pir: tool `read`"),
        "an enabled leaf must not be diagnosed; stderr:\n{stderr}"
    );
}

/// A genuinely unknown explicit name stays silent (upstream drops it); only a
/// known surface whose cargo feature is off is diagnosed.
#[cfg(all(not(feature = "grep-find"), not(feature = "provider-anthropic")))]
#[test]
fn unknown_explicit_tool_names_stay_silent() {
    let agent = TempDir::new("tool-unknown-agent");
    let cwd = TempDir::new("tool-unknown-cwd");
    let output = run_with_compiled_out_tool(&agent, &cwd, "definitely-not-a-tool");
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("compiled without the `provider-anthropic` feature"),
        "the run must reach provider construction after the loadout; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("pir: tool "),
        "unknown names must not produce a tool diagnostic; stderr:\n{stderr}"
    );
}

/// Q66: the RPC host resolves the same request through the shared resolver, so
/// an explicit `--tools <compiled-out>` names the missing surface on stderr
/// exactly like print/TUI — before any protocol frame. `Command::output()`
/// closes stdin, so the RPC loop ends cleanly and no provider is needed.
#[cfg(not(feature = "tool-read"))]
#[test]
fn rpc_reports_the_compiled_out_tool_diagnostic() {
    let agent = TempDir::new("rpc-tool-off-agent");
    let cwd = TempDir::new("rpc-tool-off-cwd");
    let output = run(
        &agent,
        &cwd,
        &[
            "rpc",
            "--tools",
            "read",
            "--only-builtin",
            "--no-mcp",
            "--offline",
        ],
    );
    let stderr = text(&output.stderr);
    assert!(
        output.status.success(),
        "rpc must exit cleanly on closed stdin; stderr:\n{stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "no protocol frames before a request; stdout:\n{}",
        text(&output.stdout)
    );
    assert!(
        stderr.contains(
            "pir: tool `read` is unavailable: this build was compiled without the `tool-read` feature"
        ),
        "expected the compiled-out `read` diagnostic; stderr:\n{stderr}"
    );
}

// ---------------------------------------------------------------------------
// Print-mode smoke: local OpenAI-compatible SSE mock, no network.
// ---------------------------------------------------------------------------

/// One OpenAI-compatible SSE completion: "Hello" then a stop chunk.
fn mock_sse_body() -> &'static str {
    concat!(
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    )
}

/// Serve every connection on `listener` with the fixed SSE body (the test
/// process is short-lived; the thread dies with it).
fn spawn_mock_openai() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock listener");
    let port = listener.local_addr().expect("mock addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            let header_end = loop {
                if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break Some(index + 4);
                }
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break None,
                    Ok(read) => request.extend_from_slice(&chunk[..read]),
                }
            };
            let Some(header_end) = header_end else {
                continue;
            };
            // Drain the request body so closing the socket does not reset the
            // response before the client reads it.
            let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&chunk[..read]),
                }
            }
            let body = mock_sse_body();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    port
}

fn write_models_json(agent_dir: &Path, port: u16) {
    let models = format!(
        r#"{{
  "providers": {{
    "mock": {{
      "name": "Mock",
      "baseUrl": "http://127.0.0.1:{port}/v1",
      "api": "openai-completions",
      "apiKey": "test-key",
      "models": [
        {{
          "id": "mock-model",
          "name": "Mock model",
          "reasoning": false,
          "input": ["text"],
          "contextWindow": 100000,
          "maxTokens": 1024
        }}
      ]
    }}
  }}
}}"#
    );
    std::fs::write(agent_dir.join("models.json"), models).expect("write models.json");
}

fn run_print(agent: &TempDir, cwd: &TempDir) -> Output {
    pir(&agent.0, &cwd.0)
        .args([
            "print",
            "hi",
            "--only-builtin",
            "--provider",
            "mock",
            "--model",
            "mock-model",
            "--offline",
        ])
        .output()
        .expect("pir binary should start")
}

#[cfg(feature = "provider-openai")]
#[test]
fn print_streams_one_mock_turn() {
    let port = spawn_mock_openai();
    let agent = TempDir::new("print-agent");
    let cwd = TempDir::new("print-cwd");
    write_models_json(&agent.0, port);

    let output = run_print(&agent, &cwd);
    assert!(
        output.status.success(),
        "print against the local mock failed: {}\n{}",
        output.status,
        text(&output.stderr)
    );
    assert_eq!(
        text(&output.stdout).trim(),
        "Hello",
        "the mock reply is the print-mode result"
    );
}

/// No wire adapter is compiled in: the run must name the missing provider
/// feature, never fall back silently.
#[cfg(not(feature = "provider-openai"))]
#[test]
fn print_names_the_missing_provider_adapter() {
    let port = spawn_mock_openai();
    let agent = TempDir::new("print-off-agent");
    let cwd = TempDir::new("print-off-cwd");
    write_models_json(&agent.0, port);

    let output = run_print(&agent, &cwd);
    let stderr = text(&output.stderr);
    let stdout = text(&output.stdout);
    assert!(
        !output.status.success(),
        "print must fail without a wire adapter; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("compiled without the `provider-openai` feature"),
        "the missing adapter must name `provider-openai`; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("openai-completions"),
        "the missing adapter names the model's api; stderr:\n{stderr}"
    );
}
