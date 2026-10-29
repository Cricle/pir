//! Persistent shell pool (settings `shellPool`): member reuse, fresh-shell
//! cwd semantics, variable-pollution isolation, exit codes, timeout kill +
//! respawn, fallback paths, and concurrent queueing — with the member-spawn
//! seam injected so spawn counts are observable.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pir::coding_agent::tools::resolve_shell_config;
use pir::coding_agent::tools::shell_pool::{
    member_args, MemberSpawn, ShellPool, ShellProtocol, SpawnedMember,
};

// ---------------------------------------------------------------------------
// Injectable spawners
// ---------------------------------------------------------------------------

/// Spawns real persistent shells and counts every spawn.
#[derive(Default)]
struct CountingSpawner {
    count: AtomicUsize,
}

impl MemberSpawn for CountingSpawner {
    fn spawn(
        &self,
        protocol: ShellProtocol,
        program: &str,
        cwd: &Path,
    ) -> Result<SpawnedMember, String> {
        self.count.fetch_add(1, Ordering::SeqCst);
        spawn_real(protocol, program, cwd)
    }
}

/// Always fails (e.g. no shell executable): the pool must fall back.
struct FailingSpawner;

impl MemberSpawn for FailingSpawner {
    fn spawn(
        &self,
        _protocol: ShellProtocol,
        _program: &str,
        _cwd: &Path,
    ) -> Result<SpawnedMember, String> {
        Err("no shell available".to_owned())
    }
}

/// Fails the first spawn, then succeeds (prewarm/lazy handoff races).
struct RecoveringSpawner {
    count: AtomicUsize,
}

impl MemberSpawn for RecoveringSpawner {
    fn spawn(
        &self,
        protocol: ShellProtocol,
        program: &str,
        cwd: &Path,
    ) -> Result<SpawnedMember, String> {
        if self.count.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err("transient spawn failure".to_owned());
        }
        spawn_real(protocol, program, cwd)
    }
}

/// Spawn a real persistent shell member (the production shape).
fn spawn_real(protocol: ShellProtocol, program: &str, cwd: &Path) -> Result<SpawnedMember, String> {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let mut spawn = tokio::process::Command::new(program);
    spawn
        .args(member_args(protocol))
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = spawn.spawn().map_err(|error| error.to_string())?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "stdin pipe unavailable".to_owned())?;
    Ok(SpawnedMember::from_child(child, stdin, token))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn temp_pool_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-shell-pool-{}-{}",
        tag,
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// The bash executable the bash tool would resolve (git-bash / /bin/bash).
fn bash_program() -> String {
    resolve_shell_config(None)
        .expect("bash shell must be resolvable for pool tests")
        .program()
        .to_string()
}

/// Trim the capture tail so assertions compare logical output.
fn trimmed(output: &str) -> String {
    output.trim().to_owned()
}

// ---------------------------------------------------------------------------
// Bash pool
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bash_pool_reuses_one_member_across_calls() {
    let dir = temp_pool_dir("bash-reuse");
    // The pool owns the spawner; keep a shared handle to read the count.
    let spawner = Arc::new(CountingSpawner::default());
    let pool = std::sync::Arc::new(ShellPool::new(Arc::clone(&spawner) as Arc<dyn MemberSpawn>));
    let program = bash_program();
    for index in 0..5 {
        let outcome = pool
            .run_test(
                ShellProtocol::Bash,
                &program,
                &dir,
                &format!("echo n{index}"),
                None,
            )
            .await;
        match outcome {
            pir::coding_agent::tools::shell_pool::PooledTestOutcome::Completed {
                exit_code,
                output,
            } => {
                assert_eq!(exit_code, 0);
                assert_eq!(trimmed(&output), format!("n{index}"));
                // The sentinel must never leak into the capture.
                assert!(!output.contains('\x1e'));
            }
            other => panic!("command {index} did not complete: {other:?}"),
        }
    }
    assert_eq!(
        spawner.count.load(Ordering::SeqCst),
        1,
        "N calls must spawn exactly one persistent member"
    );
    let _ = dir;
}

#[tokio::test]
async fn bash_pool_cwd_matches_fresh_shell_semantics() {
    // Upstream spawns a fresh shell per call at the session cwd: a `cd`
    // inside one command must NOT persist into the next call.
    let dir = temp_pool_dir("bash-cwd");
    let sub = dir.join("sub");
    std::fs::create_dir_all(&sub).expect("create sub dir");
    let spawner = Arc::new(CountingSpawner::default());
    let pool = std::sync::Arc::new(ShellPool::new(Arc::clone(&spawner) as Arc<dyn MemberSpawn>));
    let program = bash_program();

    let first = pool
        .run_test(ShellProtocol::Bash, &program, &dir, "cd sub", None)
        .await;
    assert!(matches!(
        first,
        pir::coding_agent::tools::shell_pool::PooledTestOutcome::Completed { exit_code: 0, .. }
    ));

    let second = pool
        .run_test(
            ShellProtocol::Bash,
            &program,
            &dir,
            "basename \"$PWD\"",
            None,
        )
        .await;
    match second {
        pir::coding_agent::tools::shell_pool::PooledTestOutcome::Completed {
            exit_code,
            output,
        } => {
            assert_eq!(exit_code, 0);
            assert_eq!(
                trimmed(&output),
                dir.file_name().unwrap().to_string_lossy(),
                "cd side effects must not persist across calls (upstream fresh-shell behavior)"
            );
        }
        other => panic!("pwd command did not complete: {other:?}"),
    }
}

#[tokio::test]
async fn bash_pool_isolates_variable_pollution() {
    // The subshell boundary must block exports/functions/options from
    // leaking into the next call (fresh-shell behavior upstream).
    let dir = temp_pool_dir("bash-pollution");
    let spawner = Arc::new(CountingSpawner::default());
    let pool = std::sync::Arc::new(ShellPool::new(Arc::clone(&spawner) as Arc<dyn MemberSpawn>));
    let program = bash_program();

    let first = pool
        .run_test(
            ShellProtocol::Bash,
            &program,
            &dir,
            "export PIR_POOL_LEAK=1 && echo set",
            None,
        )
        .await;
    match first {
        pir::coding_agent::tools::shell_pool::PooledTestOutcome::Completed {
            exit_code,
            output,
        } => {
            assert_eq!(exit_code, 0);
            assert_eq!(trimmed(&output), "set");
        }
        other => panic!("export command did not complete: {other:?}"),
    }

    let second = pool
        .run_test(
            ShellProtocol::Bash,
            &program,
            &dir,
            "echo ${PIR_POOL_LEAK:-clean}",
            None,
        )
        .await;
    match second {
        pir::coding_agent::tools::shell_pool::PooledTestOutcome::Completed {
            exit_code,
            output,
        } => {
            assert_eq!(exit_code, 0);
            assert_eq!(
                trimmed(&output),
                "clean",
                "export must not leak across calls"
            );
        }
        other => panic!("leak probe did not complete: {other:?}"),
    }

    // A failed command must not poison the member either.
    let failed = pool
        .run_test(ShellProtocol::Bash, &program, &dir, "false", None)
        .await;
    assert!(matches!(
        failed,
        pir::coding_agent::tools::shell_pool::PooledTestOutcome::Completed { exit_code: 1, .. }
    ));
    let next = pool
        .run_test(ShellProtocol::Bash, &program, &dir, "echo ok", None)
        .await;
    match next {
        pir::coding_agent::tools::shell_pool::PooledTestOutcome::Completed {
            exit_code,
            output,
        } => {
            assert_eq!(exit_code, 0);
            assert_eq!(trimmed(&output), "ok");
        }
        other => panic!("post-failure command did not complete: {other:?}"),
    }
}

#[tokio::test]
async fn bash_pool_exit_codes_streams_and_empty_command() {
    let dir = temp_pool_dir("bash-exit");
    let spawner = Arc::new(CountingSpawner::default());
    let pool = std::sync::Arc::new(ShellPool::new(Arc::clone(&spawner) as Arc<dyn MemberSpawn>));
    let program = bash_program();

    // Non-zero exit code propagates; stdout+stderr both captured.
    let outcome = pool
        .run_test(
            ShellProtocol::Bash,
            &program,
            &dir,
            "echo hello; echo oops >&2; exit 7",
            None,
        )
        .await;
    match outcome {
        pir::coding_agent::tools::shell_pool::PooledTestOutcome::Completed {
            exit_code,
            output,
        } => {
            assert_eq!(exit_code, 7);
            assert!(output.contains("hello"), "stdout captured: {output:?}");
            assert!(output.contains("oops"), "stderr captured: {output:?}");
            assert!(!output.contains('\x1e'), "sentinel must not leak");
        }
        other => panic!("exit-7 command did not complete: {other:?}"),
    }

    // Empty command: upstream `bash -c ""` exits 0 with no output.
    let empty = pool
        .run_test(ShellProtocol::Bash, &program, &dir, "", None)
        .await;
    assert!(matches!(
        empty,
        pir::coding_agent::tools::shell_pool::PooledTestOutcome::Completed { exit_code: 0, .. }
    ));
}

#[tokio::test]
async fn bash_pool_timeout_kills_member_and_respawns() {
    let dir = temp_pool_dir("bash-timeout");
    let spawner = Arc::new(CountingSpawner::default());
    let pool = std::sync::Arc::new(ShellPool::new(Arc::clone(&spawner) as Arc<dyn MemberSpawn>));
    let program = bash_program();

    let hung = pool
        .run_test(ShellProtocol::Bash, &program, &dir, "sleep 30", Some(0.4))
        .await;
    assert!(
        matches!(
            hung,
            pir::coding_agent::tools::shell_pool::PooledTestOutcome::TimedOut
        ),
        "hung command must report the timeout outcome"
    );

    // The next call respawns a fresh member and works normally.
    let next = pool
        .run_test(
            ShellProtocol::Bash,
            &program,
            &dir,
            "echo after-timeout",
            None,
        )
        .await;
    match next {
        pir::coding_agent::tools::shell_pool::PooledTestOutcome::Completed {
            exit_code,
            output,
        } => {
            assert_eq!(exit_code, 0);
            assert_eq!(trimmed(&output), "after-timeout");
        }
        other => panic!("post-timeout command did not complete: {other:?}"),
    }
    assert_eq!(
        spawner.count.load(Ordering::SeqCst),
        2,
        "timeout kills the member; the next call respawns"
    );
}

#[tokio::test]
async fn bash_pool_falls_back_when_spawn_fails() {
    let dir = temp_pool_dir("bash-fallback");
    let pool = std::sync::Arc::new(ShellPool::new(Arc::new(FailingSpawner)));
    let program = bash_program();
    let outcome = pool
        .run_test(ShellProtocol::Bash, &program, &dir, "echo anything", None)
        .await;
    assert!(
        matches!(
            outcome,
            pir::coding_agent::tools::shell_pool::PooledTestOutcome::Lost
        ),
        "spawn failure must surface as the fallback outcome"
    );
}

#[tokio::test]
async fn bash_pool_recovers_after_transient_spawn_failure() {
    let dir = temp_pool_dir("bash-recover");
    let spawner = Arc::new(RecoveringSpawner {
        count: AtomicUsize::new(0),
    });
    let pool = std::sync::Arc::new(ShellPool::new(Arc::clone(&spawner) as Arc<dyn MemberSpawn>));
    let program = bash_program();

    let first = pool
        .run_test(ShellProtocol::Bash, &program, &dir, "echo one", None)
        .await;
    assert!(matches!(
        first,
        pir::coding_agent::tools::shell_pool::PooledTestOutcome::Lost
    ));
    let second = pool
        .run_test(ShellProtocol::Bash, &program, &dir, "echo two", None)
        .await;
    match second {
        pir::coding_agent::tools::shell_pool::PooledTestOutcome::Completed {
            exit_code,
            output,
        } => {
            assert_eq!(exit_code, 0);
            assert_eq!(trimmed(&output), "two");
        }
        other => panic!("recovery command did not complete: {other:?}"),
    }
    assert_eq!(spawner.count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn bash_pool_concurrent_calls_serialize_on_one_member() {
    let dir = temp_pool_dir("bash-concurrent");
    let spawner = Arc::new(CountingSpawner::default());
    let pool = std::sync::Arc::new(ShellPool::new(Arc::clone(&spawner) as Arc<dyn MemberSpawn>));
    let program = bash_program();

    let mut handles = Vec::new();
    for index in 0..4 {
        let pool = Arc::clone(&pool);
        let program = program.clone();
        let dir = dir.clone();
        handles.push(tokio::spawn(async move {
            pool.run_test(
                ShellProtocol::Bash,
                &program,
                &dir,
                &format!("echo c{index}"),
                None,
            )
            .await
        }));
    }
    for (index, handle) in handles.into_iter().enumerate() {
        match handle.await.expect("task join") {
            pir::coding_agent::tools::shell_pool::PooledTestOutcome::Completed {
                exit_code,
                output,
            } => {
                assert_eq!(exit_code, 0);
                assert_eq!(trimmed(&output), format!("c{index}"));
            }
            other => panic!("concurrent command {index} did not complete: {other:?}"),
        }
    }
    assert_eq!(
        spawner.count.load(Ordering::SeqCst),
        1,
        "concurrent calls queue on the single member (bash is Sequential upstream)"
    );
}

/// Dispatch liveness: a call queued behind a busy member must not wait
/// forever for the member mutex. It gives up after the pool's dispatch window
/// and reports "not usable", so the tool starts the command on a fresh
/// per-call shell instead of stalling invisibly (the measured
/// accepted-but-never-started gap).
#[tokio::test]
async fn dispatch_behind_a_busy_member_times_out_and_falls_back() {
    let dir = temp_pool_dir("dispatch-liveness");
    let pool = std::sync::Arc::new(ShellPool::new(Arc::new(CountingSpawner::default())));
    let program = bash_program();
    let marker = dir.join("holder-running");
    let holder_command = format!(
        "touch '{}'; sleep 30",
        marker.to_string_lossy().replace('\\', "/")
    );
    let holder = {
        let pool = Arc::clone(&pool);
        let dir = dir.clone();
        let program = program.clone();
        tokio::spawn(async move {
            pool.run_test(ShellProtocol::Bash, &program, &dir, &holder_command, None)
                .await
        })
    };
    let mut running = false;
    for _ in 0..600 {
        if marker.exists() {
            running = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(running, "the holder command never started");

    let started = Instant::now();
    let outcome = pool
        .run_test(
            ShellProtocol::Bash,
            &program,
            &dir,
            "echo second",
            Some(60.0),
        )
        .await;
    let elapsed = started.elapsed();

    assert!(
        matches!(
            outcome,
            pir::coding_agent::tools::shell_pool::PooledTestOutcome::Lost
        ),
        "the queued call reports the pool unusable instead of blocking: {outcome:?}"
    );
    assert!(
        elapsed >= Duration::from_secs(4) && elapsed < Duration::from_secs(15),
        "the dispatch-liveness window bounded the wait (elapsed {elapsed:?})"
    );

    holder.abort();
}
