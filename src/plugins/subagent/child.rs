use super::*;

/// Sets its flag on drop so the abort watcher never outlives the run.
struct DoneGuard(Arc<AtomicBool>);

impl Drop for DoneGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Child session identity (newtype; never a bare `u64` at use sites).
/// Allocated per spawned child runtime and surfaced to the child's
/// extensions as its `session_id` (upstream's child process owns a session
/// id of its own even with `--no-session`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SubagentSessionId(u64);

impl SubagentSessionId {
    /// Allocate the next child session id (monotonic per process).
    fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::SeqCst))
    }
}

impl std::fmt::Display for SubagentSessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "subagent-{}", self.0)
    }
}

/// One live slot under the [`MAX_CHILD_RUNTIMES`] cap. The tokio semaphore
/// permit IS the guard: dropping it releases the slot. The permit field is
/// never read — it is held purely for its `Drop`.
#[allow(dead_code)]
struct ChildRuntimeSlot(tokio::sync::SemaphorePermit<'static>);

/// Take one child-runtime slot under the global cap; `None` when the cap is
/// reached (the caller falls back to sharing the parent's handle — never
/// blocks, never fails the run).
fn try_acquire_child_runtime_slot() -> Option<ChildRuntimeSlot> {
    CHILD_RUNTIME_SEM.try_acquire().ok().map(ChildRuntimeSlot)
}

/// One child session's ISOLATED extension runtime, owned by `run_child` for
/// the whole child run. Ownership expresses the lifecycle: there is no
/// manual shutdown call chain — dropping this value tears the runtime down.
/// The [`ExtensionHandle`] field's last-clone drop closes the actor mailbox,
/// which fires `session_shutdown {reason: "quit"}` to the child's extensions
/// and applies the stale-instance protocol on the actor thread
/// ([`ExtensionHandle`]'s documented drop semantics); [`Drop`] here runs the
/// synchronous half (captured `pi`/`ctx` objects stop working immediately)
/// and the semaphore permit's drop releases the global cap slot.
///
/// Public only as the `#[doc(hidden)]` spawn seam
/// ([`spawn_child_runtime`]); production code reaches it through
/// `run_child`.
#[doc(hidden)]
pub struct ChildRuntime {
    #[allow(dead_code)]
    session_id: SubagentSessionId,
    handle: ExtensionHandle,
    _slot: ChildRuntimeSlot,
}

impl ChildRuntime {
    /// The child's own runtime handle (clone into the child session config).
    #[doc(hidden)]
    pub fn handle(&self) -> &ExtensionHandle {
        &self.handle
    }
}

impl Drop for ChildRuntime {
    fn drop(&mut self) {
        // Stale-instance protocol, synchronous half: all captured `pi`/`ctx`
        // objects of the child runtime throw from here on. The actor thread
        // completes the teardown (session_shutdown emit + invalidate) when
        // this handle — its last mailbox sender — drops right after.
        self.handle.invalidate();
    }
}

/// Spawn a FRESH extension runtime for one child session (upstream's
/// per-child-process runtime, plan A of the isolation design).
///
/// The new actor loads the same extension path plan the parent's host
/// recorded (CLI `-e` paths, then settings/default paths, `builtin:<name>`
/// entries included in order — [`crate::plugins::partition_paths`] splits
/// builtins from on-disk files exactly like
/// [`crate::agent_session::load_extension_runtime`]) but is built from an
/// [`crate::extensions::ExtensionConfig`] whose `cwd` is the child's task
/// cwd and whose `session_id` names the child session. Consequences: the
/// child's `session_start`/`session_shutdown` lifecycle and its settings
/// snapshot (re-read for the child cwd by the child session build) land on
/// its own actor and can never reach the parent's JS handlers or replace
/// the parent's `getSettings` snapshot. (`project_trust` is deliberately
/// NOT re-consulted per child: the trust decision belongs to the host and
/// was already made for the parent run.)
///
/// Returns `None` — and the caller then shares the parent's handle — when:
/// the parent carries no recorded path plan (a directly started handle, as
/// in tests), the global runtime cap is reached, the runtime cannot start, or
/// the load fails. On every early return the partially built runtime is
/// dropped, which shuts its actor down cleanly.
///
/// The path plan is partitioned BEFORE the runtime starts, exactly like
/// [`crate::agent_session::load_extension_runtime`]: a builtin-only plan
/// (`builtin:<name>` entries only, no on-disk JS path) builds a plan-only
/// handle. Starting an actor unconditionally would construct the QuickJS
/// runtime + prelude for every subagent child of a builtin-only session — the
/// ~10 MB cost `start_plan_only` exists to avoid. A plan that does carry an
/// on-disk JS extension keeps the full actor (and, in a build without the
/// `extensions` feature, `start` still fails and the child falls back to the
/// parent's handle as before).
///
/// `catalog` is the parent's serialized model catalog (models + providers,
/// the same shape `AgentSession::push_session_context` pushes). It is applied
/// to the fresh handle BEFORE [`ExtensionHandle::load`]: extension factories
/// evaluate synchronously during load, and the installed `pi-subagents`
/// model-resolver shim calls `modelRuntime.getModels()` at that point. With
/// no pre-load push it sees an empty list and fails the child with
/// "No models available. Check your installation or add models to
/// models.json.". The late child-session context push refreshes the same
/// snapshot once the child `AgentSession` exists, but cannot rescue a failure
/// that already happened during load.
///
/// `#[doc(hidden)]` as the tests' spawn seam (see
/// `tests/subagent_model_catalog.rs`); production calls it from
/// [`run_child`].
#[doc(hidden)]
pub async fn spawn_child_runtime(
    parent: &ExtensionHandle,
    cwd: &Path,
    catalog: &ModelCatalog,
) -> Option<ChildRuntime> {
    let paths = parent.path_plan();
    if paths.is_empty() {
        return None;
    }
    let slot = try_acquire_child_runtime_slot()?;
    let session_id = SubagentSessionId::next();
    let mut config = parent.config();
    config.cwd = cwd.to_path_buf();
    config.session_id = Some(session_id.to_string());
    let (builtin_names, file_paths) = crate::plugins::partition_paths(&paths);
    // A print-mode parent may already know these paths fail at load (persisted
    // negative cache). Skipping them here keeps the child plan-only instead of
    // constructing a full actor for a runtime that will never load anything.
    let (file_paths, skipped) =
        crate::agent_session::partition_cached_extension_failures(file_paths, &config.mode);
    for (path, reason) in &skipped {
        crate::ext_diag!(
            "[ext] subagent child runtime: skipping cached extension failure {}: {}",
            path.display(),
            reason
        );
    }
    let handle = if file_paths.is_empty() {
        // Builtin-only plan: shared state + path plan, no actor and no
        // QuickJS runtime. `load(&[])` below is a no-op success.
        ExtensionHandle::start_plan_only(config)
    } else {
        match ExtensionHandle::start(config) {
            Ok(handle) => handle,
            Err(error) => {
                crate::ext_diag!("[ext] subagent child runtime failed to start: {error}");
                return None;
            }
        }
    };
    handle.set_path_plan(paths);
    // ORDERING (the fix): the catalog must be on the runtime BEFORE the load
    // command reaches the actor. Extension factories run synchronously there
    // and the `pi-subagents` model-resolver shim resolves `getModels()` during
    // its factory, so a post-load push is too late (the factory already threw
    // "No models available..."). The child session's own context push later
    // re-applies the same snapshot; applying it here is the only way a
    // load-time resolver can succeed.
    catalog.apply(&handle);
    if !builtin_names.is_empty() {
        // `builtin:<name>` entries that name no registered builtin surface
        // the upstream error text (mirrors the host load path).
        let mut diagnostics = Vec::new();
        crate::plugins::unknown_builtin_errors(&builtin_names, &mut diagnostics);
        for diagnostic in &diagnostics {
            crate::ext_diag!("[ext] subagent {}: {}", diagnostic.path, diagnostic.message);
        }
    }
    match handle.load(&file_paths).await {
        Ok(result) => {
            for error in &result.errors {
                crate::ext_diag!(
                    "[ext] subagent child runtime failed to load {}: {}",
                    error.path.display(),
                    error.error
                );
            }
            Some(ChildRuntime {
                session_id,
                handle,
                _slot: slot,
            })
        }
        Err(error) => {
            crate::ext_diag!("[ext] subagent child runtime load failed: {error}");
            // Drop the unusable runtime (actor shutdown) and fall back.
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Nesting-depth guard (builtin audit P1-6)
// ---------------------------------------------------------------------------

/// Maximum subagent nesting depth. A root session runs at depth 0; a child
/// session entered by the `subagent` tool is exactly one deeper than the
/// session that dispatched it. Entering a child at depth
/// [`MAX_SUBAGENT_DEPTH`] is refused, so root → A → B is allowed (B at depth
/// 2) and the attempt made by B to enter another child (depth 3) is the one
/// refused. The audit's A→B→C numbering counts the root as level 1, which is
/// the same chain: A(root) → B → C allowed, D refused.
pub const MAX_SUBAGENT_DEPTH: usize = 3;

/// The single hidden tool name that carried a session's nesting depth before
/// it became registry metadata. Kept only so a child registry inherited from
/// a dispatcher built by the old representation is scrubbed.
const DEPTH_MARKER_TOOL: &str = "__pir_subagent_depth";

/// Refusal text; names the configured limit so callers can surface it
/// verbatim (the deepest `SingleResult` carries it as `error_message`/
/// `stderr`, and single mode renders it as `Agent error: ...`).
fn depth_limit_message() -> String {
    format!("Subagent nesting depth limit ({MAX_SUBAGENT_DEPTH}) reached")
}

/// Record `depth` as the subagent nesting depth of the session that owns
/// `registry`.
///
/// The depth is PER-SESSION state carried as registry METADATA
/// ([`ToolRegistry::set_session_depth`]), not a registered tool: a child
/// session is built from its own registry, so parallel siblings (each a
/// session of its own, possibly spawned onto different tokio tasks) can never
/// observe or corrupt each other's level, and a detached/background child
/// carries its level with the session env it runs under. There is no shared
/// cell to leak on abort/timeout/error: dropping the session drops the
/// registry, and a refused depth never builds a session at all.
///
/// It must not be a registered tool: the session build strips
/// [`ToolExposure::Hidden`] tools from the base registry before that registry
/// reaches [`ToolSessionEnv::base_tools`], which deleted an earlier
/// `Hidden` marker and left the guard dead (every child read depth 0). As
/// metadata the depth survives that strip, is cloned with the registry, and
/// still never reaches the model — it is not a declaration, has no name and
/// no parameters, and [`ToolRegistry::iter`] never yields it.
///
/// Public as a test/embedder seam ([`subagent_depth`] reads it back);
/// production calls it from [`run_child`] when it builds a child's tool
/// registry.
#[doc(hidden)]
pub fn install_subagent_depth(registry: &mut ToolRegistry, depth: usize) {
    registry.set_session_depth(depth);
}

/// The subagent nesting depth recorded in a session's base tool registry
/// (0 for a root session).
#[doc(hidden)]
pub fn subagent_depth(registry: &ToolRegistry) -> usize {
    registry.session_depth()
}

/// The depth of the session that owns `env` (the dispatcher of this child).
fn session_depth(env: &ToolSessionEnv) -> usize {
    subagent_depth(&env.base_tools)
}

/// Run one task in a fresh, isolated, in-process ephemeral child session
/// (upstream `runSingleAgent`; see the module doc for the in-process
/// deviation).
///
/// The returned future is HEAP-BOXED (`Box::pin`), and the child prompt it
/// awaits is boxed at the recursion point too: a nested subagent call polls
/// `run_child` under the parent child's poll stack, so keeping each level's
/// (large) state machine inline would grow the stack per nesting level and
/// overflow libtest's default test-thread stack even one level deep (builtin
/// audit P1-6). With both boundaries boxed, only a handful of small poll
/// frames remain per level and the depth guard bounds the chain.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_child(
    env: Arc<ToolSessionEnv>,
    extensions: Option<ExtensionHandle>,
    agents: Arc<Vec<AgentConfig>>,
    agent_name: String,
    task: String,
    cwd: PathBuf,
    step: Option<usize>,
    parent_signal: Option<AbortSignal>,
    on_task_update: Option<OnTaskUpdate>,
) -> BoxFuture<'static, SingleResult> {
    Box::pin(async move {
        // Refuse BEFORE any agent/session/runtime work: the depth check is
        // pure registry metadata, so a refused depth never allocates a child
        // session, a child extension runtime slot, or an abort watcher.
        let child_depth = session_depth(&env).saturating_add(1);
        if child_depth >= MAX_SUBAGENT_DEPTH {
            return SingleResult::failed(&agent_name, &task, depth_limit_message(), step);
        }

        let Some(agent) = agents.iter().find(|agent| agent.name == agent_name) else {
            return SingleResult::failed(
                &agent_name,
                &task,
                format!(
                    "Unknown agent: \"{agent_name}\". Available agents: {}.",
                    agent_names_quoted(&agents)
                ),
                step,
            );
        };

        // Model: the agent's own override or the parent's (upstream dispatch
        // defaults). Thinking level is inherited from the parent only when the
        // agent has no own model (upstream passes no `--thinking` flag then, so
        // the child falls back to its default thinking level — upstream
        // `DEFAULT_THINKING_LEVEL`, medium — instead of turning thinking off).
        let inherits_dispatch = agent.model.is_none();
        let model = match &agent.model {
            Some(spec) => match resolve_model(&env.provider_registry, spec) {
                Some(model) => model,
                None => {
                    return SingleResult::failed(
                        &agent_name,
                        &task,
                        format!("Unknown model: \"{spec}\""),
                        step,
                    )
                }
            },
            None => env.model.clone(),
        };

        // System prompt: pir's default (context discovery for the child cwd and
        // tools) with the agent body APPENDED (upstream `--append-system-prompt`
        // semantics). An empty body keeps the child's own default prompt.
        //
        // Ambient boundary: the in-process child session stands in for upstream's
        // fresh child PROCESS (which inherits the environment), so the prompt's
        // context-file/skill/trust discovery resolves against the ENCLOSING
        // session's agent directory (propagated as `ToolSessionEnv::agent_dir`)
        // instead of re-reading the process environment. `user_agents_dir` is
        // the `<agentDir>/agents` discovery dir and `project_trusted` is the
        // PARENT's decision while the prompt re-resolves trust for the child's
        // own cwd.
        let agent_dir = env
            .agent_dir
            .clone()
            .unwrap_or_else(crate::coding_agent::context::agent_dir);
        let mut child_tools = child_tool_registry(&env, agent);
        // Drop the dispatcher's inherited depth marker BEFORE deriving the
        // prompt's tool infos. The depth itself is registry metadata now, so
        // it can never appear as a tool — this only scrubs a stale
        // `DEPTH_MARKER_TOOL` registration inherited from a registry built by
        // the former hidden-tool representation.
        child_tools.remove(DEPTH_MARKER_TOOL);
        // The tool selection the child session will carry (built from the same
        // frontmatter allowlist the child registry filter used) is computed
        // FIRST, because the prompt must advertise exactly the tools the
        // session DECLARES — the D6 rule the session build applies
        // (`declared_tool_infos` / `rust_hidden_declarations` in
        // `src/agent_session/build.rs`; upstream `_rebuildSystemPrompt`'s
        // `selectedTools: validToolNames` plus `hiddenTools`,
        // agent-session.ts:1705-1731). A `None`/empty allowlist means
        // registration-active (`active_names(None)`), NOT every registered
        // tool.
        let tool_selection = agent
            .tools
            .as_deref()
            .filter(|tools| !tools.is_empty())
            .map(|tools| ToolSelection {
                enabled: tools.to_vec(),
                // An explicit allowlist: no `+name`/`-name` entries and no
                // resume restore (upstream's `usesDefaultTools` gate is false
                // for `--tools <names>`).
                ..Default::default()
            });
        // The depth is registry metadata, so it never joins `active_names`,
        // `declared_tool_infos` or `rust_hidden_declarations`: the prompt
        // cannot mention it whatever the selection says. The
        // `prepareLoadout` hidden declarations of the declared tools leave the
        // `<tools>`/`<rules>` sections exactly as they leave the child
        // session's request declarations (codemode mode `"only"`).
        let active = child_tools.active_names(
            tool_selection
                .as_ref()
                .map(|selection| selection.enabled.as_slice()),
        );
        let prompt_tools = declared_tool_infos(&child_tools, &active);
        let hidden_tools = rust_hidden_declarations(&child_tools, &active);
        let base_prompt = crate::coding_agent::context::context_system_prompt_with_hidden_in(
            &cwd,
            &prompt_tools,
            &hidden_tools,
            &agent_dir,
        );
        let body = agent.system_prompt.trim();
        let system_prompt = match (base_prompt, body.is_empty()) {
            (_, true) => None,
            (Some(base), false) => Some(format!("{base}\n\n{body}")),
            (None, false) => Some(body.to_owned()),
        };
        // The child's own depth rides its base tool registry as metadata (see
        // [`install_subagent_depth`]): out-of-band, so it survives the session
        // build's hidden-tool strip and reaches `ToolSessionEnv::base_tools`,
        // where the child's own `subagent` dispatch reads it.
        install_subagent_depth(&mut child_tools, child_depth);

        // Per-child extension runtime isolation (upstream: a full `pi` child
        // process owns its runtime; see the module doc). `child_runtime` owns
        // the fresh actor for the whole run — dropping it (end of this fn, after
        // the child session itself) is the shutdown. When spawning is not
        // possible (no recorded plan / cap reached / start or load failure) the
        // parent's shared handle is reused for this run.
        let child_runtime = match &extensions {
            // The plan check is repeated cheaply here so a directly started
            // handle (no recorded plan, the test/embedder case) never pays the
            // credential-environment read behind the catalog build.
            Some(parent) if !parent.path_plan().is_empty() => {
                // The parent's live catalog: `get_models` already merges
                // dynamically registered providers and virtual models, so the
                // child resolves exactly the models the parent can. Auth is
                // resolved for the child cwd (the same inputs the child
                // session's `authenticated_providers` reads), keeping
                // `hasConfiguredAuth` exact even before the child session
                // pushes its context.
                let catalog = ModelCatalog::from_registry_with_authenticated_providers(
                    env.provider_registry.as_ref(),
                    &cwd,
                );
                spawn_child_runtime(parent, &cwd, &catalog).await
            }
            _ => None,
        };
        let extensions = child_runtime
            .as_ref()
            .map(|runtime| runtime.handle().clone())
            .or(extensions);

        // The parent's explicit credential is provider-scoped: inherit it only
        // while the child's own model targets the same provider.
        let api_key = env
            .credential
            .as_ref()
            .and_then(|credential| credential.key_for(&model.provider))
            .map(str::to_owned);
        let config = AgentSessionConfig {
            provider_registry: Arc::clone(&env.provider_registry),
            model,
            api_key,
            thinking: if inherits_dispatch {
                env.thinking
            } else {
                // Upstream passes no `--thinking` when the agent has its own
                // model: the child uses its CLI default thinking level (upstream
                // `defaults.ts DEFAULT_THINKING_LEVEL`, medium) — thinking is
                // NOT silently lost.
                crate::ai::providers::resolution::DEFAULT_THINKING_LEVEL
            },
            tools: child_tools,
            // The Rust performance win: an in-process ephemeral child — the
            // transcript lives in memory only, nothing is persisted (upstream
            // `--no-session`; see the module doc for the isolation deviation).
            session: None,
            compaction: None,
            // No turn cap (upstream passes no `maxTurns` to the child): the
            // value only feeds the loop's own default bound, which is
            // `usize::MAX` — i.e. the cap is removed, not relaxed.
            max_turns: usize::MAX,
            system_prompt,
            cwd: Some(cwd.clone()),
            event_stream: None,
            // The child's OWN extension runtime handle (spawned above from the
            // parent's recorded plan; falls back to the parent's shared handle
            // when a fresh runtime is not possible): the child's unified plugin
            // host re-registers the JS-extension and Rust-builtin tools on it
            // (upstream's child is a full `pi` with the complete tool surface;
            // see the module doc for the isolation design and the same-actor
            // re-entrancy analysis).
            extensions,
            // A `tools` frontmatter allowlist also gates the merged plugin tools
            // (upstream passes `--tools` to the child, which filters its whole
            // registry): the selection becomes the child's active-tool override.
            // Built above, next to the prompt it also drives.
            tool_selection,
        };
        let child = Arc::new(AgentSession::new_with_agent_dir(config, &agent_dir));
        if child_runtime.is_none() {
            // No dedicated child runtime was spawned, so `extensions` is the
            // parent's shared handle. The child host still runs its native
            // teardown, but must not terminate the parent's JS actor.
            if let Some(host) = child.plugin_host() {
                host.mark_js_shared();
            }
        }

        // Live per-message state (upstream accumulates `message_end` events).
        let state = Arc::new(Mutex::new(SingleResult {
            agent: agent_name.clone(),
            agent_source: agent.source.as_str(),
            task: task.clone(),
            messages: Vec::new(),
            usage: UsageTotals::default(),
            model: None,
            stop_reason: None,
            error_message: None,
            stderr: None,
            step,
            pending: true,
        }));

        let handler: EventHandler = {
            let state = Arc::clone(&state);
            let on_task_update = on_task_update.clone();
            Arc::new(move |event| {
                let AgentEvent::TurnEnd { message, .. } = event else {
                    return;
                };
                let mut current = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                current.usage.turns += 1;
                current.usage.input += message.usage.input;
                current.usage.output += message.usage.output;
                current.usage.cache_read += message.usage.cache_read;
                current.usage.cache_write += message.usage.cache_write;
                current.usage.cost += message.usage.cost.total;
                current.usage.context_tokens = message.usage.total_tokens;
                if current.model.is_none() && !message.model.is_empty() {
                    current.model = Some(message.model.clone());
                }
                if message.stop_reason != StopReason::Pending {
                    current.stop_reason = Some(message.stop_reason.to_string());
                }
                if let Some(error_message) = &message.error_message {
                    current.error_message = Some(error_message.clone());
                }
                current.messages.push(Message::Assistant(message.clone()));
                if let Some(on_task_update) = &on_task_update {
                    on_task_update(&current.clone());
                }
            })
        };

        // Parent-abort propagation: the watcher sets the child session's abort
        // signal; the child loop stops at its next cancellation checkpoint. The
        // watcher polls until the run settles (upstream kills the process;
        // in-process abort is cooperative — see the module doc).
        let done = Arc::new(AtomicBool::new(false));
        if let Some(parent) = &parent_signal {
            let child = Arc::clone(&child);
            let done = Arc::clone(&done);
            let parent = parent.clone();
            tokio::spawn(async move {
                while !done.load(Ordering::SeqCst) {
                    if parent.is_aborted() {
                        child.abort();
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(ABORT_POLL_MS)).await;
                }
            });
        }
        let _guard = DoneGuard(Arc::clone(&done));

        // The recursion point: this child prompt polls the child agent loop, whose
        // tool batch can call `subagent` again. Box it so the child's (large)
        // prompt/loop state lives on the heap instead of inline in this future —
        // combined with `run_child`'s own boxed future this keeps the per-level
        // poll stack flat (builtin audit P1-6).
        let outcome = Box::pin(child.prompt(&task, handler)).await;
        let messages = child.ephemeral_transcript().await;

        // Child teardown BEFORE `child_runtime` drops: the child session's plugin
        // host runs every plugin's `on_shutdown` exactly once (the `mcp`
        // builtin's graceful stdio close and process-tree sweep become reachable
        // for the child too — previously only the SDK path ran them) and, for a
        // dedicated child runtime, the JS actor's `session_shutdown` + exit. A
        // shared parent handle is marked non-owning above, so this call cannot
        // stop the parent's actor.
        child.shutdown().await;

        let mut result = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        result.pending = false;
        // Full transcript including the user message and tool results (upstream
        // collects the same set from the child's JSON-mode events).
        result.messages = messages;
        match outcome {
            Ok(terminal) => {
                if terminal.stop_reason == StopReason::Aborted
                    || parent_signal.is_some_and(|signal| signal.is_aborted())
                {
                    result.stop_reason = Some("aborted".to_owned());
                }
                if terminal.stop_reason == StopReason::Error && result.error_message.is_none() {
                    result.error_message = terminal.error_message.clone();
                }
            }
            Err(error) => {
                result.stderr = Some(error.to_string());
                if result.stop_reason.is_none() {
                    result.stop_reason = Some("error".to_owned());
                }
            }
        }
        result
    })
}

/// The tool list the child's context-derived system prompt renders: the
/// child's DECLARED (active) tools, in declaration order — upstream
/// `_rebuildSystemPrompt`'s `selectedTools: validToolNames`
/// (agent-session.ts:1705-1731).
///
/// Mirrors `declared_tool_infos` in `src/agent_session/build.rs`, the
/// canonical copy; it is private to the session build, so the subagent
/// carries its own. `hidden`-exposure tools are guarded out by
/// [`crate::agent::tool_execution::declared_tools`], so the
/// `<tools>`/`<rules>` sections list exactly the child session's declared
/// set.
fn declared_tool_infos(
    registry: &crate::agent::tool_execution::ToolRegistry,
    active: &[String],
) -> Vec<crate::system_prompt::ToolInfo> {
    crate::agent::tool_execution::declared_tools(registry, active)
        .iter()
        .map(|tool| crate::system_prompt::ToolInfo::from_agent_tool(tool.as_ref()))
        .collect()
}

/// The hidden declarations the child prompt's `hiddenTools` list carries: the
/// union of every DECLARED tool's Rust
/// [`crate::agent::tool_execution::AgentTool::prepare_loadout`] result
/// (upstream `_rebuildSystemPrompt`'s `hiddenTools: [..._hiddenDeclarations]`,
/// agent-session.ts:1727).
///
/// Mirrors `rust_hidden_declarations` in `src/agent_session/build.rs`, the
/// canonical copy. The loadout carries the declared and callable sets because
/// a hook may classify by exposure
/// (`codemode` mode `"only"` hides the active `direct` tools that stay
/// callable). JS `prepareLoadout` hooks are async and only reach the child
/// session's per-request declarations; the in-tree hiding hooks are Rust.
fn rust_hidden_declarations(
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
