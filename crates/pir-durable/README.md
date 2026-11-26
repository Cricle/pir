# pir-durable

Durable conversation, task and document runtime for
[`pir`](https://crates.io/crates/pir-sdk) — a native Rust port of
`@earendil-works/pi-durable`.

This crate is what makes a long-running agent survivable: entries append
durably, tasks carry a state machine, documents are chunked and searchable,
and every write survives the process that made it. It depends on
[`pir-ai-types`](https://crates.io/crates/pir-ai-types) for the value shapes
but **not** on `pir-core`, so the durability layer can be used (and tested)
without the agent loop.

## What is here

| module | responsibility |
|---|---|
| `entries` | the append-only conversation entry log |
| `session` | session persistence and replay |
| `tasks` | the task state machine and its transitions |
| `documents` | document storage, chunking and search |
| `storage` | the storage backends underneath the above |
| `search` | query evaluation over stored documents |
| `pi_format` | the on-disk `pi` format: import, export, round-trip |
| `delta` / `truncate` / `ids` / `timers` | streaming deltas, size caps, identifiers, timers |
| `context` / `types` / `errors` | the shared context, value types and error taxonomy |
| `chord` | the chord-specific durable surface |
| `harness` | test scaffolding for the above |

## Using it

`pir-core` re-exports this crate at its historical path:

```rust
use pir::durable::entries;
```

## License

MIT OR Apache-2.0.