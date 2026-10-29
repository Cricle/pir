# Built-in provider catalog: refreshing the snapshot

`src/ai/providers/catalog/*.json` is pir's frozen copy of upstream pi-ai's
built-in model catalog. Upstream does **not** commit those files: they are
generated at build time by `packages/ai/scripts/generate-models.ts` from
models.dev, the OpenRouter API and gateway pricing, and are gitignored at
upstream HEAD. pir therefore ships a snapshot and needs a deterministic way to
refresh it.

`scripts/gen_catalog.mjs` converts an upstream `providers/data` directory into
pir's catalog shape. It never contacts the network.

## Find the upstream data directory

The generator auto-detects, in order:

1. `--data-dir <path>`
2. `$PI_AI_DATA_DIR`
3. `../pi/packages/ai/src/providers/data`
4. the installed npm package:
   `%APPDATA%/npm/node_modules/@earendil-works/pi-coding-agent/node_modules/@earendil-works/pi-ai/dist/providers/data`
   (or `$npm_config_prefix/node_modules/@earendil-works/pi-ai/dist/providers/data`)

The snapshot this repository currently carries comes from
`@earendil-works/pi-ai@1.3.0` (data manifest generated 2026-10-15). Pass
`--data-dir` explicitly when refreshing from a newer checkout or package.

## Commands

```sh
# Regenerate every provider that has an upstream snapshot (the default).
node scripts/gen_catalog.mjs --data-dir <dir> --all

# Regenerate one (or several) providers only.
node scripts/gen_catalog.mjs --data-dir <dir> --only anthropic --only google

# CI gate: regenerate in memory and fail if any checked-in file would change.
node scripts/gen_catalog.mjs --data-dir <dir> --check
```

`--check` exits non-zero and lists every stale file; it never writes. It exists
because the catalog is generated, not hand-edited: without the gate, a
hand-edited JSON file silently drifts from the upstream generated data and the
next refresh overwrites the change. Keeping `--check` green makes the snapshot
reproducible and makes any manual change explicit in
`scripts/catalog-overrides.json`.

## Determinism

The output is byte-stable for a given upstream data directory:

- provider files are discovered in sorted order;
- model keys are sorted (`id` for chat, `type:id` for image/classifier);
- fields are emitted in a fixed order and maps are key-sorted;
- compact JSON (no insignificant whitespace) with LF line endings and exactly
  one trailing newline.

Do not hand-edit `src/ai/providers/catalog/*.json`. Change the generator or the
overrides file, then re-run it.

## `scripts/catalog-overrides.json`

Everything upstream cannot express in pir's catalog schema lives here:

```jsonc
{
  "providers": {
    "anthropic": { "name": "Anthropic" },          // display name
    "typesafe": {
      "name": "TypeSafe",
      "baseUrl": "https://api.typesafe.ai/v1"      // provider-level override
    }
  },
  "models": {
    "provider-id": {
      "model-key": {
        "baseUrl": "https://...",                  // per-model override; null = drop
        "compat": { "supportsReasoningEffort": false } // merged over derived compat
      }
    }
  },
  "extraModels": {
    "provider-id": {
      "model-key": { /* full pir catalog model entry */ }
    }
  }
}
```

- **`providers[].name`** — display names are provider metadata, not part of the
  generated model data, so all 42 are pinned here.
- **`providers[].baseUrl` / `models[].baseUrl`** — the generator derives the
  provider base URL as the most common per-model upstream `baseUrl` and emits a
  per-model `baseUrl` whenever a model differs (Bedrock regions, OpenCode's
  Anthropic root vs OpenAI `/v1`, Fireworks, Cloudflare gateways, ...).
  Overrides pin values upstream cannot carry (TypeSafe's no-trailing-slash
  root) or intentionally differ; `null` suppresses the field.
- **`models[].compat`** — merged over the derived compat block. This preserves
  manual protocol flags that are absent from (or deliberately differ from) the
  upstream generated data: Azure/DeepSeek `thinkingFormat`/`maxTokensField`,
  Moonshot `supportsUsageInStreaming`, OpenRouter session affinity,
  Together streaming flags, NVIDIA streaming, Z.ai GLM-4.7 effort. A `null`
  value removes a derived key.
- **`extraModels`** — complete entries retained from earlier snapshots when a
  newer upstream generator no longer emits them (for example
  `opencode-go/minimax-m2.7`, which W10-B refreshed and the current upstream
  package no longer lists). This is the no-silent-data-loss rule: refresh adds
  upstream models but never drops a model that pir shipped before. Delete an
  entry here deliberately when a model is retired.

## Field mapping

pir's `CatalogModel` schema is smaller than upstream's `Model`:

| Upstream | pir catalog |
| --- | --- |
| `id`, `name`, `api`, `reasoning`, `input`, `cost`, `contextWindow`, `maxTokens`, `thinkingLevelMap` | same field |
| `type` (`chat`/`image`/`classifier`) | `type`, omitted for chat |
| `baseUrl` | per-model `baseUrl` when it differs from the provider root |
| `compat` | `compat` (only keys `OpenAiCompat` understands) |
| `inputLimits`, `promptCache` | folded into `compat` (`inputLimits`, `promptCache`) |
| `output` (image models) | `output` |
| `provider` | implied by the file |
| `headers`, `lab`, `enabled`, `providers` | dropped — pir's catalog schema has no field for them |

Unknown upstream `compat` keys are reported as warnings and dropped rather
than written into a file where serde would silently ignore them.
