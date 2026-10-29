//! The built-in conversation documents in `src/durable/harness/documents.rs`.
//!
//! Pins the public protocol table (spec §8, upstream `harness/harness.ts:410-417`
//! and `harness/provider.ts`): the five kinds, their version and
//! scope/history/fork semantics, the fixed creation order, the staging drafts
//! that must neither open a transaction nor mint ids, the task-owned
//! `pi.agent` seed's copy semantics, and the `pi.provider` camelCase wire shape.
//!
//! Tests live in an integration suite because the crate forbids inline tests.

use pir::durable::harness::agent::AgentState;
use pir::durable::harness::documents::{
    agent_definition, agent_seed, builtin_conversation_documents, conversation_definitions,
    provider_definition, ProviderState, AGENT_KIND, AGENT_VERSION, PROVIDER_KIND, PROVIDER_VERSION,
};
use pir::durable::harness::inbox::{INBOX_DEFINITION, INBOX_KIND};
use pir::durable::harness::live::{LIVE_DEFINITION, LIVE_KIND};
use pir::durable::harness::usage::{USAGE_DEFINITION, USAGE_KIND};
use pir::durable::{
    AnyDocDefinition, ConversationId, DocumentFork, DocumentHistory, DocumentSemantics, JsonObject,
};
use serde_json::{json, Value};

// ── helpers ────────────────────────────────────────────────────────────────

fn conversation() -> ConversationId {
    ConversationId::from_raw(42)
}

/// The five fixed identity facts of one conversation document.
fn assert_protocol_identity(
    definition: &AnyDocDefinition,
    kind: &str,
    version: i64,
    scope: &str,
    history: DocumentHistory,
    fork: DocumentFork,
) {
    assert_eq!(definition.common.kind, kind, "kind is public protocol");
    assert_eq!(
        definition.common.version, version,
        "version is public protocol"
    );
    assert_eq!(
        definition.common.semantics.scope_kind(),
        scope,
        "scope is public protocol"
    );
    assert_eq!(
        definition.common.semantics.history(),
        Some(history),
        "history is public protocol"
    );
    assert_eq!(
        definition.common.semantics.fork(),
        Some(fork),
        "fork is public protocol"
    );
}

fn object(value: Value) -> JsonObject {
    value.as_object().expect("JSON object").clone()
}

/// The serialized default provider state, as a `JsonObject`.
fn default_provider_wire() -> JsonObject {
    object(serde_json::to_value(ProviderState::default()).expect("ProviderState serializes"))
}

// ── pi.agent ───────────────────────────────────────────────────────────────

#[test]
fn agent_definition_is_conversation_rewindable_and_forks_as_of() {
    let definition = agent_definition();

    assert_eq!(AGENT_KIND, "pi.agent");
    assert_eq!(AGENT_VERSION, 1);
    assert_protocol_identity(
        &definition,
        AGENT_KIND,
        AGENT_VERSION,
        "conversation",
        DocumentHistory::Rewindable,
        DocumentFork::AsOf,
    );
    assert_eq!(
        definition.common.semantics,
        DocumentSemantics::ConversationRewindable {
            fork: DocumentFork::AsOf
        }
    );

    // Every AgentState field is optional, so a fresh incarnation is `{}`.
    assert_eq!(definition.initial_value(), JsonObject::new());
    assert_eq!(
        definition.initial_value(),
        object(serde_json::to_value(AgentState::default()).expect("AgentState serializes"))
    );
    assert!(!definition.has_migration(), "version 1 has no migration");
    // Upstream AgentDoc.checkpointWhen is always true.
    assert!(definition.forces_checkpoint(&JsonObject::new()));
    assert!(definition.forces_checkpoint(&object(json!({
        "model": {"provider": "anthropic", "modelId": "claude"}
    }))));
}

// ── pi.provider ────────────────────────────────────────────────────────────

#[test]
fn provider_definition_is_conversation_latest_and_forks_initial() {
    let definition = provider_definition();

    assert_eq!(PROVIDER_KIND, "pi.provider");
    assert_eq!(PROVIDER_VERSION, 1);
    assert_protocol_identity(
        &definition,
        PROVIDER_KIND,
        PROVIDER_VERSION,
        "conversation",
        DocumentHistory::Latest,
        DocumentFork::Initial,
    );
    assert_eq!(
        definition.common.semantics,
        DocumentSemantics::ConversationLatest {
            fork: DocumentFork::Initial
        }
    );

    // The definition mints no id: the generation task writes `sessionId`.
    assert_eq!(definition.initial_value(), JsonObject::new());
    assert!(!definition.initial_value().contains_key("sessionId"));
    assert!(!definition.has_migration());
    assert!(definition.forces_checkpoint(&JsonObject::new()));
}

#[test]
fn provider_definition_initial_never_mints_a_session_id_across_calls() {
    // Building the definition (or asking for its initial value) repeatedly must
    // not share or generate an identity; forks (`fork: initial`) rely on this.
    for _ in 0..3 {
        let initial = provider_definition().initial_value();
        assert!(initial.is_empty(), "an incarnation starts as {{}}");
        assert_eq!(initial, default_provider_wire());
    }
}

// ── the five-document table ────────────────────────────────────────────────

#[test]
fn conversation_definitions_are_five_in_fixed_creation_order() {
    let definitions = conversation_definitions();

    assert_eq!(definitions.len(), 5);
    assert_eq!(
        definitions
            .iter()
            .map(|(kind, _)| *kind)
            .collect::<Vec<_>>(),
        vec!["pi.agent", "pi.live", "pi.inbox", "pi.provider", "pi.usage"],
        "creation order is agent, live, inbox, provider, usage"
    );

    // Each pair names the definition it carries, all version 1, all
    // conversation-scoped.
    for (kind, definition) in &definitions {
        assert_eq!(definition.common.kind.as_str(), *kind, "kind {kind}");
        assert_eq!(definition.common.version, 1, "kind {kind}");
        assert_eq!(
            definition.common.semantics.scope_kind(),
            "conversation",
            "kind {kind}"
        );
    }

    assert_eq!(definitions[0].0, AGENT_KIND);
    assert_eq!(definitions[1].0, LIVE_KIND);
    assert_eq!(definitions[2].0, INBOX_KIND);
    assert_eq!(definitions[3].0, PROVIDER_KIND);
    assert_eq!(definitions[4].0, USAGE_KIND);
}

#[test]
fn conversation_definitions_are_the_module_definitions() {
    let definitions = conversation_definitions();

    // pi.agent and pi.provider are built on each call; compare semantics and
    // initial value rather than pointer identity.
    assert_eq!(
        definitions[0].1.common.semantics,
        agent_definition().common.semantics
    );
    assert_eq!(
        definitions[3].1.common.semantics,
        provider_definition().common.semantics
    );
    // The other three are clones of the module statics.
    for (index, expected) in [
        (1usize, &*LIVE_DEFINITION),
        (2, &*INBOX_DEFINITION),
        (4, &*USAGE_DEFINITION),
    ] {
        assert_eq!(definitions[index].1.common, expected.common);
        assert_eq!(
            definitions[index].1.initial_value(),
            expected.initial_value()
        );
    }
}

#[test]
fn only_pi_agent_is_rewindable_and_as_of() {
    // spec §8: pi.agent is the sole rewindable/asOf document; the other four
    // are latest/initial.
    for (kind, definition) in conversation_definitions() {
        let is_agent = kind == AGENT_KIND;
        assert_eq!(
            definition.common.semantics.history(),
            Some(if is_agent {
                DocumentHistory::Rewindable
            } else {
                DocumentHistory::Latest
            }),
            "history of {kind}"
        );
        assert_eq!(
            definition.common.semantics.fork(),
            Some(if is_agent {
                DocumentFork::AsOf
            } else {
                DocumentFork::Initial
            }),
            "fork of {kind}"
        );
        assert!(
            !definition.has_migration(),
            "version 1 needs no migration: {kind}"
        );
    }
}

// ── staging drafts ─────────────────────────────────────────────────────────

#[test]
fn builtin_drafts_are_the_definitions_initial_values_in_order() {
    let drafts = builtin_conversation_documents(conversation());

    assert_eq!(drafts.len(), 5);
    assert_eq!(
        drafts
            .iter()
            .map(|(kind, _)| kind.as_str())
            .collect::<Vec<_>>(),
        vec![AGENT_KIND, LIVE_KIND, INBOX_KIND, PROVIDER_KIND, USAGE_KIND]
    );

    // `builtin_conversation_documents` is a pure fold over the definitions:
    // each draft is exactly that definition's `initial` value.
    let expected: Vec<(String, JsonObject)> = conversation_definitions()
        .into_iter()
        .map(|(kind, definition)| (kind.to_owned(), definition.initial_value()))
        .collect();
    assert_eq!(drafts, expected);

    // Spelled out so a shape change in any module fails here.
    assert_eq!(drafts[0].1, object(json!({})));
    assert_eq!(drafts[1].1, object(json!({})));
    assert_eq!(drafts[2].1, object(json!({"items": []})));
    assert_eq!(drafts[3].1, object(json!({})));
    assert_eq!(drafts[4].1, object(json!({"models": {}, "tools": {}})));
}

#[test]
fn builtin_drafts_open_no_transaction_and_mint_no_ids() {
    // The helper takes no transaction by construction; these assertions pin the
    // observable half: drafts are plain values, carry no document/incarnation
    // id, and are identical for every owner conversation.
    let first = builtin_conversation_documents(ConversationId::from_raw(1));
    let second = builtin_conversation_documents(ConversationId::from_raw(2));
    assert_eq!(first, second, "drafts do not depend on the conversation");

    for (kind, value) in &first {
        for key in ["id", "documentId", "document_id", "incarnationId"] {
            assert!(
                !value.contains_key(key),
                "{kind} draft must not carry {key}"
            );
        }
    }

    // pi.provider is staged empty: its session id belongs to the generation
    // task, not to document creation (no id is minted here).
    let provider = first
        .iter()
        .find(|(kind, _)| kind == PROVIDER_KIND)
        .expect("pi.provider is staged");
    assert!(provider.1.is_empty());
    assert!(!provider.1.contains_key("sessionId"));

    // The drafts own their values: mutating one call's result cannot leak into
    // another call's result.
    let mut mine = first;
    mine[0].1.insert("cwd".to_owned(), json!("/elsewhere"));
    assert!(!second[0].1.contains_key("cwd"));
}

// ── the task-owned agent seed ──────────────────────────────────────────────

#[test]
fn agent_seed_without_an_owner_is_the_empty_agent() {
    assert_eq!(agent_seed(None), JsonObject::new());
    assert_eq!(agent_seed(None), agent_definition().initial_value());
}

#[test]
fn agent_seed_copies_the_owner_agent_instead_of_aliasing_it() {
    let mut owner = JsonObject::new();
    owner.insert(
        "model".to_owned(),
        json!({"provider": "anthropic", "modelId": "claude-sonnet-4-5"}),
    );
    owner.insert("thinkingLevel".to_owned(), json!("high"));
    owner.insert("extensions".to_owned(), json!(["tools", "skills"]));
    owner.insert("tools".to_owned(), json!({"remove": ["bash"]}));
    owner.insert("instructions".to_owned(), json!("be brief"));
    owner.insert("cwd".to_owned(), json!("/work"));
    owner.insert("futureSection".to_owned(), json!({"kept": true}));

    let seed = agent_seed(Some(&owner));
    assert_eq!(seed, owner, "every section is copied verbatim");

    // Changing the owner after the copy does not reach the seed...
    owner.insert("cwd".to_owned(), json!("/changed"));
    owner["model"]["modelId"] = json!("changed");
    assert_eq!(seed["cwd"], json!("/work"));
    assert_eq!(seed["model"]["modelId"], json!("claude-sonnet-4-5"));

    // ...and changing the seed (including a nested value) does not reach the
    // owner.
    let mut seed = seed;
    seed.insert("instructions".to_owned(), json!("changed"));
    seed["tools"]["remove"] = json!([]);
    assert_eq!(owner["instructions"], json!("be brief"));
    assert_eq!(owner["tools"]["remove"], json!(["bash"]));
    assert_eq!(
        seed["futureSection"],
        json!({"kept": true}),
        "unknown future sections survive the copy"
    );
}

// ── pi.provider wire shape ─────────────────────────────────────────────────

#[test]
fn provider_state_round_trips_camel_case() {
    let empty = ProviderState::default();
    assert_eq!(empty.session_id, None);
    assert_eq!(
        serde_json::to_value(&empty).expect("serialize"),
        json!({}),
        "an absent id is omitted, never null"
    );

    let state = ProviderState {
        session_id: Some("0191f3a0-0000-7000-8000-000000000000".to_owned()),
    };
    let wire = serde_json::to_value(&state).expect("serialize");
    assert_eq!(
        wire,
        json!({"sessionId": "0191f3a0-0000-7000-8000-000000000000"}),
        "the field is camelCase on the wire"
    );

    assert_eq!(
        serde_json::from_value::<ProviderState>(wire).expect("deserialize"),
        state
    );
    assert_eq!(
        serde_json::from_value::<ProviderState>(json!({})).expect("deserialize"),
        empty,
        "an omitted id decodes to None"
    );
    assert_eq!(
        serde_json::from_value::<ProviderState>(json!({"sessionId": null})).expect("deserialize"),
        empty
    );
    assert_eq!(
        serde_json::from_value::<ProviderState>(json!({"sessionId": "abc", "extra": 1}))
            .expect("unknown fields are ignored"),
        ProviderState {
            session_id: Some("abc".to_owned())
        }
    );
}

#[test]
fn provider_document_initial_matches_the_default_provider_state() {
    assert_eq!(
        provider_definition().initial_value(),
        default_provider_wire()
    );
}
