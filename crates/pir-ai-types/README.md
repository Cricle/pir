# pir-ai-types

Provider-neutral AI value types for [`pir`](https://crates.io/crates/pir-sdk):
messages, models, usage, stream options and the assistant event stream — the
Rust port of upstream `packages/ai/src/types.ts`.

Nothing here talks to a provider or holds a socket. That is the point: both
the engine and the durability layer need to name a message and an event, and
putting those names in one leaf crate lets `pir-durable` depend on them
without depending on `pir-core`.

## What is here

Public modules:

| module | responsibility |
|---|---|
| `error` | `AiError`, `AiResult`, `is_retryable`, `TurnRetryClass`, provider HTTP errors |
| `event_stream` | the assistant message event stream a provider yields |
| `text` | terminal-text helpers shared by the engine and the UI crates |
| `util` | retry classification and context-overflow helpers |
| `session` | the session-facing value shapes |
| `seam` | provider seams the engine calls into (transport, hooks, chunk sinks) |
| `partial_mirror` | the partially-streamed mirror of an assistant message |

The value types themselves (`Message`, `Model`, usage, stream options,
credentials, images) are re-exported from the crate root rather than from a
submodule, so `pir_ai_types::Message` is the canonical path.

## Using it

`pir-core` re-exports this crate under the historical paths, so existing code
keeps compiling unchanged:

```rust
use pir::ai::types::{Message, Model};
use pir::ai::error::AiError;
use pir::ai::event_stream::AssistantMessageEventStream;
```

## License

MIT OR Apache-2.0.