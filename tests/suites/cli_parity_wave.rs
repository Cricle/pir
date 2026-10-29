//! Feature-parity wave (2026-10-10): the `pir update` target flags
//! (`--models`/`--extensions`/`--all`, upstream package-manager-cli.ts:526-605),
//! the `pir mcp add`/`remove` subcommands (upstream extensions/mcp/cli.ts:33-73),
//! and the `pir auth` flag parity (`--model` on check/print-api-key,
//! `--provider` on print-bearer-token, upstream cli/auth-command.ts:51-61).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use clap::Parser;
use pir::coding_agent::cli::{preprocess_args, AuthCommand, Cli, Command, McpCommand};

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
