//! Incremental tree/projection snapshot unit tests moved out of
//! `src/agent_session/session_ctx.rs`'s inline `#[cfg(test)] mod
//! snapshot_text_tests` (PIR-SDK-027): the split arena render and the
//! projection splice must reproduce the stock `serde_json` serializers byte
//! for byte.
//!
//! The cache types and helpers are `#[doc(hidden)] pub` seams on
//! `pir::agent_session::session_ctx`; `full_render_tree` itself needs a live
//! `SessionManager`, so the suite rebuilds the same arena bookkeeping inline
//! (unchanged from the source-side fixture).

use std::collections::HashMap;
use std::sync::Arc;

use pir::agent_session::session_ctx::{
    attach_cached_node, extend_cached_forest, push_cached_node, render_cached_forest,
    serialize_projection_snapshot, CachedTree, TreeCacheKey,
};
use pir::session_manager::{FileEntry, ModelChangeEntry, TreeChildren, TreeNode};

fn model_change_entry(id: &str, parent: Option<&str>, minute: u64) -> FileEntry {
    FileEntry::ModelChange(ModelChangeEntry {
        id: id.to_owned(),
        parent_id: parent.map(str::to_owned),
        timestamp: format!("2026-01-01T00:{:02}:00.000Z", minute),
        provider: "fake".to_owned(),
        model_id: format!("m-{id}"),
    })
}

fn tree_node(id: &str, parent: Option<&str>, minute: u64) -> TreeNode {
    TreeNode {
        entry: model_change_entry(id, parent, minute),
        children: Default::default(),
        label: None,
        label_timestamp: None,
        id: id.to_owned(),
    }
}

fn insert_by_parent(forest: &mut [TreeNode], node: TreeNode, parent: &str) -> bool {
    fn find_path(forest: &[TreeNode], parent: &str, path: &mut Vec<usize>) -> bool {
        for (index, entry) in forest.iter().enumerate() {
            path.push(index);
            if entry.id == parent {
                return true;
            }
            if find_path(&entry.children, parent, path) {
                return true;
            }
            path.pop();
        }
        false
    }
    fn node_at_path<'a>(forest: &'a mut [TreeNode], path: &[usize]) -> Option<&'a mut TreeNode> {
        let (first, rest) = path.split_first()?;
        let mut node = forest.get_mut(*first)?;
        for index in rest {
            node = node.children.get_mut(*index)?;
        }
        Some(node)
    }
    let mut path = Vec::new();
    if !find_path(forest, parent, &mut path) {
        return false;
    }
    let target = node_at_path(forest, &path).expect("the found path resolves");
    target.children.push(node);
    true
}

/// Test-only arena builder with the same bookkeeping `full_render_tree`
/// performs (the session-side fn needs a live `SessionManager`).
fn full_render_cache_for_test(forest: Vec<TreeNode>) -> CachedTree {
    let mut tree = CachedTree {
        key: TreeCacheKey {
            session_id: String::new(),
            session_path: None,
            entry_count: 0,
            leaf_id: None,
        },
        text: String::new(),
        arena: Vec::new(),
        roots: Vec::new(),
        by_id: HashMap::new(),
        last_node_id: String::new(),
        node_count: 0,
        incremental_ok: true,
    };
    let mut pending: Vec<(TreeNode, Option<u32>)> =
        forest.into_iter().rev().map(|node| (node, None)).collect();
    while let Some((node, parent)) = pending.pop() {
        let TreeNode {
            entry,
            children,
            label,
            label_timestamp,
            id: _,
        } = node;
        let index = push_cached_node(
            &mut tree,
            &entry,
            label.as_deref(),
            label_timestamp.as_deref(),
        );
        attach_cached_node(&mut tree, parent, index);
        tree.node_count += 1;
        for child in children.into_iter().rev() {
            pending.push((child, Some(index)));
        }
    }
    tree
}

/// `render_cached_forest` over a full arena render must reproduce
/// `forest_to_json_iterative`'s bytes, and the incremental splice must
/// land on the same text as a fresh full render after appends that chain
/// into the cache and root at the top level.
#[test]
fn cached_tree_render_and_splice_reproduce_forest_to_json_iterative() {
    // Shape 1: linear chain a -> b -> c (one root, three deep).
    let mut b = tree_node("b", Some("a"), 1);
    b.children = TreeChildren::from(vec![tree_node("c", Some("b"), 2)]);
    let mut a = tree_node("a", None, 0);
    a.children = TreeChildren::from(vec![b]);
    let linear = vec![a];

    // Shape 2: forked root with an ordered child mix (labeled branch,
    // leaf child, sibling subtree), plus an orphan second root.
    let mut mid = tree_node("m1", Some("r"), 1);
    mid.label = Some("checkpoint".to_owned());
    mid.label_timestamp = Some("2026-01-01T00:09:00.000Z".to_owned());
    mid.children = TreeChildren::from(vec![tree_node("m2", Some("m1"), 2)]);
    let mut sib = tree_node("s1", Some("r"), 3);
    sib.children = TreeChildren::from(vec![
        tree_node("s2", Some("s1"), 4),
        tree_node("s3", Some("s1"), 5),
    ]);
    let mut r = tree_node("r", None, 0);
    r.children = TreeChildren::from(vec![mid, tree_node("leaf", Some("r"), 6), sib]);
    let forked = vec![r, tree_node("orphan", Some("missing-parent"), 7)];

    for (shape, forest) in [("linear", linear), ("forked", forked)] {
        let expected = TreeNode::forest_to_json_iterative(forest.to_vec());
        let mut tree = full_render_cache_for_test(forest.to_vec());
        assert_eq!(
            render_cached_forest(&tree),
            expected,
            "{shape}: the split render must reproduce forest_to_json_iterative byte for byte"
        );

        // Incremental splice: two appends - one chained off the deepest
        // node of the shape, one whose parent id names no entry (orphan
        // root) - with a label resolution read on the first.
        let chain_parent = match shape {
            "linear" => "c",
            _ => "m2",
        };
        let first = model_change_entry("x1", Some(chain_parent), 10);
        let second = model_change_entry("x2", Some("missing-parent"), 11);
        let resolve = |id: &str| match id {
            "x1" => (
                Some("pinned".to_owned()),
                Some("2026-01-01T00:12:00.000Z".to_owned()),
            ),
            _ => (None, None),
        };
        let text = extend_cached_forest(&mut tree, &[&first, &second], &resolve);
        assert!(
            text.is_some(),
            "{shape}: the append must splice, not fall back"
        );
        let text = render_cached_forest(&tree);

        // The oracle: the same entries appended to a fresh forest, fully
        // re-rendered by the stock serializer. The orphan keeps its
        // parent_id (an orphan ROOT in `tree()` still serializes the
        // stored `parentId` field — only the NESTING is root-level).
        let mut oracle = forest.to_vec();
        let mut x1 = tree_node("x1", Some(chain_parent), 10);
        x1.label = Some("pinned".to_owned());
        x1.label_timestamp = Some("2026-01-01T00:12:00.000Z".to_owned());
        assert!(
            insert_by_parent(&mut oracle, x1, chain_parent),
            "{shape}: x1's parent must exist in the oracle forest"
        );
        oracle.push(tree_node("x2", Some("missing-parent"), 11));
        assert_eq!(
            text,
            TreeNode::forest_to_json_iterative(oracle),
            "{shape}: the spliced text must equal a fresh full render"
        );
    }
}

/// The projection text must equal serializing the OLD `Value` build
/// (`{entries: [views], messages: <aggregate clone>}`) byte for byte,
/// and the aggregate must stay the entries field.
#[test]
fn projection_text_matches_the_value_form_byte_for_byte() {
    use pir::ai::types::{ContentBlock, Message, TextContent, UserMessage, UserRole};
    use pir::session_manager::{
        MessageEntry, ProjectedSessionEntry, SessionProjection, StoredBashExecutionMessage,
        StoredMessage,
    };

    let message = |text: &str, timestamp: i64| {
        Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: text.to_owned(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: timestamp as u64,
            sections: None,
        })
    };
    let entry = |id: &str, parent: Option<&str>| {
        FileEntry::Message(MessageEntry {
            id: id.to_owned(),
            parent_id: parent.map(str::to_owned),
            timestamp: "2026-01-01T00:00:00.000Z".to_owned(),
            message: StoredMessage::BashExecution(StoredBashExecutionMessage {
                role: pir::session_manager::types::BashExecutionRole::BashExecution,
                command: String::new(),
                output: String::new(),
                exit_code: None,
                cancelled: false,
                truncated: false,
                full_output_path: None,
                timestamp: 0,
                exclude_from_context: None,
            }),
        })
    };
    let source_entries = [entry("e1", None), entry("e2", Some("e1"))];
    let projection = SessionProjection {
        entries: vec![
            ProjectedSessionEntry {
                source_entry: Arc::new(source_entries[0].clone()),
                messages: 0..1,
            },
            ProjectedSessionEntry {
                source_entry: Arc::new(source_entries[1].clone()),
                messages: 1..3,
            },
        ],
        messages: vec![message("one", 1), message("two", 2), message("three", 3)],
        thinking_level: String::new(),
        model: None,
    };

    let (aggregate, projection_json, fragments) = serialize_projection_snapshot(&projection, None);

    // R56: an append push must splice the cached text into byte-identical
    // output. Build the extended projection (one more entry), pass the
    // cached set back, and compare with a fresh full serialization.
    {
        let mut source_entries = [
            entry("e1", None),
            entry("e2", Some("e1")),
            entry("e3", Some("e2")),
        ];
        source_entries[2] = entry("e3", Some("e2"));
        let extended = SessionProjection {
            entries: vec![
                ProjectedSessionEntry {
                    source_entry: Arc::new(source_entries[0].clone()),
                    messages: 0..1,
                },
                ProjectedSessionEntry {
                    source_entry: Arc::new(source_entries[1].clone()),
                    messages: 1..3,
                },
                ProjectedSessionEntry {
                    source_entry: Arc::new(source_entries[2].clone()),
                    messages: 3..5,
                },
            ],
            messages: vec![
                message("one", 1),
                message("two", 2),
                message("three", 3),
                message("four", 4),
                message("five", 5),
            ],
            thinking_level: String::new(),
            model: None,
        };
        let set_len = fragments.fragments.len();
        let (_agg2, text2, _set2) =
            serialize_projection_snapshot(&extended, Some((Arc::new(fragments), set_len)));

        let message_items: Vec<serde_json::Value> = extended
            .messages
            .iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        let entry_views: Vec<serde_json::Value> = extended
            .entries
            .iter()
            .map(|entry| {
                let end = entry.messages.end.min(message_items.len());
                let start = entry.messages.start.min(end);
                let mut view = serde_json::Map::new();
                view.insert(
                    String::from("entry"),
                    serde_json::to_value(entry.source_entry.as_ref()).unwrap(),
                );
                view.insert(
                    String::from("messages"),
                    serde_json::Value::Array(message_items[start..end].to_vec()),
                );
                serde_json::Value::Object(view)
            })
            .collect();
        let mut oracle = serde_json::Map::new();
        oracle.insert(
            String::from("entries"),
            serde_json::Value::Array(entry_views),
        );
        oracle.insert(
            String::from("messages"),
            serde_json::Value::Array(message_items),
        );
        let oracle_text = serde_json::to_string(&serde_json::Value::Object(oracle)).unwrap();
        assert_eq!(
            text2, oracle_text,
            "the appended splice must match a fresh full serialization"
        );
    }

    // The OLD shape, reconstructed as the oracle (the exact construction
    // `serialize_projection_snapshot` replaced).
    let message_items: Vec<serde_json::Value> = projection
        .messages
        .iter()
        .map(|message| serde_json::to_value(message).unwrap())
        .collect();
    let entry_views: Vec<serde_json::Value> = projection
        .entries
        .iter()
        .map(|entry| {
            let end = entry.messages.end.min(message_items.len());
            let start = entry.messages.start.min(end);
            let mut view = serde_json::Map::new();
            view.insert(
                String::from("entry"),
                serde_json::to_value(entry.source_entry.as_ref()).unwrap(),
            );
            view.insert(
                String::from("messages"),
                serde_json::Value::Array(message_items[start..end].to_vec()),
            );
            serde_json::Value::Object(view)
        })
        .collect();
    let mut oracle = serde_json::Map::new();
    oracle.insert(
        String::from("entries"),
        serde_json::Value::Array(entry_views),
    );
    oracle.insert(
        String::from("messages"),
        serde_json::Value::Array(message_items.clone()),
    );
    let oracle_text = serde_json::to_string(&serde_json::Value::Object(oracle)).unwrap();

    assert_eq!(
        projection_json, oracle_text,
        "the spliced projection text must match the Value form byte for byte"
    );
    assert_eq!(
        *aggregate,
        serde_json::Value::Array(message_items),
        "the entries aggregate is unchanged"
    );
}
