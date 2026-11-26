# pir-plugin-api

Engine-free plugin protocol for [`pir`](https://crates.io/crates/pir-sdk):
builtin descriptors, plugin events, and tool results.

This is the contract between the engine and a plugin implementation, and it is
deliberately the smallest crate in the workspace — one file, no engine. A
plugin author depends on this and nothing else; the builtins depend on it so
that the engine consumes one implementation instead of carrying duplicates.

## What is here

- **builtin descriptors** — how a builtin declares itself (id, kind, enable
  defaults) without linking the engine;
- **plugin events** — the shapes a plugin receives and emits;
- **tool results** — the result envelope a plugin tool returns.

## Using it

Plugins implement against this crate directly:

```toml
[dependencies]
pir-plugin-api = "0.3"
```

Embedders normally see it through the facade, where it appears at the
historical path as `pir::plugins::...`.

## License

MIT OR Apache-2.0.