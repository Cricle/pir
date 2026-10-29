//! The provider-facing identity of one conversation.
//!
//! Port of upstream `packages/durable/src/harness/provider.ts`. The persisted
//! shape itself (`pi.provider`, version 1, conversation scope, `latest`
//! history, `initial` fork) lives in [`documents`](super::documents), because
//! fork copies and the built-in-document table needed it before this module
//! existed; this module adds the two behaviours upstream's file owns:
//!
//! * [`mint_session_id`] — a UUIDv7-shaped id built from the conversation's
//!   storage id and the harness clock. pir has no `uuid` crate and must not add
//!   one (`Cargo.toml` is frozen for this lane), so the 74 random bits are
//!   derived from a splitmix64 mix of `(conversation id, now_ms)`. That is
//!   deliberately *pseudorandom, not cryptographic*: the id is an affinity /
//!   prompt-cache key forwarded to a provider, never a secret.
//! * [`ensure_provider_session_id`] — upstream `ensureProviderSessionId`:
//!   return the committed identity without writing on the normal path; a legacy
//!   conversation with an empty `pi.provider` gets one migration commit that
//!   runs the document's `initial()` before the provider request starts.
//!
//! # Why the id is not minted by the definition
//!
//! Upstream's `ProviderDoc.initial()` calls `uuidv7()` per incarnation. A pir
//! [`DocInitial`](crate::durable::documents::DocInitial) is a plain function
//! pointer that cannot capture the conversation id, and minting at
//! definition-construction time would share one id across every conversation.
//! The id is therefore minted here, on first use, which keeps `fork: initial`
//! honest (a fork starts from `{}`, so it has no identity to copy) and lets a
//! legacy conversation acquire one lazily.
//!
//! # Fork semantics
//!
//! A fork must never reuse its parent's provider session: the provider keys
//! server-side cache state on it, and two transcripts with one identity would
//! cross-contaminate. `fork: "initial"` guarantees the child document starts
//! empty, so the first [`ensure_provider_session_id`] on the child mints its
//! own id.

use crate::durable::context::Context;
use crate::durable::documents::{AnyDocDefinition, DocOwner, DocumentSemantics, ResolvedAddress};
use crate::durable::errors::StorageError;
use crate::durable::harness::documents::{provider_definition, PROVIDER_KIND};
use crate::durable::ids::ConversationId;
use crate::durable::session::Session;
use crate::durable::types::DocumentFork;

pub use crate::durable::harness::documents::ProviderState;

/// The `pi.provider` document definition (upstream `ProviderDoc`).
///
/// Alias of [`provider_definition`] under the upstream name, so a caller
/// porting upstream code can write `ProviderDoc`-equivalent lookups. The
/// definition is rebuilt per call: it carries function pointers and building it
/// validates the version, neither of which is a `const` operation.
#[must_use]
pub fn provider_doc() -> AnyDocDefinition {
    provider_definition()
}

/// The logical address of one conversation's `pi.provider` document.
#[must_use]
pub fn provider_address(conversation: ConversationId) -> ResolvedAddress {
    ResolvedAddress::new(
        PROVIDER_KIND,
        DocumentSemantics::ConversationLatest {
            fork: DocumentFork::Initial,
        },
        Some(DocOwner::Conversation(conversation)),
        None,
    )
    .expect("pi.provider is a conversation document")
}

/// Mint one provider session id for `conversation` at `now_ms`.
///
/// The result has the UUIDv7 text shape (`8-4-4-4-12`, version nibble `7`,
/// variant nibble `8..b`) with the 48-bit millisecond timestamp in the high
/// bits, so ids sort the way upstream's `uuidv7()` results do. The random bits
/// come from a splitmix64 mix of the conversation id and the clock — unique
/// per `(conversation, millisecond)` and stable for the same pair, which is all
/// this identity needs and all a dependency-free implementation can promise.
#[must_use]
pub fn mint_session_id(conversation: ConversationId, now_ms: i64) -> String {
    let timestamp = (now_ms as u64) & 0x0000_FFFF_FFFF_FFFF;
    let seed = (conversation.as_i64() as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(now_ms as u64);
    let high = splitmix64(seed);
    let low = splitmix64(high ^ 0xD1B5_4A32_D192_ED03);

    // 12 random bits after the version nibble.
    let rand_a = (high >> 52) as u16 & 0x0FFF;
    // The variant nibble is `10xx`; the remaining 60 bits follow it.
    let variant = 0b1000u8 | ((low >> 61) as u8 & 0b0011);

    format!(
        "{:08x}-{:04x}-7{:03x}-{:x}{:03x}-{:012x}",
        (timestamp >> 16) as u32,
        (timestamp & 0xFFFF) as u16,
        rand_a,
        variant,
        (low >> 48) as u16 & 0x0FFF,
        low & 0xFFFF_FFFF_FFFF,
    )
}

/// splitmix64: the standard 64-bit finalizer, used only to spread the id mix.
fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = value;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Return the persisted provider identity, minting and committing one when the
/// conversation has none yet.
///
/// The normal path is a single committed read (`Session::snapshot`): a
/// conversation created by this port already has its `pi.provider` document
/// (empty until first use). Only a legacy conversation — or a fork whose
/// initial document was never read — takes the second path, one
/// `Session::doc` commit whose creation runs the definition's `initial()`
/// before the provider request starts, exactly upstream's migration commit.
///
/// `now_ms` is the harness clock (epoch milliseconds). It is a parameter rather
/// than read from a global so two processes opening the same storage on the
/// same clock agree, and so tests can pin it.
pub async fn ensure_provider_session_id(
    session: &Session,
    conversation: ConversationId,
    now_ms: i64,
    context: Context,
) -> Result<String, StorageError> {
    let resolved = provider_address(conversation);
    if let Some(state) = session.snapshot(&resolved, &context).await? {
        let decoded: ProviderState =
            serde_json::from_value(serde_json::Value::Object(state)).unwrap_or_default();
        if let Some(session_id) = decoded.session_id {
            return Ok(session_id);
        }
    }
    let minted = mint_session_id(conversation, now_ms);
    let written = minted.clone();
    session
        .doc(resolved, provider_doc(), context, move |value| {
            value.insert("sessionId".to_owned(), serde_json::Value::String(written));
        })
        .await?;
    Ok(minted)
}
