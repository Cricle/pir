//! The built-in conversation documents and the drafts that stage them.
//!
//! Every conversation owns exactly five built-in documents (spec §8, upstream
//! `packages/durable/src/harness/harness.ts:410-417`):
//!
//! | kind | scope | history | fork | initial |
//! |---|---|---|---|---|
//! | `pi.agent` | conversation | `rewindable` | `asOf` | `{}` ([`AgentState::default`]) |
//! | `pi.live` | conversation | `latest` | `initial` | `{}` |
//! | `pi.inbox` | conversation | `latest` | `initial` | `{ items: [] }` |
//! | `pi.provider` | conversation | `latest` | `initial` | `{}` |
//! | `pi.usage` | conversation | `latest` | `initial` | `{ models: {}, tools: {} }` |
//!
//! The five kinds and their semantics are **public protocol**: a stored
//! conversation is read by other implementations, so the kind, the version, the
//! scope, the history and the fork rule are fixed. All five are version 1; a
//! future change to a stored shape bumps that document's version and adds a
//! `migrate`, never rewrites version 1 in place.
//!
//! # `pi.agent` and forks
//!
//! `pi.agent` is rewindable with `fork: asOf`, so a fork starts from the agent
//! as of its fork entry. A *task-owned* conversation (one created by a task for
//! a subagent) instead starts from a copy of its owner conversation's
//! `pi.agent`; [`agent_seed`] is that copy (upstream `createAgent`,
//! `harness/agent.ts:174-180`).
//!
//! # `pi.provider` and session IDs
//!
//! Upstream's `pi.provider` `initial()` mints a fresh `uuidv7` per incarnation.
//! Here the definition never generates an id: a definition's `initial` is a
//! plain function pointer that cannot capture the conversation, and minting an
//! id at definition-construction time would share one id across every
//! conversation. The provider session id is therefore minted by the generation
//! task, which writes it into the document before its first request
//! (`ensureProviderSessionId` upstream); [`provider_initial`] is `{}` and
//! [`ProviderState::session_id`] stays absent until then. A fork still gets a
//! fresh identity because `fork: initial` starts from `{}`.

use serde::{Deserialize, Serialize};

use crate::durable::documents::{AnyDocDefinition, CommonDocDefinition};
use crate::durable::harness::agent::AgentState;
use crate::durable::harness::inbox::INBOX_DEFINITION;
use crate::durable::harness::inbox::INBOX_KIND;
use crate::durable::harness::live::LIVE_DEFINITION;
use crate::durable::harness::live::LIVE_KIND;
use crate::durable::harness::usage::USAGE_DEFINITION;
use crate::durable::harness::usage::USAGE_KIND;
use crate::durable::ids::ConversationId;
use crate::durable::types::{DocumentFork, JsonObject};
use serde_json::Value;

/// Persisted kind of the conversation's agent document (spec §8).
pub const AGENT_KIND: &str = "pi.agent";
/// Persisted version of the agent document's value shape.
pub const AGENT_VERSION: i64 = 1;

/// Persisted kind of the conversation's provider-identity document (spec §8).
pub const PROVIDER_KIND: &str = "pi.provider";
/// Persisted version of the provider document's value shape.
pub const PROVIDER_VERSION: i64 = 1;

/// The `pi.provider` conversation document: one provider-facing identity per
/// conversation.
///
/// Only `sessionId` is stored. Upstream types it as a required string; it is
/// optional here because the id is minted by the generation task rather than by
/// the definition's `initial` (see the module docs), so an incarnation is `{}`
/// until that task writes it.
///
/// Both fields are absent by default, so `ProviderState::default()` serializes
/// to `{}` exactly as the document's initial value.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderState {
    /// Opaque provider session id forwarded for prompt-cache / affinity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Initial value of a new `pi.agent` incarnation: `AgentState::default()` as
/// JSON, which is `{}` because every stored field is optional.
fn agent_initial() -> JsonObject {
    match serde_json::to_value(AgentState::default()) {
        Ok(Value::Object(object)) => object,
        _ => JsonObject::new(),
    }
}

/// `pi.agent`'s checkpoint predicate: a complete base on every change
/// (upstream `AgentDoc.checkpointWhen: () => true`).
fn agent_checkpoint_when(_value: &JsonObject) -> bool {
    true
}

/// Initial value of a new `pi.provider` incarnation: `{}`.
///
/// The session id is not generated here; see the module docs and
/// [`provider_definition`].
fn provider_initial() -> JsonObject {
    match serde_json::to_value(ProviderState::default()) {
        Ok(Value::Object(object)) => object,
        _ => JsonObject::new(),
    }
}

/// `pi.provider`'s checkpoint predicate: a complete base on every change
/// (upstream `ProviderDoc.checkpointWhen: () => true`).
fn provider_checkpoint_when(_value: &JsonObject) -> bool {
    true
}

/// The `pi.agent` conversation document.
///
/// Kind `pi.agent`, version 1, conversation scope, `history: "rewindable"`,
/// `fork: "asOf"`, initial `{}` (an [`AgentState`] with every field unset), and
/// a complete base on every change. Rewindable with `asOf` means a fork starts
/// from the agent as of its fork entry.
///
/// Built on each call rather than cached because the definition carries
/// function pointers and building it validates the version, which is not a
/// `const` operation; it is cheap and callers only build it to stage a
/// document.
#[must_use]
pub fn agent_definition() -> AnyDocDefinition {
    CommonDocDefinition::conversation_rewindable(AGENT_KIND, AGENT_VERSION, DocumentFork::AsOf)
        .expect("the pi.agent definition version is valid")
        .behaviour(agent_initial, None, Some(agent_checkpoint_when))
}

/// The `pi.provider` conversation document: one distinct provider session id
/// per conversation.
///
/// Kind `pi.provider`, version 1, conversation scope, `history: "latest"`,
/// `fork: "initial"`, initial `{}`, and a complete base on every change.
///
/// This definition **never generates an id itself.** The id is minted by the
/// generation task and written to the document before the first request, so a
/// fork (`fork: "initial"`) starts empty and receives its own id rather than
/// copying its parent's. See [`ProviderState`] and the module docs.
#[must_use]
pub fn provider_definition() -> AnyDocDefinition {
    CommonDocDefinition::conversation_latest(PROVIDER_KIND, PROVIDER_VERSION, DocumentFork::Initial)
        .expect("the pi.provider definition version is valid")
        .behaviour(provider_initial, None, Some(provider_checkpoint_when))
}

/// Every built-in conversation document, in creation order.
///
/// `pi.agent`, `pi.live`, `pi.inbox`, `pi.provider`, `pi.usage`. The order is
/// the one a caller stages them in; the documents are independent, so no
/// correctness property depends on it (upstream `conversationCreated` stages
/// live/inbox/usage/provider and then the agent).
///
/// Each definition is the same one its own module exposes ([`LIVE_DEFINITION`],
/// [`INBOX_DEFINITION`], [`USAGE_DEFINITION`]), cloned; the kind is returned
/// beside it so a caller can address the staged document without parsing the
/// definition.
#[must_use]
pub fn conversation_definitions() -> Vec<(&'static str, AnyDocDefinition)> {
    vec![
        (AGENT_KIND, agent_definition()),
        (LIVE_KIND, LIVE_DEFINITION.clone()),
        (INBOX_KIND, INBOX_DEFINITION.clone()),
        (PROVIDER_KIND, provider_definition()),
        (USAGE_KIND, USAGE_DEFINITION.clone()),
    ]
}

/// Stage every built-in document for a newly created conversation.
///
/// Returns the drafts the caller stages with `Transaction::stage_document`:
/// each document's kind and its `initial` value, in creation order. This
/// function does not touch a transaction, so it stays testable, and it mints
/// nothing: `pi.provider`'s session id is the generation task's job (see
/// [`provider_definition`]).
///
/// `conversation` names the owner the caller stages at; the values themselves
/// are conversation-independent, which is why it is not read here.
#[must_use]
pub fn builtin_conversation_documents(_conversation: ConversationId) -> Vec<(String, JsonObject)> {
    conversation_definitions()
        .into_iter()
        .map(|(kind, definition)| (kind.to_owned(), definition.initial_value()))
        .collect()
}

/// The `pi.agent` value a task-owned conversation starts from.
///
/// A subagent's conversation copies its owner conversation's `pi.agent` so it
/// inherits the model, thinking level, extensions, tool filters, instructions
/// and working directory. Without an owner (a root or independent conversation)
/// the seed is the definition's initial value, `{}` — every field follows the
/// host.
///
/// The map is cloned, so the caller may mutate the returned draft without
/// touching the owner's stored value.
#[must_use]
pub fn agent_seed(owner_agent: Option<&JsonObject>) -> JsonObject {
    match owner_agent {
        Some(owner) => owner.clone(),
        None => agent_initial(),
    }
}
