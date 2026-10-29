# Adding models

pir resolves models through (in order): the `--provider/--model` CLI flags,
the `--models` patterns, the settings `defaultProvider`/`defaultModel`, and
the built-in catalog (`src/ai/providers/catalog/*.json`, 42 providers).

## models.json

Place a `models.json` in the agent dir (`~/.pi/agent/models.json`, or
`PIR_AGENT_DIR`):

```json
{
  "providers": {
    "my-gateway": {
      "name": "My Gateway",
      "baseUrl": "https://gw.example.com/v1",
      "api": "openai-completions",
      "apiKey": "sk-...",
      "models": [
        {
          "id": "my-model",
          "name": "My Model",
          "reasoning": true,
          "input": ["text", "image"],
          "contextWindow": 200000,
          "maxTokens": 8192,
          "cost": { "input": 3.0, "output": 15.0 }
        }
      ]
    }
  }
}
```

`$ENV` and `!command` templates are allowed for `apiKey` (upstream parity).
`api` selects the protocol adapter: `openai-completions`, `openai-responses`,
`anthropic-messages`, `google-generative-ai`, `google-vertex`,
`mistral-conversations`, `bedrock-converse-stream`, `azure-openai-responses`,
`openai-codex-responses`, `pi-messages` (Radius), or `openai-compat`
(auto-detect).

## Claude Code environment compatibility

The Claude Code Anthropic-protocol environment works as-is, with no
`models.json`:

```bash
ANTHROPIC_BASE_URL=https://gateway.example
ANTHROPIC_AUTH_TOKEN=<token>
ANTHROPIC_MODEL=gpt-5
ANTHROPIC_DEFAULT_OPUS_MODEL=gpt-5
ANTHROPIC_DEFAULT_SONNET_MODEL=gpt-5
ANTHROPIC_DEFAULT_HAIKU_MODEL=gpt-5
CLAUDE_CODE_MAX_CONTEXT_TOKENS=1000000
CLAUDE_CODE_MAX_OUTPUT_TOKENS=65536
```

The values are read from the process environment only and are never written
to disk. They configure the built-in `anthropic` provider, so
`--model anthropic/<id>`, `/model`, `pir print`, and `--list-models` all see
the synthesized entries.

### Authentication header precedence

For requests through the `anthropic-messages` adapter:

1. An explicit models.json `apiKey` (also `--api-key` / a stored credential)
   is sent as `x-api-key`. An explicit `authorization` or `x-api-key` in
   models.json `headers` always wins (applied last).
2. `ANTHROPIC_AUTH_TOKEN`, then `ANTHROPIC_OAUTH_TOKEN`, is sent as
   `Authorization: Bearer <token>`.
3. `ANTHROPIC_API_KEY` is sent as `x-api-key`.

With no explicit key and no token, the request fails before it is sent with a
clear authentication error instead of reaching the gateway unauthenticated.
OAuth access tokens (tokens containing `sk-ant-oat`) keep Claude Code's
stealth request shape.

### Bootstrap precedence

Environment values fill gaps; explicit configuration wins:

- **Base URL**: an explicit models.json provider `baseUrl` wins. Otherwise
  `ANTHROPIC_BASE_URL` replaces the anthropic models' base URL; an explicit
  per-model `baseUrl` still wins per model.
- **Models**: ids from `ANTHROPIC_MODEL` and the three
  `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL` aliases are added to the
  anthropic provider only when that id is not already defined by models.json.
  A models.json definition is never duplicated or overwritten (name, context,
  limits, headers, and base URL all stay explicit).
- **Synthesized entry**: id and name are the env model id; `reasoning: true`;
  input `["text", "image"]`; `contextWindow` from
  `CLAUDE_CODE_MAX_CONTEXT_TOKENS` (default `200000`); `maxTokens` from
  `CLAUDE_CODE_MAX_OUTPUT_TOKENS` (default `32000`).
- **Default model**: `ANTHROPIC_MODEL` (or the first alias id) becomes the
  default only when neither settings.json nor models.json declares a
  `defaultProvider`/`defaultModel`. No model id is invented when
  `ANTHROPIC_MODEL` and the aliases are unset; `ANTHROPIC_BASE_URL` alone
  keeps the catalog's default model.

## Thinking levels

`thinkingLevelMap` maps a requested level to what the model supports:

```json
"thinkingLevelMap": { "minimal": null, "low": "low", "medium": "low", "high": "high" }
```

A `null` value disables that level for the model (shift+tab clamps to the
map — the footer shows the clamped request, the worker reports the truth).

## Model-level metadata

- `input: ["text", "image"]` — vision support (the `read` tool inline-image
  path downgrades for non-vision models).
- `inputLimits.images.resize` — inline-image resize profile
  (width/height/quality bytes).
- `cost` — per-Mtok rates used by `/session` cost rendering and usage
  rebilling.

`pir models` lists the resolved registry; `pir models <search>` filters.
