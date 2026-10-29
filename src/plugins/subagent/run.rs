use super::*;

/// Tool execution entry point: mode validation, the project-agent trust
/// gate, then the single/parallel/chain dispatch (upstream `execute`).
/// Pre-abort handling is per-mode (below) so the parent-aborted case yields
/// the subagent-specific "Subagent was aborted" error.
pub(super) async fn execute(
    args: &JsonValue,
    ctx: &ToolContext,
    extensions: Option<ExtensionHandle>,
) -> ToolResult {
    let Some(env) = ctx.session_env.clone() else {
        return ToolResult::error(
            "subagent requires a session environment (no provider registry available)",
        );
    };
    // Borrowed-Value deserialization: `&Value` implements `serde::Deserializer`,
    // so the whole arguments object (agent / task / chain texts, the largest
    // strings this call carries) is no longer deep-cloned just to be parsed.
    let params: SubagentParams = match <SubagentParams as serde::Deserialize>::deserialize(args) {
        Ok(params) => params,
        Err(error) => return ToolResult::error(format!("Invalid subagent parameters: {error}")),
    };
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

    // Project-agent trust gate (upstream `ctx.ui.confirm`). pir's plugin host
    // has no interactive confirm dialog seam yet: when a UI declares itself
    // and the project is untrusted, the SAFE path asks the user to approve
    // the project first instead of running untrusted repo-controlled agents
    // (see the module doc). Trusted projects and
    // `confirmProjectAgents: false` skip the gate; headless runs (no UI) do
    // not prompt, matching upstream print-mode behavior.
    let confirm = params.confirm_project_agents.unwrap_or(true);
    if matches!(agent_scope, AgentScope::Project | AgentScope::Both)
        && confirm
        && env.has_ui
        && !env.project_trusted
    {
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
                    "Project-local agents require approval before running.\nAgents: {names}\nSource: {dir}\n\nProject agents are repo-controlled. Approve this project first (the project-trust prompt, or run with --approve), then retry; or pass confirmProjectAgents: false if this repository is already trusted."
                ),
                builder.build(&[]),
            );
        }
    }

    let emit = env.on_update.clone();
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
            )
            .await
        }
        _ => run_single(params, env, extensions, agents, builder, emit, ctx).await,
    }
}

/// Single mode (upstream `params.agent && params.task` branch).
async fn run_single(
    params: SubagentParams,
    env: Arc<ToolSessionEnv>,
    extensions: Option<ExtensionHandle>,
    agents: Arc<Vec<AgentConfig>>,
    builder: DetailsBuilder,
    emit: EmitUpdate,
    ctx: &ToolContext,
) -> ToolResult {
    let agent_name = params.agent.unwrap_or_default();
    let task = params.task.unwrap_or_default();
    let cwd = params
        .cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.cwd.clone());
    if ctx.is_aborted() {
        return ToolResult::error("Subagent was aborted");
    }
    let on_task_update: OnTaskUpdate = {
        let emit = emit.clone();
        let builder = builder.clone();
        Arc::new(move |result| {
            emit_partial(
                &emit,
                &builder,
                std::slice::from_ref(result),
                Some(
                    &get_final_output(&result.messages)
                        .unwrap_or_else(|| "(running...)".to_owned()),
                ),
            );
        })
    };
    let result = run_child(
        env,
        extensions,
        agents,
        agent_name,
        task,
        cwd,
        None,
        ctx.signal.clone(),
        Some(on_task_update),
    )
    .await;
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
async fn run_chain(
    chain: Vec<TaskItem>,
    env: Arc<ToolSessionEnv>,
    extensions: Option<ExtensionHandle>,
    agents: Arc<Vec<AgentConfig>>,
    builder: DetailsBuilder,
    emit: EmitUpdate,
    ctx: &ToolContext,
) -> ToolResult {
    let completed: Arc<Mutex<Vec<SingleResult>>> = Arc::new(Mutex::new(Vec::new()));
    let mut previous_output = String::new();

    for (index, step) in chain.iter().enumerate() {
        if ctx.is_aborted() {
            let results = completed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            return error_with_details("Subagent was aborted".to_owned(), builder.build(&results));
        }
        let task_with_context = apply_previous(&step.task, &previous_output);
        let cwd = step
            .cwd
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.cwd.clone());
        let on_task_update: OnTaskUpdate = {
            let emit = emit.clone();
            let builder = builder.clone();
            let completed = Arc::clone(&completed);
            Arc::new(move |result| {
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
                all.push(result.clone());
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
        let result = run_child(
            Arc::clone(&env),
            extensions.clone(),
            Arc::clone(&agents),
            step.agent.clone(),
            task_with_context,
            cwd,
            Some(index + 1),
            ctx.signal.clone(),
            Some(on_task_update),
        )
        .await;
        let failed = is_failed(&result);
        completed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(result.clone());
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
                builder.build(&results),
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
    ok_with_details(final_output, builder.build(&results))
}

/// Parallel mode (upstream `params.tasks` branch): max 8 tasks, 4
/// concurrent, per-task 50 KB model-visible output cap, live aggregate
/// updates.
async fn run_parallel(
    tasks: Vec<TaskItem>,
    env: Arc<ToolSessionEnv>,
    extensions: Option<ExtensionHandle>,
    agents: Arc<Vec<AgentConfig>>,
    builder: DetailsBuilder,
    emit: EmitUpdate,
    ctx: &ToolContext,
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
    let shared: Arc<Mutex<Vec<SingleResult>>> = Arc::new(Mutex::new(
        tasks
            .iter()
            .map(|task| SingleResult::placeholder(&task.agent, &task.task))
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
                "details": builder.build(&results),
            }));
        })
    };

    let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENCY));
    let mut handles = Vec::with_capacity(tasks.len());
    for (index, task) in tasks.iter().enumerate() {
        let env = Arc::clone(&env);
        let extensions = extensions.clone();
        let agents = Arc::clone(&agents);
        let shared = Arc::clone(&shared);
        let emit_aggregate = Arc::clone(&emit_aggregate);
        let semaphore = Arc::clone(&semaphore);
        let cwd = task
            .cwd
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.cwd.clone());
        let signal = ctx.signal.clone();
        let agent_name = task.agent.clone();
        let task_text = task.task.clone();
        let on_task_update: OnTaskUpdate = {
            let shared = Arc::clone(&shared);
            let emit_aggregate = Arc::clone(&emit_aggregate);
            Arc::new(move |result| {
                {
                    let mut slots = shared
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(slot) = slots.get_mut(index) {
                        *slot = result.clone();
                    }
                }
                emit_aggregate();
            })
        };
        handles.push(tokio::spawn(async move {
            let _permit = semaphore.acquire_owned().await;
            let result = run_child(
                env,
                extensions,
                agents,
                agent_name,
                task_text,
                cwd,
                None,
                signal,
                Some(on_task_update),
            )
            .await;
            {
                let mut slots = shared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(slot) = slots.get_mut(index) {
                    *slot = result.clone();
                }
            }
            emit_aggregate();
            result
        }));
    }

    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        results.push(match handle.await {
            Ok(result) => result,
            Err(error) => SingleResult::failed(
                "(parallel task)",
                "",
                format!("subagent task failed to run: {error}"),
                None,
            ),
        });
    }

    let details = builder.build(&results);
    if ctx
        .signal
        .as_ref()
        .is_some_and(|signal| signal.is_aborted())
    {
        return error_with_details("Subagent was aborted".to_owned(), details);
    }
    ok_with_details(aggregate_parallel_text(&results), details)
}
