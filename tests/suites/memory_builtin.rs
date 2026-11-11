//! The `memory` builtin (embedded cm-memory engine): registration surface,
//! the session-directory store path, the tool action round trip, auto-recall
//! injection and the read-only setting.

#![cfg(feature = "memory")]

use std::sync::Arc;

use serde_json::{json, Value};

use pir::agent::tool_batch::ToolResult;
use pir::agent::tool_execution::ToolContext;
use pir::plugins::builtin::memory::{MemoryPlugin, COMMAND_NAME, TOOL_NAME};
use pir::plugins::{AgentPlugin, PluginHost, ToolBackend};

fn temp_dir(name: &str) -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("pir-memory-{name}-{stamp}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn host_with_store(
    path: &std::path::Path,
    cwd: &std::path::Path,
) -> (PluginHost, Arc<MemoryPlugin>) {
    let plugin = Arc::new(MemoryPlugin::with_store_path(path, cwd));
    let as_plugin: Arc<dyn AgentPlugin> = plugin.clone();
    (PluginHost::build(None, vec![as_plugin]), plugin)
}

async fn run_tool(host: &PluginHost, args: Value) -> ToolResult {
    run_tool_at(host, args, std::env::temp_dir()).await
}

async fn run_tool_at(
    host: &PluginHost,
    args: Value,
    cwd: impl Into<std::path::PathBuf>,
) -> ToolResult {
    let registry = host.registry();
    let tool = registry
        .resolved_tools()
        .into_iter()
        .find(|(def, _)| def.name == TOOL_NAME)
        .map(|(_, backend)| backend)
        .expect("memory tool registered");
    let ToolBackend::Rust(tool) = tool else {
        panic!("memory must be a native Rust tool");
    };
    tool.run(&args, &ToolContext::new(cwd.into())).await
}

fn parse(result: &ToolResult) -> Value {
    serde_json::from_str(&result.content).expect("memory result is JSON")
}

#[test]
fn registers_the_memory_tool_and_command() {
    let dir = temp_dir("register");
    let (host, _) = host_with_store(&dir.join("cm"), &dir);
    let tools: Vec<String> = host
        .registry()
        .resolved_tools()
        .iter()
        .map(|(def, _)| def.name.clone())
        .collect();
    let commands: Vec<String> = host
        .registry()
        .resolved_commands()
        .iter()
        .map(|command| command.invocation_name.clone())
        .collect();
    assert!(tools.iter().any(|name| name == TOOL_NAME), "{tools:?}");
    assert!(
        commands.iter().any(|name| name == COMMAND_NAME),
        "{commands:?}"
    );
}

#[test]
fn tool_schema_declares_format_output_selector() {
    // `format` selects prompt/json/cmx/jsonl output for recall, context,
    // scan, brief, pack and export. The tool schema is the only gate the
    // model-facing arguments pass (`additionalProperties: false`), so a
    // property missing here makes every documented selector unusable even
    // though `MemoryArgs::format` and the dispatch arms support it.
    let dir = temp_dir("schema-format");
    let (host, _) = host_with_store(&dir.join("cm"), &dir);
    let registry = host.registry();
    let entry = registry
        .resolved_tools()
        .into_iter()
        .find(|entry| entry.0.name == TOOL_NAME)
        .expect("memory tool registered");
    let def = &entry.0;
    assert_eq!(
        def.parameters.get("additionalProperties"),
        Some(&Value::Bool(false))
    );
    let properties = def
        .parameters
        .get("properties")
        .and_then(Value::as_object)
        .expect("memory schema properties");
    let format = properties
        .get("format")
        .expect("memory schema must declare the format selector");
    assert_eq!(format.get("type"), Some(&json!("string")), "{format}");
}

#[tokio::test]
async fn slash_command_status_works() {
    let dir = temp_dir("command");
    let (host, _) = host_with_store(&dir.join("cm"), &dir);
    let outcome = host
        .dispatch_command("memory", "status")
        .await
        .expect("dispatch /memory status");
    assert!(outcome.ok, "{:?}", outcome.error);
    let text = outcome.error.unwrap_or_default();
    assert!(text.contains("\"exists\""), "{text}");
    assert!(text.contains("session"), "{text}");
}

#[tokio::test]
async fn remember_recall_brief_and_frame_round_trip() {
    let dir = temp_dir("roundtrip");
    let store = dir.join("cm");
    let (host, _) = host_with_store(&store, &dir);

    let remember = run_tool(
        &host,
        json!({
            "action": "remember",
            "kind": "decision",
            "scope": "repo",
            "subject": "build command",
            "text": "Build with cargo build --release.",
            "source": "agent:test",
            "confidence": 0.9
        }),
    )
    .await;
    assert!(!remember.is_error, "{}", remember.content);
    let remembered = parse(&remember);
    assert_eq!(remembered["duplicate"], false);
    let id = remembered["id"].as_str().expect("id").to_string();

    let recall = run_tool(
        &host,
        json!({"action": "recall", "query": "build", "budget": 500, "format": "json"}),
    )
    .await;
    assert!(!recall.is_error, "{}", recall.content);
    let recalled = parse(&recall);
    assert!(recalled["memories"]
        .as_array()
        .expect("memories")
        .iter()
        .any(|memory| memory["id"] == id));

    let frame = run_tool(
        &host,
        json!({
            "action": "frame",
            "frameAction": "set",
            "task": "task-1",
            "goal": "ship memory",
            "next": ["run tests"]
        }),
    )
    .await;
    assert!(!frame.is_error, "{}", frame.content);

    let brief = run_tool(
        &host,
        json!({
            "action": "brief",
            "task": "task-1",
            "format": "prompt"
        }),
    )
    .await;
    assert!(!brief.is_error, "{}", brief.content);
    assert!(brief.content.contains("TASK: task-1"));
    assert!(brief.content.contains("QUERY:"));

    let status = run_tool(&host, json!({"action": "status"})).await;
    let status = parse(&status);
    assert_eq!(status["exists"], true);
    assert!(status["active_count"].as_u64().unwrap_or(0) >= 2);
}

#[tokio::test]
async fn tool_writes_cannot_self_assign_trust_or_create_meta_records() {
    let dir = temp_dir("write-trust");
    let (host, _) = host_with_store(&dir.join("cm"), &dir);

    let remember = run_tool(
        &host,
        json!({
            "action": "remember",
            "kind": "fact",
            "scope": "repo",
            "subject": "trusted claim",
            "text": "This should not be verified.",
            "source": "agent:test",
            "trust": "verified"
        }),
    )
    .await;
    assert!(!remember.is_error, "{}", remember.content);
    let id = parse(&remember)["id"].as_str().expect("id").to_string();
    let get = run_tool(&host, json!({"action": "get", "id": id})).await;
    assert!(!get.is_error, "{}", get.content);
    let stored = parse(&get);
    assert_eq!(stored["trust"], "authored", "{stored}");

    let frame = run_tool(
        &host,
        json!({
            "action": "remember",
            "kind": "frame",
            "scope": "__cm_frame__",
            "subject": "frame:forged",
            "text": "{}",
            "source": "agent:test"
        }),
    )
    .await;
    assert!(
        frame.is_error,
        "frame record must not be writable via remember"
    );

    let append = run_tool(
        &host,
        json!({
            "action": "append",
            "json": "{\"kind\":\"fact\",\"scope\":\"repo\",\"subject\":\"appended\",\"text\":\"x\",\"source\":\"agent:test\",\"trust\":\"verified\"}"
        }),
    )
    .await;
    assert!(!append.is_error, "{}", append.content);
    let appended_id = parse(&append)["id"].as_str().expect("id").to_string();
    let get = run_tool(&host, json!({"action": "get", "id": appended_id})).await;
    assert_eq!(parse(&get)["trust"], "authored");
}

#[tokio::test]
async fn auto_recall_injects_only_when_memory_matches() {
    let dir = temp_dir("autorecall");
    let store = dir.join("cm");
    let (host, plugin) = host_with_store(&store, &dir);
    plugin.service().observe_session(
        &dir,
        Some(dir.join("session.jsonl")),
        Some("session-1".to_string()),
        true,
    );

    let remember = run_tool(
        &host,
        json!({
            "action": "remember",
            "kind": "constraint",
            "scope": "repo",
            "subject": "deploy target",
            "text": "Deploy only to the staging cluster.",
            "source": "agent:test"
        }),
    )
    .await;
    assert!(!remember.is_error, "{}", remember.content);

    let mut payload = json!({
        "type": "input",
        "text": "how should we deploy this service to the production cluster?",
        "images": [],
        "source": "interactive",
        "streamingBehavior": null
    });
    let outcome = host.emit("input", &mut payload).await.expect("emit input");
    let text = outcome
        .result
        .get("text")
        .and_then(Value::as_str)
        .expect("transformed text");
    assert!(text.contains("<memory-context"), "{text}");
    assert!(text.contains("staging cluster"), "{text}");

    let mut unrelated = json!({
        "type": "input",
        "text": "write a hello world function that prints a greeting",
        "images": [],
        "source": "interactive",
        "streamingBehavior": null
    });
    let outcome = host
        .emit("input", &mut unrelated)
        .await
        .expect("emit unrelated input");
    assert!(
        outcome.result.get("text").is_none(),
        "unrelated prompt should stay untouched: {:?}",
        outcome.result
    );
}

#[test]
fn session_store_lives_next_to_the_transcript() {
    let dir = temp_dir("session-path");
    let agent_dir = dir.join("agent");
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let plugin = MemoryPlugin::with_paths(&agent_dir, &cwd);
    let session_file = agent_dir
        .join("sessions")
        .join("--work--")
        .join("2026-01-01T00-00-00-000Z_session.jsonl");
    plugin.service().observe_session(
        &cwd,
        Some(session_file.clone()),
        Some("session".to_string()),
        true,
    );
    assert_eq!(
        plugin.service().store_path().expect("store path"),
        session_file.parent().expect("parent").join("cm")
    );
}

#[tokio::test]
async fn auto_policy_skips_short_edits() {
    let dir = temp_dir("auto-small");
    let store = dir.join("cm");
    let (host, plugin) = host_with_store(&store, &dir);
    let remember = run_tool(
        &host,
        json!({
            "action": "remember",
            "kind": "decision",
            "scope": "repo",
            "subject": "cache eviction",
            "text": "Cache is FIFO by insertion order. Updating an existing key must not evict another key.",
            "source": "agent:test"
        }),
    )
    .await;
    assert!(!remember.is_error, "{}", remember.content);
    let frame = run_tool(
        &host,
        json!({
            "action": "frame",
            "frameAction": "set",
            "task": "short-edit",
            "goal": "fix cache tests"
        }),
    )
    .await;
    assert!(!frame.is_error, "{}", frame.content);

    let block = plugin
        .service()
        .auto_recall("Fix the failing cache tests. Keep the public API unchanged.")
        .await;
    assert!(
        block.is_none(),
        "a plain short edit must not auto-inject even with a frame: {block:?}"
    );
}

#[tokio::test]
async fn auto_policy_short_edit_gets_frame_only_when_frame_exists() {
    let dir = temp_dir("auto-frame");
    let store = dir.join("cm");
    let (host, plugin) = host_with_store(&store, &dir);
    let frame = run_tool(
        &host,
        json!({
            "action": "frame",
            "frameAction": "set",
            "task": "short-edit",
            "goal": "rename old_timeout to timeout",
            "next": ["update the config accessor"],
            "constraints": ["keep the tests unchanged"]
        }),
    )
    .await;
    assert!(!frame.is_error, "{}", frame.content);
    let block = plugin
        .service()
        .auto_recall("Session 2: fix the failing config tests. Keep the public API unchanged.")
        .await
        .expect("continuation short edit with a frame should get frame-only context");
    assert!(block.contains("<memory-context"), "{block}");
    assert!(block.contains("TASK: short-edit"), "{block}");
    assert!(block.contains("old_timeout"), "{block}");
    assert!(
        !block.contains("trust=imported") && !block.contains("trust=authored"),
        "frame-only context must not include memory records: {block}"
    );
}

#[tokio::test]
async fn always_policy_keeps_full_brief_for_short_edits() {
    let dir = temp_dir("always-short");
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"memory":{"store":"workspace","autoRecallPolicy":"always"}}"#,
    )
    .expect("settings");
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let plugin = Arc::new(MemoryPlugin::with_paths(&agent_dir, &cwd));
    let as_plugin: Arc<dyn AgentPlugin> = plugin.clone();
    let host = PluginHost::build(None, vec![as_plugin]);
    let remember = run_tool_at(
        &host,
        json!({
            "action": "remember",
            "kind": "decision",
            "scope": "repo",
            "subject": "cache eviction",
            "text": "Cache is FIFO by insertion order.",
            "source": "agent:test"
        }),
        &cwd,
    )
    .await;
    assert!(!remember.is_error, "{}", remember.content);
    let block = plugin
        .service()
        .auto_recall("Fix the failing cache tests. Keep the public API unchanged.")
        .await
        .expect("always policy should still inject");
    assert!(block.contains("<memory-context"), "{block}");
    assert!(block.contains("cache eviction"), "{block}");
}

#[tokio::test]
async fn auto_policy_uses_full_brief_for_questions() {
    let dir = temp_dir("auto-question");
    let store = dir.join("cm");
    let (host, plugin) = host_with_store(&store, &dir);
    let remember = run_tool(
        &host,
        json!({
            "action": "remember",
            "kind": "fact",
            "scope": "repo",
            "subject": "cache eviction",
            "text": "Cache is FIFO by insertion order and updates in place.",
            "source": "agent:test"
        }),
    )
    .await;
    assert!(!remember.is_error, "{}", remember.content);

    let block = plugin
        .service()
        .auto_recall("Explain how the cache eviction policy works.")
        .await
        .expect("question should get the full brief");
    assert!(block.contains("<memory-context"), "{block}");
    assert!(block.contains("QUERY:"), "{block}");
    assert!(block.contains("cache eviction"), "{block}");
}

#[tokio::test]
async fn auto_policy_skips_irrelevant_memory() {
    let dir = temp_dir("auto-irrelevant");
    let store = dir.join("cm");
    let (host, plugin) = host_with_store(&store, &dir);
    let remember = run_tool(
        &host,
        json!({
            "action": "remember",
            "kind": "fact",
            "scope": "repo",
            "subject": "cache eviction",
            "text": "Cache is FIFO by insertion order.",
            "source": "agent:test"
        }),
    )
    .await;
    assert!(!remember.is_error, "{}", remember.content);

    let block = plugin
        .service()
        .auto_recall("Explain the orbital mechanics of Neptune's moons.")
        .await;
    assert!(
        block.is_none(),
        "unrelated query must not inject memory: {block:?}"
    );
}

#[tokio::test]
async fn auto_brief_writes_a_stable_system_prompt_section() {
    let dir = temp_dir("autobrief");
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"memory":{"autoBrief":true}}"#,
    )
    .expect("settings");
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let plugin = Arc::new(MemoryPlugin::with_paths(&agent_dir, &cwd));
    let as_plugin: Arc<dyn AgentPlugin> = plugin.clone();
    let host = PluginHost::build(None, vec![as_plugin]);

    let frame = run_tool(
        &host,
        json!({
            "action": "frame",
            "frameAction": "set",
            "task": "brief-task",
            "goal": "keep the brief stable",
            "next": ["do the thing"]
        }),
    )
    .await;
    assert!(!frame.is_error, "{}", frame.content);

    let mut payload = json!({
        "type": "before_agent_start",
        "messageType": "user",
        "prompt": "hello",
        "systemPrompt": "base prompt",
        "systemPromptOptions": {"sections": {}}
    });
    let outcome = host
        .emit("before_agent_start", &mut payload)
        .await
        .expect("emit before_agent_start");
    let block = payload["systemPromptOptions"]["sections"]["memory_brief"]
        .as_str()
        .unwrap_or_default();
    assert!(block.contains("<memory-brief"), "{block}");
    assert!(block.contains("TASK: brief-task"), "{block}");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
}

#[tokio::test]
async fn read_only_setting_rejects_writes() {
    let dir = temp_dir("readonly");
    let agent_dir = dir.join("agent");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"memory":{"write":"readOnly"}}"#,
    )
    .expect("settings");
    let cwd = dir.join("work");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let plugin = Arc::new(MemoryPlugin::with_paths(&agent_dir, &cwd));
    let as_plugin: Arc<dyn AgentPlugin> = plugin;
    let host = PluginHost::build(None, vec![as_plugin]);
    let result = run_tool(
        &host,
        json!({
            "action": "remember",
            "kind": "fact",
            "scope": "repo",
            "subject": "nope",
            "text": "must not be written",
            "source": "agent:test"
        }),
    )
    .await;
    assert!(result.is_error, "{}", result.content);
    assert!(result.content.contains("read-only"), "{}", result.content);
}

#[tokio::test]
async fn wiki_action_plans_pages_through_the_tool() {
    let dir = temp_dir("wiki-action");
    let cwd = dir.join("project");
    std::fs::create_dir_all(&cwd).expect("project");
    std::fs::write(
        cwd.join("README.md"),
        "# Wiki Action Demo\n\nA minimal repository for the memory tool's wiki action.\n",
    )
    .expect("readme");
    let (host, _) = host_with_store(&dir.join("cm"), &cwd);
    let result = run_tool_at(
        &host,
        json!({"action": "wiki", "wikiAction": "plan", "root": ".", "out": ".cm/wiki"}),
        &cwd,
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    let value = parse(&result);
    assert!(
        value["pages"]
            .as_array()
            .is_some_and(|pages| !pages.is_empty()),
        "{value}"
    );
}

#[tokio::test]
async fn relative_source_resolves_against_session_cwd_not_git_root() {
    // Layout: outer/.git marks the VCS root; the session runs in outer/nested
    // and the cited file lives there. `path:line` sources must resolve against
    // the session cwd (the root the write/edit tools use), not the nearest
    // `.git` ancestor.
    let dir = temp_dir("nested-source");
    let outer = dir.join("outer");
    let cwd = outer.join("nested");
    std::fs::create_dir_all(outer.join(".git")).expect("git marker");
    std::fs::create_dir_all(&cwd).expect("nested cwd");
    std::fs::write(
        cwd.join("release-checklist.md"),
        "# Checklist\nRun the smoke suite before tagging.\n",
    )
    .expect("source file");

    let (host, _) = host_with_store(&dir.join("cm"), &cwd);

    let remember = run_tool_at(
        &host,
        json!({
            "action": "remember",
            "kind": "fact",
            "scope": "project",
            "subject": "release-checklist",
            "text": "Run the smoke suite before tagging.",
            "source": "release-checklist.md:1",
            "confidence": 0.9
        }),
        &cwd,
    )
    .await;
    assert!(!remember.is_error, "{}", remember.content);

    let recall = run_tool_at(
        &host,
        json!({"action": "recall", "query": "release checklist", "format": "json"}),
        &cwd,
    )
    .await;
    assert!(!recall.is_error, "{}", recall.content);
    let recalled = parse(&recall);
    assert!(
        recalled["memories"]
            .as_array()
            .is_some_and(|memories| memories
                .iter()
                .any(|memory| memory["subject"] == "release-checklist")),
        "{recalled}"
    );

    // Confinement still holds: the same relative resolution must reject a
    // source that escapes the session cwd.
    let escape = run_tool_at(
        &host,
        json!({
            "action": "remember",
            "kind": "fact",
            "scope": "project",
            "subject": "escape",
            "text": "must not be written",
            "source": "../outside.md:1"
        }),
        &cwd,
    )
    .await;
    assert!(escape.is_error, "{}", escape.content);
    assert!(escape.content.contains("source"), "{}", escape.content);
}

#[tokio::test]
async fn nested_cwd_root_actions_share_the_session_cwd() {
    // PIR-BIN-005: `root`-taking memory actions (brief/pack/audit/explore/wiki)
    // must default to the session cwd (the cm CLI's `--root .`), the same base
    // the strict `SourcePolicy` uses for `path:symbol` sources. They used to
    // anchor at the nearest `.git` ancestor, so in `outer/.git` + cwd
    // `outer/nested` a remembered `release-checklist.md:Run` produced no
    // snippet and was reported as an unsafe source.
    let dir = temp_dir("nested-roots");
    let outer = dir.join("outer");
    let cwd = outer.join("nested");
    std::fs::create_dir_all(outer.join(".git")).expect("git marker");
    std::fs::create_dir_all(&cwd).expect("nested cwd");
    std::fs::write(
        cwd.join("release-checklist.md"),
        "# Release checklist\n\nRun the smoke suite before tagging.\nDeploy after checks pass.\n",
    )
    .expect("source file");

    let (host, _) = host_with_store(&dir.join("cm"), &cwd);
    let remember = run_tool_at(
        &host,
        json!({
            "action": "remember",
            "kind": "fact",
            "scope": "project",
            "subject": "release-checklist-run",
            "text": "Run the smoke suite before tagging.",
            "source": "release-checklist.md:Run",
            "confidence": 0.9
        }),
        &cwd,
    )
    .await;
    assert!(!remember.is_error, "{}", remember.content);

    let brief = run_tool_at(
        &host,
        json!({"action": "brief", "query": "release checklist", "snippetLines": 2, "format": "json"}),
        &cwd,
    )
    .await;
    assert!(!brief.is_error, "{}", brief.content);
    let brief = parse(&brief);
    let snippet = brief["memories"][0]["snippet"].as_str().unwrap_or_else(|| {
        panic!("brief must resolve the snippet against the session cwd: {brief}")
    });
    assert!(snippet.contains("smoke suite"), "{snippet}");
    assert!(
        brief["prompt"]
            .as_str()
            .is_some_and(|prompt| prompt.contains("Deploy after checks pass.")),
        "{brief}"
    );

    let audit = run_tool_at(&host, json!({"action": "audit", "limit": 20}), &cwd).await;
    assert!(!audit.is_error, "{}", audit.content);
    let audit = parse(&audit);
    assert_eq!(audit["issue_count"], json!(0), "{audit}");
    let audit_root = audit["root"].as_str().expect("audit root");
    assert!(
        audit_root
            .to_ascii_lowercase()
            .replace('/', "\\")
            .ends_with(r"outer\nested"),
        "{audit_root}"
    );

    let explore = run_tool_at(
        &host,
        json!({"action": "explore", "exploreAction": "plan", "query": "release checklist"}),
        &cwd,
    )
    .await;
    assert!(!explore.is_error, "{}", explore.content);
    let explore = parse(&explore);
    let prompt = explore["prompt"].as_str().expect("explore prompt");
    assert!(
        prompt.contains("ROOT: ") && prompt.contains("nested"),
        "{prompt}"
    );

    let wiki = run_tool_at(&host, json!({"action": "wiki", "wikiAction": "plan"}), &cwd).await;
    assert!(!wiki.is_error, "{}", wiki.content);
    let wiki = parse(&wiki);
    let wiki_root = wiki["root"].as_str().expect("wiki root");
    let wiki_out = wiki["out"].as_str().expect("wiki out");
    assert!(
        wiki_root
            .to_ascii_lowercase()
            .replace('/', "\\")
            .ends_with(r"outer\nested"),
        "{wiki_root}"
    );
    assert!(
        wiki_out
            .to_ascii_lowercase()
            .replace('/', "\\")
            .ends_with(r"outer\nested\.cm\wiki"),
        "{wiki_out}"
    );

    // Confinement is unchanged: an explicit root still cannot leave the
    // session cwd/project base.
    let escape = run_tool_at(
        &host,
        json!({"action": "explore", "exploreAction": "plan", "root": "..", "query": "x"}),
        &cwd,
    )
    .await;
    assert!(escape.is_error, "{}", escape.content);
    assert!(escape.content.contains("escapes"), "{}", escape.content);
}
