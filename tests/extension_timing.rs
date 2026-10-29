//! `PI_TIMING=1` extension/boot phase instrumentation.
//!
//! The flag must emit one `pir-timing: <phase>: <ms>ms` line per phase and
//! change nothing else. The test runs the real binary against a local
//! OpenAI-compatible mock (no network) with `--only-builtin`, so every phase
//! of the fast path is exercised end-to-end:
//!
//! - `ext.runtime.plan_only` (and NOT `ext.runtime.start` / `ext.runtime.load`)
//! - `plugins.host.build`
//! - `prompt.system_prompt_build`
//! - `loadout.first_build`
//! - `provider.first_request_send`
//! - `provider.first_stream_event`
//! - `ext.event.session_start`
//!
//! The same command without `PIR_TIMING` must produce the same successful
//! result and no `pir-timing:` lines.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-timing-{label}-{}-{}",
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

fn run_print(agent_dir: &Path, cwd: &Path, timing: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .env("PIR_AGENT_DIR", agent_dir)
        .env("PIR_SESSION_DIR", agent_dir.join("sessions"))
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env_remove("PIR_TRUST_PROJECT")
        .env_remove("PI_TIMING")
        .env_remove("PI_STARTUP_BENCHMARK")
        .current_dir(cwd)
        .args([
            "print",
            "hi",
            "--only-builtin",
            "--provider",
            "mock",
            "--model",
            "mock-model",
            "--offline",
        ]);
    if timing {
        command.env("PI_TIMING", "1");
    }
    command.output().expect("pir binary should start")
}

fn assert_phase(stderr: &str, phase: &str) {
    let marker = format!("pir-timing: {phase}:");
    assert!(
        stderr.contains(&marker),
        "missing timing phase `{phase}` in stderr:\n{stderr}"
    );
}

#[test]
fn timing_flag_emits_phase_names_without_changing_behavior() {
    let port = spawn_mock_openai();
    let agent = TempDir::new("agent");
    let project = TempDir::new("project");
    write_models_json(&agent.0, port);

    let timed = run_print(&agent.0, &project.0, true);
    let timed_stdout = String::from_utf8_lossy(&timed.stdout).into_owned();
    let timed_stderr = String::from_utf8_lossy(&timed.stderr).into_owned();
    assert!(
        timed.status.success(),
        "timed run failed: {}\n{}",
        timed.status,
        timed_stderr
    );
    assert_eq!(timed_stdout.trim(), "Hello");

    for phase in [
        "ext.runtime.plan_only",
        "plugins.host.build",
        "prompt.system_prompt_build",
        "loadout.first_build",
        "provider.first_request_send",
        "provider.first_stream_event",
        "ext.event.session_start",
    ] {
        assert_phase(&timed_stderr, phase);
    }
    // The fast path never starts the QuickJS actor or loads on-disk JS.
    assert!(
        !timed_stderr.contains("pir-timing: ext.runtime.start:"),
        "--only-builtin must not start the JS actor:\n{timed_stderr}"
    );
    assert!(
        !timed_stderr.contains("pir-timing: ext.runtime.load:"),
        "--only-builtin must not load on-disk JS:\n{timed_stderr}"
    );

    let plain = run_print(&agent.0, &project.0, false);
    assert!(
        plain.status.success(),
        "untimed run failed: {}\n{}",
        plain.status,
        String::from_utf8_lossy(&plain.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&plain.stdout).trim(),
        "Hello",
        "timing must not change the printed result"
    );
    assert!(
        !String::from_utf8_lossy(&plain.stderr).contains("pir-timing:"),
        "timing lines must be gated on PI_TIMING=1"
    );
}

/// `print` run with one on-disk extension (`-e`), used for the per-handler
/// timing lines. The extension has a `session_start` + `input` handler; the
/// input handler rewrites the prompt with a marker the log must never show.
fn run_print_with_extension(
    agent_dir: &Path,
    cwd: &Path,
    extension: &Path,
    timing: bool,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pir"));
    command
        .env("PIR_AGENT_DIR", agent_dir)
        .env("PIR_SESSION_DIR", agent_dir.join("sessions"))
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env_remove("PIR_TRUST_PROJECT")
        .env_remove("PI_TIMING")
        .env_remove("PI_STARTUP_BENCHMARK")
        .current_dir(cwd)
        .args([
            "print",
            "hi",
            "-e",
            &extension.display().to_string(),
            "--provider",
            "mock",
            "--model",
            "mock-model",
            "--offline",
        ]);
    if timing {
        command.env("PI_TIMING", "1");
    }
    command.output().expect("pir binary should start")
}

#[test]
fn per_handler_timing_is_gated_and_carries_no_payload() {
    let port = spawn_mock_openai();
    let agent = TempDir::new("handler-agent");
    let project = TempDir::new("handler-project");
    write_models_json(&agent.0, port);

    let extension = project.0.join("handler-timing.js");
    const SECRET: &str = "PAYLOAD_SECRET_9f3c";
    std::fs::write(
        &extension,
        format!(
            r#"
export default function (pi) {{
    pi.on("session_start", () => {{}});
    pi.on("input", (event) => ({{
        action: "transform",
        text: "{SECRET}:" + event.text,
    }}));
}}
"#
        ),
    )
    .expect("write extension fixture");

    let timed = run_print_with_extension(&agent.0, &project.0, &extension, true);
    let stderr = String::from_utf8_lossy(&timed.stderr).into_owned();
    assert!(
        timed.status.success(),
        "timed handler run failed: {}\n{stderr}",
        timed.status
    );
    for phase in [
        "pir-timing: ext.handler.session_start:",
        "pir-timing: ext.handler.input:",
    ] {
        assert!(
            stderr.contains(phase),
            "missing per-handler timing line `{phase}` in stderr:\n{stderr}"
        );
    }
    assert!(
        stderr.contains("plugin=") && stderr.contains("handler-timing.js"),
        "the timing line must carry the sanitized extension identity:\n{stderr}"
    );
    // Sanitized: no absolute fixture path, no prompt/payload text.
    assert!(
        !stderr.contains(&extension.display().to_string()),
        "the timing line must not print the full extension path:\n{stderr}"
    );
    assert!(
        !stderr.contains(SECRET),
        "the timing line must never contain payload text:\n{stderr}"
    );

    let plain = run_print_with_extension(&agent.0, &project.0, &extension, false);
    let plain_stderr = String::from_utf8_lossy(&plain.stderr).into_owned();
    assert!(
        plain.status.success(),
        "untimed handler run failed: {}\n{plain_stderr}",
        plain.status
    );
    assert!(
        !plain_stderr.contains("pir-timing: ext.handler."),
        "per-handler timing must be gated on PI_TIMING=1:\n{plain_stderr}"
    );
}
