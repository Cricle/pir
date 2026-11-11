use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::time::SystemTime;

thread_local! {
    /// `context_usage()` walks paid for by `push_session_context` on this
    /// thread. The P3-01 cache-hit short circuit is otherwise invisible to
    /// tests (the rebuilt payload is identical); this counter makes a
    /// regression that moves `context_usage()` back before the fingerprint
    /// check observable without timing a 10k-entry fixture.
    static CONTEXT_USAGE_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// R56 fragment-cache diagnostics: full fragment-set builds
    /// (structure change / first push) versus incremental extends paid by
    /// `serialize_projection_snapshot` on this thread. A steady append-only
    /// session must extend every push after the first.
    static FRAGMENT_FULL_BUILDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static FRAGMENT_EXTENDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static FRAGMENT_EXTENDS_CLONED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// One `share_metadata` result: the resolved system prompt plus the declared
/// tool entries (name, description, JSON schema) for the TUI `/share` and
/// `/bug` export payloads.
pub type ShareMetadata = (
    Option<String>,
    Vec<(String, String, Option<serde_json::Value>)>,
);

/// The resources one `resources_discover` dispatch contributed, identified by
/// name AND source path. Tracking both is what makes a reload able to REPLACE
/// its own previous contribution: a same-named resource served from a
/// different path (a host-installed base skill) is not ours to drop, while the
/// exact entry we added is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ExtensionResourceContribution {
    /// `(skill name, skill path)` actually added by the last dispatch.
    pub(crate) skills: Vec<(String, PathBuf)>,
    /// `(template name, template file path)` actually added by the last
    /// dispatch.
    pub(crate) prompt_templates: Vec<(String, PathBuf)>,
}

impl ExtensionResourceContribution {
    /// Whether nothing was contributed.
    fn is_empty(&self) -> bool {
        self.skills.is_empty() && self.prompt_templates.is_empty()
    }

    /// Remove this contribution's entries from `skills`/`prompt_templates`
    /// (name + path must both match, so a same-named entry from another
    /// source survives).
    fn retract(
        &self,
        skills: &mut Vec<crate::skills::SkillInfo>,
        prompt_templates: &mut Vec<crate::coding_agent::prompt_templates::PromptTemplate>,
    ) {
        skills.retain(|skill| {
            !self
                .skills
                .iter()
                .any(|(name, path)| skill.name == *name && skill.path == *path)
        });
        prompt_templates.retain(|template| {
            !self
                .prompt_templates
                .iter()
                .any(|(name, path)| template.name == *name && template.file_path == *path)
        });
    }
}

/// Fingerprint of everything the pushed `ctx.sessionManager` snapshot is built
/// from. The snapshot push runs twice per prompt (before the run and after it
/// settles); the pre-run push is normally the previous post-run push's state
/// plus nothing, so the whole session-derived payload — context entries,
/// branch, tree, labels, projection, session listing — can be skipped when
/// this value matches the last push.
///
/// Only CHEAP session reads feed it: the resident entry count and the last
/// non-header entry id (`SessionManager::resident_entries`, borrowed — no
/// clone), the manager's id/file identity, the leaf id, the cwd, the session
/// directory's mtime (the `sessions` listing), plus the scalar snapshot cells.
/// Every session mutation the snapshot reflects either appends an entry
/// (messages, labels, name changes, thinking/model changes, compactions,
/// context edits, custom entries: entry count + last id move), moves the leaf
/// (`branch`/`reset_leaf`), or rewrites the storage identity
/// (`replace_session_storage` / `create_branched_session`: manager id + file
/// move).
#[derive(PartialEq)]
struct SnapshotFingerprint {
    model: JsonValue,
    session_id: Option<String>,
    session_path: Option<PathBuf>,
    session_cwd: String,
    entry_count: usize,
    last_entry_id: Option<String>,
    leaf_id: Option<String>,
    context_tokens: u64,
    pending_messages: usize,
    project_trusted: bool,
    sessions_dir: Option<PathBuf>,
    sessions_modified: Option<SystemTime>,
}

/// The serialized `sessions` list plus the directory state it was built from,
/// so a rebuild caused by new entries (the usual post-run push) can reuse it
/// instead of re-reading every session file in the directory.
#[derive(Clone)]
struct CachedSessions {
    dir: Option<PathBuf>,
    modified: Option<SystemTime>,
    value: JsonValue,
}

impl CachedSessions {
    fn matches(&self, dir: Option<&Path>, modified: Option<SystemTime>) -> bool {
        self.dir.as_deref() == dir && self.modified == modified
    }
}

/// Identity of the session forest the cached tree text was rendered from.
///
/// `session_id` / `session_path` cover switches, forks into a new file, and
/// `create_branched_session` (all rewrite the storage identity); `entry_count`
/// covers every append (messages, compaction, context edits, custom entries,
/// and label changes — `set_label` appends a label entry); `leaf_id` covers
/// `branch` / `reset_leaf` / fork-at-entry. A key match therefore means the
/// same append-only entry list in the same tree shape, so the text is never
/// stale.
#[derive(Clone, PartialEq, Eq)]
#[doc(hidden)]
pub struct TreeCacheKey {
    #[doc(hidden)]
    pub session_id: String,
    #[doc(hidden)]
    pub session_path: Option<PathBuf>,
    #[doc(hidden)]
    pub entry_count: usize,
    #[doc(hidden)]
    pub leaf_id: Option<String>,
}

/// One tree node's serialized text, SPLIT at its `children` array and stored
/// in a FLAT arena: the rendered bytes are `head` + children (`,`-joined,
/// child order in `first_child`/`next_sibling` slots) + `tail`, so an
/// append-only session can splice new nodes into the cached structure and
/// re-render without re-serializing any existing entry (PIR-SDK-027).
///
/// The flat arena keeps the splice O(new nodes): a parent lookup is one map
/// read and an attach is one slot write, instead of a per-append index-path
/// DFS whose path vectors made the whole push O(transcript^2) in practice.
#[derive(Clone)]
#[doc(hidden)]
pub struct CachedTreeNode {
    /// `{"entry":<entry>,"children":[`
    #[doc(hidden)]
    pub head: String,
    /// `],"label":<label>[,"labelTimestamp":<ts>],"id":<id>}`
    #[doc(hidden)]
    pub tail: String,
    /// First child's arena index, or [`tree_cache::NONE`].
    #[doc(hidden)]
    pub first_child: u32,
    /// Last child's arena index, or [`tree_cache::NONE`] — kept so the next
    /// append is O(1) (`push` to the tail instead of walking the siblings).
    #[doc(hidden)]
    pub last_child: u32,
    /// Next sibling's arena index, or [`tree_cache::NONE`].
    #[doc(hidden)]
    pub next_sibling: u32,
}

/// The pre-rendered forest text plus the SPLIT structure it was rendered
/// from. The split keeps per-node `head`/`tail` fragments in a flat arena, so
/// appends splice new node text into the structure and re-render by one
/// concatenation pass — no `tree()` deep clone and no re-serialization of
/// existing entries. A key match still serves `text` directly (a string
/// clone). `Clone` backs the rare shared-`Arc` incremental append (one
/// structural copy replaces the full re-render it would otherwise pay).
#[derive(Clone)]
#[doc(hidden)]
pub struct CachedTree {
    #[doc(hidden)]
    pub key: TreeCacheKey,
    #[doc(hidden)]
    pub text: String,
    #[doc(hidden)]
    pub arena: Vec<CachedTreeNode>,
    /// Forest root arena indexes in render order.
    #[doc(hidden)]
    pub roots: Vec<u32>,
    /// `entry id -> arena index` for every node the structure covers.
    #[doc(hidden)]
    pub by_id: HashMap<String, u32>,
    /// The id of the last non-header entry `text` covers — the sentinel the
    /// incremental append walk stops at.
    #[doc(hidden)]
    pub last_node_id: String,
    /// How many non-header entries the structure covers.
    #[doc(hidden)]
    pub node_count: usize,
    /// `false` when the full render DROPPED entries (a parent cycle —
    /// `tree()` keeps only root-reachable nodes). The split structure then no
    /// longer mirrors the entry list, so appends must full-render.
    #[doc(hidden)]
    pub incremental_ok: bool,
}

mod tree_cache {
    /// Arena slot meaning "no node" (the linked-list terminator).
    pub(super) const NONE: u32 = u32::MAX;
}

/// The cached per-entry projection fragments (PIR-SDK-027 round 2) plus the
/// append-sentinel state the incremental extension validates against — the
/// projection twin of [`CachedTree`]'s `last_node_id` sentinel.
///
/// Invariant: `fragments.fragments.len() == entry_count` and the retained
/// entry at index `len - 1` has id `last_entry_id` — the fragment vec is
/// index-aligned with the projection's `entries`, which index-aligns with the
/// manager's retained context list, which appends. A key match therefore
/// proves the cached fragments are EXACTLY the first `entry_count` views of
/// the live projection.
#[derive(Clone)]
struct CachedProjectionFragments {
    key: TreeCacheKey,
    fragments: Arc<crate::session_manager::ProjectionEntryFragments>,
    last_entry_id: String,
}

/// Cheap identity of the registry catalog serialized into
/// `ctx.modelRegistry`. A value match proves a rebuild from the live registry
/// would produce the same wire snapshot.
/// The per-provider `identity` is the live provider `Arc`'s address: catalog
/// and dynamic providers are immutable values, and a runtime registration /
/// remote-overlay update replaces the whole `Arc`, so a refcount-free pointer
/// comparison detects it. Providers that mutate their model list in place
/// declare it with [`crate::ai::provider::Provider::supports_refresh`]; their
/// current model identities are included explicitly.
#[derive(Clone, PartialEq, Eq)]
struct ModelCatalogFingerprint {
    providers: Vec<ProviderCatalogFingerprint>,
    virtual_models: Vec<VirtualModelFingerprint>,
    credentials: CredentialEnvironmentFingerprint,
}

/// The fields of a virtual-model registration that change its serialized
/// catalog row (identity, display name, routing target, reasoning flag and
/// limit overrides). A re-registration under the same id with any of these
/// different must rebuild the catalog.
#[derive(Clone, PartialEq, Eq)]
struct VirtualModelFingerprint {
    provider: String,
    id: String,
    physical_provider: String,
    physical_id: String,
    name: Option<String>,
    reasoning: Option<bool>,
    context_window: Option<u64>,
    max_tokens: Option<u64>,
}

#[derive(Clone, PartialEq, Eq)]
struct ProviderCatalogFingerprint {
    id: String,
    /// The display name is part of the serialized providers wire
    /// (`runtime_loader::ModelCatalog::from_models`); an in-place name change
    /// must rebuild the catalog even when the model list is unchanged.
    name: String,
    /// Live `Arc` address for immutable catalog providers / non-plugin
    /// dynamic providers (a runtime registration replaces the whole `Arc`).
    /// For a `pi.registerProvider`-owned dynamic provider this is `0`: its
    /// `sync_plugin_providers` entry is rebuilt every prompt, so the address
    /// is not a stable identity (P2-新-1). [`Self::plugin_config`] plus
    /// [`Self::models_content`] identify it instead.
    identity: usize,
    /// Fingerprint of the plugin definition's config JSON for a dynamic
    /// provider owned by `pi.registerProvider`. `None` for every other
    /// provider (static catalog / remote-overlay / host-registered dynamic).
    plugin_config: Option<u64>,
    /// Content hashes of an in-place-refreshable provider's current models
    /// (currently only the single-model radius gateway), AND of every
    /// plugin-owned dynamic provider (whose `Arc` is rebuilt on each sync).
    /// `Debug` output is a deterministic, complete field dump of the plain
    /// data `Model`; hashes keep credentials that `authHeader` writes into
    /// model headers out of the cached fingerprint.
    models_content: Option<Vec<u64>>,
}

/// The credential-bearing files [`crate::ai::providers::load_pi_environment`]
/// derives `authenticated_providers` from (`models.json` templates,
/// `settings.json` scope, `auth.json` stored credentials), identified by
/// `(path, mtime, size, content hash)`, plus the process environment values
/// they resolve (`has_configured_auth` provider env vars and every `$ENV` /
/// `${ENV}` template reference found in those files).
///
/// The content hash makes an equal-length rewrite inside one filesystem
/// timestamp tick observable (P3-新-2); the env snapshot makes an in-process
/// credential environment change observable (P3-新-1). Files stay small
/// (`models.json` is capped at 8 MiB), so hashing is far cheaper than the
/// catalog rebuild it guards.
#[derive(Clone, PartialEq, Eq)]
struct CredentialEnvironmentFingerprint {
    agent_dir: PathBuf,
    cwd: PathBuf,
    project_trusted: bool,
    files: Vec<CredentialFileStamp>,
    /// `(env name, value hash)` for every auth-relevant variable, sorted by
    /// name. `None` means unset; the hash never stores the secret itself.
    env: Vec<(String, Option<u64>)>,
}

#[derive(Clone, PartialEq, Eq)]
struct CredentialFileStamp {
    path: PathBuf,
    modified: Option<SystemTime>,
    len: Option<u64>,
    /// `DefaultHasher` over the full file bytes. `None` when the file does
    /// not exist or could not be read (a read failure does not pin the cache:
    /// the load also fails and [`AgentSession::build_model_catalog`] refuses
    /// to store its fingerprint).
    content_hash: Option<u64>,
}

/// The catalog fingerprint the runtime's current `ctx.modelRegistry` was
/// built from (the serialized catalog itself is not retained: the session
/// push rebuilds only on a mismatch, and the rebuild moves the values into
/// the runtime).
#[derive(Clone, PartialEq, Eq)]
struct ModelCatalogCache {
    fingerprint: ModelCatalogFingerprint,
}

/// Cheap identity of the non-extension tool surface pushed into the runtime:
/// the session's fixed core [`ToolRegistry`](crate::agent::tool_execution::ToolRegistry)
/// and the unified plugin host's registry generation (which moves on every
/// JS/MCP/builtin registration rebuild). Builtin commands ride the same
/// generation, so one key covers both pushes.
#[derive(Clone, PartialEq, Eq)]
struct ToolSurfaceCacheKey {
    plugin_registry_generation: Option<u64>,
    tools_identity: usize,
}

#[derive(Clone, PartialEq, Eq)]
struct ToolSurfaceCache {
    key: ToolSurfaceCacheKey,
}

/// Session-side counter for the per-prompt catalog push (the path the
/// snapshot cache exists to skip). The other two probes are measured at their
/// common sink instead of inside `build_model_catalog`, so a regression that
/// re-adds a rebuild outside it is still visible:
/// - catalog pushes / model rows: [`crate::extensions::ExtensionHandle::update_model_registry`]
///   (every rebuild path that reaches the runtime must pass through it);
/// - credential loads: [`AgentSession::authenticated_providers`] (the only
///   session path that calls `load_pi_environment` for the catalog).
#[derive(Default)]
struct ModelCatalogPushStats {
    credential_environment_loads: AtomicU64,
}

/// The last session-context push's fingerprint (plus the ephemeral history
/// snapshot it was taken from). `None` until the first push.
pub(super) struct SessionContextCache {
    fingerprint: SnapshotFingerprint,
    /// The ephemeral-history snapshot the fingerprint describes. Keeping the
    /// `Arc` alive lets the check use `Arc::ptr_eq` — exact, with no chance of
    /// a freed allocation's address being reused by a new history slot.
    history: Option<HistorySnapshot>,
    sessions: CachedSessions,
    /// The last rendered forest text, reused while [`TreeCacheKey`] matches.
    tree: Option<Arc<CachedTree>>,
    /// The last cached per-entry projection fragments, reused while the same
    /// key matches and extended by the appended entries' views otherwise.
    fragments: Option<CachedProjectionFragments>,
    /// The catalog fingerprint the runtime's `ctx.modelRegistry` reflects.
    catalog: Option<ModelCatalogCache>,
    /// The tool-surface key the runtime's core tools / builtin commands
    /// reflect.
    tool_surface: Option<ToolSurfaceCache>,
    /// Counters shared across every push of this session (observability for
    /// the cache tests; no production behavior depends on them).
    stats: Arc<ModelCatalogPushStats>,
}

impl SessionContextCache {
    fn matches(
        &self,
        fingerprint: &SnapshotFingerprint,
        history: &Option<HistorySnapshot>,
    ) -> bool {
        if &self.fingerprint != fingerprint {
            return false;
        }
        match (&self.history, history) {
            (None, None) => true,
            (Some(cached), Some(current)) => Arc::ptr_eq(cached, current),
            _ => false,
        }
    }
}

/// Dispatch the `resources_discover` event and resolve the paths its handlers
/// contributed (shared by AgentSession's resource install and the print
/// host's one-shot discovery). Returns `None` when no handler is registered,
/// the dispatch failed, or the merged result carried no paths.
pub(crate) async fn dispatch_resources_discover_event(
    plugins: &PluginHost,
    cwd: &Path,
    reason: &str,
) -> Option<crate::extensions::resources::ExtensionResourcePaths> {
    use crate::extensions::resources::ExtensionResourcePaths;

    if !plugins.has_handlers("resources_discover") {
        return None;
    }
    let mut payload = serde_json::json!({
        "type": "resources_discover",
        "reason": reason,
        "cwd": cwd.display().to_string(),
    });
    let outcome = match plugins.emit("resources_discover", &mut payload).await {
        Ok(outcome) => outcome,
        Err(error) => {
            crate::ext_diag!("[ext] resources_discover dispatch failed: {error}");
            return None;
        }
    };
    for error in &outcome.errors {
        crate::ext_diag!(
            "[ext] resources_discover handler error ({}): {}",
            error.event,
            error.error
        );
    }
    let paths = ExtensionResourcePaths::from_event_result(&outcome.result, cwd);
    (!paths.is_empty()).then_some(paths)
}

/// Named inputs for [`AgentSession::build_session_context_snapshot`].
struct SessionSnapshotInputs<'a> {
    model: JsonValue,
    context_window: u64,
    trusted_project: bool,
    context_tokens: Option<u64>,
    sessions_dir: Option<PathBuf>,
    sessions_modified: Option<SystemTime>,
    cached_sessions: Option<CachedSessions>,
    cached_tree: Option<Arc<CachedTree>>,
    /// The previous push's per-entry projection fragments (PIR-SDK-027 round
    /// 2), extended by the appended views instead of re-serializing every
    /// retained message.
    cached_fragments: Option<CachedProjectionFragments>,
    /// The prompt-start projection this run already built. Reused instead of
    /// calling `build_session_projection` a second time for the snapshot.
    prebuilt_projection: Option<&'a crate::session_manager::SessionProjection>,
}

/// Serialize one projection into the snapshot's `entries` aggregate, its
/// `{entries, messages}` projection object, and the fragment cache entry the
/// NEXT push extends.
///
/// MEMORY SHAPE (PIR-SDK-027 + round 2): `entries` is the ONE
/// `Arc<JsonValue>` (shared with the plugin host's `session_entries_snapshot`
/// cell — the native contract the todo plugin reads); the projection is
/// PRE-RENDERED TEXT assembled by splicing per-entry views, and those views
/// are CACHED as owned `JsonValue` fragments when `cached` is
/// `Some((fragments, appended_from))`: the appended entries' slices are
/// serialized exactly once, every retained entry rides its cached fragment
/// VALUE (no re-serialization), the aggregate `Arc<JsonValue>` + text come
/// from the cache (an append rebuilds them from the fragment set — new
/// values only; the old entries are never cloned or re-serialized). The old
/// round-1 shape paid a full aggregate `to_value` (~200k allocs / ~16.8 MB)
/// plus the per-entry splices (~24k / ~8.1 MB) on EVERY push; every
/// "rebuild per push" variant measured worse (see
/// `ProjectionEntryFragments`), so the cached set is the steady state.
///
/// With `cached = None` the fragments + aggregate + text are built for the
/// FULL projection (structure change / first push): one fragment pass, one
/// aggregate build, one aggregate serialization — the OLD per-push cost,
/// now paid once per structure change.
///
/// The splice reproduces `serde_json::to_string`'s exact bytes: the
/// projection map is `{entries: [...views], messages: <aggregate>}`. Any
/// fragment `to_value` failure degrades BOTH the aggregate and the text to
/// the old `Null` fallback (one serialization contract, no mixed output).
#[doc(hidden)]
pub fn serialize_projection_snapshot(
    projection: &crate::session_manager::SessionProjection,
    cached: Option<(Arc<crate::session_manager::ProjectionEntryFragments>, usize)>,
) -> (
    Arc<JsonValue>,
    String,
    crate::session_manager::ProjectionEntryFragments,
) {
    let fallback = (
        Arc::new(JsonValue::Null),
        String::from("null"),
        crate::session_manager::ProjectionEntryFragments {
            fragments: Vec::new(),
            aggregate: None,
            aggregate_text: None,
            projection_json: None,
        },
    );
    let fragments = match cached {
        // The extend needs the projection to be the cached views PLUS an
        // append (a branch/trim SHRINKS or reshuffles the retained list —
        // splicing would land the cached views at wrong positions).
        Some((cached_fragments, appended_from))
            if cached_fragments.fragments.len() == appended_from
                && projection.entries.len() >= appended_from =>
        {
            // The caller's `Arc` is THIS call's own handle: `try_unwrap`
            // succeeds whenever the cache was the only other holder (the
            // steady prompt case — the previous snapshot holds the published
            // aggregate/text, not the fragment set). A concurrent reader of
            // the SET itself pays the one structural clone (still strictly
            // cheaper than re-serializing every retained message; measured
            // in the R56 logs). NOTE: do NOT `Arc::clone` before
            // `try_unwrap` — that forces the clone path on every push.
            match Arc::try_unwrap(cached_fragments) {
                Ok(owned) => {
                    FRAGMENT_EXTENDS.with(|calls| calls.set(calls.get().saturating_add(1)));
                    crate::session_manager::projection::extend_projection_entry_fragments(
                        owned,
                        projection,
                        appended_from,
                    )
                }
                Err(shared) => {
                    FRAGMENT_EXTENDS.with(|calls| calls.set(calls.get().saturating_add(1)));
                    FRAGMENT_EXTENDS_CLONED.with(|calls| calls.set(calls.get().saturating_add(1)));
                    crate::session_manager::projection::extend_projection_entry_fragments(
                        (*shared).clone(),
                        projection,
                        appended_from,
                    )
                }
            }
        }
        _ => {
            FRAGMENT_FULL_BUILDS.with(|calls| calls.set(calls.get().saturating_add(1)));
            crate::session_manager::projection::build_projection_entry_fragments(projection)
        }
    };
    if fragments.fragments.len() != projection.entries.len() {
        // Defensive: the index contract is broken (never in practice — both
        // sides derive from the same projection). Degrade to the old fallback.
        return fallback;
    }
    // The aggregate: the cached `Arc<JsonValue>` when the set carries one
    // (rebuilt from the cached items + the appended fragments by the extend;
    // the per-message clone is the one term the native `Arc<JsonValue>`
    // contract forces). A set without an aggregate falls back to the
    // per-push `to_value`.
    let message_values: Arc<JsonValue> = match &fragments.aggregate {
        Some(aggregate) => Arc::clone(aggregate),
        None => Arc::new(match serde_json::to_value(&projection.messages) {
            Ok(value) => value,
            Err(_) => return fallback,
        }),
    };
    // The FULL projection object text: cached and spliced by the extend
    // (one string copy + the appended views). Re-rendering it per push
    // (`{"entry":<entry>,"messages":<slice>}` per view) was ~144k allocs /
    // ~16 MB per push on the 8k probe (the R55 residual hotspot).
    let out: String = match &fragments.projection_json {
        Some(text) => text.clone(),
        None => {
            let entries_text = fragments.aggregate_text.clone().unwrap_or_default();
            let mut rendered = String::with_capacity(entries_text.len() * 2 + 32);
            rendered.push_str("{\"entries\":[");
            let mut first_entry = true;
            for (index, entry) in projection.entries.iter().enumerate() {
                if first_entry {
                    first_entry = false;
                } else {
                    rendered.push(',');
                }
                rendered.push_str("{\"entry\":");
                rendered.push_str(&json_text_or_null(entry.source_entry.as_ref()));
                rendered.push_str(",\"messages\":");
                match fragments.fragment(index).map(serde_json::to_string) {
                    Some(Ok(text)) => rendered.push_str(&text),
                    _ => {
                        let slice = entry.messages_in(&projection.messages);
                        match serde_json::to_string(slice) {
                            Ok(text) => rendered.push_str(&text),
                            Err(_) => return fallback,
                        }
                    }
                }
                rendered.push('}');
            }
            rendered.push_str("],\"messages\":");
            rendered.push_str(&entries_text);
            rendered.push('}');
            rendered
        }
    };
    (message_values, out, fragments)
}

/// `serde_json` text of one value, `null` when it cannot be encoded (the
/// fallback the previous `to_value`-based fields applied).
fn json_text_or_null<T: serde::Serialize + ?Sized>(value: &T) -> String {
    match serde_json::to_string(value) {
        Ok(text) => text,
        Err(_) => String::from("null"),
    }
}

/// Whether `projection` was built from the session's current leaf. Empty
/// projections match an empty leaf; a concurrent append invalidates the match.
fn projection_leaf_matches(
    session: &crate::session_manager::SessionManager,
    projection: &crate::session_manager::SessionProjection,
) -> bool {
    let live_leaf = session.leaf_id().map(str::to_owned);
    let projected_leaf = projection
        .entries
        .last()
        .and_then(|entry| entry.source_entry.id())
        .map(str::to_owned);
    live_leaf == projected_leaf
}

/// The id of the last non-header resident entry — the append sentinel the
/// fragment cache validates its continuity against (the projection twin of
/// [`CachedTree::last_node_id`]). Empty when the session has no entries.
fn resident_last_entry_id(session: &crate::session_manager::SessionManager) -> String {
    session
        .resident_entries()
        .iter()
        .rev()
        .find(|entry| !entry.is_header())
        .map(|entry| entry.entry_id().to_owned())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Incremental tree text (PIR-SDK-027)
// ---------------------------------------------------------------------------

/// Render the cached arena back into the exact forest text.
///
/// Byte-identical to [`crate::session_manager::TreeNode::forest_to_json_iterative`]
/// over the same forest (unit-tested): `[` + head/children/tail splice per
/// node + `]`. The walk is iterative over the sibling/child links, so a
/// linear session's nesting cannot recurse.
#[doc(hidden)]
pub fn render_cached_forest(tree: &CachedTree) -> String {
    enum Task {
        /// Emit one node (its head, then children, then tail).
        Node(u32),
        /// Close one children array (the `tail` after the last child).
        Tail(u32),
    }
    let mut out = String::with_capacity(tree.text.len().max(64));
    out.push('[');
    // One "has this array emitted a value yet" flag per OPEN array, so
    // commas never need lookahead (the same scheme the stock renderer
    // uses).
    let mut open_arrays: Vec<bool> = vec![true];
    let mut stack: Vec<Task> = tree
        .roots
        .iter()
        .rev()
        .map(|root| Task::Node(*root))
        .collect();
    while let Some(task) = stack.pop() {
        match task {
            Task::Node(index) => {
                let node = &tree.arena[index as usize];
                if let Some(first) = open_arrays.last_mut() {
                    if *first {
                        *first = false;
                    } else {
                        out.push(',');
                    }
                }
                out.push_str(&node.head);
                open_arrays.push(true);
                if node.first_child != tree_cache::NONE {
                    stack.push(Task::Tail(index));
                    // Push children reversed so `pop` emits the first first.
                    let mut children = Vec::new();
                    let mut child = Some(node.first_child);
                    while let Some(current) = child {
                        children.push(current);
                        child = match tree.arena[current as usize].next_sibling {
                            tree_cache::NONE => None,
                            next => Some(next),
                        };
                    }
                    for child in children.into_iter().rev() {
                        stack.push(Task::Node(child));
                    }
                } else {
                    // Empty children array: close it immediately (the
                    // tail carries the label/id fields after the `]`).
                    open_arrays.pop();
                    out.push_str(&node.tail);
                }
            }
            Task::Tail(index) => {
                open_arrays.pop();
                out.push_str(&tree.arena[index as usize].tail);
            }
        }
    }
    out.push(']');
    out
}

/// Append one arena node (its text built from `entry` and the RESOLVED
/// label facts — the same source `tree()` reads: the manager's
/// `labels_by_id` / `label_timestamps_by_id` maps) and return its index.
/// Borrowed serialization: no `FileEntry` clone, no intermediate `Value`.
#[doc(hidden)]
pub fn push_cached_node(
    tree: &mut CachedTree,
    entry: &crate::session_manager::FileEntry,
    label: Option<&str>,
    label_timestamp: Option<&str>,
) -> u32 {
    let mut head = String::with_capacity(96);
    head.push_str("{\"entry\":");
    head.push_str(&json_text_or_null(entry));
    head.push_str(",\"children\":[");
    let mut tail = String::with_capacity(64 + label.map_or(0, str::len));
    tail.push_str("],\"label\":");
    tail.push_str(&json_text_or_null(&label));
    if let Some(timestamp) = label_timestamp {
        tail.push_str(",\"labelTimestamp\":");
        tail.push_str(&json_text_or_null(&timestamp));
    }
    let id = entry.entry_id().to_owned();
    tail.push_str(",\"id\":");
    tail.push_str(&json_text_or_null(&id));
    tail.push('}');
    let index = u32::try_from(tree.arena.len())
        .expect("a session tree beyond u32::MAX nodes is not a real fixture");
    tree.arena.push(CachedTreeNode {
        head,
        tail,
        first_child: tree_cache::NONE,
        last_child: tree_cache::NONE,
        next_sibling: tree_cache::NONE,
    });
    tree.by_id.insert(id, index);
    index
}

/// Attach `child` as the LAST child of `parent` (or a forest root).
/// Appends keep file order, which equals the timestamp order `tree()`
/// sorts by for fresh entries (their timestamps are newer than every
/// existing child's; the sort is stable), so O(1) tail-linking reproduces
/// the full render's position.
#[doc(hidden)]
pub fn attach_cached_node(tree: &mut CachedTree, parent: Option<u32>, child: u32) {
    match parent {
        Some(parent) => {
            let previous_last = tree.arena[parent as usize].last_child;
            match previous_last {
                tree_cache::NONE => tree.arena[parent as usize].first_child = child,
                last => tree.arena[last as usize].next_sibling = child,
            }
            tree.arena[parent as usize].last_child = child;
        }
        None => {
            // Root level: link against the last root, then append.
            if let Some(last) = tree.roots.last().copied() {
                tree.arena[last as usize].next_sibling = child;
            }
            tree.roots.push(child);
        }
    }
}

/// Resolves `(label, label timestamp)` for one entry id while a cached tree is
/// extended; the rendered tree only keeps the two optional strings.
#[doc(hidden)]
pub type ResolveLabelFn<'a> = dyn Fn(&str) -> (Option<String>, Option<String>) + 'a;

/// Extend the cached tree by the APPENDED entries only.
///
/// `new_entries` (file order) are the entries added since the cached
/// render; their parents resolve against the cache's `by_id` map, the new
/// set, or (unknown parent / no parent) become roots — the same resolution
/// `tree()` applies (`parentId === null || parentId === id` and unknown
/// parents root the child).
///
/// Returns `None` when the batch cannot be expressed against the cache
/// (duplicate ids in one batch); the caller falls back to a full render.
#[doc(hidden)]
pub fn extend_cached_forest(
    tree: &mut CachedTree,
    new_entries: &[&crate::session_manager::FileEntry],
    resolve_label: &ResolveLabelFn<'_>,
) -> Option<()> {
    // Build every node first (a node's parent may itself be a new node
    // later in the batch), then attach parent-first over (cache ∪ new).
    let mut new_ids: Vec<&str> = Vec::with_capacity(new_entries.len());
    let mut new_slots: Vec<Option<u32>> = Vec::with_capacity(new_entries.len());
    let mut index_of_new: HashMap<&str, u32> = HashMap::with_capacity(new_entries.len());
    for entry in new_entries {
        let id = entry.entry_id();
        // A duplicate id inside one batch (invalid data) falls back.
        if index_of_new.insert(id, u32::MAX).is_some() {
            return None;
        }
        let (label, timestamp) = resolve_label(id);
        let slot = push_cached_node(tree, entry, label.as_deref(), timestamp.as_deref());
        index_of_new.insert(id, slot);
        new_ids.push(id);
        new_slots.push(Some(slot));
    }
    for (position, entry) in new_entries.iter().enumerate() {
        let own = new_slots[position]?;
        let parent_slot = match entry.parent_id() {
            // `tree()` filters `parentId === id` to a root.
            Some(parent) if parent != entry.entry_id() => match index_of_new.get(parent) {
                Some(slot) => Some(*slot),
                None => tree.by_id.get(parent).copied(),
                // An unknown parent is an ORPHAN: `tree()` roots the
                // child — the same treatment here (None below).
            },
            _ => None,
        };
        attach_cached_node(tree, parent_slot, own);
    }
    Some(())
}

/// Full tree render into the split cache: the nested `tree()` forest is
/// converted one node at a time (borrowed serialization per node — no
/// intermediate `Value`) into the flat arena. The per-node fragments are
/// the exact bytes `forest_to_json_iterative` writes, so the result — and
/// every future incremental splice — is byte-identical. The conversion is
/// a preorder walk with an explicit worklist (no recursion: a linear
/// session nests one node per entry).
fn full_render_tree(
    key: TreeCacheKey,
    session: &crate::session_manager::SessionManager,
) -> (String, Option<Arc<CachedTree>>) {
    let forest = session.tree();
    let mut tree = CachedTree {
        key,
        text: String::new(),
        arena: Vec::new(),
        roots: Vec::new(),
        by_id: HashMap::new(),
        last_node_id: String::new(),
        node_count: 0,
        incremental_ok: true,
    };
    // (node, parent arena slot); `None` parent = forest root. Preorder:
    // a parent's slot always exists by the time its children attach.
    let mut pending: Vec<(crate::session_manager::TreeNode, Option<u32>)> =
        forest.into_iter().rev().map(|node| (node, None)).collect();
    while let Some((node, parent)) = pending.pop() {
        let crate::session_manager::TreeNode {
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
    let text = render_cached_forest(&tree);
    tree.last_node_id = session
        .resident_entries()
        .iter()
        .rev()
        .find(|entry| !entry.is_header())
        .map(|entry| entry.entry_id().to_owned())
        .unwrap_or_default();
    tree.text = text.clone();
    (text, Some(Arc::new(tree)))
}

/// R56 fragment-cache diagnostics: `(full builds, incremental extends)` paid
/// by snapshot pushes on this thread since the last reset. A steady
/// append-only session extends every push after the first; a structure
/// change (fork/branch/label/trim) must show exactly one full build.
#[doc(hidden)]
pub fn fragment_cache_stats_for_test() -> (u64, u64, u64) {
    (
        FRAGMENT_FULL_BUILDS.with(|calls| calls.get()),
        FRAGMENT_EXTENDS.with(|calls| calls.get()),
        FRAGMENT_EXTENDS_CLONED.with(|calls| calls.get()),
    )
}

/// Reset [`fragment_cache_stats_for_test`].
#[doc(hidden)]
pub fn reset_fragment_cache_stats_for_test() {
    FRAGMENT_FULL_BUILDS.with(|calls| calls.set(0));
    FRAGMENT_EXTENDS.with(|calls| calls.set(0));
    FRAGMENT_EXTENDS_CLONED.with(|calls| calls.set(0));
}

impl AgentSession {
    // ------------------------------------------------------------------
    // Extension runtime integration
    // ------------------------------------------------------------------

    /// The effective system prompt string (empty when unset), as
    /// `ctx.getSystemPrompt()` and the `before_agent_start` event report it.
    pub(super) fn effective_system_prompt(&self) -> String {
        self.system_prompt_snapshot().unwrap_or_default()
    }

    /// A clone of the active system prompt (thread-safe snapshot; `None`
    /// when the host left the prompt to the context builder and no resources
    /// were installed).
    pub fn system_prompt_snapshot(&self) -> Option<String> {
        self.system_prompt
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The normalized `BuildSystemPromptOptions` the current system prompt was
    /// derived from (upstream `_baseSystemPromptOptions`,
    /// `agent-session.ts:1735-1750`), in the mutable event shape
    /// `before_agent_start.systemPromptOptions` carries
    /// (`extensions/types.ts:932-938`).
    ///
    /// The object is rebuilt from the SAME inputs the live prompt renders:
    /// the declared/hidden tool set (with every registered tool's snippet and
    /// guidelines), discovered context files and skills, the preamble and
    /// addendum (recovered from the live prompt — the prompt cache's private
    /// fields are the builder's), the MCP custom section and the working
    /// directory. A caller-supplied prompt is reported as `customPrompt`.
    ///
    /// The snapshot is cached on the extension runtime keyed by the live
    /// prompt + declared set + MCP section, so resource discovery runs only
    /// when one of those changes (a steady turn reuses the pushed object,
    /// exactly like upstream holds the resource loader's already-loaded
    /// resources).
    ///
    /// Known limitation: the SDK-only `append_derived_system_prompt` text is
    /// appended AFTER the rendered prompt rather than as a tagged section, so
    /// it is not recoverable into the `appendSystemPrompt` option field. A
    /// handler that mutates `systemPromptOptions` therefore re-renders without
    /// that trailing SDK addendum. The CLI `--system-prompt` /
    /// `--append-system-prompt` forms render as the preamble/`addendum`
    /// section and ARE recovered.
    pub(super) fn system_prompt_options_snapshot(&self) -> JsonValue {
        use crate::extensions::system_prompt_options::{
            ContextFileOption, SkillOption, SystemPromptOptions,
        };

        let plugins = self.plugins.as_deref();
        // The JS runtime is the consumer that can mutate the object. Without
        // one, the snapshot still backs native handlers, but resource
        // discovery is skipped (it would be pure cost for a no-JS session).
        let full = self.extensions.is_some();
        let registry = match plugins {
            Some(plugins) => self.merged_tool_registry(plugins),
            None => std::sync::Arc::new((*self.tools).clone()),
        };
        let active = registry.active_names(self.resolve_active_override(plugins).as_deref());
        let mcp_servers =
            crate::coding_agent::context::mcp_servers_prompt_section(self.plugins.as_deref());
        let live_prompt = self.effective_system_prompt();
        let key = format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}",
            if full { "full" } else { "lite" },
            live_prompt,
            active.join("\u{1f}"),
            mcp_servers.clone().unwrap_or_default(),
        );
        if let Some(handle) = &self.extensions {
            if let Some((cached_key, options)) = handle.system_prompt_options() {
                if cached_key == key {
                    return options;
                }
            }
        }

        let hidden = hidden_declarations_for(registry.as_ref(), &active);
        // Upstream `_rebuildSystemPrompt` fills the snippet/guideline maps from
        // EVERY registered tool (not only the selected ones): a handler may
        // add a name to `selectedTools` and expect its snippet available.
        let mut tool_snippets = std::collections::BTreeMap::new();
        let mut tool_guidelines = std::collections::BTreeMap::new();
        for tool in registry.iter() {
            let info = crate::system_prompt::ToolInfo::from_agent_tool(tool.as_ref());
            if let Some(snippet) = info.snippet.as_deref().map(str::trim) {
                if !snippet.is_empty() {
                    tool_snippets.insert(info.name.clone(), snippet.to_owned());
                }
            }
            if !info.guidelines.is_empty() {
                tool_guidelines.insert(info.name.clone(), info.guidelines.clone());
            }
        }
        let (files, skills) = if full {
            let options = crate::coding_agent::context::ContextOptions {
                disabled: !crate::coding_agent::context::context_files_enabled(),
                agent_dir: &self.agent_dir,
                package_dir: self.package_dir.as_deref(),
                max_total_bytes: self.performance.effective_context_files_max_bytes(),
            };
            (
                crate::coding_agent::context::resolve_context_files_with(&self.cwd, &options),
                crate::coding_agent::context::discover_prompt_skills_with(&self.cwd, &options),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        // Recover the preamble/addendum inputs from the live prompt. For a
        // context-derived prompt the head before the first tagged section IS
        // the preamble; compare it with the builder's default preamble to
        // decide whether `customPrompt` is set. A caller-supplied prompt is
        // opaque and becomes `customPrompt` wholesale.
        let (custom_prompt, append_system_prompt) = if self.context_derived_prompt {
            let (head, _) = split_prompt_head(&live_prompt);
            let default_head = SystemPromptOptions {
                cwd: self.cwd.display().to_string(),
                ..SystemPromptOptions::default()
            }
            .render(self.package_dir.as_deref());
            let (default_head, _) = split_prompt_head(&default_head);
            let custom = if head.is_empty() || head == default_head {
                None
            } else {
                Some(head.to_owned())
            };
            (
                custom,
                extract_tagged_section(&live_prompt, "addendum").unwrap_or_default(),
            )
        } else {
            (
                self.system_prompt_snapshot()
                    .filter(|prompt| !prompt.is_empty()),
                String::new(),
            )
        };

        let mut sections = serde_json::Map::new();
        for (name, content) in
            crate::coding_agent::context::mcp_custom_sections(mcp_servers.as_deref())
        {
            if !content.is_empty() {
                sections.insert(name, JsonValue::String(content));
            }
        }

        let options = SystemPromptOptions {
            custom_prompt,
            force_system_prompt: None,
            selected_tools: active,
            hidden_tools: hidden,
            tool_snippets,
            tool_guidelines,
            prompt_guidelines: Vec::new(),
            append_system_prompt,
            sections,
            cwd: self.cwd.display().to_string(),
            context_files: files
                .into_iter()
                .map(|(path, content)| ContextFileOption {
                    path: path.display().to_string(),
                    content,
                })
                .collect(),
            skills: skills.iter().map(SkillOption::from_skill).collect(),
        };
        let options = options.to_json();
        if let Some(handle) = &self.extensions {
            handle.update_system_prompt_options(options.clone(), key);
        }
        options
    }

    /// Execute custom `BashOperations` returned by a `user_bash` handler
    /// (upstream `executeBash(command, onChunk, {operations})`,
    /// agent-session.ts:3901-3928; rpc-mode.ts:579-583 and interactive-mode
    /// pass the handler's `operations` straight into it).
    ///
    /// Streams each output chunk through the live `bash_execution_update`
    /// session event (and `on_chunk`), records the result like a local `!`
    /// command, and returns the [`BashResult`] the host surfaces. This is the
    /// shared host path every `!` surface should call for
    /// [`UserBashOutcome::Operations`]; the TUI/RPC match arms are the only
    /// remaining wiring points (their files are owned by another lane).
    pub async fn execute_user_bash_operations(
        &self,
        operations: &JsonValue,
        command: &str,
        exclude_from_context: bool,
        on_chunk: Option<super::bash::BashChunkCallback>,
    ) -> BashResult {
        let generation = self.session_storage_generation();
        let events = self.session_events.clone();
        let sink: std::sync::Arc<dyn Fn(&str) + Send + Sync> =
            std::sync::Arc::new(move |delta: &str| {
                emit_session_event_to(
                    &events,
                    SessionEvent::BashExecutionUpdate {
                        id: None,
                        delta: delta.to_owned(),
                    },
                );
                if let Some(on_chunk) = &on_chunk {
                    on_chunk(delta);
                }
            });
        let outcome = match self.extensions_handle() {
            Some(handle) => {
                crate::extensions::user_bash::execute(handle, operations, command, &self.cwd, sink)
                    .await
            }
            None => crate::extensions::user_bash::BashOperationsResult {
                error: Some(
                    "custom bash operations require the JS extension runtime, which this \
session does not have"
                        .to_owned(),
                ),
                ..crate::extensions::user_bash::BashOperationsResult::default()
            },
        };
        let mut output = outcome.output;
        if let Some(error) = &outcome.error {
            if !output.is_empty() && !output.ends_with('\n') {
                output.push('\n');
            }
            output.push_str(error);
        }
        let result = BashResult {
            output,
            exit_code: outcome.exit_code,
            cancelled: outcome.cancelled,
            truncated: false,
            full_output_path: None,
        };
        self.record_bash_result_at_generation(command, &result, exclude_from_context, generation)
            .await;
        result
    }

    /// The system prompt plus the declared tool list for export metadata
    /// (upstream `createShareTrailingEntries` /
    /// `collectBugReportMetadata`'s tool entries: name, description, JSON
    /// schema). Public for the TUI `/share` and `/bug` flows.
    pub fn share_metadata(&self) -> ShareMetadata {
        let system_prompt = self.system_prompt_snapshot();
        let tools = self
            .tools
            .iter()
            .map(|tool| {
                (
                    tool.name().to_string(),
                    tool.description().to_string(),
                    tool.parameters(),
                )
            })
            .collect();
        (system_prompt, tools)
    }

    // ------------------------------------------------------------------
    // RSS-B: prompt projection cache
    // ------------------------------------------------------------------

    /// Take the previous prompt's projection snapshot, if any. The caller
    /// either consumes it (incremental build) or drops it (fallback).
    pub(super) fn take_cached_prompt_projection(
        &self,
    ) -> Option<crate::session_manager::projection::CachedSessionProjection> {
        self.prompt_projection_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Park a completed run's projection for the next prompt. Only called
    /// after the append-only/count validation succeeds.
    pub(super) fn store_cached_prompt_projection(
        &self,
        cached: crate::session_manager::projection::CachedSessionProjection,
    ) {
        *self
            .prompt_projection_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cached);
    }

    /// Drop the snapshot without using it (failed runs, storage swaps).
    pub(super) fn clear_cached_prompt_projection(&self) {
        *self
            .prompt_projection_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Count one prompt-start projection served by extending the previous
    /// snapshot instead of materializing the whole transcript.
    pub(super) fn record_projection_reuse(&self) {
        self.projection_reuse_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Test/profiling probe: how many prompt-start projections reused the
    /// incremental snapshot (see [`Self::take_cached_prompt_projection`]).
    #[doc(hidden)]
    pub fn projection_reuse_count(&self) -> u64 {
        self.projection_reuse_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Push the live session/model snapshot backing the `ctx` facade.
    ///
    /// Returns whether the session-derived snapshot was rebuilt; `false` means
    /// the previous push's fingerprint still describes the live state, so the
    /// runtime/plugin host already holds the identical snapshot and the push
    /// is skipped.
    pub(super) async fn push_session_context(&self, model: &Model) -> bool {
        self.push_session_context_with_projection(model, None).await
    }

    /// [`Self::push_session_context`] with a projection the caller already
    /// built for this run. The projection is serialized into the snapshot
    /// instead of rebuilding the whole transcript through
    /// `build_session_projection`. The caller must only pass one whose leaf
    /// still matches the live session (the prompt loop checks that before
    /// handing it over).
    pub(super) async fn push_session_context_with_projection(
        &self,
        model: &Model,
        prebuilt_projection: Option<&crate::session_manager::SessionProjection>,
    ) -> bool {
        let Some(runtime) = self.event_extensions() else {
            // No JS face for this session (no runtime at all, or the
            // subagent shared-runtime fallback whose parent handle must not
            // receive the child's session state). The Rust plugin list is
            // still a full consumer: a builtin/inline-only session has no
            // other source for `entries`/`session_path`/`cwd`, and the todo
            // transcript replay + memory session binding read exactly those.
            // Push the SAME snapshot payload the JS path builds, native-only
            // so a shared parent handle never sees the child's state.
            if self.plugins.is_some() {
                return self
                    .push_session_context_native_only(model, prebuilt_projection)
                    .await;
            }
            return false;
        };
        // The snapshot's `context_tokens` is the LAST usage record (upstream
        // `SessionContextSnapshot.contextTokens` = "Estimated context tokens
        // (last usage)"), not a lifetime accumulator: `self.tokens` sums every
        // settle, so it would report a monotonically growing number that
        // `ctx.getContextUsage()` then turns into a bogus percentage.
        //
        // The live value is computed AFTER the fingerprint cache check below
        // (P3-01): the check only needs a candidate, and a cache HIT proves
        // the projection inputs — resident entries (count + last id), leaf,
        // storage identity, model (which carries `contextWindow`) and the
        // ephemeral history Arc — are unchanged, so the cached count is still
        // the live one. A usage record is a message ENTRY, so one can only
        // appear through an append, which moves `entry_count`/`last_entry_id`.
        // This keeps the hit path free of `context_usage()`'s O(entries)
        // metadata walk; a miss pays it once and stores the fresh value in
        // the fingerprint (shared by fingerprint and payload, so they agree).
        let cached_context_tokens = self
            .session_context_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|cache| cache.fingerprint.context_tokens);
        // Same resolution as the settings load: operator override, the
        // extension `project_trust` decision, then the trust store/policy.
        let trusted_project =
            crate::extensions::project_trust::resolve_project_trust(&self.cwd, &self.agent_dir);
        let model_json = serde_json::to_value(model).unwrap_or(JsonValue::Null);
        let context_window = model.context_window;
        // The session-derived payload is the expensive half (context
        // projection, full entry tree, per-entry projection copy, session
        // listing). Its fingerprint is computed FIRST so a hit can skip the
        // catalog/tool-surface rebuilds below as well; the runtime and the
        // plugin host already hold exactly the previous push's state.
        let history = match &self.session {
            Some(_) => None,
            None => Some(self.history_snapshot()),
        };
        let mut fingerprint = self
            .snapshot_fingerprint(
                model_json.clone(),
                trusted_project,
                &history,
                cached_context_tokens,
            )
            .await;
        // One cache lock decides the session-snapshot hit and takes every
        // sub-cache (session listing, tree, projection fragments, catalog
        // stamp, tool-surface key) plus the shared probe counters.
        //
        // The FRAGMENTS are TAKEN, not cloned: the cache entry is replaced at
        // the end of this push, so the old set has no future reader. A clone
        // would pin the Arc at refcount 2 for the whole push and force the
        // extend's structural clone on EVERY append (measured: +200k
        // allocs/push on the 8k probe) — `try_unwrap` must see refcount 1.
        let (
            session_snapshot_matches,
            cached_sessions,
            cached_tree,
            cached_fragments,
            cached_catalog,
            cached_tool_surface,
            stats,
        ) = {
            let mut cache = self
                .session_context_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match cache.as_mut() {
                Some(cache) => (
                    cache.matches(&fingerprint, &history),
                    if cache.sessions.matches(
                        fingerprint.sessions_dir.as_deref(),
                        fingerprint.sessions_modified,
                    ) {
                        Some(cache.sessions.clone())
                    } else {
                        None
                    },
                    cache.tree.clone(),
                    cache.fragments.take(),
                    cache.catalog.clone(),
                    cache.tool_surface.clone(),
                    Arc::clone(&cache.stats),
                ),
                None => (
                    false,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Arc::new(ModelCatalogPushStats::default()),
                ),
            }
        };

        // Providers with a usable credential back the `pi.setModel` `false`
        // result (upstream returns false for unauthenticated providers), and
        // the model catalog backs `ctx.modelRegistry`/`modelRuntime`. Both
        // halves are cached by fingerprint: the ~1.7k-model `to_value`
        // serialization and the `load_pi_environment` settings/models/auth
        // reread only happen when the registry identity or one of the
        // credential files actually moved. A session-snapshot hit therefore
        // skips them too (R1-L1-008).
        let catalog_fingerprint = self.model_catalog_fingerprint(trusted_project);
        let (catalog_cache, catalog_rebuilt) = match cached_catalog {
            Some(cache) if cache.fingerprint == catalog_fingerprint => (Some(cache), false),
            _ => {
                let (catalog, credentials_ok) = self.build_model_catalog(&stats);
                catalog.apply_owned(runtime);
                // A transient credential-environment failure must not be
                // cached as a permanent empty auth list (P3-新-3): keep no
                // stamp so the next push retries. `apply_owned` still pushes
                // the catalog models with the permissive-empty convention.
                (
                    credentials_ok.then_some(ModelCatalogCache {
                        fingerprint: catalog_fingerprint,
                    }),
                    true,
                )
            }
        };

        // Host-pushed tool defs so the JS `getActiveTools`/`getAllTools`
        // facades see non-extension tools WITH their real description and
        // parameter schema: the session's core (builtin) tools first, then
        // every Rust-backend tool of the unified plugin registry (`subagent`,
        // `tool_search`, `python`, `codemode`, MCP-discovered `mcp__*` rows).
        // First name wins with CORE first — the same precedence
        // `merge_plugin_tools` applies when the run registry is built, so a
        // plugin can never shadow a core tool in the JS view either.
        // JS-extension tools are skipped: they already ride the QuickJS
        // registry export (`tool_defs`), and pushing them here would
        // duplicate their rows. Cached on the plugin registry generation, so
        // a steady prompt does not clone every def/schema again.
        let tool_surface_key = self.tool_surface_cache_key();
        let (tool_surface_cache, tool_surface_rebuilt) = match cached_tool_surface {
            Some(cache) if cache.key == tool_surface_key => (cache, false),
            _ => {
                self.push_tool_surface(runtime);
                (
                    ToolSurfaceCache {
                        key: tool_surface_key,
                    },
                    true,
                )
            }
        };

        // `ctx.modelRegistry.complete` host path: the completion closure built
        // for the session's current model (`__pir_host.modelCompleteJson`),
        // covering builtin/models.json providers the extension runtime cannot
        // stream itself. The session-scoped variant resolves the provider (and
        // OAuth credential) through this session's overlay instead of the
        // process-shared dynamic map (SNAP9 P2-S8-4). `actor_hooks: false`
        // omits the INLINE transport hooks that would re-enter the (suspended)
        // extension actor from inside the host op. Re-pushed before every
        // prompt, so a model switch is visible to the op.
        //
        // R1: the host op serializes the completion's message straight to the
        // extension (`CompleteOk { ok: true, message }`), a consumer face the
        // agent loop's take never reaches — a completion runs outside any
        // turn, so nothing takes the internal `pir_retry_class` carrier a
        // failed provider finalizer attached. Strip it at this seam, before
        // the message is handed to the runtime: the payload is carrier-free
        // while the real error text is unchanged.
        runtime.set_complete_fn(
            model.provider.as_str(),
            model.id.as_str(),
            without_retry_class_carrier(self.complete_fn_for_host_op_session_scoped(model)),
        );
        // The live session-only model scope (upstream `ctx.scopedModels`), not
        // an always-empty list: extension handlers read it to mirror the
        // host's model restriction.
        runtime.update_scoped_models(serialize_scoped_models(&self.scoped_models()));
        runtime.set_thinking_level_value(self.current_thinking().wire_key());
        runtime.set_system_prompt_value(&self.effective_system_prompt());

        if session_snapshot_matches {
            // The catalog/tool caches can invalidate independently of the
            // session-derived payload (an auth.json edit, a provider
            // registration); persist their fresh stamps so the next push
            // reuses instead of rebuilding a second time.
            if catalog_rebuilt || tool_surface_rebuilt {
                let mut cache = self
                    .session_context_cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(cache) = cache.as_mut() {
                    if catalog_rebuilt {
                        // `None` clears a stale stamp after a failed
                        // credential load, forcing a retry next push.
                        cache.catalog = catalog_cache;
                    }
                    if tool_surface_rebuilt {
                        cache.tool_surface = Some(tool_surface_cache);
                    }
                }
            }
            return false;
        }
        // Cache miss: compute the live last-usage value once and use it for
        // BOTH the fingerprint being stored and the payload, so a usage change
        // and a rebuilt snapshot always agree. The counter is the P3-01
        // regression seam: a hit returns above and must not move it.
        CONTEXT_USAGE_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
        let context_tokens = self
            .context_usage_with_projection(prebuilt_projection)
            .await
            .and_then(|usage| usage.tokens);
        fingerprint.context_tokens = context_tokens.unwrap_or(0);

        let (snapshot, sessions, tree, fragments) = self
            .build_session_context_snapshot(SessionSnapshotInputs {
                model: model_json,
                context_window,
                trusted_project,
                context_tokens,
                sessions_dir: fingerprint.sessions_dir.clone(),
                sessions_modified: fingerprint.sessions_modified,
                cached_sessions,
                cached_tree,
                cached_fragments,
                prebuilt_projection,
            })
            .await;
        // Unified push: with a plugin host (always present alongside an
        // extension runtime) the host forwards the snapshot to the JS handle
        // it owns AND fans it out to the Rust plugin list
        // (`AgentPlugin::on_session_context`, the `tool-repair` builtin's model
        // reach). The direct runtime push is the no-host fallback — a single
        // JS push either way, so `ctx` state never sees a partial snapshot.
        match &self.plugins {
            Some(plugins) => plugins.update_session_context(snapshot),
            None => runtime.update_session_context(snapshot),
        }
        *self
            .session_context_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(SessionContextCache {
            fingerprint,
            history,
            sessions,
            tree,
            fragments,
            catalog: catalog_cache,
            tool_surface: Some(tool_surface_cache),
            stats,
        });
        true
    }

    /// No-JS-runtime half of [`Self::push_session_context_with_projection`]:
    /// build and push the COMPLETE snapshot through the same fingerprint
    /// cache the runtime path uses, but only to the Rust plugin list
    /// ([`crate::plugins::PluginHost::update_session_context_native_only`]).
    /// The runtime-only pushes (catalog, tool surface, scoped models, `ctx`
    /// completion hooks) are skipped — this session has no runtime that owns
    /// them. This is the whole plugin contract for a builtin/inline-only
    /// session, and it shares the JS path's payload so todo replay, memory
    /// session binding and `tool-repair`'s model gate see identical values.
    async fn push_session_context_native_only(
        &self,
        model: &Model,
        prebuilt_projection: Option<&crate::session_manager::SessionProjection>,
    ) -> bool {
        let Some(plugins) = &self.plugins else {
            return false;
        };
        let cached_context_tokens = self
            .session_context_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|cache| cache.fingerprint.context_tokens);
        let trusted_project =
            crate::extensions::project_trust::resolve_project_trust(&self.cwd, &self.agent_dir);
        let model_json = serde_json::to_value(model).unwrap_or(JsonValue::Null);
        let context_window = model.context_window;
        let history = match &self.session {
            Some(_) => None,
            None => Some(self.history_snapshot()),
        };
        let mut fingerprint = self
            .snapshot_fingerprint(
                model_json.clone(),
                trusted_project,
                &history,
                cached_context_tokens,
            )
            .await;
        let (session_snapshot_matches, cached_sessions, cached_tree, cached_fragments, stats) = {
            let mut cache = self
                .session_context_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match cache.as_mut() {
                Some(cache) => (
                    cache.matches(&fingerprint, &history),
                    if cache.sessions.matches(
                        fingerprint.sessions_dir.as_deref(),
                        fingerprint.sessions_modified,
                    ) {
                        Some(cache.sessions.clone())
                    } else {
                        None
                    },
                    cache.tree.clone(),
                    // Taken, not cloned — see the runtime path above: the
                    // extend's `try_unwrap` needs refcount 1.
                    cache.fragments.take(),
                    Arc::clone(&cache.stats),
                ),
                None => (
                    false,
                    None,
                    None,
                    None,
                    Arc::new(ModelCatalogPushStats::default()),
                ),
            }
        };
        if session_snapshot_matches {
            // The plugin host already holds this exact payload; a second
            // push would only repeat the fan-out.
            return false;
        }
        // Same P3-01 ordering as the runtime path: the fingerprint check is
        // cheap, the usage walk is paid once on a miss and shared by both
        // the stored fingerprint and the payload.
        CONTEXT_USAGE_CALLS.with(|calls| calls.set(calls.get().saturating_add(1)));
        let context_tokens = self
            .context_usage_with_projection(prebuilt_projection)
            .await
            .and_then(|usage| usage.tokens);
        fingerprint.context_tokens = context_tokens.unwrap_or(0);
        let (snapshot, sessions, tree, fragments) = self
            .build_session_context_snapshot(SessionSnapshotInputs {
                model: model_json,
                context_window,
                trusted_project,
                context_tokens,
                sessions_dir: fingerprint.sessions_dir.clone(),
                sessions_modified: fingerprint.sessions_modified,
                cached_sessions,
                cached_tree,
                cached_fragments,
                prebuilt_projection,
            })
            .await;
        plugins.update_session_context_native_only(snapshot);
        *self
            .session_context_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(SessionContextCache {
            fingerprint,
            history,
            sessions,
            tree,
            fragments,
            // This host has no JS runtime face, so there is no catalog or
            // tool-surface state to stamp.
            catalog: None,
            tool_surface: None,
            stats,
        });
        true
    }

    /// Test/profiling seam: push the `ctx.sessionManager` snapshot for the
    /// current model and report whether the session-derived payload was
    /// rebuilt (`false` = the fingerprint cache reused the snapshot the
    /// runtime/plugin host already holds).
    #[doc(hidden)]
    pub async fn push_session_context_for_test(&self) -> bool {
        let model = self.current_model();
        self.push_session_context(&model).await
    }

    /// Test/profiling seam for the prompt-loop projection handoff: push the
    /// snapshot using an already materialized projection; the push must not
    /// build another one.
    #[doc(hidden)]
    pub async fn push_session_context_with_projection_for_test(
        &self,
        projection: &crate::session_manager::SessionProjection,
    ) -> bool {
        let model = self.current_model();
        self.push_session_context_with_projection(&model, Some(projection))
            .await
    }

    /// Test seam for the P3-01 cache-hit short circuit: how many
    /// `context_usage()` walks `push_session_context` paid for on the calling
    /// thread since the last reset. A cache hit must not move it.
    #[doc(hidden)]
    pub fn session_context_usage_calls_for_test(&self) -> u64 {
        CONTEXT_USAGE_CALLS.with(|calls| calls.get())
    }

    /// Reset [`Self::session_context_usage_calls_for_test`].
    #[doc(hidden)]
    pub fn reset_session_context_usage_calls_for_test(&self) {
        CONTEXT_USAGE_CALLS.with(|calls| calls.set(0));
    }

    /// Test seam: run `sync_plugin_providers` (the per-prompt plugin provider
    /// pass) exactly like `run_prompt` does, so a test can observe that a
    /// re-sync with unchanged definitions does not invalidate the catalog
    /// fingerprint.
    #[doc(hidden)]
    pub async fn sync_plugin_providers_for_test(&self) {
        if let Some(plugins) = &self.plugins {
            self.sync_plugin_providers(plugins).await;
        }
    }

    /// Test seam: rebuild the plugin registry (the `/reload` definition
    /// refresh) and then run the per-prompt provider sync, so a test can flip
    /// a plugin `pi.registerProvider` definition between syncs.
    #[doc(hidden)]
    pub async fn refresh_plugin_registry_and_sync_providers_for_test(&self) {
        if let Some(plugins) = &self.plugins {
            plugins.rebuild_registry().await;
            self.sync_plugin_providers(plugins).await;
        }
    }

    /// Test seam: whether THIS session's `extension_api_keys` map holds a key
    /// installed by `sync_plugin_providers` for `provider`. A subagent child
    /// shares its parent's `ProviderRegistry` but must install its own copy;
    /// the registry alone cannot answer this (SNAP3 P2-1).
    #[doc(hidden)]
    pub fn extension_api_key_installed_for_test(&self, provider: &str) -> bool {
        self.extension_api_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(provider)
    }

    /// Test seam: the plugin API key THIS session installed for `provider`.
    /// Shared-registry children must keep their own value; the map is
    /// per-session even though the registry is not (SNAP3 P2-1, SNAP4).
    #[doc(hidden)]
    pub fn extension_api_key_for_test(&self, provider: &str) -> Option<String> {
        self.extension_api_keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider)
            .cloned()
    }

    /// Test/profiling probe for the R1-L1-008 cache:
    /// `(catalog registry pushes, model rows pushed,
    /// credential-environment loads)` paid since the last reset.
    ///
    /// The first two counters live at `ExtensionHandle::update_model_registry`
    /// — the common sink every catalog rebuild must reach — so a regression
    /// that rebuilds the catalog OUTSIDE `build_model_catalog` is still
    /// counted (P2-新-2). The credential counter lives in
    /// `authenticated_providers`, the session's only `load_pi_environment`
    /// caller for this path. A cache hit must leave all three at zero.
    #[doc(hidden)]
    pub fn model_catalog_push_stats(&self) -> (u64, u64, u64) {
        let (catalog_pushes, model_rows) = self
            .extensions
            .as_ref()
            .map(ExtensionHandle::model_registry_push_stats_for_test)
            .unwrap_or((0, 0));
        let credential_environment_loads = self
            .session_context_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|cache| {
                cache
                    .stats
                    .credential_environment_loads
                    .load(std::sync::atomic::Ordering::Relaxed)
            })
            .unwrap_or(0);
        (catalog_pushes, model_rows, credential_environment_loads)
    }

    /// Reset [`Self::model_catalog_push_stats`].
    #[doc(hidden)]
    pub fn reset_model_catalog_push_stats(&self) {
        if let Some(runtime) = &self.extensions {
            runtime.reset_model_registry_push_stats_for_test();
        }
        if let Some(cache) = self
            .session_context_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            cache
                .stats
                .credential_environment_loads
                .store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The cheap fingerprint described on [`SnapshotFingerprint`].
    async fn snapshot_fingerprint(
        &self,
        model: JsonValue,
        project_trusted: bool,
        history: &Option<HistorySnapshot>,
        context_tokens: Option<u64>,
    ) -> SnapshotFingerprint {
        let (
            session_cwd,
            entry_count,
            last_entry_id,
            leaf_id,
            sessions_dir,
            session_id,
            session_path,
        ) = match &self.session {
            Some(session) => {
                let session = session.lock().await;
                // Borrowed resident slice: no entry clone, no tree walk.
                let resident = session.resident_entries();
                let last_entry_id = resident
                    .iter()
                    .rev()
                    .find(|entry| !entry.is_header())
                    .map(|entry| entry.entry_id().to_owned());
                let dir = session
                    .path()
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("."));
                (
                    session.cwd().to_owned(),
                    resident.len(),
                    last_entry_id,
                    session.leaf_id().map(str::to_owned),
                    Some(dir),
                    // The MANAGER's identity, not the facade's cached copy:
                    // `create_branched_session` rewrites the storage in place
                    // (new id + file) without refreshing the facade cells, and
                    // a branch that keeps the same entries would otherwise
                    // fingerprint as unchanged.
                    Some(session.id().to_owned()),
                    session.session_file().map(Path::to_path_buf),
                )
            }
            None => (
                self.cwd.display().to_string(),
                history.as_ref().map_or(0, |messages| messages.len()),
                None,
                None,
                None,
                self.session_id(),
                self.session_path(),
            ),
        };
        // Filesystem metadata is read AFTER the session lock is released: a
        // slow/network path must never serialize every session operation
        // behind a stat call. Same value as the in-lock read.
        let sessions_modified = sessions_dir.as_deref().and_then(|dir| {
            std::fs::metadata(dir)
                .and_then(|metadata| metadata.modified())
                .ok()
        });
        SnapshotFingerprint {
            model,
            session_id,
            session_path,
            session_cwd,
            entry_count,
            last_entry_id,
            leaf_id,
            context_tokens: context_tokens.unwrap_or(0),
            pending_messages: self.pending_messages.load(Ordering::SeqCst),
            project_trusted,
            sessions_dir,
            sessions_modified,
        }
    }

    /// Build the session-derived snapshot payload (the expensive half), the
    /// `sessions` listing cache entry, and the rendered-tree cache entry that
    /// go with it.
    async fn build_session_context_snapshot(
        &self,
        inputs: SessionSnapshotInputs<'_>,
    ) -> (
        SessionContextSnapshot,
        CachedSessions,
        Option<Arc<CachedTree>>,
        Option<CachedProjectionFragments>,
    ) {
        let SessionSnapshotInputs {
            model,
            context_window,
            trusted_project,
            context_tokens,
            sessions_dir,
            sessions_modified,
            cached_sessions,
            cached_tree,
            cached_fragments,
            prebuilt_projection,
        } = inputs;
        // Session-manager snapshot extensions: header, branch, full tree,
        // leaf, labels, and the session projection backing the new
        // `ctx.sessionManager` methods. ONE session lock covers all of it; the
        // `sessions` directory listing below runs after it is released.
        let (
            entries,
            cwd_value,
            header,
            branch,
            tree_json,
            leaf_id,
            labels,
            projection,
            tree_cache,
            fragments_cache,
        ) = match &self.session {
            Some(session) => {
                let session = session.lock().await;
                let cwd_value = Some(session.cwd().to_string());
                let header = serde_json::to_value(session.header()).unwrap_or(JsonValue::Null);
                // Borrowed branch TEXT (PIR-SDK-027): `branch_entries` is this
                // same path walk followed by a deep clone of every entry, and
                // the old `to_value` then materialized a second transcript-
                // sized `Value`. Serializing the borrowed entries into one
                // pre-rendered string produces identical JSON bytes with no
                // intermediate value tree (the manager's `Arc<FileEntry>` list
                // stays shared).
                let branch = {
                    let path = crate::session_manager::projection::build_session_path(
                        session.resident_entries(),
                        session.leaf_id(),
                    );
                    let mut text = String::with_capacity(64 + path.len() * 256);
                    text.push('[');
                    let mut first = true;
                    for entry in &path {
                        if first {
                            first = false;
                        } else {
                            text.push(',');
                        }
                        text.push_str(&json_text_or_null(*entry));
                    }
                    text.push(']');
                    text
                };
                // Labels are read off the BORROWED resident entry list (no
                // clone — the old code paid a whole-session `entries()`
                // clone just for this map). `tree()` below internally
                // calls `entries()` and deep-clones each entry into its
                // node; the public API has no borrowed forest builder, so
                // that copy is unavoidable (documented residual).
                let mut labels_map = serde_json::Map::new();
                for entry in session.resident_entries() {
                    if entry.is_header() {
                        continue;
                    }
                    if let Some(id) = entry.id() {
                        if let Some(label) = session.get_label(id) {
                            labels_map.insert(id.to_string(), JsonValue::String(label));
                        }
                    }
                }
                // Nested forest with the upstream `SessionTreeNode`
                // contract (session-manager.ts:199-207: entry/children/
                // label/labelTimestamp) — the SAME builder the RPC
                // `get_tree` answers with (session_manager::
                // SessionManager::tree), so the JS `getTree()`/
                // `getEntry()`/`getLeafEntry()` facades walk real nodes.
                //
                // The snapshot carries the tree as PRE-RENDERED JSON TEXT:
                // the forest nests one level per entry, and a deeply
                // nested `Value` both serializes and DROPS with one stack
                // frame per level (serde_json has no stack-safe path for
                // either), which overran a 2 MiB thread on a 2000-entry
                // session. `getSessionManagerJson` splices this text into
                // the reply, so the JS side still parses the same nested
                // array.
                //
                // The rendered text is cached ACROSS rebuilds (keyed on the
                // storage identity + resident entry count + leaf id) and
                // kept as a SPLIT structure (per-node head/children/tail),
                // so the usual rebuild — an append of new entries — splices
                // only the new nodes' text and re-renders by concatenation:
                // no `tree()` deep clone and no re-serialization of any
                // existing entry. A structural change (fork/branch, label
                // rewrite, trim, paged widen) or a cache miss falls back to
                // the full `forest_to_json_iterative` render.
                let tree_key = TreeCacheKey {
                    session_id: session.id().to_owned(),
                    session_path: session.session_file().map(Path::to_path_buf),
                    entry_count: session.resident_entries().len(),
                    leaf_id: session.leaf_id().map(str::to_owned),
                };
                // The tree cache consumes its key (it becomes the extended
                // cache entry's key); the fragment cache below re-derives the
                // SAME key (same inputs, same value) and the fragment cache
                // entry stores the clone.
                let fragment_key = tree_key.clone();
                let (tree_json, tree_cache) = match cached_tree {
                    Some(cached) if cached.key == tree_key => (cached.text.clone(), Some(cached)),
                    Some(cached)
                        if cached.incremental_ok
                            && cached.key.session_id == tree_key.session_id
                            && cached.key.session_path == tree_key.session_path
                            && tree_key.entry_count > cached.key.entry_count =>
                    {
                        // Prefix continuity: walk the resident entries
                        // backwards and stop at the sentinel — the entries
                        // passed must be exactly the append delta.
                        let sentinel = cached.last_node_id.clone();
                        let delta_target = tree_key.entry_count - cached.key.entry_count;
                        let mut new_entries: Vec<&crate::session_manager::FileEntry> = Vec::new();
                        let mut prefix_ok = false;
                        for entry in session.resident_entries().iter().rev() {
                            if entry.is_header() {
                                // Headers only ever sit at the file front, so
                                // one encountered mid-delta means the
                                // resident list was rewritten: fall back.
                                if new_entries.len() < delta_target {
                                    break;
                                }
                                prefix_ok = true;
                                break;
                            }
                            if entry.entry_id() == sentinel.as_str() {
                                prefix_ok = new_entries.len() == delta_target;
                                break;
                            }
                            if new_entries.len() >= delta_target {
                                // More non-header entries than the count
                                // delta explains (rewritten list): fall back.
                                prefix_ok = false;
                                break;
                            }
                            new_entries.push(entry.as_ref());
                        }
                        // A label entry changes an EXISTING node's rendered
                        // label fields, which the splice cannot patch; the
                        // full render handles it.
                        let has_label_entry = new_entries.iter().any(|entry| {
                            matches!(entry, crate::session_manager::FileEntry::Label(_))
                        });
                        if prefix_ok && !has_label_entry && !new_entries.is_empty() {
                            new_entries.reverse();
                            let resolve_label =
                                |id: &str| (session.get_label(id), session.label_timestamp_for(id));
                            // The Arc is this push's private copy unless a
                            // concurrent reader holds the previous snapshot's
                            // clone; `unwrap_or_clone` mutates in place in the
                            // common case and clones once (the old full-render
                            // cost) when shared.
                            let mut cached = match Arc::try_unwrap(cached) {
                                Ok(cached) => cached,
                                Err(shared) => (*shared).clone(),
                            };
                            match extend_cached_forest(&mut cached, &new_entries, &resolve_label) {
                                Some(()) => {
                                    let last_id = new_entries
                                        .last()
                                        .map(|entry| entry.entry_id().to_owned())
                                        .unwrap_or_else(|| cached.last_node_id.clone());
                                    cached.key = tree_key;
                                    cached.node_count += new_entries.len();
                                    cached.last_node_id = last_id;
                                    let text = render_cached_forest(&cached);
                                    cached.text = text.clone();
                                    (text, Some(Arc::new(cached)))
                                }
                                // Structural surprise: full render.
                                None => full_render_tree(tree_key, &session),
                            }
                        } else {
                            full_render_tree(tree_key, &session)
                        }
                    }
                    _ => full_render_tree(tree_key, &session),
                };
                let leaf_id = session.leaf_id().map(str::to_owned);
                // ONE projection build feeds both `entries` and
                // `projection`: `build_context_entries()` is defined as
                // the projection's `messages`, so the old second walk over
                // the leaf path + message materialization was pure
                // duplication.
                //
                // ROUND 2 (PIR-SDK-027): the per-entry message views are
                // additionally CACHED as owned `JsonValue` fragments. The
                // same key the tree cache uses (storage identity + resident
                // entry count + leaf id) describes the retained-entry LIST:
                // an append adds views at the end and never rewrites old
                // ones, so the cached fragments extend by the appended
                // views; a fork/branch/label/trim/rewrite (key mismatch,
                // count shrink, or sentinel break) falls back to a full
                // fragment build. The aggregate `entries` value is assembled
                // from the SAME fragments (one array move), so the push
                // never re-serializes a retained entry a second time.
                let (entries, projection, fragments_cache) = {
                    // Resolve the extension against the cached set BEFORE
                    // building: the fragments must describe the retained
                    // entry list the projection will walk. The sentinel is
                    // the cached set's last retained entry id — a plain
                    // append keeps it exactly `appended_from` positions
                    // back in the resident list.
                    let cached_extension = cached_fragments.and_then(|cached| {
                        // The FRAGMENT count (projection entries cached) is the
                        // append boundary the splice uses; the resident count
                        // includes the session header, so it is one higher.
                        let cached_views = cached.fragments.fragments.len();
                        if cached_views == 0 {
                            // An empty projection extends by everything.
                            return Some((cached.fragments, 0usize));
                        }
                        if fragment_key.session_id != cached.key.session_id
                            || fragment_key.session_path != cached.key.session_path
                            || fragment_key.entry_count < cached.key.entry_count
                        {
                            return None;
                        }
                        // Sentinel continuity over the resident list: exactly
                        // the entries after the cached last id must be the
                        // append delta (headers only at the front). The delta
                        // is RESIDENT-level; the fragment-level delta is the
                        // same (headers never project).
                        let resident = session.resident_entries();
                        let sentinel_index = resident.iter().rposition(|entry| {
                            !entry.is_header() && entry.entry_id() == cached.last_entry_id.as_str()
                        })?;
                        let appended = fragment_key.entry_count - cached.key.entry_count;
                        if resident.len() - 1 - sentinel_index != appended {
                            return None;
                        }
                        Some((cached.fragments, cached_views))
                    });
                    match prebuilt_projection {
                        // The prompt run already built this projection for its
                        // context; serialize it instead of rebuilding the whole
                        // transcript for the snapshot. A concurrent append between
                        // that build and this lock makes the projection stale, so
                        // fall back unless its leaf is still the live leaf.
                        Some(projection) if projection_leaf_matches(&session, projection) => {
                            let (entries, text, fragments) =
                                serialize_projection_snapshot(projection, cached_extension);
                            (
                                entries,
                                text,
                                CachedProjectionFragments {
                                    key: fragment_key.clone(),
                                    fragments: Arc::new(fragments),
                                    last_entry_id: resident_last_entry_id(&session),
                                },
                            )
                        }
                        _ => match session.build_session_projection() {
                            Ok(projection) => {
                                let (entries, text, fragments) =
                                    serialize_projection_snapshot(&projection, cached_extension);
                                (
                                    entries,
                                    text,
                                    CachedProjectionFragments {
                                        key: fragment_key.clone(),
                                        fragments: Arc::new(fragments),
                                        last_entry_id: resident_last_entry_id(&session),
                                    },
                                )
                            }
                            Err(_) => (
                                Arc::new(JsonValue::Null),
                                String::from("null"),
                                CachedProjectionFragments {
                                    key: fragment_key.clone(),
                                    fragments: Arc::new(
                                        crate::session_manager::ProjectionEntryFragments {
                                            fragments: Vec::new(),
                                            aggregate: None,
                                            aggregate_text: None,
                                            projection_json: None,
                                        },
                                    ),
                                    last_entry_id: resident_last_entry_id(&session),
                                },
                            ),
                        },
                    }
                };
                (
                    entries,
                    cwd_value,
                    header,
                    branch,
                    tree_json,
                    leaf_id,
                    JsonValue::Object(labels_map),
                    projection,
                    tree_cache,
                    Some(fragments_cache),
                )
            }
            None => (
                Arc::new(
                    serde_json::to_value(self.history_snapshot().as_slice())
                        .unwrap_or(JsonValue::Null),
                ),
                Some(self.cwd.display().to_string()),
                JsonValue::Null,
                String::from("null"),
                String::new(),
                None,
                JsonValue::Object(serde_json::Map::new()),
                String::from("null"),
                None,
                None,
            ),
        };
        // The session list is cached separately from the fingerprint-driven
        // rebuild: the usual rebuild (new entries after a run) does not
        // invalidate the listing, and its directory mtime is part of the
        // fingerprint so an added/removed session still forces a miss. On a
        // miss the directory/header scan is blocking I/O and runs on the
        // blocking pool — this async path (the per-prompt snapshot push) must
        // not stall a Tokio worker.
        let (sessions, sessions_cache) = match &self.session {
            Some(_) => match cached_sessions {
                Some(cached) => (cached.value.clone(), cached),
                None => {
                    let dir = sessions_dir.clone().unwrap_or_else(|| PathBuf::from("."));
                    let summaries = tokio::task::spawn_blocking(move || {
                        crate::session_manager::SessionManager::list_sessions(&dir, None)
                    })
                    .await
                    .unwrap_or_default();
                    let rows: Vec<JsonValue> = summaries
                        .iter()
                        .map(|summary| {
                            serde_json::json!({
                                "id": summary.id,
                                "path": summary.path.display().to_string(),
                                "cwd": summary.cwd,
                                "name": summary.name,
                                "parentSessionPath": summary.parent_session_path,
                                "messageCount": summary.message_count,
                                "firstMessage": summary.first_message,
                                "modifiedMs": summary
                                    .modified
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|duration| duration.as_millis() as u64)
                                    .unwrap_or(0),
                            })
                        })
                        .collect();
                    let value = JsonValue::Array(rows);
                    (
                        value.clone(),
                        CachedSessions {
                            dir: sessions_dir,
                            modified: sessions_modified,
                            value,
                        },
                    )
                }
            },
            None => (
                JsonValue::Null,
                CachedSessions {
                    dir: None,
                    modified: None,
                    value: JsonValue::Null,
                },
            ),
        };
        let snapshot = SessionContextSnapshot {
            model,
            entries,
            sessions,
            session_id: self.session_id(),
            session_path: self
                .session_path()
                .as_ref()
                .map(|path| path.display().to_string()),
            // The last-usage value computed by `push_session_context` (the
            // same value the fingerprint used); `None`/zero means unknown.
            context_tokens: context_tokens.filter(|tokens| *tokens > 0),
            context_window: Some(context_window),
            has_pending_messages: self.pending_messages.load(Ordering::SeqCst) > 0,
            project_trusted: trusted_project,
            cwd: cwd_value,
            header,
            branch_json: branch,
            tree_json,
            leaf_id,
            labels,
            projection_json: projection,
        };
        (snapshot, sessions_cache, tree_cache, fragments_cache)
    }

    /// The cheap fingerprint described on [`ModelCatalogFingerprint`]. Reads
    /// only provider identities/names, virtual-model identities, the
    /// refresh-capable and plugin-owned dynamic providers' current model
    /// definitions, the plugin provider definitions' config hashes, four
    /// file stamps (with content hashes) and the auth-relevant environment —
    /// no catalog serialization and no settings/models/auth parse.
    fn model_catalog_fingerprint(&self, trusted_project: bool) -> ModelCatalogFingerprint {
        // `pi.registerProvider` definitions are re-applied (and their dynamic
        // `Arc` rebuilt) on every prompt by `sync_plugin_providers`. Their
        // address is therefore NOT an identity; the definition config hash
        // plus the resulting model content are (P2-新-1).
        let plugin_definitions = plugin_provider_definitions(self.plugins.as_deref());
        let dynamic_ids: BTreeSet<String> = self
            .registry
            .dynamic
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        let providers =
            self.registry
                .providers()
                .iter()
                .map(|provider| {
                    let plugin_config = dynamic_ids
                        .contains(provider.id())
                        .then(|| plugin_definitions.get(provider.id()).copied())
                        .flatten();
                    // Catalog and dynamic providers are immutable snapshots; a
                    // runtime registration replaces the `Arc`, and a remote
                    // overlay update swaps the dynamic-map entry. The address
                    // therefore identifies the exact model list without cloning
                    // it. Plugin-owned and in-place-refresh providers use their
                    // model content instead.
                    ProviderCatalogFingerprint {
                        id: provider.id().to_owned(),
                        name: provider.name().to_owned(),
                        identity: if plugin_config.is_some() {
                            0
                        } else {
                            Arc::as_ptr(provider) as *const () as usize
                        },
                        plugin_config,
                        models_content: (provider.supports_refresh() || plugin_config.is_some())
                            .then(|| {
                                provider
                                    .get_models()
                                    .iter()
                                    .map(|model| hash_bytes(format!("{model:?}").as_bytes()))
                                    .collect()
                            }),
                    }
                })
                .collect();
        let virtual_models = self
            .registry
            .virtual_models()
            .iter()
            .map(|registration| VirtualModelFingerprint {
                provider: registration.provider.clone(),
                id: registration.id.clone(),
                physical_provider: registration.physical.provider.clone(),
                physical_id: registration.physical.id.clone(),
                name: registration.name.clone(),
                reasoning: registration.reasoning,
                context_window: registration
                    .model_override
                    .and_then(|overrides| overrides.context_window),
                max_tokens: registration
                    .model_override
                    .and_then(|overrides| overrides.max_tokens),
            })
            .collect();
        ModelCatalogFingerprint {
            providers,
            virtual_models,
            credentials: credential_environment_fingerprint(&self.cwd, trusted_project),
        }
    }

    /// Serialize the live catalog and credential environment (the expensive
    /// half of the push). Returns whether the credential environment loaded:
    /// on failure the caller must NOT store the catalog fingerprint, so the
    /// next push retries instead of caching a permanent empty auth list
    /// (P3-新-3).
    fn build_model_catalog(&self, stats: &ModelCatalogPushStats) -> (ModelCatalog, bool) {
        let (authenticated, credentials_ok) = match self.authenticated_providers(stats) {
            Ok(providers) => (providers, true),
            Err(()) => (Vec::new(), false),
        };
        let catalog =
            ModelCatalog::from_registry(&self.registry).with_authenticated_providers(authenticated);
        (catalog, credentials_ok)
    }

    /// Cheap identity described on [`ToolSurfaceCacheKey`].
    fn tool_surface_cache_key(&self) -> ToolSurfaceCacheKey {
        ToolSurfaceCacheKey {
            plugin_registry_generation: self
                .plugins
                .as_ref()
                .map(|plugins| plugins.registry_generation()),
            tools_identity: Arc::as_ptr(&self.tools) as usize,
        }
    }

    /// Push the core tool definitions and the builtin (Rust plugin) commands
    /// into the runtime (cache-miss half of [`Self::tool_surface_cache_key`]).
    fn push_tool_surface(&self, runtime: &ExtensionHandle) {
        let mut defs: Vec<crate::extensions::ToolDef> =
            crate::coding_agent::tools::tool_definitions(&self.tools)
                .into_iter()
                .map(|tool| {
                    crate::extensions::ToolDef::core(tool.name, tool.description, tool.parameters)
                })
                .collect();
        if let Some(plugins) = &self.plugins {
            let mut seen: std::collections::BTreeSet<String> =
                defs.iter().map(|def| def.name.clone()).collect();
            for (def, backend) in plugins.registry().resolved_tools() {
                if matches!(backend, crate::plugins::ToolBackend::Rust(_))
                    && seen.insert(def.name.clone())
                {
                    defs.push(def.clone());
                }
            }
        }
        runtime.update_core_tools(defs);
        // Builtin (Rust plugin) commands join `getCommands()` alongside the
        // extension-registered ones (all entries carry the upstream `source`
        // tag).
        if let Some(plugins) = &self.plugins {
            let builtin_commands: Vec<crate::extensions::CommandDef> = plugins
                .registry()
                .resolved_commands()
                .into_iter()
                .filter(|command| {
                    matches!(
                        command.backend,
                        Some(crate::plugins::CommandBackend::Rust(_))
                    )
                })
                .map(|command| command.def)
                .collect();
            runtime.update_builtin_commands(builtin_commands);
        }
    }

    /// Provider ids with a usable credential (models.json key, provider
    /// environment variable, or stored `auth.json` credential). `Err` when
    /// the environment cannot be loaded; the catalog is still pushed with an
    /// empty (permissive) list, but the caller must not cache the result
    /// (P3-新-3). The counter lives here so any catalog path that resolves
    /// credentials is counted, not just [`Self::build_model_catalog`]
    /// (P2-新-2).
    fn authenticated_providers(&self, stats: &ModelCatalogPushStats) -> Result<Vec<String>, ()> {
        stats
            .credential_environment_loads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match crate::ai::providers::load_pi_environment(Some(&self.cwd)) {
            Ok(environment) => {
                // Providers can come from THREE places: the session registry
                // (plugin-registered), the models.json-composed environment
                // registry (providers defined only in `models.json` never
                // appear in the session registry), and resolved models.json
                // API keys. Enumerating only the session registry made a
                // models.json-only credential invisible to
                // `hasConfiguredAuth`, so the JS answer depended on the
                // permissive-empty fallback.
                let mut authenticated: Vec<String> = Vec::new();
                for provider in environment
                    .registry
                    .providers()
                    .iter()
                    .chain(self.registry.providers().iter())
                {
                    let id = provider.id();
                    if !authenticated.iter().any(|known| known == id)
                        && crate::ai::providers::has_configured_auth(id, &environment.api_keys)
                    {
                        authenticated.push(id.to_owned());
                    }
                }
                Ok(authenticated)
            }
            Err(_) => Err(()),
        }
    }

    /// Take the previous `resources_discover` contribution (leaving the cell
    /// empty) so the caller can retract and/or re-merge it.
    fn take_extension_resource_contribution(&self) -> ExtensionResourceContribution {
        std::mem::take(
            &mut *self
                .extension_resource_contribution
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Retract the previous contribution when a refresh contributes NOTHING
    /// (no handler, a failed dispatch, or an empty path set): the resources it
    /// added stop expanding, exactly like a reload that removed them upstream.
    /// A session that never received a contribution is untouched (no prompt
    /// rebuild).
    fn retract_previous_extension_resources(
        &self,
        report: &mut crate::extensions::resources::ExtensionResourceReload,
    ) {
        let previous = self.take_extension_resource_contribution();
        if previous.is_empty() {
            return;
        }
        let mut skills = self
            .skills
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut templates = self
            .prompt_templates
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        previous.retract(&mut skills, &mut templates);
        for (name, _) in &previous.skills {
            report
                .diagnostics
                .push(format!("skill \"{name}\" retracted by resources_discover"));
        }
        for (name, _) in &previous.prompt_templates {
            report
                .diagnostics
                .push(format!("prompt \"{name}\" retracted by resources_discover"));
        }
        self.set_prompt_resources_shared(skills, templates);
    }

    /// `resources_discover` (startup/reload): load the returned
    /// `skillPaths` / `promptPaths` / `themePaths` into the session's resource
    /// set (upstream `extendResources`, resource-loader.ts:493-528).
    ///
    /// Skills and prompt templates merge FIRST-WINS into the installed sets
    /// (a name the host already holds is kept; the contribution is reported
    /// as a collision), then
    /// [`AgentSession::set_prompt_resources_shared`] refreshes `/skill:`
    /// expansion and the derived prompt's skills section. The previous
    /// dispatch's own contribution is retracted first, so a repeated dispatch
    /// (a reload) is authoritative for the resources it contributed: a
    /// changed skill file, a rename, or a removed path takes effect. Theme
    /// files register in the process theme registry (the `tui` feature's
    /// registry; a non-TUI build reports that no consumer exists). Every
    /// load/collision warning goes to the diagnostic log and the returned
    /// report.
    pub(super) async fn emit_resources_discover(
        &self,
        plugins: &PluginHost,
        reason: &str,
    ) -> crate::extensions::resources::ExtensionResourceReload {
        use crate::extensions::resources::{
            load_extension_resources, merge_prompt_templates, merge_skills, register_theme_files,
            ExtensionResourceReload,
        };

        let mut report = ExtensionResourceReload::default();
        // Upstream `ResourcesDiscoverEvent` carries the session cwd
        // (extensions/types.ts:724-729) so handlers resolve relative skill /
        // prompt / theme paths against the same directory the host loads from.
        let Some(paths) = dispatch_resources_discover_event(plugins, &self.cwd, reason).await
        else {
            self.retract_previous_extension_resources(&mut report);
            return report;
        };
        let loaded = load_extension_resources(&self.cwd, &paths);
        report.diagnostics.extend(loaded.diagnostics);
        report.theme_files = loaded.theme_files.len();

        let mut skills = self
            .skills
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut templates = self
            .prompt_templates
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // Retract the PREVIOUS contribution first: this dispatch is
        // authoritative for the resources it contributes, so a reload whose
        // skill/template changed on disk (same name, same path) or disappeared
        // takes effect instead of being shadowed by the first-wins merge
        // (upstream `resourceLoader.reload()` rebuilds the resource set).
        let had_previous_contribution = {
            let contribution = self
                .extension_resource_contribution
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let had_previous = !contribution.is_empty();
            contribution.retract(&mut skills, &mut templates);
            had_previous
        };
        let skills_added_at = skills.len();
        merge_skills(&mut skills, loaded.skills, &mut report.diagnostics);
        report.skills = skills.len().saturating_sub(skills_added_at);

        let templates_added_at = templates.len();
        merge_prompt_templates(
            &mut templates,
            loaded.prompt_templates,
            &mut report.diagnostics,
        );
        report.prompt_templates = templates.len().saturating_sub(templates_added_at);
        // Record exactly the entries this dispatch added (a collision with a
        // base resource is NOT ours, so it is never retracted by a later
        // reload). `merge_*` only appends, so the tail is the added set.
        let contributed = ExtensionResourceContribution {
            skills: skills[skills_added_at..]
                .iter()
                .map(|skill| (skill.name.clone(), skill.path.clone()))
                .collect(),
            prompt_templates: templates[templates_added_at..]
                .iter()
                .map(|template| (template.name.clone(), template.file_path.clone()))
                .collect(),
        };
        *self
            .extension_resource_contribution
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = contributed;

        if !loaded.theme_files.is_empty() {
            report
                .diagnostics
                .extend(register_theme_files(&loaded.theme_files));
        }
        for diagnostic in &report.diagnostics {
            crate::ext_diag!("[ext] resources_discover: {diagnostic}");
        }
        // Reinstall when anything moved: an added resource, or a retraction
        // alone (a contributed resource removed by a reload) — both must reach
        // `/skill:` expansion and the prompt's skills section. A dispatch that
        // changed nothing keeps the current install (no prompt rebuild).
        if had_previous_contribution || report.skills > 0 || report.prompt_templates > 0 {
            self.set_prompt_resources_shared(skills, templates);
        }
        report
    }

    /// Re-run `resources_discover` against the live plugin host and install
    /// the contributed resources (the session half of `/reload`).
    ///
    /// Unlike the startup dispatch this is callable at any time; it is
    /// idempotent (first-wins merges) and never fails — dispatch/load warnings
    /// ride the returned report and the diagnostic log. The extension runtime
    /// itself is NOT reloaded here: call
    /// [`crate::extensions::ExtensionHandle::reload`] first when factories
    /// must re-run, then this.
    pub async fn reload_extension_resources(
        &self,
    ) -> crate::extensions::resources::ExtensionResourceReload {
        match &self.plugins {
            Some(plugins) => self.emit_resources_discover(plugins, "reload").await,
            None => crate::extensions::resources::ExtensionResourceReload::default(),
        }
    }
}

/// Identity of every file [`crate::ai::providers::load_pi_environment`] reads
/// for the session's credential state. The loader resolves the GLOBAL agent
/// dir through the process environment (same as `auth.json` reads in
/// `has_configured_auth`), so the stamps use that same directory; the project
/// settings scope is included because a malformed project settings file turns
/// the whole load into an empty authenticated list.
fn credential_environment_fingerprint(
    cwd: &Path,
    project_trusted: bool,
) -> CredentialEnvironmentFingerprint {
    let agent_dir = crate::ai::providers::agent_dir();
    // Every env var `has_configured_auth` consults ...
    let mut env_names: BTreeSet<String> = crate::ai::auth::env_api_keys::all()
        .into_iter()
        .flat_map(|(_, names)| names.iter().map(|name| (*name).to_owned()))
        .collect();
    let files: Vec<CredentialFileStamp> = [
        agent_dir.join("settings.json"),
        agent_dir.join("models.json"),
        agent_dir.join("auth.json"),
        cwd.join(".pi").join("settings.json"),
    ]
    .into_iter()
    .map(|path| {
        // `auth.json` holds opaque stored credentials, never `$ENV`
        // templates; scanning it would only yield bogus names from tokens.
        let scan_env = path.file_name().and_then(|name| name.to_str()) != Some("auth.json");
        let (stamp, references) = credential_file_stamp(path, scan_env);
        // ... plus every `$ENV` / `${ENV}` template reference in the files
        // themselves (models.json apiKey/headers, project settings values).
        if scan_env {
            env_names.extend(references);
        }
        stamp
    })
    .collect();
    let env = env_names
        .into_iter()
        .map(|name| {
            let value = std::env::var_os(&name).map(|value| hash_os_str(&value));
            (name, value)
        })
        .collect();
    CredentialEnvironmentFingerprint {
        agent_dir,
        cwd: cwd.to_path_buf(),
        project_trusted,
        files,
        env,
    }
}

/// `(mtime, size, content hash)` of one credential file; all `None` when it
/// does not exist. The content hash catches an equal-length rewrite that
/// lands inside the filesystem timestamp granularity (P3-新-2). The returned
/// strings are the `$ENV` / `${ENV}` names referenced by the file, for the
/// environment half of the fingerprint.
fn credential_file_stamp(path: PathBuf, scan_env: bool) -> (CredentialFileStamp, Vec<String>) {
    match std::fs::metadata(&path) {
        Ok(metadata) => {
            let content = std::fs::read(&path).ok();
            let content_hash = content.as_deref().map(hash_bytes);
            let mut references = Vec::new();
            if scan_env {
                if let Some(bytes) = &content {
                    if let Ok(text) = std::str::from_utf8(bytes) {
                        collect_env_references(text, &mut references);
                    }
                }
            }
            (
                CredentialFileStamp {
                    path,
                    modified: metadata.modified().ok(),
                    len: Some(metadata.len()),
                    content_hash,
                },
                references,
            )
        }
        Err(_) => (
            CredentialFileStamp {
                path,
                modified: None,
                len: None,
                content_hash: None,
            },
            Vec::new(),
        ),
    }
}

/// `DefaultHasher` over arbitrary bytes. Only compared in-process (the
/// fingerprint never leaves the session cache), so hash stability across
/// compiler versions does not matter.
fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn hash_os_str(value: &std::ffi::OsStr) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Collect `$NAME` / `${NAME}` template identifiers from a credential file.
/// Conservative: only ASCII identifiers starting with a letter or `_`, and a
/// braced reference must be closed. Values are never stored; the caller keeps
/// a hash of each referenced variable's current value.
fn collect_env_references(text: &str, out: &mut Vec<String>) {
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'$' {
            index += 1;
            continue;
        }
        let braced = bytes.get(index + 1) == Some(&b'{');
        let start = index + if braced { 2 } else { 1 };
        let mut end = start;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        let valid_start = bytes
            .get(start)
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_');
        if valid_start && end > start && (!braced || bytes.get(end) == Some(&b'}')) {
            if let Ok(name) = std::str::from_utf8(&bytes[start..end]) {
                out.push(name.to_owned());
            }
        }
        index = end.max(index + 1);
    }
}

/// One `{id, config}` `pi.registerProvider` definition: id → config JSON
/// hash. Last definition wins, matching `sync_plugin_providers`' sequential
/// upsert.
fn plugin_provider_definitions(
    plugins: Option<&crate::plugins::PluginHost>,
) -> BTreeMap<String, u64> {
    let Some(plugins) = plugins else {
        return BTreeMap::new();
    };
    let mut definitions = BTreeMap::new();
    for def in plugins.registry().provider_defs() {
        let Some(id) = def.get("id").and_then(JsonValue::as_str) else {
            continue;
        };
        let config = def.get("config").cloned().unwrap_or(JsonValue::Null);
        definitions.insert(id.to_owned(), hash_json(&config));
    }
    definitions
}

fn hash_json(value: &JsonValue) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(value)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

/// Wrap the `ctx.modelRegistry.complete` host closure so its returned message
/// never carries the internal retry-class diagnostic.
///
/// The host op answers `{ok: true, message: AssistantMessage}` with the message
/// serialized AS-IS to the extension. A completion runs outside an agent turn,
/// so the loop's take (which strips the carrier on the turn's terminal message)
/// never sees it; without this wrapper a failed completion — and any marker a
/// foreign provider planted — would reach extension code. Stripping here keeps
/// the payload clean while leaving `error_message` and every other field
/// byte-identical.
fn without_retry_class_carrier(complete: Arc<CompleteFn>) -> Arc<CompleteFn> {
    Arc::new(Box::new(move |context| {
        let complete = Arc::clone(&complete);
        Box::pin(async move {
            let mut message = complete(context).await?;
            crate::ai::error::strip_turn_retry_class(&mut message);
            Ok(message)
        })
    }))
}

/// Split a rendered structured prompt into its untagged preamble and the
/// first tagged section (`preamble` is the only untagged section; the builder
/// joins sections with a blank line). A prompt without a tagged section
/// answers `(prompt, "")`.
fn split_prompt_head(prompt: &str) -> (&str, &str) {
    match prompt.find("\n\n<") {
        Some(index) => (&prompt[..index], &prompt[index + 2..]),
        None => (prompt, ""),
    }
}

/// The content of one XML-wrapped section (`<tag>\ncontent\n</tag>`) in a
/// rendered structured prompt.
fn extract_tagged_section(prompt: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>\n");
    let start = prompt.find(&open)? + open.len();
    let rest = &prompt[start..];
    let close = format!("\n</{tag}>");
    let end = rest.find(&close)?;
    Some(rest[..end].to_owned())
}

/// The hidden declarations the prompt's `hiddenTools` list carries: the
/// union of every declared tool's Rust `prepare_loadout` result (upstream
/// `_rebuildSystemPrompt`'s `hiddenTools: [..._hiddenDeclarations]`). Mirrors
/// the session-build derivation so the options object and the rendered prompt
/// agree (JS `prepareLoadout` hooks are async and stay out of the synchronous
/// snapshot, exactly like the session build).
fn hidden_declarations_for(
    registry: &crate::agent::tool_execution::ToolRegistry,
    active: &[String],
) -> Vec<String> {
    let declared = crate::agent::tool_execution::declared_tools(registry, active);
    if declared.is_empty() {
        return Vec::new();
    }
    let callable = crate::agent::tool_execution::callable_tools(registry, active);
    let mut exposures = std::collections::BTreeMap::new();
    let mut namespaces = std::collections::BTreeMap::new();
    for tool in registry.iter() {
        exposures.insert(tool.name().to_owned(), tool.exposure());
        if let Some(namespace) = tool.namespace() {
            namespaces.insert(tool.name().to_owned(), namespace);
        }
    }
    let loadout = crate::agent::tool_execution::ToolLoadout {
        declared,
        callable,
        registered: registry.iter().cloned().collect(),
        exposures,
        namespaces,
    };
    let mut hidden = std::collections::BTreeSet::new();
    for tool in &loadout.declared {
        if let Some(changes) = tool.prepare_loadout(&loadout) {
            hidden.extend(changes.hidden_declarations);
        }
    }
    hidden.into_iter().collect()
}
