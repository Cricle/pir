//! Lane EVENT3 regression: the `--mode json` headless host must use the same
//! extension runtime config as its `print` sibling, and the parked strict
//! validation runtime must be consumed instead of dropped.
//!
//! The sequence is pinned end-to-end through the real `pir` binary against a
//! local OpenAI-compatible mock (no network/model):
//!
//! - text print and `--mode json` both observe
//!   `project_trust, session_start, ..., session_shutdown`;
//! - no startup-time `session_shutdown` is emitted while the CLI is still
//!   validating extensions;
//! - the extension factory runs exactly twice with the pre-trust bootstrap
//!   enabled (bootstrap pass + final validation load) and only once without
//!   it — the JSON host no longer creates a third runtime because its
//!   `"json"` config matches the parked runtime.
//! - the no-trust-requiring-resources path stays bootstrap-free (no
//!   `project_trust`, one factory).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Output;

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        // Use a neutral Windows root when the process temp dir lives under a
        // developer profile: the trust scan walks every ancestor and would
        // otherwise find the user's real `.agents/skills`.
        let base = if cfg!(windows) {
            let public = PathBuf::from("C:\\Users\\Public");
            if public.is_dir() {
                public
            } else {
                std::env::temp_dir()
            }
        } else {
            std::env::temp_dir()
        };
        let path = base.join(format!(
            "pir-lane-event3-{label}-{}-{}",
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
          "api": "openai-completions",
          "baseUrl": "http://127.0.0.1:{port}/v1",
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

fn read_lines(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("read {}: {error}", path.display()),
    }
}

/// Write an extension that records every factory evaluation, the three
/// lifecycle events this lane cares about, and whether the SAME runtime
/// instance that served `project_trust` also serves `session_start`.
fn write_logging_extension(path: &Path, events_log: &Path, factories_log: &Path) {
    let events = events_log.display().to_string();
    let factories = factories_log.display().to_string();
    std::fs::write(
        path,
        format!(
            r#"import fs from "node:fs";
const EVENTS = {events:?};
const FACTORIES = {factories:?};
let sawProjectTrust = false;
export default function (pi) {{
  fs.appendFileSync(FACTORIES, "factory\n");
  pi.on("project_trust", () => {{
    sawProjectTrust = true;
    fs.appendFileSync(EVENTS, "project_trust\n");
    return {{ trusted: true }};
  }});
  pi.on("session_start", (event) => {{
    fs.appendFileSync(
      EVENTS,
      "session_start:" + (event && event.reason ? event.reason : "") +
        ":" + (sawProjectTrust ? "trust-handler-state" : "fresh-runtime") + "\n",
    );
  }});
  pi.on("session_shutdown", (event) => {{
    fs.appendFileSync(EVENTS, "session_shutdown:" + (event && event.reason ? event.reason : "") + "\n");
  }});
}}
"#
        ),
    )
    .expect("write logging extension");
}

fn run_host(
    agent_dir: &Path,
    home: &Path,
    project_dir: &Path,
    extension: &Path,
    json: bool,
    extra_args: &[&str],
) -> Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_pir"));
    if json {
        command.args(["--mode", "json"]);
    } else {
        command.arg("print");
    }
    command
        .arg("hi")
        .args([
            "-e",
            &extension.display().to_string(),
            "--provider",
            "mock",
            "--model",
            "mock-model",
            "--offline",
            "--no-session",
            "--no-context-files",
        ])
        .args(extra_args)
        .env("PIR_AGENT_DIR", agent_dir)
        .env("PIR_SESSION_DIR", agent_dir.join("sessions"))
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("PIR_TRUST_PROJECT")
        .env_remove("PI_TIMING")
        .env_remove("PI_STARTUP_BENCHMARK")
        .current_dir(project_dir)
        .output()
        .expect("pir binary should start")
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Fixture {
    root: TempDir,
    agent: PathBuf,
    home: PathBuf,
    project: PathBuf,
    extension: PathBuf,
    events: PathBuf,
    factories: PathBuf,
}

impl Fixture {
    fn new(label: &str, port: u16, trust_requiring_resources: bool) -> Self {
        let root = TempDir::new(label);
        let agent = root.0.join("agent");
        let home = root.0.join("home");
        let project = root.0.join("project");
        std::fs::create_dir_all(&agent).expect("agent dir");
        std::fs::create_dir_all(&home).expect("home dir");
        std::fs::create_dir_all(&project).expect("project dir");
        if trust_requiring_resources {
            // A project `.pi/settings.json` marks the project as
            // trust-requiring without contributing any extension path: the
            // trusted final plan stays byte-identical to the bootstrap plan,
            // which is the upstream case where `preTrustExtensions.runtime`
            // can be adopted as the live runtime.
            std::fs::create_dir_all(project.join(".pi")).expect("project config dir");
            std::fs::write(project.join(".pi").join("settings.json"), "{}")
                .expect("project settings");
        }
        write_models_json(&agent, port);
        let extension = project.join("lane_event3_ext.ts");
        let events = project.join("events.log");
        let factories = project.join("factories.log");
        write_logging_extension(&extension, &events, &factories);
        Self {
            root,
            agent,
            home,
            project,
            extension,
            events,
            factories,
        }
    }
}

#[test]
fn json_and_print_hosts_emit_the_same_lifecycle_sequence_and_factory_count() {
    let port = spawn_mock_openai();
    let fixture = Fixture::new("compare", port, true);

    let text = run_host(
        &fixture.agent,
        &fixture.home,
        &fixture.project,
        &fixture.extension,
        false,
        &[],
    );
    assert_success(&text, "text print");
    let text_events = read_lines(&fixture.events);
    let text_factories = read_lines(&fixture.factories);

    std::fs::remove_file(&fixture.events).ok();
    std::fs::remove_file(&fixture.factories).ok();

    let json = run_host(
        &fixture.agent,
        &fixture.home,
        &fixture.project,
        &fixture.extension,
        true,
        &[],
    );
    assert_success(&json, "json print");
    let json_events = read_lines(&fixture.events);
    let json_factories = read_lines(&fixture.factories);

    let expected = vec![
        "project_trust".to_owned(),
        "session_start:startup:trust-handler-state".to_owned(),
        "session_shutdown:quit".to_owned(),
    ];
    assert_eq!(
        text_events, expected,
        "text print must emit project_trust once, session_start once from the \
         SAME runtime that observed it, and no startup shutdown"
    );
    assert_eq!(
        json_events, expected,
        "`--mode json` must match the text host's lifecycle sequence (no startup shutdown)"
    );
    assert_eq!(
        text_factories.len(),
        2,
        "bootstrap + final strict load evaluate the factory twice: {text_factories:?}"
    );
    assert_eq!(
        json_factories.len(),
        2,
        "`--mode json` must reuse the parked runtime, not evaluate a third factory: {json_factories:?}"
    );
    drop(fixture.root);
}

#[test]
fn projects_without_trust_requiring_resources_skip_the_bootstrap_pass() {
    let port = spawn_mock_openai();
    let fixture = Fixture::new("skip", port, false);

    // `has_trust_requiring_project_resources` also scans ancestor
    // `.agents/skills` dirs and skips only the one under HOME. The fixture
    // root is a neutral temp dir (see `TempDir::new`), so HOME=root and no
    // ancestor carries `.agents/skills`: the project has no trust-requiring
    // resources (the branch this test pins).
    let ancestor_home = fixture.root.0.clone();
    let json = run_host(
        &fixture.agent,
        &ancestor_home,
        &fixture.project,
        &fixture.extension,
        true,
        &[],
    );
    assert_success(&json, "json print (no trust resources)");
    assert_eq!(
        read_lines(&fixture.events),
        vec![
            "session_start:startup:fresh-runtime".to_owned(),
            "session_shutdown:quit".to_owned(),
        ],
        "no trust-requiring resources must not dispatch project_trust"
    );
    assert_eq!(
        read_lines(&fixture.factories).len(),
        1,
        "without a bootstrap pass the final load runs the factory once"
    );
    drop(fixture.root);
}

#[test]
fn explicit_approve_override_skips_the_bootstrap_pass() {
    let port = spawn_mock_openai();
    let fixture = Fixture::new("approve", port, true);

    let json = run_host(
        &fixture.agent,
        &fixture.home,
        &fixture.project,
        &fixture.extension,
        true,
        &["--approve"],
    );
    assert_success(&json, "json print (--approve)");
    assert_eq!(
        read_lines(&fixture.events),
        vec![
            "session_start:startup:fresh-runtime".to_owned(),
            "session_shutdown:quit".to_owned(),
        ],
        "a CLI trust override must skip the pre-trust bootstrap pass"
    );
    assert_eq!(
        read_lines(&fixture.factories).len(),
        1,
        "the override path loads/validates/consumes one runtime"
    );
    drop(fixture.root);
}
