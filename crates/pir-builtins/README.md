# pir-builtins

The aggregate catalog of [`pir`](https://crates.io/crates/pir-sdk)'s builtin
plugins: registration order, default-enable flags, and the host-feature
mapping.

This crate owns one table and no implementations. Each builtin's pure logic
lives in its own `pir-builtin-*` crate; this one lists them in the historical
order (`subagent, mode, mcp, codemode, context-mode, tool-search,
tool-repair, todo, memory, phone`) and records which are enabled by default —
`phone` is off, because it binds a local port and holds a server task.

It depends only on [`pir-plugin-api`](https://crates.io/crates/pir-plugin-api)
and the implementation crates, never on `pir-core` or `pir-sdk`, so the engine
consumes the catalog without any dependency edge back into it.

## Two projections of one table

- `KNOWN_BUILTINS` is feature-independent: every builtin this workspace knows
  about, with its order, default-enable flag and the host cargo feature that
  compiles it in. A host reads it to explain a *missing* builtin ("not
  compiled into this build: the `mcp` feature is off"), which by definition
  needs rows this build does not have.
- `descriptors` is the same table projected onto the builtins this build
  actually compiled.

## License

MIT OR Apache-2.0.