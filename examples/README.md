# pir SDK examples

All examples are small, runnable programs built against the public
`pir::sdk` facade. Except for the two network examples, they are **offline and
deterministic**: no API key, no local model, no `~/.pi` state. They use a
scripted `Provider` from [`common/mod.rs`](common/mod.rs), which is also the
smallest useful template for a custom provider.

## Run them

The SDK / default-on surfaces are separate cargo features. Each example
declares only the features its own command needs (`required-features` in
`Cargo.toml`): `sdk` is required everywhere, `sessions` adds `session`, the
builtin/extension examples add their own runtime feature, and `real_endpoint`
declares the common `sdk,session,skills` set. TUI, CLI, Python, and MCP stay
off; only the `extension` example turns on the embedded QuickJS runtime:

```bash
# Offline: no network, no credentials.
cargo run --example quickstart --no-default-features --features "sdk"
cargo run --example streaming --no-default-features --features "sdk"
cargo run --example plugin --no-default-features --features "sdk"
cargo run --example control --no-default-features --features "sdk"
# The only example that writes/attaches transcript files.
cargo run --example sessions --no-default-features --features "sdk,session"

# `tools` compiles in the built-in coding tools. The example follows the
# build (`cfg!(feature = "tools")`): with the feature off it registers only
# its two custom tools; `core_tools(true)` in a tool-less build fails
# `build()` with an explicit missing-feature diagnostic (never a silent
# no-op).
cargo run --example tools --no-default-features --features "sdk,tools"

# Compiled-in builtin (`subagent`).
cargo run --example builtin --no-default-features --features "sdk,subagent"

# JS extension in the embedded QuickJS runtime.
cargo run --example extension --no-default-features --features "sdk,extensions"

# Application proxy (network): the provider ships with the example.
cargo run --example proxy --no-default-features --features "sdk"

# Real provider (network): the example compiles in its declared
# `sdk,session,skills` shape; add the wire adapter you need to RUN it.
cargo run --example real_endpoint --no-default-features \
  --features "sdk,session,skills,provider-openai"
```

`--no-default-features` drops `full` (TUI, CLI, extensions, Python, MCP, phone,
and the extra tools); each command above adds back only what it needs.

The whole offline set, in these exact feature shapes, is also available as one
script:

```bash
scripts/run_examples.sh
```

It exits nonzero on the first failure. The two network examples are
compile-checked only (no request is sent).

## What each example shows

| Example | Demonstrates | Features | Network |
|---|---|---|---|
| [`quickstart.rs`](quickstart.rs) | `with_registry`, a custom `Provider`, one prompt, `AgentOutcome`, `shutdown` | `sdk` | no |
| [`streaming.rs`](streaming.rs) | `prompt_stream_with_outcome`, every `SdkEvent` variant (an explicit `event_name` checklist), text deltas, tool start/update/end, terminal outcome | `sdk` | no |
| [`tools.rs`](tools.rs) | `core_tools(cfg!(feature = "tools"))`, typed `with_tool`, closure `tool`, `ToolContext` (`cwd` + `is_aborted()`), one tool batch, `tool_result_texts`, `Agent::tools` | `sdk`; add `tools` for the built-in coding tools | no |
| [`sessions.rs`](sessions.rs) | `SessionMode::Dir`, `session_path`/`session_id`, `set_session_name`, `Agent::on_session_event` subscription, `new_session`, `attach_session` | `sdk,session` | no |
| [`plugin.rs`](plugin.rs) | inline `AgentPlugin`, `register_tool`, `on_event("input")` transform, merged tool registry | `sdk` | no |
| [`control.rs`](control.rs) | `steer`, `follow_up`, `pending_message_count`, `abort`, one in-memory transcript across runs | `sdk` | no |
| [`builtin.rs`](builtin.rs) | `.builtin("subagent")`, compiled-in builtin registration, `Agent::tools()` | `sdk,subagent` | no |
| [`extension.rs`](extension.rs) | `extension_paths`, a JS `registerTool` extension running in QuickJS | `sdk,extensions` | no |
| [`proxy.rs`](proxy.rs) | `ProxyProvider` / `ProxyEndpoint`, requests through an application proxy | `sdk` | yes |
| [`real_endpoint.rs`](real_endpoint.rs) | `Endpoint`, `ApiKind`, `api_key`, a real HTTP provider | `sdk,session,skills`; add `provider-openai` (or `provider-anthropic` / `provider-google`) to RUN | yes |

[`common/mod.rs`](common/mod.rs) is shared code, not an example target. It
contains the complete `Model`, the scripted `Provider` (with an optional
request log), the gated `AbortableProvider` used by the control example,
message builders, `user_texts`, and the `EchoTool` that emits a progress
update.

## The SDK ideas these examples cover

1. **Model setup is explicit.** `with_registry(registry, model)` uses your
   provider directly and needs no `provider-*` feature. `endpoint(...)` selects
   a built-in wire adapter and needs the matching feature. `model(...)` resolves
   from `models.json` + the catalog + auth.
2. **Tools are opt-in.** `AgentBuilder::new()` has no built-in tools. Call
   `core_tools(true)` for `read`/`write`/`edit`/`bash`/`powershell`/`ls` (and
   the background controls; the `tools` feature must be compiled in), and
   register your own with `tool(...)` (closure) or `with_tool(...)` (typed).
3. **Events and outcome are separate views.** `prompt()` returns
   `AgentOutcome`; `prompt_stream()` returns typed `SdkEvent`s. Use
   `prompt_stream_with_outcome()` when you want both. `on_session_event()`
   subscribes to the out-of-loop `SessionEvent`s (useful with `prompt()`,
   which has no stream); `SdkEvent::from_session_event` projects them onto
   the typed event type.
4. **Sessions are explicit.** `no_session()` keeps everything in memory;
   `SessionMode::Dir(path)` writes the shared pi/pir JSONL format;
   `attach_session(path)` resumes an existing transcript.
5. **Runs are controllable.** `steer` and `follow_up` queue messages,
   `pending_message_count()` reports what is still queued, and `abort()` stops
   an in-flight run (`AgentSettled { aborted: true }`). Typed tools receive a
   `ToolContext`: use `cwd` for relative I/O and check `is_aborted()`
   periodically so a long tool exits when the host aborts.
6. **One plugin host.** Closure tools, typed tools, inline `AgentPlugin`s,
   `on_event` handlers, compiled-in builtins, and JS extensions all register
   through the same host.

## Embedding in your own crate

The examples live inside this package, so they can use `pir` directly. An
external embedder needs the SDK plus the default-on surfaces it uses:

```toml
[dependencies]
# Package name `pir-sdk`; the library is still imported as `pir`.
pir-sdk = { version = "0.0.1", default-features = false, features = [
    "sdk",
    # Add only what you use:
    # "session", "skills", "tools", "subagent", "extensions",
    # "provider-openai", "provider-anthropic", "grep-find", "image-tools",
] }

futures-util = "0.3"
serde_json = "1"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time"] }
```

Start from `quickstart.rs`, then copy `common/mod.rs` and replace
`ScriptedProvider::stream` with your backend. For a real endpoint, copy
`real_endpoint.rs` and add the matching `provider-*` feature. For skills and
prompt templates, build a `DefaultResourceLoader` and attach it with
`AgentBuilder::resource_loader(loader)` (requires the `skills` feature for
actual discovery).

Release embeds should use `opt-level = "s"` (the SDK is provider-I/O-bound and
this saves roughly 28% in the measured consumer); see
[`docs/sdk.md#packaging-and-size`](../docs/sdk.md#packaging-and-size) for the
profile and numbers.

### Feature notes

- `sdk` is all the facade itself needs; `session`, `skills` (and the extra
  tools/builtins) are opt-in. `session` and `skills` degrade to documented
  no-ops when off: the public signatures remain, and the matching
  functionality reports "disabled" rather than failing to compile. `sdk` is
  the exception: `pir::sdk` is not compiled at all without it (there is no
  stub), so an embedder that uses the facade must enable `sdk`.
- `provider-openai`, `provider-anthropic`, `provider-google`,
  `provider-bedrock`, `provider-mistral` gate the built-in wire adapters.
  Without one, `with_registry` still works; `endpoint()` fails at `build()`
  with the missing-feature error.
- `tools` adds the built-in coding tools (`read`/`write`/`edit`/`bash`/
  `powershell`/`ls` and the background controls). `grep-find` adds the
  `grep`/`find` tools; `image-tools` adds inline images.
- `subagent`, `tool-repair`, `tool-search`, `todo`, `python`, `mcp`, and
  `extensions` gate compiled-in builtins / JS extensions. Calling
  `.builtin("name")` without its feature fails `build()` with an explicit
  diagnostic; `extension_paths` with an on-disk file fails the same way
  without `extensions`.
- `cli`, `tui`, `env`, `server`, `phone`, and `mcp-sd` are host surfaces an
  embedder does not need.
