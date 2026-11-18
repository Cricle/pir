//! Optional process-per-task subagent executor.
//!
//! `executor=process` runs one `pir --mode json --no-session` child per task.
//! The child receives the agent body through `--append-system-prompt`, the
//! agent's model/tool allowlist through the CLI, and the task through stdin.
//! Its JSON event stream is parsed back into the same [`SingleResult`] shape
//! the inline executor produces.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::*;

static PROMPT_SEQ: AtomicU64 = AtomicU64::new(0);
const MAX_PROCESS_LINE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PROCESS_STDERR_BYTES: u64 = 1024 * 1024;
/// Internal marker envs for process-mode children (never secrets in argv).
pub(crate) const SUBAGENT_DEPTH_ENV: &str = "PIR_SUBAGENT_DEPTH";
pub(crate) const SUBAGENT_API_KEY_ENV: &str = "PIR_SUBAGENT_API_KEY";

pub(super) struct ProcessChildInputs {
    pub env: Arc<ToolSessionEnv>,
    pub agent: AgentConfig,
    pub task: String,
    pub cwd: PathBuf,
    pub step: Option<usize>,
    pub parent_signal: Option<AbortSignal>,
    pub on_task_update: Option<OnTaskUpdate>,
    pub budget: SubagentBudget,
    pub isolation: Isolation,
    pub worktree_merge: WorktreeMerge,
    /// Current child depth computed by the parent (1-based).
    pub child_depth: usize,
}

pub(super) async fn run_child_process(inputs: ProcessChildInputs) -> SingleResult {
    let ProcessChildInputs {
        env,
        agent,
        task,
        cwd,
        step,
        parent_signal,
        on_task_update,
        budget,
        isolation,
        worktree_merge,
        child_depth,
    } = inputs;

    let worktree = if isolation == Isolation::Worktree {
        match super::worktree::prepare(&cwd, &agent.name).await {
            Ok(lease) => Some(lease),
            Err(error) => return SingleResult::failed(&agent.name, &task, error, step),
        }
    } else {
        None
    };
    if let Some(signal) = &parent_signal {
        if signal.is_aborted() {
            return SingleResult::failed(
                &agent.name,
                &task,
                "Subagent was aborted".to_owned(),
                step,
            );
        }
    }

    let model = match &agent.model {
        Some(spec) => match resolve_model(&env.provider_registry, spec) {
            Some(model) => model,
            None => {
                return SingleResult::failed(
                    &agent.name,
                    &task,
                    format!("Unknown model: \"{spec}\""),
                    step,
                )
            }
        },
        None => env.dispatch_model(),
    };
    let cwd = worktree
        .as_ref()
        .map(|lease| lease.path.clone())
        .unwrap_or(cwd);
    let prompt_file = match write_agent_prompt(&agent.name, &agent.system_prompt) {
        Ok(path) => path,
        Err(error) => {
            return SingleResult::failed(&agent.name, &task, error, step);
        }
    };

    let mut command = tokio::process::Command::new(process_program());
    command
        .arg("--mode")
        .arg("json")
        .arg("--no-session")
        .arg("--provider")
        .arg(&model.provider)
        .arg("--model")
        .arg(&model.id)
        .arg("--thinking")
        .arg(if agent.model.is_none() {
            env.dispatch_thinking().wire_key()
        } else {
            crate::ai::providers::resolution::DEFAULT_THINKING_LEVEL.wire_key()
        })
        .arg("--append-system-prompt")
        .arg(&prompt_file)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    match agent.tools.as_ref() {
        // An explicit empty allowlist means "no tools", not "inherit the
        // child's defaults".
        Some(tools) if tools.is_empty() => {
            command.arg("--no-tools");
        }
        Some(tools) => {
            command.arg("--tools").arg(tools.join(","));
        }
        None => {}
    }
    if env.project_trusted {
        command.arg("--approve");
    }
    // Pass the parent-computed depth so the child's own `session_depth` starts
    // at the right value instead of resetting to 0 across the process boundary.
    command.env(SUBAGENT_DEPTH_ENV, child_depth.to_string());
    // Inherit the parent's explicit credential only for the child's provider
    // (mirrors the inline path in child.rs). Never place the key in argv.
    if let Some(key) = env
        .credential
        .as_ref()
        .and_then(|credential| credential.key_for(&model.provider))
    {
        command.env(SUBAGENT_API_KEY_ENV, key);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = std::fs::remove_file(&prompt_file);
            return SingleResult::failed(
                &agent.name,
                &task,
                format!("Cannot start process worker: {error}"),
                step,
            );
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(error) = stdin.write_all(task.as_bytes()).await {
            let _ = child.start_kill();
            let _ = std::fs::remove_file(&prompt_file);
            return SingleResult::failed(
                &agent.name,
                &task,
                format!("Cannot send task to process worker: {error}"),
                step,
            );
        }
        let _ = stdin.shutdown().await;
    }
    let stderr = child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            // Drain the pipe so a chatty child never blocks on a full stderr
            // buffer, but retain only the diagnostic head.
            let mut reader = BufReader::new(stderr);
            let cap = MAX_PROCESS_STDERR_BYTES as usize;
            let mut kept = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(count) => {
                        if kept.len() < cap {
                            let take = count.min(cap - kept.len());
                            kept.extend_from_slice(&buf[..take]);
                        }
                    }
                }
            }
            String::from_utf8_lossy(&kept).into_owned()
        })
    });
    let Some(stdout) = child.stdout.take() else {
        let _ = child.start_kill();
        let _ = std::fs::remove_file(&prompt_file);
        return SingleResult::failed(
            &agent.name,
            &task,
            "Process worker stdout is not piped".to_owned(),
            step,
        );
    };
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let mut messages = Vec::new();
    let mut seen_assistant = std::collections::HashSet::new();
    let mut usage = UsageTotals::default();
    let mut model_id = None;
    let mut stop_reason = None;
    let mut error_message = None;
    let mut timed_out = false;
    let mut turn_budget_exceeded = false;
    let mut aborted = false;
    // The process itself is budgeted by its wall-clock/turn limits below;
    // worktree preparation already has its own bounded git calls.
    let deadline = budget.wall_clock.map(|limit| Instant::now() + limit);
    let max_turns = u64::try_from(budget.max_turns).unwrap_or(u64::MAX);
    loop {
        let read = reader.read_line(&mut line);
        tokio::pin!(read);
        let outcome = tokio::select! {
            result = &mut read => result,
            _ = wait_abort(parent_signal.clone()) => {
                aborted = true;
                break;
            }
            _ = wait_deadline(deadline) => {
                timed_out = true;
                break;
            }
        };
        let Ok(count) = outcome else {
            error_message = Some("Cannot read process worker output".to_owned());
            break;
        };
        if count == 0 {
            break;
        }
        if line.len() > MAX_PROCESS_LINE_BYTES {
            error_message = Some("Process worker emitted an oversized JSON line".to_owned());
            break;
        }
        if let Some((message, update)) = parse_event(&line) {
            if let Some(update) = update {
                if let Some(on_update) = &on_task_update {
                    let mut snapshot = SingleResult::placeholder(&agent.name, &task);
                    snapshot.pending = true;
                    snapshot.executor = "process";
                    snapshot.messages = update;
                    on_update(&snapshot);
                }
            }
            for message in message {
                if !accept_message(&mut seen_assistant, &message) {
                    continue;
                }
                if let Message::Assistant(assistant) = &message {
                    usage.turns += 1;
                    usage.input += assistant.usage.input;
                    usage.output += assistant.usage.output;
                    usage.cache_read += assistant.usage.cache_read;
                    usage.cache_write += assistant.usage.cache_write;
                    usage.cost += assistant.usage.cost.total;
                    usage.context_tokens = assistant.usage.total_tokens;
                    if model_id.is_none() && !assistant.model.is_empty() {
                        model_id = Some(assistant.model.clone());
                    }
                    if assistant.stop_reason != StopReason::Pending {
                        stop_reason = Some(assistant.stop_reason.to_string());
                    }
                    if let Some(error) = &assistant.error_message {
                        error_message = Some(error.clone());
                    }
                    if max_turns != u64::MAX && usage.turns >= max_turns {
                        turn_budget_exceeded = true;
                        messages.push(message);
                        break;
                    }
                }
                messages.push(message);
            }
            if turn_budget_exceeded {
                break;
            }
        }
        line.clear();
    }

    let status = if aborted || timed_out || turn_budget_exceeded {
        kill_process_tree(&mut child).await;
        let _ = child.wait().await;
        None
    } else {
        enum WaitOutcome {
            Exited(std::io::Result<std::process::ExitStatus>),
            Aborted,
            TimedOut,
        }
        // The child can close stdout and then hang (or keep a grandchild
        // alive). Keep the budget/abort contract active while waiting for the
        // exit status too, instead of awaiting `wait()` unconditionally.
        let mut wait = Box::pin(child.wait());
        let mut deadline_wait = Box::pin(wait_deadline(deadline));
        let outcome = tokio::select! {
            result = &mut wait => WaitOutcome::Exited(result),
            _ = wait_abort(parent_signal.clone()) => WaitOutcome::Aborted,
            _ = &mut deadline_wait => WaitOutcome::TimedOut,
        };
        drop(wait);
        drop(deadline_wait);
        match outcome {
            WaitOutcome::Exited(result) => Some(result),
            WaitOutcome::Aborted => {
                aborted = true;
                kill_process_tree(&mut child).await;
                let _ = child.wait().await;
                None
            }
            WaitOutcome::TimedOut => {
                timed_out = true;
                kill_process_tree(&mut child).await;
                let _ = child.wait().await;
                None
            }
        }
    };
    let stderr_text = match stderr {
        // A grandchild can inherit stderr and keep the pipe open after the
        // direct child exits; never let diagnostics block the task result.
        Some(task) => tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default(),
        None => String::new(),
    };
    let _ = std::fs::remove_file(&prompt_file);

    let mut result = SingleResult {
        agent: agent.name.clone(),
        agent_source: agent.source.as_str(),
        task: task.clone(),
        messages,
        usage,
        model: model_id,
        stop_reason,
        error_message,
        stderr: None,
        step,
        pending: false,
        worktree: None,
        executor: "process",
    };
    if aborted {
        result.stop_reason = Some("aborted".to_owned());
        result.error_message = Some("Subagent was aborted".to_owned());
    } else if timed_out {
        let limit = budget.wall_clock.unwrap_or_default();
        result.stop_reason = Some("error".to_owned());
        result.error_message = Some(format!(
            "Subagent wall-clock budget exceeded ({} ms): the process worker was killed",
            limit.as_millis()
        ));
    } else if turn_budget_exceeded {
        result.stop_reason = Some("error".to_owned());
        result.error_message = Some(format!(
            "Subagent turn budget exceeded ({} turns): the process worker was killed",
            budget.max_turns
        ));
    } else if let Some(Err(error)) = status {
        result.stop_reason = Some("error".to_owned());
        result.error_message = Some(format!("Cannot wait for process worker: {error}"));
    } else if let Some(Ok(status)) = status {
        if !status.success() {
            result.stop_reason = Some("error".to_owned());
            result.error_message = Some(if stderr_text.trim().is_empty() {
                format!("Process worker exited with {status}")
            } else {
                stderr_text.trim().to_owned()
            });
        }
    }
    if result.stop_reason.is_none() {
        result.stop_reason = Some("stop".to_owned());
    }
    if let Some(lease) = worktree {
        let mut merge_result = super::worktree::WorktreeResult::default();
        if is_failed(&result) {
            // Retain the worktree on failure so the worker's partial changes
            // remain inspectable (matches the conflict-retention policy).
            merge_result.cleaned = false;
            let retained = format!(
                "Worktree retained after failed run at {} (branch {})",
                lease.path.display(),
                lease.branch
            );
            result.error_message = Some(match result.error_message.take() {
                Some(existing) => format!("{existing} — {retained}"),
                None => retained,
            });
        } else if worktree_merge == WorktreeMerge::Auto {
            merge_result = lease.merge().await;
            if let Some(error) = &merge_result.error {
                result.stop_reason = Some("error".to_owned());
                result.error_message = Some(error.clone());
                result.stderr = Some(error.clone());
            }
        }
        result.worktree = Some(lease.outcome(&merge_result));
    }
    if let Some(on_update) = &on_task_update {
        on_update(&result);
    }
    result
}

fn process_program() -> PathBuf {
    std::env::var_os("PIR_FOREMAN_PIR")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("pir"))
}

fn write_agent_prompt(agent: &str, body: &str) -> Result<PathBuf, String> {
    let root = crate::coding_agent::context::agent_dir()
        .join("foreman")
        .join("prompts");
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("Cannot create prompt directory: {error}"))?;
    let name = sanitize(agent);
    let path = root.join(format!(
        "{}-{}-{}.md",
        std::process::id(),
        PROMPT_SEQ.fetch_add(1, Ordering::Relaxed),
        name
    ));
    std::fs::write(&path, body)
        .map_err(|error| format!("Cannot write process worker prompt: {error}"))?;
    Ok(path)
}

fn sanitize(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars().take(32) {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        "agent".to_owned()
    } else {
        out
    }
}

#[doc(hidden)]
pub fn parse_event_for_test(line: &str) -> Option<Vec<Message>> {
    parse_event(line).map(|(messages, _)| messages)
}

/// The JSON host emits BOTH `message_end` (stream completion) and `turn_end`
/// (turn completion) for the same assistant response. Keep one copy so the
/// process executor neither duplicates messages nor double-counts usage.
fn accept_message(seen: &mut std::collections::HashSet<String>, message: &Message) -> bool {
    let Message::Assistant(assistant) = message else {
        return true;
    };
    let Ok(signature) = serde_json::to_string(assistant) else {
        return true;
    };
    seen.insert(signature)
}

#[doc(hidden)]
pub fn dedupe_events_for_test(lines: &[&str]) -> Vec<Message> {
    let mut seen = std::collections::HashSet::new();
    let mut messages = Vec::new();
    for line in lines {
        let Some((parsed, _)) = parse_event(line) else {
            continue;
        };
        for message in parsed {
            if accept_message(&mut seen, &message) {
                messages.push(message);
            }
        }
    }
    messages
}

fn parse_event(line: &str) -> Option<(Vec<Message>, Option<Vec<Message>>)> {
    let value: Value = serde_json::from_str(line).ok()?;
    let kind = value.get("type").and_then(Value::as_str)?;
    if !matches!(kind, "message_end" | "turn_end") {
        return None;
    }
    let message = value.get("message")?;
    if message.get("role").and_then(Value::as_str) == Some("assistant") {
        let parsed: Message = serde_json::from_value(message.clone()).ok()?;
        return Some((vec![parsed], None));
    }
    None
}

async fn wait_abort(signal: Option<AbortSignal>) {
    match signal {
        Some(signal) => signal.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

async fn wait_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => {
            let now = Instant::now();
            if deadline > now {
                tokio::time::sleep(deadline - now).await;
            }
        }
        None => std::future::pending::<()>().await,
    }
}

async fn kill_process_tree(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        #[cfg(windows)]
        {
            let system_root = std::env::var_os("SystemRoot")
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| std::ffi::OsString::from(r"C:\Windows"));
            let _ = tokio::task::spawn_blocking(move || {
                let _ = std::process::Command::new(
                    std::path::Path::new(&system_root)
                        .join("System32")
                        .join("taskkill.exe"),
                )
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            })
            .await;
        }
        #[cfg(unix)]
        {
            if let Ok(pid) = libc::pid_t::try_from(pid) {
                // SAFETY: kill only acts on the pid/pgid; the child was
                // spawned with process_group(0), so -pid is its group.
                unsafe {
                    let _ = libc::kill(-pid, libc::SIGKILL);
                }
            }
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = pid;
        }
    }
    let _ = child.start_kill();
}
