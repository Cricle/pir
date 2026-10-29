#!/usr/bin/env node
// Deterministic provider-catalog generator.
//
// Upstream `@earendil-works/pi-ai` does not commit its per-provider model data:
// `packages/ai/scripts/generate-models.ts` rebuilds
// `packages/ai/src/providers/data/*.json` from models.dev, the OpenRouter API
// and gateway pricing at build time. pir ships a frozen snapshot of those
// files under `src/ai/providers/catalog/`, but in pir's own (simpler, stable)
// catalog shape.
//
// This script converts an upstream data directory into that shape without
// touching the network. It is deterministic: provider files are discovered in
// sorted order, model keys are sorted, fields are emitted in a fixed order,
// files are compact JSON (no insignificant whitespace; they are embedded
// verbatim with `include_str!`), and every file ends with exactly one newline.
//
// Upstream data directory (only needed for a full regeneration): the
// `providers/data` directory shipped in the installed `@earendil-works/pi-ai`
// package. It is GENERATED at upstream build time (`npm run generate:models`)
// and is not committed to the pi repository, so a pi checkout does not contain
// it. Resolution order, first existing directory wins:
//
//   1. --data-dir <dir>
//   2. $PI_AI_DATA_DIR
//   3. ../pi/packages/ai/src/providers/data (a checkout that ran the generator)
//   4. %APPDATA%/npm/node_modules/@earendil-works/pi-coding-agent/
//        node_modules/@earendil-works/pi-ai/dist/providers/data
//   5. $npm_config_prefix/node_modules/@earendil-works/pi-ai/dist/providers/data
//
// When none exists, use `--self-check` (canonical snapshot gate, no upstream
// data); do not invent model data from elsewhere.
//
// Usage:
//   node scripts/gen_catalog.mjs --data-dir <upstream data dir> [--all]
//   node scripts/gen_catalog.mjs --data-dir <dir> --only anthropic --only google
//   node scripts/gen_catalog.mjs --data-dir <dir> --check
//   node scripts/gen_catalog.mjs --self-check
//
// `--check` regenerates in memory and exits non-zero when any catalog file
// would change; it never writes. `--only <provider>` restricts both write and
// check to the named provider(s). `--all` (the default) selects every provider
// that has an upstream snapshot.
//
// `--self-check` needs NO upstream data directory: it re-canonicalizes the
// checked-in catalog files through the same serializer (field order, key
// order, compact JSON, LF, one trailing newline) and exits non-zero on any drift. Use it in
// environments where the generated upstream `providers/data` directory is
// unavailable; `--check --data-dir <dir>` remains the full regeneration gate.
//
// Fields upstream carries that pir's `CatalogModel` cannot represent
// (`headers`, `lab`, `enabled`, `providers`, `promptCache` as a top-level
// field, ...) are either folded into `compat` where pir supports them
// (`inputLimits`, `promptCache`) or dropped. Values that cannot be derived
// from upstream data at all live in `scripts/catalog-overrides.json`.

import {
  existsSync,
  readFileSync,
  readdirSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(scriptDir, "..");
const catalogDir = join(repoRoot, "src", "ai", "providers", "catalog");
const overridesPath = join(scriptDir, "catalog-overrides.json");

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

function usage() {
  return `Usage: node scripts/gen_catalog.mjs --data-dir <dir> [--all | --only <provider>]... [--check] [--out-dir <dir>]
       node scripts/gen_catalog.mjs --self-check [--out-dir <dir>]

  --data-dir <dir>   Upstream pi-ai providers/data directory (required unless
                     PI_AI_DATA_DIR or a known checkout/package location exists)
  --all              Generate every provider with an upstream snapshot (default)
  --only <provider>  Generate only this provider (repeatable)
  --check            Exit 1 when regeneration would change any catalog file
  --self-check       No upstream data needed: re-canonicalize the checked-in
                     catalog files and exit 1 on formatting, key-order or
                     compactness drift
  --out-dir <dir>    Write/check against this directory instead of
                     src/ai/providers/catalog
  --help             Show this help`;
}

function fail(message) {
  console.error(`gen_catalog: ${message}`);
  process.exit(2);
}

function parseArgs(argv) {
  const options = {
    dataDir: undefined,
    outDir: catalogDir,
    check: false,
    selfCheck: false,
    all: false,
    only: [],
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    const next = () => {
      const value = argv[index + 1];
      if (value === undefined || value.startsWith("--")) {
        fail(`${arg} requires a value`);
      }
      index += 1;
      return value;
    };
    switch (arg) {
      case "--data-dir":
        options.dataDir = resolve(next());
        break;
      case "--out-dir":
        options.outDir = resolve(next());
        break;
      case "--only":
        options.only.push(next());
        break;
      case "--all":
        options.all = true;
        break;
      case "--check":
        options.check = true;
        break;
      case "--self-check":
        options.selfCheck = true;
        break;
      case "--help":
      case "-h":
        console.log(usage());
        process.exit(0);
        break;
      default:
        fail(`unknown argument: ${arg}\n\n${usage()}`);
    }
  }
  if (options.all && options.only.length > 0) {
    fail("use either --all or --only, not both");
  }
  if (options.selfCheck && (options.dataDir !== undefined || options.check)) {
    fail("--self-check runs without --data-dir/--check");
  }
  return options;
}

const knownDataDirs = [
  process.env.PI_AI_DATA_DIR,
  resolve(repoRoot, "..", "pi", "packages", "ai", "src", "providers", "data"),
  process.env.APPDATA
    ? join(
        process.env.APPDATA,
        "npm",
        "node_modules",
        "@earendil-works",
        "pi-coding-agent",
        "node_modules",
        "@earendil-works",
        "pi-ai",
        "dist",
        "providers",
        "data",
      )
    : undefined,
  process.env.npm_config_prefix
    ? join(
        process.env.npm_config_prefix,
        "node_modules",
        "@earendil-works",
        "pi-ai",
        "dist",
        "providers",
        "data",
      )
    : undefined,
].filter(Boolean);

function resolveDataDir(explicit) {
  if (explicit) {
    if (!isDirectory(explicit)) fail(`--data-dir is not a directory: ${explicit}`);
    return explicit;
  }
  for (const candidate of knownDataDirs) {
    if (isDirectory(candidate)) return resolve(candidate);
  }
  fail(
    "no upstream data directory found; pass --data-dir <path> (for example " +
      "the installed @earendil-works/pi-ai dist/providers/data directory)",
  );
}

function isDirectory(path) {
  try {
    return statSync(path).isDirectory();
  } catch {
    return false;
  }
}

// ---------------------------------------------------------------------------
// Schema knowledge
// ---------------------------------------------------------------------------

// Every `compat` key pir's `OpenAiCompat` deserializes (src/ai/types/model/
// compat.rs). Unknown upstream keys are reported and dropped instead of being
// written into a catalog where serde would silently ignore them.
const KNOWN_COMPAT_KEYS = new Set([
  "supportsReasoningEffort",
  "supportsStrictMode",
  "samplingParamsByThinkingLevel",
  "supportsStrictTools",
  "forceAdaptiveThinking",
  "allowEmptySignature",
  "supportsTemperature",
  "supportsEagerToolInputStreaming",
  "supportsCacheControlOnTools",
  "supportsMidConvoSystemMessages",
  "supportsMidConvoToolAdditions",
  "supportsMidConvoToolChanges",
  "supportsMidConvoEffort",
  "supportsUsageInStreaming",
  "thinkingFormat",
  "maxTokensField",
  "supportsFinishReason",
  "allowedFallbackModels",
  "sendSessionAffinityHeaders",
  "customHeaders",
  "supportsStore",
  "supportsDeveloperRole",
  "requiresToolResultName",
  "requiresAssistantAfterToolResult",
  "requiresThinkingAsText",
  "requiresReasoningContentOnAssistantMessages",
  "zaiToolStream",
  "supportsOpenAIGrammarTools",
  "supportsAdditionalTools",
  "supportsToolSearch",
  "supportsExplicitPromptCacheMode",
  "supportsMaxOutputTokens",
  "cacheControlFormat",
  "sessionAffinityFormat",
  "supportsLongCacheRetention",
  "openRouterRouting",
  "vercelGatewayRouting",
  "chatTemplateKwargs",
  "chatTemplateArgs",
  "vllmPriority",
  "supportsThinkingTokenBudget",
  "thinkingTokenBudgetField",
  "inputLimits",
  "promptCache",
  "authHeader",
]);

const PROVIDER_FILE_NAME = /^[a-z0-9][a-z0-9._-]*\.json$/;

// Canonical thinking-level order (then any extra keys, sorted).
const THINKING_LEVEL_ORDER = [
  "off",
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
  "max",
];

const MODEL_TYPE_ORDER = ["chat", "image", "classifier"];

// ---------------------------------------------------------------------------
// Serialization helpers
// ---------------------------------------------------------------------------

function sortObjectKeys(value, order = undefined) {
  if (Array.isArray(value)) return value.map((item) => sortObjectKeys(item));
  if (value === null || typeof value !== "object") return value;
  const keys = Object.keys(value);
  if (order) {
    const ranked = new Map(order.map((key, index) => [key, index]));
    keys.sort((a, b) => {
      const ra = ranked.has(a) ? ranked.get(a) : Number.MAX_SAFE_INTEGER;
      const rb = ranked.has(b) ? ranked.get(b) : Number.MAX_SAFE_INTEGER;
      if (ra !== rb) return ra - rb;
      return a < b ? -1 : a > b ? 1 : 0;
    });
  } else {
    keys.sort();
  }
  const result = {};
  for (const key of keys) result[key] = sortObjectKeys(value[key]);
  return result;
}

function canonicalModel(model) {
  const result = {};
  const put = (key) => {
    if (Object.prototype.hasOwnProperty.call(model, key) && model[key] !== undefined) {
      result[key] = model[key];
    }
  };
  put("id");
  put("name");
  put("type");
  put("api");
  put("baseUrl");
  put("reasoning");
  put("thinkingLevelMap");
  put("input");
  put("output");
  put("cost");
  put("compat");
  put("contextWindow");
  put("maxTokens");
  // Preserve any additional override-provided field deterministically.
  for (const key of Object.keys(model).sort()) {
    if (!Object.prototype.hasOwnProperty.call(result, key) && model[key] !== undefined) {
      result[key] = model[key];
    }
  }
  if (result.thinkingLevelMap && typeof result.thinkingLevelMap === "object") {
    result.thinkingLevelMap = canonicalThinkingMap(result.thinkingLevelMap);
  }
  if (result.input) result.input = normalizeModalities(result.input);
  if (result.output) result.output = normalizeModalities(result.output);
  if (result.cost && typeof result.cost === "object") {
    result.cost = convertCostWithTiers(result.cost);
  }
  if (result.compat && typeof result.compat === "object") {
    result.compat = canonicalCompat(result.compat);
  }
  return result;
}

function convertCostWithTiers(cost) {
  const result = {
    input: numberOrZero(cost.input),
    output: numberOrZero(cost.output),
    cacheRead: numberOrZero(cost.cacheRead),
    cacheWrite: numberOrZero(cost.cacheWrite),
  };
  if (Array.isArray(cost.tiers) && cost.tiers.length > 0) {
    result.tiers = cost.tiers.map((tier) => ({
      inputTokensAbove: numberOrZero(tier.inputTokensAbove),
      input: numberOrZero(tier.input),
      output: numberOrZero(tier.output),
      cacheRead: numberOrZero(tier.cacheRead),
      cacheWrite: numberOrZero(tier.cacheWrite),
    }));
  }
  return result;
}

function canonicalThinkingMap(map) {
  return sortObjectKeys(map, THINKING_LEVEL_ORDER);
}

function canonicalCompat(compat) {
  // `samplingParamsByThinkingLevel` is a level-keyed object; keep its level
  // order canonical too. Other nested values are plain JSON.
  const result = {};
  for (const key of Object.keys(compat).sort()) {
    const value = compat[key];
    if (key === "samplingParamsByThinkingLevel" && value && typeof value === "object") {
      result[key] = sortObjectKeys(value, THINKING_LEVEL_ORDER);
    } else {
      result[key] = sortObjectKeys(value);
    }
  }
  return result;
}

function normalizeModalities(values) {
  if (!Array.isArray(values) || values.length === 0) return values;
  const order = ["text", "image", "audio", "video"];
  const ranked = new Map(order.map((name, index) => [name, index]));
  return [...values].sort((a, b) => {
    const ra = ranked.has(a) ? ranked.get(a) : Number.MAX_SAFE_INTEGER;
    const rb = ranked.has(b) ? ranked.get(b) : Number.MAX_SAFE_INTEGER;
    if (ra !== rb) return ra - rb;
    return a < b ? -1 : a > b ? 1 : 0;
  });
}

function convertCost(cost) {
  return convertCostWithTiers(cost && typeof cost === "object" ? cost : {});
}

function numberOrZero(value) {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

// ---------------------------------------------------------------------------
// Upstream -> pir conversion
// ---------------------------------------------------------------------------

function loadProviderData(dataDir, providerId) {
  const path = join(dataDir, `${providerId}.json`);
  const parsed = JSON.parse(readFileSync(path, "utf8"));
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    fail(`${path}: expected an object keyed by api id`);
  }
  return parsed;
}

function flattenEntries(providerData) {
  const entries = [];
  for (const apiId of Object.keys(providerData).sort()) {
    const group = providerData[apiId];
    if (group === null || typeof group !== "object" || Array.isArray(group)) {
      fail(`provider data api group '${apiId}' is not an object`);
    }
    for (const key of Object.keys(group).sort()) {
      const model = group[key];
      if (model === null || typeof model !== "object" || Array.isArray(model)) {
        fail(`${apiId}/${key}: model entry is not an object`);
      }
      if (typeof model.id !== "string" || model.id.length === 0) {
        fail(`${apiId}/${key}: model entry has no id`);
      }
      if (typeof model.api !== "string" || model.api.length === 0) {
        fail(`${apiId}/${key}: model entry has no api`);
      }
      const type = typeof model.type === "string" ? model.type : "chat";
      if (!MODEL_TYPE_ORDER.includes(type)) {
        fail(`${apiId}/${key}: unknown model type '${type}'`);
      }
      entries.push({ key, apiId, type, model });
    }
  }
  return entries;
}

function catalogKey(entry) {
  return entry.type === "chat" ? entry.model.id : `${entry.type}:${entry.model.id}`;
}

function deriveProviderBaseUrl(entries) {
  const counts = new Map();
  for (const entry of entries) {
    const baseUrl = entry.model.baseUrl;
    if (typeof baseUrl === "string" && baseUrl.length > 0) {
      counts.set(baseUrl, (counts.get(baseUrl) ?? 0) + 1);
    }
  }
  if (counts.size === 0) return undefined;
  return [...counts.entries()].sort((a, b) => {
    if (b[1] !== a[1]) return b[1] - a[1];
    return a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0;
  })[0][0];
}

function deriveModel(entry, providerBaseUrl, warnings) {
  const source = entry.model;
  const type = entry.type;
  const model = {
    id: String(source.id),
    name: typeof source.name === "string" && source.name.length > 0
      ? source.name
      : String(source.id),
  };
  if (type !== "chat") model.type = type;
  model.api = String(source.api);
  if (
    typeof source.baseUrl === "string" &&
    source.baseUrl.length > 0 &&
    source.baseUrl !== providerBaseUrl
  ) {
    model.baseUrl = source.baseUrl;
  }
  if (type === "chat" && source.reasoning === true) model.reasoning = true;
  if (source.thinkingLevelMap && typeof source.thinkingLevelMap === "object") {
    model.thinkingLevelMap = canonicalThinkingMap(source.thinkingLevelMap);
  }
  if (Array.isArray(source.input)) model.input = normalizeModalities(source.input);
  if (type === "image") {
    model.output = normalizeModalities(
      Array.isArray(source.output) && source.output.length > 0
        ? source.output
        : ["image"],
    );
  }
  model.cost = convertCost(source.cost);
  const compat = deriveCompat(source, warnings, `${entry.model.id}`);
  if (compat) model.compat = compat;
  if (type !== "image" && typeof source.contextWindow === "number") {
    model.contextWindow = source.contextWindow;
  }
  if (type === "chat" && typeof source.maxTokens === "number") {
    model.maxTokens = source.maxTokens;
  }
  return canonicalModel(model);
}

function deriveCompat(source, warnings, label) {
  const compat = {};
  if (source.compat && typeof source.compat === "object") {
    for (const key of Object.keys(source.compat)) {
      if (!KNOWN_COMPAT_KEYS.has(key)) {
        warnings.push(`${label}: dropping unsupported compat field '${key}'`);
        continue;
      }
      compat[key] = source.compat[key];
    }
  }
  // pir carries these model-level upstream fields on the compat block so
  // models.json round-trips without changing `Model`'s shape.
  if (source.inputLimits !== undefined) compat.inputLimits = source.inputLimits;
  if (source.promptCache !== undefined) compat.promptCache = source.promptCache;
  if (Object.keys(compat).length === 0) return undefined;
  return canonicalCompat(compat);
}

// ---------------------------------------------------------------------------
// Overrides
// ---------------------------------------------------------------------------

function loadOverrides() {
  if (!existsSync(overridesPath)) return { providers: {}, models: {}, extraModels: {} };
  const parsed = JSON.parse(readFileSync(overridesPath, "utf8"));
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    fail(`${overridesPath}: expected an object`);
  }
  return {
    providers: parsed.providers ?? {},
    models: parsed.models ?? {},
    extraModels: parsed.extraModels ?? {},
  };
}

function applyModelOverride(model, override, providerBaseUrl) {
  const result = { ...model };
  for (const key of Object.keys(override)) {
    const value = override[key];
    if (key === "compat") {
      if (value === null) {
        delete result.compat;
        continue;
      }
      const merged = { ...(result.compat ?? {}) };
      for (const compatKey of Object.keys(value)) {
        if (value[compatKey] === null) delete merged[compatKey];
        else merged[compatKey] = value[compatKey];
      }
      if (Object.keys(merged).length > 0) result.compat = canonicalCompat(merged);
      else delete result.compat;
      continue;
    }
    if (key === "baseUrl") {
      if (value === null || value === providerBaseUrl) delete result.baseUrl;
      else result.baseUrl = value;
      continue;
    }
    if (value === null) delete result[key];
    else result[key] = value;
  }
  return canonicalModel(result);
}

function applyProviderOverrideName(providerId, overrides) {
  const entry = overrides.providers[providerId] ?? {};
  return typeof entry.name === "string" ? entry.name : providerId;
}

function providerBaseUrlOverride(providerId, overrides) {
  const entry = overrides.providers[providerId] ?? {};
  if (!Object.prototype.hasOwnProperty.call(entry, "baseUrl")) return { present: false };
  return { present: true, value: entry.baseUrl };
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

function generateProvider(providerId, dataDir, overrides, warnings) {
  const providerData = loadProviderData(dataDir, providerId);
  const entries = flattenEntries(providerData);
  const derivedBaseUrl = deriveProviderBaseUrl(entries);
  const override = providerBaseUrlOverride(providerId, overrides);
  const providerBaseUrl = override.present ? override.value ?? undefined : derivedBaseUrl;

  const generated = new Map();
  for (const entry of entries) {
    const key = catalogKey(entry);
    if (generated.has(key)) {
      fail(`${providerId}: duplicate catalog key '${key}' in upstream data`);
    }
    generated.set(key, deriveModel(entry, providerBaseUrl, warnings));
  }

  const modelOverrides = overrides.models[providerId] ?? {};
  for (const key of Object.keys(modelOverrides)) {
    if (!generated.has(key)) {
      fail(`${overridesPath}: model override '${providerId}/${key}' does not exist upstream`);
    }
    generated.set(
      key,
      applyModelOverride(generated.get(key), modelOverrides[key], providerBaseUrl),
    );
  }

  const extraModels = overrides.extraModels[providerId] ?? {};
  for (const key of Object.keys(extraModels)) {
    if (generated.has(key)) {
      fail(`${overridesPath}: extra model '${providerId}/${key}' shadows an upstream model`);
    }
    generated.set(key, canonicalModel(extraModels[key]));
  }

  const models = {};
  for (const key of [...generated.keys()].sort()) models[key] = generated.get(key);

  const provider = { name: applyProviderOverrideName(providerId, overrides) };
  if (typeof providerBaseUrl === "string" && providerBaseUrl.length > 0) {
    provider.baseUrl = providerBaseUrl;
  }
  provider.models = models;
  return { provider, json: `${JSON.stringify(provider)}\n` };
}

function discoverProviders(dataDir) {
  const providers = readdirSync(dataDir)
    .filter((name) => PROVIDER_FILE_NAME.test(name) && name !== ".manifest.json")
    .map((name) => name.slice(0, -".json".length))
    .sort();
  if (providers.length === 0) fail(`no provider JSON files in ${dataDir}`);
  return providers;
}

// ---------------------------------------------------------------------------
// Snapshot self-check (no upstream data directory required)
// ---------------------------------------------------------------------------

/// Rebuild one checked-in catalog file from its own parsed content using the
/// generator's canonical serializer. This cannot discover missing models (that
/// needs upstream data), but it proves the snapshot is exactly what the writer
/// would emit: provider field order, sorted model keys, canonical model field
/// order, sorted compat keys, canonical thinking-level/modality order, compact
/// JSON (no insignificant whitespace), LF line endings and exactly one
/// trailing newline.
function canonicalizeCatalogFile(providerId, raw) {
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) {
    fail(`${providerId}: provider file is not an object`);
  }
  if (raw.models === null || typeof raw.models !== "object" || Array.isArray(raw.models)) {
    fail(`${providerId}: models is not an object`);
  }
  const models = {};
  const dropped = [];
  for (const key of Object.keys(raw.models).sort()) {
    const model = raw.models[key];
    if (model === null || typeof model !== "object" || Array.isArray(model)) {
      fail(`${providerId}/${key}: model entry is not an object`);
    }
    if (model.compat && typeof model.compat === "object") {
      for (const compatKey of Object.keys(model.compat)) {
        if (!KNOWN_COMPAT_KEYS.has(compatKey)) {
          dropped.push(`${providerId}/${key}: unknown compat field '${compatKey}'`);
          delete model.compat[compatKey];
        }
      }
      if (Object.keys(model.compat).length === 0) delete model.compat;
    }
    models[key] = canonicalModel(model);
  }
  const provider = {
    name: typeof raw.name === "string" && raw.name.length > 0 ? raw.name : providerId,
  };
  if (typeof raw.baseUrl === "string" && raw.baseUrl.length > 0) {
    provider.baseUrl = raw.baseUrl;
  }
  provider.models = models;
  return {
    provider,
    json: `${JSON.stringify(provider)}\n`,
    warnings: dropped,
  };
}

function selfCheck(options) {
  if (!isDirectory(options.outDir)) {
    fail(`--out-dir is not a directory: ${options.outDir}`);
  }
  let files = readdirSync(options.outDir)
    .filter((name) => PROVIDER_FILE_NAME.test(name))
    .sort();
  if (files.length === 0) fail(`no catalog JSON files in ${options.outDir}`);
  if (options.only.length > 0) {
    const wanted = new Set(options.only);
    const missing = options.only.filter((id) => !files.includes(`${id}.json`));
    if (missing.length > 0) {
      fail(`no checked-in catalog file for: ${missing.join(", ")}`);
    }
    files = files.filter((name) => wanted.has(name.slice(0, -".json".length)));
  }

  const drifted = [];
  const warnings = [];
  for (const file of files) {
    const providerId = file.slice(0, -".json".length);
    const path = join(options.outDir, file);
    let raw;
    try {
      raw = JSON.parse(readFileSync(path, "utf8"));
    } catch (error) {
      console.error(`gen_catalog: ${file}: invalid JSON: ${error.message}`);
      drifted.push(providerId);
      continue;
    }
    const result = canonicalizeCatalogFile(providerId, raw);
    warnings.push(...result.warnings);
    const current = normalizeNewlines(readFileSync(path, "utf8"));
    if (current !== result.json) drifted.push(providerId);
  }

  for (const warning of [...new Set(warnings)]) console.error(`warning: ${warning}`);
  if (drifted.length > 0) {
    console.error(
      `gen_catalog: ${drifted.length} checked-in catalog file(s) are not canonical: ${drifted.join(", ")}`,
    );
    console.error(
      "gen_catalog: regenerate with `node scripts/gen_catalog.mjs --data-dir <dir> --all` (the full gate), or restore the canonical snapshot",
    );
    process.exit(1);
  }
  console.log(
    `gen_catalog: ${files.length} checked-in catalog file(s) are canonical (self-check; no upstream data required)`,
  );
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

const options = parseArgs(process.argv.slice(2));

// Git's `core.autocrlf` may materialize checked-in text as CRLF on Windows.
// The gate compares content, not checkout line endings, so normalize CRLF
// before comparing; freshly generated files are always written with LF.
function normalizeNewlines(text) {
  return text.replace(/\r\n/g, "\n");
}

if (options.selfCheck) {
  selfCheck(options);
  process.exit(0);
}

const dataDir = resolveDataDir(options.dataDir);
const overrides = loadOverrides();
const available = discoverProviders(dataDir);

let selected;
if (options.only.length > 0) {
  const unknown = options.only.filter((id) => !available.includes(id));
  if (unknown.length > 0) {
    fail(`no upstream snapshot for: ${unknown.join(", ")} (available: ${available.join(", ")})`);
  }
  selected = [...new Set(options.only)].sort();
} else {
  selected = available;
}
if (selected.length === 0) fail("no providers selected");

const warnings = [];
const results = [];
for (const providerId of selected) {
  results.push([providerId, generateProvider(providerId, dataDir, overrides, warnings)]);
}

for (const warning of [...new Set(warnings)]) console.error(`warning: ${warning}`);

if (options.check) {
  const changed = [];
  for (const [providerId, result] of results) {
    const target = join(options.outDir, `${providerId}.json`);
    const current = existsSync(target) ? readFileSync(target, "utf8") : undefined;
    if (current === undefined || normalizeNewlines(current) !== result.json) {
      changed.push(providerId);
    }
  }
  if (changed.length > 0) {
    console.error(
      `gen_catalog: ${changed.length} catalog file(s) out of date: ${changed.join(", ")}`,
    );
    console.error("gen_catalog: run `node scripts/gen_catalog.mjs --data-dir <dir> --all`");
    process.exit(1);
  }
  console.log(`gen_catalog: ${results.length} catalog file(s) up to date`);
} else {
  let updated = 0;
  for (const [providerId, result] of results) {
    const target = join(options.outDir, `${providerId}.json`);
    const current = existsSync(target) ? readFileSync(target, "utf8") : undefined;
    if (current !== undefined && normalizeNewlines(current) === result.json) continue;
    writeFileSync(target, result.json, "utf8");
    updated += 1;
    console.log(`gen_catalog: updated ${providerId}.json`);
  }
  console.log(
    `gen_catalog: ${updated} updated, ${results.length - updated} unchanged (${dataDir})`,
  );
}
