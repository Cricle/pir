use super::child::ChildRunInputs;
use super::*;

/// Apply the top-level isolation/executor/merge defaults to every task item
/// and validate the resulting enum spellings before any child runs.
fn resolve_task_defaults(params: &mut SubagentParams) -> Result<(), String> {
    let default_isolation = params.isolation.clone();
    let default_merge = params.worktree_merge.clone();
    let default_executor = params.executor.clone();
    let default_tools = params.tools.clone();
    for item in params
        .chain
        .iter_mut()
        .flatten()
        .chain(params.tasks.iter_mut().flatten())
    {
        if item.isolation.is_none() {
            item.isolation = default_isolation.clone();
        }
        if item.worktree_merge.is_none() {
            item.worktree_merge = default_merge.clone();
        }
        if item.executor.is_none() {
            item.executor = default_executor.clone();
        }
        if item.tools.is_none() {
            item.tools = default_tools.clone();
        }
        Isolation::parse(item.isolation.as_deref())?;
        WorktreeMerge::parse(item.worktree_merge.as_deref())?;
        Executor::parse(item.executor.as_deref())?;
    }
    Isolation::parse(params.isolation.as_deref())?;
    WorktreeMerge::parse(params.worktree_merge.as_deref())?;
    Executor::parse(params.executor.as_deref())?;
    Ok(())
}

/// Abort the child when either the parent signal or the selected-run signal
/// fires. `AbortSignal::any` owns the watcher tasks; callers are already on
/// the async runtime.
fn combined_signal(parent: Option<&AbortSignal>, task: &AbortSignal) -> Option<AbortSignal> {
    match parent {
        Some(parent) => Some(AbortSignal::any([parent.clone(), task.clone()])),
        None => Some(task.clone()),
    }
}

fn resolve_child_cwd(raw: Option<&str>, parent: &std::path::Path) -> Result<PathBuf, String> {
    let candidate = raw
        .map(PathBuf::from)
        .unwrap_or_else(|| parent.to_path_buf());
    let candidate = if candidate.is_absolute() {
        candidate
    } else {
        parent.join(candidate)
    };
    let parent = std::fs::canonicalize(parent)
        .map_err(|error| format!("Cannot resolve subagent parent cwd: {error}"))?;
    let candidate = std::fs::canonicalize(&candidate).map_err(|error| {
        format!(
            "Cannot resolve subagent cwd {}: {error}",
            candidate.display()
        )
    })?;
    if !candidate.starts_with(&parent) {
        return Err(format!(
            "Subagent cwd {} is outside the parent workspace {}",
            candidate.display(),
            parent.display()
        ));
    }
    Ok(candidate)
}

/// gate, then the single/parallel/chain dispatch (upstream `execute`).
/// Pre-abort handling is per-mode (below) so the parent-aborted case yields
/// the subagent-specific "Subagent was aborted" error.
pub(super) async fn execute(
    args: &JsonValue,
    ctx: &ToolContext,
    extensions: Option<ExtensionHandle>,
    budget: SubagentBudget,
    activity: Arc<super::activity::SubagentActivity>,
) -> ToolResult {
    let Some(env) = ctx.session_env.clone() else {
        return ToolResult::error(
            "subagent requires a session environment (no provider registry available)",
        );
    };
    // Borrowed-Value deserialization: `&Value` implements `serde::Deserializer`,
    // so the whole arguments object (agent / task / chain texts, the largest
    // strings this call carries) is no longer deep-cloned just to be parsed.
    let mut params: SubagentParams = match <SubagentParams as serde::Deserialize>::deserialize(args)
    {
        Ok(params) => params,
        Err(error) => return ToolResult::error(format!("Invalid subagent parameters: {error}")),
    };
    if let Err(error) = resolve_task_defaults(&mut params) {
        return ToolResult::error(error);
    }
    let agent_scope = match &params.agent_scope {
        None => AgentScope::User,
        Some(value) => match AgentScope::parse(value) {
            Some(scope) => scope,
            None => {
                return ToolResult::error(format!(
                    "Invalid agentScope: \"{value}\". Expected \"user\", \"project\", or \"both\"."
                ))
            }
        },
    };

    // Agent discovery is re-read on EVERY call (upstream contract: mid-session
    // agent edits take effect immediately).
    let user_agents_dir = env
        .user_agents_dir
        .clone()
        .unwrap_or_else(|| crate::coding_agent::context::agent_dir().join("agents"));
    let discovery = discover_agents(&ctx.cwd, agent_scope, &user_agents_dir);
    let agents = Arc::new(discovery.agents);
    let builder = DetailsBuilder {
        mode: "single",
        agent_scope,
        project_agents_dir: discovery.project_agents_dir,
    };

    let has_chain = params.chain.as_ref().is_some_and(|chain| !chain.is_empty());
    let has_tasks = params.tasks.as_ref().is_some_and(|tasks| !tasks.is_empty());
    let has_single = params
        .agent
        .as_deref()
        .is_some_and(|agent| !agent.is_empty())
        && params.task.as_deref().is_some_and(|task| !task.is_empty());
    let mode_count = usize::from(has_chain) + usize::from(has_tasks) + usize::from(has_single);
    if mode_count != 1 {
        let available = agent_list(&agents);
        return ok_with_details(
            format!("Invalid parameters. Provide exactly one mode.\nAvailable agents: {available}"),
            builder.build(&[]),
        );
    }
    let mode = if has_chain {
        "chain"
    } else if has_tasks {
        "parallel"
    } else {
        "single"
    };
    let builder = DetailsBuilder { mode, ..builder };

    // Project-local agents are never allowed to bypass the trust boundary. A
    // trusted project skips the gate; an untrusted project always fails closed
    // for headless hosts or requires the host's approval surface. The model's
    // `confirmProjectAgents` field is not a trust assertion and cannot disable
    // this check.
    if matches!(agent_scope, AgentScope::Project | AgentScope::Both) && !env.project_trusted {
        let mut requested: Vec<&str> = Vec::new();
        if let Some(agent) = &params.agent {
            requested.push(agent.as_str());
        }
        if let Some(tasks) = &params.tasks {
            requested.extend(tasks.iter().map(|task| task.agent.as_str()));
        }
        if let Some(chain) = &params.chain {
            requested.extend(chain.iter().map(|step| step.agent.as_str()));
        }
        let project_requested: Vec<&AgentConfig> = requested
            .iter()
            .filter_map(|name| agents.iter().find(|agent| agent.name == *name))
            .filter(|agent| agent.source == AgentSource::Project)
            .collect();
        if !project_requested.is_empty() {
            let names = project_requested
                .iter()
                .map(|agent| agent.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let dir = builder
                .project_agents_dir
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "(unknown)".to_owned());
            return ok_with_details(
                format!(
                    "Project-local agents require approval before running.\nAgents: {names}\nSource: {dir}\n\nProject agents are repo-controlled. Approve this project first (the project-trust prompt, or run with --approve), then retry."
                ),
                builder.build(&[]),
            );
        }
    }

    // Per-call partial updates: `ToolContext::on_update` is the channel the
    // scheduler stamped with `toolCallId`/`toolName`/`args` (upstream
    // `execute(_toolCallId, params, signal, onUpdate, ctx)`), so hosts can
    // route every partial in place. `env.on_update` is the raw session-level
    // channel and has no call identity: using it made the TUI open a fresh
    // anonymous `tool` block per update (PIR-TUI-002). Fall back to the env
    // only when no context channel was wired (embedders without a scheduler).
    let emit = ctx.update_channel();
    match mode {
        // `mode` is only "chain"/"parallel" when the corresponding list was
        // validated non-empty above, so the default here is unreachable.
        "chain" => {
            run_chain(
                params.chain.clone().unwrap_or_default(),
                env,
                extensions,
                agents,
                builder,
                emit,
                ctx,
                budget,
                Arc::clone(&activity),
            )
            .await
        }
        "parallel" => {
            run_parallel(
                params.tasks.clone().unwrap_or_default(),
                env,
                extensions,
                agents,
                builder,
                emit,
                ctx,
                budget,
                Arc::clone(&activity),
            )
            .await
        }
        _ => {
            run_single(
                params, env, extensions, agents, builder, emit, ctx, budget, activity,
            )
            .await
        }
    }
}

/// Single mode (upstream `params.agent && params.task` branch).
#[allow(clippy::too_many_arguments)]
async fn run_single(
    params: SubagentParams,
    env: Arc<ToolSessionEnv>,
    extensions: Option<ExtensionHandle>,
    agents: Arc<Vec<AgentConfig>>,
    builder: DetailsBuilder,
    emit: EmitUpdate,
    ctx: &ToolContext,
    budget: SubagentBudget,
    activity: Arc<super::activity::SubagentActivity>,
) -> ToolResult {
    let agent_name = params.agent.clone().unwrap_or_default();
    let task = params.task.clone().unwrap_or_default();
    let cwd = match resolve_child_cwd(params.cwd.as_deref(), &ctx.cwd) {
        Ok(cwd) => cwd,
        Err(error) => return ToolResult::error(error),
    };
    let isolation = Isolation::parse(params.isolation.as_deref()).unwrap_or(Isolation::None);
    let worktree_merge =
        WorktreeMerge::parse(params.worktree_merge.as_deref()).unwrap_or(WorktreeMerge::Auto);
    let executor = Executor::parse(params.executor.as_deref()).unwrap_or(Executor::Inline);
    if ctx.is_aborted() {
        return ToolResult::error("Subagent was aborted");
    }
    let run = activity.begin(agent_name.clone(), task.clone(), "single", None);
    let task_signal = combined_signal(ctx.signal.as_ref(), &run.signal);
    let on_task_update: OnTaskUpdate = {
        let emit = emit.clone();
        let builder = builder.clone();
        let activity = Arc::clone(&activity);
        let run_id = run.id;
        Arc::new(move |result| {
            activity.update(run_id, &result);
            emit_partial(
                &emit,
                &builder,
                std::slice::from_ref(&result),
                Some(
                    &get_final_output(&result.messages)
                        .unwrap_or_else(|| "(running...)".to_owned()),
                ),
            );
        })
    };
    let result = run_child(ChildRunInputs {
        env,
        extensions,
        agents,
        agent_name,
        task,
        cwd,
        step: None,
        parent_signal: task_signal,
        on_task_update: Some(on_task_update),
        budget,
        isolation,
        worktree_merge,
        executor,
        tools: params.tools.clone(),
    })
    .await;
    activity.finish(run.id, &result);
    let details = builder.build(std::slice::from_ref(&result));
    if ctx
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
    {
        return error_with_details("Subagent was aborted".to_owned(), details);
    }
    if is_failed(&result) {
        return error_with_details(
            format!(
                "Agent {}: {}",
                result.stop_reason.as_deref().unwrap_or("failed"),
                result_output(&result)
            ),
            details,
        );
    }
    ok_with_details(
        get_final_output(&result.messages).unwrap_or_else(|| "(no output)".to_owned()),
        details,
    )
}

/// Chain mode (upstream `params.chain` branch): sequential steps with
/// `{previous}` substitution; stops at the first failed step.
#[allow(clippy::too_many_arguments)]
async fn run_chain(
    chain: Vec<TaskItem>,
    env: Arc<ToolSessionEnv>,
    extensions: Option<ExtensionHandle>,
    agents: Arc<Vec<AgentConfig>>,
    builder: DetailsBuilder,
    emit: EmitUpdate,
    ctx: &ToolContext,
    budget: SubagentBudget,
    activity: Arc<super::activity::SubagentActivity>,
) -> ToolResult {
    if chain.len() > MAX_CHAIN_STEPS {
        return ToolResult::error(format!(
            "Too many chain steps ({}). Max is {MAX_CHAIN_STEPS}.",
            chain.len()
        ));
    }
    let completed: Arc<Mutex<Vec<Arc<SingleResult>>>> = Arc::new(Mutex::new(Vec::new()));
    let mut previous_output = String::new();

    for (index, step) in chain.iter().enumerate() {
        if ctx.is_aborted() {
            let results = completed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            return error_with_details(
                "Subagent was aborted".to_owned(),
                builder.build_arcs(&results),
            );
        }
        let task_with_context = apply_previous(&step.task, &previous_output);
        let cwd = match resolve_child_cwd(step.cwd.as_deref(), &ctx.cwd) {
            Ok(cwd) => cwd,
            Err(error) => {
                let failed = Arc::new(SingleResult::failed(
                    &step.agent,
                    &task_with_context,
                    error,
                    Some(index + 1),
                ));
                let snapshot = {
                    let mut guard = completed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.push(failed);
                    guard.clone()
                };
                return error_with_details(
                    "Subagent cwd rejected".to_owned(),
                    builder.build_arcs(&snapshot),
                );
            }
        };
        let isolation = Isolation::parse(step.isolation.as_deref()).unwrap_or(Isolation::None);
        let worktree_merge =
            WorktreeMerge::parse(step.worktree_merge.as_deref()).unwrap_or(WorktreeMerge::Auto);
        let executor = Executor::parse(step.executor.as_deref()).unwrap_or(Executor::Inline);
        // Emit the running-step placeholder as soon as the step starts: the
        // completed steps plus exactly this step (upstream renders
        // `─── Step N: agent ⏳` immediately, PIR-TUI-005).
        {
            let mut snapshot = completed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            snapshot.push(Arc::new(SingleResult::placeholder_for_step(
                &step.agent,
                &task_with_context,
                index + 1,
            )));
            emit_partial(&emit, &builder, &snapshot, Some("(running...)"));
        }
        let run = activity.begin(
            step.agent.clone(),
            task_with_context.clone(),
            "chain",
            Some(index + 1),
        );
        let task_signal = combined_signal(ctx.signal.as_ref(), &run.signal);
        let on_task_update: OnTaskUpdate = {
            let emit = emit.clone();
            let builder = builder.clone();
            let completed = Arc::clone(&completed);
            let activity = Arc::clone(&activity);
            let run_id = run.id;
            Arc::new(move |result| {
                activity.update(run_id, &result);
                // Upstream shows the completed steps plus exactly ONE entry
                // for the running step: the latest snapshot REPLACES any
                // earlier partial of the same step (rebuilt from the
                // completed list on every update, so the details array never
                // grows with the child's turn count).
                let completed = completed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                let mut all = completed;
                all.push(Arc::clone(&result));
                emit_partial(
                    &emit,
                    &builder,
                    &all,
                    Some(
                        &get_final_output(&result.messages)
                            .unwrap_or_else(|| "(running...)".to_owned()),
                    ),
                );
            })
        };
        let result = run_child(ChildRunInputs {
            env: Arc::clone(&env),
            extensions: extensions.clone(),
            agents: Arc::clone(&agents),
            agent_name: step.agent.clone(),
            task: task_with_context,
            cwd,
            step: Some(index + 1),
            parent_signal: task_signal,
            on_task_update: Some(on_task_update),
            budget,
            isolation,
            worktree_merge,
            executor,
            tools: step.tools.clone(),
        })
        .await;
        activity.finish(run.id, &result);
        let failed = is_failed(&result);
        completed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Arc::new(result.clone()));
        if failed {
            let results = completed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            return error_with_details(
                format!(
                    "Chain stopped at step {} ({}): {}",
                    index + 1,
                    step.agent,
                    result_output(&result)
                ),
                builder.build_arcs(&results),
            );
        }
        previous_output = get_final_output(&result.messages).unwrap_or_default();
    }

    let results = completed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let final_output = results
        .last()
        .and_then(|result| get_final_output(&result.messages))
        .unwrap_or_else(|| "(no output)".to_owned());
    ok_with_details(final_output, builder.build_arcs(&results))
}

/// Parallel mode (upstream `params.tasks` branch): max 8 tasks, 4
/// concurrent, per-task 50 KB model-visible output cap, live aggregate
/// updates.
#[allow(clippy::too_many_arguments)]
async fn run_parallel(
    tasks: Vec<TaskItem>,
    env: Arc<ToolSessionEnv>,
    extensions: Option<ExtensionHandle>,
    agents: Arc<Vec<AgentConfig>>,
    builder: DetailsBuilder,
    emit: EmitUpdate,
    ctx: &ToolContext,
    budget: SubagentBudget,
    activity: Arc<super::activity::SubagentActivity>,
) -> ToolResult {
    if tasks.len() > MAX_PARALLEL_TASKS {
        return ok_with_details(
            format!(
                "Too many parallel tasks ({}). Max is {MAX_PARALLEL_TASKS}.",
                tasks.len()
            ),
            builder.build(&[]),
        );
    }
    if ctx.is_aborted() {
        return ToolResult::error("Subagent was aborted");
    }

    // Running placeholders (upstream `exitCode: -1` slots), updated live.
    // `Arc` slots: an update replaces a pointer instead of deep-copying the
    // child transcript into the shared list.
    let shared: Arc<Mutex<Vec<Arc<SingleResult>>>> = Arc::new(Mutex::new(
        tasks
            .iter()
            .map(|task| Arc::new(SingleResult::placeholder(&task.agent, &task.task)))
            .collect(),
    ));
    let emit_aggregate: Arc<dyn Fn() + Send + Sync> = {
        let emit = emit.clone();
        let builder = builder.clone();
        let shared = Arc::clone(&shared);
        Arc::new(move || {
            let Some(emit) = &emit else {
                return;
            };
            let results = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let running = results.iter().filter(|result| result.pending).count();
            let done = results.len() - running;
            emit(serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": format!("Parallel: {done}/{} done, {running} running...", results.len()),
                }],
                "details": builder.build_light(&results),
            }));
        })
    };

    // The initial partial must show every task slot (upstream spawns all
    // placeholders before any child event arrives, PIR-TUI-005).
    emit_aggregate();

    let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENCY));
    // `JoinSet` is both awaitable and abort-on-drop: normal completion is
    // observed through `join_next_with_id`, and dropping the `run_parallel`
    // future (outer tool cancellation / session shutdown) drops the set,
    // which aborts every still-running child. The children therefore cannot
    // outlive their parent future and keep holding `ChildRuntimeSlot`s or
    // spending provider tokens (C1-03; the old detached `Vec<JoinHandle>` did
    // exactly that).
    let mut children: tokio::task::JoinSet<(usize, SingleResult)> = tokio::task::JoinSet::new();
    let mut task_indices: std::collections::HashMap<tokio::task::Id, usize> =
        std::collections::HashMap::with_capacity(tasks.len());
    let mut run_ids: Vec<u64> = Vec::with_capacity(tasks.len());
    for (index, task) in tasks.iter().enumerate() {
        let env = Arc::clone(&env);
        let extensions = extensions.clone();
        let agents = Arc::clone(&agents);
        let shared = Arc::clone(&shared);
        let emit_aggregate = Arc::clone(&emit_aggregate);
        let semaphore = Arc::clone(&semaphore);
        let cwd = match resolve_child_cwd(task.cwd.as_deref(), &ctx.cwd) {
            Ok(cwd) => cwd,
            Err(error) => return ToolResult::error(error),
        };
        let isolation = Isolation::parse(task.isolation.as_deref()).unwrap_or(Isolation::None);
        let worktree_merge =
            WorktreeMerge::parse(task.worktree_merge.as_deref()).unwrap_or(WorktreeMerge::Auto);
        let executor = Executor::parse(task.executor.as_deref()).unwrap_or(Executor::Inline);
        let task_tools = task.tools.clone();
        let run = activity.begin(task.agent.clone(), task.task.clone(), "parallel", None);
        run_ids.push(run.id);
        let signal = combined_signal(ctx.signal.as_ref(), &run.signal);
        let agent_name = task.agent.clone();
        let task_text = task.task.clone();
        let activity_update = Arc::clone(&activity);
        let run_id = run.id;
        let on_task_update: OnTaskUpdate = {
            let shared = Arc::clone(&shared);
            let emit_aggregate = Arc::clone(&emit_aggregate);
            Arc::new(move |result| {
                activity_update.update(run_id, &result);
                {
                    let mut slots = shared
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(slot) = slots.get_mut(index) {
                        *slot = Arc::clone(&result);
                    }
                }
                emit_aggregate();
            })
        };
        let activity_finish = Arc::clone(&activity);
        let handle = children.spawn(async move {
            let _permit = semaphore.acquire_owned().await;
            let result = run_child(ChildRunInputs {
                env,
                extensions,
                agents,
                agent_name,
                task: task_text,
                cwd,
                step: None,
                parent_signal: signal,
                on_task_update: Some(on_task_update),
                budget,
                isolation,
                worktree_merge,
                executor,
                tools: task_tools,
            })
            .await;
            activity_finish.finish(run_id, &result);
            {
                let mut slots = shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(slot) = slots.get_mut(index) {
                    *slot = Arc::new(result.clone());
                }
            }
            emit_aggregate();
            (index, result)
        });
        task_indices.insert(handle.id(), index);
    }
    activity.notify_parallel_started(tasks.len());

    let mut results: Vec<Option<SingleResult>> = (0..tasks.len()).map(|_| None).collect();
    while let Some(joined) = children.join_next_with_id().await {
        match joined {
            Ok((_task_id, (index, result))) => {
                if let Some(slot) = results.get_mut(index) {
                    *slot = Some(result);
                }
            }
            Err(error) => {
                let index = task_indices.get(&error.id()).copied();
                let (agent, task_text) = index
                    .and_then(|index| tasks.get(index))
                    .map(|task| (task.agent.as_str(), task.task.as_str()))
                    .unwrap_or(("(parallel task)", ""));
                let failed = SingleResult::failed(
                    agent,
                    task_text,
                    format!("subagent task failed to run: {error}"),
                    None,
                );
                if let Some(index) = index {
                    if let Some(run_id) = run_ids.get(index) {
                        activity.finish(*run_id, &failed);
                    }
                    if let Some(slot) = results.get_mut(index) {
                        *slot = Some(failed);
                    }
                }
            }
        }
    }
    // Defensive: a panic in a task without a resolvable id must still occupy
    // its slot rather than silently shrinking the result list.
    let results: Vec<SingleResult> = results
        .into_iter()
        .enumerate()
        .map(|(index, result)| {
            result.unwrap_or_else(|| {
                let task = &tasks[index];
                SingleResult::failed(
                    &task.agent,
                    &task.task,
                    "subagent task failed to run: task ended without a result".to_owned(),
                    None,
                )
            })
        })
        .collect();

    let details = builder.build(&results);
    if ctx
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
    {
        return error_with_details("Subagent was aborted".to_owned(), details);
    }
    // Any failed parallel leg makes the whole batch an error. Previously a
    // batch of all-failed workers still returned success, which let foreman
    // treat a broken worker as verified input. Keep the aggregate text and
    // details so callers can still see every leg's outcome.
    if results.iter().any(is_failed) {
        return error_with_details(aggregate_parallel_text(&results), details);
    }
    ok_with_details(aggregate_parallel_text(&results), details)
}
