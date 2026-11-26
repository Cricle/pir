# pir-core

The UI-agnostic engine behind [`pir-sdk`](https://crates.io/crates/pir-sdk):
agent loop, provider adapters, sessions, tools, config, plugins and the
extension runtime.

`pir-core` has no terminal, clipboard or window dependency. A front end (the
terminal one is [`pir-tui`](https://crates.io/crates/pir-tui)) is a separate
crate that depends on this one; the engine never depends on a UI.

## Embedding

Most embedders depend on the `pir-sdk` facade instead of this crate directly,
because it keeps the historical `pir::...` paths and re-exports the engine:

```toml
[dependencies]
pir-sdk = { version = "0.3", default-features = false, features = ["sdk"] }
```

Depending on `pir-core` directly is the right choice when you are writing a new
front end rather than embedding the agent:

```toml
[dependencies]
pir-core = { version = "0.3", default-features = false }
```

## What is here, and what is a separate crate

- `crate::ai`, `crate::agent`, `crate::agent_session`, `crate::coding_agent`,
  `crate::session_manager`, `crate::extensions`, `crate::plugins`,
  `crate::config` — the engine itself.
- `crate::protocol` → re-export of `pir-protocol` (strict CBOR, framing,
  schema validation; no engine types).
- `crate::telemetry` → re-export of `pir-telemetry` (spans, metrics, W3C
  propagation, trace store).
- `crate::durable` → re-export of `pir-durable`, behind the default-on
  `durable` feature (JSONL/memory/SQLite runtime, session kernel, harness).
- `crate::ai::{types,error,event_stream,util}`, `crate::text`,
  `crate::session_manager::types`, `crate::ai::partial_mirror` → re-exports of
  `pir-ai-types` (provider-neutral value types and the transport seam).
- `crate::plugins::builtin::*` pure halves → re-exports of the
  `pir-builtin-*` crates, which depend only on `pir-plugin-api`.

The re-exports exist so the crate split is invisible to callers: every
historical `crate::…` path still resolves.

## Features

`default = ["full"]` builds every optional surface. A slim embed turns them off
(`--no-default-features`, then add back what it needs); `minimal` is the
zero-optional-feature profile that is still a runnable agent. `Cargo.toml`
`[features]` is the authoritative list.

## Documentation

- [Architecture](https://github.com/Cricle/pir/blob/master/docs/architecture.md) — the workspace split and the dependency direction.
- [Features](https://github.com/Cricle/pir/blob/master/docs/FEATURES.md) — every cargo feature and the off-error contract.
- [SDK guide](https://github.com/Cricle/pir/blob/master/docs/sdk.md) — embedding through the facade.

## License

MIT OR Apache-2.0.