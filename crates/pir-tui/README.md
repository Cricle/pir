# pir-tui

The terminal front end for the [`pir`](https://crates.io/crates/pir-sdk) coding
agent: a crossterm event loop with a hand-rolled diff painter (no `ratatui`),
a multiline editor, streaming markdown/diff renderers, themes, clipboard and
dialogs.

This crate is a *consumer* of the engine: everything it renders comes from
[`pir-core`](https://crates.io/crates/pir-core), and `pir-core` never depends
on this crate or on any other UI.

## Using it

Most users get the TUI through the facade, which re-exports this crate as
`pir::tui` behind the `tui` feature:

```toml
[dependencies]
pir-sdk = { version = "0.3", features = ["tui"] }
```

```rust
use pir::tui::TuiOverrides; // only with the `tui` feature
```

Depending on `pir-tui` directly is the right choice when you are extending the
terminal front end itself, not embedding the agent.

## What it owns

The crossterm loop and rendering (`crates/pir-tui/src/terminal.rs`, `app.rs`,
`renderers.rs`), the editor (`editor.rs`), markdown/diff streaming (`markdown/`,
`mermaid.rs`), autocomplete (`autocomplete.rs`, `fuzzy.rs`), keybindings,
OKLCH/OKHSL themes (`theme/`), the OS clipboard, and the terminal-only halves of
the SDK parity surface (`sdk_parity`, `bug_report`, `session_pick`).

The facade folds `session_pick` / `bug_report` back into
`pir::coding_agent::*`, so those historical paths keep working.

## Features

`tui`-side feature names mirror the facade's and are forwarded to the engine
where they belong (`session`, `image-protocol`, …). `Cargo.toml` `[features]` is
the authoritative list.

## Documentation

- [TUI](https://github.com/Cricle/pir/blob/master/docs/tui.md) — the hard contracts (input order, cancel, streaming deltas, resize, cursor, diff painting).
- [Architecture](https://github.com/Cricle/pir/blob/master/docs/architecture.md) — how a second front end would be added.

## License

MIT OR Apache-2.0.