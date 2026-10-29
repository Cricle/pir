//! P0b wave: `@earendil-works/pi-coding-agent` full value-export surface
//! mirror lock tests.
//!
//! Fixture extensions are seeded from source strings at runtime (the repo
//! hygiene gate forbids tracked `.ts` files), mirroring tests/tui_shims.rs.
//! Three probes: (1) every upstream value export is reachable as a named
//! export AND a member of the default export object, (2) the real helper
//! ports behave like the upstream logic, (3) the host-coupled skeletons
//! throw the documented error instead of failing silently.

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionHandle, ExtensionRuntime};
use serde_json::{json, Value};

fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-coding-agent-shims-{}-{test_name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    for (name, content) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture subdir");
        }
        std::fs::write(&path, content).expect("write fixture");
    }
    dir
}

fn test_config_at(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

async fn new_runtime_at(dir: &Path) -> ExtensionRuntime {
    let rt = ExtensionRuntime::new(test_config_at(dir))
        .await
        .expect("runtime construction");
    rt.allow_root(dir.to_path_buf());
    rt
}

async fn run_seeded(name: &str, fixture: &str) -> (PathBuf, ExtensionRuntime) {
    let dir = write_temp_fixture(name, &[]);
    let rt = new_runtime_at(&dir).await;
    let module = format!("pir-ext:{name}.ts");
    rt.seed_module(&module, fixture).expect("seed fixture");
    let loaded = rt.seeded_load(&module).await;
    assert_eq!(loaded.expect("seeded load"), 1, "fixture must load");

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors in {name}: {:?}",
        outcome.errors
    );
    (dir, rt)
}

// ---------------------------------------------------------------------------
// 1. Surface coverage: the full upstream value-export list (154 names).
// ---------------------------------------------------------------------------

const CODING_AGENT_SURFACE_TS: &str = r##"
import * as CaModule from "@earendil-works/pi-coding-agent";
import CaDefault from "@earendil-works/pi-coding-agent";

// The upstream value-export list (packages/coding-agent/src/index.ts),
// embedded as the regression const.
const EXPECTED_NAMES: string[] = [
  "AgentSession", "AgentSessionRuntime", "ArminComponent", "AssistantMessageComponent",
  "BashExecutionComponent", "BorderedLoader", "BranchSummaryMessageComponent",
  "CONFIG_DIR_NAME", "CURRENT_SESSION_VERSION", "CompactionSummaryMessageComponent",
  "CredentialSynchronizationError", "CustomEditor", "CustomMessageComponent",
  "DEFAULT_COMPACTION_SETTINGS", "DEFAULT_MAX_BYTES", "DEFAULT_MAX_LINES",
  "DefaultPackageManager", "DefaultResourceLoader", "DynamicBorder",
  "ExtensionEditorComponent", "ExtensionInputComponent", "ExtensionRunner",
  "ExtensionSelectorComponent", "FooterComponent", "InteractiveMode",
  "LoginDialogComponent", "ModelRegistry", "ModelRuntime", "ModelSelectorComponent",
  "OAuthSelectorComponent", "ProjectTrustStore", "RpcClient", "SessionManager",
  "SessionSelectorComponent", "SettingsManager", "SettingsSelectorComponent",
  "ShowImagesSelectorComponent", "SkillInvocationMessageComponent", "Theme",
  "ThemeSelectorComponent", "ThinkingSelectorComponent", "ToolExecutionComponent",
  "TreeSelectorComponent", "UserMessageComponent", "UserMessageSelectorComponent",
  "VERSION", "VIRTUAL_MODEL_STATE_ENTRY", "buildContextEntries", "buildSessionContext",
  "buildSessionProjection", "calculateContextTokens", "collectEntriesForBranchSummary",
  "compact", "convertToLlm", "convertToPng", "copyToClipboard",
  "createAgentSessionFromServices", "createAgentSessionRuntime",
  "createAgentSessionServices", "createBashTool", "createBashToolDefinition",
  "createCodemodeExtension", "createEditTool", "createEditToolDefinition",
  "createEventBus", "createExtensionRuntime", "createFindTool",
  "createFindToolDefinition", "createGrepTool", "createGrepToolDefinition",
  "createLocalBashOperations", "createLocalPowerShellOperations", "createLsTool",
  "createLsToolDefinition", "createMcpExtension", "createPowerShellTool",
  "createPowerShellToolDefinition", "createReadOnlyTools", "createReadTool",
  "createReadToolDefinition", "createSyntheticSourceInfo",
  "createToolSearchExtension", "createWriteTool", "createWriteToolDefinition",
  "defineTool", "detectSupportedImageMimeTypeFromFile", "discoverAndLoadExtensions",
  "estimateTokens", "findCutPoint", "findTurnStartIndex", "formatDimensionNote",
  "formatSize", "formatSkillsForPrompt", "generateBranchSummary",
  "generateDiffString", "generateSummary", "generateSummaryWithUsage",
  "generateUnifiedPatch", "getAgentDir", "getDocsPath", "getExamplesPath",
  "getLanguageFromPath", "getLastAssistantUsage", "getLatestCompactionEntry",
  "getMarkdownTheme", "getPackageDir", "getPowerShellConfig", "getReadmePath",
  "getSelectListTheme", "getSettingsListTheme", "getShellConfig",
  "hasTrustRequiringProjectResources", "highlightCode", "initTheme",
  "isBashToolResult", "isEditToolResult", "isFindToolResult", "isGrepToolResult",
  "isLsToolResult", "isPowerShellToolResult", "isReadToolResult",
  "isToolCallEventType", "isWriteToolResult", "keyHint", "keyText",
  "loadProjectContextFiles", "loadSkills", "loadSkillsFromDir", "main",
  "migrateSessionEntries", "parseArgs", "parseFrontmatter", "parseSessionEntries",
  "parseSkillBlock", "prepareBranchEntries", "rawKeyHint", "readStoredCredential",
  "renderDiff", "resizeImage", "resolveCliModel", "resolveModelScopeWithDiagnostics",
  "runPrintMode", "runRpcMode", "serializeConversation", "sessionEntryToContextMessages",
  "shouldCompact", "stripFrontmatter", "truncateHead", "truncateLine", "truncateTail",
  "truncateToVisualLines", "withFileMutationQueue", "wrapRegisteredTool",
  "wrapRegisteredTools",
];

export default (pi: any) => {
  pi.on("session_start", async () => {
    const missing: string[] = [];
    const notInDefault: string[] = [];
    for (const name of EXPECTED_NAMES) {
      if ((CaModule as any)[name] === undefined) missing.push(name);
      if ((CaDefault as any)[name] === undefined) notInDefault.push(name);
    }
    globalThis.__caCoverage = {
      count: EXPECTED_NAMES.length,
      missing,
      notInDefault,
      defaultIsObject: typeof CaDefault === "object" && CaDefault !== null,
      legacyFace: [
        typeof CaModule.defineTool === "function",
        typeof CaModule.SessionManager === "function",
        typeof CaModule.Theme === "function",
        typeof CaModule.DynamicBorder === "function",
      ],
    };
  });
};
"##;

#[tokio::test(flavor = "current_thread")]
async fn coding_agent_shims_surface_coverage() {
    let (dir, rt) = run_seeded("ca-coverage", CODING_AGENT_SURFACE_TS).await;
    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__caCoverage)")
        .await
        .expect("read coverage results");

    assert_eq!(
        v["count"], 154,
        "the embedded list must have 154 names: {v}"
    );
    assert_eq!(v["missing"], json!([]), "missing named exports: {v}");
    assert_eq!(
        v["notInDefault"],
        json!([]),
        "names absent from default export: {v}"
    );
    assert_eq!(v["defaultIsObject"], true, "{v}");
    assert_eq!(v["legacyFace"], json!([true, true, true, true]), "{v}");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 2. Behaviour probes: real helper ports must behave like the upstream logic.
// ---------------------------------------------------------------------------

const CODING_AGENT_BEHAVIOUR_TS: &str = r##"
import {
  CONFIG_DIR_NAME, CURRENT_SESSION_VERSION, DEFAULT_COMPACTION_SETTINGS, DEFAULT_MAX_BYTES,
  DEFAULT_MAX_LINES, VIRTUAL_MODEL_STATE_ENTRY, buildSessionContext, calculateContextTokens,
  defineTool, estimateTokens, formatSize, getAgentDir, isToolCallEventType, isToolResult,
  keyText, parseFrontmatter, serializeConversation, shouldCompact, stripFrontmatter,
  truncateTail, withFileMutationQueue,
} from "@earendil-works/pi-coding-agent";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    out.defineToolPassthrough = defineTool({ name: "probe" }).name;
    out.eventTypeNamedFirst = isToolCallEventType("bash", { type: "tool_call", toolName: "bash" });
    out.eventTypeEventFirst = isToolCallEventType({ type: "tool_call", toolName: "bash" }, "bash");
    out.eventTypeOtherTool = isToolCallEventType("run", { type: "tool_call", toolName: "bash" });
    out.eventTypeNotCall = isToolCallEventType("bash", { type: "tool_result" });
    out.isToolResult = isToolResult({ content: [] });
    out.isToolResultNull = isToolResult(null);

    out.configDir = CONFIG_DIR_NAME;
    out.sessionVersion = CURRENT_SESSION_VERSION;
    out.maxLines = DEFAULT_MAX_LINES;
    out.maxBytes = DEFAULT_MAX_BYTES;
    out.reserveTokens = DEFAULT_COMPACTION_SETTINGS.reserveTokens;
    out.virtualModelState = VIRTUAL_MODEL_STATE_ENTRY;

    const fm = parseFrontmatter("---\nname: probe\nenabled: true\n---\nbody line");
    out.frontmatterName = fm.frontmatter.name;
    out.frontmatterBody = fm.body;
    out.stripFrontmatter = stripFrontmatter("---\nname: probe\n---\nbody line");
    out.noFrontmatter = parseFrontmatter("plain").body;

    out.estimateText = estimateTokens({ role: "user", content: "abcd" });
    out.estimateBlocks = estimateTokens({
      role: "assistant",
      content: [{ type: "text", text: "abcdefgh" }],
    });
    out.contextTokensSum = calculateContextTokens({ input: 1, output: 2, cacheRead: 3, cacheWrite: 4 });
    out.contextTokensTotal = calculateContextTokens({ totalTokens: 7 });
    out.contextTokensEmpty = calculateContextTokens(null);

    out.formatSizeBytes = formatSize(512);
    out.formatSizeKb = formatSize(2048);

    out.truncateSmall = truncateTail("a\nb\nc", { maxLines: 2, maxBytes: 1024 }).truncated;

    out.keyTextUnknown = keyText("no-such-binding");
    out.agentDirType = typeof getAgentDir();

    const ctx = buildSessionContext([
      { type: "message", message: { role: "user", content: "hi" } },
      { type: "other" },
    ]);
    out.sessionContextMessages = ctx.messages.length;
    out.sessionContextRole = ctx.messages[0] ? ctx.messages[0].role : null;

    out.serialized = serializeConversation([{ role: "user", content: "hello" }]);

    out.shouldCompactOff = shouldCompact(10, 100, { enabled: false, reserveTokens: 16384, keepRecentTokens: 20000 });
    out.mutationQueue = await withFileMutationQueue("probe", async () => "done");

    globalThis.__caBehaviour = out;
  });
};

"##;

#[tokio::test(flavor = "current_thread")]
async fn coding_agent_shims_behaviour_probes() {
    let (dir, rt) = run_seeded("ca-behaviour", CODING_AGENT_BEHAVIOUR_TS).await;
    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__caBehaviour)")
        .await
        .expect("read behaviour probes");

    assert_eq!(v["defineToolPassthrough"], "probe", "{v}");
    assert_eq!(v["eventTypeNamedFirst"], true, "{v}");
    assert_eq!(v["eventTypeEventFirst"], true, "{v}");
    assert_eq!(v["eventTypeOtherTool"], false, "{v}");
    assert_eq!(v["eventTypeNotCall"], false, "{v}");
    assert_eq!(v["isToolResult"], true, "{v}");
    assert_eq!(v["isToolResultNull"], false, "{v}");

    assert_eq!(v["configDir"], ".pi", "{v}");
    assert_eq!(v["sessionVersion"], 3, "{v}");
    assert_eq!(v["maxLines"], 2000, "{v}");
    assert_eq!(v["maxBytes"], 51200, "{v}");
    assert_eq!(v["reserveTokens"], 16384, "{v}");
    assert_eq!(v["virtualModelState"], "pi.virtual-model-state", "{v}");

    assert_eq!(v["frontmatterName"], "probe", "{v}");
    assert_eq!(v["frontmatterBody"], "body line", "{v}");
    assert_eq!(v["stripFrontmatter"], "body line", "{v}");
    assert_eq!(v["noFrontmatter"], "plain", "{v}");

    assert_eq!(v["estimateText"], 1, "{v}");
    assert_eq!(v["estimateBlocks"], 2, "{v}");
    assert_eq!(v["contextTokensSum"], 10, "{v}");
    assert_eq!(v["contextTokensTotal"], 7, "{v}");
    assert_eq!(v["contextTokensEmpty"], 0, "{v}");

    assert_eq!(v["formatSizeBytes"], "512B", "{v}");
    assert_eq!(v["formatSizeKb"], "2.0KB", "{v}");

    assert_eq!(v["truncateSmall"], true, "{v}");
    assert_eq!(v["keyTextUnknown"], "", "{v}");
    assert_eq!(v["agentDirType"], "string", "{v}");

    assert_eq!(v["sessionContextMessages"], 1, "{v}");
    assert_eq!(v["sessionContextRole"], "user", "{v}");
    assert!(
        v["serialized"]
            .as_str()
            .unwrap_or_default()
            .contains("[User]: hello"),
        "{v}"
    );
    assert_eq!(v["shouldCompactOff"], false, "{v}");
    assert_eq!(v["mutationQueue"], "done", "{v}");

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 3. Throw probes: host-coupled skeletons fail loudly with the documented
//    message instead of returning undefined.
// ---------------------------------------------------------------------------

const CODING_AGENT_THROW_TS: &str = r##"
import { AgentSession, AgentSessionRuntime, ModelRuntime, RpcClient } from "@earendil-works/pi-coding-agent";

function err(fn: () => any): string | null {
  try {
    fn();
    return null;
  } catch (error) {
    return String((error as any).message);
  }
}

export default (pi: any) => {
  pi.on("session_start", async () => {
    const session = new AgentSession({});
    const rpc = new RpcClient({});
    const runtime = new ModelRuntime();
    const sessionRuntime = new AgentSessionRuntime(session, {}, null, {});

    globalThis.__caThrows = {
      constructs: true,
      prompt: err(() => session.prompt("hi")),
      abort: err(() => session.abort()),
      rpcStart: err(() => rpc.start()),
      rpcPrompt: err(() => rpc.prompt("hi")),
      modelStream: err(() => runtime.stream({})),
      modelCreate: err(() => (ModelRuntime as any).create()),
      runtimeDispose: err(() => sessionRuntime.dispose()),
    };
  });
};
"##;

#[tokio::test(flavor = "current_thread")]
async fn coding_agent_shims_throw_probes() {
    let (dir, rt) = run_seeded("ca-throws", CODING_AGENT_THROW_TS).await;
    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__caThrows)")
        .await
        .expect("read throw probes");

    assert_eq!(
        v["constructs"], true,
        "skeleton constructors must not throw: {v}"
    );
    // `ModelRuntime.create` and the model/credential faces with a host seam are
    // real now (section 4); a bare `new ModelRuntime()` instance stays unbound
    // and keeps the loud error on the completion faces.
    for key in [
        "prompt",
        "abort",
        "rpcStart",
        "rpcPrompt",
        "modelStream",
        "runtimeDispose",
    ] {
        let message = v[key].as_str().unwrap_or_default();
        assert!(
            message.contains("is not available in the pir extension runtime"),
            "{key} must throw the documented stub error: {v}"
        );
    }
    assert_eq!(
        v["modelCreate"],
        Value::Null,
        "ModelRuntime.create is a real async factory and must not throw: {v}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 4. Real host-backed SDK runtime (pi-subagents child-session chain):
//    SettingsManager, ModelRuntime, DefaultResourceLoader
// ---------------------------------------------------------------------------

/// ExtensionConfig with cwd anchored at `dir` (same shape as `test_config_at`).
fn handle_config_at(dir: &Path) -> ExtensionConfig {
    test_config_at(dir)
}

/// Load a fixture extension through the actor-backed handle (the path the
/// host uses), so the tests can push the host state the shim reads
/// (`update_model_registry`, `set_complete_fn`, ...).
async fn start_handle(dir: &Path, fixture: &str) -> ExtensionHandle {
    let path = dir.join("sdk-runtime-probe.ts");
    std::fs::write(&path, fixture).expect("write probe extension");
    let handle = ExtensionHandle::start(handle_config_at(dir)).expect("runtime start");
    handle.set_path_plan(vec![path.clone()]);
    let load = handle.load(&[path]).await.expect("load probe extension");
    assert!(
        load.errors.is_empty(),
        "probe load errors: {:?}",
        load.errors
    );
    handle
}

async fn read_probe(handle: &ExtensionHandle) -> Value {
    handle
        .eval_json("JSON.stringify(globalThis.__pirSdkRuntime)")
        .await
        .expect("read probe results")
}

/// `SettingsManager` is real: global + trust-gated project `settings.json`
/// through the host fs bridge, upstream merge rules (`defaultTools` modifiers,
/// `queueMode` -> `steeringMode` migration), and a write path that preserves
/// unknown keys in the file.
#[tokio::test(flavor = "current_thread")]
async fn settings_manager_reads_and_writes_through_the_host_bridge() {
    let dir = write_temp_fixture("sdk-settings", &[]);
    let agent_dir = dir.join("agent");
    let project_cwd = dir.join("project");
    std::fs::create_dir_all(&agent_dir).expect("agent dir");
    std::fs::create_dir_all(project_cwd.join(".pi")).expect("project config dir");
    std::fs::write(
        agent_dir.join("settings.json"),
        r#"{"theme":"global-theme","shellPath":"/bin/bash","shellCommandPrefix":"PREFIX",
            "defaultTools":["read","bash"],"unknownGlobalKey":{"keep":true}}"#,
    )
    .expect("write global settings");
    std::fs::write(
        project_cwd.join(".pi/settings.json"),
        r#"{"theme":"project-theme","defaultTools":["+edit"],"queueMode":"all"}"#,
    )
    .expect("write project settings");

    let fixture = format!(
        r##"
import {{ SettingsManager }} from "@earendil-works/pi-coding-agent";

export default (pi: any) => {{
  pi.on("session_start", async () => {{
    const agentDir = {agent_dir};
    const cwd = {cwd};
    const out: any = {{}};
    const trusted = SettingsManager.create(cwd, agentDir, {{ projectTrusted: true }});
    out.theme = trusted.getTheme();
    out.shellPath = trusted.getShellPath();
    out.shellCommandPrefix = trusted.getShellCommandPrefix();
    out.steeringMode = trusted.getSteeringMode();
    out.defaultTools = trusted.getDefaultTools();
    out.trusted = trusted.isProjectTrusted();
    const untrusted = SettingsManager.create(cwd, agentDir, {{ projectTrusted: false }});
    out.untrustedTheme = untrusted.getTheme();
    out.untrustedProject = JSON.stringify(untrusted.getProjectSettings());
    // Write path: global scope, atomic temp+rename through the fs bridge.
    trusted.setTheme("written-theme");
    await trusted.flush();
    out.errors = trusted.drainErrors().map((error: any) => error.scope + ":" + error.error.message);
    out.written = JSON.stringify(trusted.getGlobalSettings().unknownGlobalKey);
    out.memoryTheme = SettingsManager.inMemory({{ theme: "mem" }}).getTheme();
    globalThis.__pirSdkRuntime = out;
  }});
}};
"##,
        agent_dir = serde_json::to_string(&agent_dir.display().to_string()).unwrap(),
        cwd = serde_json::to_string(&project_cwd.display().to_string()).unwrap(),
    );

    let handle = start_handle(&dir, &fixture).await;
    let mut payload = json!({ "reason": "startup" });
    handle
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    let v = read_probe(&handle).await;

    assert_eq!(v["theme"], "project-theme", "project scope wins: {v}");
    assert_eq!(v["shellPath"], "/bin/bash", "{v}");
    assert_eq!(v["shellCommandPrefix"], "PREFIX", "{v}");
    assert_eq!(v["steeringMode"], "all", "queueMode migrates: {v}");
    assert_eq!(
        v["defaultTools"],
        json!(["read", "bash", "edit"]),
        "the `+edit` modifier appends to the inherited list: {v}"
    );
    assert_eq!(v["trusted"], true, "{v}");
    assert_eq!(
        v["untrustedTheme"], "global-theme",
        "an untrusted project scope is not read: {v}"
    );
    assert_eq!(v["untrustedProject"], "{}", "{v}");
    assert_eq!(v["errors"], json!([]), "no write errors: {v}");
    assert_eq!(
        v["written"],
        json!("{\"keep\":true}"),
        "unrelated global keys survive the write: {v}"
    );
    assert_eq!(v["memoryTheme"], "mem", "inMemory scope: {v}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// `ModelRuntime` is real: the catalog/credential faces answer from the
/// host-pushed registry snapshot, provider registration rides the host ops,
/// and the completion faces run the host completion capability (the same seam
/// `ctx.modelRegistry.complete` uses) with `completeSimple` = collect(stream).
#[tokio::test(flavor = "current_thread")]
async fn model_runtime_reads_host_catalog_and_completes_through_host() {
    let dir = write_temp_fixture("sdk-model-runtime", &[]);
    let fixture = r##"
import { ModelRuntime, resolveCliModel } from "@earendil-works/pi-coding-agent";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const runtime: any = await ModelRuntime.create();
    out.models = runtime.getModels().map((model: any) => model.provider + "/" + model.id);
    out.providerIds = Array.from(runtime.providerIds());
    out.available = (await runtime.getAvailable()).map((model: any) => model.provider + "/" + model.id);
    out.configuredAuth = [
      runtime.hasConfiguredAuth("anthropic"),
      runtime.hasConfiguredAuth("openai"),
    ];
    out.resolved = JSON.stringify(resolveCliModel({ cliModel: "sonnet", modelRuntime: runtime }));
    out.resolvedMissing = JSON.stringify(resolveCliModel({ cliModel: "nope", modelRuntime: runtime }));
    // Registration rides `registerProviderJson`/`registerVirtualModelJson`.
    runtime.registerProvider("acme", { baseUrl: "http://localhost:9/v1" });
    runtime.registerVirtualModel({ provider: "acme", id: "virtual-1" });
    out.registeredIds = runtime.getRegisteredProviderIds();
    // Completion faces: the host completion capability answers with the real
    // message; the stream carries it as a single delta (documented).
    const stream = runtime.streamSimple(runtime.getModels()[0], { messages: [] }, {});
    const events: string[] = [];
    for await (const event of stream) {
      events.push(event.type + (event.type === "text_delta" ? ":" + event.delta : ""));
    }
    out.events = events;
    out.streamResult = (await stream.result()).content[0].text;
    out.completed = (await runtime.completeSimple(runtime.getModels()[0], { messages: [] }, {})).content[0].text;
    // A bare instance stays unbound and fails loudly on the completion faces.
    const bare: any = new ModelRuntime();
    out.bareModels = bare.getModels().length;
    try {
      bare.stream({});
      out.bareStream = null;
    } catch (error: any) {
      out.bareStream = String(error.message);
    }
    globalThis.__pirSdkRuntime = out;
  });
};
"##;

    let handle = start_handle(&dir, fixture).await;
    // The host pushes the provider registry + credential list before a prompt
    // (`AgentSession` build), and a completion closure bound to the session
    // model (`AgentSession::complete_fn_for`).
    let model = |id: &str, provider: &str, api: &str| pir::ai::types::Model {
        id: id.to_string(),
        name: id.to_string(),
        api: api.to_string(),
        provider: provider.to_string(),
        base_url: "http://localhost:9/v1".to_string(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        cost: pir::ai::types::ModelCost {
            rates: pir::ai::types::ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 1000,
        max_tokens: 100,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    };
    handle.update_model_registry(
        json!([
            serde_json::to_value(model("sonnet-4", "anthropic", "anthropic-messages")).unwrap(),
            serde_json::to_value(model("gpt-5", "openai", "openai-responses")).unwrap(),
        ]),
        json!([{ "id": "anthropic", "name": "Anthropic" }, { "id": "openai", "name": "OpenAI" }]),
    );
    handle.update_authenticated_providers(vec!["anthropic".to_string()]);
    let complete: pir::compaction::CompleteFn = Box::new(|_context| {
        Box::pin(async move {
            Ok(pir::ai::types::AssistantMessage {
                content: vec![pir::ai::types::ContentBlock::Text(
                    pir::ai::types::TextContent {
                        text: "hello from host".to_string(),
                        text_signature: None,
                    },
                )],
                stop_reason: pir::ai::types::StopReason::Stop,
                ..Default::default()
            })
        })
    });
    handle.set_complete_fn("anthropic", "sonnet-4", std::sync::Arc::new(complete));
    let mut payload = json!({ "reason": "startup" });
    let outcome = handle
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "probe handler errors: {:?}",
        outcome.errors
    );

    let v = read_probe(&handle).await;
    assert_eq!(
        v["models"],
        json!(["anthropic/sonnet-4", "openai/gpt-5"]),
        "the runtime reports the host catalog: {v}"
    );
    assert_eq!(v["providerIds"], json!(["anthropic", "openai"]), "{v}");
    assert_eq!(
        v["available"],
        json!(["anthropic/sonnet-4"]),
        "availability follows the host credential list: {v}"
    );
    assert_eq!(v["configuredAuth"], json!([true, false]), "{v}");
    let resolved: Value =
        serde_json::from_str(v["resolved"].as_str().unwrap_or_default()).expect("resolved JSON");
    assert_eq!(resolved["model"]["provider"], "anthropic", "{v}");
    assert_eq!(resolved["model"]["id"], "sonnet-4", "{v}");
    let missing: Value = serde_json::from_str(v["resolvedMissing"].as_str().unwrap_or_default())
        .expect("missing JSON");
    assert!(
        missing["error"]
            .as_str()
            .unwrap_or_default()
            .contains("No models match pattern"),
        "{v}"
    );
    assert_eq!(
        v["registeredIds"],
        json!(["acme"]),
        "registerVirtualModel does not add a provider id (upstream: virtual models compose onto it): {v}"
    );
    assert_eq!(
        v["events"],
        json!([
            "start",
            "text_start",
            "text_delta:hello from host",
            "text_end",
            "done"
        ]),
        "the host completion surfaces as a one-shot stream: {v}"
    );
    assert_eq!(v["streamResult"], "hello from host", "{v}");
    assert_eq!(v["completed"], "hello from host", "{v}");
    assert_eq!(
        v["bareModels"], 0,
        "an unbound runtime reports no catalog: {v}"
    );
    assert!(
        v["bareStream"]
            .as_str()
            .unwrap_or_default()
            .contains("ModelRuntime.stream is not available"),
        "an unbound runtime keeps the loud error: {v}"
    );

    // The registration reached the host's provider table.
    let provider_defs = handle.provider_defs();
    assert!(
        provider_defs
            .iter()
            .any(|def| def.get("id").and_then(Value::as_str) == Some("acme")),
        "registerProvider must reach the host: {provider_defs:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `DefaultResourceLoader` is real for the child-session surface: it resolves
/// the requested extension paths to on-disk entry files (which
/// `createAgentSession` forwards to the child process), reports inline
/// factories as load errors instead of pretending success, and carries the
/// `loaded` field pi-subagents' cache-reset probe reads.
#[tokio::test(flavor = "current_thread")]
async fn default_resource_loader_resolves_child_extensions() {
    let dir = write_temp_fixture("sdk-resource-loader", &[]);
    let child_extension = dir.join("child-extension");
    std::fs::create_dir_all(&child_extension).expect("child extension dir");
    std::fs::write(child_extension.join("index.js"), "export default () => {};")
        .expect("child extension");

    let fixture = format!(
        r##"
import {{ DefaultResourceLoader }} from "@earendil-works/pi-coding-agent";

export default (pi: any) => {{
  pi.on("session_start", async () => {{
    const out: any = {{}};
    const loader: any = new DefaultResourceLoader({{
      cwd: {cwd},
      agentDir: {agent_dir},
      noExtensions: false,
      noSkills: true,
      noContextFiles: true,
      additionalExtensionPaths: [{child_extension}],
      extensionFactories: [{{ name: "child-hooks", factory: () => {{}} }}],
    }});
    out.hasLoadedField = "loaded" in loader;
    await loader.reload();
    const result = loader.getExtensions();
    out.paths = result.extensions.map((extension: any) => extension.path);
    out.errors = result.errors.map((entry: any) => entry.path);
    out.hasRuntimeQueue = Array.isArray(result.runtime.pendingProviderRegistrations);
    out.loaded = loader.loaded;
    out.skills = loader.getSkills().skills.length;
    out.missingPathErrors = (() => {{
      const other: any = new DefaultResourceLoader({{
        cwd: {cwd},
        agentDir: {agent_dir},
        additionalExtensionPaths: ["/definitely/missing/extension.ts"],
      }});
      return other.reload().then(() => other.getExtensions().errors.length);
    }})();
    out.missingPathErrorCount = await out.missingPathErrors;
    delete out.missingPathErrors;
    globalThis.__pirSdkRuntime = out;
  }});
}};
"##,
        cwd = serde_json::to_string(&dir.display().to_string()).unwrap(),
        agent_dir = serde_json::to_string(&dir.display().to_string()).unwrap(),
        child_extension = serde_json::to_string(&child_extension.display().to_string()).unwrap(),
    );

    let handle = start_handle(&dir, &fixture).await;
    let mut payload = json!({ "reason": "startup" });
    handle
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    let v = read_probe(&handle).await;

    assert_eq!(v["hasLoadedField"], true, "{v}");
    assert_eq!(v["loaded"], true, "{v}");
    assert_eq!(v["skills"], 0, "noSkills suppresses the skill scan: {v}");
    assert_eq!(
        v["paths"],
        json!([child_extension
            .join("index.js")
            .display()
            .to_string()
            .replace('\\', "/")]),
        "the directory resolves to its entry file: {v}"
    );
    assert_eq!(
        v["errors"],
        json!(["<inline:child-hooks>"]),
        "inline factories are reported, not silently dropped: {v}"
    );
    assert_eq!(v["hasRuntimeQueue"], true, "{v}");
    assert_eq!(
        v["missingPathErrorCount"], 1,
        "an unresolvable extension path is an error (that is what makes requiredExtensions fail closed): {v}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
