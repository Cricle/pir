use super::*;

// ---------------------------------------------------------------------------
// Extension command-context session actions (upstream print-mode.ts:78-100 /
// rpc-mode.ts:322-344 `commandContextActions`)
// ---------------------------------------------------------------------------

/// One session-control action requested by an extension command handler
/// through the `ctx` surface (upstream `ExtensionCommandContextActions`,
/// extensions/types.ts:2208-2228: `ctx.newSession` / `ctx.fork` /
/// `ctx.navigateTree` / `ctx.switchSession` / `ctx.reload`).
///
/// The JS prelude forwards every call to the `commandAction` host function,
/// which records the parsed action here; the session host drains and applies
/// them against its session-control plane after the command handler settles
/// (the print host's command path via
/// [`apply_session_command_to_storage`], an [`AgentSession`]-backed host via
/// [`AgentSession::apply_session_command_actions`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCommandAction {
    /// `ctx.newSession({ parentSession? })`: swap in fresh session storage
    /// (upstream `runtimeHost.newSession`). `with_session` is the runtime
    /// token of the caller's `withSession` callback
    /// ([`ExtensionHandle::call_session_callback`](crate::extensions::ExtensionHandle::call_session_callback)),
    /// run once the replacement session is live (upstream
    /// `finishSessionReplacement`).
    NewSession {
        parent_session: Option<String>,
        with_session: Option<u64>,
    },
    /// `ctx.fork(entryId?, {position?})`: branch the live storage at an
    /// entry (leaf move), or fork the current session into a new file when
    /// no entry id was given (upstream `runtimeHost.fork`; pir's fork-at-
    /// current also serves upstream's `clone`). `position` follows upstream
    /// `fork(entryId, {position})` (agent-session-runtime.ts:263-289):
    /// `"before"` (the upstream default) branches at a user message's
    /// PARENT and yields its text as `selectedText`; `"at"` branches at the
    /// entry itself.
    Fork {
        entry_id: Option<String>,
        position: Option<String>,
        /// The caller's `withSession` callback token (see
        /// [`SessionCommandAction::NewSession`]'s field).
        with_session: Option<u64>,
    },
    /// `ctx.navigateTree(targetId, options)`: move the leaf, optionally
    /// summarizing the abandoned branch (upstream `session.navigateTree`).
    NavigateTree {
        target_id: String,
        summarize: bool,
        custom_instructions: Option<String>,
        replace_instructions: bool,
        label: Option<String>,
    },
    /// `ctx.switchSession(sessionPath)`: open another session file and swap
    /// it in as the live storage (upstream `runtimeHost.switchSession`).
    SwitchSession {
        session_path: String,
        /// The caller's `withSession` callback token (see
        /// [`SessionCommandAction::NewSession`]'s field).
        with_session: Option<u64>,
    },
    /// `ctx.reload()`: reload extensions, native plugins, tools, skills,
    /// prompts and themes in place (upstream `session.reload`,
    /// `core/extensions/types.ts:433-434`). Applied through
    /// [`AgentSession::reload`], which keeps the current session and
    /// transcript. A storage-only host (print mode) has no resource surface,
    /// so its apply path reports that instead of pretending.
    Reload,
}

/// What [`AgentSession::reload`] reloaded (upstream `session.reload()` returns
/// nothing; the report exists so hosts and tests can tell the halves apart).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SessionReloadOutcome {
    /// The JS extension runtime was rebuilt and its factories re-ran. `false`
    /// when the session has no extension file paths to reload (builtin-only
    /// plan, no extensions).
    pub extensions_reloaded: bool,
    /// The native plugin lifecycle ran (`session_shutdown {reason:"reload"}`,
    /// every `on_reload` hook, one registry rebuild).
    pub native_plugins_reloaded: bool,
    /// What the fresh `resources_discover` dispatch installed (and retracted).
    pub resources: crate::extensions::resources::ExtensionResourceReload,
}

/// Host identity stamped onto every recorded command-context action: each
/// session host claims only the actions stamped with ITS identity, so a host
/// can never apply (or drain) another host's recorded actions (upstream hosts
/// consume their own `commandContextActions`; the queue used to be anonymous,
/// which let a print host steal an AgentSession host's actions and left
/// AgentSession hosts with zero consumers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOrigin {
    /// The print host (one-shot CLI process; one shared session storage).
    Print,
    /// An AgentSession-backed host (TUI / RPC / server / SDK): the live
    /// session id the host pushed to the runtime (`ctx.sessionId`); `None`
    /// for ephemeral (no session storage) runs.
    AgentSession { session_id: Option<String> },
}

impl SessionOrigin {
    /// The origin a `commandAction` host fn stamps actions with, derived from
    /// the runtime's configured host mode and the pushed session id. The
    /// print host's runtime says mode `print`; every other host is
    /// AgentSession-backed (unknown modes resolve to the AgentSession
    /// identity so their actions are never claimed by a print host).
    pub fn for_runtime_mode(mode: &str, session_id: Option<String>) -> Self {
        match mode {
            "print" => SessionOrigin::Print,
            _ => SessionOrigin::AgentSession { session_id },
        }
    }
}

/// One queued command-context action plus the host identity that owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCommandActionEntry {
    pub origin: SessionOrigin,
    pub action: SessionCommandAction,
}

/// Process-global queue of extension command-context actions recorded by the
/// `commandAction` host function ([`record_session_command_action`]) and
/// consumed by the session host ([`take_session_command_actions_for`]). Every
/// entry carries the originating host's [`SessionOrigin`]. The queue is
/// bounded: above the cap the OLDEST entry is dropped (the newest request per
/// control surface wins, like a user repeating the request).
static SESSION_COMMAND_ACTIONS: Mutex<Vec<SessionCommandActionEntry>> = Mutex::new(Vec::new());

/// Queue cap for recorded command-context actions: a host that never drains
/// must not grow memory unboundedly.
const SESSION_COMMAND_ACTION_CAP: usize = 64;

/// Record one extension command-context action for the OWNING session host to
/// apply (called by the `commandAction` host function inside the JS runtime,
/// stamped with the runtime's host identity).
pub fn record_session_command_action(origin: SessionOrigin, action: SessionCommandAction) {
    let mut queue = SESSION_COMMAND_ACTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if queue.len() >= SESSION_COMMAND_ACTION_CAP {
        queue.remove(0);
    }
    queue.push(SessionCommandActionEntry { origin, action });
}

/// Take every recorded extension command-context action, in request order
/// (leaves the queue empty). UNFILTERED: tests and full drains only — session
/// hosts claim their own actions through [`take_session_command_actions_for`].
pub fn take_session_command_actions() -> Vec<SessionCommandAction> {
    std::mem::take(
        &mut *SESSION_COMMAND_ACTIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
    .into_iter()
    .map(|entry| entry.action)
    .collect()
}

/// Take the recorded actions owned by `origin`, in request order. Actions
/// belonging to OTHER hosts stay queued for their owner — a consumer can
/// neither apply nor silently lose another host's recorded actions.
pub fn take_session_command_actions_for(origin: &SessionOrigin) -> Vec<SessionCommandActionEntry> {
    let mut queue = SESSION_COMMAND_ACTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut *queue)
        .into_iter()
        .partition(|entry| &entry.origin == origin);
    *queue = rest;
    mine
}

/// Rejection for a `withSession` action applied by a STORAGE-ONLY host.
///
/// The callback is a JS function registered in the extension runtime; running
/// it needs a host that owns that runtime and can push the replacement
/// session's context (an [`AgentSession`]-backed host). A print-mode host has
/// raw session storage but no replacement-session callback plane, so the
/// action is refused BEFORE the storage is touched — the honest outcome, and
/// the caller's session is not half-replaced.
fn with_session_needs_session_host(action: &str) -> String {
    format!(
        "{action}({{ withSession }}) requires an AgentSession-backed host: the callback is a JS \
         function that must be invoked with the replacement session's context, which this \
         storage-level host cannot provide. The session was not replaced."
    )
}

/// Provider binding for `ctx.navigateTree({ summarize: true })` applied at
/// the storage level (the print host has no [`AgentSession`]): the routed
/// model, its completion function, and the branch-summary token reserve
/// ([`AgentSession::navigate_tree`] uses the session's own binding).
pub struct NavigateSummaryBinding {
    pub model: Model,
    pub complete: Arc<crate::compaction::CompleteFn>,
    pub reserve_tokens: u64,
}

/// Apply one extension command-context action against raw session storage
/// (the print host's `SharedSession`): new/fork/switch replace the storage,
/// fork-at-entry and navigateTree move the leaf in place. `None` storage
/// (`--no-session`) rejects every action like the upstream runtimeHost
/// ("no attached session storage"). Summarized navigation requires the
/// `navigate_summary` binding; without one it is rejected.
///
/// This entry point applies everything in one call. A caller that shares its
/// storage behind a lock (the print host) uses the split sequence instead —
/// [`prepare_session_command_to_storage`] under the lock,
/// [`generate_navigation_summary`] WITHOUT it, then
/// [`commit_navigation_to_storage`] under the lock — so the provider call
/// never blocks other storage users.
pub async fn apply_session_command_to_storage(
    storage: &mut Option<SessionManager>,
    session_dir: &Path,
    action: &SessionCommandAction,
    navigate_summary: Option<&NavigateSummaryBinding>,
) -> Result<(), String> {
    match prepare_session_command_to_storage(storage, session_dir, action, navigate_summary)? {
        SessionCommandStep::Applied => Ok(()),
        SessionCommandStep::Navigate(plan) => {
            let Some(binding) = navigate_summary else {
                return Err(
                    "navigateTree(summarize) requires a provider-backed session host".to_owned(),
                );
            };
            let summary = generate_navigation_summary(&plan, binding).await?;
            commit_navigation_to_storage(storage, plan, summary)
        }
    }
}

/// What applying an action under the caller's storage lock produced.
pub enum SessionCommandStep {
    /// The action was applied synchronously (or rejected with `Err`); the
    /// storage is already in its final state.
    Applied,
    /// `navigateTree(summarize)`: the abandoned branch was collected, but the
    /// storage was NOT mutated. The caller must drop the lock, summarize with
    /// [`generate_navigation_summary`], then re-lock and
    /// [`commit_navigation_to_storage`] (which re-checks the leaf).
    Navigate(PreparedNavigateTree),
}

/// A `navigateTree(summarize)` plan captured while the storage lock is held.
/// It owns the collected (abandoned) entries, so the summarizer call can run
/// with the lock released.
pub struct PreparedNavigateTree {
    pub target_id: String,
    pub old_leaf_id: Option<String>,
    pub new_leaf_id: Option<String>,
    pub entries: Vec<FileEntry>,
    pub custom_instructions: Option<String>,
    pub replace_instructions: bool,
    pub label: Option<String>,
    /// Session identity at capture time: a storage swap invalidates the plan
    /// even when the replacement happens to reuse the leaf id.
    session_id: String,
}

/// The summarizer output handed to [`commit_navigation_to_storage`].
pub struct NavigateSummaryOutcome {
    pub summary: Option<String>,
    pub details: Option<JsonValue>,
    pub usage: Option<crate::ai::types::Usage>,
}

/// Apply one action against raw session storage under the caller's lock.
///
/// Every action except `navigateTree(summarize)` is synchronous and completes
/// here; `navigateTree(summarize)` returns [`SessionCommandStep::Navigate`]
/// with the branch entries collected and the leaf untouched, so the provider
/// call runs outside the lock.
pub fn prepare_session_command_to_storage(
    storage: &mut Option<SessionManager>,
    session_dir: &Path,
    action: &SessionCommandAction,
    navigate_summary: Option<&NavigateSummaryBinding>,
) -> Result<SessionCommandStep, String> {
    match action {
        SessionCommandAction::NewSession {
            parent_session,
            with_session,
        } => {
            if with_session.is_some() {
                return Err(with_session_needs_session_host("newSession"));
            }
            let cwd = storage
                .as_ref()
                .map(|session| session.cwd().to_owned())
                .or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|path| path.to_str().map(str::to_owned))
                })
                .unwrap_or_else(|| session_dir.display().to_string());
            let mut fresh = SessionManager::create(&cwd, session_dir)
                .map_err(|error| format!("new session failed: {error}"))?;
            if let Some(parent) = parent_session {
                fresh
                    .new_session(Some(crate::session_manager::NewSessionOptions {
                        id: None,
                        parent_session: Some(parent.clone()),
                    }))
                    .map_err(|error| format!("new session failed: {error}"))?;
            }
            *storage = Some(fresh);
            Ok(SessionCommandStep::Applied)
        }
        SessionCommandAction::SwitchSession {
            session_path,
            with_session,
        } => {
            if with_session.is_some() {
                return Err(with_session_needs_session_host("switchSession"));
            }
            let manager = SessionManager::open(session_path).map_err(|error| error.to_string())?;
            *storage = Some(manager);
            Ok(SessionCommandStep::Applied)
        }
        SessionCommandAction::Fork {
            entry_id,
            position,
            with_session,
        } => {
            if with_session.is_some() {
                return Err(with_session_needs_session_host("fork"));
            }
            let Some(session) = storage.as_ref() else {
                return Err("no attached session storage; the run stays ephemeral".to_owned());
            };
            let Some(entry_id) = entry_id else {
                // Fork at the current position: new file, full history copy
                // (upstream `runtimeHost.fork` at the leaf; pir's `clone`).
                let Some(source) = session.session_file().map(Path::to_path_buf) else {
                    return Err("cannot fork an in-memory session".to_owned());
                };
                let cwd = session.cwd().to_owned();
                let forked = SessionManager::fork_from_source(&source, &cwd, session_dir, None)
                    .map_err(|error| format!("fork failed: {error}"))?;
                *storage = Some(forked);
                return Ok(SessionCommandStep::Applied);
            };
            // Fork at an entry (upstream `fork(entryId, {position})`):
            // `"at"` branches at the entry itself; the default `"before"`
            // requires a user message and branches at its PARENT — a `None`
            // parent (fork before the first user message) starts a fresh
            // session with the current one as its `parentSession` link.
            // Note: the print host applies raw storage with no plugin
            // surface in reach, so no `session_before_fork` dispatch runs on
            // this path (the AgentSession host does, via
            // `AgentSession::fork_entry`).
            let session = storage
                .as_mut()
                .ok_or_else(|| "no attached session storage; the run stays ephemeral".to_owned())?;
            match (
                ForkPosition::parse(position.as_deref()),
                session.entry(entry_id),
            ) {
                (ForkPosition::At, Some(_)) => session
                    .branch(entry_id)
                    .map_err(|error| format!("fork failed: {error}"))?,
                (ForkPosition::At, None) => return Err("Invalid entry ID for forking".to_owned()),
                (ForkPosition::Before, Some(FileEntry::Message(inner)))
                    if inner.message.role() == "user" =>
                {
                    match inner.parent_id.clone() {
                        Some(parent) => session
                            .branch(&parent)
                            .map_err(|error| format!("fork failed: {error}"))?,
                        None => {
                            // Fork before the first user message: fresh
                            // session with the current one as its parent
                            // link (upstream `SessionManager.create` +
                            // `newSession({parentSession})`).
                            let cwd = session.cwd().to_owned();
                            let parent_file = session.session_file().map(Path::to_path_buf);
                            let mut fresh = SessionManager::create(&cwd, session_dir)
                                .map_err(|error| format!("fork failed: {error}"))?;
                            if let Some(parent) = parent_file {
                                fresh
                                    .new_session(Some(crate::session_manager::NewSessionOptions {
                                        id: None,
                                        parent_session: Some(parent.display().to_string()),
                                    }))
                                    .map_err(|error| format!("fork failed: {error}"))?;
                            }
                            *storage = Some(fresh);
                        }
                    }
                }
                (ForkPosition::Before, _) => return Err("Invalid entry ID for forking".to_owned()),
            }
            Ok(SessionCommandStep::Applied)
        }
        SessionCommandAction::NavigateTree {
            target_id,
            summarize,
            custom_instructions,
            replace_instructions,
            label,
        } => {
            let Some(session) = storage.as_mut() else {
                return Err("no attached session storage; the run stays ephemeral".to_owned());
            };
            prepare_navigate_storage_tree(
                session,
                target_id,
                *summarize,
                custom_instructions.as_deref(),
                *replace_instructions,
                label.as_deref(),
                navigate_summary,
            )
        }
        SessionCommandAction::Reload => Err(
            "ctx.reload() is not a session-storage action: a host with an extension runtime \
             reloads it through AgentSession::reload (the CLI /reload equivalent); this \
             storage-only host has no resource surface to reload"
                .to_owned(),
        ),
    }
}

/// Phase 1 of storage-level `navigateTree`: collect the abandoned branch, and
/// either move the leaf synchronously (no summary needed) or return the plan
/// for the caller to summarize outside the lock.
fn prepare_navigate_storage_tree(
    session: &mut SessionManager,
    target_id: &str,
    summarize: bool,
    custom_instructions: Option<&str>,
    replace_instructions: bool,
    label: Option<&str>,
    binding: Option<&NavigateSummaryBinding>,
) -> Result<SessionCommandStep, String> {
    use crate::compaction::branch_summarization::collect_entries_for_branch_summary;

    let old_leaf_id = session.leaf_id().map(str::to_owned);
    if old_leaf_id.as_deref() == Some(target_id) {
        // No-op: already at the target (upstream `cancelled: false`).
        return Ok(SessionCommandStep::Applied);
    }
    if session.entry(target_id).is_none() {
        return Err(format!("Entry {target_id} not found"));
    }
    let collected = collect_entries_for_branch_summary(session, old_leaf_id.as_deref(), target_id);
    let new_leaf_id = match session.entry(target_id) {
        Some(FileEntry::Message(inner)) if inner.message.role() == "user" => {
            inner.parent_id.clone()
        }
        Some(FileEntry::CustomMessage(inner)) => inner.parent_id.clone(),
        _ => Some(target_id.to_owned()),
    };
    if summarize && !collected.entries.is_empty() {
        if binding.is_none() {
            return Err(
                "navigateTree(summarize) requires a provider-backed session host".to_owned(),
            );
        }
        return Ok(SessionCommandStep::Navigate(PreparedNavigateTree {
            target_id: target_id.to_owned(),
            old_leaf_id,
            new_leaf_id,
            entries: collected.entries,
            custom_instructions: custom_instructions.map(str::to_owned),
            replace_instructions,
            label: label.map(str::to_owned),
            session_id: session.id().to_owned(),
        }));
    }
    // No summary: the leaf move is synchronous storage work.
    apply_navigation_move(session, new_leaf_id.as_deref())?;
    apply_navigation_label(session, target_id, label);
    Ok(SessionCommandStep::Applied)
}

/// Run the branch summarizer for a [`PreparedNavigateTree`]. The caller must
/// NOT hold the storage lock (the provider streams here).
pub async fn generate_navigation_summary(
    plan: &PreparedNavigateTree,
    binding: &NavigateSummaryBinding,
) -> Result<NavigateSummaryOutcome, String> {
    use crate::compaction::branch_summarization::generate_branch_summary;

    let result = generate_branch_summary(
        &plan.entries,
        &binding.model,
        Arc::clone(&binding.complete),
        plan.custom_instructions.as_deref(),
        plan.replace_instructions,
        binding.reserve_tokens,
    )
    .await;
    if let Some(error) = result.error {
        return Err(error);
    }
    Ok(NavigateSummaryOutcome {
        summary: result.summary,
        details: Some(serde_json::json!({
            "readFiles": result.read_files,
            "modifiedFiles": result.modified_files,
        })),
        usage: result.usage,
    })
}

/// Phase 3 of storage-level `navigateTree`: re-lock and move the leaf with the
/// pre-computed summary. The leaf is re-checked against the prepare-time value
/// ([`ensure_leaf_unchanged`]): a concurrent mutation aborts the navigation
/// instead of appending the summary over a moved position.
pub fn commit_navigation_to_storage(
    storage: &mut Option<SessionManager>,
    plan: PreparedNavigateTree,
    summary: NavigateSummaryOutcome,
) -> Result<(), String> {
    use crate::compaction::branch_summarization::ensure_leaf_unchanged;

    let session = storage
        .as_mut()
        .ok_or_else(|| "no attached session storage; the run stays ephemeral".to_owned())?;
    if session.id() != plan.session_id.as_str() {
        return Err("session changed during branch summarization; navigation aborted".to_owned());
    }
    ensure_leaf_unchanged(session.leaf_id(), plan.old_leaf_id.as_deref())?;
    match summary.summary {
        Some(text) => {
            session
                .branch_with_summary(
                    plan.new_leaf_id.as_deref(),
                    text,
                    summary.details,
                    None,
                    summary.usage,
                )
                .map_err(|error| format!("branch summary failed: {error}"))?;
        }
        None => apply_navigation_move(session, plan.new_leaf_id.as_deref())?,
    }
    apply_navigation_label(session, &plan.target_id, plan.label.as_deref());
    Ok(())
}

fn apply_navigation_move(
    session: &mut SessionManager,
    new_leaf_id: Option<&str>,
) -> Result<(), String> {
    match new_leaf_id {
        Some(id) => session
            .branch(id)
            .map_err(|error| format!("branch failed: {error}")),
        None => {
            session.reset_leaf();
            Ok(())
        }
    }
}

/// Upstream navigateTree `label` option: label the target entry after the
/// move (best-effort: a label failure does not undo the navigation).
fn apply_navigation_label(session: &mut SessionManager, target_id: &str, label: Option<&str>) {
    if let Some(label) = label {
        if let Err(error) = session.set_label(target_id, Some(label)) {
            crate::ext_diag!("[ext] navigateTree label ignored: {error}");
        }
    }
}
/// One native session replacement request (see
/// [`AgentSession::native_new_session`] / `native_fork` /
/// `native_switch_session`). Private: the public surface is the three typed
/// methods and [`crate::agent_session::NativeSessionHost`].
enum NativeSessionChange {
    NewSession {
        parent_session: Option<String>,
    },
    ForkCurrent,
    ForkEntry {
        entry_id: String,
        position: Option<String>,
    },
    SwitchSession {
        path: std::path::PathBuf,
    },
}

impl AgentSession {
    /// Capability probe for the native host interface: can this session
    /// perform `action` right now?
    ///
    /// A detached/ephemeral session (`--no-session`) cannot replace storage,
    /// so every action reports
    /// [`NativeCapability::Unsupported`](crate::agent_session::NativeCapability)
    /// with the same reason the trait-level call would return. Forking an
    /// in-memory manager at its current position is additionally refused at
    /// call time (there is no file to fork from).
    pub fn native_session_capability(
        &self,
        _action: crate::agent_session::NativeSessionAction,
    ) -> crate::agent_session::NativeCapability {
        if self.session.is_none() {
            return crate::agent_session::NativeCapability::Unsupported {
                reason: "no attached session storage; the run stays ephemeral".to_owned(),
            };
        }
        crate::agent_session::NativeCapability::Available
    }

    /// `ctx.newSession` with a typed result and caller cancellation.
    pub async fn native_new_session(
        &self,
        parent_session: Option<&str>,
        cancel: &AbortSignal,
    ) -> Result<crate::agent_session::SessionReplacement, crate::agent_session::NativeHostError>
    {
        self.apply_native_session_change(
            NativeSessionChange::NewSession {
                parent_session: parent_session.map(str::to_owned),
            },
            cancel,
        )
        .await
    }

    /// `ctx.fork` with a typed result and caller cancellation.
    ///
    /// `entry_id: None` forks at the current position into a new file
    /// (storage replaced); `entry_id: Some(id)` branches at the entry
    /// (`position` follows upstream's `before` default).
    pub async fn native_fork(
        &self,
        entry_id: Option<&str>,
        position: Option<&str>,
        cancel: &AbortSignal,
    ) -> Result<crate::agent_session::SessionReplacement, crate::agent_session::NativeHostError>
    {
        let change = match entry_id {
            Some(entry_id) => NativeSessionChange::ForkEntry {
                entry_id: entry_id.to_owned(),
                position: position.map(str::to_owned),
            },
            None => NativeSessionChange::ForkCurrent,
        };
        self.apply_native_session_change(change, cancel).await
    }

    /// `ctx.switchSession` with a typed result and caller cancellation.
    pub async fn native_switch_session(
        &self,
        path: &Path,
        cancel: &AbortSignal,
    ) -> Result<crate::agent_session::SessionReplacement, crate::agent_session::NativeHostError>
    {
        self.apply_native_session_change(
            NativeSessionChange::SwitchSession {
                path: path.to_path_buf(),
            },
            cancel,
        )
        .await
    }

    /// One native replacement: prepare without touching the live session,
    /// observe `cancel` at the checkpoints, then swap (or branch) through the
    /// SAME guards the JS `ctx` path uses (`session_before_switch` /
    /// `session_before_fork` can refuse).
    async fn apply_native_session_change(
        &self,
        change: NativeSessionChange,
        cancel: &AbortSignal,
    ) -> Result<crate::agent_session::SessionReplacement, crate::agent_session::NativeHostError>
    {
        use crate::agent_session::NativeHostError;

        if self.session.is_none() {
            return Err(NativeHostError::Unsupported {
                reason: "no attached session storage; the run stays ephemeral".to_owned(),
            });
        }
        check_native_cancel(cancel)?;
        match change {
            NativeSessionChange::NewSession { parent_session } => {
                let (cwd, session_dir) = self
                    .with_session_manager(|sm| {
                        (sm.cwd().to_owned(), sm.session_dir().to_path_buf())
                    })
                    .await
                    .ok_or_else(|| NativeHostError::Unsupported {
                        reason: "no attached session storage; the run stays ephemeral".to_owned(),
                    })?;
                check_native_cancel(cancel)?;
                let mut fresh = SessionManager::create(&cwd, &session_dir).map_err(|error| {
                    NativeHostError::Failed {
                        message: format!("new session failed: {error}"),
                    }
                })?;
                if let Some(parent) = parent_session {
                    fresh
                        .new_session(Some(crate::session_manager::NewSessionOptions {
                            id: None,
                            parent_session: Some(parent),
                        }))
                        .map_err(|error| NativeHostError::Failed {
                            message: format!("new session failed: {error}"),
                        })?;
                }
                check_native_cancel(cancel)?;
                if !self.replace_session_storage(fresh).await {
                    return Err(NativeHostError::Cancelled {
                        reason: "session switch cancelled or no attached session storage"
                            .to_owned(),
                    });
                }
                self.native_replacement_outcome(true, None).await
            }
            NativeSessionChange::SwitchSession { path } => {
                let manager =
                    SessionManager::open(&path).map_err(|error| NativeHostError::Failed {
                        message: error.to_string(),
                    })?;
                check_native_cancel(cancel)?;
                if !self.replace_session_storage(manager).await {
                    return Err(NativeHostError::Cancelled {
                        reason: "session switch cancelled or no attached session storage"
                            .to_owned(),
                    });
                }
                self.native_replacement_outcome(true, None).await
            }
            NativeSessionChange::ForkCurrent => {
                let (source, cwd, session_dir) = self
                    .with_session_manager(|sm| {
                        (
                            sm.session_file().map(Path::to_path_buf),
                            sm.cwd().to_owned(),
                            sm.session_dir().to_path_buf(),
                        )
                    })
                    .await
                    .ok_or_else(|| NativeHostError::Unsupported {
                        reason: "no attached session storage; the run stays ephemeral".to_owned(),
                    })?;
                let Some(source) = source else {
                    return Err(NativeHostError::Failed {
                        message: "cannot fork an in-memory session".to_owned(),
                    });
                };
                check_native_cancel(cancel)?;
                let forked = SessionManager::fork_from_source(&source, &cwd, &session_dir, None)
                    .map_err(|error| NativeHostError::Failed {
                        message: format!("fork failed: {error}"),
                    })?;
                check_native_cancel(cancel)?;
                if !self.replace_session_storage(forked).await {
                    return Err(NativeHostError::Cancelled {
                        reason: "session switch cancelled or no attached session storage"
                            .to_owned(),
                    });
                }
                self.native_replacement_outcome(true, None).await
            }
            NativeSessionChange::ForkEntry { entry_id, position } => {
                check_native_cancel(cancel)?;
                match self
                    .fork_entry(&entry_id, ForkPosition::parse(position.as_deref()))
                    .await
                {
                    Ok(ForkEntryOutcome::Branched { selected_text }) => {
                        self.native_replacement_outcome(false, selected_text).await
                    }
                    Ok(ForkEntryOutcome::Cancelled) => Err(NativeHostError::Cancelled {
                        reason: "fork cancelled by a session_before_fork handler".to_owned(),
                    }),
                    Err(error) => Err(NativeHostError::Failed { message: error }),
                }
            }
        }
    }

    /// Read the replacement session's identity for the native result.
    async fn native_replacement_outcome(
        &self,
        replaced_storage: bool,
        selected_text: Option<String>,
    ) -> Result<crate::agent_session::SessionReplacement, crate::agent_session::NativeHostError>
    {
        use crate::agent_session::{NativeHostError, SessionReplacement};
        let identity = self
            .with_session_manager(|sm| {
                let file = sm.session_file().map(Path::to_path_buf);
                (
                    Some(sm.id().to_owned()),
                    file.clone(),
                    file,
                    sm.leaf_id().map(str::to_owned),
                )
            })
            .await
            .ok_or_else(|| NativeHostError::Unsupported {
                reason: "no attached session storage; the run stays ephemeral".to_owned(),
            })?;
        Ok(SessionReplacement {
            replaced_storage,
            session_id: identity.0,
            session_path: identity.1,
            session_file: identity.2,
            leaf_id: identity.3,
            selected_text,
        })
    }
}

/// Cancellation checkpoint for the native replacement path.
fn check_native_cancel(cancel: &AbortSignal) -> Result<(), crate::agent_session::NativeHostError> {
    if cancel.is_aborted() {
        Err(crate::agent_session::NativeHostError::Cancelled {
            reason: "cancelled by the caller before the session changed".to_owned(),
        })
    } else {
        Ok(())
    }
}

impl AgentSession {
    /// The extension handle (shared by the TUI host for notification drains).
    /// Returns `None` when the session was built without extensions.
    pub fn extensions_handle(&self) -> Option<&crate::extensions::ExtensionHandle> {
        self.extensions.as_ref()
    }

    /// The unified plugin host (JS adapter + native Rust plugins), when the
    /// session was built with a plugin surface (an extension handle or inline
    /// plugins). Read-only: hosts use it for native-facing queries such as
    /// [`PluginHost::has_native_handlers`](crate::plugins::PluginHost::has_native_handlers)
    /// and registry snapshots.
    ///
    /// [`AgentSession::dispatch_user_bash_event`] uses the same handle
    /// internally, so event dispatch needs no direct access.
    pub fn plugin_host(&self) -> Option<&crate::plugins::PluginHost> {
        self.plugins.as_deref()
    }

    /// Shared handle to the unified plugin host, for hosts that need to keep
    /// dispatching while the session owns the only reference (the TUI's
    /// raw-terminal-input dispatch thread: `terminal_input` is latency
    /// sensitive and must not run on the worker's agent loop).
    pub fn plugin_host_arc(&self) -> Option<std::sync::Arc<crate::plugins::PluginHost>> {
        self.plugins.as_ref().map(std::sync::Arc::clone)
    }

    /// Names of the NATIVE (Rust backend) slash commands registered on the
    /// session's plugin host: the builtins (`/todos`, `/phone-start`, `/mcp`,
    /// ...). JS-extension commands are deliberately absent — the TUI already
    /// dispatches those through the shared extension runtime's
    /// `command_defs()`.
    ///
    /// This is the TUI slash router's native-command snapshot (reported once
    /// as `WorkerEvent::NativeCommands`): a name in this list is sent as
    /// `WorkerCommand::DispatchPluginCommand` instead of falling through to
    /// the normal prompt submission.
    pub fn native_plugin_command_names(&self) -> Vec<String> {
        self.native_plugin_command_specs()
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// Dynamic autocomplete metadata for the same native command set:
    /// `(invocation name, registry description)` pairs read from the live
    /// [`crate::plugins::PluginRegistry`]. Nothing here is a fixed list —
    /// adding a Rust-backed command to any builtin plugin makes it appear in
    /// the TUI slash menu on the next session snapshot.
    ///
    /// Both the plain name and the deduped `name:2`/`name:3` invocation
    /// spellings are returned, matching [`Self::native_plugin_command_names`]
    /// (a typed `/name` resolves to the first registration).
    pub fn native_plugin_command_specs(&self) -> Vec<(String, Option<String>)> {
        let Some(plugins) = &self.plugins else {
            return Vec::new();
        };
        let mut specs: Vec<(String, Option<String>)> = plugins
            .registry()
            .resolved_commands()
            .into_iter()
            .filter(|command| {
                matches!(
                    command.backend,
                    Some(crate::plugins::CommandBackend::Rust(_))
                )
            })
            .flat_map(|command| {
                let description = command.def.description;
                [command.def.name, command.invocation_name]
                    .into_iter()
                    .map(move |name| (name, description.clone()))
            })
            .collect();
        specs.sort_by(|left, right| left.0.cmp(&right.0));
        specs.dedup_by(|left, right| left.0 == right.0);
        specs
    }

    /// Dispatch one plugin slash command by name through the session's
    /// [`PluginHost`](crate::plugins::PluginHost):
    /// `PluginHost::dispatch_command` runs a
    /// [`CommandBackend::Rust`](crate::plugins::CommandBackend::Rust) handler
    /// natively and falls back to the JS runtime otherwise.
    ///
    /// This is the worker seam behind the TUI's
    /// `WorkerCommand::DispatchPluginCommand`. The outcome maps to the
    /// transcript the way print mode's
    /// [`try_extension_command`](crate::coding_agent::print::try_extension_command)
    /// maps it: native commands return their display text through
    /// [`CommandOutcome::error`](crate::extensions::CommandOutcome) even when
    /// `ok` is true (the `/todos` list, `/phone-start`'s URL), so the caller
    /// reports that text for both success and failure.
    ///
    /// When the session is idle, the command-context actions the handler
    /// recorded (`ctx.newSession` / `fork` / `switchSession` / `navigateTree`
    /// / `reload`) are applied before this returns; an action failure flips
    /// the outcome to `ok: false` with its error text appended, so the host
    /// surfaces it instead of losing it. A run/compaction in flight keeps the
    /// actions queued for the settle drain.
    pub async fn dispatch_plugin_command(
        &self,
        name: &str,
        args: &str,
    ) -> anyhow::Result<crate::extensions::CommandOutcome> {
        let Some(plugins) = &self.plugins else {
            anyhow::bail!("command `/{name}` is not registered by any plugin");
        };
        let mut outcome = plugins.dispatch_command(name, args).await?;
        // Apply the session-control actions this command recorded
        // (`ctx.newSession/fork/switchSession/navigateTree/reload`) right
        // here when the session is idle. Without this an extension command
        // dispatched while idle would leave its action queued until the next
        // run settles, so `ctx.reload()` from a command would appear to do
        // nothing. A run (or compaction) in flight keeps the existing path:
        // the action stays queued and the settle drain applies it, because
        // swapping storage or reloading plugins under a live run is unsafe.
        if self.extensions.is_some() && !self.is_streaming() && !self.is_compacting() {
            for (action, result) in self.apply_session_command_actions().await {
                let Err(error) = result else {
                    continue;
                };
                crate::ext_diag!("[ext] command action {action:?} failed: {error}");
                outcome.ok = false;
                outcome.error = Some(match outcome.error.take() {
                    Some(text) if !text.is_empty() => format!("{text} {error}"),
                    _ => error,
                });
            }
        }
        Ok(outcome)
    }

    /// The `/reload` seam for the session's NATIVE plugin state.
    ///
    /// Re-runs the unified plugin lifecycle for this host:
    /// `session_shutdown {reason: "reload"}` to the native plugins, every
    /// [`AgentPlugin::on_reload`](crate::plugins::AgentPlugin::on_reload)
    /// hook (the `codemode` settings cache, the `mcp` manager's teardown, the
    /// `todo` replay cache), then one registry rebuild so the reloaded
    /// configuration is what the next tool loadout sees.
    ///
    /// This is the product caller [`PluginHost::reload_native_plugins`] was
    /// missing: the interactive host's `/reload` reaches it through
    /// `WorkerCommand::ReloadResources`. It deliberately does NOT reload the
    /// JS runtime — that half stays with the host's extension dispatcher
    /// ([`ExtensionHandle::reload`](crate::extensions::ExtensionHandle::reload)),
    /// so one `/reload` reloads each face exactly once. A plugin whose
    /// `on_reload` fails is logged and does not abort the rest of the pass.
    ///
    /// No-op (and always `Ok`) for a session built without a plugin host.
    ///
    /// Refused while a run (or a compaction) owns the session: `on_reload`
    /// tears MCP connections down and swaps the plugin registry, which must
    /// not happen under an in-flight tool call. This mirrors the extension
    /// actor's documented `/reload` refusal ("cannot reload extensions while
    /// N command(s) are still executing"), which the JS half keeps.
    pub async fn reload_plugins(&self) -> anyhow::Result<()> {
        if self.is_streaming() || self.is_compacting() {
            anyhow::bail!(
                "cannot reload plugins while a run is in flight; wait for it to finish and \
                 retry /reload"
            );
        }
        match &self.plugins {
            Some(plugins) => {
                plugins.reload_native_plugins().await;
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// The host-side `ctx.reload()` / extension `reload()` seam (upstream
    /// `ExtensionCommandContext.reload`, `core/extensions/types.ts:433-434`:
    /// "Reload extensions, skills, prompts, themes, and context files").
    ///
    /// One call reloads every face the live session owns, exactly once:
    ///
    /// 1. **extensions + native plugins + tools** — the unified plugin host's
    ///    composite reload ([`PluginHost::reload`]): native
    ///    `session_shutdown {reason:"reload"}` + every `on_reload` hook, the
    ///    JS runtime rebuild with the extension factories re-executed
    ///    ([`ExtensionHandle::reload`]), and one registry rebuild. That
    ///    rebuild is what makes a tool registered by the reloaded extension
    ///    visible to the next run: [`AgentSession::merged_tool_registry`] is
    ///    keyed on the host's registry generation. A host without a JS
    ///    extension file path (builtin-only plan, no extensions) runs only the
    ///    native half instead of failing.
    /// 2. **skills / prompts / themes** — a fresh `resources_discover`
    ///    dispatch against the rebuilt runtime
    ///    ([`AgentSession::reload_extension_resources`]), which retracts the
    ///    previous contribution before merging the new one, so a changed,
    ///    renamed or removed contributed resource takes effect. Context files
    ///    are re-read by the prompt rebuild that dispatch performs.
    /// 3. **the session itself is untouched**: no storage swap, no transcript
    ///    rewrite, no context reset. The reload only refreshes the resource
    ///    and tool surfaces; the next prompt reads the same session.
    ///
    /// Refused while a run or compaction owns the session (the plugin reload
    /// tears MCP connections down and swaps the registry, which must not
    /// happen under an in-flight tool call).
    pub async fn reload(&self) -> anyhow::Result<SessionReloadOutcome> {
        if self.is_streaming() || self.is_compacting() {
            anyhow::bail!(
                "cannot reload the session while a run is in flight; wait for it to finish \
                 and retry the reload"
            );
        }
        let mut extensions_reloaded = false;
        let mut native_plugins_reloaded = false;
        match &self.plugins {
            Some(plugins) => {
                native_plugins_reloaded = true;
                // `has_load_paths` distinguishes a real JS runtime from a
                // plan-only / never-loaded handle: the latter's reload has no
                // JS half (and would report "no extensions have been loaded
                // yet"), so the native reload + registry rebuild is the whole
                // job there.
                let js_reloads = plugins
                    .js_handle()
                    .is_some_and(crate::extensions::ExtensionHandle::has_load_paths);
                if js_reloads {
                    plugins
                        .reload()
                        .await
                        .map_err(|error| anyhow::anyhow!("extension reload failed: {error}"))?;
                    extensions_reloaded = true;
                } else {
                    plugins.reload_native_plugins().await;
                }
            }
            None => {
                if let Some(handle) = &self.extensions {
                    if handle.has_load_paths() {
                        handle
                            .reload()
                            .await
                            .map_err(|error| anyhow::anyhow!("extension reload failed: {error}"))?;
                        extensions_reloaded = true;
                    }
                }
            }
        }
        // The rebuilt runtime registered against a cleared registry, and the
        // fresh JS prelude starts from an empty ctx: re-publish the live
        // session/model/tool/command snapshot before anything (the resources
        // dispatch below, the next prompt, the withSession callback) reads it.
        let model = self.current_model();
        self.push_session_context(&model).await;
        // Upstream `reload` emits `session_start {reason: "reload"}` against
        // the rebuilt runtime, then extends resources from it
        // (agent-session.ts:3753-3757). A reload is a full lifecycle pass even
        // when no startup `session_start` fired yet, so mark the lazy startup
        // block consumed: the next prompt must not deliver a second
        // session_start.
        if let Some(plugins) = &self.plugins {
            // NATIVE face only: the JS actor's reload lifecycle already
            // dispatches its own `session_start {reason:"reload"}` to the
            // rebuilt JS runtime (`ExtensionHandle::reload`), so a second
            // both-face emit would deliver it twice.
            if plugins.has_native_handlers("session_start") {
                let mut payload = serde_json::json!({
                    "type": "session_start",
                    "reason": "reload",
                });
                plugins.emit_native("session_start", &mut payload).await;
            }
            // Handlers may have registered providers/models during the reload
            // lifecycle; apply them before the resources pass so the next
            // prompt sees the same state a startup would have produced.
            self.sync_plugin_providers(plugins).await;
            self.sync_plugin_virtual_models(plugins).await;
        }
        self.session_start_sent.store(true, Ordering::SeqCst);
        // Notifications queued by the reload `session_start` stay on the
        // plugin queue: the next host drain applies them through the normal
        // path (calling the drain here would make `reload` mutually recursive
        // with the `ctx.reload()` command action).
        let resources = self.reload_extension_resources().await;
        Ok(SessionReloadOutcome {
            extensions_reloaded,
            native_plugins_reloaded,
            resources,
        })
    }

    /// The full in-memory transcript of an ephemeral (no-session-storage)
    /// session: every user, assistant, and tool-result message appended so
    /// far, in run order. Used by tools that spawn ephemeral child sessions
    /// (the `subagent` builtin) to carry the child transcript in their result
    /// details; returns an empty transcript for persisted sessions (read the
    /// session storage instead).
    pub async fn ephemeral_transcript(&self) -> Vec<Message> {
        history_owned(self.history_snapshot())
    }

    /// Drain and apply the extension command-context actions recorded by the
    /// `commandAction` host function (upstream print/rpc
    /// `commandContextActions`, print-mode.ts:78-100 / rpc-mode.ts:322-344):
    /// `newSession` / `switchSession` / fork-at-current swap the session
    /// storage through the guarded seam ([`AgentSession::replace_session_storage`]
    /// — a `session_before_switch` handler may cancel), fork-at-entry branches
    /// in place, `navigateTree` rides [`AgentSession::navigate_tree`] with
    /// full summarization semantics, and `reload` runs the host resource
    /// reload ([`AgentSession::reload`] — extensions, native plugins, tools,
    /// skills, prompts and themes; the session and transcript stay put). A
    /// `withSession` callback travels as a runtime token and runs through
    /// [`ExtensionHandle::call_session_callback`] once the replacement session
    /// is live. Every action
    /// is consumed even when its application failed; the outcome rides the
    /// return value for host-side reporting.
    pub async fn apply_session_command_actions(
        &self,
    ) -> Vec<(SessionCommandAction, Result<(), String>)> {
        // Claim only the actions stamped with THIS session's identity; other
        // hosts' actions stay queued for their owner. The expected origin is
        // captured once for the whole drain so an action applied mid-drain
        // (a `newSession` changing the session id) cannot orphan the
        // remaining entries of the same batch.
        let origin = SessionOrigin::AgentSession {
            session_id: self
                .session_id
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        };
        let mut outcomes = Vec::new();
        for entry in take_session_command_actions_for(&origin) {
            let result = self.apply_session_command(&entry.action).await;
            outcomes.push((entry.action, result));
        }
        outcomes
    }

    async fn apply_session_command(&self, action: &SessionCommandAction) -> Result<(), String> {
        match action {
            SessionCommandAction::NewSession {
                parent_session,
                with_session,
            } => {
                // The JS face has no caller cancellation token (upstream's
                // command context does not expose one); the native
                // `NativeSessionHost` path does.
                self.native_new_session(parent_session.as_deref(), &AbortSignal::new())
                    .await
                    .map_err(|error| error.to_string())?;
                self.run_with_session_callback(*with_session, "newSession")
                    .await
            }
            SessionCommandAction::SwitchSession {
                session_path,
                with_session,
            } => {
                self.native_switch_session(Path::new(session_path), &AbortSignal::new())
                    .await
                    .map_err(|error| error.to_string())?;
                self.run_with_session_callback(*with_session, "switchSession")
                    .await
            }
            SessionCommandAction::Fork {
                entry_id,
                position,
                with_session,
            } => {
                // Fork at the current position (pir `clone` semantics): new
                // file, full history copy, storage swap. Fork at an entry:
                // `session_before_fork` dispatches first (a cancel aborts the
                // fork), then the upstream `position` semantics apply
                // ([`AgentSession::fork_entry`]). Upstream
                // `finishSessionReplacement` is skipped on a cancelled
                // replacement, so the callback never runs.
                self.native_fork(
                    entry_id.as_deref(),
                    position.as_deref(),
                    &AbortSignal::new(),
                )
                .await
                .map_err(|error| error.to_string())?;
                self.run_with_session_callback(*with_session, "fork").await
            }
            SessionCommandAction::NavigateTree {
                target_id,
                summarize,
                custom_instructions,
                replace_instructions,
                label,
            } => {
                let navigation = self
                    .navigate_tree(
                        target_id,
                        *summarize,
                        custom_instructions.as_deref(),
                        *replace_instructions,
                        crate::compaction::branch_summarization::DEFAULT_BRANCH_SUMMARY_RESERVE_TOKENS,
                    )
                    .await;
                match navigation {
                    Some(Ok(navigation)) if navigation.cancelled => {
                        Err("tree navigation cancelled by a session_before_tree handler".to_owned())
                    }
                    Some(Ok(_)) => {
                        // Upstream navigateTree `label` option: label the
                        // target entry after the move (best-effort).
                        if let Some(label) = label {
                            if let Some(Err(error)) = self
                                .with_session_manager(|sm| {
                                    sm.set_label(target_id, Some(label.as_str()))
                                })
                                .await
                            {
                                crate::ext_diag!("[ext] navigateTree label ignored: {error}");
                            }
                        }
                        Ok(())
                    }
                    Some(Err(error)) => Err(error),
                    None => Err("no attached session storage; the run stays ephemeral".to_owned()),
                }
            }
            SessionCommandAction::Reload => self
                .reload()
                .await
                .map(|_outcome| ())
                .map_err(|error| format!("reload failed: {error}")),
        }
    }

    /// Run one deferred `withSession` callback against the replacement session
    /// (upstream `finishSessionReplacement` → `withSession(createReplacedSessionContext())`,
    /// agent-session-runtime.ts:187-194).
    ///
    /// The replacement already happened when this runs, so the callback's
    /// context must describe the NEW session: the live snapshot is pushed
    /// first (session id/path/entries/leaf), then the runtime builds the ctx
    /// from it. `None` (the caller passed no `withSession`) is a no-op. A
    /// callback failure is reported with the session-RESPONSIBLE action's
    /// name; the replacement itself is not rolled back (upstream throws after
    /// the switch too).
    async fn run_with_session_callback(
        &self,
        token: Option<u64>,
        action: &str,
    ) -> Result<(), String> {
        let Some(token) = token else {
            return Ok(());
        };
        let runtime = self
            .plugins
            .as_ref()
            .and_then(|plugins| plugins.js_handle())
            .or(self.extensions.as_ref());
        let Some(runtime) = runtime else {
            return Err(format!(
                "{action}: no extension runtime is attached, so the withSession callback \
                 cannot run"
            ));
        };
        let model = self.current_model();
        self.push_session_context(&model).await;
        runtime
            .call_session_callback(token)
            .await
            .map_err(|error| format!("{action} withSession callback failed: {error}"))
    }
}

// ---------------------------------------------------------------------------
// Native phone host: command adapter over the live session
// ---------------------------------------------------------------------------
//
// Every command the phone client sends lands here through
// `HostCommands::dispatch`. The adapter owns an `Arc<AgentSession>` (upgraded
// from the phone plugin's binding slot), so each handler is the session's own
// method — prompts start a real run, compaction calls `AgentSession::compact`,
// forks move the real leaf — never a placeholder. Long-running actions
// (prompt, compaction) run on a spawned task so the bridge's WebSocket reader
// keeps accepting `abort`, and failures that happen after the reply are
// surfaced through the bridge as `client-error` frames.

#[cfg(feature = "phone")]
use crate::plugins::builtin::phone::session::{
    ForwardedCommand, HostFuture, HostReply, PhoneCommand, EARLIER_MESSAGE_PAGE_MAX,
    SNAPSHOT_MESSAGE_WINDOW,
};

#[cfg(feature = "phone")]
fn phone_plugin_note_write(session: &AgentSession) {
    if let Some(phone) = session.phone_plugin() {
        phone.note_phone_write();
    }
}

/// Spawn a detached host task; `false` when no async runtime is available.
#[cfg(feature = "phone")]
fn phone_spawn<F>(future: F) -> bool
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(future);
            true
        }
        Err(_) => false,
    }
}

#[cfg(feature = "phone")]
fn phone_image_blocks(images: &[JsonValue]) -> Vec<ContentBlock> {
    images
        .iter()
        .filter_map(|image| {
            let data = image.get("data").and_then(JsonValue::as_str)?;
            let mime_type = image
                .get("mimeType")
                .and_then(JsonValue::as_str)
                .unwrap_or("image/png");
            Some(ContentBlock::Image(crate::ai::types::ImageContent {
                data: data.to_owned(),
                mime_type: mime_type.to_owned(),
            }))
        })
        .collect()
}

#[cfg(feature = "phone")]
fn phone_model_json(model: &Model) -> JsonValue {
    serde_json::to_value(model).unwrap_or(JsonValue::Null)
}

#[cfg(feature = "phone")]
fn phone_stats_json(stats: &SessionStats) -> JsonValue {
    serde_json::json!({
        "sessionFile": stats.session_file,
        "sessionId": stats.session_id,
        "userMessages": stats.user_messages,
        "assistantMessages": stats.assistant_messages,
        "tokens": {
            "input": stats.tokens.input,
            "cacheRead": stats.tokens.cache_read,
            "cacheWrite": stats.tokens.cache_write,
            "output": stats.tokens.output,
            "reasoning": 0,
            "total": stats.tokens.total,
        },
        "toolCalls": stats.tool_calls,
        "toolResults": stats.tool_results,
        "totalMessages": stats.total_messages,
        "cost": stats.cost,
        "contextUsage": serde_json::to_value(&stats.context_usage)
            .unwrap_or(JsonValue::Null),
    })
}

/// Flatten one stored session's tree into the phone's node rows.
#[cfg(feature = "phone")]
fn phone_tree_json(manager: &SessionManager) -> JsonValue {
    let session_file = manager
        .session_file()
        .map(|path| path.display().to_string());
    let mut nodes: Vec<JsonValue> = Vec::new();
    let _ = manager.for_each_tree_row(|row| {
        let role = match row.kind_hint {
            "user_message" => Some("user"),
            "assistant_message" => Some("assistant"),
            "tool_result" => Some("toolResult"),
            "system_message" => Some("system"),
            "developer_message" => Some("developer"),
            _ => None,
        };
        let mut summary = serde_json::json!({
            "kind": row.kind_hint,
            "preview": row.preview,
        });
        if let Some(role) = role {
            summary["role"] = JsonValue::String(role.to_owned());
        }
        let mut node = serde_json::json!({
            "id": row.id,
            "parentId": row.parent_id,
            "type": row.kind_hint,
            "depth": row.depth,
            "timestamp": row.timestamp_ms,
            "label": row.label,
            "summary": summary,
        });
        if let Some(tool_name) = &row.tool_name {
            node["toolName"] = JsonValue::String(tool_name.clone());
        }
        nodes.push(node);
    });
    let current_path: Vec<JsonValue> = manager
        .branch_entries(None)
        .unwrap_or_default()
        .iter()
        .filter(|entry| !entry.is_header())
        .map(|entry| JsonValue::String(entry.entry_id().to_owned()))
        .collect();
    serde_json::json!({
        "sessionFile": session_file,
        "currentLeafId": manager.leaf_id(),
        "currentPathIds": current_path,
        "nodes": nodes,
    })
}

#[cfg(feature = "phone")]
fn phone_user_message_text(message: &Message) -> String {
    let Message::User(user) = message else {
        return String::new();
    };
    user.content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

#[cfg(feature = "phone")]
fn phone_path_suggestions(cwd: &Path, command: &JsonValue) -> JsonValue {
    let query = command
        .get("query")
        .and_then(JsonValue::as_str)
        .unwrap_or("");
    let mode = command
        .get("mode")
        .and_then(JsonValue::as_str)
        .unwrap_or("mention");
    let request_id = command
        .get("requestId")
        .cloned()
        .unwrap_or_else(|| JsonValue::from(0));
    let (directory, prefix) = match query.rsplit_once('/') {
        Some((directory, prefix)) => (directory, prefix),
        None => ("", query),
    };
    let read_dir = if directory.is_empty() {
        cwd.to_path_buf()
    } else {
        cwd.join(directory)
    };
    let mut suggestions: Vec<JsonValue> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&read_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "." || name == ".." || !name.starts_with(prefix) {
                continue;
            }
            let is_directory = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
            let value = if directory.is_empty() {
                name.clone()
            } else {
                format!("{directory}/{name}")
            };
            suggestions.push(serde_json::json!({
                "value": value,
                "isDirectory": is_directory,
                "description": name,
                "kind": if is_directory { "dir" } else { "file" },
            }));
            if suggestions.len() >= 20 {
                break;
            }
        }
    }
    suggestions.sort_by(|left, right| {
        let left_directory = left["isDirectory"].as_bool().unwrap_or(false);
        let right_directory = right["isDirectory"].as_bool().unwrap_or(false);
        right_directory.cmp(&left_directory).then_with(|| {
            left["value"]
                .as_str()
                .unwrap_or("")
                .cmp(right["value"].as_str().unwrap_or(""))
        })
    });
    serde_json::json!({
        "requestId": request_id,
        "mode": mode,
        "query": query,
        "suggestions": suggestions,
    })
}

#[cfg(feature = "phone")]
impl AgentSession {
    /// Dispatch one forwarded phone command. The returned future owns an
    /// `Arc<AgentSession>` (never a borrow of the bridge), so it is safe to
    /// await from the WebSocket task.
    pub(crate) fn phone_dispatch_command(
        self: Arc<Self>,
        forwarded: ForwardedCommand,
    ) -> HostFuture {
        Box::pin(async move {
            match forwarded.command {
                PhoneCommand::Prompt {
                    message,
                    images,
                    streaming_behavior,
                } => self.phone_prompt_command(message, images, streaming_behavior),
                PhoneCommand::Abort => self.phone_abort_command().await,
                PhoneCommand::SetModel { provider, model_id } => {
                    self.phone_set_model_command(&provider, &model_id).await
                }
                PhoneCommand::SetThinkingLevel { level } => {
                    self.phone_set_thinking_command(&level).await
                }
                PhoneCommand::Compact {
                    custom_instructions,
                } => self.phone_compact_command(custom_instructions),
                PhoneCommand::Reload => Err(
                    "reload is not supported by the live native session host: no session \
                     reload seam for phone-initiated reloads. Run /reload in the CLI."
                        .to_owned(),
                ),
                PhoneCommand::GetSessionStats => {
                    let stats = self.session_stats().await;
                    Ok(HostReply::data(phone_stats_json(&stats)))
                }
                PhoneCommand::PhoneGetTree => self.phone_tree_command().await,
                PhoneCommand::Fork { entry_id } => self.phone_fork_command(entry_id).await,
                PhoneCommand::PhoneListSessions { include_subagents } => {
                    self.phone_list_sessions_command(include_subagents).await
                }
                PhoneCommand::NewSession { parent_session } => {
                    self.phone_new_session_command(parent_session).await
                }
                PhoneCommand::SwitchSession { session_path } => {
                    self.phone_switch_session_command(&session_path).await
                }
                PhoneCommand::SetSessionName { name } => {
                    self.phone_set_session_name_command(&name).await
                }
                PhoneCommand::Refresh => {
                    self.publish_phone_state_now().await;
                    Ok(HostReply::default())
                }
                PhoneCommand::LoadEarlierMessages { before, count } => {
                    self.phone_load_earlier_command(before, count).await
                }
                PhoneCommand::SessionSpawn { .. } => Err(
                    "parallel phone sessions require the session pool, which is not wired \
                     to this live-session host yet."
                        .to_owned(),
                ),
                PhoneCommand::SessionSelect { session_id } => Err(format!(
                    "session `{session_id}` is not a parallel pool session: this host mirrors \
                     the live CLI session only."
                )),
                PhoneCommand::SessionParentNew => self.phone_parent_new_command().await,
                PhoneCommand::Raw { command } => self.phone_raw_command(&command).await,
            }
        })
    }

    /// Start (or queue) a phone prompt with the bridge's `steer`/`followUp`
    /// semantics and image support.
    fn phone_prompt_command(
        self: &Arc<Self>,
        message: String,
        images: Vec<JsonValue>,
        streaming_behavior: Option<String>,
    ) -> Result<HostReply, String> {
        let blocks = phone_image_blocks(&images);
        if message.trim().is_empty() && blocks.is_empty() {
            return Err("prompt requires a non-empty message".to_owned());
        }
        if self.is_streaming() {
            let behavior = streaming_behavior.as_deref();
            match behavior {
                Some("steer") | Some("followUp") => {
                    if !blocks.is_empty() {
                        return Err("Images can only be attached to a new prompt; queued steer/\
                             follow-up messages carry text only."
                            .to_owned());
                    }
                    phone_plugin_note_write(self);
                    let disposition = if behavior == Some("followUp") {
                        self.follow_up(&message)
                    } else {
                        self.steer(&message)
                    };
                    let session = Arc::clone(self);
                    let _ = phone_spawn(async move {
                        session.publish_phone_status().await;
                    });
                    return Ok(HostReply::data(serde_json::json!({
                        "disposition": disposition,
                    })));
                }
                Some(other) => {
                    return Err(format!(
                        "Unknown streamingBehavior `{other}`: use `steer` or `followUp`."
                    ))
                }
                None => {
                    return Err(
                        "Agent is already processing. Specify streamingBehavior ('steer' or \
                         'followUp') to queue the message."
                            .to_owned(),
                    )
                }
            }
        }
        // Idle: this is a real run, exactly like the RPC prompt path.
        phone_plugin_note_write(self);
        if let Some(phone) = self.phone_plugin() {
            phone.begin_phone_run();
        }
        let session = Arc::clone(self);
        let started = phone_spawn(async move {
            let handler: EventHandler = Arc::new(|_| {});
            let result = session.prompt_with_images(&message, blocks, handler).await;
            if let Some(phone) = session.phone_plugin() {
                phone.end_phone_run();
            }
            if let Err(error) = result {
                session
                    .publish_phone_run_error(&format!("Prompt failed: {error}"))
                    .await;
            }
        });
        if !started {
            if let Some(phone) = self.phone_plugin() {
                phone.end_phone_run();
            }
            return Err("no async runtime is available to start the prompt".to_owned());
        }
        Ok(HostReply::data(serde_json::json!({
            "disposition": "started",
        })))
    }

    async fn phone_abort_command(&self) -> Result<HostReply, String> {
        phone_plugin_note_write(self);
        self.abort();
        self.publish_phone_status().await;
        Ok(HostReply::data(serde_json::json!({ "aborted": true })))
    }

    async fn phone_set_model_command(
        &self,
        provider: &str,
        model_id: &str,
    ) -> Result<HostReply, String> {
        let model = self
            .find_model(provider, model_id)
            .ok_or_else(|| format!("Model not found: {provider}/{model_id}"))?;
        phone_plugin_note_write(self);
        self.set_model(model.clone())
            .await
            .map_err(|error| error.to_string())?;
        self.publish_phone_state_now().await;
        Ok(HostReply::data(phone_model_json(&model)))
    }

    async fn phone_set_thinking_command(&self, level: &str) -> Result<HostReply, String> {
        let parsed = crate::ai::providers::resolution::parse_thinking_level(level)
            .ok_or_else(|| format!("Invalid thinking level: {level}"))?;
        phone_plugin_note_write(self);
        let applied = self.set_thinking_level(parsed).await;
        self.publish_phone_state_now().await;
        Ok(HostReply::data(serde_json::json!({
            "level": applied,
        })))
    }

    fn phone_compact_command(
        self: &Arc<Self>,
        custom_instructions: Option<String>,
    ) -> Result<HostReply, String> {
        phone_plugin_note_write(self);
        let session = Arc::clone(self);
        let started = phone_spawn(async move {
            match session.compact(custom_instructions.as_deref()).await {
                Ok(_) => session.publish_phone_state_now().await,
                Err(error) => {
                    session
                        .publish_phone_run_error(&format!("Compaction failed: {error}"))
                        .await;
                }
            }
        });
        if !started {
            return Err("no async runtime is available to start compaction".to_owned());
        }
        Ok(HostReply::data(serde_json::json!({ "started": true })))
    }

    async fn phone_tree_command(&self) -> Result<HostReply, String> {
        let tree = self
            .with_session_manager(|manager| phone_tree_json(manager))
            .await
            .ok_or_else(|| "no attached session storage".to_owned())?;
        Ok(HostReply::data(tree))
    }

    async fn phone_fork_command(&self, entry_id: Option<String>) -> Result<HostReply, String> {
        phone_plugin_note_write(self);
        let entry_id = entry_id.filter(|id| !id.is_empty());
        match entry_id {
            Some(entry_id) => match self.fork_entry(&entry_id, ForkPosition::Before).await {
                Ok(ForkEntryOutcome::Branched { .. }) => {
                    self.publish_phone_state_now().await;
                    Ok(HostReply::data(serde_json::json!({ "cancelled": false })))
                }
                Ok(ForkEntryOutcome::Cancelled) => {
                    Ok(HostReply::data(serde_json::json!({ "cancelled": true })))
                }
                Err(error) => Err(error),
            },
            None => {
                self.apply_session_command(&SessionCommandAction::Fork {
                    entry_id: None,
                    position: None,
                    with_session: None,
                })
                .await?;
                self.publish_phone_state_now().await;
                Ok(HostReply::data(serde_json::json!({ "cancelled": false })))
            }
        }
    }

    async fn phone_list_sessions_command(
        &self,
        include_subagents: bool,
    ) -> Result<HostReply, String> {
        let mut listing = self.phone_session_list_json().await;
        if !include_subagents {
            if let Some(sessions) = listing
                .get_mut("sessions")
                .and_then(JsonValue::as_array_mut)
            {
                sessions.retain(|session| {
                    session.get("isSubagent").and_then(JsonValue::as_bool) != Some(true)
                });
            }
        }
        if let Some(phone) = self.phone_plugin() {
            phone.publish_session_list(listing.clone());
        }
        Ok(HostReply::data(listing))
    }

    async fn phone_new_session_command(
        &self,
        parent_session: Option<String>,
    ) -> Result<HostReply, String> {
        phone_plugin_note_write(self);
        self.apply_session_command(&SessionCommandAction::NewSession {
            parent_session,
            with_session: None,
        })
        .await?;
        self.publish_phone_state_now().await;
        Ok(HostReply::default())
    }

    async fn phone_parent_new_command(&self) -> Result<HostReply, String> {
        self.phone_new_session_command(None).await
    }

    async fn phone_switch_session_command(&self, session_path: &str) -> Result<HostReply, String> {
        if session_path.trim().is_empty() {
            return Err("switch_session requires a sessionPath".to_owned());
        }
        phone_plugin_note_write(self);
        self.apply_session_command(&SessionCommandAction::SwitchSession {
            session_path: session_path.to_owned(),
            with_session: None,
        })
        .await?;
        self.publish_phone_state_now().await;
        Ok(HostReply::default())
    }

    async fn phone_set_session_name_command(&self, name: &str) -> Result<HostReply, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("Session name cannot be empty".to_owned());
        }
        phone_plugin_note_write(self);
        self.set_session_name(name)
            .await
            .map_err(|error| error.to_string())?;
        self.publish_phone_state_now().await;
        Ok(HostReply::default())
    }

    async fn phone_load_earlier_command(
        &self,
        before: Option<usize>,
        count: Option<usize>,
    ) -> Result<HostReply, String> {
        let messages = self.transcript_json().await;
        let total = messages.len();
        let count = count
            .unwrap_or(SNAPSHOT_MESSAGE_WINDOW)
            .clamp(1, EARLIER_MESSAGE_PAGE_MAX);
        // `before` is the index (in the full transcript) of the oldest message
        // the client currently holds.
        let end = before
            .map(|before| before.min(total))
            .unwrap_or_else(|| total.saturating_sub(SNAPSHOT_MESSAGE_WINDOW));
        let start = end.saturating_sub(count);
        let page = messages[start..end].to_vec();
        Ok(HostReply::broadcast_event(serde_json::json!({
            "type": "earlier-messages",
            "sessionWorkerId": "parent-session",
            "messages": page,
            "startIndex": start,
            "endIndex": end,
            "hasMore": start > 0,
            "totalMessageCount": total,
        })))
    }

    /// pi-RPC passthrough for everything the phone client sends that has a
    /// session-shaped equivalent. Unknown commands fail honestly.
    async fn phone_raw_command(self: &Arc<Self>, command: &JsonValue) -> Result<HostReply, String> {
        let command_type = command
            .get("type")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| "Missing rpc command type.".to_owned())?;
        match command_type {
            "get_state" => Ok(HostReply::data(self.phone_state_json().await)),
            "get_messages" => Ok(HostReply::data(serde_json::json!({
                "messages": self.transcript_json().await,
            }))),
            "get_commands" => Ok(HostReply::data(serde_json::json!({
                "commands": self.phone_command_catalog(),
            }))),
            "get_available_models" => Ok(HostReply::data(serde_json::json!({
                "models": self
                    .available_models()
                    .iter()
                    .map(phone_model_json)
                    .collect::<Vec<_>>(),
            }))),
            "get_session_stats" | "stats" => {
                let stats = self.session_stats().await;
                Ok(HostReply::data(phone_stats_json(&stats)))
            }
            "get_tree" | "tree" => self.phone_tree_command().await,
            "get_entries" => {
                let entries = self
                    .with_session_manager(|manager| {
                        serde_json::json!({
                            "entries": serde_json::to_value(manager.entries())
                                .unwrap_or(JsonValue::Null),
                            "leafId": manager.leaf_id(),
                        })
                    })
                    .await
                    .ok_or_else(|| "no attached session storage".to_owned())?;
                Ok(HostReply::data(entries))
            }
            "get_fork_messages" => {
                let messages = self
                    .with_session_manager(|manager| {
                        manager
                            .build_context_entries_with_ids()
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|(_, message)| matches!(message, Message::User(_)))
                            .filter_map(|(entry_id, message)| {
                                let text = phone_user_message_text(&message);
                                if text.trim().is_empty() {
                                    return None;
                                }
                                Some(serde_json::json!({
                                    "entryId": entry_id,
                                    "text": text,
                                }))
                            })
                            .collect::<Vec<_>>()
                    })
                    .await
                    .ok_or_else(|| "no attached session storage".to_owned())?;
                Ok(HostReply::data(serde_json::json!({ "messages": messages })))
            }
            "get_last_assistant_text" => Ok(HostReply::data(serde_json::json!({
                "text": self.last_assistant_text().await,
            }))),
            "get_available_thinking_levels" => Ok(HostReply::data(serde_json::json!({
                "levels": self.available_thinking_levels(),
            }))),
            "clear_queue" => {
                let cleared = self.clear_queue().await;
                Ok(HostReply::data(
                    serde_json::to_value(cleared).unwrap_or(JsonValue::Null),
                ))
            }
            "compact" => {
                let instructions = command
                    .get("customInstructions")
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned);
                self.phone_compact_command(instructions)
            }
            "prompt" => {
                let message = command
                    .get("message")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let images = command
                    .get("images")
                    .and_then(JsonValue::as_array)
                    .cloned()
                    .unwrap_or_default();
                let behavior = command
                    .get("streamingBehavior")
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned);
                self.phone_prompt_command(message, images, behavior)
            }
            "abort" => self.phone_abort_command().await,
            "set_model" => {
                let provider = command
                    .get("provider")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                let model_id = command
                    .get("modelId")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                self.phone_set_model_command(provider, model_id).await
            }
            "set_thinking_level" => {
                let level = command
                    .get("level")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                self.phone_set_thinking_command(level).await
            }
            "set_session_name" => {
                let name = command
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                self.phone_set_session_name_command(name).await
            }
            "new_session" => {
                let parent = command
                    .get("parentSession")
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned);
                self.phone_new_session_command(parent).await
            }
            "switch_session" => {
                let path = command
                    .get("sessionPath")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                self.phone_switch_session_command(path).await
            }
            "fork" | "phone_open_branch_path" => {
                let entry_id = command
                    .get("entryId")
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned);
                self.phone_fork_command(entry_id).await
            }
            "slash-command" => {
                let text = command
                    .get("text")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("");
                let images = command
                    .get("images")
                    .and_then(JsonValue::as_array)
                    .cloned()
                    .unwrap_or_default();
                let behavior = command
                    .get("streamingBehavior")
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned);
                self.phone_slash_command(text, images, behavior).await
            }
            "path_suggestions" | "path-suggestions" => {
                Ok(HostReply::data(phone_path_suggestions(&self.cwd, command)))
            }
            "phone_cancel_pending_message" => Ok(HostReply::data(serde_json::json!({
                "cancelled": false,
                "message": "Queued messages cannot be recalled individually once handed \
                            to the session; use /compact or wait for delivery.",
            }))),
            other => Err(format!(
                "Command `{other}` is not supported by the live native session host."
            )),
        }
    }

    async fn phone_slash_command(
        self: &Arc<Self>,
        text: &str,
        images: Vec<JsonValue>,
        streaming_behavior: Option<String>,
    ) -> Result<HostReply, String> {
        let trimmed = text.trim();
        let Some(rest) = trimmed.strip_prefix('/') else {
            return self.phone_prompt_command(text.to_owned(), images, streaming_behavior);
        };
        let mut parts = rest.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("");
        let args = parts.next().unwrap_or("").trim();
        let is_plugin_command = self.plugins.as_ref().is_some_and(|plugins| {
            plugins
                .registry()
                .resolved_commands()
                .iter()
                .any(|command| command.def.name == name || command.invocation_name == name)
        });
        if !is_plugin_command {
            // `/skill:name` and prompt templates are expanded by the session's
            // own prompt path.
            return self.phone_prompt_command(text.to_owned(), images, streaming_behavior);
        }
        phone_plugin_note_write(self);
        let outcome = self
            .dispatch_plugin_command(name, args)
            .await
            .map_err(|error| error.to_string())?;
        if !outcome.ok {
            return Err(outcome
                .error
                .unwrap_or_else(|| format!("command `/{name}` failed")));
        }
        self.publish_phone_state_now().await;
        Ok(HostReply::data(serde_json::json!({
            "source": "extension",
            "text": outcome.error,
        })))
    }

    async fn phone_state_json(&self) -> JsonValue {
        serde_json::json!({
            "model": phone_model_json(&self.model()),
            "thinkingLevel": self.thinking_level(),
            "isStreaming": self.is_streaming(),
            "isCompacting": self.is_compacting(),
            "steeringMode": self.steering_mode(),
            "followUpMode": self.follow_up_mode(),
            "sessionFile": self.session_path().map(|path| path.display().to_string()),
            "sessionId": self.session_id(),
            "sessionName": self.session_name().await,
            "autoCompactionEnabled": self.auto_compaction_enabled(),
            "messageCount": self.message_count().await,
            "pendingMessageCount": self.pending_message_count(),
        })
    }
}
