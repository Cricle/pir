# pi-phone-server

Central information proxy for pi-phone sessions — the relay between a
pi-phone client and the sessions it talks to.

This crate is the **server** half: it holds the session registry, routes
requests to the owning session, and serves the relay protocol. It is
`publish = false` (package name `pir-phone-server`), so it is not on crates.io
and not in the release publish order; it is built and tested as part of the
workspace.

The phone *builtin* in the agent — config resolution, URL normalization,
quota derivation and relay envelopes — is the separate
[`pir-builtin-phone`](https://crates.io/crates/pir-builtin-phone) crate. That
one is host-neutral pure logic; this one owns the port, the tasks and the
state.

## What is here

| module | responsibility |
|---|---|
| `ws` / `http` | the client transports: the WebSocket relay and the HTTP surface |
| `protocol` | the relay envelope and its framing |
| `registry` / `state` | the session registry and the server's shared state |
| `store` | the persisted session records |
| `auth` | client authentication |
| `config` | server configuration |
| `metrics` | counters for the served sessions |

## License

MIT OR Apache-2.0.