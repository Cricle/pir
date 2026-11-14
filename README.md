# Pi in Rust (pir)

Pi coding agent reimplemented in pure Rust — a full Rust equivalent of `pi`,
following upstream's development and ecosystem, with no Node.js/V8/WASM runtime
dependency.

## Status

0.3.1-rc.1 (2026-11-14). End-to-end functional: agent loop, providers, sessions,
extensions, TUI, RPC, server/client, SDK, builtins. Final result:
[`RESULTS.md`](RESULTS.md); per-module truth: [MAPPING.md](MAPPING.md); feature
contract: [`docs/FEATURES.md`](docs/FEATURES.md). Settled scope: chord delta
subset only; llama.cpp N/A; library-first.

Install from crates.io with `cargo install pir-sdk` (the package is `pir-sdk`;
the installed binary is `pir`). CI tags also attach `pir-<tag>-windows-x86_64.zip`
(MSVC), `pir-<tag>-linux-x86_64-musl.tar.gz` and
`pir-<tag>-macos-universal.tar.gz`, each containing `pir` and
`pir-phone-server`; local Windows verification in `RESULTS.md` is GNU-only.

The local release gate is green on 2026-11-14 (`v0.3.1-rc.1`):
`cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features
--locked -- -D warnings`, `cargo check --no-default-features --features sdk`,
doctests (33 passed / 0 failed / 7 ignored) and the full cargo-nextest suite
(7500 tests, 0 failed, 46 skipped across the lib/bins target and the 8 merged
harnesses plus the standalone and profiling targets) all exit 0. Evidence:
`.play/evidence/release-candidate-032/` (`summary.txt`, `gates.log`).

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
pir-sdk = { version = "0.3.1-rc.1", default-features = false, features = ["sdk"] }
```

Every optional surface is a cargo feature; `full` is the default, and the
binary requires only `cli` (which implies `print`/`rpc`/`export-html` but not
`tools`). Authoritative list and off-error contract:
[`docs/FEATURES.md`](docs/FEATURES.md).

## Running

```bash
pir                    # interactive TUI agent (default)
pir print "<prompt>"   # non-interactive single execution
pir print --mode json "<prompt>"
pir rpc                # JSON-lines RPC over stdin/stdout
pir server / pir client
pir auth / pir models list / pir session / pir jobs / pir config / pir version
pir grep|sed|python <args>
```

`pir grep|sed|python` forward argv, fall back to the system tool, and keep GNU
exit codes; `--no-fallback` / `PIR_NATIVE_FALLBACK=0` disables that.

### Remote server security

`pir server` requires `PIR_SERVER_AUTH_TOKEN`. `--allow-unauthenticated` is a
local-development escape hatch only: without a token any local process can
create sessions, use the server's model credentials, and read/write files
under its working directory. Loopback binding does **not** isolate other local
users; never use the flag on a shared host. Tokens are compared in constant
time and never logged.

### Rust SDK

[`pir::sdk`](docs/sdk.md) builds an agent from an endpoint, the environment, or
your own `ProviderRegistry` + `Provider`; extends it with typed/closure tools,
inline plugins, `on_event` handlers or builtins; and runs `prompt`
(`AgentOutcome`) or `prompt_stream` (bounded `SdkEvent`). Quickstart:
[docs/sdk.md#quickstart](docs/sdk.md#quickstart); runnable examples:
[`examples/README.md`](examples/README.md).

### Sessions

```bash
pir -c / -r                 # continue / resume picker
pir --session <id|path>     # open a session
pir --session-id <uuid>     # exact id
pir --fork <id|path>        # fork
pir --no-session            # ephemeral
pir --no-context-files      # skip AGENTS.md/CLAUDE.md
pir -e ./my-ext.ts          # load an extension
```

Sessions are JSONL under `<agent-dir>/sessions/<encoded-cwd>/`, byte-compatible
with pi; `PIR_SESSION_DIR` overrides. Interactive mode loads
`AGENTS.override.md`/`AGENTS.md`/`CLAUDE.md` and supports `/model`,
`/thinking`, `/compact`, `/sessions`, `/help` plus extension commands.

### Background jobs

`pir jobs` runs detached commands that survive the agent exiting and are
tracked across processes (pir-native; upstream `pi` has no `jobs` command):

```bash
pir jobs start -- <command...>   # detached run, prints the job id
pir jobs list                    # newest first (--json)
pir jobs status <id>             # persisted status (--json)
pir jobs wait <id> [--timeout S] # wait for a terminal state (default 600s)
pir jobs log <id> [--lines N]    # output tail (defaults 4096 lines / 262144 bytes)
pir jobs reap <id>               # status + output, mark reaped (--json)
pir jobs kill <id>               # kill the supervisor process tree
pir jobs clean [--older-than-days D | --all]  # prune job dirs (default 7 days, --json)
```

Each job is `<agent-dir>/jobs/<job-id>/` with `meta.json` and an `output.log`
capped at 256 MiB, written by a detached supervisor. `start` takes
`--cwd <dir>` and `--shell <path>` (a `-c` shell is required) and must receive
the command after `--`.

### Providers

`pir auth status|login|logout|check|print-api-key`. Ships a frozen catalog
(**42 providers / 1,756 model entries**, pinned by
`tests/suites/ai_catalog_full_w10.rs`) plus OAuth flows; credentials live in
`~/.pi/agent/auth.json` (stored OAuth > stored API key > environment).
`~/.pi/agent/models.json` overrides the catalog. Details:
[docs/models.md](docs/models.md).

### Extensions and builtins

Builtins compile behind their features (`subagent`, `tool-repair`,
`tool-search`, `todo`, `mcp-sd`); `--only-builtin` loads Rust-only,
`--no-extensions` wins if both are given. `pir --help-extensions` lists the CLI
flags extensions registered with `registerFlag` (it loads them; plain
`--help` never executes extension code). JS limits live in
`extensionLimits` + `PI_EXT_*` (project settings cannot loosen them). The
default-off JS `phone` package is not required by the native path. The native
phone plugin is an agent client for the standalone central relay:
`cargo build -p pir-phone-server`, then
`/phone-start wss://<server> <device-token>`. The relay never executes the
agent; it stores only user/device/session metadata and the latest status line,
and routes messages without retaining transcripts or images. Details:
[`docs/phone-server.md`](docs/phone-server.md).

## Development

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
powershell -NoProfile -File scripts/test_fast.ps1 -UseNextest   # full suite, nextest runner
# `autotests = false` merges tests/*.rs into the generated zz_merged_N harnesses;
# a merged binary shares ONE process across its tests, so the whole suite must run
# under nextest (process per test): `cargo nextest run --profile local`.
bash scripts/run_examples.sh        # offline SDK examples
bash scripts/run_benchmarks.sh      # manual benchmarks (dev profile)
```

Integration gates cover CLI/RPC framing, remote-session policy/limits,
persistence and forbidden upstream artifacts. Feature matrix:
`bash scripts/feature_matrix.sh`.

## Security boundaries

The remote-server token (`PIR_SERVER_AUTH_TOKEN`) is required and provides
pre-handshake authentication, **not TLS**: keep `PIR_SERVER_ADDRESS` on
loopback/private networks or behind an authenticated TLS proxy/VPN, and note
that loopback alone does not isolate other local users.
`--allow-unauthenticated` is local-development only. Remote bash is off by
default; body limits are enforced; `PIR_ALLOW_REMOTE_AGENT=false` disables
remote agent execution. Credentials: `~/.pi/agent/auth.json` — see
[MAPPING.md](MAPPING.md).

## Compatibility

From-scratch reimplementation. Public JSON uses camelCase and matches upstream
field names, missing/null semantics and status codes. Protocol version 8.

## License

MIT OR Apache-2.0
