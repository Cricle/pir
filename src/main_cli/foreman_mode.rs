//! `pir foreman` — detached foreman runs and approval round-trips.
//!
//! A detached run is a normal `pir --mode json` process supervised by the
//! existing `jobs` store. The mode plugin restores `foreman` from the
//! `.mode.json` sidecar written here; no separate daemon protocol is needed.
//! Manual approvals are asynchronous: the first run may settle at the blocked
//! delegation, and `approve`/`deny` appends the decision and starts a new run.

use std::path::{Path, PathBuf};

use serde_json::json;

use pir::coding_agent::cli::ForemanCommand;
use pir::config::Config;
use pir::foreman::{ForemanApprovals, ForemanRouter};
use pir::session_manager::SessionManager;
use ForemanCommand::*;

const LAST_SESSION_FILE: &str = "last_session";
const LOG_TAIL_BYTES: u64 = 1024 * 1024;

pub(super) async fn run(command: ForemanCommand, config: &Config) -> pir::Result<()> {
    let session_dir = PathBuf::from(&config.session_dir);
    let agent_dir = pir::coding_agent::context::agent_dir();
    let jobs = pir::jobs::JobStore::new(&agent_dir);
    match command {
        Run {
            task,
            cwd,
            session,
            worktree,
            router,
            approvals,
            foreground,
            json,
        } => {
            run_start(
                &jobs,
                &session_dir,
                &agent_dir,
                task,
                cwd,
                session,
                worktree,
                parse_router(router.as_deref()),
                parse_approvals(approvals.as_deref()),
                foreground,
                json,
            )
            .await
        }
        Status { target, json } => run_status(&jobs, &session_dir, &agent_dir, target, json),
        Logs { job, follow } => run_logs(&jobs, &job, follow).await,
        Abort { job } => {
            let killed = jobs.kill(&job).map_err(anyhow::Error::msg)?;
            println!("{}", if killed { "aborted" } else { "not running" });
            Ok(())
        }
        Approve { id, all, session } => {
            run_decision(&jobs, &session_dir, &agent_dir, id, all, session, true).await
        }
        Deny { id, all, session } => {
            run_decision(&jobs, &session_dir, &agent_dir, id, all, session, false).await
        }
        Resume { session } => run_resume(&jobs, &session_dir, &session),
        Clean { session } => run_clean(&session_dir, &session),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_start(
    jobs: &pir::jobs::JobStore,
    session_dir: &Path,
    agent_dir: &Path,
    task: String,
    cwd: Option<String>,
    session: Option<String>,
    worktree: bool,
    router: ForemanRouter,
    approvals: ForemanApprovals,
    foreground: bool,
    json: bool,
) -> pir::Result<()> {
    let cwd = cwd.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| ".".to_owned())
    });
    if worktree && !git_clean(Path::new(&cwd))? {
        anyhow::bail!("--worktree requires a clean git working tree");
    }

    let mut manager = match session.as_deref() {
        Some(target) => open_session(session_dir, target)?,
        None => SessionManager::create(&cwd, session_dir)?,
    };
    manager.ensure_exclusive_writer()?;
    let session_path = manager
        .session_file()
        .ok_or_else(|| anyhow::anyhow!("session has no file path"))?
        .to_path_buf();
    let session_cwd = manager.cwd().to_owned();
    manager.append_custom_entry(
        "foreman_run",
        Some(json!({
            "task": task,
            "worktree": worktree,
            "router": router_name(router),
            "approvals": approvals_name(approvals),
        })),
    )?;
    drop(manager);

    pir::foreman::write_running_sidecar(&session_path, worktree, router, approvals)?;
    write_last_session(agent_dir, &session_path)?;
    let prompt_file = write_prompt(agent_dir, &session_path, &task)?;
    let args = child_args(&session_path, &prompt_file);
    let cwd_path = PathBuf::from(&session_cwd);
    if foreground {
        let status = std::process::Command::new(current_exe()?)
            .args(&args)
            .current_dir(&cwd_path)
            .status()?;
        if !status.success() {
            anyhow::bail!("foreman run exited with {status}");
        }
        return Ok(());
    }
    let meta = jobs
        .spawn_argv(&current_exe()?, &args, &cwd_path, &[])
        .map_err(anyhow::Error::msg)?;
    let result = json!({
        "job": meta.id,
        "session": session_path.display().to_string(),
        "cwd": session_cwd,
        "log": jobs.root().join(&meta.id).join("output.log").display().to_string(),
    });
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "started foreman run {} for {}",
            result["job"].as_str().unwrap_or("?"),
            result["session"].as_str().unwrap_or("?")
        );
        println!("log: {}", result["log"].as_str().unwrap_or("?"));
    }
    Ok(())
}

fn run_status(
    jobs: &pir::jobs::JobStore,
    session_dir: &Path,
    agent_dir: &Path,
    target: Option<String>,
    json: bool,
) -> pir::Result<()> {
    let session_path = match target.as_deref() {
        Some(target) if is_job_id(jobs, target) => {
            jobs.status(target).and_then(|meta| session_from_job(&meta))
        }
        Some(target) => Some(resolve_session_path(session_dir, target)?),
        None => last_session(agent_dir),
    };
    let Some(session_path) = session_path else {
        anyhow::bail!("no foreman session found; pass a session path or start a run");
    };
    let status = pir::foreman::read_status(&session_path);
    let job = find_job_for_session(jobs, &session_path);
    let value = json!({
        "session": session_path.display().to_string(),
        "status": status,
        "job": job.as_ref().map(|meta| json!({
            "id": meta.id,
            "state": meta.status_line(),
            "exit": meta.exit_code,
            "log": jobs.root().join(&meta.id).join("output.log").display().to_string(),
        })),
    });
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    println!("session: {}", session_path.display());
    match status {
        Some(status) => {
            println!(
                "mode: {}  phase: {}  generation: {}  nudges: {}",
                status.mode, status.phase, status.generation, status.nudges
            );
            println!("pending approvals: {}", status.pending.len());
            for approval in &status.pending {
                println!(
                    "  {}  [{}]  {}",
                    approval.id, approval.kind, approval.summary
                );
            }
        }
        None => println!("mode: no foreman journal yet"),
    }
    if let Some(meta) = job {
        println!("job: {} ({})", meta.id, meta.status_line());
    }
    Ok(())
}

async fn run_logs(jobs: &pir::jobs::JobStore, job: &str, follow: bool) -> pir::Result<()> {
    let mut seen = 0usize;
    loop {
        if let Some(log) = jobs.read_log(job, LOG_TAIL_BYTES, 4096) {
            // The tail window can rotate between polls: clamp/reset `seen`
            // and advance to a char boundary so a multibyte line never makes
            // the slice panic.
            if log.text.len() < seen {
                seen = 0;
            }
            let mut start = seen.min(log.text.len());
            while start < log.text.len() && !log.text.is_char_boundary(start) {
                start += 1;
            }
            if start < log.text.len() {
                print!("{}", &log.text[start..]);
            }
            seen = log.text.len();
        } else {
            anyhow::bail!("job not found: {job}");
        }
        if !follow {
            return Ok(());
        }
        let terminal = jobs
            .status(job)
            .map(|meta| meta.status.is_terminal())
            .unwrap_or(true);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if terminal {
            return Ok(());
        }
    }
}

async fn run_decision(
    jobs: &pir::jobs::JobStore,
    session_dir: &Path,
    agent_dir: &Path,
    id: Option<String>,
    all: bool,
    session: Option<String>,
    approved: bool,
) -> pir::Result<()> {
    if !all && id.is_none() {
        anyhow::bail!("pass an approval id or --all");
    }
    let session_path = match session {
        Some(target) => resolve_session_path(session_dir, &target)?,
        None => last_session(agent_dir)
            .ok_or_else(|| anyhow::anyhow!("no recent foreman session; pass --session"))?,
    };
    if let Some(meta) = find_job_for_session(jobs, &session_path) {
        if !meta.status.is_terminal() {
            anyhow::bail!(
                "job {} is still running; use the TUI /approvals command for a live session",
                meta.id
            );
        }
    }
    let decisions = pir::foreman::decide_approval(
        &session_path,
        if all { None } else { id.as_deref() },
        approved,
    )
    .map_err(anyhow::Error::msg)?;
    let mut manager = SessionManager::open(&session_path)?;
    manager.ensure_exclusive_writer()?;
    for record in &decisions {
        let text = if record.kind == "plan" && approved {
            format!(
                "PLAN APPROVED: `{}` ({}).\n\n{}\n\nImplement this plan now and verify the result.",
                record.id,
                record.summary,
                record.plan.as_deref().unwrap_or("(see session transcript)")
            )
        } else if record.kind == "plan" {
            format!(
                "PLAN DENIED: `{}` ({}). Revise the plan and submit it again, or ask a clarifying question.",
                record.id, record.summary
            )
        } else if approved {
            format!(
                "FOREMAN APPROVAL APPROVED: `{}` ({}). Retry that worker delegation now, then \
                 run the independent reviewer before finishing.",
                record.id, record.summary
            )
        } else {
            format!(
                "FOREMAN APPROVAL DENIED: `{}` ({}). Do not retry; choose another approach.",
                record.id, record.summary
            )
        };
        manager.append_custom_message_entry(
            "foreman_approval",
            json!(text),
            true,
            Some(json!({"id": record.id, "state": record.state, "kind": record.kind})),
        )?;
    }
    let cwd = manager.cwd().to_owned();
    drop(manager);
    let has_plan = decisions.iter().any(|record| record.kind == "plan");
    let prompt = if approved {
        if has_plan {
            "The plan was approved. Implement the approved plan from the transcript and verify \
             the result."
        } else {
            "Continue the foreman task. The approval decision is in the transcript."
        }
    } else if has_plan {
        "The plan was denied. Revise the plan, ask a clarifying question, or report the blocker."
    } else {
        "Continue without the denied delegation. Choose another approach or report the blocker."
    };
    let prompt_file = write_prompt(agent_dir, &session_path, prompt)?;
    let args = child_args(&session_path, &prompt_file);
    let meta = jobs
        .spawn_argv(&current_exe()?, &args, Path::new(&cwd), &[])
        .map_err(anyhow::Error::msg)?;
    println!(
        "{} {} approval(s); resumed as job {}",
        if approved { "approved" } else { "denied" },
        decisions.len(),
        meta.id
    );
    Ok(())
}

fn run_resume(jobs: &pir::jobs::JobStore, session_dir: &Path, session: &str) -> pir::Result<()> {
    let path = resolve_session_path(session_dir, session)?;
    if let Some(meta) = find_job_for_session(jobs, &path) {
        if !meta.status.is_terminal() {
            anyhow::bail!(
                "job {} is still running; abort it or wait for it to finish",
                meta.id
            );
        }
    }
    let status = std::process::Command::new(current_exe()?)
        .arg("--session")
        .arg(&path)
        .status()?;
    if !status.success() {
        anyhow::bail!("interactive resume exited with {status}");
    }
    Ok(())
}

fn run_clean(session_dir: &Path, session: &str) -> pir::Result<()> {
    let path = resolve_session_path(session_dir, session)?;
    let mut removed = 0usize;
    let text = std::fs::read_to_string(&path)?;
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(results) = entry
            .pointer("/message/details/results")
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        for result in results {
            let Some(worktree) = result.get("worktree") else {
                continue;
            };
            // `cleaned` means the worktree is already gone. A merged worktree
            // whose cleanup failed is still retained and must be removable.
            if worktree.get("cleaned").and_then(serde_json::Value::as_bool) == Some(true) {
                continue;
            }
            let (Some(repo), Some(path), Some(branch)) = (
                worktree.get("repo").and_then(serde_json::Value::as_str),
                worktree.get("path").and_then(serde_json::Value::as_str),
                worktree.get("branch").and_then(serde_json::Value::as_str),
            ) else {
                continue;
            };
            let _ = std::process::Command::new("git")
                .args(["-C", repo, "worktree", "remove", "--force", path])
                .status();
            let _ = std::process::Command::new("git")
                .args(["-C", repo, "branch", "-D", branch])
                .status();
            removed += 1;
        }
    }
    println!("removed {removed} retained worktree(s)");
    Ok(())
}

fn parse_router(value: Option<&str>) -> ForemanRouter {
    match value {
        Some("process") => ForemanRouter::Process,
        _ => ForemanRouter::Inline,
    }
}

fn parse_approvals(value: Option<&str>) -> ForemanApprovals {
    match value {
        Some("manual") => ForemanApprovals::Manual,
        _ => ForemanApprovals::Auto,
    }
}

fn router_name(router: ForemanRouter) -> &'static str {
    match router {
        ForemanRouter::Inline => "inline",
        ForemanRouter::Process => "process",
    }
}

fn approvals_name(approvals: ForemanApprovals) -> &'static str {
    match approvals {
        ForemanApprovals::Auto => "auto",
        ForemanApprovals::Manual => "manual",
    }
}

fn child_args(session_path: &Path, prompt_file: &Path) -> Vec<String> {
    vec![
        "--session".to_owned(),
        session_path.display().to_string(),
        "--mode".to_owned(),
        "json".to_owned(),
        format!("@{}", prompt_file.display()),
    ]
}

fn current_exe() -> pir::Result<PathBuf> {
    std::env::current_exe()
        .map_err(|error| anyhow::anyhow!("cannot resolve pir executable: {error}"))
}

fn write_prompt(agent_dir: &Path, session_path: &Path, task: &str) -> pir::Result<PathBuf> {
    let root = agent_dir.join("foreman").join("prompts");
    std::fs::create_dir_all(&root)?;
    let name = session_path
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("session");
    let path = root.join(format!("{name}-{}.md", std::process::id()));
    std::fs::write(&path, task)?;
    Ok(path)
}

fn write_last_session(agent_dir: &Path, session_path: &Path) -> pir::Result<()> {
    let root = agent_dir.join("foreman");
    std::fs::create_dir_all(&root)?;
    std::fs::write(
        root.join(LAST_SESSION_FILE),
        session_path.display().to_string(),
    )?;
    Ok(())
}

fn last_session(agent_dir: &Path) -> Option<PathBuf> {
    let path = agent_dir.join("foreman").join(LAST_SESSION_FILE);
    let text = std::fs::read_to_string(path).ok()?;
    let path = PathBuf::from(text.trim());
    path.exists().then_some(path)
}

fn resolve_session_path(session_dir: &Path, target: &str) -> pir::Result<PathBuf> {
    let direct = PathBuf::from(target);
    if direct.is_file() {
        return Ok(direct);
    }
    SessionManager::find_session_by_id(session_dir, target)
        .ok_or_else(|| anyhow::anyhow!("session not found: {target}"))
}

fn open_session(session_dir: &Path, target: &str) -> pir::Result<SessionManager> {
    SessionManager::open(resolve_session_path(session_dir, target)?)
}

fn is_job_id(jobs: &pir::jobs::JobStore, target: &str) -> bool {
    jobs.status(target).is_some()
}

fn find_job_for_session(
    jobs: &pir::jobs::JobStore,
    session_path: &Path,
) -> Option<pir::jobs::JobMeta> {
    jobs.list()
        .into_iter()
        .find(|meta| session_from_job(meta).is_some_and(|path| path == session_path))
}

fn session_from_job(meta: &pir::jobs::JobMeta) -> Option<PathBuf> {
    meta.spec
        .args
        .windows(2)
        .find(|pair| pair[0] == "--session")
        .map(|pair| PathBuf::from(&pair[1]))
}

fn git_clean(cwd: &Path) -> pir::Result<bool> {
    let root = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| anyhow::anyhow!("cannot run git: {error}"))?;
    if !root.status.success() {
        anyhow::bail!("--worktree requires a git repository");
    }
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["status", "--porcelain"])
        .output()?;
    Ok(status.stdout.is_empty())
}
