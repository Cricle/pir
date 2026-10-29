//! Phase 4f: `@earendil-works/pi-ai` (+ `/oauth`, `/providers/all`) and
//! `@earendil-works/pi-agent-core` value-surface mirror.
//!
//! Repo hygiene forbids tracked `.ts` fixtures, so the probe extension is
//! seeded from a source string at runtime (mirrors tests/pi_shims.rs). One
//! seeded extension statically imports every worklist name (a missing export
//! fails module instantiation), re-checks the whole surface through dynamic
//! namespace imports, and records behaviour probes on `globalThis.__aiShims`.

use std::path::{Path, PathBuf};

use pir::ai::providers::catalog;
use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// See tests/extensions_runtime.rs: fixtures must be written at runtime.
fn write_temp_fixture(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pir-ai-shims-{}-{test_name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
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
        .unwrap_or_else(|error| panic!("runtime construction: {error:?}"));
    rt.allow_root(dir.to_path_buf());
    rt
}

/// The pi-ai worklist from %TEMP%/pir-compat/pi-gaps-ai.txt (65 missing names
/// at wave start; StringEnum/uuidv7/Type were already present, embedded here
/// too so the full upstream index.ts surface is asserted).
const AI_WORKLIST: &[&str] = &[
    "AssistantMessageEventStream",
    "AssistantMessageFrameEncoder",
    "DEFAULT_MAX_AGENT_RETRY_DELAY_MS",
    "EventStream",
    "InMemoryCredentialStore",
    "InMemoryModelsStore",
    "ModelsError",
    "StringEnum",
    "Type",
    "appendAssistantMessageDiagnostic",
    "calculateCost",
    "clampThinkingLevel",
    "cleanupSessionResources",
    "collapseSystemMessages",
    "contentText",
    "createAssistantMessageDiagnostic",
    "createAssistantMessageEventStream",
    "createFauxCore",
    "createInitialSystemMessage",
    "createModels",
    "createProvider",
    "declarationsEqual",
    "defaultProviderAuthContext",
    "envApiKeyAuth",
    "extractDiagnosticError",
    "fauxAssistantMessage",
    "fauxProvider",
    "fauxText",
    "fauxThinking",
    "fauxToolCall",
    "formatThrownValue",
    "getCurrentSystemMessage",
    "getCurrentSystemPrompt",
    "getCurrentTools",
    "getDeclaredTools",
    "getInitialSystemMessage",
    "getModelType",
    "getOverflowPatterns",
    "getSupportedThinkingLevels",
    "getSystemMessageText",
    "getToolStateChanges",
    "hasApi",
    "hasNonAdditiveToolChanges",
    "hasToolRedefinitions",
    "isContextOverflow",
    "isModelType",
    "isRecoverableLength",
    "isRetryableAssistantError",
    "lazyApi",
    "lazyOAuth",
    "lazyStream",
    "modelsAreEqual",
    "normalizeContext",
    "parseJsonWithRepair",
    "parseStreamingJson",
    "reduceAssistantMessageFrames",
    "registerSessionResourceCleanup",
    "renderSystemMessageUpdate",
    "repairJson",
    "resolveTranscript",
    "resolveTranscriptTools",
    "retryAssistantCall",
    "retryDelayMs",
    "toToolDeclaration",
    "uuidv7",
    "validateToolArguments",
    "validateToolCall",
    "withoutInitialSystemMessage",
];

/// Agent-core worklist from pi-gaps-agent-core.txt (7 missing; `Agent` was
/// already present and is embedded as the eighth name).
const AGENT_CORE_WORKLIST: &[&str] = &[
    "Agent",
    "agentLoop",
    "agentLoopContinue",
    "runAgentLoop",
    "runAgentLoopContinue",
    "runToolCall",
    "setDefaultStreamFn",
    "streamProxy",
];

/// `@earendil-works/pi-ai/providers/all` value exports (14).
const PROVIDERS_ALL_WORKLIST: &[&str] = &[
    "builtinModels",
    "builtinProviders",
    "getAllBuiltinModels",
    "getBuiltinClassifierModel",
    "getBuiltinClassifierModels",
    "getBuiltinImageModel",
    "getBuiltinImageModels",
    "getBuiltinModel",
    "getBuiltinModelDataGeneratedAt",
    "getBuiltinModelDataManifest",
    "getBuiltinModelDataVersion",
    "getBuiltinModels",
    "getBuiltinProviders",
    "radiusProvider",
];

/// The virtual module backing `@earendil-works/pi-ai/providers/all`.
const PROVIDERS_ALL_SHIM: &str = "pi_ai_providers_all_shim.js.txt";

/// Manifest pinned in the providers/all shim; the runtime
/// `getBuiltinModelDataManifest()` returns `JSON.parse` of this literal.
fn pinned_providers_all_manifest(shim: &str) -> Value {
    const MARKER: &str = "const PIR_BUILTIN_MODEL_DATA_MANIFEST_JSON = `";
    let start = shim
        .find(MARKER)
        .expect("providers/all shim manifest constant")
        + MARKER.len();
    let rest = &shim[start..];
    let end = rest
        .find('`')
        .expect("providers/all shim manifest literal end");
    serde_json::from_str(&rest[..end]).expect("providers/all shim manifest JSON")
}

/// Version pinned in the providers/all shim; the runtime
/// `getBuiltinModelDataVersion()` returns this literal.
fn pinned_providers_all_version(shim: &str) -> &str {
    const MARKER: &str = "const PIR_BUILTIN_MODEL_DATA_VERSION = \"";
    let start = shim
        .find(MARKER)
        .expect("providers/all shim version constant")
        + MARKER.len();
    let rest = &shim[start..];
    let end = rest
        .find('"')
        .expect("providers/all shim version literal end");
    &rest[..end]
}

/// One seeded extension linking the whole mirrored surface; every probe is
/// recorded on `globalThis.__aiShims` during `session_start`.
const AI_SHIMS_TS: &str = r##"
import {
  AssistantMessageEventStream, AssistantMessageFrameEncoder, DEFAULT_MAX_AGENT_RETRY_DELAY_MS,
  EventStream, InMemoryCredentialStore, InMemoryModelsStore, ModelsError, StringEnum, Type,
  appendAssistantMessageDiagnostic, calculateCost, clampThinkingLevel, cleanupSessionResources,
  collapseSystemMessages, contentText, createAssistantMessageDiagnostic, createAssistantMessageEventStream,
  createFauxCore, createInitialSystemMessage, createModels, createProvider, declarationsEqual,
  defaultProviderAuthContext, envApiKeyAuth, extractDiagnosticError, fauxAssistantMessage, fauxProvider,
  fauxText, fauxThinking, fauxToolCall, formatThrownValue, getCurrentSystemMessage, getCurrentSystemPrompt,
  getCurrentTools, getDeclaredTools, getInitialSystemMessage, getModelType, getOverflowPatterns,
  getSupportedThinkingLevels, getSystemMessageText, getToolStateChanges, hasApi, hasNonAdditiveToolChanges,
  hasToolRedefinitions, isContextOverflow, isModelType, isRecoverableLength, isRetryableAssistantError,
  lazyApi, lazyOAuth, lazyStream, modelsAreEqual, normalizeContext, parseJsonWithRepair, parseStreamingJson,
  reduceAssistantMessageFrames, registerSessionResourceCleanup, renderSystemMessageUpdate, repairJson,
  resolveTranscript, resolveTranscriptTools, retryAssistantCall, retryDelayMs, toToolDeclaration, uuidv7,
  validateToolArguments, validateToolCall, withoutInitialSystemMessage,
} from "@earendil-works/pi-ai";
import {
  getAllBuiltinModels, builtinModels, builtinProviders, getBuiltinClassifierModel,
  getBuiltinClassifierModels, getBuiltinImageModel, getBuiltinImageModels, getBuiltinModel,
  getBuiltinModelDataGeneratedAt, getBuiltinModelDataManifest, getBuiltinModelDataVersion,
  getBuiltinModels, getBuiltinProviders, radiusProvider,
} from "@earendil-works/pi-ai/providers/all";
import {
  Agent, agentLoop, agentLoopContinue, runAgentLoop, runAgentLoopContinue, runToolCall,
  setDefaultStreamFn, streamProxy,
} from "@earendil-works/pi-agent-core";
import oauthDefault from "@earendil-works/pi-ai/oauth";

const PI_AI_NAMES = [
  "AssistantMessageEventStream", "AssistantMessageFrameEncoder", "DEFAULT_MAX_AGENT_RETRY_DELAY_MS",
  "EventStream", "InMemoryCredentialStore", "InMemoryModelsStore", "ModelsError", "StringEnum", "Type",
  "appendAssistantMessageDiagnostic", "calculateCost", "clampThinkingLevel", "cleanupSessionResources",
  "collapseSystemMessages", "contentText", "createAssistantMessageDiagnostic",
  "createAssistantMessageEventStream", "createFauxCore", "createInitialSystemMessage", "createModels",
  "createProvider", "declarationsEqual", "defaultProviderAuthContext", "envApiKeyAuth",
  "extractDiagnosticError", "fauxAssistantMessage", "fauxProvider", "fauxText", "fauxThinking",
  "fauxToolCall", "formatThrownValue", "getCurrentSystemMessage", "getCurrentSystemPrompt",
  "getCurrentTools", "getDeclaredTools", "getInitialSystemMessage", "getModelType", "getOverflowPatterns",
  "getSupportedThinkingLevels", "getSystemMessageText", "getToolStateChanges", "hasApi",
  "hasNonAdditiveToolChanges", "hasToolRedefinitions", "isContextOverflow", "isModelType",
  "isRecoverableLength", "isRetryableAssistantError", "lazyApi", "lazyOAuth", "lazyStream",
  "modelsAreEqual", "normalizeContext", "parseJsonWithRepair", "parseStreamingJson",
  "reduceAssistantMessageFrames", "registerSessionResourceCleanup", "renderSystemMessageUpdate",
  "repairJson", "resolveTranscript", "resolveTranscriptTools", "retryAssistantCall", "retryDelayMs",
  "toToolDeclaration", "uuidv7", "validateToolArguments", "validateToolCall",
  "withoutInitialSystemMessage",
];
const PROVIDERS_ALL_NAMES = [
  "builtinModels", "builtinProviders", "getAllBuiltinModels", "getBuiltinClassifierModel",
  "getBuiltinClassifierModels", "getBuiltinImageModel", "getBuiltinImageModels", "getBuiltinModel",
  "getBuiltinModelDataGeneratedAt", "getBuiltinModelDataManifest", "getBuiltinModelDataVersion",
  "getBuiltinModels", "getBuiltinProviders", "radiusProvider",
];
const AGENT_CORE_NAMES = [
  "Agent", "agentLoop", "agentLoopContinue", "runAgentLoop", "runAgentLoopContinue", "runToolCall",
  "setDefaultStreamFn", "streamProxy",
];

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const err = (fn: any): any => { try { fn(); return null; } catch (e: any) { return String(e && e.message ? e.message : e); } };
    const aerr = async (fn: any): Promise<any> => { try { await fn(); return null; } catch (e: any) { return String(e && e.message ? e.message : e); } };

    // ---- surface: per-package existence (worklists embedded above) ----
    const aiNs: any = await import("@earendil-works/pi-ai");
    const allNs: any = await import("@earendil-works/pi-ai/providers/all");
    const agentNs: any = await import("@earendil-works/pi-agent-core");
    const oauthNs: any = await import("@earendil-works/pi-ai/oauth");
    const typesOf = (ns: any, names: string[]) => {
      const record: any = {};
      for (const name of names) record[name] = typeof ns[name];
      return record;
    };
    out.surface = {
      aiNames: PI_AI_NAMES,
      providersNames: PROVIDERS_ALL_NAMES,
      agentNames: AGENT_CORE_NAMES,
      aiTypes: typesOf(aiNs, PI_AI_NAMES),
      providersTypes: typesOf(allNs, PROVIDERS_ALL_NAMES),
      agentTypes: typesOf(agentNs, AGENT_CORE_NAMES),
      aiMissing: PI_AI_NAMES.filter((n: string) => typeof aiNs[n] === "undefined"),
      providersMissing: PROVIDERS_ALL_NAMES.filter((n: string) => typeof allNs[n] === "undefined"),
      agentMissing: AGENT_CORE_NAMES.filter((n: string) => typeof agentNs[n] === "undefined"),
      oauthKeys: Object.keys(oauthNs).sort(),
      oauthDefaultObject: oauthDefault !== null && typeof oauthDefault === "object",
      aiDefaultObject: aiNs.default !== null && typeof aiNs.default === "object",
      agentDefaultObject: agentNs.default !== null && typeof agentNs.default === "object",
      typeofAgent: typeof Agent,
      typeofType: typeof Type,
    };

    // ---- text utilities (utils/text.ts) ----
    out.text = {
      plain: contentText("abc"),
      joined: contentText(
        [{ type: "text", text: "a" }, { type: "image", data: "x" }, { type: "text", text: "b" }],
        "|",
      ),
      sysmsg: getSystemMessageText({ content: [{ type: "text", text: "C" }], sections: { x: "X", y: null } }),
      update: renderSystemMessageUpdate({ content: "C", sections: { x: "X2", y: null } }),
    };

    // ---- transcript replay (utils/transcript.ts) ----
    const toolA = { name: "read", description: "Read", parameters: { type: "object", properties: {} } };
    const toolB = { name: "write", description: "Write", parameters: { type: "object", properties: {} } };
    const sys1 = { role: "system", content: "S1", sections: { a: "A1" }, timestamp: 1 };
    const sys2 = { role: "system", content: "S2", sections: { a: null, b: "B" }, toolsAdded: [toolA], timestamp: 2 };
    const sys3 = { role: "system", content: "", toolsRemoved: [toolA], toolsAdded: [toolB], timestamp: 3 };
    const userMsg = { role: "user", content: "hi", timestamp: 4 };
    const replay = [sys1, sys2, sys3, userMsg];
    const currentMessage = getCurrentSystemMessage(replay);
    out.transcript = {
      initial: createInitialSystemMessage("P", [toolA]),
      normalizedFirstRole: normalizeContext({ systemPrompt: "P", tools: [toolA], messages: [userMsg] }).messages[0].role,
      noInitial: getInitialSystemMessage([userMsg]) === undefined,
      currentContent: currentMessage.content,
      currentSectionsKeys: Object.keys(currentMessage.sections ?? {}),
      currentSectionB: (currentMessage.sections ?? {}).b,
      currentToolNames: (currentMessage.toolsAdded ?? []).map((t: any) => t.name),
      currentTimestamp: currentMessage.timestamp,
      currentPrompt: getCurrentSystemPrompt(replay),
      emptyPrompt: getCurrentSystemPrompt([userMsg]),
      withoutInitialLength: withoutInitialSystemMessage([sys1, userMsg]).length,
      collapsedRoles: collapseSystemMessages({ messages: [sys1, userMsg, sys2] }).messages.map((m: any) => m.role),
      resolvedLength: resolveTranscript({ messages: [sys1, userMsg, sys2] }, true).messages.length,
      declaredNames: getDeclaredTools(replay).map((t: any) => t.name),
      currentTools: getCurrentTools(replay).map((t: any) => t.name),
      hasRedefinitions: hasToolRedefinitions(replay),
      redefinitionsHeard: hasToolRedefinitions([sys2, { role: "system", content: "", toolsAdded: [{ ...toolA, description: "changed" }], timestamp: 5 }]),
      hasNonAdditive: hasNonAdditiveToolChanges(replay),
      resolveToolsAnchorsAdditions: resolveTranscriptTools(replay, true).anchorsAdditions,
      resolveToolsRequestNames: resolveTranscriptTools(replay, true).requestTools.map((t: any) => t.name),
      strippedDeclaration: JSON.stringify(toToolDeclaration({ name: "t", description: "d", parameters: { type: "object" }, execute: () => {} })),
      declarationsEqual: declarationsEqual(toolA, { ...toolA }),
      declarationsEqualChanged: declarationsEqual(toolA, { ...toolA, description: "x" }),
      stateChanges: {
        added: getToolStateChanges([toolA], [toolB]).toolsAdded.map((t: any) => t.name),
        removed: getToolStateChanges([toolA], [toolB]).toolsRemoved.map((t: any) => t.name),
      },
    };

    // ---- JSON repair + streaming parse (utils/json-parse.ts) ----
    out.json = {
      repairChanged: repairJson("{\"a\": \"x\ny\"}").indexOf("\\n") >= 0,
      repairedValue: parseJsonWithRepair("{\"a\": \"x\ny\"}").a === "x\ny",
      strictValue: parseJsonWithRepair('{"a": 1}').a,
      streaming: JSON.stringify(parseStreamingJson('{"a": 1, "b": [1, 2')),
      streamingString: parseStreamingJson('{"s": "hel').s,
      streamingEmpty: JSON.stringify(parseStreamingJson("")),
    };

    // ---- retry policy + overflow classification ----
    let retryAttempts = 0;
    const retried = await retryAssistantCall(
      async () => {
        retryAttempts++;
        return retryAttempts < 2 ? { stopReason: "error", errorMessage: "429 rate limit" } : { stopReason: "stop" };
      },
      { enabled: true, maxRetries: 3, baseDelayMs: 1, maxAgentDelayMs: 2 },
      undefined,
      undefined,
    );
    let noRetryAttempts = 0;
    const notRetried = await retryAssistantCall(
      async () => {
        noRetryAttempts++;
        return { stopReason: "error", errorMessage: "This model's maximum context length is 8192 tokens" };
      },
      { enabled: true, maxRetries: 3, baseDelayMs: 1, maxAgentDelayMs: 2 },
      undefined,
      undefined,
    );
    out.retry = {
      defaultMax: DEFAULT_MAX_AGENT_RETRY_DELAY_MS,
      delay1: retryDelayMs({ baseDelayMs: 100, maxAgentDelayMs: 1000 }, 1),
      delay3: retryDelayMs({ baseDelayMs: 100, maxAgentDelayMs: 1000 }, 3),
      cappedByPolicy: retryDelayMs({ baseDelayMs: 100, maxAgentDelayMs: 1000 }, 10),
      cappedByDefault: retryDelayMs({ baseDelayMs: 100 }, 20),
      retryable429: isRetryableAssistantError({ stopReason: "error", errorMessage: "429 Too Many Requests" }),
      retryableQuota: isRetryableAssistantError({ stopReason: "error", errorMessage: "insufficient_quota" }),
      retryableStop: isRetryableAssistantError({ stopReason: "stop", errorMessage: "429" }),
      overflowText: isContextOverflow({ stopReason: "error", errorMessage: "This model's maximum context length is 8192 tokens" }),
      overflowRateLimited: isContextOverflow({ stopReason: "error", errorMessage: "Rate limit exceeded" }),
      overflowWindow: isContextOverflow({ stopReason: "stop", usage: { input: 5, cacheRead: 0, output: 0, cacheWrite: 0 } }, 4),
      overflowPatterns: getOverflowPatterns().length,
      recoverable: isRecoverableLength({ stopReason: "length", usage: { output: 10 } }, 100),
      notRecoverable: isRecoverableLength({ stopReason: "stop", usage: { output: 10 } }, 100),
      retryAttempts,
      retriedStop: retried.stopReason,
      noRetryAttempts,
      notRetriedStop: notRetried.stopReason,
    };

    // ---- model metadata + cost + thinking levels (models.ts) ----
    const costModel = {
      id: "m1", api: "x", provider: "p",
      cost: { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75, tiers: [{ inputTokensAbove: 1000, input: 6, output: 30, cacheRead: 0.6, cacheWrite: 7.5 }] },
    };
    const usageBase: any = { input: 1000, output: 500, cacheRead: 0, cacheWrite: 0, cacheWrite1h: 0, cost: {} };
    calculateCost(costModel, usageBase);
    const usageTier: any = { input: 1500, output: 0, cacheRead: 0, cacheWrite: 0, cacheWrite1h: 0, cost: {} };
    calculateCost(costModel, usageTier);
    out.modelMeta = {
      typeDefault: getModelType({ id: "m" }),
      typeImage: getModelType({ id: "m", type: "image" }),
      isChat: isModelType({ id: "m" }, "chat"),
      hasApiChat: hasApi({ id: "m", api: "openai" }, "openai"),
      hasApiImage: hasApi({ id: "m", api: "openai", type: "image" }, "openai"),
      costTotalMicro: Math.round(usageBase.cost.total * 1e6),
      costInputMicro: Math.round(usageBase.cost.input * 1e6),
      costTierInputMicro: Math.round(usageTier.cost.input * 1e6),
      levelsAll: getSupportedThinkingLevels({ reasoning: true }),
      levelsMapped: getSupportedThinkingLevels({ reasoning: true, thinkingLevelMap: { off: null, xhigh: null, max: undefined } }),
      levelsNone: getSupportedThinkingLevels({ reasoning: false }),
      clampHigh: clampThinkingLevel({ reasoning: true, thinkingLevelMap: { xhigh: null, max: undefined } }, "xhigh"),
      clampOff: clampThinkingLevel({ reasoning: false }, "high"),
      equalSame: modelsAreEqual({ id: "a", provider: "p" }, { id: "a", provider: "p" }),
      equalDifferent: modelsAreEqual({ id: "a", provider: "p" }, { id: "a", provider: "q" }),
      uuidv7Shape: /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(uuidv7()),
      stringEnumShape: JSON.stringify(StringEnum(["a", "b"], { description: "d" })) === '{"type":"string","enum":["a","b"],"description":"d"}',
      typeIsObject: typeof Type === "object",
      modelsError: (() => { const e = new ModelsError("auth", "login failed"); return e.name + ":" + e.code + ":" + e.message; })(),
      modelsErrorCause: new ModelsError("auth", "login failed", { cause: new Error("net down") }).message,
    };

    // ---- diagnostics (utils/diagnostics.ts) ----
    const diagMessage: any = { role: "assistant" };
    appendAssistantMessageDiagnostic(diagMessage, createAssistantMessageDiagnostic("retry", new Error("x"), { attempt: 1 }));
    out.diag = {
      formatError: formatThrownValue(new Error("boom")),
      formatString: formatThrownValue("plain"),
      extractedMessage: extractDiagnosticError(new Error("oops")).message,
      extractedName: extractDiagnosticError(new Error("oops")).name,
      thrownValueName: extractDiagnosticError("str").name,
      count: diagMessage.diagnostics.length,
      type: diagMessage.diagnostics[0].type,
      details: diagMessage.diagnostics[0].details,
    };

    // ---- event streams + frame encode/reduce round trip ----
    const raw = new EventStream((e: any) => e.done === true, (e: any) => e.value);
    raw.push({ value: 1, done: false });
    raw.push({ value: 2, done: true });
    const rawResult = await raw.result();
    const rawValues = [];
    for await (const ev of raw) rawValues.push(ev.value);

    const es = createAssistantMessageEventStream();
    const finalMessage: any = {
      role: "assistant", content: [{ type: "text", text: "hi" }], stopReason: "stop", timestamp: Date.now(),
      usage: { input: 1, output: 1, cacheRead: 0, cacheWrite: 0, totalTokens: 2, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
    };
    es.push({ type: "done", reason: "stop", message: finalMessage });
    const esResult = await es.result();
    out.streams = {
      subclass: es instanceof EventStream,
      assistantText: esResult.content[0].text,
      durationType: typeof esResult.durationMs,
      rawResult,
      rawValues,
    };

    const encoder = new AssistantMessageFrameEncoder();
    const labels: string[] = [];
    const pushFrame = (frame: any) => { if (frame !== undefined) labels.push(frame.type); };
    const startPartial: any = {
      role: "assistant", content: [], api: "probe", provider: "probe", model: "m1", stopReason: "pending", timestamp: 111,
      usage: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
    };
    const frames: any[] = [];
    const push = (frame: any) => { if (frame !== undefined) frames.push(frame); };
    push(encoder.encode({ type: "start", partial: startPartial }));
    push(encoder.encode({ type: "text_start", contentIndex: 0, partial: { ...startPartial, content: [{ type: "text", text: "" }] } }));
    push(encoder.encode({ type: "text_delta", contentIndex: 0, delta: "Hello", partial: { ...startPartial, content: [{ type: "text", text: "Hello" }] } }));
    push(encoder.encode({ type: "text_end", contentIndex: 0, content: "Hello", partial: { ...startPartial, content: [{ type: "text", text: "Hello" }] } }));
    const partialTool: any = { ...startPartial, content: [{ type: "text", text: "Hello" }, { type: "toolCall", id: "tc1", name: "grep", arguments: {} }] };
    push(encoder.encode({ type: "toolcall_start", contentIndex: 1, partial: partialTool }));
    push(encoder.encode({ type: "toolcall_delta", contentIndex: 1, delta: '{"q":', partial: partialTool }));
    push(encoder.encode({ type: "toolcall_delta", contentIndex: 1, delta: '"x"}', partial: partialTool }));
    push(encoder.encode({ type: "toolcall_end", contentIndex: 1, toolCall: { type: "toolCall", id: "tc1", name: "grep", arguments: { q: "x" } }, partial: partialTool }));
    const doneFrameDropped = encoder.encode({ type: "done", message: finalMessage }) === undefined;
    const terminalError = err(() => encoder.encode({ type: "text_delta", contentIndex: 0, delta: "late", partial: partialTool }));
    const reduced = reduceAssistantMessageFrames(frames);
    out.frames = {
      frameTypes: frames.map((f) => f.type),
      labelTypes: labels,
      doneFrameDropped,
      terminalError,
      reducedText: reduced.content[0].text,
      reducedStop: reduced.stopReason,
      reducedToolName: reduced.content[1].name,
      reducedToolArgs: JSON.stringify(reduced.content[1].arguments),
      noStartReduce: reduceAssistantMessageFrames([]) === undefined,
    };

    // ---- provider/models skeleton: registry reads real, execution throws ----
    const fakeStreams = {
      stream: (model: any) => createAssistantMessageEventStream(),
      streamSimple: (model: any) => createAssistantMessageEventStream(),
    };
    const probeProvider = createProvider({
      id: "probe",
      api: fakeStreams,
      models: [{
        id: "probe-1", name: "Probe 1", api: "probe", provider: "probe", reasoning: false, input: ["text"],
        cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 1000, maxTokens: 100,
      }],
    });
    const models = createModels();
    models.setProvider(probeProvider);
    const streamOk = (() => {
      const s = probeProvider.stream(probeProvider.getModels()[0], { messages: [] }, {});
      return s !== null && typeof s === "object";
    })();
    out.provider = {
      providerName: probeProvider.name,
      chatModels: probeProvider.getModels().map((m: any) => m.id),
      registry: models.getModels().map((m: any) => m.id),
      single: models.getModel("probe", "probe-1")?.id,
      byId: models.getProvider("probe")?.id,
      streamOk,
      emptyProviderError: err(() => createProvider({ id: "empty" })),
      streamError: err(() => models.stream()),
      completeError: err(() => models.complete()),
      streamSimpleError: err(() => models.streamSimple()),
      refreshError: err(() => models.refresh()),
      loginError: err(() => models.login()),
      availableError: err(() => models.getAvailable()),
      checkAuthError: err(() => models.checkAuth()),
    };

    // ---- faux provider (scripted double, never touches the network) ----
    const faux = fauxProvider({ provider: "faux-probe", api: "faux-probe" });
    faux.setResponses([fauxAssistantMessage("hello")]);
    const fauxModel = faux.getModel();
    const fauxStream = faux.provider.stream(fauxModel, { messages: [{ role: "user", content: "hi" }] }, {});
    const fauxEventTypes = [];
    for await (const ev of fauxStream) fauxEventTypes.push(ev.type);
    const fauxFinal = await fauxStream.result();
    out.faux = {
      modelId: fauxModel.id,
      finalText: fauxFinal.content[0].text,
      finalStop: fauxFinal.stopReason,
      eventTypes: fauxEventTypes,
      callCount: faux.state.callCount,
      pendingCount: faux.getPendingResponseCount(),
      coreModels: createFauxCore({ api: "c-api" }).models.length,
      textShape: JSON.stringify(fauxText("t")),
      thinkingShape: JSON.stringify(fauxThinking("th")),
      callName: fauxToolCall("grep", { q: "x" }).name,
      callArgs: JSON.stringify(fauxToolCall("grep", { q: "x" }).arguments),
      assistDefaultStop: fauxAssistantMessage("m").stopReason,
    };

    // ---- tool validation (utils/validation.ts, host schema validator) ----
    const tools = [{
      name: "t", description: "T",
      parameters: { type: "object", properties: { a: { type: "string" } }, required: ["a"], additionalProperties: false },
    }];
    out.validation = {
      okArgs: JSON.stringify(validateToolCall(tools, { name: "t", arguments: { a: "x" } })),
      directArgs: JSON.stringify(validateToolArguments(tools[0], { name: "t", arguments: { a: "y" } })),
      missingArgsError: err(() => validateToolCall(tools, { name: "t", arguments: {} })),
      unknownToolError: err(() => validateToolCall(tools, { name: "nope", arguments: {} })),
    };

    // ---- auth helpers, credential/models stores, session resources ----
    const authCtx = defaultProviderAuthContext();
    const apiKeyAuth = envApiKeyAuth("Probe", ["FAKE_KEY_ENV"]);
    const resolved = await apiKeyAuth.resolve({
      ctx: { env: async (name: string) => (name === "FAKE_KEY_ENV" ? "sk-test" : undefined) },
      credential: undefined,
      signal: undefined,
    });
    const unresolved = await apiKeyAuth.resolve({
      ctx: { env: async () => undefined },
      credential: undefined,
      signal: undefined,
    });
    const store = new InMemoryCredentialStore();
    await store.modify("p1", async () => ({ type: "api_key", key: "k1" }));
    const credential = await store.read("p1");
    const list = await store.list();
    await store.delete("p1");
    const afterDelete = await store.read("p1");
    const mstore = new InMemoryModelsStore();
    await mstore.write("p1", { models: [1], checkedAt: 5 });
    const mEntry = await mstore.read("p1");
    const cleaned: any[] = [];
    const unregister = registerSessionResourceCleanup((sid: any) => cleaned.push(sid));
    cleanupSessionResources("s1");
    unregister();
    cleanupSessionResources("s2");
    out.aux = {
      fileExists: await authCtx.fileExists("C:/definitely/missing"),
      missingEnvUndefined: (await authCtx.env("PIR_DEFINITELY_MISSING_ENV")) === undefined,
      apiKey: resolved?.auth?.apiKey,
      apiKeySource: resolved?.source,
      unresolvedUndefined: unresolved === undefined,
      credentialKey: credential.key,
      listType: list[0].type,
      listProviderId: list[0].providerId,
      deleted: afterDelete === undefined,
      modelsStoreLen: mEntry.models.length,
      modelsStoreCheckedAt: mEntry.checkedAt,
      sessionCleanup: cleaned,
      lazyText: await (async () => {
        const lazy = lazyApi(
          async () => ({
            stream: (model: any) => {
              const s = createAssistantMessageEventStream();
              const msg = fauxAssistantMessage("lazy");
              s.push({ type: "done", reason: "stop", message: msg });
              s.end(msg);
              return s;
            },
            streamSimple: () => createAssistantMessageEventStream(),
          }),
          {},
        );
        const msg = await lazy.stream({ api: "x", provider: "p", id: "m" }, { messages: [] }, {}).result();
        return msg.content[0].text;
      })(),
      lazyStreamError: (await lazyStream({ api: "x", provider: "p", id: "m" }, async () => { throw new Error("setup boom"); }).result()).errorMessage,
      lazyOAuthName: (() => {
        const lo = lazyOAuth({
          name: "fake-oauth", isSubscription: true,
          load: async () => ({
            login: async () => ({ type: "oauth", access: "a" }),
            refresh: async () => ({ type: "oauth", access: "refreshed" }),
            toAuth: async () => ({ auth: {} }),
          }),
        });
        return lo.name + ":" + lo.isSubscription;
      })(),
    };
    const lo = lazyOAuth({
      name: "fake-oauth", isSubscription: true,
      load: async () => ({
        login: async () => ({ type: "oauth", access: "a" }),
        refresh: async () => ({ type: "oauth", access: "refreshed" }),
        toAuth: async () => ({ auth: {} }),
      }),
    });
    out.aux.lazyOAuthAccess = (await lo.login({}, undefined)).access;

    // ---- agent-core: Agent stub + host-side loop faces ----
    const agent = new Agent({ initialState: { messages: [] } });
    agent.steer("s1");
    agent.followUp("f1");
    out.agent = {
      constructed: typeof Agent === "function",
      hasQueued: agent.hasQueuedMessages(),
      peek: agent.peekQueuedMessages(),
      stateLen: agent.state.messages.length,
    };
    agent.clearAllQueues();
    out.agent.clearedHasQueued = agent.hasQueuedMessages();
    agent.abort();
    await agent.waitForIdle();
    out.agent.idle = true;
    out.agent.promptError = await aerr(() => agent.prompt("hi"));
    out.agent.continueError = await aerr(() => agent.continue());
    out.agent.runError = await aerr(() => agent.run());
    out.agent.loopError = err(() => agentLoop());
    out.agent.loopContinueError = err(() => agentLoopContinue());
    out.agent.runLoopError = err(() => runAgentLoop());
    out.agent.runLoopContinueError = err(() => runAgentLoopContinue());
    out.agent.toolCallError = err(() => runToolCall());
    out.agent.proxyError = err(() => streamProxy());
    setDefaultStreamFn(() => {});
    out.agent.setDefaultStreamFnOk = true;

    // ---- providers/all: catalog metadata is real, model objects are not ----
    out.providersAll = {
      providers: getBuiltinProviders(),
      generatedAt: getBuiltinModelDataGeneratedAt(),
      manifest: getBuiltinModelDataManifest(),
      version: getBuiltinModelDataVersion(),
      modelError: err(() => getBuiltinModel("openai", "gpt-4o")),
      imageModelError: err(() => getBuiltinImageModel("openai", "gpt-image-1")),
      classifierModelError: err(() => getBuiltinClassifierModel("openai", "x")),
      modelsError: err(() => getBuiltinModels("openai")),
      imageModelsError: err(() => getBuiltinImageModels("openai")),
      classifierModelsError: err(() => getBuiltinClassifierModels("openai")),
      allModelsError: err(() => getAllBuiltinModels("openai")),
      builtinProvidersError: err(() => builtinProviders()),
      builtinModelsError: err(() => builtinModels()),
      radiusType: typeof radiusProvider,
      radiusUseError: err(() => { (radiusProvider as any).anything; }),
    };

    globalThis.__aiShims = out;
  });
};
"##;

#[tokio::test(flavor = "current_thread")]
async fn pi_ai_and_agent_core_surface_and_behaviour() {
    let dir = write_temp_fixture("surface");
    let rt = new_runtime_at(&dir).await;
    rt.seed_module("pir-ext:ai-shims.ts", AI_SHIMS_TS)
        .expect("seed ai-shims extension");
    let loaded = rt.seeded_load("pir-ext:ai-shims.ts").await;
    assert_eq!(loaded.expect("seeded load"), 1);

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors while exercising ai shims: {:?}",
        outcome.errors
    );

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__aiShims)")
        .await
        .expect("read ai shim results");

    // ------------------------------------------------------------------
    // 1) Surface: every worklist name exists in its package.
    // ------------------------------------------------------------------
    let surface = &v["surface"];
    assert_eq!(surface["aiMissing"], json!([]), "pi-ai missing names: {v}");
    assert_eq!(
        surface["providersMissing"],
        json!([]),
        "providers/all missing names: {v}"
    );
    assert_eq!(
        surface["agentMissing"],
        json!([]),
        "pi-agent-core missing names: {v}"
    );
    assert_eq!(
        surface["aiNames"].as_array().map(Vec::len),
        Some(AI_WORKLIST.len()),
        "fixture must embed the full pi-ai worklist: {v}"
    );
    assert_eq!(
        surface["agentNames"].as_array().map(Vec::len),
        Some(AGENT_CORE_WORKLIST.len()),
        "{v}"
    );
    assert_eq!(
        surface["providersNames"].as_array().map(Vec::len),
        Some(PROVIDERS_ALL_WORKLIST.len()),
        "{v}"
    );
    for name in AI_WORKLIST {
        let kind = surface["aiTypes"][name].as_str().unwrap_or("missing");
        assert!(
            kind != "undefined" && kind != "missing",
            "pi-ai export {name} must exist (typeof=undefined): {v}"
        );
    }
    for name in AGENT_CORE_WORKLIST {
        let kind = surface["agentTypes"][name].as_str().unwrap_or("missing");
        assert!(
            kind != "undefined" && kind != "missing",
            "agent-core export {name} missing: {v}"
        );
    }
    for name in PROVIDERS_ALL_WORKLIST {
        let kind = surface["providersTypes"][name]
            .as_str()
            .unwrap_or("missing");
        assert!(
            kind != "undefined" && kind != "missing",
            "providers/all export {name} missing: {v}"
        );
    }
    assert_eq!(
        surface["oauthKeys"],
        json!(["default"]),
        "oauth entry is type-only upstream: {v}"
    );
    assert_eq!(surface["oauthDefaultObject"], true, "{v}");
    assert_eq!(surface["aiDefaultObject"], true, "{v}");
    assert_eq!(surface["agentDefaultObject"], true, "{v}");
    assert_eq!(
        surface["typeofType"], "object",
        "Type comes from the typebox shim: {v}"
    );

    // ------------------------------------------------------------------
    // 2) Pure utils: text + transcript replay.
    // ------------------------------------------------------------------
    let text = &v["text"];
    assert_eq!(text["plain"], "abc", "{v}");
    assert_eq!(text["joined"], "a|b", "only text blocks join: {v}");
    assert_eq!(text["sysmsg"], "C\n\nX", "null sections are skipped: {v}");
    assert_eq!(
        text["update"],
        "C\n\nUpdated system prompt section \"x\":\n\nX2\n\nRemoved system prompt section \"y\".",
        "mid-conversation update framing: {v}"
    );

    let transcript = &v["transcript"];
    assert_eq!(transcript["initial"]["role"], "system", "{v}");
    assert_eq!(transcript["initial"]["content"], "P", "{v}");
    assert_eq!(
        transcript["initial"]["toolsAdded"][0]["name"], "read",
        "{v}"
    );
    assert_eq!(transcript["initial"]["timestamp"], 0, "{v}");
    assert_eq!(transcript["normalizedFirstRole"], "system", "{v}");
    assert_eq!(transcript["noInitial"], true, "{v}");
    assert_eq!(
        transcript["currentContent"], "S1\n\nS2",
        "system content replays in order: {v}"
    );
    assert_eq!(
        transcript["currentSectionsKeys"],
        json!(["b"]),
        "null section deleted: {v}"
    );
    assert_eq!(transcript["currentSectionB"], "B", "{v}");
    assert_eq!(
        transcript["currentToolNames"],
        json!(["write"]),
        "removed tool drops out: {v}"
    );
    assert_eq!(
        transcript["currentTimestamp"], 1,
        "first system timestamp wins: {v}"
    );
    assert_eq!(transcript["currentPrompt"], "S1\n\nS2\n\nB", "{v}");
    assert_eq!(transcript["emptyPrompt"], "", "{v}");
    assert_eq!(transcript["withoutInitialLength"], 1, "{v}");
    assert_eq!(
        transcript["collapsedRoles"],
        json!(["system", "user"]),
        "{v}"
    );
    assert_eq!(
        transcript["resolvedLength"], 3,
        "mid-convo capable keeps transcript: {v}"
    );
    assert_eq!(transcript["declaredNames"], json!(["read", "write"]), "{v}");
    assert_eq!(transcript["currentTools"], json!(["write"]), "{v}");
    assert_eq!(transcript["hasRedefinitions"], false, "{v}");
    assert_eq!(
        transcript["redefinitionsHeard"], true,
        "same-name changed declaration: {v}"
    );
    assert_eq!(
        transcript["hasNonAdditive"], true,
        "removals are non-additive: {v}"
    );
    assert_eq!(transcript["resolveToolsAnchorsAdditions"], false, "{v}");
    assert_eq!(
        transcript["resolveToolsRequestNames"],
        json!(["write"]),
        "{v}"
    );
    assert_eq!(
        transcript["strippedDeclaration"],
        "{\"name\":\"t\",\"description\":\"d\",\"parameters\":{\"type\":\"object\"}}",
        "executable fields are stripped: {v}"
    );
    assert_eq!(transcript["declarationsEqual"], true, "{v}");
    assert_eq!(transcript["declarationsEqualChanged"], false, "{v}");
    assert_eq!(transcript["stateChanges"]["added"], json!(["write"]), "{v}");
    assert_eq!(
        transcript["stateChanges"]["removed"],
        json!(["read"]),
        "{v}"
    );

    // ------------------------------------------------------------------
    // 3) JSON repair + streaming parse.
    // ------------------------------------------------------------------
    let json_ = &v["json"];
    assert_eq!(
        json_["repairChanged"], true,
        "raw control chars inside strings are escaped: {v}"
    );
    assert_eq!(
        json_["repairedValue"], true,
        "repaired parse round-trips the newline: {v}"
    );
    assert_eq!(json_["strictValue"], 1, "{v}");
    assert_eq!(
        json_["streaming"], "{\"a\":1,\"b\":[1,2]}",
        "incomplete tail is dropped: {v}"
    );
    assert_eq!(
        json_["streamingString"], "hel",
        "unterminated string keeps its content: {v}"
    );
    assert_eq!(json_["streamingEmpty"], "{}", "{v}");

    // ------------------------------------------------------------------
    // 4) Retry policy + overflow classification.
    // ------------------------------------------------------------------
    let retry = &v["retry"];
    assert_eq!(retry["defaultMax"], 60000, "{v}");
    assert_eq!(retry["delay1"], 100, "base delay on first attempt: {v}");
    assert_eq!(retry["delay3"], 400, "exponential growth: {v}");
    assert_eq!(retry["cappedByPolicy"], 1000, "policy cap applies: {v}");
    assert_eq!(retry["cappedByDefault"], 60000, "default cap applies: {v}");
    assert_eq!(retry["retryable429"], true, "{v}");
    assert_eq!(
        retry["retryableQuota"], false,
        "quota exhaustion is not retryable: {v}"
    );
    assert_eq!(retry["retryableStop"], false, "{v}");
    assert_eq!(retry["overflowText"], true, "{v}");
    assert_eq!(
        retry["overflowRateLimited"], false,
        "rate limit is not overflow: {v}"
    );
    assert_eq!(
        retry["overflowWindow"], true,
        "input beyond context window: {v}"
    );
    assert!(
        retry["overflowPatterns"].as_u64().unwrap_or(0) >= 20,
        "overflow pattern table is ported: {v}"
    );
    assert_eq!(retry["recoverable"], true, "{v}");
    assert_eq!(retry["notRecoverable"], false, "{v}");
    assert_eq!(retry["retryAttempts"], 2, "transient error is retried: {v}");
    assert_eq!(retry["retriedStop"], "stop", "{v}");
    assert_eq!(
        retry["noRetryAttempts"], 1,
        "overflow error returns immediately: {v}"
    );
    assert_eq!(retry["notRetriedStop"], "error", "{v}");

    // ------------------------------------------------------------------
    // 5) Model metadata + cost + thinking levels + string helpers.
    // ------------------------------------------------------------------
    let meta = &v["modelMeta"];
    assert_eq!(meta["typeDefault"], "chat", "{v}");
    assert_eq!(meta["typeImage"], "image", "{v}");
    assert_eq!(meta["isChat"], true, "{v}");
    assert_eq!(meta["hasApiChat"], true, "{v}");
    assert_eq!(
        meta["hasApiImage"], false,
        "non-chat models never match hasApi: {v}"
    );
    assert_eq!(
        meta["costTotalMicro"], 10500,
        "3/15 per-MTok base math: {v}"
    );
    assert_eq!(meta["costInputMicro"], 3000, "{v}");
    assert_eq!(
        meta["costTierInputMicro"], 9000,
        "tier over 1000 input tokens: {v}"
    );
    assert_eq!(
        meta["levelsAll"],
        json!(["off", "minimal", "low", "medium", "high"]),
        "xhigh/max need explicit mappings: {v}"
    );
    assert_eq!(
        meta["levelsMapped"],
        json!(["minimal", "low", "medium", "high"]),
        "{v}"
    );
    assert_eq!(meta["levelsNone"], json!(["off"]), "{v}");
    assert_eq!(
        meta["clampHigh"], "high",
        "xhigh clamps down to the next available: {v}"
    );
    assert_eq!(meta["clampOff"], "off", "{v}");
    assert_eq!(meta["equalSame"], true, "{v}");
    assert_eq!(meta["equalDifferent"], false, "{v}");
    assert_eq!(
        meta["uuidv7Shape"], true,
        "uuidv7 keeps the version/variant nibbles: {v}"
    );
    assert_eq!(meta["stringEnumShape"], true, "{v}");
    assert_eq!(meta["typeIsObject"], true, "{v}");
    assert_eq!(meta["modelsError"], "ModelsError:auth:login failed", "{v}");
    assert_eq!(
        meta["modelsErrorCause"], "login failed: net down",
        "cause detail is folded in: {v}"
    );

    // ------------------------------------------------------------------
    // 6) Diagnostics.
    // ------------------------------------------------------------------
    let diag = &v["diag"];
    assert_eq!(diag["formatError"], "boom", "{v}");
    assert_eq!(diag["formatString"], "plain", "{v}");
    assert_eq!(diag["extractedMessage"], "oops", "{v}");
    assert_eq!(diag["extractedName"], "Error", "{v}");
    assert_eq!(diag["thrownValueName"], "ThrownValue", "{v}");
    assert_eq!(diag["count"], 1, "{v}");
    assert_eq!(diag["type"], "retry", "{v}");
    assert_eq!(diag["details"]["attempt"], 1, "{v}");

    // ------------------------------------------------------------------
    // 7) Event streams + frame round trip.
    // ------------------------------------------------------------------
    let streams = &v["streams"];
    assert_eq!(
        streams["subclass"], true,
        "AssistantMessageEventStream extends EventStream: {v}"
    );
    assert_eq!(streams["assistantText"], "hi", "{v}");
    assert_eq!(
        streams["durationType"], "number",
        "durationMs is stamped: {v}"
    );
    assert_eq!(streams["rawResult"], 2, "{v}");
    assert_eq!(
        streams["rawValues"],
        json!([1, 2]),
        "async iteration drains the queue: {v}"
    );

    let frames = &v["frames"];
    assert_eq!(
        frames["frameTypes"],
        json!([
            "start",
            "text_start",
            "text_delta",
            "text_end",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_delta",
            "toolcall_end"
        ]),
        "{v}"
    );
    assert_eq!(
        frames["doneFrameDropped"], true,
        "done encodes to undefined: {v}"
    );
    assert_eq!(
        frames["terminalError"], "Assistant message event text_delta follows a terminal event",
        "{v}"
    );
    assert_eq!(
        frames["reducedText"], "Hello",
        "text_end rewrites the block: {v}"
    );
    assert_eq!(frames["reducedStop"], "pending", "{v}");
    assert_eq!(frames["reducedToolName"], "grep", "{v}");
    assert_eq!(
        frames["reducedToolArgs"], "{\"q\":\"x\"}",
        "toolcall_end restores arguments: {v}"
    );
    assert_eq!(frames["noStartReduce"], true, "{v}");

    // ------------------------------------------------------------------
    // 8) Provider/models skeleton: registry reads real; execution throws
    //    the documented host-side error.
    // ------------------------------------------------------------------
    let provider = &v["provider"];
    assert_eq!(provider["providerName"], "probe", "{v}");
    assert_eq!(provider["chatModels"], json!(["probe-1"]), "{v}");
    assert_eq!(provider["registry"], json!(["probe-1"]), "{v}");
    assert_eq!(provider["single"], "probe-1", "{v}");
    assert_eq!(provider["byId"], "probe", "{v}");
    assert_eq!(
        provider["streamOk"], true,
        "createProvider dispatches to the caller-supplied streams: {v}"
    );
    assert_eq!(
        provider["emptyProviderError"],
        "Provider empty: at least one of \"api\", \"images\", or \"classifiers\" is required.",
        "{v}"
    );
    for (key, name) in [
        ("streamError", "stream"),
        ("completeError", "complete"),
        ("streamSimpleError", "streamSimple"),
        ("refreshError", "refresh"),
        ("loginError", "login"),
        ("availableError", "getAvailable"),
        ("checkAuthError", "checkAuth"),
    ] {
        assert_eq!(
            provider[key],
            format!("Models.{name} is not available in the pir extension runtime (provider execution is host-side)"),
            "provider execution must throw the documented error ({key}): {v}"
        );
    }

    // ------------------------------------------------------------------
    // 9) Faux provider: scripted double streams real events.
    // ------------------------------------------------------------------
    let faux = &v["faux"];
    assert_eq!(faux["modelId"], "faux-1", "{v}");
    assert_eq!(faux["finalText"], "hello", "{v}");
    assert_eq!(faux["finalStop"], "stop", "{v}");
    assert_eq!(
        faux["eventTypes"],
        json!(["start", "text_start", "text_delta", "text_end", "done"]),
        "{v}"
    );
    assert_eq!(faux["callCount"], 1, "{v}");
    assert_eq!(faux["pendingCount"], 0, "{v}");
    assert_eq!(faux["coreModels"], 1, "{v}");
    assert_eq!(
        faux["textShape"], "{\"type\":\"text\",\"text\":\"t\"}",
        "{v}"
    );
    assert_eq!(
        faux["thinkingShape"], "{\"type\":\"thinking\",\"thinking\":\"th\"}",
        "{v}"
    );
    assert_eq!(faux["callName"], "grep", "{v}");
    assert_eq!(faux["callArgs"], "{\"q\":\"x\"}", "{v}");
    assert_eq!(faux["assistDefaultStop"], "stop", "{v}");

    // ------------------------------------------------------------------
    // 10) Tool validation against the host schema validator.
    // ------------------------------------------------------------------
    let validation = &v["validation"];
    assert_eq!(validation["okArgs"], "{\"a\":\"x\"}", "{v}");
    assert_eq!(validation["directArgs"], "{\"a\":\"y\"}", "{v}");
    assert!(
        validation["missingArgsError"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Validation failed for tool \"t\":"),
        "required-argument violation formats the tool name: {v}"
    );
    assert_eq!(
        validation["unknownToolError"], "Tool \"nope\" not found",
        "{v}"
    );

    // ------------------------------------------------------------------
    // 11) Auxiliary helpers: auth, stores, session cleanup, lazy wrappers.
    // ------------------------------------------------------------------
    let aux = &v["aux"];
    assert_eq!(
        aux["fileExists"], false,
        "no filesystem on the extension realm: {v}"
    );
    assert_eq!(aux["missingEnvUndefined"], true, "{v}");
    assert_eq!(aux["apiKey"], "sk-test", "{v}");
    assert_eq!(aux["apiKeySource"], "FAKE_KEY_ENV", "{v}");
    assert_eq!(aux["unresolvedUndefined"], true, "{v}");
    assert_eq!(aux["credentialKey"], "k1", "{v}");
    assert_eq!(aux["listType"], "api_key", "{v}");
    assert_eq!(aux["listProviderId"], "p1", "{v}");
    assert_eq!(aux["deleted"], true, "{v}");
    assert_eq!(aux["modelsStoreLen"], 1, "{v}");
    assert_eq!(aux["modelsStoreCheckedAt"], 5, "{v}");
    assert_eq!(
        aux["sessionCleanup"],
        json!(["s1"]),
        "unregister removes the cleanup: {v}"
    );
    assert_eq!(
        aux["lazyText"], "lazy",
        "lazyApi forwards the async-loaded stream: {v}"
    );
    assert_eq!(
        aux["lazyStreamError"], "setup boom",
        "setup errors surface as error messages: {v}"
    );
    assert_eq!(aux["lazyOAuthName"], "fake-oauth:true", "{v}");
    assert_eq!(aux["lazyOAuthAccess"], "a", "{v}");

    // ------------------------------------------------------------------
    // 12) Agent-core: Agent stub stays inert; loop faces are host-side.
    // ------------------------------------------------------------------
    let agent = &v["agent"];
    assert_eq!(agent["constructed"], true, "{v}");
    assert_eq!(agent["hasQueued"], true, "{v}");
    assert_eq!(agent["peek"], json!(["s1"]), "{v}");
    assert_eq!(agent["stateLen"], 0, "{v}");
    assert_eq!(agent["clearedHasQueued"], false, "{v}");
    assert_eq!(agent["idle"], true, "{v}");
    assert_eq!(
        agent["promptError"], "Agent execution is not available in the pir extension runtime",
        "Agent stub must not regress: {v}"
    );
    assert_eq!(
        agent["continueError"], "Agent execution is not available in the pir extension runtime",
        "{v}"
    );
    assert_eq!(
        agent["runError"], "Agent execution is not available in the pir extension runtime",
        "{v}"
    );
    for (key, name) in [
        ("loopError", "agentLoop"),
        ("loopContinueError", "agentLoopContinue"),
        ("runLoopError", "runAgentLoop"),
        ("runLoopContinueError", "runAgentLoopContinue"),
        ("toolCallError", "runToolCall"),
        ("proxyError", "streamProxy"),
    ] {
        assert_eq!(
            agent[key],
            format!("{name} is not available in the pir extension runtime (agent execution is host-side)"),
            "agent-core face must throw the documented error ({key}): {v}"
        );
    }
    assert_eq!(
        agent["setDefaultStreamFnOk"], true,
        "setDefaultStreamFn stays an inert setter: {v}"
    );

    // ------------------------------------------------------------------
    // 13) providers/all: the catalog manifest is real; model objects are not.
    // ------------------------------------------------------------------
    let providers_all = &v["providersAll"];
    // Upstream `getBuiltinProviders()` returns the catalog provider id strings
    // (`Object.keys(MODELS)`), not provider objects. The shim derives them from
    // the pinned manifest's file names, which must be exactly the Rust catalog
    // provider ids (42), in the same sorted order.
    let mut expected_providers: Vec<String> = catalog::builtin_catalog_manifest()
        .expect("catalog manifest")
        .files
        .keys()
        .map(|file| {
            file.strip_suffix(".json")
                .unwrap_or_else(|| panic!("manifest file {file} is not a catalog json resource"))
                .to_string()
        })
        .collect();
    expected_providers.sort();
    let shim_providers: Vec<String> = providers_all["providers"]
        .as_array()
        .unwrap_or_else(|| panic!("getBuiltinProviders must return an array: {v}"))
        .iter()
        .map(|value| {
            value
                .as_str()
                .unwrap_or_else(|| panic!("provider ids are strings: {v}"))
                .to_string()
        })
        .collect();
    let mut sorted_shim_providers = shim_providers.clone();
    sorted_shim_providers.sort();
    assert_eq!(
        sorted_shim_providers, expected_providers,
        "shim provider ids must match the embedded catalog files: {v}"
    );
    assert_eq!(
        shim_providers.len(),
        catalog::CATALOG_FILES.len(),
        "one id per built-in catalog provider: {v}"
    );
    assert_eq!(
        shim_providers.len(),
        42,
        "getBuiltinProviders() must expose all 42 catalog ids: {v}"
    );
    assert!(
        shim_providers.iter().any(|id| id == "openai")
            && shim_providers.iter().any(|id| id == "anthropic")
            && shim_providers.iter().any(|id| id == "radius"),
        "enumerable provider ids include the catalog providers (radius included): {v}"
    );
    let generated_at = catalog::builtin_catalog_generated_at().expect("generated-at constant");
    assert_eq!(
        providers_all["generatedAt"].as_u64(),
        Some(generated_at),
        "shim generatedAt must equal the Rust catalog constant: {v}"
    );
    assert_eq!(
        providers_all["manifest"],
        serde_json::to_value(catalog::builtin_catalog_manifest().expect("catalog manifest"))
            .expect("serialize manifest"),
        "shim manifest must equal the Rust catalog manifest: {v}"
    );
    let version = catalog::builtin_catalog_version();
    assert_eq!(
        providers_all["version"].as_str(),
        Some(version.as_str()),
        "shim version must equal builtin_catalog_version(): {v}"
    );

    // The runtime getters read the embedded deflate blob, not the checked-in
    // `.js.txt`; inflate the exact `include_bytes!` asset so a stale blob (the
    // D5 catalog update) fails here instead of silently serving an old catalog.
    let shim = pir::extensions::api::all_shims()
        .into_iter()
        .find(|(name, _)| *name == PROVIDERS_ALL_SHIM)
        .map(|(_, shim)| shim)
        .expect("providers/all shim is embedded by the extension host");
    let shim_text = shim
        .inflate()
        .expect("inflate the embedded providers/all shim");
    let blob_manifest = pinned_providers_all_manifest(&shim_text);
    let blob_version = pinned_providers_all_version(&shim_text);
    assert_eq!(
        providers_all["manifest"], blob_manifest,
        "getBuiltinModelDataManifest() must equal the manifest pinned in the embedded blob: {v}"
    );
    assert_eq!(
        providers_all["version"].as_str(),
        Some(blob_version),
        "getBuiltinModelDataVersion() must equal the version pinned in the embedded blob: {v}"
    );
    assert_eq!(
        blob_manifest,
        serde_json::to_value(catalog::builtin_catalog_manifest().expect("catalog manifest"))
            .expect("serialize manifest"),
        "embedded blob manifest must equal builtin_catalog_manifest()"
    );
    assert_eq!(
        blob_manifest["structureHash"],
        json!(catalog::builtin_catalog_structure_hash().expect("structure hash")),
        "embedded blob structureHash must equal builtin_catalog_structure_hash()"
    );
    assert_eq!(
        blob_manifest["files"].as_object().map(|files| files.len()),
        Some(42),
        "embedded blob must pin 42 provider catalog files"
    );
    for key in [
        "modelError",
        "imageModelError",
        "classifierModelError",
        "modelsError",
        "imageModelsError",
        "classifierModelsError",
        "allModelsError",
        "builtinProvidersError",
        "builtinModelsError",
    ] {
        let message = providers_all[key].as_str().unwrap_or_default();
        assert!(
            message.contains("is not available")
                && message.contains("not bundled in the pir runtime"),
            "providers/all face {key} must fail loudly: {v}"
        );
    }
    assert_eq!(
        providers_all["radiusType"], "function",
        "radiusProvider is a loud function proxy: {v}"
    );
    assert!(
        providers_all["radiusUseError"]
            .as_str()
            .unwrap_or_default()
            .contains("built-in provider implementations are not bundled"),
        "{v}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
