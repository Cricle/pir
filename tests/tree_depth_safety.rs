//! Deep-session stack safety for the session tree (`SessionManager::tree`).
//!
//! `tree()` returns a forest nested one `TreeNode` per entry, so a long LINEAR
//! session is a chain as deep as the session is long. Everything recursive over
//! that chain grows with the session length — the derived `Serialize`
//! (`serde_json::to_value` / `to_vec`), `serde_json::Value` equality, and the
//! compiler's recursive drop glue — and at ~1400 entries it overran a 2 MiB
//! thread stack. This suite is the regression net for the three fixed call
//! sites plus the type-level safety:
//!
//! - `src/agent_session/session_store.rs` (the `session_tree` event payload,
//!   and its reply-free dispatch through a real `ExtensionHandle`),
//! - `src/agent_session/session_ctx.rs` (the `ctx.sessionManager` snapshot),
//! - `src/agent_session/ext_session_ops.rs` (the SDK `SessionManager.*` read
//!   face over `__pir_host.pirSession*`),
//! - `TreeNode::to_value_iterative` / `TreeNode::forest_to_json_iterative`,
//!   `json_depth::{write_value_iteratively, drop_value_iteratively}`,
//!   `SessionManager::for_each_tree_row`, and the iterative `TreeChildren`
//!   teardown.
//!
//! EVERY case runs on a default test thread (2 MiB stack, unless the runner
//! exports `RUST_MIN_STACK`): the deep fixtures are `DEEP_ENTRIES` entries
//! long, well past the depth that crashed, so a reintroduced recursion fails
//! here instead of in a user session. Deep `serde_json::Value`s are never
//! compared, cloned, parsed, or serialized in these tests — all of those
//! recurse per nesting level (serde_json has no stack-safe text writer for a
//! nested `Value`) — and where a deep `Value` is built at all (the one
//! producer that still needs the nested shape, `to_value_iterative`) it is
//! torn down with `json_depth::drop_value_iteratively` instead of the drop
//! glue. The `ctx.sessionManager` snapshot no longer holds one: the tree is
//! carried as pre-rendered text (that path is what crashed the suite once,
//! see `extension_session_snapshot_covers_a_deep_session`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pir::agent_session::{AgentSession, AgentSessionConfig};
use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::*;
use pir::session_manager::types::{
    FileEntry, LabelEntry, MessageEntry, SessionHeader, StoredMessage,
};
use pir::session_manager::{json_depth, ms_from_iso, SessionManager, TreeNode};

/// Entries in the deep fixtures: past the ~1400-entry point where the derived
/// serialization of the nested chain overran a 2 MiB thread.
const DEEP_ENTRIES: usize = 2_000;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Per-test temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pir-tree-depth-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A valid, strictly increasing session timestamp for entry `index`.
fn timestamp_for(index: usize) -> String {
    format!(
        "2026-10-17T{:02}:{:02}:{:02}.000Z",
        index / 3600,
        (index / 60) % 60,
        index % 60
    )
}

fn user_entry(id: &str, parent_id: Option<&str>, text: &str, timestamp: &str) -> FileEntry {
    FileEntry::Message(MessageEntry {
        id: id.to_owned(),
        parent_id: parent_id.map(str::to_owned),
        timestamp: timestamp.to_owned(),
        message: StoredMessage::from(Message::User(UserMessage {
            role: UserRole::User,
            content: vec![ContentBlock::Text(TextContent {
                text: text.to_owned(),
                text_signature: None,
            })],
            tools_added: None,
            tools_removed: None,
            timestamp: 0,
            sections: None,
        })),
    })
}

/// Write one session JSONL file (header + entries) directly: the fixture is
/// built for tree SHAPE, and writing it in one pass keeps a 2000-entry
/// fixture cheap (the append API fsyncs per entry).
fn write_session_file(dir: &Path, name: &str, entries: &[FileEntry]) -> PathBuf {
    let path = dir.join(name);
    let mut text = String::new();
    let header = FileEntry::Session(SessionHeader::new(
        "tree-depth-session",
        "C:\\repo",
        timestamp_for(0),
    ));
    text.push_str(&serde_json::to_string(&header).expect("header json"));
    text.push('\n');
    for entry in entries {
        text.push_str(&serde_json::to_string(entry).expect("entry json"));
        text.push('\n');
    }
    std::fs::write(&path, text).expect("write session file");
    path
}

/// A linear chain `e0000 -> e0001 -> ...`: the shape that crashed (one nesting
/// level per entry).
fn write_linear_session(dir: &Path, entries: usize) -> PathBuf {
    let mut rows = Vec::with_capacity(entries);
    let mut previous: Option<String> = None;
    for index in 0..entries {
        let id = format!("e{index:04}");
        rows.push(user_entry(
            &id,
            previous.as_deref(),
            &format!("turn {index}"),
            &timestamp_for(index),
        ));
        previous = Some(id);
    }
    write_session_file(dir, "linear-deep.jsonl", &rows)
}

/// A star: one root with `entries - 1` children (breadth, depth 1).
fn write_star_session(dir: &Path, entries: usize) -> PathBuf {
    let mut rows = Vec::with_capacity(entries);
    rows.push(user_entry("s0000", None, "root", &timestamp_for(0)));
    for index in 1..entries {
        let id = format!("s{index:04}");
        rows.push(user_entry(
            &id,
            Some("s0000"),
            &format!("spoke {index}"),
            &timestamp_for(index),
        ));
    }
    write_session_file(dir, "star-deep.jsonl", &rows)
}

/// A small branched forest with a label: the wire-shape reference fixture.
/// `b0` has children `b1` (labeled) and `b3`; `b2`/`b4` are their leaves and
/// `l1` is the label entry on `b1` (itself a node, as `tree()` nests it).
fn write_branched_session(dir: &Path) -> PathBuf {
    let entries = vec![
        user_entry("b0", None, "root", &timestamp_for(0)),
        user_entry("b1", Some("b0"), "first branch", &timestamp_for(1)),
        user_entry("b2", Some("b1"), "leaf of first branch", &timestamp_for(2)),
        user_entry("b3", Some("b0"), "second branch", &timestamp_for(3)),
        user_entry("b4", Some("b3"), "leaf of second branch", &timestamp_for(4)),
        FileEntry::Label(LabelEntry {
            id: "l1".to_owned(),
            parent_id: Some("b2".to_owned()),
            timestamp: timestamp_for(5),
            target_id: "b1".to_owned(),
            label: Some("checkpoint".to_owned()),
        }),
    ];
    write_session_file(dir, "branched.jsonl", &entries)
}

// ---------------------------------------------------------------------------
// Wire shape: the iterative serializers must match the derived ones exactly
// ---------------------------------------------------------------------------

/// The iterative `Value` conversion and the iterative JSON text must equal the
/// derived serde output byte for byte (key INSERTION order included, which
/// `serde_json`'s `preserve_order` makes observable on the wire).
#[test]
fn iterative_tree_serialization_matches_serde_for_shallow_forests() {
    let temp = TempDir::new("wire-shape");
    let path = write_branched_session(temp.path());
    let manager = SessionManager::open(&path).expect("open branched session");

    // Reference: the derived serialization. Safe at this size, and the only
    // place in this suite where the recursive path is used — as the oracle.
    let expected_value = serde_json::to_value(manager.tree()).expect("derived value");
    let expected_text = serde_json::to_string(&expected_value).expect("derived text");

    assert_eq!(
        TreeNode::to_value_iterative(manager.tree()),
        expected_value,
        "to_value_iterative must reproduce serde_json::to_value"
    );
    assert_eq!(
        TreeNode::forest_to_json_iterative(manager.tree()),
        expected_text,
        "forest_to_json_iterative must reproduce serde_json::to_string"
    );

    // Value equality cannot see key order, so pin it: `preserve_order` writes
    // the derived insertion order, and the extension session_tree payload /
    // SDK snapshot are pinned on that order.
    let roots = expected_value.as_array().expect("forest array");
    assert_eq!(roots.len(), 1);
    let root = &roots[0];
    assert_eq!(root["id"], serde_json::json!("b0"));
    assert_eq!(
        root["children"]
            .as_array()
            .map(|children| children.iter().map(|c| c["id"].clone()).collect::<Vec<_>>()),
        Some(vec![serde_json::json!("b1"), serde_json::json!("b3")]),
        "children are ordered oldest-first by entry timestamp"
    );
    let labeled = &root["children"][0];
    assert_eq!(labeled["label"], serde_json::json!("checkpoint"));
    assert_eq!(
        labeled["labelTimestamp"],
        serde_json::json!(timestamp_for(5))
    );
    let keys: Vec<&str> = labeled
        .as_object()
        .expect("node object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec!["entry", "children", "label", "labelTimestamp", "id"],
        "the node key order is the derived insertion order"
    );

    // The flat row scan reports the same DFS order (and the same nesting
    // facts) without building the chain.
    let mut rows: Vec<(String, Option<String>, usize)> = Vec::new();
    let stats = manager
        .for_each_tree_row(|row| rows.push((row.id.clone(), row.parent_id.clone(), row.depth)))
        .expect("row scan");
    assert_eq!(
        rows.iter().map(|(id, ..)| id.as_str()).collect::<Vec<_>>(),
        vec!["b0", "b1", "b2", "l1", "b3", "b4"]
    );
    assert_eq!(
        rows.iter().map(|(_, _, depth)| *depth).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 1, 2],
        "b3/b4 hang off the root; the label entry sits under b2"
    );
    assert_eq!(rows[1].1.as_deref(), Some("b0"));
    assert_eq!(stats.entries, 6);
    assert_eq!(stats.rows, 6);
    assert_eq!(stats.roots, 1);
    assert_eq!(stats.max_depth, 3);

    // Row hints: label resolution, kind and preview.
    let mut hints: Vec<(String, &'static str, String, Option<String>)> = Vec::new();
    manager
        .for_each_tree_row(|row| {
            hints.push((
                row.id.clone(),
                row.kind_hint,
                row.preview.clone(),
                row.label.clone(),
            ))
        })
        .expect("row scan");
    assert_eq!(hints[0].1, "user_message");
    assert_eq!(hints[0].2, "root");
    assert_eq!(hints[1].3.as_deref(), Some("checkpoint"));
    assert_eq!(hints[3].1, "label");
    assert_eq!(hints[3].3, None, "the label entry itself is unlabeled");
    assert_eq!(
        hints
            .iter()
            .find(|(id, ..)| id == "b1")
            .map(|row| row.3.as_deref()),
        Some(Some("checkpoint"))
    );
}

// ---------------------------------------------------------------------------
// Depth: 2000-entry forests through the iterative faces
// ---------------------------------------------------------------------------

/// The deep linear chain through `to_value_iterative`, the flat row scan, and
/// an explicit forest drop — all on a 2 MiB test thread.
#[test]
fn deep_linear_tree_serializes_scans_and_drops_iteratively() {
    let temp = TempDir::new("deep-linear");
    let path = write_linear_session(temp.path(), DEEP_ENTRIES);
    let manager = SessionManager::open(&path).expect("open deep session");
    assert_eq!(manager.entries().len(), DEEP_ENTRIES);

    // 1. `to_value_iterative` (the `session_tree` payload / ctx snapshot /
    //    SDK snapshot tree face). Consumed as it converts, so the forest's
    //    teardown is iterative too.
    let value = TreeNode::to_value_iterative(manager.tree());
    let roots = value.as_array().expect("forest array");
    assert_eq!(roots.len(), 1, "a linear session has one root");
    let keys: Vec<&str> = roots[0]
        .as_object()
        .expect("node object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec!["entry", "children", "label", "id"],
        "labelTimestamp stays omitted when there is no label"
    );
    // Walk the chain with references only: comparing, cloning, parsing or
    // serializing a nested `Value` recurses per level (the very thing this
    // test guards against), so only shallow accessors are used.
    let mut node = &roots[0];
    let mut depth = 0usize;
    let mut visited = 0usize;
    loop {
        let expected_id = format!("e{depth:04}");
        assert_eq!(node["id"], serde_json::json!(expected_id));
        assert_eq!(node["entry"]["id"], serde_json::json!(expected_id));
        assert_eq!(
            node["entry"]["message"]["content"][0]["text"],
            serde_json::json!(format!("turn {depth}"))
        );
        visited += 1;
        let children = node["children"].as_array().expect("children array");
        if children.is_empty() {
            break;
        }
        assert_eq!(children.len(), 1, "a linear session has one child per node");
        node = &children[0];
        depth += 1;
    }
    assert_eq!(visited, DEEP_ENTRIES);
    assert_eq!(depth, DEEP_ENTRIES - 1);
    // Tear the deep value down iteratively: `serde_json`'s drop glue recurses
    // one frame per level (see `json_depth::drop_value_iteratively`).
    json_depth::drop_value_iteratively(value);

    // 2. The flat row scan (no nested chain is built at all).
    let mut rows = 0usize;
    let mut deepest = 0usize;
    let mut monotonic = true;
    let mut first_id = String::new();
    let mut last_id = String::new();
    let stats = manager
        .for_each_tree_row(|row| {
            if rows == 0 {
                first_id = row.id.clone();
            }
            last_id = row.id.clone();
            if row.depth != rows {
                monotonic = false;
            }
            deepest = row.depth;
            rows += 1;
            assert_eq!(row.kind_hint, "user_message");
            assert_eq!(
                row.parent_id.as_deref().map(str::to_owned),
                if rows == 1 {
                    None
                } else {
                    Some(format!("e{:04}", rows - 2))
                }
            );
        })
        .expect("row scan");
    assert_eq!(first_id, "e0000");
    assert_eq!(last_id, format!("e{:04}", DEEP_ENTRIES - 1));
    assert!(monotonic, "the scan is DFS pre-order, one level per step");
    assert_eq!(deepest, DEEP_ENTRIES - 1);
    assert_eq!(stats.entries, DEEP_ENTRIES);
    assert_eq!(stats.rows, DEEP_ENTRIES);
    assert_eq!(stats.roots, 1);
    assert_eq!(stats.max_depth, DEEP_ENTRIES - 1);

    // 3. An explicit forest drop: the iterative `TreeChildren` teardown (the
    //    recursive drop glue overran a 2 MiB thread at ~1400 entries).
    let forest = manager.tree();
    assert_eq!(forest.len(), 1);
    drop(forest);
}

/// Breadth is safe too: a 2000-node star (one root, 1999 children).
#[test]
fn deep_star_forest_drops_and_serializes_iteratively() {
    let temp = TempDir::new("deep-star");
    let path = write_star_session(temp.path(), DEEP_ENTRIES);
    let manager = SessionManager::open(&path).expect("open star session");

    let forest = manager.tree();
    assert_eq!(forest.len(), 1);
    assert_eq!(forest[0].children.len(), DEEP_ENTRIES - 1);
    drop(forest);

    let value = TreeNode::to_value_iterative(manager.tree());
    let roots = value.as_array().expect("forest array");
    assert_eq!(roots.len(), 1);
    let children = roots[0]["children"].as_array().expect("children array");
    assert_eq!(children.len(), DEEP_ENTRIES - 1);
    assert_eq!(children[0]["id"], serde_json::json!("s0001"));
    assert_eq!(
        children[DEEP_ENTRIES - 2]["id"],
        serde_json::json!(format!("s{:04}", DEEP_ENTRIES - 1))
    );
    json_depth::drop_value_iteratively(value);

    let stats = manager.for_each_tree_row(|_row| {}).expect("row scan");
    assert_eq!(stats.rows, DEEP_ENTRIES);
    assert_eq!(stats.max_depth, 1);
}

/// The shared stack-safe JSON writer: byte-identical to `serde_json` on a
/// shallow oracle (structure, commas, numbers, empty containers, escaping) and
/// depth-safe when it writes the 2000-deep tree text — where it must agree
/// with the tree renderer exactly.
#[test]
fn iterative_json_writer_matches_serde_and_is_depth_safe() {
    // Shallow oracle: every `Value` shape the payloads use.
    let sample = serde_json::json!({
        "text": "line\nquote\"back\\slash\u{1}\u{7f}é",
        "number": 1.5,
        "int": -7,
        "big": 18446744073709551615u64,
        "exponent": 1e300,
        "tiny": 1.5e-9,
        "bool": true,
        "nothing": null,
        "emptyArray": [],
        "emptyObject": {},
        "nested": [{"a": [1, 2, {"b": "c"}]}, {}],
    });
    assert_eq!(
        json_depth::write_value_iteratively(&sample),
        serde_json::to_string(&sample).expect("serde text"),
        "the iterative writer must reproduce serde_json byte for byte"
    );

    // Deep: the writer and the tree renderer must produce the same text.
    let temp = TempDir::new("json-writer");
    let path = write_linear_session(temp.path(), DEEP_ENTRIES);
    let manager = SessionManager::open(&path).expect("open deep session");
    let value = TreeNode::to_value_iterative(manager.tree());
    let written = json_depth::write_value_iteratively(&value);
    assert_eq!(
        written,
        TreeNode::forest_to_json_iterative(manager.tree()),
        "both iterative writers must agree at depth {DEEP_ENTRIES}"
    );
    assert!(
        written.starts_with('[') && written.ends_with(']'),
        "{written}"
    );
    assert_eq!(
        written.matches("\"entry\":").count(),
        DEEP_ENTRIES,
        "every entry must appear exactly once"
    );
    json_depth::drop_value_iteratively(value);
}

// ---------------------------------------------------------------------------
// The three fixed call sites, end to end
// ---------------------------------------------------------------------------

/// `src/agent_session/session_store.rs`: the session SWAP with no plugin
/// host. The `session_tree` payload has no consumer, so it is not built at
/// all; the swap must complete on the 2 MiB test thread without touching the
/// tree.
#[tokio::test(flavor = "current_thread")]
async fn session_swap_serializes_a_deep_session_tree() {
    let temp = TempDir::new("swap");
    let path = write_linear_session(temp.path(), DEEP_ENTRIES);
    let deep = SessionManager::open(&path).expect("open deep session");
    let agent = agent_with(
        temp.path(),
        SessionManager::in_memory(".", None).expect("in-memory session"),
    );

    let swapped = agent.replace_session_storage(deep).await;
    assert!(swapped, "the swap must complete");
    assert_eq!(
        agent.session_path().as_deref(),
        Some(path.as_path()),
        "the swap adopted the deep session"
    );
}

/// The same swap through a REAL [`pir::extensions::ExtensionHandle`]: the
/// `session_tree` event is informational (the prelude's default arm ignores a
/// handler's return value), so the emitter routes it through the reply-free
/// entry. Both configurations run on a default 2 MiB test thread:
///
/// - with NO `session_tree` handler the gate skips the payload build (and the
///   forest rendering) entirely;
/// - with a handler the JS side still receives the full 2000-entry tree,
///   while the return leg carries no `Value` — the old path re-parsed, deep
///   cloned and recursively dropped the echoed payload here.
#[tokio::test(flavor = "current_thread")]
async fn session_swap_with_a_real_extension_handle_is_depth_safe() {
    // 1. A live runtime with no `session_tree` handler.
    {
        let temp = TempDir::new("swap-handle-none");
        let path = write_linear_session(temp.path(), DEEP_ENTRIES);
        let deep = SessionManager::open(&path).expect("open deep session");
        let mut config = agent_config(
            temp.path(),
            SessionManager::in_memory(".", None).expect("in-memory session"),
        );
        config.extensions = Some(extension_handle(temp.path()));
        let agent = AgentSession::new(config);

        assert!(
            agent.replace_session_storage(deep).await,
            "the swap must complete without a session_tree handler"
        );
        assert_eq!(
            agent.session_path().as_deref(),
            Some(path.as_path()),
            "the swap adopted the deep session"
        );
    }

    // 2. A real `session_tree` handler on a real handle.
    {
        let temp = TempDir::new("swap-handle-tree");
        let path = write_linear_session(temp.path(), DEEP_ENTRIES);
        let deep = SessionManager::open(&path).expect("open deep session");
        let handle = extension_handle(temp.path());
        let extension = temp.path().join("session-tree-probe.ts");
        std::fs::write(&extension, SESSION_TREE_PROBE).expect("write probe extension");
        let loaded = handle
            .load(std::slice::from_ref(&extension))
            .await
            .expect("load probe extension");
        assert!(
            loaded.errors.is_empty(),
            "extension load errors: {:?}",
            loaded.errors
        );
        let mut config = agent_config(
            temp.path(),
            SessionManager::in_memory(".", None).expect("in-memory session"),
        );
        config.extensions = Some(handle.clone());
        let agent = AgentSession::new(config);

        assert!(
            agent.replace_session_storage(deep).await,
            "the swap must complete with a deep session_tree handler"
        );
        let probe = handle
            .eval_json("JSON.stringify(globalThis.__pirSessionTreeProbe ?? null)")
            .await
            .expect("read the session_tree probe");
        assert_eq!(probe["roots"], serde_json::json!(1), "{probe}");
        assert_eq!(
            probe["depth"],
            serde_json::json!(DEEP_ENTRIES - 1),
            "the JS handler must receive and walk the whole chain: {probe}"
        );
        assert_eq!(probe["firstId"], serde_json::json!("e0000"), "{probe}");
        assert_eq!(
            probe["lastId"],
            serde_json::json!(format!("e{:04}", DEEP_ENTRIES - 1)),
            "{probe}"
        );
        assert_eq!(
            probe["newLeafId"],
            serde_json::json!(format!("e{:04}", DEEP_ENTRIES - 1)),
            "the event still carries the post-swap leaf: {probe}"
        );
        assert_eq!(
            probe["oldLeafId"],
            serde_json::Value::Null,
            "the in-memory session had no leaf: {probe}"
        );
    }
}

/// `src/agent_session/session_ctx.rs`: the extension session snapshot is
/// pushed at the start of every prompt with the full tree; a 2000-entry
/// session must not overflow the 2 MiB test thread building it.
#[tokio::test(flavor = "current_thread")]
async fn extension_session_snapshot_covers_a_deep_session() {
    let temp = TempDir::new("ctx-snapshot");
    let path = write_linear_session(temp.path(), DEEP_ENTRIES);
    let handle = extension_handle(temp.path());

    let mut config = agent_config(
        temp.path(),
        SessionManager::open(&path).expect("open deep session"),
    );
    config.extensions = Some(handle);
    let agent = Arc::new(AgentSession::new(config));

    agent
        .prompt("deep session turn", silent())
        .await
        .expect("prompt must complete with a deep session tree");
    assert_eq!(
        agent.session_path().as_deref(),
        Some(path.as_path()),
        "the prompt ran against the deep session"
    );
    // The run appended at least its user turn on top of the 2000 entries.
    let entry_count = agent
        .with_session_manager(|session| session.entries().len())
        .await;
    assert!(
        entry_count.is_some_and(|count| count > DEEP_ENTRIES),
        "the deep transcript must have grown: {entry_count:?}"
    );
}

/// `src/agent_session/ext_session_ops.rs`: the SDK `SessionManager.open` host
/// op returns the whole snapshot (tree included) as JSON text. The DEEP case
/// reads that envelope as TEXT (counting the nested nodes) so the assertion
/// does not depend on the JS engine's own recursion over a 2000-level
/// structure; the shallow case goes through the SDK shim (`JSON.parse` +
/// `getEntries`/`getTree`/`getLabel`), pinning that the assembled snapshot is
/// the shape the shim expects.
#[tokio::test(flavor = "current_thread")]
async fn extension_session_handle_snapshot_covers_a_deep_session() {
    let temp = TempDir::new("ext-handle");
    let deep_path = write_linear_session(temp.path(), DEEP_ENTRIES);
    let shallow_path = write_branched_session(temp.path());

    let rt = pir::extensions::ExtensionRuntime::new(script_config_at(temp.path()))
        .await
        .expect("runtime construction");
    rt.eval_boot(SCRIPT_RUN_BOOT).await.expect("script boot");
    let source = DEEP_SESSION_PROBE
        .replace(
            "__PIR_SHALLOW_FILE__",
            &format!("{:?}", shallow_path.to_string_lossy().replace('\\', "/")),
        )
        .replace(
            "__PIR_DEEP_FILE__",
            &format!("{:?}", deep_path.to_string_lossy().replace('\\', "/")),
        );
    rt.seed_module("pir-ext:deep_tree_probe.js", &source)
        .expect("seed module");
    let run = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        rt.run_script("pir-ext:deep_tree_probe.js"),
    )
    .await
    .expect("probe timed out");
    match run {
        Ok(Ok(())) => {}
        Ok(Err(stack)) => panic!("probe module failed: {stack}"),
        Err(error) => panic!("script run failed: {error}"),
    }
    let value: serde_json::Value = rt
        .eval_json("JSON.stringify(globalThis.__deepTreeResult ?? null)")
        .await
        .expect("read probe results");

    // Shallow: the parsed snapshot through the SDK read face.
    assert_eq!(value["shallowEntryCount"], serde_json::json!(6), "{value}");
    assert_eq!(value["shallowTreeRoots"], serde_json::json!(1), "{value}");
    assert_eq!(
        value["shallowLeafId"],
        serde_json::json!("l1"),
        "the last entry in file order is the leaf: {value}"
    );
    assert_eq!(
        value["shallowChildIds"],
        serde_json::json!(["b1", "b3"]),
        "{value}"
    );
    assert_eq!(
        value["shallowLabel"],
        serde_json::json!("checkpoint"),
        "{value}"
    );

    // Deep: the whole envelope is one `{"ok":true,"handle":N,"snapshot":{…}}`
    // with the 2000-entry tree inside it, rendered iteratively.
    assert_eq!(value["deepRawIsOk"], serde_json::json!(true), "{value}");
    assert_eq!(value["deepRawHasTree"], serde_json::json!(true), "{value}");
    assert_eq!(
        value["deepRawTreeNodes"],
        serde_json::json!(DEEP_ENTRIES),
        "every entry must be present exactly once in the tree: {value}"
    );
    assert_eq!(
        value["deepRawHasLastNode"],
        serde_json::json!(true),
        "{value}"
    );
}

// ---------------------------------------------------------------------------
// Agent/JS harness helpers (mirroring tests/suites/session_commands.rs and
// tests/session_manager_ext.rs)
// ---------------------------------------------------------------------------

fn fake_model() -> Model {
    Model {
        id: "fake-model".into(),
        name: "Fake model".into(),
        api: "fake-api".into(),
        provider: "fake".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 4096,
        max_tokens: 256,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

struct FakeProvider;

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn name(&self) -> &str {
        "Fake provider"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![fake_model()]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageEventStream {
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            AssistantMessage {
                api: "fake".into(),
                provider: "fake".into(),
                model: "fake-model".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "reply".into(),
                    text_signature: None,
                })],
                stop_reason: StopReason::Stop,
                timestamp: 1,
                ..Default::default()
            },
        ));
        stream
    }
}

fn agent_config(cwd: &Path, session: SessionManager) -> AgentSessionConfig {
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(FakeProvider));
    AgentSessionConfig {
        provider_registry: Arc::new(registry),
        model: fake_model(),
        api_key: None,
        thinking: ThinkingLevel::Off,
        tools: pir::agent::tool_execution::ToolRegistry::new(),
        session: Some(session),
        compaction: None,
        max_turns: 2,
        system_prompt: None,
        cwd: Some(cwd.to_path_buf()),
        event_stream: None,
        extensions: None,
        tool_selection: None,
    }
}

fn agent_with(cwd: &Path, session: SessionManager) -> Arc<AgentSession> {
    Arc::new(AgentSession::new(agent_config(cwd, session)))
}

/// A no-op event handler (the seams under test never stream).
fn silent() -> pir::agent_session::EventHandler {
    Arc::new(|_| {})
}

fn extension_config(cwd: &Path) -> pir::extensions::ExtensionConfig {
    pir::extensions::ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: None,
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

fn extension_handle(cwd: &Path) -> pir::extensions::ExtensionHandle {
    pir::extensions::ExtensionHandle::start(extension_config(cwd)).expect("extension handle")
}

fn script_config_at(cwd: &Path) -> pir::extensions::ExtensionConfig {
    pir::extensions::ExtensionConfig {
        mode: "script".to_string(),
        ..extension_config(cwd)
    }
}

/// The script-mode module runner boot (`pir js` defines the same shape).
const SCRIPT_RUN_BOOT: &str = r#"
globalThis.__pirScriptRun = async (moduleName) => {
  try {
    await import(moduleName);
    return JSON.stringify({ ok: true });
  } catch (error) {
    const message = error && error.message !== undefined ? String(error.message) : String(error);
    const rawStack = error && error.stack ? String(error.stack) : "";
    const stack = rawStack.indexOf(message) >= 0 ? rawStack : message + (rawStack ? "\n" + rawStack : "");
    return JSON.stringify({ ok: false, message: message, stack: stack });
  }
};
"#;

/// A real `session_tree` handler: records the tree's shape on `globalThis`
/// with an iterative walk (the fixture is 2000 levels deep, so the probe
/// itself must not recurse).
const SESSION_TREE_PROBE: &str = r#"
export default function (pi) {
  pi.on("session_tree", (event) => {
    const tree = event.tree;
    let node = tree[0];
    let depth = 0;
    while (node.children.length > 0) {
      node = node.children[0];
      depth += 1;
    }
    globalThis.__pirSessionTreeProbe = {
      roots: tree.length,
      depth,
      firstId: tree[0].id,
      lastId: node.id,
      newLeafId: event.newLeafId ?? null,
      oldLeafId: event.oldLeafId ?? null,
    };
  });
}
"#;

/// SDK session handle over the `__pir_host.pirSession*` ops: the shallow read
/// face through the shim's parsed snapshot, plus the DEEP envelope read as raw
/// text (no JS-side recursion over the 2000-level tree).
const DEEP_SESSION_PROBE: &str = r#"
import { SessionManager } from "@earendil-works/pi-coding-agent";

export default (pi) => {};

const out = {};

// Shallow: the SDK shim parses the snapshot and walks the read face.
const shallow = SessionManager.open(__PIR_SHALLOW_FILE__);
out.shallowEntryCount = shallow.getEntries().length;
out.shallowTreeRoots = shallow.getTree().length;
out.shallowLeafId = shallow.getLeafId();
out.shallowChildIds = shallow.getTree()[0].children.map((node) => node.id);
out.shallowLabel = shallow.getLabel("b1");

// Deep: the host op's envelope is checked as text; every tree node carries
// exactly one `"entry":` key, so the count is the node count.
const deepRaw = __pir_host.pirSessionOpenJson(
  JSON.stringify({ kind: "open", path: __PIR_DEEP_FILE__ }),
);
out.deepRawIsOk = deepRaw.indexOf('{"ok":true') === 0 && deepRaw.indexOf('"error"') === -1;
out.deepRawHasTree = deepRaw.indexOf('"tree":[') !== -1;
let nodes = 0;
let from = 0;
for (;;) {
  const at = deepRaw.indexOf('"entry":', from);
  if (at === -1) break;
  nodes += 1;
  from = at + 8;
}
out.deepRawTreeNodes = nodes;
out.deepRawHasLastNode = deepRaw.indexOf('"id":"e1999"') !== -1;
// Tidy: close the handle the direct host call created.
const handleMatch = /"handle":(\d+)/.exec(deepRaw);
if (handleMatch) {
  __pir_host.pirSessionClose(Number(handleMatch[1]));
}

globalThis.__deepTreeResult = out;
"#;

/// `ms_from_iso` sanity: the fixture timestamps parse (and order the rows).
#[test]
fn fixture_timestamps_are_parsable() {
    let first = ms_from_iso(&timestamp_for(0)).expect("first timestamp");
    let later = ms_from_iso(&timestamp_for(3)).expect("later timestamp");
    assert!(later > first);
}
