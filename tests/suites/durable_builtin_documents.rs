//! The five built-in conversation documents (spec §8).
//!
//! Covers the definition table every conversation is created with — the five
//! kinds and their exact scope/history/fork semantics (upstream
//! `harness/harness.ts:410-417`), the creation-order list, the drafts
//! `builtin_conversation_documents` hands a caller to stage, the task-owned
//! `pi.agent` seed, and `pi.provider`'s wire shape.
//!
//! Tests live here as an integration suite rather than inline in the module:
//! the crate forbids inline tests.

use pir::durable::harness::documents::{
    agent_definition, agent_seed, builtin_conversation_documents, conversation_definitions,
    provider_definition, ProviderState, AGENT_KIND, AGENT_VERSION, PROVIDER_KIND, PROVIDER_VERSION,
};
use pir::durable::harness::inbox::{INBOX_DEFINITION, INBOX_KIND, INBOX_VERSION};
use pir::durable::harness::live::{LIVE_DEFINITION, LIVE_KIND, LIVE_VERSION};
use pir::durable::harness::usage::{USAGE_DEFINITION, USAGE_KIND, USAGE_VERSION};
use pir::durable::{
    AnyDocDefinition, ConversationId, DocumentFork, DocumentHistory, DocumentSemantics,
};
use serde_json::{json, Map, Value};

// ── helpers ────────────────────────────────────────────────────────────────

fn conversation() -> ConversationId {
    ConversationId::from_raw(7)
}

/// Assert the whole identity of one definition.
fn assert_definition(
    definition: &AnyDocDefinition,
    kind: &str,
    version: i64,
    semantics: DocumentSemantics,
) {
    assert_eq!(definition.common.kind, kind);
    assert_eq!(definition.common.version, version);
    assert_eq!(definition.common.semantics, semantics);
}

fn kinds() -> Vec<&'static str> {
    conversation_definitions()
        .into_iter()
        .map(|(kind, _)| kind)
        .collect()
}

// ── the definition table ───────────────────────────────────────────────────

#[test]
fn agent_definition_is_rewindable_and_forks_as_of() {
    let definition = agent_definition();
    assert_definition(
        &definition,
        "pi.agent",
        1,
        DocumentSemantics::ConversationRewindable {
            fork: DocumentFork::AsOf,
        },
    );
    assert_eq!(definition.common.kind, AGENT_KIND);
    assert_eq!(definition.common.version, AGENT_VERSION);
    assert_eq!(
        definition.common.semantics.history(),
        Some(DocumentHistory::Rewindable)
    );
    assert_eq!(definition.common.semantics.fork(), Some(DocumentFork::AsOf));
    assert_eq!(
        definition.common.semantics.scope_kind(),
        "conversation",
        "pi.agent is conversation-scoped"
    );
    // Every AgentState field is optional, so the initial value is `{}`.
    assert_eq!(Value::Object(definition.initial_value()), json!({}));
    assert!(!definition.has_migration(), "version 1 needs no migration");
    // The agent document is a complete base on every change upstream.
    assert!(definition.forces_checkpoint(&Map::new()));
    assert!(definition.forces_checkpoint(
        json!({"model": {"provider": "anthropic", "modelId": "claude"}})
            .as_object()
            .expect("object")
    ));
}

#[test]
fn provider_definition_is_latest_and_forks_initial() {
    let definition = provider_definition();
    assert_definition(
        &definition,
        "pi.provider",
        1,
        DocumentSemantics::ConversationLatest {
            fork: DocumentFork::Initial,
        },
    );
    assert_eq!(definition.common.kind, PROVIDER_KIND);
    assert_eq!(definition.common.version, PROVIDER_VERSION);
    assert_eq!(
        definition.common.semantics.history(),
        Some(DocumentHistory::Latest)
    );
    assert_eq!(
        definition.common.semantics.fork(),
        Some(DocumentFork::Initial),
        "a fork gets a fresh provider identity, never its parent's"
    );
    assert_eq!(definition.common.semantics.scope_kind(), "conversation");
    assert_eq!(
        Value::Object(definition.initial_value()),
        json!({}),
        "the definition mints no session id; the generation task does"
    );
    assert!(!definition.has_migration());
}

#[test]
fn live_inbox_and_usage_keep_their_declared_semantics() {
    // The three already-existing modules and this one agree on the table.
    for (definition, kind, version) in [
        (&*LIVE_DEFINITION, LIVE_KIND, LIVE_VERSION),
        (&*INBOX_DEFINITION, INBOX_KIND, INBOX_VERSION),
        (&*USAGE_DEFINITION, USAGE_KIND, USAGE_VERSION),
    ] {
        assert_definition(
            definition,
            kind,
            version,
            DocumentSemantics::ConversationLatest {
                fork: DocumentFork::Initial,
            },
        );
        assert_eq!(
            definition.common.semantics.history(),
            Some(DocumentHistory::Latest)
        );
        assert_eq!(
            definition.common.semantics.fork(),
            Some(DocumentFork::Initial)
        );
        assert!(!definition.has_migration());
    }

    // And their initial values are the ones the staging helper must produce.
    assert_eq!(Value::Object(LIVE_DEFINITION.initial_value()), json!({}));
    assert_eq!(
        Value::Object(INBOX_DEFINITION.initial_value()),
        json!({"items": []})
    );
    assert_eq!(
        Value::Object(USAGE_DEFINITION.initial_value()),
        json!({"models": {}, "tools": {}})
    );
}

#[test]
fn every_builtin_version_is_one() {
    assert_eq!(AGENT_VERSION, 1);
    assert_eq!(LIVE_VERSION, 1);
    assert_eq!(INBOX_VERSION, 1);
    assert_eq!(PROVIDER_VERSION, 1);
    assert_eq!(USAGE_VERSION, 1);
}

// ── creation order ─────────────────────────────────────────────────────────

#[test]
fn conversation_definitions_are_five_in_creation_order() {
    let definitions = conversation_definitions();
    assert_eq!(definitions.len(), 5);
    assert_eq!(
        definitions
            .iter()
            .map(|(kind, _)| *kind)
            .collect::<Vec<_>>(),
        vec!["pi.agent", "pi.live", "pi.inbox", "pi.provider", "pi.usage"]
    );

    // The list holds the same definitions the modules expose.
    for (kind, definition) in &definitions {
        assert_eq!(definition.common.kind.as_str(), *kind);
        assert_eq!(definition.common.version, 1);
        assert_eq!(definition.common.semantics.scope_kind(), "conversation");
    }
}

// ── staging drafts ─────────────────────────────────────────────────────────

#[test]
fn builtin_conversation_documents_are_the_definitions_initial_values() {
    let drafts = builtin_conversation_documents(conversation());
    assert_eq!(drafts.len(), 5);
    assert_eq!(
        drafts
            .iter()
            .map(|(kind, _)| kind.as_str())
            .collect::<Vec<_>>(),
        kinds()
    );

    // Each draft is exactly its definition's `initial` value.
    let expected: Vec<(String, Map<String, Value>)> = vec![
        ("pi.agent".to_owned(), agent_definition().initial_value()),
        ("pi.live".to_owned(), LIVE_DEFINITION.initial_value()),
        ("pi.inbox".to_owned(), INBOX_DEFINITION.initial_value()),
        (
            "pi.provider".to_owned(),
            provider_definition().initial_value(),
        ),
        ("pi.usage".to_owned(), USAGE_DEFINITION.initial_value()),
    ];
    assert_eq!(drafts, expected);

    // Spelled out, so a shape change in any module fails here.
    assert_eq!(drafts[0].1, json!({}).as_object().unwrap().clone());
    assert_eq!(drafts[1].1, json!({}).as_object().unwrap().clone());
    assert_eq!(
        drafts[2].1,
        json!({"items": []}).as_object().unwrap().clone()
    );
    assert_eq!(drafts[3].1, json!({}).as_object().unwrap().clone());
    assert_eq!(
        drafts[4].1,
        json!({"models": {}, "tools": {}})
            .as_object()
            .unwrap()
            .clone()
    );
}

#[test]
fn staging_does_not_depend_on_the_conversation_and_mints_nothing() {
    let first = builtin_conversation_documents(ConversationId::from_raw(1));
    let second = builtin_conversation_documents(ConversationId::from_raw(2));
    assert_eq!(
        first, second,
        "the drafts are conversation-independent values"
    );

    // `pi.provider` is empty: its session id belongs to the generation task.
    let provider = first
        .iter()
        .find(|(kind, _)| kind == PROVIDER_KIND)
        .expect("pi.provider is staged");
    assert!(!provider.1.contains_key("sessionId"));
    assert!(provider.1.is_empty());
}

// ── the task-owned agent seed ──────────────────────────────────────────────

#[test]
fn agent_seed_defaults_to_an_empty_agent() {
    assert_eq!(agent_seed(None), agent_definition().initial_value());
    assert!(agent_seed(None).is_empty());
}

#[test]
fn agent_seed_copies_the_owner_agent() {
    let mut owner = Map::new();
    owner.insert(
        "model".to_owned(),
        json!({"provider": "anthropic", "modelId": "claude-sonnet-4-5"}),
    );
    owner.insert("thinkingLevel".to_owned(), json!("high"));
    owner.insert("extensions".to_owned(), json!(["tools", "skills"]));
    owner.insert("tools".to_owned(), json!({"remove": ["bash"]}));
    owner.insert("instructions".to_owned(), json!("be brief"));
    owner.insert("cwd".to_owned(), json!("/work"));

    let seed = agent_seed(Some(&owner));
    assert_eq!(seed, owner, "the owner's sections are copied verbatim");
    assert_eq!(seed.len(), 6);

    // A clone: editing the seed never reaches the owner's map.
    let mut seed = seed;
    seed.insert("instructions".to_owned(), json!("changed"));
    assert_eq!(owner["instructions"], json!("be brief"));
}

#[test]
fn agent_seed_preserves_every_known_section_key() {
    // `camelCase` keys as AgentState serializes them; the seed does not
    // filter, so an unknown future section survives the copy too.
    for key in [
        "model",
        "thinkingLevel",
        "extensions",
        "tools",
        "modelTools",
        "instructions",
        "cwd",
        "futureSection",
    ] {
        let mut owner = Map::new();
        owner.insert(key.to_owned(), json!("value"));
        assert_eq!(agent_seed(Some(&owner)), owner, "key {key}");
    }
}

// ── pi.provider wire shape ─────────────────────────────────────────────────

#[test]
fn provider_state_serializes_camel_case_and_omits_an_absent_id() {
    let empty = ProviderState::default();
    assert_eq!(empty.session_id, None);
    assert_eq!(
        serde_json::to_value(&empty).expect("serialize"),
        json!({}),
        "an absent id is omitted, not written as null"
    );

    let present = ProviderState {
        session_id: Some("0191f3a0-0000-7000-8000-000000000000".to_owned()),
    };
    assert_eq!(
        serde_json::to_value(&present).expect("serialize"),
        json!({"sessionId": "0191f3a0-0000-7000-8000-000000000000"})
    );

    // camelCase on the way in, and a missing id decodes to `None`.
    let decoded: ProviderState =
        serde_json::from_value(json!({"sessionId": "abc"})).expect("decode");
    assert_eq!(decoded.session_id.as_deref(), Some("abc"));
    assert_eq!(
        serde_json::from_value::<ProviderState>(json!({})).expect("decode"),
        ProviderState::default()
    );
    // The snapshot extra key is tolerated.
    assert_eq!(
        serde_json::from_value::<ProviderState>(json!({"sessionId": "abc", "extra": 1}))
            .expect("decode")
            .session_id
            .as_deref(),
        Some("abc")
    );
}

#[test]
fn provider_initial_value_matches_provider_state_default() {
    assert_eq!(
        provider_definition().initial_value(),
        serde_json::to_value(ProviderState::default())
            .expect("serialize")
            .as_object()
            .expect("object")
            .clone()
    );
}

// ── the spec's own table ───────────────────────────────────────────────────

#[test]
fn the_five_kinds_are_exactly_the_spec_table() {
    // spec §8: `pi.agent`, `pi.live`, `pi.inbox`, `pi.provider`, `pi.usage`.
    // `pi.agent` alone is rewindable with an as-of fork; the other four are
    // latest with an initial fork.
    for (kind, definition) in conversation_definitions() {
        let rewindable = kind == "pi.agent";
        assert_eq!(
            definition.common.semantics.history(),
            Some(if rewindable {
                DocumentHistory::Rewindable
            } else {
                DocumentHistory::Latest
            }),
            "kind {kind}"
        );
        assert_eq!(
            definition.common.semantics.fork(),
            Some(if rewindable {
                DocumentFork::AsOf
            } else {
                DocumentFork::Initial
            }),
            "kind {kind}"
        );
    }
}
