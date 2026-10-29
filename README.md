# Pi in Rust (pir)

Pi coding agent reimplemented in pure Rust — a full Rust equivalent of `pi`,
following upstream's development and ecosystem, with no Node.js/V8/WASM runtime
dependency.

## Status

End-to-end functional: agent loop, providers, sessions, extensions, TUI, RPC,
server/client, SDK, builtins. Current truth: [`docs/STATUS.md`](docs/STATUS.md);
per-module truth: [MAPPING.md](MAPPING.md); feature contract:
[`docs/FEATURES.md`](docs/FEATURES.md). Settled scope: chord delta subset only;
llama.cpp N/A; library-first.

## Building

```bash
cargo build          # debug build (default)
```

## SDK and feature flags

The crate is a library first. `pir::sdk` (`features = ["sdk"]`) embeds the
agent loop without the CLI; the bare core (`default-features = false`) keeps
providers, sessions, config, prompt/skills and the plugin framework.

```toml
# Package name `pir-sdk`; the library is still imported as `pir`.
pir-sdk = { version = "0.0.1", default-features = false, features = ["sdk"] }
```

Every optional surface is a cargo feature; `full` is the default, and the
binary requires only `cli` (which implies `print`/`rpc`/`export-html` but not
`tools`). The authoritative feature list, off-error contract and profiles are
in [`docs/FEATURES.md`](docs/FEATURES.md); the feature set lives in
`Cargo.toml` `[features]`.

### Extensions, builtins and limits

- Builtin plugins compile behind their features: `subagent`, `tool-repair`,
  `tool-search`, `todo`, and `pir mcp-sd`. `--only-builtin` loads only the Rust
  builtins (no JS/packages/settings scan); `--no-extensions` wins if both are
  given; `-e builtin:<name>` still works.
- `@malinamnam/pi-phone` and the native `phone` builtin are default-off (the JS
  package starts a local server, ~14 MB QuickJS heap); opt in via global
  settings or `-e`. JS limits live in `extensionLimits` + `PI_EXT_*`; project
  settings cannot loosen them. Details: [`docs/extensions.md`](docs/extensions.md).
- Shell pool prefers an installed Git Bash (or `/bin/bash`); without one the
  `brush` feature's `pir __brush` is the zero-install fallback, and `shellPath`
  still wins. `tools.commandTimeoutMs` (default 120000; `0` disables) bounds
  blocking calls; `background: true` is exempt.
- Degenerate loops stop after 20 identical tool calls; print/RPC/server and
  the SDK `AgentBuilder` default to 1,000 turns (`PIR_MAX_TURNS` for the CLI
  hosts; raw `CreateAgentSessionOptions` stays at 100), the TUI at 100; a
  changed result resets the streak. Provider event queues are bounded with
  producer backpressure.

## Running

```bash
pir                    # interactive TUI agent (default)
pir print "<prompt>"   # non-interactive single execution
pir print --mode json "<prompt>"
pir rpc                # JSON-lines RPC over stdin/stdout
pir server / pir client
pir auth / pir models list / pir session / pir config / pir version
pir grep|sed|python <args>
```

`pir grep|sed|python` forward argv, fall back to the system tool, and keep GNU
exit codes; `--no-fallback` / `PIR_NATIVE_FALLBACK=0` disables that. Inside the
agent, `bash`/`powershell` export `PIR_BIN` and prepend its directory to `PATH`.

### Rust SDK

[`pir::sdk`](docs/sdk.md) builds an agent from an endpoint, the environment, or
your own `ProviderRegistry` + `Provider`; extends it with typed/closure tools,
inline plugins, `on_event` handlers or builtins; and runs `prompt`
(`AgentOutcome`) or `prompt_stream` (bounded `SdkEvent`). Quickstart:
[docs/sdk.md#quickstart](docs/sdk.md#quickstart); runnable examples:
[`examples/quickstart.rs`](examples/quickstart.rs) and
[`examples/README.md`](examples/README.md).

### Sessions and context

```bash
pir -c / -r                 # continue / resume picker
pir --session <id|path>     # open a session
pir --session-id <uuid>     # exact id
pir --fork <id|path>        # fork
pir --no-session            # ephemeral
pir --no-context-files      # skip AGENTS.md/CLAUDE.md
pir --theme <name>
pir -e ./my-ext.ts          # load an extension
pir --only-builtin / --no-extensions
```

Sessions are JSONL under `<agent-dir>/sessions/<encoded-cwd>/`, byte-compatible
with pi; `PIR_SESSION_DIR` overrides. Interactive mode loads
`AGENTS.override.md`/`AGENTS.md`/`CLAUDE.md`, supports `/model`, `/thinking`,
`/debug`, `/compact`, `/image`, `/clear`, `/sessions`, `/help` and extension
commands; the editor has fuzzy path/slash completion and configurable
keybindings.

### Providers and authentication

```bash
pir auth status|login|logout|check|print-api-key
```

pir ships a frozen catalog (**42 providers / 1,756 model entries**, pinned by
`tests/suites/ai_catalog_full_w10.rs`) plus OAuth flows
(anthropic, openai-codex, openai-chatgpt, github-copilot, openrouter,
kimi-coding, xai, radius, meta). Credentials: `~/.pi/agent/auth.json`; order =
stored OAuth > stored API key > environment. Custom
`~/.pi/agent/models.json` overrides the catalog. Adapters cover
openai-completions/responses/codex/azure, anthropic-messages,
google-generative-ai, google-vertex, bedrock-converse-stream and
mistral-conversations; an unknown api id errors clearly.

#### Claude Code environment compatibility

```bash
export ANTHROPIC_BASE_URL=https://gateway.example
export ANTHROPIC_AUTH_TOKEN=...        # Authorization: Bearer
export ANTHROPIC_MODEL=gpt-5
export CLAUDE_CODE_MAX_CONTEXT_TOKENS=1000000
export CLAUDE_CODE_MAX_OUTPUT_TOKENS=65536
pir print --model anthropic/gpt-5 "hi"
```

`ANTHROPIC_AUTH_TOKEN`/`ANTHROPIC_OAUTH_TOKEN` use Bearer; `ANTHROPIC_API_KEY`
uses `x-api-key`; an explicit models.json key/header wins. The env is read per
process and never written. See
[docs/models.md](docs/models.md#claude-code-environment-compatibility).

## Development

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

The benchmark exercises CBOR framing, fragmented decode, scheduler, session
persistence and SSE-style event streams without external providers.
Integration gates cover CLI/RPC framing, remote-session policy/limits,
persistence and forbidden upstream artifacts. Feature matrix:
`bash scripts/feature_matrix.sh`.

## Configuration

Environment variables: see `.env.example`. MCP servers live in `mcp.json`;
`builtinTools` (default `true`) makes servers without explicit `exposure`
default to `direct`; `false` restores upstream's `codemode` default.

## Security boundaries

The optional remote-server token is pre-handshake authentication, **not TLS**.
Keep `PIR_SERVER_ADDRESS` on loopback/private networks or front it with an
authenticated TLS proxy/VPN. Remote bash is off by default; body limits are
enforced; `PIR_ALLOW_REMOTE_AGENT=false` disables remote agent execution.
Credentials live in `~/.pi/agent/auth.json` — see [MAPPING.md](MAPPING.md).

## Compatibility

From-scratch reimplementation. Public JSON uses camelCase and matches upstream
field names, missing/null semantics and status codes. Protocol version 8.

## License

MIT OR Apache-2.0
