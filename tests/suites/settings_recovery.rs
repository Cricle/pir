//! PIR-BIN-071 regression: a malformed / unreadable / non-object
//! `settings.json` must NOT abort a run.
//!
//! The settings manager records such a parse failure as a non-fatal
//! diagnostic and keeps the defaults for that scope (its own docs, upstream
//! `SettingsManager.create`), and the CLI prints
//! `Warning: Invalid settings file <path>: …` for it. The provider-facing
//! loader (`ai::providers::settings::load_settings` →
//! `load_pi_environment` / `load_pi_config_with`) used to convert those
//! recorded diagnostics into `AiError::Authentication`, so `pir print`, `rpc`
//! and `models` exited 1 with `Error: authentication failure: global
//! settings: …` and no model request was ever issued.
//!
//! These tests drive the REAL `pir` binary through the whole user-visible
//! outcome — the run proceeds (the model request happens, the answer is
//! printed, the exit code is 0) AND the warning is still emitted — because
//! "the error type changed" is not the contract: the user's typo must degrade
//! to defaults, not stop the agent.
//!
//! Every arm gets its own hermetic agent dir + project dir. The provider is a
//! loopback OpenAI-compatible mock; nothing touches the real `~/.pi`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Loopback mock provider (OpenAI chat-completions SSE, one text reply)
// ---------------------------------------------------------------------------

/// Serve `reply` as one OpenAI-compatible SSE completion per request, and
/// record every request body so a test can prove the model was really called.
fn spawn_mock(reply: &'static str) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock provider");
    let port = listener.local_addr().expect("mock local addr").port();
    let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let bodies_thread = Arc::clone(&bodies);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut request = Vec::new();
            let mut buffer = [0u8; 8192];
            let header_end = loop {
                if let Some(index) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(index + 4);
                }
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break None,
                    Ok(read) => request.extend_from_slice(&buffer[..read]),
                }
            };
            let Some(header_end) = header_end else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while request.len() < header_end + length {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&buffer[..read]),
                }
            }
            bodies_thread
                .lock()
                .expect("mock bodies")
                .push(String::from_utf8_lossy(&request[header_end..]).to_string());

            let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
                serde_json::json!({
                    "id": "chatcmpl-settings-mock",
                    "object": "chat.completion.chunk",
                    "created": 1,
                    "model": "mock-model",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
                })
            };
            let mut events = String::new();
            events.push_str(&format!(
                "data: {}\n\n",
                chunk(
                    serde_json::json!({"role": "assistant", "content": ""}),
                    serde_json::Value::Null
                )
            ));
            events.push_str(&format!(
                "data: {}\n\n",
                chunk(
                    serde_json::json!({"content": reply}),
                    serde_json::Value::Null
                )
            ));
            let mut final_chunk = chunk(serde_json::json!({}), serde_json::json!("stop"));
            final_chunk["usage"] = serde_json::json!({
                "prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12,
            });
            events.push_str(&format!("data: {final_chunk}\n\n"));
            events.push_str("data: [DONE]\n\n");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                events.len(),
                events
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    (port, bodies)
}

// ---------------------------------------------------------------------------
// Hermetic agent/project dirs
// ---------------------------------------------------------------------------

static SEQ: AtomicUsize = AtomicUsize::new(0);

struct Hermetic {
    root: PathBuf,
}

impl Hermetic {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!(
            "pir-settings-recovery-{label}-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("agent")).expect("create agent dir");
        std::fs::create_dir_all(root.join("project")).expect("create project dir");
        Self { root }
    }

    fn agent_dir(&self) -> PathBuf {
        self.root.join("agent")
    }

    fn project_dir(&self) -> PathBuf {
        self.root.join("project")
    }

    /// `models.json` pointing the `mock` provider at the loopback server.
    fn write_models(&self, port: u16) {
        std::fs::write(
            self.agent_dir().join("models.json"),
            format!(
                r#"{{"providers":{{"mock":{{"name":"Mock","baseUrl":"http://127.0.0.1:{port}/v1","api":"openai-completions","apiKey":"sk-mock","models":[{{"id":"mock-model","name":"Mock","reasoning":false,"input":["text"],"contextWindow":200000,"maxTokens":4096,"cost":{{"input":0,"output":0,"cacheRead":0,"cacheWrite":0}}}}]}}}}}}"#
            ),
        )
        .expect("write models.json");
    }

    fn write_auth(&self) {
        std::fs::write(self.agent_dir().join("auth.json"), "{}").expect("write auth.json");
    }

    /// The valid settings this arm's malformed file replaces: only the
    /// provider/model defaults live here, so "fell back to defaults" is still
    /// a working run (the mock provider comes from models.json too).
    fn valid_settings(&self) -> &'static str {
        r#"{"defaultProvider":"mock","defaultModel":"mock-model"}"#
    }
}

impl Drop for Hermetic {
    fn drop(&mut self) {
        // A test that made the settings path a directory still removes it
        // recursively; a permissions arm restores access before dropping.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Run the real `pir` binary in the hermetic project dir.
fn run_pir(home: &Hermetic, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pir"))
        .current_dir(home.project_dir())
        .env("PIR_AGENT_DIR", home.agent_dir())
        .env("PIR_SESSION_DIR", home.root.join("sessions"))
        .env("HOME", &home.root)
        .env("USERPROFILE", &home.root)
        .env("PI_OFFLINE", "1")
        .env("PIR_SKIP_VERSION_CHECK", "1")
        .env("NO_COLOR", "1")
        // Verbose diagnostics are log-only; clear the ambient opt-ins so the
        // default-output assertions below cannot be poisoned by the runner's
        // environment.
        .env_remove("RUST_LOG")
        .env_remove("PIR_VERBOSE")
        // The project scope must stay out of the way: these arms are about
        // the GLOBAL settings file, and an untrusted project must not load a
        // second copy.
        .env("PIR_TRUST_PROJECT", "0")
        .args(args)
        .output()
        .expect("pir binary should start")
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The one assertion every arm shares: the run proceeded with the answer on
/// stdout, exit 0, one model request, and the warning still on stderr.
fn assert_ran_and_warned(output: &Output, bodies: &Arc<Mutex<Vec<String>>>, arm: &str) {
    let stdout = stdout_of(output);
    let stderr = stderr_of(output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "[{arm}] the run must proceed on defaults: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stdout.contains("settings mock reply"),
        "[{arm}] the model answer must reach stdout: {stdout:?} (stderr: {stderr:?})"
    );
    assert!(
        !stderr.contains("authentication failure"),
        "[{arm}] a settings typo must never be reported as an authentication failure: {stderr:?}"
    );
    let requests = bodies.lock().expect("mock bodies").len();
    assert_eq!(
        requests, 1,
        "[{arm}] exactly one model request must be issued (got {requests}); stderr: {stderr:?}"
    );
}

const PRINT_ARGS: [&str; 9] = [
    "print",
    "hello",
    "--mode",
    "text",
    "--no-session",
    "--provider",
    "mock",
    "--model",
    "mock-model",
];

// ---------------------------------------------------------------------------
// The arms
// ---------------------------------------------------------------------------

/// Unterminated JSON: the exact document from PIR-BIN-071's repro.
///
/// Before the fix this arm exited 1 with
/// `Error: authentication failure: global settings: EOF while parsing a value
/// at line 2 column 0` and never called the model.
#[test]
fn malformed_settings_still_runs_and_warns() {
    let (port, bodies) = spawn_mock("settings mock reply");
    let home = Hermetic::new("malformed");
    home.write_models(port);
    home.write_auth();
    let settings_path = home.agent_dir().join("settings.json");
    std::fs::write(&settings_path, "{\"defaultProvider\":\"mock\",\n").expect("write settings");

    let output = run_pir(&home, &PRINT_ARGS);

    let stderr = stderr_of(&output);
    assert!(
        stderr.contains(&format!(
            "Warning: Invalid settings file {}",
            settings_path.display()
        )),
        "the parse failure must still be reported once, naming the file: {stderr:?}"
    );
    assert_eq!(
        stderr.matches("Invalid settings file").count(),
        1,
        "exactly one warning per broken file: {stderr:?}"
    );
    assert_ran_and_warned(&output, &bodies, "unterminated JSON");
    // The damaged file is never rewritten.
    assert_eq!(
        std::fs::read_to_string(&settings_path).expect("settings.json stays readable"),
        "{\"defaultProvider\":\"mock\",\n",
        "a broken settings file must not be rewritten"
    );
}

/// An empty file: a valid read that cannot parse.
#[test]
fn empty_settings_file_still_runs_and_warns() {
    let (port, bodies) = spawn_mock("settings mock reply");
    let home = Hermetic::new("empty");
    home.write_models(port);
    home.write_auth();
    std::fs::write(home.agent_dir().join("settings.json"), "").expect("empty settings");

    let output = run_pir(&home, &PRINT_ARGS);

    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("Warning: Invalid settings file"),
        "an empty settings file must be reported: {stderr:?}"
    );
    assert_ran_and_warned(&output, &bodies, "empty file");
}

/// A non-object root (`[]`): the document parses but is not a settings object.
#[test]
fn non_object_settings_root_still_runs_and_warns() {
    let (port, bodies) = spawn_mock("settings mock reply");
    let home = Hermetic::new("array-root");
    home.write_models(port);
    home.write_auth();
    std::fs::write(home.agent_dir().join("settings.json"), "[]").expect("array settings");

    let output = run_pir(&home, &PRINT_ARGS);

    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("settings must be a JSON object"),
        "the non-object root must be named: {stderr:?}"
    );
    assert_ran_and_warned(&output, &bodies, "array root");
}

/// A settings path that is a DIRECTORY: the read fails with the OS error.
#[test]
fn directory_settings_path_still_runs_and_warns() {
    let (port, bodies) = spawn_mock("settings mock reply");
    let home = Hermetic::new("directory");
    home.write_models(port);
    home.write_auth();
    let settings_path = home.agent_dir().join("settings.json");
    std::fs::create_dir_all(&settings_path).expect("settings.json as a directory");

    let output = run_pir(&home, &PRINT_ARGS);

    let stderr = stderr_of(&output);
    assert!(
        stderr.contains(&format!(
            "Warning: Invalid settings file {}",
            settings_path.display()
        )),
        "an unreadable settings path must be reported with its path: {stderr:?}"
    );
    assert_ran_and_warned(&output, &bodies, "directory");
    assert!(
        settings_path.is_dir(),
        "an unreadable settings path must not be replaced by a file"
    );
}

/// `pir models` (the `load_pi_config` path) degrades the same way.
#[test]
fn models_command_survives_malformed_settings() {
    let (port, _bodies) = spawn_mock("settings mock reply");
    let home = Hermetic::new("models-cmd");
    home.write_models(port);
    home.write_auth();
    std::fs::write(home.agent_dir().join("settings.json"), "{ nope").expect("write settings");

    let output = run_pir(&home, &["models"]);

    let stdout = stdout_of(&output);
    let stderr = stderr_of(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "`pir models` must survive a broken settings file: stdout={stdout:?} stderr={stderr:?}"
    );
    // The configured default comes from models.json now that settings.json
    // contributes nothing: the command answers instead of aborting.
    assert!(
        stdout.contains("mock") && stdout.contains("mock-model"),
        "`pir models` must still resolve the models.json default: {stdout:?}"
    );
    assert!(
        stderr.contains("Warning: Invalid settings file"),
        "`pir models` must report the broken file: {stderr:?}"
    );
    assert!(
        !stderr.contains("authentication failure"),
        "`pir models` must not report a config typo as an auth failure: {stderr:?}"
    );
}

/// The project scope: a malformed trusted `.pi/settings.json` must degrade
/// too (the provider snapshot is merged, so it can come from either scope).
#[test]
fn malformed_project_settings_still_runs_and_warns() {
    let (port, bodies) = spawn_mock("settings mock reply");
    let home = Hermetic::new("project-scope");
    home.write_models(port);
    home.write_auth();
    std::fs::write(
        home.agent_dir().join("settings.json"),
        home.valid_settings(),
    )
    .expect("write global settings");
    let project_settings = home.project_dir().join(".pi").join("settings.json");
    std::fs::create_dir_all(project_settings.parent().expect("project .pi")).expect(".pi dir");
    std::fs::write(&project_settings, r#"{"compaction": "#).expect("write project settings");

    let output = Command::new(env!("CARGO_BIN_EXE_pir"))
        .current_dir(home.project_dir())
        .env("PIR_AGENT_DIR", home.agent_dir())
        .env("PIR_SESSION_DIR", home.root.join("sessions"))
        .env("HOME", &home.root)
        .env("USERPROFILE", &home.root)
        .env("PI_OFFLINE", "1")
        .env("PIR_SKIP_VERSION_CHECK", "1")
        .env("NO_COLOR", "1")
        .env_remove("RUST_LOG")
        .env_remove("PIR_VERBOSE")
        .env("PIR_TRUST_PROJECT", "1")
        .args(PRINT_ARGS)
        .output()
        .expect("pir binary should start");

    let stderr = stderr_of(&output);
    assert!(
        stderr.contains(&format!(
            "Warning: Invalid settings file {}",
            project_settings.display()
        )),
        "the project-scope failure must be reported with its path: {stderr:?}"
    );
    assert_ran_and_warned(&output, &bodies, "project scope");
}

// ---------------------------------------------------------------------------
// PIR-BIN-072: a wrong-typed leaf is reported instead of silently defaulted
// ---------------------------------------------------------------------------

/// `compaction.reserveTokens` as a string: the document loads, the leaf falls
/// back to its default, and the user is told which dotted path was ignored.
/// Upstream rejects this value; pir warns and runs (the narrow half of the
/// issue — the compaction-token validation itself is not implemented).
#[test]
fn wrong_typed_leaf_is_reported_as_a_warning() {
    let (port, bodies) = spawn_mock("settings mock reply");
    let home = Hermetic::new("wrong-typed");
    home.write_models(port);
    home.write_auth();
    std::fs::write(
        home.agent_dir().join("settings.json"),
        r#"{"defaultProvider":"mock","defaultModel":"mock-model","compaction":{"enabled":true,"reserveTokens":"8192"}}"#,
    )
    .expect("write settings");

    let output = run_pir(&home, &PRINT_ARGS);

    let stderr = stderr_of(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "a wrong-typed leaf must not abort the run: stderr={stderr:?}"
    );
    assert!(
        stderr.contains("\"compaction.reserveTokens\"") && stderr.contains("invalid setting"),
        "the ignored dotted path must be named on stderr: {stderr:?}"
    );
    assert!(
        !stderr.contains("Invalid settings file"),
        "the document itself parsed, so there is no load failure to report: {stderr:?}"
    );
    let requests = bodies.lock().expect("mock bodies").len();
    assert_eq!(requests, 1, "the run must proceed: {stderr:?}");
}

/// PIR-BIN-071 one-line contract: ONE broken settings file must produce
/// exactly ONE user-facing stderr line naming the problem.
///
/// Every subsystem that re-reads `settings.json` (the codemode plugin, the
/// memory builtin, the extension runtime snapshot) used to echo the same
/// recorded parse failure as its own `WARN` record, so a single typo showed
/// up as three stderr lines: the CLI's `Warning: Invalid settings file …`
/// plus two subsystem copies. The subsystem copies are log-only now
/// (`ext_diag_debug!`), so the user sees the problem once and `--verbose` /
/// `RUST_LOG=debug` still carries the detail.
#[test]
fn one_broken_settings_file_is_reported_on_exactly_one_stderr_line() {
    let (port, bodies) = spawn_mock("settings mock reply");
    let home = Hermetic::new("one-line");
    home.write_models(port);
    home.write_auth();
    let settings_path = home.agent_dir().join("settings.json");
    std::fs::write(&settings_path, "{\"defaultProvider\":\"mock\",\n").expect("write settings");

    let output = run_pir(&home, &PRINT_ARGS);

    let stderr = stderr_of(&output);
    assert_ran_and_warned(&output, &bodies, "one-line");

    let warning_lines: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("Warning:"))
        .collect();
    assert_eq!(
        warning_lines.len(),
        1,
        "one settings problem must produce exactly one `Warning:` line: {stderr:?}"
    );
    assert!(
        warning_lines[0].contains(&format!(
            "Invalid settings file {}",
            settings_path.display()
        )),
        "the single warning must name the file: {warning_lines:?}"
    );

    // The user-facing line is not the only channel: the same parse failure
    // must not reappear as subsystem detail in the default (non-verbose)
    // output. Assert on the error text, which every duplicate carries.
    let problem_lines: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("EOF while parsing a value"))
        .collect();
    assert_eq!(
        problem_lines.len(),
        1,
        "the same settings problem must not be echoed by subsystem loaders: {stderr:?}"
    );
}

/// A valid settings file emits no diagnostics.
#[test]
fn valid_settings_produce_no_settings_warnings() {
    let (port, bodies) = spawn_mock("settings mock reply");
    let home = Hermetic::new("valid");
    home.write_models(port);
    home.write_auth();
    std::fs::write(
        home.agent_dir().join("settings.json"),
        home.valid_settings(),
    )
    .expect("write settings");

    let output = run_pir(&home, &PRINT_ARGS);

    let stderr = stderr_of(&output);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr:?}");
    assert!(
        !stderr.contains("invalid setting") && !stderr.contains("Invalid settings file"),
        "a valid settings file must stay silent: {stderr:?}"
    );
    assert_eq!(bodies.lock().expect("mock bodies").len(), 1);
}

/// Unix-only: a settings file the process cannot READ (mode 000) degrades to
/// defaults with the OS error in the warning. Skipped when running as root,
/// whose credentials ignore the mode.
#[cfg(unix)]
#[test]
fn unreadable_settings_file_still_runs_and_warns() {
    use std::os::unix::fs::PermissionsExt;

    let (port, bodies) = spawn_mock("settings mock reply");
    let home = Hermetic::new("unreadable");
    home.write_models(port);
    home.write_auth();
    let settings_path = home.agent_dir().join("settings.json");
    std::fs::write(&settings_path, home.valid_settings()).expect("write settings");
    std::fs::set_permissions(&settings_path, std::fs::Permissions::from_mode(0o000))
        .expect("chmod settings.json");

    if std::fs::read_to_string(&settings_path).is_ok() {
        let _ = std::fs::set_permissions(&settings_path, std::fs::Permissions::from_mode(0o600));
        return;
    }

    let output = run_pir(&home, &PRINT_ARGS);
    let _ = std::fs::set_permissions(&settings_path, std::fs::Permissions::from_mode(0o600));

    let stderr = stderr_of(&output);
    assert!(
        stderr.contains(&format!(
            "Warning: Invalid settings file {}",
            settings_path.display()
        )),
        "an unreadable file must be reported with its path: {stderr:?}"
    );
    assert_ran_and_warned(&output, &bodies, "unreadable");
}

// ---------------------------------------------------------------------------
// The loader seam itself (no process, no mock): the provider loaders return
// the degraded snapshot rather than an error.
// ---------------------------------------------------------------------------

/// `load_settings_in` never fails on a broken file, and the degraded snapshot
/// keeps the values the OTHER scope supplied.
#[test]
fn provider_settings_loader_degrades_instead_of_failing() {
    let home = Hermetic::new("loader");
    std::fs::write(
        home.agent_dir().join("settings.json"),
        "{ definitely broken",
    )
    .expect("write broken settings");
    std::fs::create_dir_all(home.project_dir().join(".pi")).expect(".pi");
    std::fs::write(
        home.project_dir().join(".pi").join("settings.json"),
        r#"{"theme":"dark"}"#,
    )
    .expect("write project settings");

    // The global scope failed; the trusted project scope still applies.
    let settings = pir::ai::providers::load_settings_in(
        &home.agent_dir(),
        Some(home.project_dir().as_path()),
        true,
    );
    assert_eq!(
        settings.theme.as_deref(),
        Some("dark"),
        "the readable scope must survive the unreadable one"
    );

    // Both scopes unreadable: defaults, still no panic and no error.
    std::fs::write(home.project_dir().join(".pi").join("settings.json"), "[")
        .expect("break project settings");
    let settings = pir::ai::providers::load_settings_in(
        &home.agent_dir(),
        Some(home.project_dir().as_path()),
        true,
    );
    assert_eq!(settings, pir::config::settings::Settings::default());
}

/// The merge-level consequence asserted directly on the manager: a broken
/// global scope produces a recorded diagnostic (not an error return) and the
/// typed defaults, which is what every provider loader now consumes.
#[test]
fn recorded_parse_errors_and_typed_defaults_are_the_only_signal() {
    let home = Hermetic::new("manager");
    std::fs::write(home.agent_dir().join("settings.json"), "{\"theme\": ")
        .expect("write broken settings");

    let mut manager = pir::config::settings::SettingsManager::load(
        &home.agent_dir(),
        &home.project_dir().join(".pi"),
        &home.project_dir(),
        false,
    );
    assert_eq!(
        manager.settings,
        pir::config::settings::Settings::default(),
        "the broken scope contributes defaults"
    );
    let diagnostics = pir::config::settings::collect_settings_diagnostics(&mut manager);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert!(
        diagnostics[0]
            .stderr_line()
            .starts_with("Warning: Invalid settings file "),
        "{diagnostics:?}"
    );
}

// ---------------------------------------------------------------------------
// PIR-BIN-098: a malformed models.json degrades instead of aborting
// ---------------------------------------------------------------------------

/// Before the fix every surface exited 1 with
/// `Error: Failed to parse models.json: …` (mislabeled a moment earlier as an
/// "authentication failure") and the intact built-in catalog was discarded.
/// pi's `ModelConfig.load` keeps the empty models.json layer plus the error
/// text as a warning: `--list-models` exits 0 and still lists the catalog
/// (`cli/list-models.ts` `Warning: errors loading models.json:`), and a print
/// run reaches ordinary model resolution. This arm drives the real binary
/// across all three surfaces of the issue.
#[test]
fn malformed_models_json_degrades_to_the_catalog_and_warns() {
    let home = Hermetic::new("models-json-malformed");
    home.write_auth();
    let models_path = home.agent_dir().join("models.json");
    std::fs::write(&models_path, "{ \"providers\": { oops\n").expect("write malformed models.json");

    // 1. `--list-models`: pi's contract — exit 0, warning on stderr naming the
    //    parse failure and the resolved file, catalog still on stdout.
    let output = run_pir(&home, &["--list-models"]);
    let stdout = stdout_of(&output);
    let stderr = stderr_of(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "a models.json typo must not abort --list-models: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains("Warning: errors loading models.json:\nFailed to parse models.json:"),
        "the pi warning must name the parse failure: {stderr:?}"
    );
    assert!(
        stderr.contains(&format!("File: {}", models_path.display())),
        "the warning must name the resolved file: {stderr:?}"
    );
    assert!(
        !stderr.contains("Error: Failed to parse models.json")
            && !stderr.contains("authentication failure"),
        "the file failure must stay a warning: {stderr:?}"
    );
    assert!(
        stdout.lines().count() > 1000,
        "the built-in catalog must still be listed ({} lines)",
        stdout.lines().count()
    );

    // 2. `pir models`: exit 0, same warning, the ordinary empty-config state
    //    (the degraded layer contributes no provider).
    let output = run_pir(&home, &["models"]);
    let stdout = stdout_of(&output);
    let stderr = stderr_of(&output);
    assert_eq!(
        output.status.code(),
        Some(0),
        "a models.json typo must not abort `models`: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains("Warning: errors loading models.json:"),
        "{stderr:?}"
    );
    assert!(
        stdout.contains("No models configured"),
        "the degraded layer contributes no provider: {stdout:?}"
    );

    // 3. A print run proceeds past the provider load: the remaining failure is
    //    the ordinary no-credential resolution (this harness has none), never
    //    the models.json parse abort.
    let output = run_pir(&home, &["-p", "hi", "--mode", "text", "--no-session"]);
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("Warning: errors loading models.json:"),
        "the print host must report the degraded file too: {stderr:?}"
    );
    assert!(
        !stderr.contains("Error: Failed to parse models.json")
            && !stderr.contains("authentication failure"),
        "{stderr:?}"
    );
    assert!(
        stderr.contains("no model available"),
        "the run must reach ordinary model resolution: {stderr:?}"
    );
}
