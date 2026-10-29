//! System prompt and tool planning (spec §7.4).
//!
//! Port of upstream `packages/durable/src/harness/prompt.ts`. Durable stores no
//! prompt sections: the conversation's resolved agent renders the desired
//! sections for each request, and the transcript's `pi.system` entries are the
//! only durable record of what the model saw. This module replays those
//! entries, renders the agent's sections, and plans the next `pi.system`
//! entries:
//!
//! * a head marker with no `pi.system` entry after it gets one complete
//!   baseline that omits every retained earlier `pi.system` entry, written even
//!   when it restates the replayed values;
//! * otherwise the minimal value patch (with `null` removals), or a
//!   remove-all/re-add-all pair when a minimal patch would change section order;
//! * tool changes ride on the last planned entry, or on one entry of their own;
//! * nothing changed and no tool change: no entries.
//!
//! The rendered section map is a [`BTreeMap`] because the Rust message model
//! stores sections in one (`UserMessage.sections`), so render order is key
//! order. That makes an order-only section change unrepresentable here, while
//! tool order (a `Vec`) is preserved and compared exactly.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::ai::types::{get_current_tools, Message, Tool, ToolReference, UserMessage, UserRole};
use crate::durable::entries::SystemEntry;
use crate::durable::harness::context::ContextView;
use crate::durable::harness::registry::{PromptInput, PromptSection};
use crate::durable::session::transaction::EntryDraft;
use crate::durable::types::{ContextEdit, ContextEditAction};

/// Sections in effect after replaying system messages in order.
///
/// A section string sets or replaces that name in place (its position stays for
/// a replace), and `null` removes it; a later re-addition is a new set. With a
/// [`BTreeMap`] the position is the key's sort position, which matches how
/// [`crate::ai::types::UserMessage`] stores sections.
#[must_use]
pub fn replay_sections(messages: &[Message]) -> BTreeMap<String, String> {
    let mut shown = BTreeMap::new();
    for message in messages {
        let Message::System(system) = message else {
            continue;
        };
        for (key, value) in system.sections.iter().flatten() {
            match value {
                None => {
                    shown.remove(key);
                }
                Some(value) => {
                    shown.insert(key.clone(), value.clone());
                }
            }
        }
    }
    shown
}

/// Render the agent's sections in order.
///
/// A section that renders `None` is omitted. Untagged text (`tag == false`) is
/// used as-is; tagged text is wrapped as `<key>\n...\n</key>`.
///
/// `shown` is the replayed state the renderers already see through
/// [`PromptInput::shown`]. Upstream also uses it to keep a failing section's
/// previous text, but a Rust [`SectionRenderer`](crate::durable::harness::registry::SectionRenderer)
/// cannot fail (it returns `Option<String>`), so there is nothing to fall back
/// to here.
#[must_use]
pub fn render_sections(
    agent_sections: &[PromptSection],
    input: &PromptInput,
    _shown: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut desired = BTreeMap::new();
    for section in agent_sections {
        let Some(text) = section.render(input) else {
            continue;
        };
        let rendered = if section.tag {
            format!("<{}>\n{text}\n</{}>", section.key, section.key)
        } else {
            text
        };
        desired.insert(section.key.clone(), rendered);
    }
    desired
}

/// Tool changes to write into one entry's system message.
#[derive(Clone, Debug, Default, PartialEq)]
struct ToolChanges {
    /// Tools that stop being available, in offered order.
    removed: Vec<ToolReference>,
    /// Complete declarations that become available.
    added: Vec<Tool>,
}

impl ToolChanges {
    fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.added.is_empty()
    }
}

/// Plan the `pi.system` entries that make the replayed sections and tools of
/// `view` equal `desired_sections` and `desired_tools` in values and order.
///
/// Returns an empty vector when nothing changed and no tool change is needed.
#[must_use]
pub fn plan_system_entries(
    view: &ContextView,
    desired_sections: &BTreeMap<String, String>,
    desired_tools: &[Tool],
    timestamp: u64,
) -> Vec<EntryDraft> {
    // A head marker with no later `pi.system` entry: write one complete
    // baseline and omit every retained `pi.system` entry, so replay of the
    // retained deltas cannot merge into the new baseline.
    if let Some(head) = &view.head {
        let has_later_system = view
            .entries
            .iter()
            .any(|entry| SystemEntry::is(Some(entry)) && entry.id > head.id);
        if !has_later_system {
            let edits: Vec<ContextEdit> = view
                .entries
                .iter()
                .filter(|entry| SystemEntry::is(Some(entry)))
                .map(|entry| ContextEdit {
                    target: entry.id,
                    action: ContextEditAction::Omit,
                })
                .collect();
            let sections: BTreeMap<String, Option<String>> = desired_sections
                .iter()
                .map(|(key, value)| (key.clone(), Some(value.clone())))
                .collect();
            let tools = ToolChanges {
                removed: Vec::new(),
                added: desired_tools.to_vec(),
            };
            let mut draft = system_draft(Some(&sections), Some(&tools), timestamp);
            if !edits.is_empty() {
                draft.edits = Some(edits);
            }
            return vec![draft];
        }
    }

    let sections = plan_sections(&replay_sections(&view.messages), desired_sections);
    let changes = plan_tools(&get_current_tools(&view.messages), desired_tools);
    if changes.is_empty() {
        return sections
            .into_iter()
            .map(|patch| system_draft(Some(&patch), None, timestamp))
            .collect();
    }
    if sections.is_empty() {
        return vec![system_draft(None, Some(&changes), timestamp)];
    }
    let last = sections.len() - 1;
    sections
        .into_iter()
        .enumerate()
        .map(|(index, patch)| {
            let tools = if index == last { Some(&changes) } else { None };
            system_draft(Some(&patch), tools, timestamp)
        })
        .collect()
}

/// One `pi.system` entry draft.
fn system_draft(
    sections: Option<&BTreeMap<String, Option<String>>>,
    tools: Option<&ToolChanges>,
    timestamp: u64,
) -> EntryDraft {
    let added = tools
        .filter(|tools| !tools.added.is_empty())
        .map(|tools| tools.added.clone());
    let removed = tools
        .filter(|tools| !tools.removed.is_empty())
        .map(|tools| tools.removed.clone());
    let message = Message::System(UserMessage {
        role: UserRole::System,
        content: Vec::new(),
        tools_added: added,
        tools_removed: removed,
        sections: sections.cloned(),
        timestamp,
    });
    EntryDraft {
        kind: SystemEntry::KIND.kind.to_owned(),
        model: Some(vec![
            serde_json::to_value(message).expect("a system message is JSON-serializable")
        ]),
        ..EntryDraft::default()
    }
}

/// Section patches: none, the minimal patch, or a remove-all/re-add-all pair
/// when the patch's order would differ from the desired order.
fn plan_sections(
    shown: &BTreeMap<String, String>,
    desired: &BTreeMap<String, String>,
) -> Vec<BTreeMap<String, Option<String>>> {
    let patched_order: Vec<&String> = shown
        .keys()
        .filter(|key| desired.contains_key(*key))
        .chain(desired.keys().filter(|key| !shown.contains_key(*key)))
        .collect();
    let desired_order: Vec<&String> = desired.keys().collect();
    if patched_order != desired_order {
        let removed = shown
            .keys()
            .map(|key| (key.clone(), None))
            .collect::<BTreeMap<_, _>>();
        let readded = desired
            .iter()
            .map(|(key, value)| (key.clone(), Some(value.clone())))
            .collect::<BTreeMap<_, _>>();
        return vec![removed, readded];
    }

    let mut patch = BTreeMap::new();
    for (key, value) in shown {
        match desired.get(key) {
            Some(next) if next == value => {}
            Some(next) => {
                patch.insert(key.clone(), Some(next.clone()));
            }
            None => {
                patch.insert(key.clone(), None);
            }
        }
    }
    for (key, value) in desired {
        if !shown.contains_key(key) {
            patch.insert(key.clone(), Some(value.clone()));
        }
    }
    if patch.is_empty() {
        Vec::new()
    } else {
        vec![patch]
    }
}

/// Tool changes from `offered` to `desired`.
///
/// A changed declaration is removed and re-added. Replay keeps retained tools
/// in place and appends additions; when that would not yield the desired order,
/// every offered tool is removed and every desired tool re-added in order.
fn plan_tools(offered: &[Tool], desired: &[Tool]) -> ToolChanges {
    let kept: Vec<&Tool> = offered
        .iter()
        .filter(|tool| {
            desired
                .iter()
                .find(|wanted| wanted.name == tool.name)
                .is_some_and(|wanted| declarations_equal(tool, wanted))
        })
        .collect();
    let kept_names: Vec<&str> = kept.iter().map(|tool| tool.name.as_str()).collect();
    let added: Vec<&Tool> = desired
        .iter()
        .filter(|tool| !kept_names.contains(&tool.name.as_str()))
        .collect();
    let replayed_names: Vec<&str> = kept
        .iter()
        .map(|tool| tool.name.as_str())
        .chain(added.iter().map(|tool| tool.name.as_str()))
        .collect();
    let desired_names: Vec<&str> = desired.iter().map(|tool| tool.name.as_str()).collect();
    if replayed_names != desired_names {
        return ToolChanges {
            removed: offered
                .iter()
                .map(|tool| ToolReference {
                    name: tool.name.clone(),
                })
                .collect(),
            added: desired.to_vec(),
        };
    }
    ToolChanges {
        removed: offered
            .iter()
            .filter(|tool| !kept_names.contains(&tool.name.as_str()))
            .map(|tool| ToolReference {
                name: tool.name.clone(),
            })
            .collect(),
        added: added.into_iter().cloned().collect(),
    }
}

/// Whether two tools declare the same interface to the model.
///
/// The declaration is exactly the [`Tool`] struct (its executable and
/// display-only fields are not part of this crate's type), so the serialized
/// values compare the same fields in a stable key order.
fn declarations_equal(left: &Tool, right: &Tool) -> bool {
    serde_json::to_value(left).unwrap_or(Value::Null)
        == serde_json::to_value(right).unwrap_or(Value::Null)
}
