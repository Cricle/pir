# pir-protocol

Provider-agnostic wire protocol for [`pir`](https://crates.io/crates/pir-sdk):
a strict CBOR subset, length-prefixed framing, and schema validation.

This crate owns the bytes on the wire and nothing else. It knows no provider,
no model and no agent — which is why it has no dependency on `pir-core`, and
`pir-core` can carry it without pulling the engine into anything that only
needs to decode a frame.

## What is here

| module | responsibility |
|---|---|
| `codec` | encode/decode the protocol envelope |
| `cbor` | the strict CBOR subset (rejects duplicate map keys, non-string keys, tags, over-deep nesting) |
| `framing` | 4-byte length prefix read/write, including the frame-size limit |
| `schemas` | client/server message shapes and their validation |

Strictness is deliberate: a decoder that silently accepts duplicate map keys
or tags turns a malformed peer into a confused one.

## Using it

Most embedders reach the protocol through the facade, which re-exports this
crate at its historical path:

```toml
[dependencies]
pir-sdk = { version = "0.3", default-features = false }
```

```rust
use pir::protocol::{framing, schemas};
```

A crate that only needs framing can depend on `pir-protocol` directly and
skip the engine entirely:

```toml
[dependencies]
pir-protocol = "0.3"
```

## License

MIT OR Apache-2.0.