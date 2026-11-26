# pir-builtin-tool-repair

Pure repair pipeline for assistant tool calls: grammar recovery, live-schema repair and alias normalization.

This crate holds **no engine and no host**: it is the logic half of the
builtin, split out so it can be tested on its own and reused by a host that is
not the pir coding agent. The host half (tool registration, JS bindings,
session wiring) lives in `pir-core`; this crate never depends on it, nor on
`pir-sdk`.

## What is here

| Item | Kind |
|---|---|
| grammar recovery | pure, host-free |
| live-schema repair | pure, host-free |
| alias normalization | pure, host-free |

Pure means these functions are deterministic and side-effect free: given the
same input they return the same output, and nothing here opens a socket, reads
the process environment or touches a session.

## Using it

The engine consumes this crate through
[`pir-builtins`](https://crates.io/crates/pir-builtins), the aggregate
catalog that owns builtin order and default-enable flags. A host that wants
only this builtin can depend on it directly:

```toml
[dependencies]
pir-builtin-tool-repair = "0.3"
```

## License

MIT OR Apache-2.0.
