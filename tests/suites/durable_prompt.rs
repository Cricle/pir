//! `durable::harness::prompt` — system prompt planning (spec §7.4).
//!
//! Section set/patch/delete planning, tagged rendering, a tool-change-only
//! entry, no-change planning, and the round-trip property upstream asserts:
//! replaying the planned entries through `derive_context` yields the desired
//! sections and tools.

use std::collections::BTreeMap;
use std::sync::Arc;

use pir::ai::types::{
    get_current_tools, AssistantMessage, AssistantRole, ContentBlock, Message, StopReason,
    TextContent, Tool, UserMessage, UserRole,
};
use pir::durable::harness::context::{derive_context, ContextView};
use pir::durable::harness::prompt::{plan_system_entries, render_sections, replay_sections};
use pir::durable::harness::registry::{PromptInput, PromptSection};
use pir::durable::ids::{id_from_number, EntryId};
use pir::durable::session::transaction::{EntryDraft, HeadTarget};
use pir::durable::types::{ContextEdit, ContextEditAction, EntryRecord};
use serde_json::{json, Value};

fn id(value: i64) -> EntryId {
    id_from_number(value)
}

/// An entry list plus the next free ID, so appending a plan mirrors a commit.
struct Fixture {
    entries: Vec<EntryRecord>,
    next: i64,
}

impl Fixture {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            next: 1,
        }
    }

    fn push(&mut self, kind: &str, messages: Option<Vec<Message>>) -> EntryId {
        let entry_id = id(self.next);
        self.next += 1;
        self.entries.push(EntryRecord {
            id: entry_id,
            conversation_id: id_from_number(1),
            kind: kind.to_owned(),
            model: messages.map(|messages| messages.into_iter().map(to_value).collect()),
            data: None,
            head: None,
            edits: None,
            by_task_id: None,
        });
        entry_id
    }

    fn user(&mut self, text: &str) -> EntryId {
        self.push("pi.user", Some(vec![user(text)]))
    }

    /// A compaction marker that heads `head`.
    fn marker(&mut self, head: EntryId) -> EntryId {
        let marker = self.push("pi.compaction", Some(vec![user("summary")]));
        let index = self.entries.len() - 1;
        self.entries[index].head = Some(head);
        marker
    }

    /// A marker that heads itself: its own ID is the range start.
    fn self_marker(&mut self) -> EntryId {
        let marker = self.push("pi.compaction", Some(vec![user("summary")]));
        let index = self.entries.len() - 1;
        self.entries[index].head = Some(marker);
        marker
    }

    /// Append planned drafts, as a commit would, and return their IDs.
    fn apply(&mut self, drafts: &[EntryDraft]) -> Vec<EntryId> {
        let mut ids = Vec::new();
        for draft in drafts {
            let entry_id = id(self.next);
            self.next += 1;
            let head = match draft.head {
                Some(HeadTarget::SelfEntry) => Some(entry_id),
                Some(HeadTarget::Entry(target)) => Some(target),
                None => None,
            };
            let model = draft.model.clone();
            self.entries.push(EntryRecord {
                id: entry_id,
                conversation_id: id_from_number(1),
                kind: draft.kind.clone(),
                model,
                data: draft.data.clone(),
                head,
                edits: draft.edits.clone(),
                by_task_id: None,
            });
            ids.push(entry_id);
        }
        ids
    }

    /// Append one edit entry that omits `target`.
    fn omit(&mut self, target: EntryId) {
        let draft = EntryDraft {
            kind: "pi.edit".to_owned(),
            edits: Some(vec![ContextEdit {
                target,
                action: ContextEditAction::Omit,
            }]),
            ..EntryDraft::default()
        };
        self.apply(&[draft]);
    }

    fn view(&self) -> ContextView {
        derive_context(&self.entries, None)
    }
}

/// One plan and the entries it was written as.
struct Planned {
    drafts: Vec<EntryDraft>,
    ids: Vec<EntryId>,
}

fn to_value(message: Message) -> Value {
    serde_json::to_value(message).expect("a message is JSON-serializable")
}

/// Read a draft's message back as a typed [`Message`] (the durable record keeps
/// model messages as JSON so they can be written verbatim).
fn message_from_value(value: &Value) -> Message {
    serde_json::from_value(value.clone()).expect("a stored model message is a Message")
}

fn user(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        sections: None,
        timestamp: 1,
    })
}

fn assistant(text: &str) -> Message {
    Message::Assistant(AssistantMessage {
        role: AssistantRole::Assistant,
        content: vec![ContentBlock::Text(TextContent {
            text: text.to_owned(),
            text_signature: None,
        })],
        api: "faux".to_owned(),
        provider: "faux".to_owned(),
        model: "faux".to_owned(),
        stop_reason: StopReason::Stop,
        timestamp: 2,
        ..AssistantMessage::default()
    })
}

fn tool(name: &str, description: &str) -> Tool {
    Tool {
        name: name.to_owned(),
        description: description.to_owned(),
        parameters: json!({ "type": "object" }),
        constrained_sampling: None,
    }
}

fn sections(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn set(sections: &BTreeMap<String, String>) -> BTreeMap<String, Option<String>> {
    sections
        .iter()
        .map(|(key, value)| (key.clone(), Some(value.clone())))
        .collect()
}

/// A draft's system message, decoded.
fn system_message(draft: &EntryDraft) -> UserMessage {
    let values = draft.model.as_ref().expect("a system entry has a model");
    let first = values.first().expect("a system entry has one message");
    match message_from_value(first) {
        Message::System(message) => message,
        other => panic!("expected a system message, got {other:?}"),
    }
}

fn draft_sections(draft: &EntryDraft) -> Option<BTreeMap<String, Option<String>>> {
    system_message(draft).sections
}

fn draft_removed(draft: &EntryDraft) -> Vec<String> {
    system_message(draft)
        .tools_removed
        .unwrap_or_default()
        .into_iter()
        .map(|reference| reference.name)
        .collect()
}

fn draft_added(draft: &EntryDraft) -> Vec<String> {
    system_message(draft)
        .tools_added
        .unwrap_or_default()
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

fn omit_targets(draft: &EntryDraft) -> Vec<EntryId> {
    draft
        .edits
        .iter()
        .flatten()
        .map(|edit: &ContextEdit| {
            assert!(matches!(edit.action, ContextEditAction::Omit));
            edit.target
        })
        .collect()
}

/// Plan, append, and assert the round-trip property: replay now yields the
/// desired sections and offers the desired tools (in order).
fn round_trip(fixture: &mut Fixture, desired: &[(&str, &str)], tools: &[Tool]) -> Planned {
    let drafts = plan_system_entries(&fixture.view(), &sections(desired), tools, 7);
    let ids = fixture.apply(&drafts);
    let view = fixture.view();
    assert_eq!(replay_sections(&view.messages), sections(desired));
    assert_eq!(get_current_tools(&view.messages), tools);
    Planned { drafts, ids }
}

/// Plan and append without asserting the round trip (used after an edit that
/// intentionally leaves replay stale).
fn plan_and_apply(fixture: &mut Fixture, desired: &[(&str, &str)], tools: &[Tool]) -> Planned {
    let drafts = plan_system_entries(&fixture.view(), &sections(desired), tools, 7);
    let ids = fixture.apply(&drafts);
    Planned { drafts, ids }
}

#[test]
fn replay_sets_in_place_deletes_on_null_and_appends_readditions() {
    let system = |pairs: &[(&str, Option<&str>)]| {
        Message::System(UserMessage {
            role: UserRole::System,
            content: Vec::new(),
            tools_added: None,
            tools_removed: None,
            sections: Some(
                pairs
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), value.map(str::to_owned)))
                    .collect(),
            ),
            timestamp: 1,
        })
    };
    let shown = replay_sections(&[
        system(&[("a", Some("1")), ("b", Some("2")), ("c", Some("3"))]),
        user("x"),
        system(&[("b", Some("20")), ("a", None)]),
        system(&[("a", Some("10"))]),
    ]);
    // Keys are in sort order: a `BTreeMap` is how a message stores sections.
    assert_eq!(
        shown.into_iter().collect::<Vec<_>>(),
        vec![
            ("a".to_owned(), "10".to_owned()),
            ("b".to_owned(), "20".to_owned()),
            ("c".to_owned(), "3".to_owned()),
        ]
    );
}

#[test]
fn render_sections_tags_by_default_and_omits_none() {
    let untagged = PromptSection::text("preamble", "You are helpful.").untagged();
    let tagged = PromptSection::text("cwd", "/repo");
    let omitted = PromptSection::new("skipped", Arc::new(|_: &PromptInput| None));
    let desired = render_sections(
        &[untagged, tagged, omitted],
        &PromptInput::default(),
        &sections(&[]),
    );
    assert_eq!(
        desired.into_iter().collect::<Vec<_>>(),
        vec![
            ("cwd".to_owned(), "<cwd>\n/repo\n</cwd>".to_owned()),
            ("preamble".to_owned(), "You are helpful.".to_owned()),
        ]
    );
}

#[test]
fn emits_minimal_value_patches_removals_and_additions() {
    let mut fixture = Fixture::new();
    let first = round_trip(&mut fixture, &[("a", "1"), ("b", "2"), ("c", "3")], &[]);
    assert_eq!(first.drafts.len(), 1);
    assert_eq!(
        draft_sections(&first.drafts[0]),
        Some(set(&sections(&[("a", "1"), ("b", "2"), ("c", "3")])))
    );
    assert!(omit_targets(&first.drafts[0]).is_empty());

    let patch = round_trip(
        &mut fixture,
        &[("a", "1"), ("b", "20"), ("c", "3"), ("d", "4")],
        &[],
    );
    assert_eq!(patch.drafts.len(), 1);
    assert_eq!(
        draft_sections(&patch.drafts[0]),
        Some(
            [
                ("b".to_owned(), Some("20".to_owned())),
                ("d".to_owned(), Some("4".to_owned())),
            ]
            .into_iter()
            .collect()
        )
    );

    let removal = round_trip(&mut fixture, &[("a", "1"), ("c", "3"), ("d", "4")], &[]);
    assert_eq!(removal.drafts.len(), 1);
    assert_eq!(
        draft_sections(&removal.drafts[0]),
        Some([("b".to_owned(), None)].into_iter().collect())
    );

    // Nothing changed: no entries.
    let unchanged = round_trip(&mut fixture, &[("a", "1"), ("c", "3"), ("d", "4")], &[]);
    assert!(unchanged.drafts.is_empty());
    assert!(unchanged.ids.is_empty());

    let cleared = round_trip(&mut fixture, &[], &[]);
    assert_eq!(cleared.drafts.len(), 1);
    assert_eq!(
        draft_sections(&cleared.drafts[0]),
        Some(
            [
                ("a".to_owned(), None),
                ("c".to_owned(), None),
                ("d".to_owned(), None),
            ]
            .into_iter()
            .collect()
        )
    );
}

#[test]
fn writes_a_complete_baseline_after_a_head_marker_and_omits_retained_system_entries() {
    let mut fixture = Fixture::new();
    let baseline = round_trip(&mut fixture, &[("a", "1"), ("b", "2")], &[]);
    let baseline_id = baseline.ids[0];
    fixture.user("hi");
    let delta = round_trip(&mut fixture, &[("a", "1"), ("b", "20")], &[]);
    let delta_id = delta.ids[0];

    // A marker heads the delta: replay alone would show only `b`, so the next
    // plan must write a complete baseline and omit the retained delta.
    fixture.marker(delta_id);
    let replanned = round_trip(&mut fixture, &[("a", "1"), ("b", "20")], &[]);
    assert_eq!(replanned.drafts.len(), 1);
    assert_eq!(
        draft_sections(&replanned.drafts[0]),
        Some(
            [
                ("a".to_owned(), Some("1".to_owned())),
                ("b".to_owned(), Some("20".to_owned())),
            ]
            .into_iter()
            .collect()
        )
    );
    assert_eq!(omit_targets(&replanned.drafts[0]), vec![delta_id]);

    // A system entry follows the marker now, so later changes are ordinary patches.
    let second_baseline = replanned.ids[0];
    let patch = round_trip(&mut fixture, &[("a", "1"), ("b", "21")], &[]);
    assert_eq!(patch.drafts.len(), 1);
    assert_eq!(
        draft_sections(&patch.drafts[0]),
        Some(
            [("b".to_owned(), Some("21".to_owned()))]
                .into_iter()
                .collect()
        )
    );
    assert!(omit_targets(&patch.drafts[0]).is_empty());
    let after = patch.ids[0];

    // A second marker on the delta keeps deltas from both sides of the first one.
    let _ = baseline_id;
    fixture.marker(delta_id);
    let replanned = round_trip(&mut fixture, &[("a", "1"), ("b", "21")], &[]);
    assert_eq!(replanned.drafts.len(), 1);
    assert_eq!(
        omit_targets(&replanned.drafts[0]),
        vec![delta_id, second_baseline, after]
    );
}

#[test]
fn writes_a_complete_post_head_baseline_even_when_replay_already_matches() {
    let mut fixture = Fixture::new();
    let baseline = round_trip(&mut fixture, &[("a", "1")], &[]);
    let baseline_id = baseline.ids[0];
    fixture.marker(baseline_id);

    let replanned = round_trip(&mut fixture, &[("a", "1")], &[]);
    assert_eq!(replanned.drafts.len(), 1);
    assert_eq!(omit_targets(&replanned.drafts[0]), vec![baseline_id]);

    // A self-marker with no retained system entry in range: a bare baseline.
    fixture.self_marker();
    let cleared = round_trip(&mut fixture, &[], &[]);
    assert_eq!(cleared.drafts.len(), 1);
    assert!(omit_targets(&cleared.drafts[0]).is_empty());
    assert_eq!(draft_sections(&cleared.drafts[0]), Some(BTreeMap::new()));
    assert!(round_trip(&mut fixture, &[], &[]).drafts.is_empty());
}

#[test]
fn adds_removes_replaces_changed_declarations_and_rewrites_order_when_needed() {
    let mut fixture = Fixture::new();
    let (a, b, c) = (tool("a", "a"), tool("b", "b"), tool("c", "c"));

    let first = round_trip(&mut fixture, &[], &[a.clone(), b.clone()]);
    assert_eq!(draft_added(&first.drafts[0]), vec!["a", "b"]);
    assert!(draft_removed(&first.drafts[0]).is_empty());
    assert!(round_trip(&mut fixture, &[], &[a.clone(), b.clone()])
        .drafts
        .is_empty());

    let added = round_trip(&mut fixture, &[], &[a.clone(), b.clone(), c.clone()]);
    assert_eq!(draft_added(&added.drafts[0]), vec!["c"]);

    let removed = round_trip(&mut fixture, &[], &[a.clone(), c.clone()]);
    assert_eq!(draft_removed(&removed.drafts[0]), vec!["b"]);

    // A changed declaration at the end is removed and re-added in place.
    let c2 = tool("c", "changed");
    let changed = round_trip(&mut fixture, &[], &[a.clone(), c2.clone()]);
    assert_eq!(draft_removed(&changed.drafts[0]), vec!["c"]);
    assert_eq!(draft_added(&changed.drafts[0]), vec!["c"]);

    // A changed declaration in the middle would move to the end, so the whole order is rewritten.
    let a2 = tool("a", "changed");
    let rewritten = round_trip(&mut fixture, &[], &[a2.clone(), c2.clone()]);
    assert_eq!(draft_removed(&rewritten.drafts[0]), vec!["a", "c"]);
    assert_eq!(draft_added(&rewritten.drafts[0]), vec!["a", "c"]);

    // Order-only change.
    let reordered = round_trip(&mut fixture, &[], &[c2.clone(), a2.clone()]);
    assert_eq!(draft_removed(&reordered.drafts[0]), vec!["a", "c"]);
    assert_eq!(draft_added(&reordered.drafts[0]), vec!["c", "a"]);

    let all_removed = round_trip(&mut fixture, &[], &[]);
    assert_eq!(draft_removed(&all_removed.drafts[0]), vec!["c", "a"]);
    assert!(draft_added(&all_removed.drafts[0]).is_empty());
}

#[test]
fn a_tool_change_alone_writes_one_entry_and_rides_the_last_section_patch() {
    let mut fixture = Fixture::new();
    let a = tool("a", "a");
    let b = tool("b", "b");

    // Sections and a tool change share the only entry.
    let combined = round_trip(
        &mut fixture,
        &[("x", "1"), ("y", "2")],
        std::slice::from_ref(&a),
    );
    assert_eq!(combined.drafts.len(), 1);
    assert_eq!(draft_added(&combined.drafts[0]), vec!["a"]);
    assert_eq!(
        draft_sections(&combined.drafts[0]),
        Some(set(&sections(&[("x", "1"), ("y", "2")])))
    );

    // A tool change alone: one entry carrying no sections.
    let tools_only = round_trip(
        &mut fixture,
        &[("x", "1"), ("y", "2")],
        &[a.clone(), b.clone()],
    );
    assert_eq!(tools_only.drafts.len(), 1);
    assert_eq!(draft_sections(&tools_only.drafts[0]), None);
    assert_eq!(draft_added(&tools_only.drafts[0]), vec!["b"]);

    // Section removals and a tool change ride the same entry.
    let both = round_trip(&mut fixture, &[], std::slice::from_ref(&a));
    assert_eq!(both.drafts.len(), 1);
    assert_eq!(
        draft_sections(&both.drafts[0]),
        Some(
            [("x".to_owned(), None), ("y".to_owned(), None)]
                .into_iter()
                .collect()
        )
    );
    assert_eq!(draft_removed(&both.drafts[0]), vec!["b"]);

    // After a head cut, every retained tool is re-declared at once.
    let last = fixture.entries.last().expect("entries exist").id;
    fixture.marker(last);
    let rebaselined = round_trip(&mut fixture, &[("z", "3")], &[a]);
    assert_eq!(rebaselined.drafts.len(), 1);
    assert!(draft_removed(&rebaselined.drafts[0]).is_empty());
    assert_eq!(draft_added(&rebaselined.drafts[0]), vec!["a"]);
    assert_eq!(
        draft_sections(&rebaselined.drafts[0]),
        Some(
            [("z".to_owned(), Some("3".to_owned()))]
                .into_iter()
                .collect()
        )
    );
}

#[test]
fn round_trip_after_a_compaction_that_kept_assistant_messages_leads_with_the_baseline() {
    let mut fixture = Fixture::new();
    fixture.user("old");
    let kept = fixture.user("kept");
    fixture.push("pi.assistant", Some(vec![assistant("answer")]));
    fixture.marker(kept);
    // The baseline generation wrote, then a delta after it.
    round_trip(&mut fixture, &[("a", "1")], &[]);
    round_trip(&mut fixture, &[("a", "2")], &[]);

    // An unchanged section set: nothing to plan.
    assert!(round_trip(&mut fixture, &[("a", "2")], &[])
        .drafts
        .is_empty());

    // The desired set changes: an ordinary patch, and replay still yields it.
    let patch = round_trip(&mut fixture, &[("a", "2"), ("preamble", "Base.")], &[]);
    assert_eq!(patch.drafts.len(), 1);
    assert!(omit_targets(&patch.drafts[0]).is_empty());

    // The baseline leads the request even though kept history precedes it.
    let view = fixture.view();
    assert!(matches!(view.messages.first(), Some(Message::System(_))));
    assert_eq!(
        replay_sections(&view.messages)
            .into_iter()
            .collect::<Vec<_>>(),
        vec![
            ("a".to_owned(), "2".to_owned()),
            ("preamble".to_owned(), "Base.".to_owned()),
        ]
    );
}

#[test]
fn an_omitted_baseline_makes_the_next_plan_restore_the_full_set() {
    // Replay after an omission shows only what a later delta restated. A
    // minimal patch would append `a` after `b`, so the plan removes every shown
    // section and re-adds the desired set — the same pair upstream writes for an
    // order-only change.
    let mut fixture = Fixture::new();
    fixture.user("hi");
    let planned = plan_and_apply(&mut fixture, &[("a", "1"), ("b", "2")], &[]);
    let baseline_id = planned.ids[0];
    plan_and_apply(&mut fixture, &[("b", "20")], &[]);
    fixture.omit(baseline_id);
    assert_eq!(
        replay_sections(&fixture.view().messages),
        sections(&[("b", "20")])
    );

    let restored = round_trip(&mut fixture, &[("a", "1"), ("b", "20")], &[]);
    assert_eq!(restored.drafts.len(), 2);
    assert_eq!(
        draft_sections(&restored.drafts[0]),
        Some([("b".to_owned(), None)].into_iter().collect())
    );
    assert_eq!(
        draft_sections(&restored.drafts[1]),
        Some(set(&sections(&[("a", "1"), ("b", "20")])))
    );
    assert!(omit_targets(&restored.drafts[0]).is_empty());
}

#[test]
fn defaults_the_draft_to_a_pi_system_entry_with_empty_content() {
    let mut fixture = Fixture::new();
    let planned = round_trip(&mut fixture, &[("a", "1")], &[]);
    assert_eq!(planned.drafts[0].kind, "pi.system");
    let message = system_message(&planned.drafts[0]);
    assert!(message.content.is_empty());
    assert_eq!(message.timestamp, 7);
    assert_eq!(planned.drafts[0].head, None);
    assert_eq!(planned.drafts[0].data, None);
}
