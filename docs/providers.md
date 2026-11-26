# Providers and authentication

How pir resolves credentials and which providers ship. Model *definitions*
(models.json, thinking levels, cost) are in [`models.md`](models.md); the
built-in `anthropic` Claude Code environment is documented there too.

## Credential precedence

For a request, the first available source wins:

1. A stored credential in `~/.pi/agent/auth.json` — stored OAuth before a
   stored API key.
2. The provider's environment variables (below).
3. An explicit `apiKey` in an `models.json` provider entry
   ([`models.md`](models.md)), or `--api-key`.

Session-scoped dynamic/OAuth overlays isolate parent/child provider resolution;
native auth hooks stay process-wide, last-wins.

`pir auth status` prints both faces: `OAuth providers` with their sign-in
state, then `API-key providers` with each env var's set/unset state and whether
a stored credential exists.

## Commands

```bash
pir auth status            # OAuth + API-key state per provider
pir auth login <provider>  # run the provider's OAuth flow (browser/device code)
pir auth logout <provider> # clear the stored credential
pir auth check             # resolve credentials without sending a request
pir auth print-api-key     # print the resolved key for the current model
```

## OAuth providers

`pir auth login <provider>` works for the nine providers with a registered flow
(`crates/pir-core/src/ai/auth/resolve.rs`, `OAUTH_PROVIDERS`):

```text
anthropic, github-copilot, kimi-coding, meta, openai, openai-codex,
openrouter, radius, xai
```

Tokens land in `~/.pi/agent/auth.json` with their expiry; `pir` refreshes them
on use.

## API-key providers

Env vars per provider (`crates/pir-core/src/ai/auth/mod.rs`, `env_api_keys`).
`anthropic` resolves `ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_OAUTH_TOKEN` to an
`Authorization: Bearer` header and `ANTHROPIC_API_KEY` to `x-api-key`; the
precedence is documented in [`models.md`](models.md#authentication-header-precedence).

| Provider | Environment variables |
|---|---|
| `amazon-bedrock` | `AWS_BEARER_TOKEN_BEDROCK`, `AWS_ACCESS_KEY_ID` |
| `anthropic` | `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_OAUTH_TOKEN`, `ANTHROPIC_API_KEY` |
| `ant-ling` | `ANT_LING_API_KEY` |
| `azure` | `AZURE_OPENAI_API_KEY` |
| `baseten` | `BASETEN_API_KEY` |
| `cerebras` | `CEREBRAS_API_KEY` |
| `cloudflare-ai-gateway` | `CLOUDFLARE_API_KEY` |
| `cloudflare-workers-ai` | `CLOUDFLARE_API_KEY` |
| `deepseek` | `DEEPSEEK_API_KEY` |
| `fireworks` | `FIREWORKS_API_KEY` |
| `github-copilot` | `COPILOT_GITHUB_TOKEN` |
| `google` | `GEMINI_API_KEY`, `GOOGLE_API_KEY` |
| `google-vertex` | `GOOGLE_CLOUD_API_KEY` |
| `groq` | `GROQ_API_KEY` |
| `huggingface` | `HF_TOKEN` |
| `kimi-coding` | `KIMI_API_KEY` |
| `meta` | `META_API_KEY` |
| `minimax` | `MINIMAX_API_KEY` |
| `minimax-cn` | `MINIMAX_CN_API_KEY` |
| `mistral` | `MISTRAL_API_KEY` |
| `moonshotai` | `MOONSHOT_API_KEY` |
| `moonshotai-cn` | `MOONSHOT_API_KEY` |
| `nvidia` | `NVIDIA_API_KEY` |
| `openai` | `OPENAI_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` |
| `opencode` / `opencode-go` | `OPENCODE_API_KEY` |
| `qwen-token-plan` / `qwen-token-plan-individual` | `QWEN_TOKEN_PLAN_API_KEY` |
| `qwen-token-plan-cn` / `qwen-token-plan-individual-cn` | `QWEN_TOKEN_PLAN_CN_API_KEY` |
| `radius` | `RADIUS_API_KEY` |
| `together` | `TOGETHER_API_KEY` |
| `typesafe` | `TYPESAFE_API_KEY` |
| `vercel-ai-gateway` | `AI_GATEWAY_API_KEY` |
| `xai` | `XAI_API_KEY` |
| `xiaomi` | `XIAOMI_API_KEY` |
| `xiaomi-token-plan-ams` | `XIAOMI_TOKEN_PLAN_AMS_API_KEY` |
| `xiaomi-token-plan-cn` | `XIAOMI_TOKEN_PLAN_CN_API_KEY` |
| `xiaomi-token-plan-sgp` | `XIAOMI_TOKEN_PLAN_SGP_API_KEY` |
| `zai` | `ZAI_API_KEY` |
| `zai-coding-cn` | `ZAI_CODING_CN_API_KEY` |

This table covers providers with a documented env var; the frozen catalog is
larger. See the catalog below.

## The built-in catalog

pir ships a frozen snapshot of upstream's provider catalog: **43 providers /
1,767 model entries**, embedded from
`crates/pir-core/src/ai/providers/catalog/*.json` and pinned entry-by-entry by
`tests/suites/ai_catalog_full_w10.rs`. It is a snapshot, not a live models.dev
mirror.

`pir models list` and `--list-models [search]` show the resolved registry;
`pir models` prints the configured model. An `models.json` entry overrides or
extends the catalog per provider.

Adapter selection (`api`) and the OpenAI-compatible auto-detect are documented
in [`models.md`](models.md).
