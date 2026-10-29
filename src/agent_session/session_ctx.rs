use super::*;

use std::time::SystemTime;

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
struct TreeCacheKey {
    session_id: String,
    session_path: Option<PathBuf>,
    entry_count: usize,
    leaf_id: Option<String>,
}

/// The pre-rendered forest text plus the key it was built from. Reusing it
/// turns a snapshot rebuild that did not touch the entries (a token/model
/// change between the pre- and post-run pushes) from an O(session size)
/// `tree()` clone + serialization into a string copy. `Arc` keeps the cache
/// read a refcount bump instead of a full-text copy.
struct CachedTree {
    key: TreeCacheKey,
    text: String,
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
    /// Push the live session/model snapshot backing the `ctx` facade.
    ///
    /// Returns whether the session-derived snapshot was rebuilt; `false` means
    /// the previous push's fingerprint still describes the live state, so the
    /// runtime/plugin host already holds the identical snapshot and the push
    /// is skipped.
    pub(super) async fn push_session_context(&self, model: &Model) -> bool {
        let Some(runtime) = &self.extensions else {
            // No JS runtime, but the plugin list still receives the model
            // snapshot: a builtin/inline-only session is exactly where the
            // `tool-repair` builtin's `before_provider_request` model gate has
            // no other source for the model id. The JS-facing pushes below are
            // all runtime-only, so this branch is the whole plugin contract.
            if let Some(plugins) = &self.plugins {
                plugins.update_session_context(model_snapshot(model));
            }
            return false;
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
        // duplicate their rows.
        {
            let mut defs: Vec<crate::extensions::ToolDef> =
                crate::coding_agent::tools::tool_definitions(&self.tools)
                    .into_iter()
                    .map(|tool| {
                        crate::extensions::ToolDef::core(
                            tool.name,
                            tool.description,
                            tool.parameters,
                        )
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
        }
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
        // Providers with a usable credential back the `pi.setModel` `false`
        // result (upstream returns false for unauthenticated providers), and
        // the model catalog backs `ctx.modelRegistry`/`modelRuntime`. Same
        // serialized shape the pre-load push uses
        // (`ModelCatalog::apply`), so a factory-time lookup and a
        // post-session lookup can never disagree.
        ModelCatalog::from_registry(&self.registry)
            .with_authenticated_providers(self.authenticated_providers())
            .apply(runtime);
        // Same resolution as the settings load: operator override, the
        // extension `project_trust` decision, then the trust store/policy.
        let trusted_project =
            crate::extensions::project_trust::resolve_project_trust(&self.cwd, &self.agent_dir);
        let model_json = serde_json::to_value(model).unwrap_or(JsonValue::Null);
        let context_window = model.context_window;
        // `ctx.modelRegistry.complete` host path: the completion closure built
        // for the session's current model (`__pir_host.modelCompleteJson`),
        // covering builtin/models.json providers the extension runtime cannot
        // stream itself. `complete_fn_for_host_op` omits the INLINE transport
        // hooks that would re-enter the (suspended) extension actor from
        // inside the host op. Re-pushed before every prompt, so a model switch
        // is visible to the op.
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
            without_retry_class_carrier(self.complete_fn_for_host_op(model)),
        );
        // The live session-only model scope (upstream `ctx.scopedModels`), not
        // an always-empty list: extension handlers read it to mirror the
        // host's model restriction.
        runtime.update_scoped_models(serialize_scoped_models(&self.scoped_models()));
        runtime.set_thinking_level_value(self.current_thinking().wire_key());
        runtime.set_system_prompt_value(&self.effective_system_prompt());

        // The session-derived payload is the expensive half (context
        // projection, full entry tree, per-entry projection copy, session
        // listing). Skip it when nothing it reflects has moved since the last
        // push: the extension runtime and the plugin host still hold exactly
        // that snapshot, so a repeated push would be a no-op.
        let history = match &self.session {
            Some(_) => None,
            None => Some(self.history_snapshot()),
        };
        let fingerprint = self
            .snapshot_fingerprint(model_json.clone(), trusted_project, &history)
            .await;
        let (cached_sessions, cached_tree) = match self
            .session_context_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            Some(cache) if cache.matches(&fingerprint, &history) => return false,
            Some(cache) => (
                if cache.sessions.matches(
                    fingerprint.sessions_dir.as_deref(),
                    fingerprint.sessions_modified,
                ) {
                    Some(cache.sessions.clone())
                } else {
                    None
                },
                cache.tree.clone(),
            ),
            None => (None, None),
        };

        let (snapshot, sessions, tree) = self
            .build_session_context_snapshot(
                model_json,
                context_window,
                trusted_project,
                fingerprint.sessions_dir.clone(),
                fingerprint.sessions_modified,
                cached_sessions,
                cached_tree,
            )
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

    /// The cheap fingerprint described on [`SnapshotFingerprint`].
    async fn snapshot_fingerprint(
        &self,
        model: JsonValue,
        project_trusted: bool,
        history: &Option<HistorySnapshot>,
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
            context_tokens: self.tokens.load(Ordering::SeqCst),
            pending_messages: self.pending_messages.load(Ordering::SeqCst),
            project_trusted,
            sessions_dir,
            sessions_modified,
        }
    }

    /// Build the session-derived snapshot payload (the expensive half), the
    /// `sessions` listing cache entry, and the rendered-tree cache entry that
    /// go with it.
    #[allow(clippy::too_many_arguments)]
    async fn build_session_context_snapshot(
        &self,
        model: JsonValue,
        context_window: u64,
        trusted_project: bool,
        sessions_dir: Option<PathBuf>,
        sessions_modified: Option<SystemTime>,
        cached_sessions: Option<CachedSessions>,
        cached_tree: Option<Arc<CachedTree>>,
    ) -> (
        SessionContextSnapshot,
        CachedSessions,
        Option<Arc<CachedTree>>,
    ) {
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
        ) = match &self.session {
            Some(session) => {
                let session = session.lock().await;
                let cwd_value = Some(session.cwd().to_string());
                let header = serde_json::to_value(session.header()).unwrap_or(JsonValue::Null);
                let branch = serde_json::to_value(session.branch_entries(None).unwrap_or_default())
                    .unwrap_or(JsonValue::Null);
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
                // The rendered text is cached across rebuilds keyed on the
                // storage identity + resident entry count + leaf id: a
                // rebuild caused by a scalar change (tokens, model, trust)
                // reuses it instead of paying `tree()`'s deep entry clone
                // and the forest serialization again.
                let tree_key = TreeCacheKey {
                    session_id: session.id().to_owned(),
                    session_path: session.session_file().map(Path::to_path_buf),
                    entry_count: session.resident_entries().len(),
                    leaf_id: session.leaf_id().map(str::to_owned),
                };
                let (tree_json, tree_cache) = match cached_tree {
                    Some(cached) if cached.key == tree_key => (cached.text.clone(), Some(cached)),
                    _ => {
                        let text = crate::session_manager::TreeNode::forest_to_json_iterative(
                            session.tree(),
                        );
                        (
                            text.clone(),
                            Some(Arc::new(CachedTree {
                                key: tree_key,
                                text,
                            })),
                        )
                    }
                };
                let leaf_id = session.leaf_id().map(str::to_owned);
                // ONE projection build feeds both `entries` and
                // `projection`: `build_context_entries()` is defined as
                // the projection's `messages`, so the old second walk over
                // the leaf path + message materialization was pure
                // duplication. The aggregate message list is serialized
                // ONCE here; `entries` and `projection.messages` share
                // that value and each per-entry slice clones the already
                // built values instead of re-serializing the messages.
                let projection = session.build_session_projection();
                let (entries, projection) = match &projection {
                    Ok(projection) => {
                        let message_values =
                            serde_json::to_value(&projection.messages).unwrap_or(JsonValue::Null);
                        match &message_values {
                            JsonValue::Array(message_items) => {
                                let entry_views: Vec<JsonValue> = projection
                                    .entries
                                    .iter()
                                    .map(|entry| {
                                        let end = entry.messages.end.min(message_items.len());
                                        let start = entry.messages.start.min(end);
                                        let mut view = serde_json::Map::new();
                                        view.insert(
                                            String::from("entry"),
                                            serde_json::to_value(entry.source_entry.as_ref())
                                                .unwrap_or(JsonValue::Null),
                                        );
                                        view.insert(
                                            String::from("messages"),
                                            JsonValue::Array(message_items[start..end].to_vec()),
                                        );
                                        JsonValue::Object(view)
                                    })
                                    .collect();
                                let mut projection_map = serde_json::Map::new();
                                projection_map
                                    .insert(String::from("entries"), JsonValue::Array(entry_views));
                                projection_map
                                    .insert(String::from("messages"), message_values.clone());
                                (message_values, JsonValue::Object(projection_map))
                            }
                            // `to_value` only fails on non-string map keys
                            // (`Message` has none); keep the old
                            // Null fallback if it ever does.
                            _ => (JsonValue::Null, JsonValue::Null),
                        }
                    }
                    Err(_) => (JsonValue::Null, JsonValue::Null),
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
                )
            }
            None => (
                serde_json::to_value(self.history_snapshot().as_slice()).unwrap_or(JsonValue::Null),
                Some(self.cwd.display().to_string()),
                JsonValue::Null,
                JsonValue::Null,
                String::new(),
                None,
                JsonValue::Object(serde_json::Map::new()),
                JsonValue::Null,
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
            context_tokens: {
                let total = self.tokens.load(Ordering::SeqCst);
                if total > 0 {
                    Some(total)
                } else {
                    None
                }
            },
            context_window: Some(context_window),
            has_pending_messages: self.pending_messages.load(Ordering::SeqCst) > 0,
            project_trusted: trusted_project,
            cwd: cwd_value,
            header,
            branch,
            tree_json,
            leaf_id,
            labels,
            projection,
        };
        (snapshot, sessions_cache, tree_cache)
    }

    /// Provider ids with a usable credential (models.json key, provider
    /// environment variable, or stored `auth.json` credential). Empty when
    /// the environment cannot be loaded (the runtime then stays permissive).
    fn authenticated_providers(&self) -> Vec<String> {
        match crate::ai::providers::load_pi_environment(Some(&self.cwd)) {
            Ok(environment) => self
                .registry
                .providers()
                .iter()
                .filter(|provider| {
                    crate::ai::providers::has_configured_auth(provider.id(), &environment.api_keys)
                })
                .map(|provider| provider.id().to_owned())
                .collect(),
            Err(_) => Vec::new(),
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
            ExtensionResourcePaths, ExtensionResourceReload,
        };

        let mut report = ExtensionResourceReload::default();
        if !plugins.has_handlers("resources_discover") {
            self.retract_previous_extension_resources(&mut report);
            return report;
        }
        // Upstream `ResourcesDiscoverEvent` carries the session cwd
        // (extensions/types.ts:724-729) so handlers resolve relative skill /
        // prompt / theme paths against the same directory the host loads from.
        let mut payload = serde_json::json!({
            "type": "resources_discover",
            "reason": reason,
            "cwd": self.cwd.display().to_string(),
        });
        let outcome = match plugins.emit("resources_discover", &mut payload).await {
            Ok(outcome) => outcome,
            Err(error) => {
                crate::ext_diag!("[ext] resources_discover dispatch failed: {error}");
                self.retract_previous_extension_resources(&mut report);
                return report;
            }
        };
        // Errors inside handlers are isolated by the merge layer; a broken
        // handler must not silently disable the contributions of the others.
        for error in &outcome.errors {
            crate::ext_diag!(
                "[ext] resources_discover handler error ({}): {}",
                error.event,
                error.error
            );
        }
        let paths = ExtensionResourcePaths::from_event_result(&outcome.result, &self.cwd);
        if paths.is_empty() {
            self.retract_previous_extension_resources(&mut report);
            return report;
        }
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

/// The model-only snapshot pushed when a session has no JS extension runtime
/// (builtin/inline plugins only): the plugin list needs the model id
/// (`AgentPlugin::on_session_context`) and nothing else from the JS-facing
/// snapshot, whose session/tree payloads exist for the `ctx` facade.
fn model_snapshot(model: &Model) -> SessionContextSnapshot {
    SessionContextSnapshot {
        model: serde_json::to_value(model).unwrap_or(JsonValue::Null),
        ..SessionContextSnapshot::default()
    }
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
