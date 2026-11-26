# pir-telemetry

Vendor-neutral span, metric, propagation and trace-store core for
[`pir`](https://crates.io/crates/pir-sdk) — the Rust port of the upstream
`pi-telemetry` surface.

The engine records spans and metrics without knowing where they go: this crate
defines the recording API and the in-memory store, and leaves export to
whatever the host wires up. It has no dependency on `pir-core`, so a tool that
only wants to emit spans does not link the agent.

## What is here

| module | responsibility |
|---|---|
| `span` / `metrics` | the recording API a call site uses |
| `store` | `TelemetryStore` / `MemoryTelemetryStore` — where records land |
| `render` | `render_trace`, the human-readable trace dump |
| `propagation` | carrying trace context across an async or process boundary |
| `context` / `attributes` | the ambient context and typed attribute keys |
| `facade` | the pre-bound handle most code uses |
| `testing` | `TestTelemetry`, for asserting on what a call site recorded |

## Using it

`pir-core` re-exports this crate at its historical path, so embedders normally
see it as `pir::telemetry`:

```rust
use pir::telemetry::{Telemetry, Telemetry::noop};
```

Telemetry is always compiled — there is no `telemetry` feature and no
cfg-stubbed variant. The disabled path is `Telemetry::noop()`, so no consumer
signature forks between builds.

## License

MIT OR Apache-2.0.