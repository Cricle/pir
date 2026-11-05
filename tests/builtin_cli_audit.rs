//! Builtin CLI/registry audit: every builtin loads explicitly and exposes its
//! expected tool/command surface, including phone's default-off/replaceable
//! behavior and unknown-builtin diagnostics.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::plugins::PluginHost;

fn plan_only_handle(plan: &[PathBuf]) -> ExtensionHandle {
    let handle = ExtensionHandle::start_plan_only(ExtensionConfig {
        cwd: std::env::current_dir().unwrap(),
        mode: "print".to_owned(),
        has_ui: false,
        ..Default::default()
    });
    handle.set_path_plan(plan.to_vec());
    handle
}

fn host_for(plan: &[PathBuf]) -> PluginHost {
    PluginHost::build(Some(plan_only_handle(plan)), Vec::new())
}

fn names(host: &PluginHost) -> (Vec<String>, Vec<String>, Vec<String>) {
    (
        host.plugins()
            .iter()
            .map(|p| p.info().path.clone())
            .collect(),
        host.registry()
            .resolved_tools()
            .iter()
            .map(|(def, _)| def.name.clone())
            .collect(),
        host.registry()
            .resolved_commands()
            .iter()
            .map(|c| c.invocation_name.clone())
            .collect(),
    )
}

#[test]
fn every_builtin_loads_explicitly_and_exposes_its_surface() {
    let cases: &[(&str, &[&str], &[&str])] = &[
        ("subagent", &["subagent"], &[]),
        ("mcp", &["list_mcp_resources"], &["mcp"]),
        ("codemode", &["codemode"], &[]),
        ("python", &["python"], &[]),
        ("context-mode", &["ctx_execute", "ctx_search"], &[]),
        ("tool-search", &["tool_search"], &[]),
        ("tool-repair", &[], &[]),
        ("todo", &["todo"], &["todos"]),
        (
            "phone",
            &[],
            &["phone-start", "phone-stop", "phone-status", "phone-token"],
        ),
    ];
    for (name, tools, commands) in cases {
        let host = host_for(&[PathBuf::from(format!("builtin:{name}"))]);
        let (plugins, actual_tools, actual_commands) = names(&host);
        assert_eq!(plugins, vec![format!("builtin:{name}")], "{name}");
        for tool in *tools {
            assert!(
                actual_tools.iter().any(|t| t == tool),
                "{name}: missing tool {tool}: {actual_tools:?}"
            );
        }
        for command in *commands {
            assert!(
                actual_commands.iter().any(|c| c == command),
                "{name}: missing command {command}: {actual_commands:?}"
            );
        }
        let registry = host.registry();
        assert!(
            !registry
                .diagnostics()
                .iter()
                .any(|d| d.kind == pir::plugins::DiagnosticKind::Error),
            "{name}: {:?}",
            registry.diagnostics()
        );
    }
}

#[test]
fn default_plan_is_all_default_enabled_builtins_but_phone() {
    let plan = pir::plugins::order_extension_paths(&[], &[], false);
    let host = host_for(&plan);
    let (plugins, tools, _) = names(&host);
    assert_eq!(
        plugins,
        vec![
            "builtin:subagent",
            "builtin:mcp",
            "builtin:codemode",
            "builtin:python",
            "builtin:context-mode",
            "builtin:tool-search",
            "builtin:tool-repair",
            "builtin:todo",
        ]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>()
    );
    for tool in ["subagent", "python", "ctx_execute", "todo"] {
        assert!(tools.iter().any(|t| t == tool), "missing {tool}: {tools:?}");
    }
    assert!(!plugins.iter().any(|p| p == "builtin:phone"));
}

#[test]
fn disabled_builtins_and_only_builtin_and_unknown_diagnostic() {
    for name in pir::plugins::builtin_names() {
        let plan = pir::plugins::order_extension_paths(
            &[PathBuf::from(format!("builtin:{name}"))],
            &[],
            false,
        );
        let disabled: BTreeSet<String> = [name.to_owned()].into_iter().collect();
        let filtered = pir::plugins::filter_disabled_builtins(&plan, &disabled);
        assert!(
            !filtered
                .iter()
                .any(|p| p.to_string_lossy() == format!("builtin:{name}")),
            "{name} survived disabled filter"
        );
    }

    let only = pir::plugins::order_extension_paths(&[PathBuf::from("builtin:python")], &[], true);
    assert_eq!(only, vec![PathBuf::from("builtin:python")]);
    let (plugins, _, _) = names(&host_for(&only));
    assert_eq!(plugins, vec!["builtin:python".to_owned()]);

    let host = host_for(&[PathBuf::from("builtin:nope")]);
    let registry = host.registry();
    assert!(
        registry.diagnostics().iter().any(|d| d
            .message
            .contains("Unknown built-in extension: builtin:nope")),
        "{:?}",
        registry.diagnostics()
    );
}

// --- Real print-mode exposure against a loopback mock provider -------------

fn spawn_mock() -> (u16, Arc<Mutex<Vec<String>>>) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let bodies2 = Arc::clone(&bodies);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            let header_end = loop {
                if let Some(i) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(i + 4);
                }
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break None,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            };
            let Some(header_end) = header_end else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
            let len = headers
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while request.len() < header_end + len {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let body = String::from_utf8_lossy(&request[header_end..]).to_string();
            bodies2.lock().unwrap().push(body);
            let delta = serde_json::json!({"role":"assistant","content":"OK"});
            let chunk = serde_json::json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"mock-model","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
            let done = serde_json::json!({"id":"x","object":"chat.completion.chunk","created":1,"model":"mock-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]});
            let events = format!("data: {chunk}\n\ndata: {done}\n\ndata: [DONE]\n\n");
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", events.len(), events);
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (port, bodies)
}

fn write_agent(dir: &std::path::Path, port: u16, settings: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("models.json"),
        format!(r#"{{"providers":{{"mock":{{"name":"Mock","baseUrl":"http://127.0.0.1:{port}/v1","api":"openai-completions","apiKey":"k","models":[{{"id":"mock-model","name":"M","api":"openai-completions","baseUrl":"http://127.0.0.1:{port}/v1","reasoning":false,"input":["text"],"contextWindow":100000,"maxTokens":1024}}]}}}}}}"#),
    ).unwrap();
    std::fs::write(dir.join("settings.json"), settings).unwrap();
}

fn run_print(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_pir"))
        .current_dir(dir)
        .env("PIR_AGENT_DIR", dir)
        .env("HOME", dir)
        .env("USERPROFILE", dir)
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .args(args)
        .output()
        .expect("run pir")
}

fn wire_tools(
    dir: &std::path::Path,
    bodies: &Arc<Mutex<Vec<String>>>,
    extra: &[&str],
) -> Vec<String> {
    let mut args = vec![
        "print",
        "hi",
        "--no-session",
        "--no-context-files",
        "--provider",
        "mock",
        "--model",
        "mock-model",
    ];
    args.extend_from_slice(extra);
    let output = run_print(dir, &args);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body = bodies
        .lock()
        .unwrap()
        .first()
        .cloned()
        .expect("captured request");
    let json: serde_json::Value = serde_json::from_str(&body).expect("request body");
    // `--no-tools` omits the wire field entirely rather than sending `[]`.
    json.get("tools")
        .and_then(serde_json::Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(|t| t["function"]["name"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn print_default_and_selected_wire_surface() {
    let (port, bodies) = spawn_mock();
    let dir = std::env::temp_dir().join(format!("pir-zz-builtin-a-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_agent(&dir, port, "{}");
    let tools = wire_tools(&dir, &bodies, &[]);
    for expected in ["subagent", "python", "ctx_execute", "ctx_search", "todo"] {
        assert!(
            tools.iter().any(|t| t == expected),
            "missing {expected}: {tools:?}"
        );
    }
    assert!(!tools.iter().any(|t| t == "codemode"), "{tools:?}");
    assert!(!tools.iter().any(|t| t == "tool_search"), "{tools:?}");
    assert!(!tools.iter().any(|t| t.starts_with("phone")), "{tools:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn print_tools_flag_activates_codemode_and_tool_search() {
    let (port, bodies) = spawn_mock();
    let dir = std::env::temp_dir().join(format!("pir-zz-builtin-b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_agent(&dir, port, "{}");
    let tools = wire_tools(&dir, &bodies, &["--tools", "codemode,tool_search"]);
    for expected in ["codemode", "tool_search"] {
        assert!(
            tools.iter().any(|t| t == expected),
            "missing {expected}: {tools:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn print_settings_disable_and_no_mcp_and_unknown_builtin() {
    let (port, bodies) = spawn_mock();
    let dir = std::env::temp_dir().join(format!("pir-zz-builtin-c-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_agent(
        &dir,
        port,
        r#"{"disabledBuiltinExtensions":["python","context-mode","todo","subagent"]}"#,
    );
    let tools = wire_tools(&dir, &bodies, &["--no-mcp"]);
    for gone in ["python", "ctx_execute", "ctx_search", "todo", "subagent"] {
        assert!(
            !tools.iter().any(|t| t == gone),
            "{gone} survived: {tools:?}"
        );
    }
    // todo's /todos command is gone with the builtin; core tools remain.
    assert!(tools.iter().any(|t| t == "bash"), "{tools:?}");

    let output = run_print(
        &dir,
        &[
            "print",
            "hi",
            "--no-session",
            "--no-context-files",
            "--provider",
            "mock",
            "--model",
            "mock-model",
            "-e",
            "builtin:nope",
            "--only-builtin",
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Unknown built-in extension: builtin:nope"),
        "status={:?} stderr={stderr}",
        output.status
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn print_explicit_e_builtin_each_loads_without_error() {
    let (port, _bodies) = spawn_mock();
    let dir = std::env::temp_dir().join(format!("pir-zz-builtin-e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_agent(&dir, port, "{}");
    for name in pir::plugins::builtin_names() {
        let output = run_print(
            &dir,
            &[
                "print",
                "hi",
                "--no-session",
                "--no-context-files",
                "--provider",
                "mock",
                "--model",
                "mock-model",
                "--only-builtin",
                "-e",
                &format!("builtin:{name}"),
            ],
        );
        assert!(
            output.status.success(),
            "{name}: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn print_no_tools_flags_keep_extension_tool_semantics() {
    let (port, bodies) = spawn_mock();
    let dir = std::env::temp_dir().join(format!("pir-zz-builtin-f-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_agent(&dir, port, "{}");
    let tools = wire_tools(&dir, &bodies, &["--no-builtin-tools"]);
    assert!(
        tools.iter().any(|t| t == "todo"),
        "--no-builtin-tools dropped extension tool: {tools:?}"
    );
    assert!(!tools.iter().any(|t| t == "bash"), "{tools:?}");
    bodies.lock().unwrap().clear();
    let tools = wire_tools(&dir, &bodies, &["--no-tools"]);
    assert!(tools.is_empty(), "--no-tools declared tools: {tools:?}");
    bodies.lock().unwrap().clear();
    let tools = wire_tools(&dir, &bodies, &["--tools", "todo"]);
    assert_eq!(tools, vec!["todo".to_owned()], "{tools:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
