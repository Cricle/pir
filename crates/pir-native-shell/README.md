# pir-native-shell

Native Shell desktop front end for [`pir`](https://crates.io/crates/pir-sdk).

This crate is the **desktop** UI: a native windowing shell over the engine,
gated behind the opt-in `desktop`/GPUI backend because it needs a GPU-backed
windowing stack that most builds do not want. It is `publish = false` — it is
not on crates.io and it is not part of the release publish order, so a plain
`cargo install pir-sdk` never pulls it.

Like [`pir-tui`](https://crates.io/crates/pir-tui), it is a *consumer* of the
engine: it renders what
[`pir-core`](https://crates.io/crates/pir-core) produces, and the engine never
depends on it.

## Building it

The backend is opt-in, so it is not part of the default workspace build:

```bash
cargo build -p pir-native-shell --features desktop
```

## License

MIT OR Apache-2.0.