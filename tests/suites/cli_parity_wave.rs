//! Feature-parity wave (2026-10-10): the `pir update` target flags
//! (`--models`/`--extensions`/`--all`, upstream package-manager-cli.ts:526-605),
//! the `pir mcp add`/`remove` subcommands (upstream extensions/mcp/cli.ts:33-73),
//! and the `pir auth` flag parity (`--model` on check/print-api-key,
//! `--provider` on print-bearer-token, upstream cli/auth-command.ts:51-61).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use pir::coding_agent::cli::{preprocess_args, AuthCommand, Cli, Command, McpCommand};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Update target plumbing (clap parse -> UpdateRequest -> UpdateTarget)
// ---------------------------------------------------------------------------

fn parse_cli(argv: &[&str]) -> Cli {
    let (args, _) = preprocess_args(argv.iter().map(|arg| (*arg).to_string()).collect());
    let mut full = vec!["pir".to_string()];
    full.extend(args);
    Cli::try_parse_from(full).expect("argv should parse")
}

#[test]
fn update_target_flags_map_from_update_request() {
    use pir::coding_agent::package_manager::UpdateTarget;
    let from = |argv: &[&str]| {
        UpdateTarget::from(
            parse_cli(argv)
                .update_request()
                .expect("update command parses"),
        )
    };
    assert_eq!(from(&["update"]), UpdateTarget::SelfUpdate);
    assert_eq!(
        from(&["update", "--models"]),
        UpdateTarget::Models,
        "`pir update --models` must dispatch to the model-catalog refresh"
    );
    assert_eq!(
        from(&["update", "--extensions"]),
        UpdateTarget::Extensions,
        "`pir update --extensions` must re-pull installed packages"
    );
    assert_eq!(from(&["update", "--all"]), UpdateTarget::All);
    assert_eq!(
        from(&["update", "npm:foo"]),
        UpdateTarget::Package("npm:foo".to_string())
    );
    assert_eq!(
        from(&["update", "--extension", "npm:foo"]),
        UpdateTarget::Package("npm:foo".to_string()),
        "upstream `--extension <source>` targets the same one-source update"
    );
    assert_eq!(from(&["update", "--self"]), UpdateTarget::SelfUpdate);
}

#[test]
fn update_request_flag_conflicts_are_rejected_upstream_style() {
    for argv in [
        vec!["update", "--models", "--extensions"],
        vec!["update", "--models", "--all"],
        vec!["update", "--extensions", "--all"],
        vec!["update", "npm:foo", "--models"],
        vec!["update", "npm:foo", "--all"],
        vec!["update", "npm:foo", "--extensions"],
        vec!["update", "--all", "--extension", "npm:foo"],
        vec!["update", "--extension", "npm:foo", "--extensions"],
    ] {
        assert!(
            parse_cli(&argv).resolve_update_plan().is_err(),
            "upstream rejects conflicting update targets: {argv:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// `update --models`: the model-catalog refresh (mock HTTP catalog server)
// ---------------------------------------------------------------------------

static ENV_LOCK: Mutex<()> = Mutex::new(());

const MOCK_CATALOG_BODY: &str = r#"[
    {
        "id": "update-models-probe",
        "api": "anthropic-messages",
        "name": "Update Models Probe",
        "baseUrl": "http://127.0.0.1:1",
        "input": ["text"],
        "cost": { "input": 1.0, "output": 2.0 }
    }
]"#;

/// A local HTTP server answering every request with `200` + an ETag and the
/// catalog body, recording the request paths (the same mock pattern the
/// `catalog_overlay_install` test uses).
struct MockCatalogServer {
    base: String,
    paths: Arc<Mutex<Vec<String>>>,
    count: Arc<AtomicUsize>,
}

impl MockCatalogServer {
    fn start() -> Self {
        Self::start_with_body(MOCK_CATALOG_BODY)
    }

    fn start_with_body(body: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock catalog listener");
        let port = listener.local_addr().expect("local addr").port();
        let paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let count = Arc::new(AtomicUsize::new(0));
        {
            let paths = Arc::clone(&paths);
            let count = Arc::clone(&count);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        match stream.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(n) => {
                                buffer.extend_from_slice(&chunk[..n]);
                                if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                        if buffer.len() > 64 * 1024 {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&buffer).to_string();
                    let path = head
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_string();
                    paths.lock().unwrap().push(path);
                    count.fetch_add(1, Ordering::SeqCst);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\netag: \"v1\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });
        }
        Self {
            base: format!("http://127.0.0.1:{port}"),
            paths,
            count,
        }
    }

    fn request_count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

fn temp_agent_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-cli-parity-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp agent dir");
    dir
}

/// `pir update --models` hits the remote catalog endpoint of every built-in
/// catalog provider WITH a resolvable credential (upstream
/// `refreshModelCatalogs` + the models.ts:581-583 credential gate), persists
/// the overlay entries into `<agentDir>/models-store.json`, and succeeds;
/// with no reachable catalog server it fails with the upstream aggregated
/// error text.
#[test]
fn update_models_refreshes_remote_catalogs() {
    let _env = ENV_LOCK.lock().unwrap();
    let previous_base = std::env::var("PI_CATALOG_BASE_URL").ok();
    let previous_agent_dir = std::env::var("PIR_AGENT_DIR").ok();
    let previous_anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();

    // Success path: the mock serves a valid catalog for every provider.
    // The credential gate (upstream models.ts:581-583) requires one
    // resolvable credential — ANTHROPIC_API_KEY enables `anthropic`.
    let agent_dir = temp_agent_dir("models-ok");
    std::env::set_var("PIR_AGENT_DIR", &agent_dir);
    std::env::remove_var("PI_OFFLINE");
    std::env::set_var("ANTHROPIC_API_KEY", "test-key");
    let mock = MockCatalogServer::start();
    std::env::set_var("PI_CATALOG_BASE_URL", &mock.base);
    let result = pir::coding_agent::package_manager::update_model_catalogs(&agent_dir);
    assert!(
        result.is_ok(),
        "mock catalog refresh must succeed: {result:?}"
    );
    assert!(
        mock.request_count() > 0,
        "the refresh must request the catalog endpoint"
    );
    assert!(
        mock.paths
            .lock()
            .unwrap()
            .iter()
            .any(|path| path.starts_with("/api/models/providers/")
                && path.contains("types=chat,image,classifier")),
        "the catalog URL must follow the upstream shape: {:?}",
        mock.paths.lock().unwrap()
    );
    assert!(
        agent_dir.join("models-store.json").exists(),
        "refreshed overlays persist into models-store.json"
    );

    // Failure path: an invalid catalog document fails every provider's parse
    // and aggregates into the upstream error message.
    let bad = MockCatalogServer::start_with_body("not a catalog document");
    let agent_dir = temp_agent_dir("models-fail");
    std::env::set_var("PIR_AGENT_DIR", &agent_dir);
    std::env::set_var("PI_CATALOG_BASE_URL", &bad.base);
    let error = pir::coding_agent::package_manager::update_model_catalogs(&agent_dir)
        .expect_err("an invalid catalog document must fail");
    assert!(
        error
            .to_string()
            .starts_with("Could not refresh model catalogs: "),
        "upstream aggregated error text, got: {error}"
    );

    match previous_base {
        Some(value) => std::env::set_var("PI_CATALOG_BASE_URL", value),
        None => std::env::remove_var("PI_CATALOG_BASE_URL"),
    }
    match previous_agent_dir {
        Some(value) => std::env::set_var("PIR_AGENT_DIR", value),
        None => std::env::remove_var("PIR_AGENT_DIR"),
    }
    match previous_anthropic_key {
        Some(value) => std::env::set_var("ANTHROPIC_API_KEY", value),
        None => std::env::remove_var("ANTHROPIC_API_KEY"),
    }
}

/// `update --models` is dispatched from the CLI's async `run()` body. The
/// refresh must not build a second runtime and `block_on` it on the same
/// thread (`Cannot start a runtime from within a runtime` panic).
#[test]
fn update_models_works_inside_the_cli_runtime() {
    let _env = ENV_LOCK.lock().unwrap();
    let previous_base = std::env::var("PI_CATALOG_BASE_URL").ok();
    let previous_agent_dir = std::env::var("PIR_AGENT_DIR").ok();
    let previous_anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();

    let agent_dir = temp_agent_dir("models-runtime");
    std::env::set_var("PIR_AGENT_DIR", &agent_dir);
    std::env::remove_var("PI_OFFLINE");
    std::env::set_var("ANTHROPIC_API_KEY", "test-key");
    let mock = MockCatalogServer::start();
    std::env::set_var("PI_CATALOG_BASE_URL", &mock.base);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .expect("multi-thread test runtime");
    let result = runtime
        .block_on(async { pir::coding_agent::package_manager::update_model_catalogs(&agent_dir) });
    assert!(
        result.is_ok(),
        "a nested-runtime call must complete, not panic: {result:?}"
    );

    match previous_base {
        Some(value) => std::env::set_var("PI_CATALOG_BASE_URL", value),
        None => std::env::remove_var("PI_CATALOG_BASE_URL"),
    }
    match previous_agent_dir {
        Some(value) => std::env::set_var("PIR_AGENT_DIR", value),
        None => std::env::remove_var("PIR_AGENT_DIR"),
    }
    match previous_anthropic_key {
        Some(value) => std::env::set_var("ANTHROPIC_API_KEY", value),
        None => std::env::remove_var("ANTHROPIC_API_KEY"),
    }
}

// ---------------------------------------------------------------------------
// `pir mcp add` / `pir mcp remove`
// ---------------------------------------------------------------------------

#[test]
fn mcp_add_parses_upstream_option_surface() {
    let cli = Cli::try_parse_from([
        "pir",
        "mcp",
        "add",
        "weather",
        "--url",
        "https://mcp.example.com/mcp",
        "--header",
        "X-Api-Key=secret",
        "--oauth-client-id",
        "client-1",
        "--exposure",
        "direct",
        "--description",
        "Weather tools",
    ])
    .expect("mcp add with --url should parse");
    match cli.command {
        Some(Command::Mcp {
            command: Some(McpCommand::Add { name, url, .. }),
        }) => {
            assert_eq!(name, "weather");
            assert_eq!(url.as_deref(), Some("https://mcp.example.com/mcp"));
        }
        other => panic!("unexpected command: {other:?}"),
    }
    let cli = Cli::try_parse_from([
        "pir",
        "mcp",
        "add",
        "files",
        "--env",
        "ROOT=/tmp",
        "--cwd",
        "/srv",
        "-l",
        "--",
        "npx",
        "-y",
        "@example/files-mcp",
    ])
    .expect("mcp add with -- command should parse");
    match cli.command {
        Some(Command::Mcp {
            command:
                Some(McpCommand::Add {
                    name,
                    command,
                    env,
                    cwd,
                    local,
                    ..
                }),
        }) => {
            assert_eq!(name, "files");
            assert_eq!(command, vec!["npx", "-y", "@example/files-mcp"]);
            assert_eq!(env, vec!["ROOT=/tmp".to_string()]);
            assert_eq!(cwd.as_deref(), Some("/srv"));
            assert!(local);
        }
        other => panic!("unexpected command: {other:?}"),
    }
    let cli = Cli::try_parse_from(["pir", "mcp", "remove", "weather", "-l"])
        .expect("mcp remove should parse");
    match cli.command {
        Some(Command::Mcp {
            command: Some(McpCommand::Remove { name, local }),
        }) => {
            assert_eq!(name, "weather");
            assert!(local);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn mcp_config_upsert_remove_round_trip() {
    let dir = temp_agent_dir("mcp-roundtrip");
    let path = dir.join("mcp.json");

    let stdio = serde_json::json!({
        "command": "npx",
        "args": ["-y", "@example/files-mcp"],
        "env": { "ROOT": "/tmp" },
    });
    let replaced =
        pir::coding_agent::cli::mcp_config_upsert(&path, "files", &stdio).expect("first add");
    assert!(!replaced, "the first add inserts, it does not replace");
    let http = serde_json::json!({
        "url": "https://mcp.example.com/mcp",
        "headers": { "X-Api-Key": "secret" },
        "exposure": "direct",
    });
    let replaced =
        pir::coding_agent::cli::mcp_config_upsert(&path, "files", &http).expect("second add");
    assert!(replaced, "an existing name is replaced");
    // The written file uses the upstream `mcpServers` wrapper shape (what
    // upstream pi writes and requires) and parses back with pir's loader.
    let contents = std::fs::read_to_string(&path).expect("read mcp.json");
    let root: serde_json::Value = serde_json::from_str(&contents).expect("valid JSON");
    assert!(
        root.get("mcpServers")
            .is_some_and(serde_json::Value::is_object),
        "the writer emits the upstream wrapper shape: {root}"
    );
    let (servers, errors) = pir::config::mcp::parse_mcp_config(&contents);
    assert!(errors.is_empty(), "no loader errors: {errors:?}");
    assert_eq!(servers.len(), 1);
    assert!(matches!(
        servers.get("files"),
        Some(pir::config::mcp::McpServerConfig::Http(_))
    ));

    assert!(
        !pir::coding_agent::cli::mcp_config_remove(&path, "missing").expect("remove absent"),
        "removing an absent name reports false"
    );
    assert!(
        pir::coding_agent::cli::mcp_config_remove(&path, "files").expect("remove present"),
        "removing a defined name reports true"
    );
    let contents = std::fs::read_to_string(&path).expect("read mcp.json");
    let (servers, errors) = pir::config::mcp::parse_mcp_config(&contents);
    assert!(errors.is_empty());
    assert!(servers.is_empty(), "the entry is gone");
}

/// The full `pir mcp add`/`pir mcp remove` commands against a temp
/// `PIR_AGENT_DIR` (process-global env, hence the lock).
#[test]
fn mcp_add_and_remove_commands_round_trip_on_agent_dir() {
    let _env = ENV_LOCK.lock().unwrap();
    let previous_agent_dir = std::env::var("PIR_AGENT_DIR").ok();
    let agent_dir = temp_agent_dir("mcp-command");
    std::env::set_var("PIR_AGENT_DIR", &agent_dir);
    std::env::remove_var("PI_OFFLINE");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    // `pir mcp add weather --url ... --header ...` writes the global
    // mcp.json and reports the sign-in hint for an Authorization-less HTTP
    // server.
    runtime
        .block_on(pir::coding_agent::cli::run_mcp_command(McpCommand::Add {
            name: "weather".to_string(),
            url: Some("https://mcp.example.com/mcp".to_string()),
            command: Vec::new(),
            env: Vec::new(),
            cwd: None,
            header: vec!["X-Api-Key=secret".to_string()],
            bearer_token_env_var: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_callback_port: None,
            oauth_client_name: None,
            exposure: Some("direct".to_string()),
            description: None,
            local: false,
        }))
        .expect("mcp add succeeds");
    let path = pir::config::mcp::global_mcp_path(&agent_dir);
    let contents = std::fs::read_to_string(&path).expect("mcp.json written");
    let (servers, errors) = pir::config::mcp::parse_mcp_config(&contents);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(matches!(
        servers.get("weather"),
        Some(pir::config::mcp::McpServerConfig::Http(_))
    ));

    // `pir mcp remove weather` deletes the entry; a second remove fails with
    // the upstream not-found message.
    runtime
        .block_on(pir::coding_agent::cli::run_mcp_command(
            McpCommand::Remove {
                name: "weather".to_string(),
                local: false,
            },
        ))
        .expect("mcp remove succeeds");
    let contents = std::fs::read_to_string(&path).expect("mcp.json still there");
    let (servers, errors) = pir::config::mcp::parse_mcp_config(&contents);
    assert!(errors.is_empty());
    assert!(servers.is_empty());
    let error = runtime
        .block_on(pir::coding_agent::cli::run_mcp_command(
            McpCommand::Remove {
                name: "weather".to_string(),
                local: false,
            },
        ))
        .expect_err("second remove fails");
    assert!(
        error
            .to_string()
            .contains("No global MCP server named \"weather\""),
        "upstream not-found text, got: {error}"
    );

    match previous_agent_dir {
        Some(value) => std::env::set_var("PIR_AGENT_DIR", value),
        None => std::env::remove_var("PIR_AGENT_DIR"),
    }
}

/// `pir mcp list` reports the trusted project's `mcp.json` for a server the
/// project overrides (upstream `ServerReport.override`, extensions/mcp/cli.ts
/// :434-470), and never for a project file that DEFINES the server itself.
#[test]
fn mcp_list_reports_the_project_override_path() {
    let root = temp_agent_dir("mcp-override");
    let agent = root.join("agent");
    let work = root.join("work");
    std::fs::create_dir_all(&agent).expect("create agent dir");
    std::fs::create_dir_all(work.join(".pi")).expect("create project config dir");
    // Both servers are disabled, so `list` never connects (exit 0) while still
    // reporting the scope/override metadata this test pins.
    std::fs::write(
        agent.join("mcp.json"),
        json!({
            "mcpServers": {
                "docs": { "command": "true", "enabled": false },
                "tool": { "command": "true", "enabled": false }
            }
        })
        .to_string(),
    )
    .expect("write global mcp.json");
    let project_config = work.join(".pi").join("mcp.json");
    std::fs::write(
        &project_config,
        json!({
            "mcpServers": {
                // Override-only entry: only `enabled` (upstream `entry.override`).
                "docs": { "enabled": false },
                // A full project definition: not an override.
                "local": { "command": "true", "enabled": false }
            }
        })
        .to_string(),
    )
    .expect("write project mcp.json");

    let mut command = ProcessCommand::new(env!("CARGO_BIN_EXE_pir"));
    command
        .current_dir(&work)
        .env("PIR_AGENT_DIR", &agent)
        .env("PIR_CODING_AGENT_DIR", &agent)
        .env("PI_CODING_AGENT_DIR", &agent)
        .env("HOME", &root)
        .env("USERPROFILE", &root)
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .args(["mcp", "list", "--json", "--approve"])
        .stdin(Stdio::null());
    let output = command.output().expect("run the pir binary");
    assert!(
        output.status.success(),
        "`pir mcp list --json` exits 0 for disabled servers; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value =
        serde_json::from_slice(&output.stdout).expect("`--json` prints one JSON document");
    assert_eq!(document["errors"], json!([]), "{document}");
    let servers = document["servers"].as_array().expect("servers array");
    let find = |name: &str| {
        servers
            .iter()
            .find(|server| server["name"] == name)
            .unwrap_or_else(|| panic!("server {name} missing: {document}"))
    };
    assert_eq!(
        find("docs")["override"],
        json!(project_config.display().to_string()),
        "an override-only project entry reports the project mcp.json: {document}"
    );
    assert_eq!(
        find("docs")["enabled"],
        json!(false),
        "the merged override applies: {document}"
    );
    assert!(
        find("local").get("override").is_none(),
        "a full project definition is not an override: {document}"
    );
    assert!(
        find("tool").get("override").is_none(),
        "a global server without a project entry has no override: {document}"
    );
}

/// Removing from the wrong scope names the file that DOES define the server
/// (upstream `remove`, extensions/mcp/cli.ts:423-433): `pir mcp remove x
/// --local` when only the global file defines `x` fails with `It is defined
/// in …; omit --local`.
#[test]
fn mcp_remove_wrong_scope_points_at_the_defining_file() {
    let _env = ENV_LOCK.lock().unwrap();
    let previous_agent_dir = std::env::var("PIR_AGENT_DIR").ok();
    let agent_dir = temp_agent_dir("mcp-scope");
    std::env::set_var("PIR_AGENT_DIR", &agent_dir);
    let global = pir::config::mcp::global_mcp_path(&agent_dir);
    pir::coding_agent::cli::mcp_config_upsert(
        &global,
        "scope-probe",
        &serde_json::json!({ "command": "true" }),
    )
    .expect("seed global entry");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let error = runtime
        .block_on(pir::coding_agent::cli::run_mcp_command(
            McpCommand::Remove {
                name: "scope-probe".to_string(),
                local: true,
            },
        ))
        .expect_err("the project scope does not define the server");
    let message = error.to_string();
    assert!(
        message.contains("No project MCP server named \"scope-probe\""),
        "{message}"
    );
    assert!(
        message.contains("It is defined in") && message.contains("omit --local"),
        "upstream hint names the global file: {message}"
    );

    // And the reverse: removing a project-only server without --local.
    let cwd = std::env::current_dir().expect("cwd");
    let project = pir::config::mcp::project_mcp_path(&cwd);
    let previous_project = std::fs::read_to_string(&project).ok();
    pir::coding_agent::cli::mcp_config_upsert(
        &project,
        "scope-probe-2",
        &serde_json::json!({ "command": "true" }),
    )
    .expect("seed project entry");
    let error = runtime
        .block_on(pir::coding_agent::cli::run_mcp_command(
            McpCommand::Remove {
                name: "scope-probe-2".to_string(),
                local: false,
            },
        ))
        .expect_err("the global scope does not define the server");
    let message = error.to_string();
    assert!(
        message.contains("It is defined in") && message.contains("use --local"),
        "upstream hint names the project file: {message}"
    );
    // Clean up the seeded project file (restore whatever was there).
    match previous_project {
        Some(contents) => {
            let _ = std::fs::write(&project, contents);
        }
        None => {
            let _ = std::fs::remove_file(&project);
        }
    }
    let _ = pir::coding_agent::cli::mcp_config_remove(&global, "scope-probe");
    match previous_agent_dir {
        Some(value) => std::env::set_var("PIR_AGENT_DIR", value),
        None => std::env::remove_var("PIR_AGENT_DIR"),
    }
}

// ---------------------------------------------------------------------------
// `pir auth` flag parity (upstream cli/auth-command.ts:51-61)
// ---------------------------------------------------------------------------

#[test]
fn auth_flags_parse_on_all_subcommands() {
    let cli = Cli::try_parse_from(["pir", "auth", "check", "--model", "anthropic/claude-opus"])
        .expect("auth check --model should parse");
    match cli.command {
        Some(Command::Auth {
            command: Some(AuthCommand::Check {
                provider, model, ..
            }),
        }) => {
            assert_eq!(provider, None);
            assert_eq!(model.as_deref(), Some("anthropic/claude-opus"));
        }
        other => panic!("unexpected: {other:?}"),
    }
    let cli = Cli::try_parse_from(["pir", "auth", "print-api-key", "--model", "gpt-5"])
        .expect("print-api-key --model should parse");
    match cli.command {
        Some(Command::Auth {
            command: Some(AuthCommand::PrintApiKey { provider, model }),
        }) => {
            assert_eq!(provider, None);
            assert_eq!(model.as_deref(), Some("gpt-5"));
        }
        other => panic!("unexpected: {other:?}"),
    }
    let cli = Cli::try_parse_from([
        "pir",
        "auth",
        "print-bearer-token",
        "--provider",
        "openai-codex",
    ])
    .expect("print-bearer-token --provider should parse");
    match cli.command {
        Some(Command::Auth {
            command: Some(AuthCommand::PrintBearerToken { provider, .. }),
        }) => assert_eq!(provider.as_deref(), Some("openai-codex")),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn auth_provider_resolution_matches_upstream_rules() {
    use pir::ai::auth::cli::resolve_provider_from_flags;
    // Explicit provider wins.
    assert_eq!(
        resolve_provider_from_flags(Some("openai"), None, "unused").expect("provider only"),
        "openai"
    );
    // Model-only resolves through the model pattern (canonical provider/model
    // reference needs no environment).
    assert_eq!(
        resolve_provider_from_flags(None, Some("anthropic/claude-opus"), "unused")
            .expect("model only"),
        "anthropic"
    );
    // Provider + model must agree.
    assert_eq!(
        resolve_provider_from_flags(Some("anthropic"), Some("anthropic/claude-opus"), "unused")
            .expect("matching pair"),
        "anthropic"
    );
    // Neither given: the upstream requirement text (the typed AiError wraps
    // it with its Display prefix).
    let error = resolve_provider_from_flags(
        None,
        None,
        "Auth checks require --provider <provider> or --model <model>",
    )
    .expect_err("nothing given");
    assert!(
        error
            .to_string()
            .contains("Auth checks require --provider <provider> or --model <model>"),
        "{error}"
    );
    // Disagreeing pair: the upstream resolution-failure text.
    let error =
        resolve_provider_from_flags(Some("openai"), Some("anthropic/claude-opus"), "unused")
            .expect_err("mismatched pair");
    assert!(
        error
            .to_string()
            .contains("Unable to resolve the requested provider/model"),
        "{error}"
    );
    // Unknown model reference: the upstream not-found text.
    let error = resolve_provider_from_flags(None, Some("no-such-model-xyz"), "unused")
        .expect_err("unknown model");
    assert!(error.to_string().contains("not found"), "{error}");
}

#[test]
fn mcp_add_rejects_names_the_loader_would_drop() {
    // `mcp add` must not write an entry that `parse_mcp_config` rejects on the
    // next start (the loader's `[A-Za-z0-9_-]+` name contract).
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let error = runtime
        .block_on(pir::coding_agent::cli::run_mcp_command(McpCommand::Add {
            name: "bad name!".to_string(),
            url: Some("https://mcp.example.com/mcp".to_string()),
            command: Vec::new(),
            env: Vec::new(),
            cwd: None,
            header: Vec::new(),
            bearer_token_env_var: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_callback_port: None,
            oauth_client_name: None,
            exposure: None,
            description: None,
            local: false,
        }))
        .expect_err("an unloadable server name must be rejected");
    assert!(
        error.to_string().contains("name must match"),
        "unexpected error: {error}"
    );
}

// ---------------------------------------------------------------------------
// PIR-BIN-030: a positional that spells a subcommand is a PROMPT whenever the
// print surface is explicitly selected (upstream `resolveAppMode` decides the
// host from flags first, then treats every positional as a message).
//
// All but the two schedule tests drive the REAL parse (`parse_cli`, the one the
// binary calls): the observable contract is `Cli::command == None` and the word
// in `Cli::messages`. `pir print "<prompt>"`, bare management commands and
// `--mode rpc` keep their grammar; `--help`/`--version` keep short-circuiting.
// ---------------------------------------------------------------------------

/// [`pir::coding_agent::cli::parse_cli`] with the `pir` argv[0] prepended and
/// the bundled shorts normalized first: the exact sequence
/// `src/main_cli/run_mode.rs` runs (preprocess, then the mode-aware parse).
fn parse_pir(argv: &[&str]) -> Result<Cli, clap::Error> {
    let (args, _) = preprocess_args(argv.iter().map(|arg| (*arg).to_string()).collect());
    let mut full = vec!["pir".to_owned()];
    full.extend(args);
    pir::coding_agent::cli::parse_cli(&full)
}

/// The prompt a print-surface invocation ends up carrying (and which subcommand,
/// if any, was resolved).
///
/// The prompt comes from [`Cli::entry_plan`], the production path that folds a
/// `--print [MESSAGE]` value into the message list — `-p version` binds
/// `version` to `--print` (clap optional value) and the plan reports it as the
/// prompt, exactly as the print host prompts it.
fn prompt_of(argv: &[&str]) -> (Option<String>, Vec<String>) {
    let cli = parse_pir(argv).expect("argv should parse");
    let messages = cli.entry_plan(true, true).messages;
    let command = cli.command.as_ref().map(|command| match command {
        Command::Version => "version".to_string(),
        Command::Models { .. } => "models".to_string(),
        Command::Config { .. } => "config".to_string(),
        Command::Session => "session".to_string(),
        Command::List => "list".to_string(),
        Command::Mcp { .. } => "mcp".to_string(),
        Command::Install { .. } => "install".to_string(),
        Command::Grep { .. } => "grep".to_string(),
        Command::Print { .. } => "print".to_string(),
        Command::Interactive => "interactive".to_string(),
        Command::Rpc => "rpc".to_string(),
        other => format!("{other:?}"),
    });
    (command, messages)
}

/// Every top-level subcommand spelling the derive knows, plus the ones clap
/// generates itself (`help`) and the one `#[command(version)]` shadows
/// (`version`: the generated flag/builtin hides the same-named variant, so it
/// never appears in `get_subcommands()` while `pir version` still dispatches).
fn subcommand_spellings() -> Vec<String> {
    use clap::CommandFactory;
    let mut words: Vec<String> = Cli::command()
        .get_subcommands()
        .map(|sub| sub.get_name().to_owned())
        .collect();
    for word in ["help", "version"] {
        if !words.iter().any(|existing| existing == word) {
            words.push(word.to_owned());
        }
    }
    words
}

#[test]
fn print_surface_shadows_every_real_subcommand_name() {
    // The gate works by renaming subcommands, so its placeholder prefix must
    // stay unspellable by a real command name or alias.
    use clap::CommandFactory;
    for word in subcommand_spellings() {
        assert!(
            !word.starts_with("__pir-print-surface-"),
            "the placeholder prefix must not collide with the real command `{word}`"
        );
    }
    assert!(
        Cli::command()
            .get_subcommands()
            .any(|sub| sub.get_name() == "print"),
        "the documented `pir print` subcommand must exist"
    );
}

#[test]
fn subcommand_spelling_is_a_prompt_in_print_mode() {
    // The exact shape the issue reports: the print surface is selected by a
    // flag, so the trailing word must reach the model.
    for word in subcommand_spellings() {
        let argv = [
            "--print",
            "--mode",
            "json",
            "--no-session",
            "--provider",
            "mock",
            "--model",
            "mock/mock-model",
            word.as_str(),
        ];
        let (command, messages) = prompt_of(&argv);
        assert_eq!(
            command, None,
            "`{word}` must not resolve a subcommand in print mode"
        );
        assert_eq!(
            messages,
            vec![word.clone()],
            "`{word}` must arrive as the prompt"
        );
    }
}

#[test]
fn print_flag_alone_still_yields_the_word_as_a_prompt() {
    let (command, messages) = prompt_of(&["-p", "version"]);
    assert_eq!(command, None, "`-p version` must be a prompt");
    assert_eq!(messages, vec!["version".to_string()]);
}

#[test]
fn print_flag_after_the_prompt_still_demotes_it() {
    // Upstream parses flags by NAME wherever they appear, so the selection
    // cannot depend on flag order.
    let (command, messages) = prompt_of(&["version", "--print", "--mode", "json"]);
    assert_eq!(command, None, "`version --print` must be a prompt");
    assert_eq!(messages, vec!["version".to_string()]);
}

#[test]
fn mode_json_alone_selects_the_print_surface() {
    // `--mode json` without `--print` is the JSON print host, not a subcommand
    // carrier (upstream `resolveAppMode`).
    for argv in [
        vec!["--mode", "json", "version"],
        vec!["--mode=json", "version"],
        vec!["--mode", "text", "models"],
    ] {
        let (command, messages) = prompt_of(&argv);
        assert_eq!(command, None, "`{argv:?}` must not resolve a subcommand");
        assert!(!messages.is_empty(), "`{argv:?}` must carry the prompt");
    }
}

#[test]
fn documented_print_subcommand_keeps_its_grammar() {
    // `pir print "<prompt>"` — the leading positional IS the subcommand, and
    // its own prompt argument carries the text. The same holds with mode flags
    // in front of or after the subcommand.
    for argv in [
        vec!["print", "--mode", "json", "version"],
        vec!["print", "--no-session", "install npm:left-pad"],
        vec!["print", "models"],
    ] {
        let cli = parse_pir(&argv).expect("argv should parse");
        match cli.command {
            Some(Command::Print { prompt }) => {
                assert!(
                    prompt.is_some(),
                    "`pir {argv:?}` must carry its own prompt argument"
                );
            }
            other => panic!("`pir {argv:?}` must keep the `print` subcommand: {other:?}"),
        }
    }
    // The exemption is the FIRST TOKEN, not "the word print anywhere": with an
    // explicit print selection, a later `print` word is an ordinary prompt.
    let (command, messages) = prompt_of(&["--print", "--mode", "json", "print"]);
    assert_eq!(
        command, None,
        "`pir --print --mode json print` must treat the word as a prompt"
    );
    assert_eq!(messages, vec!["print".to_string()]);
}

#[test]
fn bare_management_commands_are_untouched() {
    for argv in [
        vec!["version"],
        vec!["models"],
        vec!["config"],
        vec!["list"],
        vec!["session"],
        vec!["mcp"],
        vec!["interactive"],
    ] {
        let cli = parse_cli(&argv);
        assert!(
            cli.command.is_some(),
            "bare `pir {}` must still dispatch a command",
            argv[0]
        );
        assert!(
            cli.messages.is_empty(),
            "bare `pir {}` must not leave a message positional",
            argv[0]
        );
    }
    // Arguments of a management command still bind to it.
    let config = parse_cli(&["config", "-l"]);
    assert!(
        matches!(config.command, Some(Command::Config { local: true, .. })),
        "`pir config -l` must keep its --local flag: {:?}",
        config.command
    );
}

#[test]
fn rpc_mode_keeps_the_subcommand_grammar() {
    // `--mode rpc` is its own host, not the print surface: a positional word
    // still resolves the subcommand exactly as before, and it wins over
    // `--print` whatever the flag order (upstream `resolveAppMode`).
    // `--mode rpc --print version` is deliberately absent: clap's `--print`
    // takes an OPTIONAL value, so there `version` is the flag's value and the
    // run is an RPC host (the pre-fix grammar did the same); it is not a
    // subcommand-resolution case.
    for argv in [
        vec!["--mode", "rpc", "version"],
        vec!["--print", "--mode", "rpc", "version"],
        vec!["--mode=rpc", "models"],
    ] {
        let cli = parse_cli(&argv);
        assert!(
            cli.command.is_some(),
            "`pir {argv:?}` must keep the subcommand: {:?}",
            cli.command
        );
        assert!(
            cli.messages.is_empty(),
            "`pir {argv:?}` must not turn the command name into a message"
        );
    }
}

#[test]
fn flags_after_a_demoted_prompt_still_parse_as_flags() {
    // The gate renames subcommands; it must NOT insert `--` (a `--` would turn
    // every later token into prompt text). Upstream keeps recognizing flags by
    // name anywhere on the line, so a trailing `--no-session` is still a flag.
    let cli = parse_pir(&["--print", "install", "npm:left-pad", "now", "--no-session"])
        .expect("the print-surface parse must accept the invocation");
    assert!(
        cli.command.is_none(),
        "no subcommand may resolve, got {:?}",
        cli.command
    );
    assert!(cli.no_session, "a trailing --no-session must stay a flag");
    assert_eq!(
        cli.entry_plan(true, true).messages,
        vec![
            "install".to_string(),
            "npm:left-pad".to_string(),
            "now".to_string()
        ],
        "every positional word stays a prompt word, in order"
    );
}

#[test]
fn help_and_version_short_circuit_before_the_gate() {
    // `--print --help` must print the NORMAL help (not a shadow grammar's),
    // and the version flags must keep clap's DisplayVersion path.
    for argv in [vec!["--print", "--help"], vec!["--mode", "json", "--help"]] {
        let Err(error) = parse_pir(&argv) else {
            panic!("`pir {argv:?}` must surface a clap DisplayHelp error");
        };
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::DisplayHelp,
            "`pir {argv:?}` must short-circuit to help"
        );
        let rendered = error.to_string();
        assert!(
            rendered.contains("Commands:") && rendered.contains("  version "),
            "help must list the REAL subcommands, got:\n{rendered}"
        );
    }
    for argv in [vec!["--print", "--version"], vec!["--mode", "json", "-v"]] {
        let Err(error) = parse_pir(&argv) else {
            panic!("`pir {argv:?}` must surface a clap DisplayVersion error");
        };
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::DisplayVersion,
            "`pir {argv:?}` must short-circuit to the version"
        );
    }
}

#[test]
fn value_words_are_not_mistaken_for_the_print_selector() {
    // `--model version` puts the word `version` in a VALUE slot: the scan must
    // not read it as a selector, and no print host is implied by it.
    let cli = parse_cli(&["--model", "version"]);
    assert!(
        cli.model.as_deref() == Some("version"),
        "the word must bind to --model"
    );
    let cli = parse_cli(&["--mode", "rpc", "--model", "json", "version"]);
    assert!(
        cli.command.is_some(),
        "rpc keeps the grammar even with a `json` model name"
    );
}

// ---------------------------------------------------------------------------
// PIR-BIN-030 process level: the REAL binary, one loopback SSE mock.
//
// The unit tests above pin the parse; these pin the user-visible contract:
// a subcommand word in print/JSON mode produces the JSONL event stream (the
// model was called), while bare management commands, `pir print "<prompt>"`,
// `--mode rpc` and `--help` keep their existing behavior.
// ---------------------------------------------------------------------------

/// A loopback OpenAI-compatible SSE provider answering every POST with one
/// complete `stop` stream and recording each request body.
struct PrintMock {
    base_url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PrintMock {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the print mock");
        listener
            .set_nonblocking(true)
            .expect("non-blocking print mock");
        let addr = listener.local_addr().expect("print mock addr");
        let requests: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicUsize::new(0));
        let recorded = Arc::clone(&requests);
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while stopping.load(Ordering::SeqCst) == 0 {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if let Some(body) = serve_print_request(stream) {
                            recorded
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push(body);
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

    /// Wait until `count` requests have landed (the child's exit can race the
    /// mock's accept loop), then return what was recorded.
    fn wait_for_requests(&self, count: usize) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let requests = self.requests();
            if requests.len() >= count || Instant::now() >= deadline {
                return requests;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for PrintMock {
    fn drop(&mut self) {
        self.stop.store(1, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Serve one request; returns the parsed POST body (the model request), or
/// `None` for the GET /models probe the provider catalog may issue.
fn serve_print_request(mut stream: std::net::TcpStream) -> Option<Value> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(position) = find_bytes(&buffer, b"\r\n\r\n") {
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
        let mut sse = String::new();
        sse.push_str(&sse_chunk(
            json!({ "role": "assistant", "content": "" }),
            None,
        ));
        sse.push_str(&sse_chunk(json!({ "content": "P030-MOCK-REPLY" }), None));
        sse.push_str(&sse_chunk(json!({}), Some("stop")));
        sse.push_str("data: [DONE]\n\n");
        http_bytes("text/event-stream", sse.into_bytes())
    } else {
        http_bytes(
            "application/json",
            br#"{"object":"list","data":[{"id":"mock-model","object":"model"}]}"#.to_vec(),
        )
    };
    let _ = stream.write_all(&response);
    let _ = stream.flush();
    parsed
}

fn http_bytes(content_type: &str, body: Vec<u8>) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncache-control: no-cache\r\n\
         connection: close\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    response
}

fn sse_chunk(delta: Value, finish_reason: Option<&str>) -> String {
    let payload = json!({
        "id": "mock-p030",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "mock-model",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }],
    });
    format!("data: {payload}\n\n")
}

/// Isolated agent dir (models.json pointing at `mock`) plus a work dir.
struct PrintSandbox {
    root: PathBuf,
    agent: PathBuf,
    work: PathBuf,
}

impl PrintSandbox {
    fn new(label: &str, mock: &PrintMock) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pir-p030-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        let agent = root.join("agent");
        let work = root.join("work");
        std::fs::create_dir_all(&agent).expect("create agent dir");
        std::fs::create_dir_all(&work).expect("create work dir");
        let models = json!({
            "providers": {
                "mock": {
                    "name": "Mock",
                    "baseUrl": mock.base_url,
                    "api": "openai-completions",
                    "apiKey": "sk-mock-p030-not-a-real-credential",
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
        std::fs::write(agent.join("models.json"), models.to_string()).expect("write models.json");
        std::fs::write(
            agent.join("settings.json"),
            json!({
                "defaultProvider": "mock",
                "defaultModel": "mock-model",
                "defaultThinkingLevel": "off",
            })
            .to_string(),
        )
        .expect("write settings.json");
        Self { root, agent, work }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    /// Run the built binary with the sandbox environment.
    fn run(&self, args: &[&str]) -> std::process::Output {
        let mut command = ProcessCommand::new(env!("CARGO_BIN_EXE_pir"));
        command
            .current_dir(&self.work)
            .env("PIR_AGENT_DIR", &self.agent)
            .env("PIR_CODING_AGENT_DIR", &self.agent)
            .env("PI_CODING_AGENT_DIR", &self.agent)
            .env("HOME", &self.root)
            .env("USERPROFILE", &self.root)
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .env("RUST_MIN_STACK", "8388608")
            .args(args)
            .stdin(Stdio::null());
        command.output().expect("run the pir binary")
    }
}

impl Drop for PrintSandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The print flags every case shares (no extensions, no context, no session).
const P030_PRINT_FLAGS: [&str; 9] = [
    "--no-session",
    "--no-context-files",
    "--no-skills",
    "--no-prompt-templates",
    "--no-extensions",
    "--provider",
    "mock",
    "--model",
    "mock-model",
];

fn print_args(word: &str) -> Vec<&str> {
    let mut args = vec!["--print", "--mode", "json"];
    args.extend(P030_PRINT_FLAGS);
    args.push(word);
    args
}

#[test]
fn binary_print_json_mode_sends_subcommand_words_to_the_model() {
    let mock = PrintMock::start();
    for word in ["version", "models", "config", "install", "grep"] {
        let sandbox = PrintSandbox::new(&format!("word-{word}"), &mock);
        let before = mock.requests().len();
        let output = sandbox.run(&print_args(word));
        assert!(
            output.status.success(),
            "`pir --print --mode json {word}` must exit 0; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let first = stdout.lines().next().unwrap_or_default();
        assert!(
            first.starts_with('{'),
            "JSON mode must emit the JSONL event stream, got first line: {first}"
        );
        assert!(
            stdout.contains("\"type\":\"session\""),
            "the stream must open with the session frame, got:\n{}",
            stdout.lines().take(3).collect::<Vec<_>>().join("\n")
        );
        assert!(
            stdout.contains(&format!("\"text\":\"{word}\"")),
            "the user message must carry the prompt `{word}`, got:\n{}",
            stdout.lines().take(6).collect::<Vec<_>>().join("\n")
        );
        let requests = mock.wait_for_requests(before + 1);
        assert_eq!(
            requests.len(),
            before + 1,
            "`{word}` must produce exactly one model request (total {} before)",
            requests.len()
        );
        let request = requests.last().expect("the recorded request");
        let user_text = request["messages"]
            .as_array()
            .and_then(|messages| {
                messages
                    .iter()
                    .rev()
                    .find(|message| message["role"] == "user")
            })
            .map(|message| message["content"].clone())
            .unwrap_or(Value::Null);
        let user_text = user_text.to_string();
        assert!(
            user_text.contains(word),
            "the model request must carry `{word}` as user content, got: {user_text}"
        );
    }
}

#[test]
fn binary_bare_management_commands_still_dispatch() {
    let mock = PrintMock::start();
    let sandbox = PrintSandbox::new("bare", &mock);

    for (args, expected) in [
        (vec!["version"], "pir "),
        (vec!["models"], "mock:mock-model"),
        (vec!["config"], "Resource configuration"),
    ] {
        let output = sandbox.run(&args);
        assert!(
            output.status.success(),
            "bare `pir {}` must exit 0; stderr:\n{}",
            args[0],
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.starts_with(expected),
            "bare `pir {}` must keep its output (expected prefix `{expected}`), got:\n{stdout}",
            args[0]
        );
    }
    assert!(
        mock.requests().is_empty(),
        "management commands must not call the model"
    );
}

#[test]
fn binary_documented_print_subcommand_still_prompts() {
    let mock = PrintMock::start();
    let sandbox = PrintSandbox::new("print-sub", &mock);
    let mut args = vec!["print", "--mode", "json"];
    args.extend(P030_PRINT_FLAGS);
    args.push("version");
    let output = sandbox.run(&args);
    assert!(
        output.status.success(),
        "`pir print --mode json version` must exit 0; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = mock.wait_for_requests(1);
    assert_eq!(
        requests.len(),
        1,
        "the documented `pir print <prompt>` spelling must still send one prompt"
    );
}

#[test]
fn binary_rpc_mode_keeps_the_subcommand_grammar() {
    let mock = PrintMock::start();
    let sandbox = PrintSandbox::new("rpc", &mock);
    // `--mode rpc version` still resolves the `version` command and prints the
    // version banner; RPC never starts for it, so the run terminates on its own
    // (the opposite outcome would hang waiting for RPC input).
    let mut command = ProcessCommand::new(env!("CARGO_BIN_EXE_pir"));
    command
        .current_dir(&sandbox.work)
        .env("PIR_AGENT_DIR", &sandbox.agent)
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .args(["--mode", "rpc", "version"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn pir");
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        match child.try_wait().expect("poll the child") {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                panic!("`pir --mode rpc version` must dispatch the version command and exit");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let output = child.wait_with_output().expect("collect output");
    assert!(status.success(), "the version command must exit 0");
    assert!(
        String::from_utf8_lossy(&output.stdout).starts_with("pir "),
        "RPC mode keeps the subcommand grammar: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(mock.requests().is_empty(), "no model traffic for `version`");
}

#[test]
fn binary_help_is_unaffected_by_an_earlier_print_flag() {
    let mock = PrintMock::start();
    let sandbox = PrintSandbox::new("help", &mock);
    for args in [
        vec!["--help"],
        vec!["--print", "--help"],
        vec!["--mode", "json", "--help"],
    ] {
        let output = sandbox.run(&args);
        assert!(
            output.status.success(),
            "`pir {args:?}` must exit 0; stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("Commands:") && stdout.contains("  version "),
            "`pir {args:?}` must print the real command list, got:\n{}",
            stdout.lines().take(6).collect::<Vec<_>>().join("\n")
        );
    }
    assert!(mock.requests().is_empty(), "help must never call the model");
}
