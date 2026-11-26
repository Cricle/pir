# Workspace architecture

pir is a small Cargo workspace. The engine, the terminal front end, and the
public facade are separate crates so another UI can be added without touching
the engine — while embedders keep depending on exactly one crate.

## Crates

| Crate | Directory | What it owns |
|---|---|---|
| `pir-sdk` (lib `pir`, bins `pir`/`pir-minimal`) | `src/` | The facade re-exporting the engine at the historical `pir::...` paths, the `pir` binary + CLI/pre-flight code, and the `pir-minimal` profile. |
| `pir-core` (lib `pir_core`) | `crates/pir-core/src/` | UI-agnostic engine: agent loop, providers, sessions, tools, config, plugins, extension runtime, protocol. No terminal or clipboard dependency. Re-exports `pir-ai-types`, `pir-telemetry` and (behind the default-on `durable` feature) `pir-durable` at their historical paths. |
| `pir-tui` (lib `pir_tui`) | `crates/pir-tui/src/` | The terminal front end: crossterm event loop, renderers, editor, themes, clipboard. Depends on `pir-core`; the engine never depends on it. |
| `pir-phone-server` | `crates/pi-phone-server/` | Standalone multi-user relay for pi-phone sessions. Does not depend on the agent engine. |
| `pir-plugin-api` | `crates/pir-plugin-api/` | Engine-free builtin/plugin protocol: identity, lifecycle DTOs, catalog descriptors and runtime-neutral result types. It depends on no workspace crate. |
| `pir-protocol` | `crates/pir-protocol/` | The wire protocol: the strict CBOR subset codec, the four-byte length-prefixed framing and the schema validators for every client/server envelope. Pure `serde` + `serde_json` + `ciborium` + `thiserror`; no engine types and no workspace dependencies. `pir-core` re-exports it as `pub use pir_protocol as protocol;`, so `crate::protocol::…` and `pir::protocol::…` are unchanged. Optional `server` feature (forwarded by `pir-core`'s same-named feature) for the typed-encoder helpers the RPC dispatch uses. |
| `pir-telemetry` | `crates/pir-telemetry/` | Vendor-neutral observability core: spans, attributes, counters/histograms, W3C propagation, and the portable trace store. Pure `std` + `serde_json` + `uuid`; no engine types. `pir-core` re-exports it as `pir::telemetry` (and `crate::telemetry` internally). Optional `store` (default) and `testing` (test-only) features. |
| `pir-ai-types` | `crates/pir-ai-types/` | Provider-neutral AI value types (`Message`, `Model`, `Usage`, `StreamOptions`, the assistant event stream), the AI error, the provider HTTP transport *seam*, the text truncation helpers, the on-disk session record types and summary constants, and the streaming tool-argument mirror. `pir-core` re-exports each at its historical `crate::ai::{types,error,transport,util,event_stream}`, `crate::text`, and `crate::session_manager::types` path (`projection` stayed in the engine: it is coupled to `SessionManager`). |
| `pir-durable` | `crates/pir-durable/` | Durable conversation/task/document runtime (native port of `@earendil-works/pi-durable`): JSONL + memory + SQLite storage, the session kernel, the harness/scheduler and the pi-format bridge. Depends only on `pir-ai-types`/`pir-telemetry` — never on the engine — and is re-exported at `crate::durable` by the default-on `durable` feature. Own `sqlite` feature (default). |
| `pir-builtins` | `crates/pir-builtins/` | The aggregate catalog: builtin registration order, default-enable flags and the host-feature mapping, projected from one `KNOWN_BUILTINS` table. It does not depend on `pir-core`. |
| `pir-builtin-*` | `crates/pir-builtin-*/` | Independent builtin implementation packages. They depend on `pir-plugin-api` and third-party crates only; they do not depend on `pir`, `pir-sdk`, `pir-core` or `pir-tui`. |
| `pir-native-shell` | `crates/pir-native-shell/` | Optional Native Shell/NSH front end. Its headless reducer/bridge depends on `pir-core`; `desktop` adds the GPUI window host; `mobile` exposes target-gated iOS UIKit/Android JNI adapter boundaries that reuse the same JSON state/command contract. |

Dependency direction is one-way:

```text
pir-sdk ──▶ pir-core ──adapts──▶ pir-plugin-api
   │            │                     ▲
   │            ├──▶ pir-telemetry ───┤
   │            │        ▲            │
   │            ├──▶ pir-ai-types ────┤   (text, session types, partial mirror)
   │            │        ▲            │
   │            ├──▶ pir-durable ─────┤   (the whole durable runtime)
   │            ├──▶ pir-protocol     │
   │            └──▶ pir-builtins ──▶ pir-builtin-*
   └──▶ pir-tui ──▶ pir-core
```

The `pir-telemetry → pir-ai-types → pir-durable` chain is strictly one-way
(each builds only on the ones above it); `pir-protocol` is a leaf with no
workspace dependency. `pir-core` re-exports all four at their historical
paths, so no consumer sees the split.

No extracted crate depends on the engine: the direction is always
`extracted ──▶ third party (+ pir-plugin-api)`, never `extracted ──▶ pir-core`.
Only `pir-tui` and `pir-native-shell` point at `pir-core`, and they are front
ends rather than extracted halves of it. `tests/version_consistency.rs`'s
`extracted_crates_never_depend_on_the_engine` enforces this, so a new
extraction cannot quietly take a back edge that would recreate the monolith
and invert the publish order.

`pir-core` re-exports `pir-telemetry` at the historical `crate::telemetry`
path (`pub use pir_telemetry as telemetry;`), so no internal call site and no
`pir::telemetry` consumer changed when the code moved out. The crate's optional
`store` (default) and `testing` (test-only) features decide whether the trace
store and the in-memory recorder compile.

`pir-core` depends on the `pir-builtin-*` crates for their PURE logic (the
repair pipeline, the todo domain, the BM25 ranker, the mode/plan state, the
fetch/chunk helpers, the MCP naming rules, agent discovery and result
formatting). The runtime halves — QuickJS, SQLite/FTS5, rmcp transports,
`AgentSession`, tool registration — stay in `pir-core`. `pir-core` re-exports
each crate's modules at their historical `pir::plugins::...` paths, so no
embedder had to change.

The seam between the two layers is `crates/pir-core/src/plugins/builtin_bridge.rs`
(`BuiltinPluginAdapter`): it presents an engine-free
`pir_plugin_api::BuiltinPlugin` as an engine `AgentPlugin`, translating
identity, the session snapshot and the event outcome. An application layer can
therefore install its own catalog (`install_builtin_catalog`) and the host needs
no knowledge of any concrete builtin type. The remaining host↔builtin needs
(`mcp`'s manager handle, `phone`'s session bridge, `tool-repair`'s config) are
expressed as capability traits in `plugins/capabilities.rs`, reached through
defaulted `AgentPlugin` accessors — never a concrete downcast.

`pir-core` must never name `pir-tui`, `pir-native-shell`, a renderer, or a terminal crate. The few
engine paths with a natural UI half go through `pir_core::host_hooks`.

## Keeping the single-crate import

External code and the repository's integration tests still write
`use pir::...`:

```toml
# Cargo.toml of a consumer
[dependencies]
pir-sdk = "0.3"
```

```rust
use pir::sdk::AgentBuilder;
use pir::tui::TuiOverrides; // only with the `tui` feature
```

The facade (`src/lib.rs`) does `pub use pir_core::*;` and, behind `tui`,
`pub use pir_tui as tui;`. Feature names on `pir-sdk` are unchanged and are
forwarded to the owning crate (`session = ["pir-core/session",
"pir-tui?/session"]`, and so on), so every documented `cargo build
--features ...` command keeps working.

The facade also folds the terminal-only modules back into their historical
paths:

- `pir::coding_agent::session_pick` / `pir::coding_agent::bug_report` — the
  implementations live in `pir-tui`, re-exported by the facade's
  `coding_agent` module.
- `pir::sdk::run_interactive_mode`, `InteractiveModeOptions`, `Keybindings`,
  theme helpers — the engine owns the non-terminal SDK; `pir_tui::sdk_parity`
  supplies the terminal half and the facade re-exports it.

## Adding another UI

A new front end is a new workspace crate. Nothing in the engine changes.

1. Create `crates/pir-web` (or `pir-gui`, `pir-remote`, …):

   ```toml
   [package]
   name = "pir-web"
   version.workspace = true
   edition = "2021"

   [dependencies]
   pir-core = { path = "../pir-core", version = "0.3.2", default-features = false }
   ```

2. Drive the engine through its public surface:
   - `pir_core::sdk` — the curated embedding API (`AgentBuilder`, `Agent`,
     `SdkEvent`), same as any external embedder.
   - `pir_core::agent_session` / `agent` / `ai` — lower-level session and
     event APIs when the UI needs direct control.
   - Read-only host data (`SessionManager`, settings, extension handles) is
     public; UI code does not reach into engine internals through `pub(crate)`
     shims.

3. If the UI should participate in the engine's optional host hooks, install
   them at startup:

   ```rust
   use pir_core::host_hooks::{set_export_palette_provider, set_theme_registrar};

   pub fn install_host_hooks() {
       set_export_palette_provider(my_palette);       // session → HTML export colors
       set_theme_registrar(my_theme_registrar);       // contributed theme files
       set_keypress_waiter(my_keypress_wait);         // CLI migration pause
   }
   ```

   Hooks are optional; every core path has a documented headless fallback.

4. To expose the UI as `pir::web` (or similar) through the single SDK crate,
   add an optional dependency + feature to `pir-sdk` and re-export the crate
   in `src/lib.rs`:

   ```toml
   [features]
   web = ["dep:pir-web"]
   ```

   ```rust
   #[cfg(feature = "web")]
   pub use pir_web as web;
   ```

   That step is only needed for convenience; a UI crate is usable on its own.

The existing `pir-native-shell` crate follows this pattern: `--no-default-features` exposes its platform-neutral `GuiState`/`NativeShellBridge` reducer for tests and embedding; `--features desktop` adds Native Shell/NSH + GPUI windowing. The reducer consumes `pir_core::sdk::SdkEvent` and emits JSON-safe state snapshots, while the desktop adapter owns only the Native Shell window and `nsh/host` message boundary. Its publisher uses a latest-wins `watch` channel so a slow GUI consumer cannot backpressure the agent; terminal settlement is authoritative, and every run carries a monotonically increasing id to reject stale events.

## Host hooks (engine → UI seams)

`pir_core::host_hooks` is the only place the engine names a UI concern:

| Hook | Engine use | Headless fallback |
|---|---|---|
| `ExportPaletteProvider` | session → HTML export theme colors | built-in dark stylesheet colors |
| `ThemeRegistrar` | `register_theme_files` (package/extension themes) | diagnostic: "loaded but not registered" |
| `KeypressWaiter` | migration notice "press any key" | return immediately |

`pir_tui::install_host_hooks()` provides the terminal implementations; the
`pir` binary calls it before dispatch (and `pir_tui::run` calls it again
defensively). `ImageResizeOptions`/`resize_image` moved to the engine behind
`image-resize` (implied by `image-tools`, and enabled by the `tui` feature) so
the TUI's image path no longer needs the engine to know about the TUI.

## Feature ownership

- Engine features live in `crates/pir-core/Cargo.toml` (same names as before,
  minus `tui`/`image-protocol`). Two internal capability features make
  front-end support explicit without pulling a front end into the engine:
  `image-resize` (the `image` codec) and `ui-support` (the edit-preview engine
  and confinement helpers).
- Terminal features live in `crates/pir-tui/Cargo.toml`: `image-protocol`,
  plus `session`/`extensions` forwarding to core.
- `pir-sdk` owns the user-facing feature names and forwards them; no feature
  changes meaning for consumers.

## Test and script paths

The gates that used to walk `src/` know the new roots:

- `tests/unsafe_gate.rs` scans the facade, engine, TUI, phone-server and
  native-shell source roots using full workspace-relative keys.
- `tests/suites/source_hygiene.rs` covers the facade, engine and TUI roots;
  phone-server/native-shell are protected by crate-level `deny(unsafe_code)`.
- Source-reading tests use `crates/pir-core/src/...` / `crates/pir-tui/src/...`
  explicitly.
- Compiled-in assets stay with the engine at `crates/pir-core/assets/`; the
  engine's `include_bytes!`/`include_str!` paths are package-local.

## Verifying the split

```sh
# Engine only (no terminal dependency)
cargo check -p pir-core --no-default-features --features sdk

# Engine + terminal UI
cargo check -p pir-tui

# The single-crate consumer surface (library)
cargo check -p pir-sdk --lib --no-default-features --features "tui,sdk"

# The binary
cargo check -p pir-sdk --bin pir
```
