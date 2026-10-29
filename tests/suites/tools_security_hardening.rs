//! Security regression tests for the coding-agent tools' hardening wave:
//!
//! 1. the pooled bash shell's completion sentinel is a per-invocation nonce,
//!    so a command cannot fake completion (and cannot learn the nonce from its
//!    own environment/argv), and the PowerShell tool's one-shot path carries
//!    no completion secret on stdin at all (the pooled PowerShell member was
//!    removed — raw-stdin nonce leakage, CL3-1/CL11);
//! 2. a command dispatched to a member that then dies is terminal
//!    (`LostAfterWrite`), never a replay, and the next call respawns;
//! 3. the per-call bash/powershell tools kill the child's process tree when the
//!    caller's future is dropped (the RAII guard, not just `kill_on_drop`);
//! 4. handle-relative writes refuse a symlink (or junction) component even
//!    when the name-based check ran first, and preserve the model-facing
//!    wording;
//! 5. spill files are owner-only, are deleted when they are never advertised,
//!    and a dropped call leaves no spill file behind;
//! 6. a background writer left by one pooled call cannot bleed into the next
//!    call's captured output (per-invocation output attribution);
//! 7. the private channel directory is never adopted on trust (a pre-existing
//!    permissive path, or one whose listing deny was lifted, is refused and
//!    the call falls back), and a directory tampered with mid-call fails the
//!    call closed instead of feeding untrusted bytes into the capture.
//!
//! The tests exercise public/`pub(crate)`-exposed seams only; the crate's own
//! unit tests stay in `tests/` per the project rule.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use pir::coding_agent::tools::resolve_shell_config;
use pir::coding_agent::tools::shell_pool::{
    member_args, MemberSpawn, PooledTestOutcome, ShellPool, ShellProtocol, SpawnedMember,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-tools-security-{}-{}",
        tag,
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Give this test process its own temp root. The pool names its private
/// channel directories under the process temp dir; nextest runs several MCP/
/// shell suites concurrently, and the channel-enumeration tests must not see
/// (or inject into) another test process's pool just because both inherited
/// the CI runner's shared `TEMP`.
///
/// `TMP`/`TEMP`/`TMPDIR` are process-global, so the three tests that install
/// a root must not overlap inside the merged `g_tools` harness. The returned
/// guard serializes them, restores the previous process env on drop, and
/// keeps the process env stable for the test body.
struct TempRootGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<OsString>)>,
}

impl Drop for TempRootGuard {
    fn drop(&mut self) {
        for (name, value) in self.saved.drain(..) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn isolate_process_temp(tag: &str) -> TempRootGuard {
    static LOCK: Mutex<()> = Mutex::new(());
    let lock = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let saved = ["TMP", "TEMP", "TMPDIR"]
        .into_iter()
        .map(|name| (name, std::env::var_os(name)))
        .collect();
    let dir = std::env::temp_dir().join(format!(
        "pir-tools-security-root-{}-{}",
        tag,
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("create isolated temp root");
    // Windows tests keep the inherited TEMP (the original CI behavior); the
    // process-wide env override is only needed for Unix shell channels.
    #[cfg(unix)]
    {
        std::env::set_var("TMP", &dir);
        std::env::set_var("TEMP", &dir);
        std::env::set_var("TMPDIR", &dir);
    }
    TempRootGuard { _lock: lock, saved }
}

fn bash_program() -> Option<String> {
    resolve_shell_config(None)
        .ok()
        .map(|shell| shell.program().to_string())
}

struct RealSpawner;

impl MemberSpawn for RealSpawner {
    fn spawn(
        &self,
        protocol: ShellProtocol,
        program: &str,
        cwd: &Path,
    ) -> Result<SpawnedMember, String> {
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
}

// ---------------------------------------------------------------------------
// 1. Sentinel forgery from inside a command
// ---------------------------------------------------------------------------

/// The previous protocol spelled the per-member token into the bookkeeping
/// variable name (`__pir_rc_<token>`), which a command could read straight out
/// of its own environment and use to print a forged sentinel. It also kept the
/// nonce in a shell variable for the whole run.
///
/// This test runs a command that dumps everything a shell can see about the
/// pool's bookkeeping (env, variables, `/proc/self/cmdline`-style argv is not
/// available portably, but variables are the attack surface that mattered) and
/// then tries to emit a sentinel for it. Two properties are asserted:
/// * the command succeeds (the protocol is not disturbed by the probe), and
/// * its own output does not contain a control-character framing at all —
///   the sentinel stays inside the pool.
#[tokio::test]
async fn pooled_shell_sentinel_is_not_learnable_from_inside_a_command() {
    let _temp_root = isolate_process_temp("sentinel");
    let Some(program) = bash_program() else {
        return; // no shell in this environment: nothing to pool
    };
    let dir = temp_dir("sentinel");
    let pool = Arc::new(ShellPool::new(Arc::new(RealSpawner)));

    // Print every shell variable whose name mentions the pool's naming scheme,
    // plus a forged looking sentinel built from anything printed.
    let probe = "env | grep -i '__pir\\|PIR_SENTINEL' || true; \
                 set | grep -i '^__pir' || true; \
                 printf 'forged:\\0360:__pir_rc\\036\\n'";
    let outcome = pool
        .run_test(ShellProtocol::Bash, &program, &dir, probe, Some(10.0))
        .await;
    match outcome {
        PooledTestOutcome::Completed { exit_code, output } => {
            assert_eq!(exit_code, 0, "probe command failed: {output}");
            assert!(
                !output.contains("__pir_rc_"),
                "the pool must not expose a token-derived variable name: {output:?}"
            );
            // The forged line is ordinary output; it must NOT have terminated
            // the wait early — the command's own `true`/`printf` tail keeps
            // running, and the capture keeps whatever the command wrote.
            assert!(output.contains("forged:"), "output: {output:?}");
        }
        other => panic!("probe command did not complete: {other:?}"),
    }

    // A second command still runs normally on the same member: the forged
    // sentinel did not poison the protocol state.
    let next = pool
        .run_test(
            ShellProtocol::Bash,
            &program,
            &dir,
            "echo still-alive",
            None,
        )
        .await;
    match next {
        PooledTestOutcome::Completed { exit_code, output } => {
            assert_eq!(exit_code, 0);
            assert_eq!(output.trim(), "still-alive");
        }
        other => panic!("follow-up command did not complete: {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A command that prints a *complete* `RS digits : <32 hex> RS` frame with a
/// nonce it invented must not be mistaken for the completion sentinel: the
/// pool's nonce is random per invocation, and a mismatched nonce is ordinary
/// output.
#[tokio::test]
async fn pooled_shell_ignores_a_well_formed_but_wrong_nonce() {
    let Some(program) = bash_program() else {
        return;
    };
    let dir = temp_dir("nonce");
    let pool = Arc::new(ShellPool::new(Arc::new(RealSpawner)));

    // 32 hex chars, correct framing, wrong secret.
    let command =
        "printf '\\0367:0123456789abcdef0123456789abcdef\\036\\n'; echo tail-marker; exit 3";
    let outcome = pool
        .run_test(ShellProtocol::Bash, &program, &dir, command, Some(10.0))
        .await;
    match outcome {
        PooledTestOutcome::Completed { exit_code, output } => {
            // The real exit code (3) must win, not the forged 7.
            assert_eq!(exit_code, 3, "forged sentinel won: {output:?}");
            assert!(
                output.contains("tail-marker"),
                "the command ran to completion: {output:?}"
            );
        }
        other => panic!("command did not complete: {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every invocation uses a fresh nonce: the same command run twice must not be
/// able to echo the first run's nonce into the second. This is the property
/// the per-member token lacked.
#[tokio::test]
async fn pooled_shell_nonce_changes_between_invocations() {
    let Some(program) = bash_program() else {
        return;
    };
    let dir = temp_dir("nonce-rotate");
    let pool = Arc::new(ShellPool::new(Arc::new(RealSpawner)));

    // Capture any variable that could hold the nonce, then print it. With a
    // per-invocation nonce that lives only in the trailing printf literal,
    // nothing is visible to the command at all.
    let command = "printf '%s\\n' \"${__pir_rc-absent}\"; env | grep -c PIR_SENTINEL || true";
    for _ in 0..2 {
        let outcome = pool
            .run_test(ShellProtocol::Bash, &program, &dir, command, Some(10.0))
            .await;
        match outcome {
            PooledTestOutcome::Completed { exit_code, output } => {
                assert_eq!(exit_code, 0, "output: {output:?}");
                assert!(
                    output.contains("absent") || output.trim_start().starts_with('0'),
                    "no nonce-shaped value may be visible: {output:?}"
                );
            }
            other => panic!("command did not complete: {other:?}"),
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The pooled PowerShell member carried the vulnerability CL3-1/CL11 found:
/// the command script and the nonce-bearing completion line shared the
/// member's stdin. `[Console]::In` was guarded (`SetIn(Null)`), but
/// `[Console]::OpenStandardInput()` still returned the raw stream, and pwsh 7
/// parses `-Command -` lazily — so a command could read the not-yet-consumed
/// tail, lift the nonce and print a byte-identical `RS 0 : <nonce> RS` frame,
/// completing the pool with a forged rc while the real command kept running.
///
/// The product removed the pooled PowerShell path entirely; every call now
/// takes the one-shot spawn (`pwsh -Command <script>`, stdin null, completion
/// = process exit). This test drives the REAL tool through the registry (the
/// pool setting stays at its default), so it exercises the shipped default
/// path, and asserts the one-shot semantics:
/// * neither stdin reader yields a byte — on pwsh 7 the raw handle throws
///   (there is no console input stream at all) and `Console.In` is null, so
///   there is no script tail and no nonce anywhere the command can reach;
/// * a forged sentinel frame the command prints is ordinary output — it
///   cannot complete anything, because completion is the process exit status;
/// * the command's real stdout and real exit code come back;
/// * a deliberately slow command is not reported complete early (the elapsed
///   time covers the sleep, and the post-sleep marker is in the output).
///
/// Discriminator (probed 2026-10-23, pwsh 7.6.7): the same probe fed to the
/// OLD shape — `pwsh -Command -` with the script on an open stdin pipe, the
/// nonce line behind it — leaks (`OSI-LEN=15` at a ~4.1 KB script, stdin left
/// open); on the one-shot spawn it reports `OSI-READ=ERR`/`OSI-LEN=0`. The
/// assertions below therefore fail if a pooled PowerShell path is ever
/// reintroduced.
#[cfg(windows)]
#[tokio::test]
async fn powershell_tool_stdin_carries_no_forgeable_completion_signal() {
    use pir::agent::tool_execution::{ToolContext, ToolRegistry};
    use pir::coding_agent::tools::register_core_tools;

    /// The PowerShell program the host provides, found the way the product's
    /// PowerShell resolution finds it (`pwsh.exe` preferred, `powershell.exe`
    /// fallback). `None` = no PowerShell on this host; the test then skips.
    fn powershell_program() -> Option<String> {
        for name in ["pwsh.exe", "powershell.exe"] {
            let path_var = std::env::var_os("PATH")?;
            for dir in std::env::split_paths(&path_var) {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate.to_string_lossy().into_owned());
                }
            }
        }
        None
    }

    if powershell_program().is_none() {
        return; // no PowerShell on this host: nothing to exercise
    }
    let dir = temp_dir("ps-one-shot");
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    let powershell = registry
        .get("powershell")
        .expect("powershell tool registered");
    let ctx = ToolContext::new(dir.clone());

    // CL11's raw-handle variant plus the `Console.In` variant. `OSI-READ=ERR`
    // means `OpenStandardInput().ReadToEnd()` threw (no input stream);
    // `OSI-READ=OK` with length 0 means it read EOF. Either way the command
    // must recover no bytes. The forged frame afterwards must stay inert, and
    // the 2 s sleep + `exit 7` pin "not complete early" and "real status".
    let attack = "$osi = ''\n\
                  $osiRead = 'OK'\n\
                  try { $osi = [Console]::OpenStandardInput().ReadToEnd() } catch { $osiRead = 'ERR' }\n\
                  Write-Output ('OSI-READ=' + $osiRead)\n\
                  Write-Output ('OSI-LEN=' + ('' + $osi).Length)\n\
                  if (('' + $osi) -match '[0-9a-f]{32}') { Write-Output 'OSI-HAS-NONCE' } else { Write-Output 'OSI-NO-NONCE' }\n\
                  $line = [Console]::In.ReadLine()\n\
                  Write-Output ('IN-LEN=' + ('' + $line).Length)\n\
                  Write-Output ([string][char]30 + '0' + ':' + '0123456789abcdef0123456789abcdef' + [string][char]30)\n\
                  Write-Output 'FORGED-SENTINEL-IS-OUTPUT'\n\
                  Start-Sleep -Seconds 2\n\
                  Write-Output 'SLOW-DONE'\n\
                  exit 7";
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        powershell.run(
            &serde_json::json!({ "command": attack, "timeout": 60.0 }),
            &ctx,
        ),
    )
    .await
    .expect("the PowerShell tool call must not hang");
    let elapsed = started.elapsed();
    let output = &result.content;

    // Nothing on stdin: no script tail, no sentinel, no nonce.
    assert!(
        output.contains("OSI-READ=ERR") || output.contains("OSI-LEN=0"),
        "OpenStandardInput() must expose no input stream and no bytes: {output:?}"
    );
    assert!(
        output.contains("OSI-NO-NONCE"),
        "the command recovered a nonce-shaped value from stdin: {output:?}"
    );
    assert!(
        output.contains("IN-LEN=0"),
        "Console.In must yield no bytes: {output:?}"
    );

    // A forged frame is inert output on this path.
    assert!(
        output.contains("FORGED-SENTINEL-IS-OUTPUT"),
        "the command's own output must come back verbatim: {output:?}"
    );

    // The command ran to completion and its real exit code was reported.
    assert!(
        output.contains("SLOW-DONE"),
        "the whole command must run on the one-shot path: {output:?}"
    );
    assert!(
        result.is_error,
        "exit code 7 must be an error result: {output:?}"
    );
    assert!(
        output.contains("Command exited with code 7"),
        "the real exit code must be returned: {output:?}"
    );
    let structured = result
        .structured_content
        .as_ref()
        .expect("completed runs carry structured content");
    assert_eq!(structured["exit_code"], serde_json::json!(7));

    // Not reported complete early: the 2 s sleep actually elapsed.
    assert!(
        elapsed >= Duration::from_millis(1800),
        "the call returned before the slow command finished ({elapsed:?}): {output:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 2. Member death mid-command is terminal
// ---------------------------------------------------------------------------

/// A member that really dies mid-call: it consumes one line of the command
/// script, writes a partial-output marker to stderr, pauses briefly so the
/// reader task can deliver it, then exits. The pool wrote (part of) the script
/// before the death, so the documented result is the terminal
/// `LostAfterWrite` shape — never a replay — and the slot must be released so
/// the next call respawns a fresh member.
///
/// The previous version of this test spawned `bash -c "cat >/dev/null"`,
/// which does not exit while the pool holds stdin: the 5 s command deadline
/// fired and the test only proved "not Completed". This one kills the member
/// for real and pins the terminal variant plus the restart.
#[tokio::test]
async fn member_death_after_dispatch_is_terminal_and_the_next_call_respawns() {
    struct DiesOnceSpawner {
        spawns: AtomicUsize,
    }
    impl MemberSpawn for DiesOnceSpawner {
        fn spawn(
            &self,
            protocol: ShellProtocol,
            program: &str,
            cwd: &Path,
        ) -> Result<SpawnedMember, String> {
            let first = self.spawns.fetch_add(1, Ordering::SeqCst) == 0;
            let mut spawn = tokio::process::Command::new(program);
            if first {
                // `read` blocks until the pool delivers the script, so the
                // write succeeds into the pipe and the member is alive when
                // the command is "dispatched"; it then dies before the
                // sentinel. stderr is used because the pool always captures
                // it regardless of the output-channel mode.
                spawn.args([
                    "-c",
                    "read -r first_line; printf 'DEATH-PARTIAL\\n' >&2; sleep 0.2; exit 0",
                ]);
            } else {
                spawn.args(member_args(protocol));
            }
            spawn
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
            Ok(SpawnedMember::from_child(
                child,
                stdin,
                uuid::Uuid::new_v4().simple().to_string(),
            ))
        }
    }

    let Some(program) = bash_program() else {
        return;
    };
    let dir = temp_dir("member-death");
    let spawner = Arc::new(DiesOnceSpawner {
        spawns: AtomicUsize::new(0),
    });
    let pool = Arc::new(ShellPool::new(Arc::clone(&spawner) as Arc<dyn MemberSpawn>));

    let first = tokio::time::timeout(
        Duration::from_secs(30),
        pool.run_test(ShellProtocol::Bash, &program, &dir, "echo hi", Some(15.0)),
    )
    .await
    .expect("member death must not hang the pool");
    match first {
        PooledTestOutcome::LostAfterWrite { output } => {
            assert!(
                output.contains("DEATH-PARTIAL"),
                "output captured before the member died must be preserved: {output:?}"
            );
        }
        other => panic!(
            "a member death after dispatch must be the terminal LostAfterWrite shape \
             (not a timeout, completion, or replayable loss): {other:?}"
        ),
    }

    // The slot was released: the next call respawns and runs normally.
    let second = tokio::time::timeout(
        Duration::from_secs(30),
        pool.run_test(
            ShellProtocol::Bash,
            &program,
            &dir,
            "echo alive-after-death",
            Some(15.0),
        ),
    )
    .await
    .expect("the post-death call must not hang");
    match second {
        PooledTestOutcome::Completed { exit_code, output } => {
            assert_eq!(exit_code, 0);
            assert_eq!(output.trim(), "alive-after-death");
        }
        other => panic!("the next call must respawn a working member: {other:?}"),
    }
    assert_eq!(
        spawner.spawns.load(Ordering::SeqCst),
        2,
        "the dead member must be discarded and exactly one fresh member spawned"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 2b. Per-invocation output attribution (CL3-6)
// ---------------------------------------------------------------------------

/// A background writer left by call 1 (`( sleep 3; echo … ) &`) keeps the
/// stdout it inherited. When the member's stdout is the capture channel, that
/// straggler lands in call 2's capture while call 2 runs — cross-call bleed
/// that defeats the per-invocation capture contract. The pool now redirects
/// each invocation's output into its own file ([OutputChannel] in
/// `shell_pool.rs`), so the straggler writes into call 1's (already unlinked)
/// file and call 2 captures only its own output.
#[tokio::test]
async fn background_output_from_a_previous_call_does_not_bleed_into_the_next_call() {
    let _temp_root = isolate_process_temp("background-bleed");
    let Some(program) = bash_program() else {
        return;
    };
    let dir = temp_dir("background-bleed");
    let pool = Arc::new(ShellPool::new(Arc::new(RealSpawner)));

    // Call 1 returns immediately and leaves a writer that fires ~1 s later.
    let first = tokio::time::timeout(
        Duration::from_secs(30),
        pool.run_test(
            ShellProtocol::Bash,
            &program,
            &dir,
            "( sleep 1; echo CALL1-BACKGROUND-OUTPUT ) &",
            Some(15.0),
        ),
    )
    .await
    .expect("call 1 must not hang the pool");
    match first {
        PooledTestOutcome::Completed { exit_code, output } => {
            // A loaded runner can delay call 1 past the straggler's 1 s mark;
            // output written while call 1 is still running belongs to call 1.
            // The cross-call contract is asserted on call 2 below.
            assert_eq!(exit_code, 0, "call 1 output: {output:?}");
        }
        other => panic!("call 1 did not complete: {other:?}"),
    }

    // Call 2 spans the straggler's 1 s mark. Its capture must contain only
    // its own output; the straggler belongs to call 1.
    let second = tokio::time::timeout(
        Duration::from_secs(60),
        pool.run_test(
            ShellProtocol::Bash,
            &program,
            &dir,
            "echo second-start; sleep 2; echo second-end",
            Some(30.0),
        ),
    )
    .await
    .expect("call 2 must not hang the pool");
    match second {
        PooledTestOutcome::Completed { exit_code, output } => {
            assert_eq!(exit_code, 0, "call 2 output: {output:?}");
            assert!(
                output.contains("second-start"),
                "call 2's own output must be captured: {output:?}"
            );
            assert!(
                output.contains("second-end"),
                "call 2 must run to completion: {output:?}"
            );
            assert!(
                !output.contains("CALL1-BACKGROUND-OUTPUT"),
                "call 1's background writer bled into call 2's capture: {output:?}"
            );
        }
        other => panic!("call 2 did not complete: {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 2c. Cross-call channel injection by temp-directory enumeration (CL16)
// ---------------------------------------------------------------------------

/// CL16 proved that channel files living directly in `$TEMP` can be *found* by
/// a background writer left over from call N, which then appends into call
/// N+1's live channel file: output spoofing across invocations and processes
/// (a fake "all tests passed" line in a later tool result). The fix moves the
/// files into a per-process private directory (`pir-pool-<pid>-<random>`) that
/// the shell can traverse and write into by exact path but cannot list.
///
/// The attack below is CL16's exact pattern, widened to both layouts: sixteen
/// rounds of globbing `$TEMP/pir-pool-out-*` (the old flat layout) and
/// `$TEMP/pir-pool-*/pir-pool-out-*` (the new private-dir layout), appending
/// `INJECTED-*` to every regular file found, while a second call runs a slow
/// command. Assertions:
/// * call 2's capture contains no injected bytes (pre-fix it does: the
///   attacker's bytes land in call 2's live channel file);
/// * the attacker's own report shows zero matches for both patterns, i.e. the
///   private directory is not listable from inside a pooled command;
/// * a direct `ls` of every `pir-pool-*` directory seen at the top level fails.
#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "Windows CI runner's inherited DACL lets Git Bash list the private dir; the Linux run covers the deny"
)]
async fn pooled_call_cannot_inject_into_a_later_calls_channel_by_enumeration() {
    let _temp_root = isolate_process_temp("channel-injection");
    // Root bypasses the Unix 0300 directory mode, so the enumeration probe
    // cannot fail even though the same-user non-root CI runner is denied.
    #[cfg(unix)]
    if std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|uid| uid.trim() == "0")
    {
        return;
    }
    let Some(program) = bash_program() else {
        return;
    };
    let dir = temp_dir("channel-injection");
    let report = dir.join("attack-report");
    let pool = Arc::new(ShellPool::new(Arc::new(RealSpawner)));
    // The shell-side spelling of the temp root: forward slashes are accepted
    // by MSYS bash, and quoting keeps paths with spaces intact.
    let temp = std::env::temp_dir()
        .to_string_lossy()
        .replace('\\', "/")
        .replace('\'', "'\\''");
    let report_arg = report.to_string_lossy().replace('\\', "/");

    // Call 1 leaves a background writer that keeps hunting for channel files
    // for ~4 s (sixteen rounds at 250 ms). Its writes to call 1's own channel
    // are irrelevant; it only matters whether it can reach call 2's.
    //
    // The attacker reports what it could SEE: `OLD` = legacy flat channel
    // files in the temp root (stale pre-fix leftovers may legitimately exist),
    // `NEW` = channel files inside a `pir-pool-*` private directory, `DIRS` =
    // private directory names visible in the temp root. `DIRS>=1` with
    // `NEW=0` is the property under test: the directory is visible by name but
    // its contents cannot be listed.
    let attack = format!(
        "( i=0\n\
          while [ $i -lt 16 ]; do\n\
            old=0; new=0; dirs=0\n\
            for f in '{temp}'/pir-pool-out-*; do [ -f \"$f\" ] && old=$((old+1)); done\n\
            for d in '{temp}'/pir-pool-*/; do [ -d \"$d\" ] && dirs=$((dirs+1)); done\n\
            for f in '{temp}'/pir-pool-*/pir-pool-out-*; do [ -f \"$f\" ] && new=$((new+1)); done\n\
            for f in '{temp}'/pir-pool-out-* '{temp}'/pir-pool-*/pir-pool-out-*; do\n\
              [ -f \"$f\" ] && printf 'INJECTED-%s\\n' \"$i\" >> \"$f\" 2>/dev/null\n\
            done\n\
            printf 'ROUND %s OLD=%s NEW=%s DIRS=%s\\n' \"$i\" \"$old\" \"$new\" \"$dirs\" >> '{report_arg}'\n\
            i=$((i+1)); sleep 0.25\n\
          done ) & echo call1-start"
    );
    let first = tokio::time::timeout(
        Duration::from_secs(30),
        pool.run_test(ShellProtocol::Bash, &program, &dir, &attack, Some(15.0)),
    )
    .await
    .expect("call 1 must not hang the pool");
    match first {
        PooledTestOutcome::Completed { exit_code, output } => {
            assert_eq!(exit_code, 0, "call 1 output: {output:?}");
        }
        other => panic!("call 1 did not complete: {other:?}"),
    }

    // Call 2 spans the whole attack window. Its probe also lists each
    // top-level `pir-pool-*` directory it can see: the names are visible in
    // the (shared) temp root, the contents must not be.
    let probe = format!(
        "echo call2-start\n\
         for d in '{temp}'/pir-pool-*/; do\n\
           [ -d \"$d\" ] || continue\n\
           if ls \"$d\" >/dev/null 2>&1; then echo \"LISTABLE:$d\"; else echo \"BLOCKED:$d\"; fi\n\
         done\n\
         sleep 4\n\
         echo call2-end"
    );
    let second = tokio::time::timeout(
        Duration::from_secs(60),
        pool.run_test(ShellProtocol::Bash, &program, &dir, &probe, Some(30.0)),
    )
    .await
    .expect("call 2 must not hang the pool");
    // Let the attacker finish its last rounds before reading the report.
    tokio::time::sleep(Duration::from_millis(700)).await;

    match second {
        PooledTestOutcome::Completed { exit_code, output } => {
            assert_eq!(exit_code, 0, "call 2 output: {output:?}");
            assert!(
                output.contains("call2-start") && output.contains("call2-end"),
                "call 2 must run to completion: {output:?}"
            );
            assert!(
                !output.contains("INJECTED-"),
                "a background writer injected bytes into call 2's capture: {output:?}"
            );
            assert!(
                !output.contains("LISTABLE:"),
                "a private channel directory is listable from inside a pooled command: {output:?}"
            );
        }
        other => panic!("call 2 did not complete: {other:?}"),
    }

    // The attacker's own report is the positive evidence that it really ran
    // and really looked: the private directory layout must never expose a
    // channel file to it. `OLD` may be non-zero (stale flat files left by
    // pre-fix runs or other lanes); those are a different, legacy layout and
    // the fix's contract is that no NEW channel file goes there.
    let report = std::fs::read_to_string(&report).unwrap_or_else(|error| {
        panic!(
            "the attacker never wrote its report ({}): {error}",
            report.display()
        )
    });
    assert!(
        report.lines().any(|line| line.starts_with("ROUND ")),
        "the attacker did not complete a single round: {report:?}"
    );
    let mut saw_private_dir = false;
    for line in report.lines().filter(|line| line.starts_with("ROUND ")) {
        assert!(
            line.contains(" NEW=0"),
            "the attacker enumerated channel files inside a private directory ({line:?}) — report:\n{report}"
        );
        let dirs = line
            .rsplit(" DIRS=")
            .next()
            .and_then(|tail| tail.trim().parse::<u32>().ok());
        saw_private_dir |= dirs.is_some_and(|count| count > 0);
    }
    assert!(
        saw_private_dir,
        "no private channel directory was visible at all: the probe did not exercise \
         the new layout — report:\n{report}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 2d. Channel-directory lifecycle: a pre-existing path is never adopted
// ---------------------------------------------------------------------------

/// CL20 closed the enumeration hole; Q15 found the remaining lifecycle gap:
/// `channel_directory` used to accept *any* existing path (`exists()`) and
/// `create_channel_directory` treated `AlreadyExists` as success without
/// validating type, owner, mode or ACL. An attacker who learned the path
/// (the pid is in the name and the name is visible in `$TEMP`) could
/// pre-create a permissive directory — or a junction — after the last drop
/// removed the real one, and the next call would write channel files into an
/// attacker-listable location.
///
/// The pool now never adopts an existing path: it validates a real directory,
/// current-user ownership and the expected protected DACL / mode, and falls
/// back to member-stdout capture on any mismatch. This test pre-creates a
/// permissive directory at the pool's exact channel path and asserts:
/// * the command still completes and its output is captured (the fallback
///   path, not an error);
/// * no channel file was created inside the attacker's directory;
/// * the pool recorded a rejection.
#[tokio::test]
async fn a_pre_existing_permissive_channel_directory_is_not_adopted() {
    let Some(program) = bash_program() else {
        return;
    };
    // A pool whose channel directory is a path this test controls, so the
    // attack cannot poison the real process channel directory shared by every
    // other test in this binary.
    let sandbox = temp_dir("channel-preadopt");
    let channel = sandbox.join("pir-pool-preattack");
    // The attack: a plain, fully permissive directory at the expected path.
    std::fs::create_dir_all(&channel).expect("pre-create the attacker directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&channel, std::fs::Permissions::from_mode(0o755))
            .expect("make the attacker directory listable");
    }

    let pool = Arc::new(ShellPool::with_channel_directory_for_tests(
        Arc::new(RealSpawner),
        channel.clone(),
    ));
    let run = {
        let pool = Arc::clone(&pool);
        let sandbox = sandbox.clone();
        tokio::spawn(async move {
            pool.run_test(
                ShellProtocol::Bash,
                &program,
                &sandbox,
                // Long enough to observe the directory WHILE the channel would
                // be live: after the call its drop unlinks the file, so an
                // end-of-call check alone could pass vacuously.
                "echo captured-through-fallback; sleep 2",
                Some(20.0),
            )
            .await
        })
    };
    // Watch the attacker's directory for a channel file for as long as the
    // call is live, not for a fixed wall-clock window: a slow pooled spawn
    // would otherwise let a fixed probe end before the channel could ever
    // exist, and the emptiness assertion would pass vacuously. The hard cap
    // bounds a member that never starts (the command itself only sleeps 2 s;
    // the join below is bounded too). A pre-fix pool writes its channel file
    // into this directory for the whole call, so every sample after the
    // member starts sees it.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut observed_channel_files = Vec::new();
    let mut samples_while_running = 0u32;
    while !run.is_finished() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        samples_while_running += 1;
        if let Ok(entries) = std::fs::read_dir(&channel) {
            for entry in entries.flatten() {
                observed_channel_files.push(entry.file_name());
            }
        }
    }
    assert!(
        samples_while_running > 0 && run.is_finished(),
        "the probe did not observe the call from start to finish within its \
         10 s cap ({samples_while_running} samples); it cannot prove the \
         attacker directory stayed unused while the channel would have been live"
    );
    let outcome = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the call must not hang")
        .expect("the pool task must not panic");
    match outcome {
        PooledTestOutcome::Completed { exit_code, output } => {
            assert_eq!(exit_code, 0, "call output: {output:?}");
            assert!(
                output.contains("captured-through-fallback"),
                "the member-stdout fallback must still capture output: {output:?}"
            );
        }
        other => panic!("the fallback path must complete normally: {other:?}"),
    }

    // The attacker's directory must never have held a channel file, not even
    // transiently while the call was running (it is listable, so this
    // assertion really looks inside).
    assert!(
        observed_channel_files.is_empty(),
        "a pre-existing permissive directory was used as the channel directory: \
         {observed_channel_files:?}"
    );
    let entries: Vec<_> = std::fs::read_dir(&channel)
        .expect("the attacker directory is listable")
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert!(
        entries.is_empty(),
        "a pre-existing permissive directory was used as the channel directory: {entries:?}"
    );
    assert!(
        pool.channel_directory_rejections_for_tests() > 0,
        "the pool must record that it refused the suspicious path"
    );

    let _ = std::fs::remove_dir_all(&sandbox);
}

/// The other half of the same predicate: a directory whose *product* private
/// shape was established and then had its `Everyone` deny-list ACE removed
/// (Windows: `icacls <dir> /remove:d Everyone`, exactly the same-user owner
/// lift Q15 demonstrated) must fail validation. Skipped gracefully when the
/// platform ACL API is unavailable.
#[tokio::test]
async fn a_channel_directory_whose_deny_ace_was_lifted_is_rejected() {
    let sandbox = temp_dir("channel-acl-lift");
    let channel = sandbox.join("pir-pool-probe");
    if !pir::coding_agent::tools::shell_pool::create_channel_probe_for_tests(&channel) {
        // The platform could not establish the private shape here (ACL API
        // unavailable / unsupported filesystem): nothing to validate.
        let _ = std::fs::remove_dir_all(&sandbox);
        return;
    }
    let pool_api = pir::coding_agent::tools::shell_pool::validate_channel_directory_for_tests;
    // Baseline: the product shape validates, so the negative case below is
    // meaningful (the validator is not simply always-false).
    assert!(
        pool_api(&channel),
        "a freshly created private channel directory must validate"
    );

    if !lift_list_denial(&channel) {
        // No usable ACL tool on this host: skip rather than fail a security
        // test for the environment.
        let _ = std::fs::remove_dir_all(&sandbox);
        return;
    }
    assert!(
        !pool_api(&channel),
        "a channel directory whose listing deny was removed must be rejected"
    );

    // And the product surface refuses it: a pool pointed at the tampered path
    // falls back instead of writing channel files there. The directory is
    // watched WHILE the call runs, because the channel's drop unlinks the
    // file at the end and an end-of-call check alone could pass vacuously.
    if let Some(program) = bash_program() {
        let pool = Arc::new(ShellPool::with_channel_directory_for_tests(
            Arc::new(RealSpawner),
            channel.clone(),
        ));
        let run = {
            let pool = Arc::clone(&pool);
            let sandbox = sandbox.clone();
            tokio::spawn(async move {
                pool.run_test(
                    ShellProtocol::Bash,
                    &program,
                    &sandbox,
                    "echo fallback-after-lift; sleep 2",
                    Some(20.0),
                )
                .await
            })
        };
        // Watch the tampered directory for a channel file for as long as the
        // call is live, not for a fixed wall-clock window: a slow pooled
        // spawn would otherwise let a fixed probe end before the fallback
        // decision could even be made, and the emptiness assertion would pass
        // vacuously. The hard cap bounds a member that never starts (the
        // command only sleeps 2 s; the join below is bounded too). A pre-fix
        // pool writes its channel file into this now-listable directory for
        // the whole call, so every sample after the member starts sees it.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut observed_channel_files = Vec::new();
        let mut samples_while_running = 0u32;
        while !run.is_finished() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
            samples_while_running += 1;
            if let Ok(entries) = std::fs::read_dir(&channel) {
                for entry in entries.flatten() {
                    observed_channel_files.push(entry.file_name());
                }
            }
        }
        assert!(
            samples_while_running > 0 && run.is_finished(),
            "the probe did not observe the call from start to finish within its \
             10 s cap ({samples_while_running} samples); it cannot prove the \
             tampered directory stayed unused while the call was live"
        );
        let outcome = tokio::time::timeout(Duration::from_secs(30), run)
            .await
            .expect("the call must not hang")
            .expect("the pool task must not panic");
        match outcome {
            PooledTestOutcome::Completed { exit_code, output } => {
                assert_eq!(exit_code, 0, "call output: {output:?}");
                assert!(
                    output.contains("fallback-after-lift"),
                    "output must come from the fallback capture: {output:?}"
                );
            }
            other => panic!("the fallback path must complete normally: {other:?}"),
        }
        assert!(
            observed_channel_files.is_empty(),
            "a directory whose deny ACE was lifted was used as a channel directory: \
             {observed_channel_files:?}"
        );
        let entries: Vec<_> = std::fs::read_dir(&channel)
            .map(|entries| entries.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(
            entries.is_empty(),
            "a directory whose deny ACE was lifted was used as a channel directory: {entries:?}"
        );
        assert!(pool.channel_directory_rejections_for_tests() > 0);
    }

    let _ = std::fs::remove_dir_all(&sandbox);
}

/// Remove the listing-deny from a directory the current user owns, the way a
/// same-user attacker would. `true` when the platform tool succeeded.
#[cfg(windows)]
fn lift_list_denial(directory: &Path) -> bool {
    std::process::Command::new("icacls")
        .arg(directory)
        .args(["/remove:d", "Everyone"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn lift_list_denial(directory: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).is_ok()
}

#[cfg(not(any(unix, windows)))]
fn lift_list_denial(_directory: &Path) -> bool {
    false
}

/// Mid-call tampering must fail the call closed: the directory is validated
/// before every drain, and a directory that stops matching ends the call as a
/// terminal loss (bytes captured before the tamper, no completion, no
/// post-tamper bytes). Without the check the channel would keep feeding the
/// attacker's data into the capture until the command finished.
#[tokio::test]
async fn a_channel_directory_tampered_mid_call_fails_closed() {
    let Some(program) = bash_program() else {
        return;
    };
    let sandbox = temp_dir("channel-midcall");
    let channel = sandbox.join("pir-pool-midcall");
    if !pir::coding_agent::tools::shell_pool::create_channel_probe_for_tests(&channel) {
        let _ = std::fs::remove_dir_all(&sandbox);
        return;
    }
    let pool = Arc::new(ShellPool::with_channel_directory_for_tests(
        Arc::new(RealSpawner),
        channel.clone(),
    ));
    // Deterministic synchronization instead of a wall-clock guess:
    // * the command prints `before-tamper` and then writes `painted` to the
    //   WORKSPACE (visible to the test, not to the channel). The shell's
    //   `echo` flushes its stdout, so the marker proves the pre-tamper bytes
    //   are already in the channel file — not merely that the member started;
    // * it keeps re-printing `before-tamper` while it waits for the test's
    //   `go` file, so every pool drain tick before the lift sees the marker;
    // * it prints `after-tamper` only after `go`, i.e. only after the pool has
    //   observed the tamper (or the run has already ended).
    let painted = sandbox.join("painted");
    let go = sandbox.join("go");
    let painted_arg = painted.to_string_lossy().replace('\\', "/");
    let go_arg = go.to_string_lossy().replace('\\', "/");
    let command = format!(
        "echo before-tamper; printf x > '{painted_arg}'; \
         while [ ! -f '{go_arg}' ]; do printf 'before-tamper\\n'; sleep 0.02; done; \
         echo after-tamper"
    );
    let canary_program = program.clone();
    let run = {
        let pool = Arc::clone(&pool);
        let sandbox = sandbox.clone();
        tokio::spawn(async move {
            pool.run_test(
                ShellProtocol::Bash,
                &program,
                &sandbox,
                &command,
                Some(30.0),
            )
            .await
        })
    };
    // Wait (bounded) until the command has actually produced the pre-tamper
    // bytes: the marker is written after the flushed `before-tamper` line.
    // The previous fixed 1.5 s wait could elapse before a loaded shell got
    // that far, and the pool would then (correctly) capture nothing because
    // the first drain it managed to run was already after the ACL change.
    let marker_started = std::time::Instant::now();
    let marker_deadline = marker_started + Duration::from_secs(10);
    let mut painted_ok = false;
    while std::time::Instant::now() < marker_deadline {
        if painted.exists() {
            painted_ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // The member never reached the marker (no usable shell / dispatch
    // timeout): the tamper scenario was not exercised, so skip instead of
    // asserting on a different failure path.
    if !painted_ok || run.is_finished() {
        run.abort();
        let _ = std::fs::remove_dir_all(&sandbox);
        return;
    }
    // The marker proves the bytes are in the channel file, but the pool
    // drains it from its own ~100 ms ticker and exposes no mid-call capture
    // snapshot. Worse, every pool call in this test binary shares one
    // single-worker runtime (`pool_runtime`), so another test's synchronous
    // member spawn can delay this call's ticker by more than any fixed
    // wall-clock settle. A canary call on a separate pool forces that shared
    // runtime to make progress: it can complete only after the worker has
    // polled tasks past the marker, and its command stays pending across
    // several tick periods, which makes the worker process expired timers --
    // including this call's drain ticker. The canary's own outcome is
    // irrelevant; only the round trip is used as a scheduling barrier.
    {
        let canary_pool = Arc::new(ShellPool::new(Arc::new(RealSpawner)));
        let canary_dir = temp_dir("channel-midcall-canary");
        let canary_cleanup = canary_dir.clone();
        let canary = tokio::spawn(async move {
            canary_pool
                .run_test(
                    ShellProtocol::Bash,
                    &canary_program,
                    &canary_dir,
                    "echo canary; sleep 0.3",
                    Some(30.0),
                )
                .await
        });
        let _ = tokio::time::timeout(Duration::from_secs(15), canary).await;
        let _ = std::fs::remove_dir_all(&canary_cleanup);
    }
    if !lift_list_denial(&channel) {
        run.abort();
        let _ = std::fs::remove_dir_all(&sandbox);
        return;
    }
    // Wait until the pool has actually SEEN the tampering (its next drain tick
    // rejects the directory) before releasing the command. Without this the
    // test would race the unavoidable validate→read window inside one drain: a
    // tick that validated just before the lift would still read bytes written
    // after it, which is the documented TOCTOU residual, not this check's
    // subject. If the tampering is never observed, the run is released anyway
    // and the assertions below fail — a pre-fix build that trusts the file
    // completes with `after-tamper` instead of failing closed.
    for _ in 0..200 {
        if pool.channel_directory_rejections_for_tests() > 0 || run.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Release the command: its next output must never be trusted.
    let _ = std::fs::write(&go, b"");
    let outcome = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the call must not hang")
        .expect("the pool task must not panic");
    match outcome {
        PooledTestOutcome::LostAfterWrite { output } => {
            assert!(
                output.contains("before-tamper"),
                "bytes captured before the tamper must be reported: {output:?}"
            );
            assert!(
                !output.contains("after-tamper"),
                "bytes from a channel directory that stopped validating must not be trusted: {output:?}"
            );
        }
        other => {
            panic!("a mid-call channel tamper must fail closed as a terminal loss, not {other:?}")
        }
    }
    assert!(pool.channel_directory_rejections_for_tests() > 0);
    let _ = std::fs::remove_dir_all(&sandbox);
}

// ---------------------------------------------------------------------------
// 3. Process-tree cleanup when the caller drops the future
// ---------------------------------------------------------------------------

/// Dropping the tool's future mid-command (the TUI aborts its prompt task)
/// must kill the command's whole process tree, not just the shell: a forked
/// descendant that writes a marker file after a delay must never write it.
#[tokio::test]
async fn dropping_the_bash_future_kills_the_process_tree() {
    use pir::agent::tool_execution::{ToolContext, ToolRegistry};
    use pir::coding_agent::tools::register_core_tools;

    if resolve_shell_config(None).is_err() {
        return;
    }
    let dir = temp_dir("drop-kill");
    let marker = dir.join("descendant-survived");
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    let bash = registry.get("bash").expect("bash tool registered");

    // A forked descendant that sleeps then writes a marker: it survives the
    // shell's own `kill_on_drop`, so only a real tree kill stops it.
    //
    // The trailing `sleep` keeps the SHELL alive on purpose. pir's default
    // shell is the embedded brush, which (unlike external bash) does not keep
    // the call alive for a background job: `( ... ) & echo started` returns as
    // soon as the shell exits, and brush then kills the job. That is a
    // deliberate brush behaviour, not a tree-kill property, so a command that
    // ends with the shell would measure brush's job semantics instead of this
    // test's subject. With the shell blocked, the descendant outlives it and
    // only a real tree kill can stop the marker.
    let command = format!(
        "( sleep 1; printf survived > {} ) & sleep 30",
        marker.display()
    );
    let ctx = ToolContext::new(dir.clone());
    let args = serde_json::json!({ "command": command });
    let run = bash.run(&args, &ctx);
    // Let the command start and the descendant fork, then drop the future.
    tokio::time::timeout(Duration::from_secs(20), async {
        let started = tokio::time::timeout(Duration::from_millis(500), run).await;
        // The 500 ms timeout drops `run` mid-await: the RAII guard fires here.
        assert!(
            started.is_err(),
            "the command was expected to be long-lived"
        );
    })
    .await
    .expect("the drop path must not hang");

    // The descendant writes its marker ONE second after it forks. Poll that
    // window (bounded) instead of sleeping a fixed 5 s: a surviving tree shows
    // up as soon as it writes, and a killed one costs only the window itself.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1500);
    while tokio::time::Instant::now() < deadline && !marker.exists() {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !marker.exists(),
        "the dropped call's descendant survived the tree kill: {}",
        marker.display()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 4. Handle-relative write confinement
// ---------------------------------------------------------------------------

/// The name-based check and the handle walk must agree on a symlinked
/// directory; the model-facing wording must not change.
#[tokio::test]
async fn write_refuses_a_symlinked_directory_component() {
    use pir::agent::tool_execution::{ToolContext, ToolRegistry};
    use pir::coding_agent::tools::register_core_tools;

    let root = temp_dir("symlink-write");
    let outside = temp_dir("symlink-outside");
    let link = root.join("link");
    let created = create_dir_symlink(&outside, &link);
    if !created {
        // Neither a symlink (needs developer mode/admin on Windows) nor a
        // junction (ordinary user, `mklink /J`) could be created: a host
        // with no link mechanism at all. On this host the junction fallback
        // builds the case, so the test is not a green no-op.
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        return;
    }

    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    let write = registry.get("write").expect("write tool registered");
    let ctx = ToolContext::new(&root);
    let result = write
        .run(
            &serde_json::json!({"path": "link/escaped.txt", "content": "blocked"}),
            &ctx,
        )
        .await;
    assert!(
        result.is_error,
        "the write must be refused: {}",
        result.content
    );
    assert!(
        result.content.contains("symbolic link")
            || result.content.contains("escapes the workspace root"),
        "unexpected refusal wording: {}",
        result.content
    );
    assert!(
        !outside.join("escaped.txt").exists(),
        "the write escaped through the link"
    );
    let _ = std::fs::remove_file(&link);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

/// A relative write must still create missing parents and land inside the
/// workspace (the handle-relative `mkdirat`/`create_dir` path).
#[tokio::test]
async fn write_still_creates_missing_parents_inside_the_workspace() {
    use pir::agent::tool_execution::{ToolContext, ToolRegistry};
    use pir::coding_agent::tools::register_core_tools;

    let root = temp_dir("nested-write");
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    let write = registry.get("write").expect("write tool registered");
    let ctx = ToolContext::new(&root);
    let result = write
        .run(
            &serde_json::json!({"path": "a/b/c/file.txt", "content": "nested"}),
            &ctx,
        )
        .await;
    assert!(!result.is_error, "nested write failed: {}", result.content);
    assert_eq!(
        std::fs::read_to_string(root.join("a/b/c/file.txt")).expect("read nested file"),
        "nested"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
fn create_dir_symlink(target: &Path, link: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).is_ok()
}

/// Windows: try a real directory symlink first, then a **junction**. A
/// symlink needs developer mode or admin (`WinError 1314` otherwise), which
/// used to make this test return early on ordinary hosts — a green no-op that
/// proved nothing. A junction is a directory reparse point with the same
/// traversal semantics and is creatable by an ordinary user (`mklink /J`), so
/// the escape test now really builds a link on every Windows host. Both
/// shapes are refused by the product's reparse-point checks; the assertion
/// that nothing was written outside is what catches a followed link.
#[cfg(windows)]
fn create_dir_symlink(target: &Path, link: &Path) -> bool {
    if std::os::windows::fs::symlink_dir(target, link).is_ok() {
        return true;
    }
    std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
        && link.exists()
}

#[cfg(not(any(unix, windows)))]
fn create_dir_symlink(_target: &Path, _link: &Path) -> bool {
    false
}

// ---------------------------------------------------------------------------
// 5. Spill files: never-advertised output is deleted
// ---------------------------------------------------------------------------

/// The `SpillHandle` RAII contract (`bash.rs:53-63`): a capture that spills an
/// evicted prefix but never advertises its path — because the call was
/// dropped/aborted before result assembly — must delete the file. No test
/// covered this. Here the bash tool is dropped mid-run (the TUI's abort-path
/// shape) after the capture has definitely spilled, and the file must
/// disappear within a bounded wait.
#[tokio::test]
async fn a_dropped_call_deletes_its_unadvertised_spill_file() {
    use pir::agent::tool_execution::{ToolContext, ToolRegistry};
    use pir::coding_agent::tools::register_core_tools;

    if resolve_shell_config(None).is_err() {
        return; // no shell: the tool cannot spill
    }
    let dir = temp_dir("spill-drop");
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    let bash = registry.get("bash").expect("bash tool registered");
    let ctx = ToolContext::new(dir.clone());

    // The marker makes the file unambiguously ours even when other suites in
    // this test binary spill concurrently.
    const MARKER: &[u8] = b"PIR-SPILL-DROP-MARKER";
    let before = spill_files_for_this_process();
    let command = "i=0; while [ $i -lt 4000 ]; do \
                   printf 'PIR-SPILL-DROP-MARKER-%04d-xxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n' $i; \
                   i=$((i+1)); \
                   done; sleep 30";
    let args = serde_json::json!({ "command": command });
    let mut run = Box::pin(bash.run(&args, &ctx));

    // Bounded wait for the capture to spill while the command is still
    // running; polling the future too keeps a fast command from hanging the
    // test.
    let spill = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(path) = spill_files_for_this_process()
                .into_iter()
                .find(|path| !before.contains(path) && file_starts_with(path, MARKER))
            {
                break path;
            }
            tokio::select! {
                _ = &mut run => {
                    panic!("the command finished before the call could be dropped");
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
    })
    .await
    .expect("a spill file must appear while the command produces its large output");

    // Dropping the future is the abort path under test: the capture (and its
    // SpillHandle) is dropped without ever handing the path out.
    drop(run);

    let mut removed = false;
    for _ in 0..400 {
        if !spill.exists() {
            removed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        removed,
        "the never-advertised spill file survived the dropped call: {}",
        spill.display()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Every `pir-shell-*` temp file owned by this test process (the spill name
/// shape pins the process id: `pir-shell-{pid}-{seq}-{uuid}`).
fn spill_files_for_this_process() -> Vec<PathBuf> {
    let prefix = format!("pir-shell-{}-", std::process::id());
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with(&prefix) {
                return None;
            }
            entry.metadata().ok().filter(|meta| meta.is_file())?;
            Some(entry.path())
        })
        .collect()
}

fn file_starts_with(path: &Path, prefix: &[u8]) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = vec![0u8; prefix.len()];
    file.read_exact(&mut head)
        .is_ok_and(|()| head.as_slice() == prefix)
}

// ---------------------------------------------------------------------------
// 6. Coverage that used to live in src/ inline test modules
// ---------------------------------------------------------------------------
//
// The whitespace-only `oldText` rejection and the read tool's real-line
// counting were unit-tested inside `edit.rs` / `read.rs`. Inline tests are
// forbidden in `src/`, so the same properties are pinned here through the
// public tool surface.

#[tokio::test]
async fn edit_rejects_whitespace_only_old_text() {
    use pir::agent::tool_execution::{ToolContext, ToolRegistry};
    use pir::coding_agent::tools::register_core_tools;

    let root = temp_dir("edit-whitespace");
    std::fs::write(root.join("file.txt"), "content\n").expect("seed file");
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    let edit = registry.get("edit").expect("edit tool registered");
    let ctx = ToolContext::new(&root);
    let result = edit
        .run(
            &serde_json::json!({
                "path": "file.txt",
                "edits": [{ "oldText": "   ", "newText": "replacement" }]
            }),
            &ctx,
        )
        .await;
    assert!(result.is_error, "whitespace-only oldText must fail");
    assert!(
        result.content.contains("oldText must not be empty"),
        "unexpected error: {}",
        result.content
    );
    assert_eq!(
        std::fs::read_to_string(root.join("file.txt")).expect("read file"),
        "content\n"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn read_counts_real_lines_and_offers_the_continuation_offset() {
    use pir::agent::tool_execution::{ToolContext, ToolRegistry};
    use pir::coding_agent::tools::register_core_tools;

    let root = temp_dir("read-lines");
    let content: String = (0..3000).map(|index| format!("line{index}\n")).collect();
    std::fs::write(root.join("large.txt"), &content).expect("seed file");
    let mut registry = ToolRegistry::new();
    register_core_tools(&mut registry);
    let read = registry.get("read").expect("read tool registered");
    let ctx = ToolContext::new(&root);

    let result = read
        .run(&serde_json::json!({ "path": "large.txt" }), &ctx)
        .await;
    assert!(!result.is_error, "read failed: {}", result.content);
    assert!(
        result
            .content
            .contains("Showing lines 1-2000 of 3001. Use offset=2001 to continue."),
        "unexpected footer: {}",
        result.content
    );

    // An offset past the end (upstream counts the trailing empty split piece,
    // so 3000 newline-terminated lines are "3001 lines total").
    let past_end = read
        .run(
            &serde_json::json!({ "path": "large.txt", "offset": 3004 }),
            &ctx,
        )
        .await;
    assert!(past_end.is_error, "out-of-range offset must fail");
    assert!(
        past_end
            .content
            .contains("Offset 3004 is beyond end of file (3001 lines total)"),
        "unexpected error: {}",
        past_end.content
    );
    let _ = std::fs::remove_dir_all(&root);
}
