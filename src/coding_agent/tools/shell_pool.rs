//! Persistent pooled shell process for the **bash** tool.
//!
//! Upstream (packages/coding-agent/src/core/tools/bash.ts) spawns a FRESH
//! shell for every tool call (`spawn(..., { cwd })`): on Windows that costs
//! ~300 ms of git-bash/cygwin process creation per bash call. This module
//! keeps ONE long-lived bash member (MSYS bash or the embedded brush) alive
//! and runs every command inside it, reducing the per-call cost to a pipe
//! round-trip.
//!
//! **The PowerShell member is gone from the product.** It delivered the
//! command script on the member's stdin together with a per-invocation
//! completion nonce; pwsh 7 parses `-Command -` lazily, so a command could
//! reach the not-yet-consumed tail of that channel through
//! `[Console]::OpenStandardInput()` — which bypasses `Console.In` (and thus
//! the `[Console]::SetIn([TextReader]::Null)` guard), and Windows PowerShell
//! 5.1 exposed `Console.In` itself without the guard — lift the nonce and
//! print a byte-identical sentinel: forged rc, early completion, the real
//! command still running (CL3-1, CL11). The nonce is unguessable only while
//! it stays off every channel the command can read, and the member's stdin is
//! exactly such a channel. The PowerShell tool now always takes its per-call
//! one-shot spawn (`pwsh -Command <script>`, stdin null, completion = process
//! exit), which carries no secret at all. The PowerShell protocol arms
//! (handshake, script composition, member slot) were DELETED with it so no
//! future caller can re-enable that path by flipping a flag. The trade is one
//! process spawn (400-1500 ms of .NET startup) per PowerShell call.
//!
//! Pooled-bash semantics mapping (each property verified against bash.ts):
//!
//! - **Fresh-shell equivalence**: upstream never persists state across calls
//!   (every spawn starts in `ctx?.cwd || cwd`; the session manager has no cwd
//!   setter, so `cd` side effects do NOT survive a call). The pool reproduces
//!   this per command: bash runs the command in a subshell that first `cd`s
//!   to the call's working directory. The subshell boundary structurally
//!   blocks every cross-call leak (exports, functions, `set -e`, umask).
//! - **PI_\* session env**: upstream injects/clears PI_PROVIDER, PI_MODEL,
//!   PI_REASONING_LEVEL (and session id/file) at every spawn. The pool
//!   replays exactly that clear+set at the top of every command script, so
//!   the values visible to a command always match the per-call spawn — even
//!   when the session model changes mid-session (no stale member env).
//! - **Exit code**: the subshell's `$?` flows through the sentinel (bash
//!   itself reports signal deaths as 128+signal, matching the per-call
//!   mapping).
//! - **Timeout/abort**: the whole member process tree is killed (the
//!   per-call `ProcessTreeKill` Job Object / process-group precedent); the
//!   run returns the timeout/abort result and the next call respawns a fresh
//!   member, so a poisoned command never survives into the pool.
//! - **Fallback**: member spawn failure or sentinel-protocol loss (member
//!   died / stdin broken) falls back to the per-call spawn path with a typed
//!   warn — the pre-pool behavior is the floor, never worse.
//! - **Per-invocation output attribution**: every command writes its output
//!   into a fresh per-invocation file ([`OutputChannel`]) inside a private
//!   per-process directory; the member's stdout carries only the sentinel. A
//!   background writer left behind by call N therefore keeps writing into
//!   call N's file — which is unlinked when the call ends — and can never
//!   appear in call N+1's capture. This reproduces the per-call path's
//!   ownership boundary (each call owns its pipes and closes them when it
//!   finishes) inside the shared member. The private directory also stops a
//!   same-user process from *enumerating* live channel files, which closed
//!   the cross-call output-spoofing hole CL16 proved. The directory is never
//!   adopted on trust: a path that already exists (a pre-created directory, a
//!   junction, a leftover after the last drop) is validated — real directory,
//!   current-user owner, expected protected DACL / mode — and rejected with a
//!   fallback when it does not match, and the same check runs before every
//!   drain so a directory subverted mid-call ends the call rather than
//!   feeding spoofed bytes into the capture. See the [`OutputChannel`] and
//!   [`channel_directory`] docs.
//! - **Streaming**: the output channel is drained into the per-command
//!   [`ShellCapture`] rolling buffer on the same cadence as the per-call
//!   path's pipe readers, and the sentinel itself never reaches the capture,
//!   so the throttled `update_snapshot` stream is shape-identical.
//!
//! One member per shell kind, mutex-serialized: within a session the bash
//! tool is `ToolExecutionMode::Sequential` (no parallel bash upstream either
//! in pir), and cross-session concurrent calls queue on the member instead
//! of getting a second process — the "simple and correct" choice.
//!
//! ## Residual risk of the channel directory (documented, not fixed here)
//!
//! The private directory closes *enumeration*, not same-user authority. The
//! owner of the directory is this process's user — a background writer left by
//! a pooled command runs with the same token — so an **adaptive** same-uid
//! process that knows (or guesses) the path can rewrite its own DACL
//! (`icacls <dir> /remove:d Everyone`, verified on this host) or `chmod` it,
//! then append to the live channel file. Validation makes that a *detected*
//! attack — the call fails closed as a terminal loss once the directory stops
//! matching — but it can only detect it, not prevent the first write, and a
//! path-based check has an unavoidable TOCTOU window between validation and
//! the file open. Preventing output spoofing by an adaptive same-user process
//! therefore needs OS-level separation (a different user/integrity boundary)
//! or a handle-based channel outside the same-user namespace (e.g. an
//! inherited pipe/anonymous handle the command can hold but cannot open by
//! name). **The product chooses the non-adaptive threat model:** an attacker
//! from an earlier call can no longer find a channel, and any tampering it
//! attempts on the directory is rejected when it happens; an attacker that
//! deliberately lifts its own access control and races this process is out of
//! scope and can still spoof output.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

use super::bash::{
    shell_heartbeat_payload, ShellCapture, SHELL_HEARTBEAT_INTERVAL, UPDATE_THROTTLE_MS,
};
use crate::agent::tool_execution::{ToolSessionEnv, ToolUpdateFn};
use crate::ai::types::AbortSignal;

// This module is the crate-visible home of the shell-spawn seam: the tool's
// per-call arm and the out-of-loop bash (`agent_session/bash.rs`) reuse the
// same spawn target and tree-kill handle from here instead of duplicating the
// Git-for-Windows shim detection (which `bash.rs` owns). The re-export stays
// `pub(crate)`: the underlying items live in the private `bash` module.
pub(crate) use super::bash::{
    apply_tree_kill_spawn_attributes, shell_spawn_target, ProcessTreeKill,
};

/// Bound on acquiring a pooled member for one command, including a spawn
/// another call triggered. The member is mutex-serialized, so a
/// long-running command must not make the next call wait invisibly; on expiry
/// the caller falls back to a fresh per-call shell.
const MEMBER_DISPATCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Sentinel framing byte (ASCII record separator, byte-verified through the
/// bash/brush console output on Windows and Unix).
const SENTINEL_RS: u8 = 0x1e;

/// Per-invocation sentinel nonce length (uuid v4 simple form = 32 hex chars).
///
/// The nonce is regenerated for EVERY command, never derived from the member:
/// a command that learns one invocation's nonce (shell variables, `ps`, a
/// leaked script) cannot forge completion of any other invocation, and cannot
/// replay the sentinel from a previous run.
///
/// This is deliberately stronger than the per-process token the security
/// review proposed (PLAN §6b item 5): a per-process secret still has to reach
/// the member through the same script or its environment — both observable to
/// the command — and stays valid for every later call once leaked. Rotating
/// per invocation gives the parser an unguessable token on every call while a
/// leaked value dies with the call that delivered it.
const TOKEN_LEN: usize = 32;

/// Upper bound of the sentinel on the wire: RS + up to 10 rc digits + ':' +
/// nonce + RS.
const SENTINEL_MAX_LEN: usize = 1 + 10 + 1 + TOKEN_LEN + 1 + 2;

/// How long held-back tail bytes may sit before the stale flush releases
/// everything that cannot start a sentinel (sub-46-byte outputs otherwise
/// never stream mid-run).
const STALE_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Fixed spawn arguments for a pooled member: the bash member reads command
/// scripts on stdin bare.
pub fn member_args(protocol: ShellProtocol) -> &'static [&'static str] {
    match protocol {
        ShellProtocol::Bash => &[],
    }
}

/// Spawn arguments for the member actually started with `program`. The
/// embedded shell (`pir __brush`) is a plain program whose only argument is
/// the hidden subcommand; it reads the pool's command scripts from stdin
/// exactly like bash. Every other program keeps [`member_args`]' shape.
fn member_args_for(embedded: bool) -> &'static [&'static str] {
    if embedded {
        &["__brush"]
    } else {
        member_args(ShellProtocol::Bash)
    }
}

/// Which shell kind a pooled member runs.
///
/// Single-variant on purpose: the pooled PowerShell member was removed from
/// the product (stdin-borne sentinel nonce; see the module docs), so there is
/// no way to spell it here. Its protocol arms were deleted rather than
/// feature-gated — a future caller cannot re-enable the vulnerable path
/// without writing a new protocol from scratch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellProtocol {
    Bash,
}

// ---------------------------------------------------------------------------
// Command-script composition
// ---------------------------------------------------------------------------

/// Single-quote a value for bash: `'` becomes `'\''`.
fn bash_single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The per-command PI_* environment replay (upstream `resolveSpawnContext`):
/// previous values are always cleared first, then the session's are set.
fn bash_env_lines(session_env: Option<&ToolSessionEnv>) -> String {
    let mut lines = String::from(
        "unset PI_SESSION_ID PI_SESSION_FILE PI_PROVIDER PI_MODEL PI_REASONING_LEVEL\n",
    );
    if let Some(env) = session_env {
        let mut exports: Vec<(&str, String)> = Vec::new();
        if let Some(session_id) = env.session_id.as_deref().filter(|id| !id.is_empty()) {
            exports.push(("PI_SESSION_ID", session_id.to_owned()));
        }
        if let Some(session_file) = &env.session_file {
            exports.push((
                "PI_SESSION_FILE",
                session_file.to_string_lossy().into_owned(),
            ));
        }
        exports.push(("PI_PROVIDER", env.model.provider.to_string()));
        exports.push(("PI_MODEL", env.model.id.clone()));
        exports.push(("PI_REASONING_LEVEL", env.thinking.wire_key().to_string()));
        for (name, value) in exports {
            lines.push_str(&format!("export {name}={}\n", bash_single_quoted(&value)));
        }
    }
    lines
}

/// One pooled bash command:
/// ```text
/// (
/// unset PI_*; export PI_*...
/// cd '<cwd>' || exit $?
/// <command>
/// ) > '<output-file>' 2>&1 < /dev/null
/// __pir_rc=$?
/// printf '\036%s:<nonce>\036' "$__pir_rc"
/// unset __pir_rc
/// ```
/// The subshell isolates all state; the stdin redirect reproduces upstream's
/// stdin-ignored spawn (commands reading stdin see EOF instead of eating the
/// command channel); `cd` failure (only possible in a deleted-dir race, the
/// caller checks `is_dir` first) exits the subshell non-zero instead of
/// running the command in the wrong directory.
///
/// When the pool created a per-invocation [`OutputChannel`], the subshell's
/// stdout *and* stderr are redirected into it. That is what attributes output
/// to the invocation that produced it: a `( … ) &` background job inherits
/// this redirect, so a straggler it writes after the call ends lands in its
/// own call's file (unlinked by then), not in the next call's capture.
///
/// The completion nonce is a **per-invocation** random string carried only by
/// the literal in the `printf` line, which the shell reads AFTER the subshell
/// has exited. It is deliberately not embedded in any variable name and not
/// stored in a variable at all: a forked child inherits the parent shell's
/// variables, so a per-member token spelled into `__pir_rc_<token>` (the
/// earlier shape) let any command list its environment, recover the token and
/// print a forged sentinel that terminated the wait early with a fake exit
/// code. The subshell's own stdin is `/dev/null`, so the not-yet-read tail of
/// the script — the only place the nonce exists while the command runs — is
/// not readable from inside it either.
///
/// Portability caveat (documented, not reproducible on Windows): the script
/// and the nonce-bearing tail share the member's stdin, so if a member were
/// ever spawned with **file-backed** stdin (`bash < script`) instead of the
/// pool's pipe, a forked child on Unix could read the remaining script through
/// `/proc/$PPID/fd/0` and learn the nonce literal before the shell consumes
/// it. Production members are spawned with `stdin(piped)`, the command
/// subshell's stdin is the null device, and the Windows bash/brush members
/// were probed safe; the risk is the stdin channel shape, not any current
/// call site.
///
/// Mitigation if the spawn ever changes: put the nonce on a channel a
/// descendant **cannot inherit**, or close the descriptor in the child. A
/// dedicated fd is NOT sufficient by itself: descriptors are inherited across
/// `fork`, and the subshell's `< /dev/null` only rebinds fd 0. `exec 3<script`
/// followed by `( cat <&3 ) </dev/null` still prints the script — fd 3 stays
/// open across the subshell. A new fd only helps if the subshell explicitly
/// closes it (`exec 3<&-`) before running the command, or if the control data
/// is delivered over a channel no descendant can reach (e.g. a pipe only the
/// parent reads, or a path the command cannot open).
///
/// `null_device` is the shell-visible null path: MSYS bash understands
/// `/dev/null`, while brush on Windows opens `NUL` (its special-file hook
/// only recognizes absolute `/dev/null`, which a bare `/dev/null` is not on
/// Windows). The caller picks it from the member that will run the script.
fn compose_bash_script(
    cwd: &Path,
    command: &str,
    session_env: Option<&ToolSessionEnv>,
    nonce: &str,
    null_device: &str,
    output_file: Option<&Path>,
) -> String {
    // After the sentinel the member `cd`s back to the process temp root: the
    // member is long-lived, and on Windows a process pins its current
    // directory against deletion — parking it in the last call's cwd would
    // make a caller's temp-dir cleanup fail with "file in use" (the pool
    // never persists cwd across calls anyway; every command re-cds first).
    let park = bash_single_quoted(&std::env::temp_dir().to_string_lossy());
    let output_redirect = match output_file {
        Some(path) => format!("> {} 2>&1 ", bash_single_quoted(&path.to_string_lossy())),
        None => String::new(),
    };
    format!(
        "(\n{}cd {} || exit $?\n{}\n) {output_redirect}< {null_device}\n__pir_rc=$?\nprintf '\\036%s:{nonce}\\036' \"$__pir_rc\"\nunset __pir_rc\ncd {park} 2>{null_device} || true\n",
        bash_env_lines(session_env),
        bash_single_quoted(&cwd.to_string_lossy()),
        command,
    )
}

/// Compose the one pooled bash command script (see [`compose_bash_script`]).
fn compose_script(
    cwd: &Path,
    command: &str,
    session_env: Option<&ToolSessionEnv>,
    nonce: &str,
    embedded: bool,
    output_file: Option<&Path>,
) -> String {
    compose_bash_script(
        cwd,
        command,
        session_env,
        nonce,
        bash_null_device(embedded),
        output_file,
    )
}

/// A fresh per-invocation sentinel nonce (32 hex characters from a v4 UUID).
fn fresh_nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// The null device a bash member can open: MSYS bash understands `/dev/null`;
/// brush on Windows needs `NUL` (its special-file hook only rewrites absolute
/// `/dev/null`, and a bare `/dev/null` is not absolute there — the redirect
/// would fail with ENOENT on every command). Everywhere else `/dev/null`.
fn bash_null_device(embedded: bool) -> &'static str {
    if embedded && cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

// ---------------------------------------------------------------------------
// Per-invocation output channel
// ---------------------------------------------------------------------------

/// Prefix of the per-invocation output files (see [`OutputChannel`]).
const OUTPUT_FILE_PREFIX: &str = "pir-pool-out-";

/// Hard cap on ONE per-invocation output-channel file. The in-memory capture
/// is bounded separately; this bounds the file the shell writes between
/// drains. A command that produces more output ends the call as a terminal
/// loss (the member tree is killed and the file unlinked) instead of filling
/// the disk for as long as the command runs.
const OUTPUT_FILE_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Prefix of the per-process private channel directory (see
/// [`channel_directory`]).
const CHANNEL_DIR_PREFIX: &str = "pir-pool-";

/// Age cap for the opportunistic cleanup of output files that could not be
/// removed at call end (a background writer still holding them on Windows).
/// One file serves one command, so a day is far past any legitimate use; the
/// generous window also keeps a concurrent sweep from reaping the channel of
/// a long-running command in another process.
const OUTPUT_FILE_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Minimum spacing between output-file cleanup scans: walking the whole temp
/// directory is not a per-command cost.
const OUTPUT_FILE_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Process-unique sequence for output-file names (the UUID suffix makes them
/// unguessable even by a same-user process; the sequence keeps concurrent
/// runs within one process distinct).
static OUTPUT_FILE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The throttle for [`sweep_output_files`].
static LAST_OUTPUT_SWEEP: OnceLock<Mutex<Option<std::time::Instant>>> = OnceLock::new();

/// The path of the process's private channel directory
/// (`%TEMP%/pir-pool-<pid>-<random>`), once per [`ShellPool`]. The *path* is
/// stable; whether it currently holds a valid private directory is
/// re-validated on every use (see [`channel_directory`]), because the last
/// drop of a call removes it and an attacker may occupy it in that window.
///
/// The directory state is per pool rather than a process static: production
/// has exactly one global pool (`global()`), so the effective layout is
/// unchanged, while a test pool can point at its own path without racing
/// every other pool in the same test binary.
struct ChannelDir {
    path: PathBuf,
    /// Serializes "ensure directory exists + create channel" against "unlink
    /// channel + maybe remove the directory", so the pool's last channel can
    /// remove the directory without racing a concurrent create.
    lifecycle: Mutex<()>,
    /// Channel files currently open for this directory.
    live: std::sync::atomic::AtomicUsize,
    /// How many times this directory's path was refused as suspicious. Only
    /// observability (one warn is logged; see
    /// [`log_channel_directory_rejection`]), plus the integration test's
    /// evidence that a suspicious path is rejected rather than adopted.
    rejections: std::sync::atomic::AtomicU64,
}

impl ChannelDir {
    fn process_default() -> Self {
        Self::at(std::env::temp_dir().join(format!(
            "{CHANNEL_DIR_PREFIX}{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        )))
    }

    fn at(path: PathBuf) -> Self {
        Self {
            path,
            lifecycle: Mutex::new(()),
            live: std::sync::atomic::AtomicUsize::new(0),
            rejections: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

/// The one global pool's channel directory (lazily built with the process
/// default path); separate pools (tests) own their own.
static GLOBAL_CHANNEL_DIR: OnceLock<Arc<ChannelDir>> = OnceLock::new();

fn global_channel_dir() -> Arc<ChannelDir> {
    Arc::clone(GLOBAL_CHANNEL_DIR.get_or_init(|| Arc::new(ChannelDir::process_default())))
}

/// Test seam: create a directory with the *product* private security shape at
/// `path` (Unix `0o300`, Windows protected DACL). `false` when the platform
/// API fails (e.g. the path exists).
#[doc(hidden)]
pub fn create_channel_probe_for_tests(path: &Path) -> bool {
    platform_dir::create_private(path).is_ok()
}

/// Test seam: the same validation [`channel_directory`] applies before using
/// (and before every drain of) a channel directory.
#[doc(hidden)]
pub fn validate_channel_directory_for_tests(path: &Path) -> bool {
    platform_dir::validate_private(path)
}

/// The private directory every channel file lives in:
/// `%TEMP%/pir-pool-<pid>-<random>`.
///
/// CL16 proved that channel files living directly in `%TEMP%` are
/// **enumerable**: a background writer left by call N could glob
/// `$TEMP/pir-pool-out-*` and append into call N+1's live channel file —
/// cross-invocation (and cross-process) output spoofing. The directory closes
/// the enumeration, not the name entropy, and is created once per pool:
///
/// * **Unix**: mode `0o300` (write + execute, no read). `readdir` fails for
///   every unprivileged user, including this process; the pool addresses each
///   channel by its exact known path and never lists the directory, and both
///   `unlink` and the shell's `>` open on an existing file need only
///   write+execute on the directory plus the file's own 0600.
/// * **Windows**: an explicit, `SE_DACL_PROTECTED` DACL with a deny-list ACE
///   for `Everyone` (list only, not inherited to children) and an inheritable
///   allow ACE for the current user. Traverse, add-file, write-data and
///   delete stay granted; listing through `DIR`/`FindFirstFile` is denied to
///   the shell. Channel files are created by the pool before the shell starts
///   and passed to the redirect by full path.
///
/// `None` (creation failed, or an existing path failed validation) makes
/// [`OutputChannel::create`] return `None`, which falls back to the
/// pre-channel behavior — the member's stdout — rather than ever putting a
/// channel file back into the shared temp directory.
///
/// The directory is created once per pool; [`OutputChannel`]'s drop removes
/// it with the pool's last channel (guarded by [`ChannelDir::lifecycle`]), and
/// [`OutputChannel::create`] recreates it on the next call.
///
/// **Never adopts an existing path.** A path that already exists is not used
/// on the strength of `exists()`: it may be a leftover from the last drop's
/// window, a directory (or a junction) pre-created by a same-user attacker who
/// learned the name, or a real channel directory whose ACL was subverted. Each
/// such path is validated by [`platform_dir::validate_private`] — a real
/// directory (no reparse point on Windows), owned by the current user, with
/// the expected protected DACL (deny `Everyone` list, before any allow ACE) or
/// Unix mode `0o300` — and a failure rejects the whole pool channel for this
/// call: the path is neither deleted nor reused, one diagnostic is logged, and
/// the caller falls back to member-stdout capture.
fn channel_directory(dir: &ChannelDir) -> Option<PathBuf> {
    let path = dir.path.clone();
    // Re-run the full validation on every use, not just the first: a previous
    // call's drop may have removed the directory, and a path that appears in
    // that window (the last drop's removal and this re-create) is rejected
    // exactly like a pre-existing path — never adopted. Creation is
    // "fail if it appeared meanwhile" (`CreateDirectoryW` /
    // `create_dir` are non-recursive and fail on an existing path), so an
    // attacker cannot slip a directory in between a probe and the create.
    ensure_channel_directory(dir, &path).then_some(path)
}

/// Ensure `path` is the validated private directory for `dir`: create it when
/// absent (atomic "fail if it appeared meanwhile" creation), validate it when
/// it exists, and never adopt a path that does not prove it is ours.
fn ensure_channel_directory(dir: &ChannelDir, path: &Path) -> bool {
    match platform_dir::create_private(path) {
        Ok(()) => {
            // Freshly created by us with the explicit security descriptor /
            // mode; validate anyway so create and adopt share one predicate
            // (and a silently degraded create still fails closed).
            if platform_dir::validate_private(path) {
                return true;
            }
            reject_channel_directory(dir, path, "newly created directory failed validation");
            false
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Never trust the name: this may be our own directory recreated
            // after a drop, or an attacker's path that appeared first.
            if platform_dir::validate_private(path) {
                return true;
            }
            reject_channel_directory(
                dir,
                path,
                "existing path is not the private channel directory (type/owner/mode/ACL)",
            );
            false
        }
        Err(error) => {
            reject_channel_directory(dir, path, "private channel directory could not be created");
            tracing::debug!(
                target: "pir::shell_pool",
                path = %path.display(),
                %error,
                "shell pool: private channel directory creation failed; \
                 falling back to member stdout capture"
            );
            false
        }
    }
}

/// Record and (once per process) log a refusal. The path is deliberately
/// neither deleted nor reused: a suspicious directory may belong to an
/// attacker, and deleting it would be an unrequested side effect; leaving it
/// means the next call re-validates and refuses again.
fn reject_channel_directory(dir: &ChannelDir, path: &Path, reason: &'static str) {
    dir.rejections
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    log_channel_directory_rejection(path, reason);
}

/// One diagnostic per process for a rejected channel directory: the fallback
/// is per call, but a log flood from a persistent condition is not useful.
fn log_channel_directory_rejection(path: &Path, reason: &'static str) {
    static LOGGED: OnceLock<()> = OnceLock::new();
    LOGGED.get_or_init(|| {
        tracing::warn!(
            target: "pir::shell_pool",
            path = %path.display(),
            reason,
            "shell pool: refusing a suspicious private channel directory; \
             not using or deleting it, falling back to member stdout capture"
        );
    });
}

/// Validate (do not create) a channel directory before a drain. `false` means
/// the directory no longer proves it is the private channel directory
/// (removed, replaced, re-ACLed, re-moded): the caller must fail closed
/// instead of trusting the captured bytes.
fn channel_directory_is_intact(path: &Path) -> bool {
    platform_dir::validate_private(path)
}

// ---------------------------------------------------------------------------
// Private channel directory, per platform
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod platform_dir {
    //! Unix private directory: mode 0300 (write+execute, no read).

    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::path::Path;

    /// Create the directory with a mode that allows `unlink`/create and
    /// traversal but no listing (`0300`).
    pub(super) fn create_private(path: &Path) -> std::io::Result<()> {
        std::fs::DirBuilder::new().mode(0o300).create(path)?;
        // The mode above is subject to the umask; force it so a restrictive
        // umask can never leave the directory readable (or hidden from us).
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o300))
    }

    /// Whether `path` is exactly the private channel directory this process
    /// would have created: a real directory (not a symlink — `symlink_metadata`
    /// does not follow it), owned by the effective user, with mode `0o300`.
    ///
    /// Used before adopting an existing path and before every drain. A same-uid
    /// owner can still `chmod` it (see the module docs' residual-risk note);
    /// this check makes that tampering *detected* on the next call/drain.
    pub(super) fn validate_private(path: &Path) -> bool {
        // `geteuid` has no safe `std` wrapper; this is the function's small
        // unsafe boundary.
        #![allow(unsafe_code)]

        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return false;
        };
        if !metadata.is_dir() {
            return false;
        }
        if metadata.mode() & 0o777 != 0o300 {
            return false;
        }
        // SAFETY: `geteuid` takes no arguments and dereferences no memory; it
        // returns this process's effective uid, which is the value compared.
        metadata.uid() == unsafe { libc::geteuid() }
    }

    /// Restore owner read+execute so the age sweep's `remove_dir_all` can
    /// enumerate (and delete) an expired directory.
    pub(super) fn make_owner_removable(path: &Path) -> bool {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).is_ok()
    }
}

#[cfg(windows)]
mod platform_dir {
    //! Windows private directory: an explicit DACL that denies listing to
    //! `Everyone` while keeping traverse/add-file/write/delete for the
    //! current user.
    //!
    //! The deny-list ACE is evaluated before every allow ACE, so it binds the
    //! owner (this process) as well: the pool must never enumerate the
    //! directory, only address channels by full path. Two deliberate choices:
    //! * the DACL is `SE_DACL_PROTECTED`, so inheritable ACEs from the temp
    //!   directory (`Users`/`Authenticated Users`) are not merged in;
    //! * the allow ACE is inheritable, so channel files created inside get
    //!   current-user-only access without a second security descriptor.
    //!
    //! Any failure to build/apply the DACL fails directory creation, and the
    //! caller falls back to stdout capture instead of a shared-temp file.

    // The safe `std::fs` API cannot carry a security descriptor, so this
    // module is an `unsafe` boundary, exactly like `tools/spill.rs`.
    #![allow(unsafe_code)]

    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, PSID};
    use windows_sys::Win32::Security::{
        AddAccessAllowedAceEx, AddAccessDeniedAceEx, CreateWellKnownSid, EqualSid, GetAce,
        GetAclInformation, GetFileSecurityW, GetLengthSid, GetSecurityDescriptorControl,
        GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation, InitializeAcl,
        InitializeSecurityDescriptor, SetFileSecurityW, SetSecurityDescriptorControl,
        SetSecurityDescriptorDacl, TokenUser, WinWorldSid, ACCESS_ALLOWED_ACE, ACCESS_DENIED_ACE,
        ACE_FLAGS, ACL, ACL_REVISION, ACL_REVISION_DS, ACL_SIZE_INFORMATION, CONTAINER_INHERIT_ACE,
        DACL_SECURITY_INFORMATION, INHERITED_ACE, INHERIT_ONLY_ACE, NO_INHERITANCE,
        OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        SECURITY_DESCRIPTOR, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateDirectoryW, GetFileAttributesW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_LIST_DIRECTORY, INVALID_FILE_ATTRIBUTES,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// `SECURITY_MAX_SID_SIZE` from winnt.h (not exported by windows-sys
    /// 0.52); a SID never exceeds it.
    const MAX_SID_BYTES: usize = 68;

    /// One ACE to add: a SID, the access mask, whether it denies, and the
    /// inheritance flags. `NO_INHERITANCE` means "this object only"; the
    /// `OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE` combination means "this
    /// object and its children" — the directory itself needs an access grant
    /// too (`remove_dir`, create/unlink of children), not just the files
    /// inside it.
    struct Entry<'a> {
        sid: &'a Sid,
        mask: u32,
        deny: bool,
        flags: ACE_FLAGS,
    }

    pub(super) fn create_private(path: &Path) -> std::io::Result<()> {
        let user = Sid::current_user().ok_or_else(|| acl_error("current-user SID unavailable"))?;
        let world = Sid::world().ok_or_else(|| acl_error("Everyone SID unavailable"))?;
        let mut acl = Acl::new(&[
            // Deny listing to everyone (the owner included — the pool never
            // lists). The deny is not inherited, so channel files inside stay
            // fully usable.
            Entry {
                sid: &world,
                mask: FILE_LIST_DIRECTORY,
                deny: true,
                flags: NO_INHERITANCE,
            },
            // The owner keeps full access ON the directory (create/unlink,
            // delete) and the same access is inherited by channel files.
            Entry {
                sid: &user,
                mask: FILE_ALL_ACCESS,
                deny: false,
                flags: OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
            },
        ])
        .ok_or_else(|| acl_error("channel-directory ACL could not be built"))?;
        let descriptor = Descriptor::new(&mut acl, true)
            .ok_or_else(|| acl_error("channel-directory descriptor failed"))?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.pointer,
            bInheritHandle: 0,
        };
        let wide = wide(path);
        // SAFETY: `wide` is NUL-terminated and outlives the call; `attributes`
        // points at a live SECURITY_DESCRIPTOR whose DACL (`acl`) outlives the
        // call as well.
        if unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Whether `path` is a private channel directory this process created:
    ///
    /// * a real directory — `GetFileAttributesW` reports
    ///   `FILE_ATTRIBUTE_DIRECTORY` and **not** `FILE_ATTRIBUTE_REPARSE_POINT`
    ///   (a junction/symlink to somewhere listable is rejected outright);
    /// * owned by the current user's SID;
    /// * a `SE_DACL_PROTECTED` DACL whose **first** ACE is
    ///   `Everyone: DENY FILE_LIST_DIRECTORY` and which contains no
    ///   `INHERITED_ACE` (no ACE merged in from the temp directory later).
    ///
    /// Every step fails closed: an API error is `false`, never "assume fine".
    /// This is the predicate for adopting an existing path and for re-checking
    /// the directory before each drain; see [`super::channel_directory`].
    pub(super) fn validate_private(path: &Path) -> bool {
        // 1. Type + reparse-point check, without following a link.
        // SAFETY: the wide-string temporary is NUL-terminated and lives until
        // the end of this statement, so its pointer is valid for the call.
        let attributes = unsafe { GetFileAttributesW(wide(path).as_ptr()) };
        if attributes == INVALID_FILE_ATTRIBUTES {
            return false;
        }
        if attributes & FILE_ATTRIBUTE_DIRECTORY == 0
            || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return false;
        }
        // 2. Owner + DACL, in two calls (size query, then fetch).
        let requested = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let mut needed: u32 = 0;
        // SAFETY: a null descriptor with length 0 asks only for the required
        // size; the documented failure fills `needed`.
        unsafe {
            GetFileSecurityW(
                wide(path).as_ptr(),
                requested,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
        }
        if needed == 0 {
            return false;
        }
        let mut storage = vec![0u64; (needed as usize).div_ceil(std::mem::size_of::<u64>())];
        // SAFETY: the buffer is at least `needed` bytes and 8-byte aligned.
        if unsafe {
            GetFileSecurityW(
                wide(path).as_ptr(),
                requested,
                storage.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return false;
        }
        let descriptor: PSECURITY_DESCRIPTOR = storage.as_mut_ptr().cast();
        // SAFETY: `descriptor` points at the self-relative descriptor the
        // successful call just filled; every accessor below reads from it.
        let (owner, dacl, control) = unsafe {
            let mut owner: PSID = std::ptr::null_mut();
            let mut owner_defaulted: i32 = 0;
            if GetSecurityDescriptorOwner(descriptor, &mut owner, &mut owner_defaulted) == 0 {
                return false;
            }
            let mut present: i32 = 0;
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut dacl_defaulted: i32 = 0;
            if GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut dacl_defaulted)
                == 0
                || present == 0
                || dacl.is_null()
            {
                return false;
            }
            let mut control: u16 = 0;
            let mut revision: u32 = 0;
            if GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) == 0 {
                return false;
            }
            (owner, dacl, control)
        };
        if control & SE_DACL_PROTECTED == 0 {
            return false;
        }
        // 3. Owner is this user.
        let Some((user, world)) = expected_sids() else {
            return false;
        };
        // SAFETY: both pointers are valid SIDs (one from the descriptor, one
        // from the cached current token).
        if unsafe { EqualSid(owner, user.pointer()) } == 0 {
            return false;
        }
        // 4. The deny-listing ACE is first and nothing inherited is present.
        // SAFETY: `dacl` points into `storage`, which outlives this block.
        unsafe { acl_is_private(dacl, world) }
    }

    /// The expected ACE layout, in order: `Everyone: DENY FILE_LIST_DIRECTORY`
    /// before every allow ACE, no inherited ACEs, at least one allow ACE.
    ///
    /// # Safety
    ///
    /// `dacl` must point at a valid, initialized `ACL` (from a security
    /// descriptor the caller owns).
    /// # Safety
    ///
    /// `dacl` must point at a live, self-relative ACL whose ACEs stay valid
    /// for the duration of the call; `world` must be a valid SID.
    unsafe fn acl_is_private(dacl: *const ACL, world: &Sid) -> bool {
        let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
        if GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            windows_sys::Win32::Security::AclSizeInformation,
        ) == 0
        {
            return false;
        }
        let revision = u32::from((*dacl).AclRevision);
        if revision != ACL_REVISION && revision != ACL_REVISION_DS {
            return false;
        }
        let mut saw_allow = false;
        let mut saw_deny = false;
        for index in 0..info.AceCount {
            let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
            if GetAce(dacl, index, &mut ace) == 0 || ace.is_null() {
                return false;
            }
            let bytes = ace.cast::<u8>();
            // The ACE header is `{u8 type; u8 flags; u16 size}`; read it
            // unaligned rather than as a struct reference, because an
            // ACE of a shorter/different shape must not become a reference to
            // a larger type.
            let ace_type = std::ptr::read_unaligned(bytes);
            let ace_flags = std::ptr::read_unaligned(bytes.add(1));
            let ace_size = u32::from(std::ptr::read_unaligned(bytes.add(2).cast::<u16>()));
            if ace_flags & INHERITED_ACE as u8 != 0 {
                return false;
            }
            match ace_type {
                // `ACCESS_DENIED_ACE_TYPE`: header (4) + mask (4) + SID (≥ 8).
                1 => {
                    if ace_size < 16 {
                        return false;
                    }
                    let mask = std::ptr::read_unaligned(bytes.add(4).cast::<u32>());
                    let sid: PSID = bytes.add(8).cast();
                    // The deny must be the FIRST ACE (before any allow, so no
                    // earlier grant can win) and must bind Everyone.
                    if index != 0
                        || mask & FILE_LIST_DIRECTORY == 0
                        || ace_flags & INHERIT_ONLY_ACE as u8 != 0
                        || EqualSid(sid, world.pointer()) == 0
                    {
                        return false;
                    }
                    saw_deny = true;
                }
                // `ACCESS_ALLOWED_ACE_TYPE`.
                0 => saw_allow = true,
                // Object/callback/audit/label ACEs are never part of our DACL.
                _ => return false,
            }
        }
        // Both halves are required: a directory whose deny ACE was removed
        // (the same-user owner can do this) is no longer private even though
        // its allow ACE still lets this process write into it.
        saw_deny && saw_allow
    }

    /// The current user and `Everyone` SIDs, resolved once per process
    /// (`validate_private` runs on every channel drain, so the token lookup
    /// and well-known-SID construction must not repeat).
    fn expected_sids() -> Option<(&'static Sid, &'static Sid)> {
        static SIDS: OnceLock<Option<(Sid, Sid)>> = OnceLock::new();
        SIDS.get_or_init(|| Some((Sid::current_user()?, Sid::world()?)))
            .as_ref()
            .map(|(user, world)| (user, world))
    }

    /// A DACL-construction failure as an `io::Error` (never `AlreadyExists`,
    /// so the caller's concurrent-create tolerance cannot swallow it).
    fn acl_error(message: &str) -> std::io::Error {
        std::io::Error::other(message)
    }

    /// Rewrite the DACL to a plain owner-only allow shape, which also lifts
    /// the deny-list ACE: `remove_dir_all` must enumerate the directory.
    /// The owner always holds implicit WRITE_DAC, so this succeeds for
    /// expired directories left by a dead process of the same user.
    pub(super) fn make_owner_removable(path: &Path) -> bool {
        let Some(user) = Sid::current_user() else {
            return false;
        };
        let Some(mut acl) = Acl::new(&[Entry {
            sid: &user,
            mask: FILE_ALL_ACCESS,
            deny: false,
            flags: OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
        }]) else {
            return false;
        };
        let Some(descriptor) = Descriptor::new(&mut acl, false) else {
            return false;
        };
        let wide = wide(path);
        // SAFETY: `wide` is NUL-terminated; `descriptor` (and the ACL it
        // points at) outlive the call.
        unsafe {
            SetFileSecurityW(wide.as_ptr(), DACL_SECURITY_INFORMATION, descriptor.pointer) != 0
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// A SID in 8-byte-aligned storage (the ACL entry APIs require a
    /// 4-byte-aligned SID).
    struct Sid {
        storage: Vec<u64>,
        offset: usize,
        length: usize,
    }

    impl Sid {
        fn current_user() -> Option<Self> {
            // SAFETY: `GetCurrentProcess` returns a pseudo-handle that is
            // always valid and must not be closed.
            let process = unsafe { GetCurrentProcess() };
            let mut token: HANDLE = 0;
            // SAFETY: `token` is a live out-parameter; TOKEN_QUERY is the
            // minimal access the lookup needs.
            let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
            if opened == 0 {
                return None;
            }
            let sid = Self::from_token(token);
            // SAFETY: the handle was opened above and is still owned here.
            unsafe {
                CloseHandle(token);
            }
            sid
        }

        fn from_token(token: HANDLE) -> Option<Self> {
            let mut needed: u32 = 0;
            // SAFETY: a null buffer with length 0 asks only for the required
            // size; the documented failure still fills `needed`.
            unsafe {
                GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            }
            if needed == 0 {
                return None;
            }
            let mut storage = vec![0u64; (needed as usize).div_ceil(std::mem::size_of::<u64>())];
            // SAFETY: the buffer is at least `needed` bytes and 8-byte aligned.
            let filled = unsafe {
                GetTokenInformation(
                    token,
                    TokenUser,
                    storage.as_mut_ptr().cast(),
                    needed,
                    &mut needed,
                )
            };
            if filled == 0 {
                return None;
            }
            // SAFETY: the call succeeded, so the buffer starts with a
            // TOKEN_USER whose SID lives inside the same allocation.
            let user = unsafe { &*(storage.as_ptr().cast::<TOKEN_USER>()) };
            Self::from_sid_pointer(user.User.Sid, storage)
        }

        /// The `Everyone` (World) well-known SID.
        fn world() -> Option<Self> {
            let mut storage = vec![0u64; MAX_SID_BYTES.div_ceil(std::mem::size_of::<u64>())];
            let mut size = MAX_SID_BYTES as u32;
            // SAFETY: the buffer is `MAX_SID_BYTES` bytes and 8-byte aligned;
            // a NULL domain SID asks for the well-known SID itself.
            let created = unsafe {
                CreateWellKnownSid(
                    WinWorldSid,
                    std::ptr::null_mut(),
                    storage.as_mut_ptr().cast(),
                    &mut size,
                )
            };
            if created == 0 {
                return None;
            }
            let length = usize::try_from(size).ok()?;
            if length == 0 || length > storage.len() * std::mem::size_of::<u64>() {
                return None;
            }
            Some(Self {
                storage,
                offset: 0,
                length,
            })
        }

        fn from_sid_pointer(sid: PSID, storage: Vec<u64>) -> Option<Self> {
            // SAFETY: `sid` points into `storage` (a live token/well-known
            // buffer), so it is a valid SID.
            let length = unsafe { GetLengthSid(sid) } as usize;
            if length == 0 || length > storage.len() * std::mem::size_of::<u64>() {
                return None;
            }
            // SAFETY: the SID occupies `length` bytes inside `storage`.
            let offset = unsafe { (sid as *const u8).offset_from(storage.as_ptr().cast()) };
            let start = usize::try_from(offset).ok()?;
            if start + length > storage.len() * std::mem::size_of::<u64>() {
                return None;
            }
            Some(Self {
                storage,
                offset: start,
                length,
            })
        }

        fn pointer(&self) -> PSID {
            // The SID starts `offset` bytes into the buffer, after the
            // TOKEN_USER header, when one is present.
            self.storage.as_ptr().cast::<u8>().wrapping_add(self.offset) as PSID
        }
    }

    /// An `ACL` holding the given entries, in 8-byte-aligned storage.
    struct Acl {
        storage: Vec<u64>,
        pointer: *mut ACL,
        size: u32,
    }

    impl Acl {
        fn new(entries: &[Entry<'_>]) -> Option<Self> {
            // `ACL` header + one ACE per entry. `SidStart` (u32) overlaps the
            // first four bytes of the SID, hence the subtraction.
            let ace_overhead =
                std::mem::size_of::<ACCESS_ALLOWED_ACE>() - std::mem::size_of::<u32>();
            let deny_overhead =
                std::mem::size_of::<ACCESS_DENIED_ACE>() - std::mem::size_of::<u32>();
            let size = std::mem::size_of::<ACL>()
                + entries
                    .iter()
                    .map(|entry| {
                        let overhead = if entry.deny {
                            deny_overhead
                        } else {
                            ace_overhead
                        };
                        overhead + entry.sid.length
                    })
                    .sum::<usize>();
            let storage = vec![0u64; size.div_ceil(std::mem::size_of::<u64>())];
            let mut acl = Self {
                storage,
                pointer: std::ptr::null_mut(),
                size: u32::try_from(size).ok()?,
            };
            acl.pointer = acl.storage.as_mut_ptr().cast();
            // SAFETY: the buffer is 8-byte aligned and at least `size` bytes;
            // `ACL_REVISION` is the required revision for a plain file ACL.
            if unsafe { InitializeAcl(acl.pointer, acl.size, ACL_REVISION) } == 0 {
                return None;
            }
            for entry in entries {
                // `NO_INHERITANCE` = this object only; `OI|CI` = this object
                // and children (the ACE applies to the directory itself AND
                // is inherited by the channel files).
                let flags = entry.flags;
                // The derive ACEs copy the SID into the ACL, so the SID only
                // has to stay alive for the duration of this call.
                //
                // SAFETY: `acl.pointer` points into `acl.storage` (kept alive
                // by `acl`) and `size` reserves every entry; the SID pointer
                // is valid and 4-byte aligned.
                let added = unsafe {
                    if entry.deny {
                        AddAccessDeniedAceEx(
                            acl.pointer,
                            ACL_REVISION,
                            flags,
                            entry.mask,
                            entry.sid.pointer(),
                        )
                    } else {
                        AddAccessAllowedAceEx(
                            acl.pointer,
                            ACL_REVISION,
                            flags,
                            entry.mask,
                            entry.sid.pointer(),
                        )
                    }
                };
                if added == 0 {
                    return None;
                }
            }
            Some(acl)
        }
    }

    struct Descriptor {
        #[allow(
            dead_code,
            reason = "the box owns the memory the descriptor pointer refers to"
        )]
        storage: Box<SECURITY_DESCRIPTOR>,
        pointer: *mut core::ffi::c_void,
    }

    impl Descriptor {
        fn new(acl: &mut Acl, protected: bool) -> Option<Self> {
            // SAFETY: `SECURITY_DESCRIPTOR` is plain old data; all-zero is a
            // valid value and `InitializeSecurityDescriptor` initializes it
            // before any read.
            let mut storage: Box<SECURITY_DESCRIPTOR> = Box::new(unsafe { std::mem::zeroed() });
            let pointer: *mut core::ffi::c_void =
                (&mut *storage as *mut SECURITY_DESCRIPTOR).cast();
            // SAFETY: `storage` is a live, aligned SECURITY_DESCRIPTOR;
            // revision 1 is `SECURITY_DESCRIPTOR_REVISION`.
            if unsafe { InitializeSecurityDescriptor(pointer, 1) } == 0 {
                return None;
            }
            // SAFETY: `acl.pointer` points into `acl.storage`, which outlives
            // this descriptor (the caller keeps both alive together); the
            // descriptor only stores the pointer.
            if unsafe { SetSecurityDescriptorDacl(pointer, 1, acl.pointer, 0) } == 0 {
                return None;
            }
            if protected {
                // SAFETY: `storage` is a live, absolute SECURITY_DESCRIPTOR
                // whose DACL was just set; the control bits being changed
                // exist on it.
                if unsafe {
                    SetSecurityDescriptorControl(pointer, SE_DACL_PROTECTED, SE_DACL_PROTECTED)
                } == 0
                {
                    return None;
                }
            }
            Some(Self { storage, pointer })
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform_dir {
    //! Other platforms: the plain temp-directory ACL is the floor.

    use std::path::Path;

    pub(super) fn create_private(path: &Path) -> std::io::Result<()> {
        std::fs::create_dir(path)
    }

    /// No platform-specific protection exists to validate here; requiring the
    /// path to be a real (non-symlink) directory keeps the fail-closed shape
    /// without pretending this platform has a private mode.
    pub(super) fn validate_private(path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
    }

    pub(super) fn make_owner_removable(_path: &Path) -> bool {
        true
    }
}

/// The pid embedded in a `pir-pool-<pid>-<random>` directory name.
fn private_dir_pid(name: &str) -> Option<&str> {
    let rest = name.strip_prefix(CHANNEL_DIR_PREFIX)?;
    let (pid, random) = rest.split_once('-')?;
    if pid.is_empty() || random.is_empty() || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(pid)
}

/// One invocation's private output channel.
///
/// The member's stdout is a single control stream shared by every invocation,
/// so a background writer left by call N keeps writing to it while call N+1
/// runs and its bytes cannot be attributed after the fact. The per-call path
/// has no such problem because each call owns its pipes and closes them when
/// it finishes. This type reproduces that boundary inside the pooled member:
/// the composed script redirects the command's output into a fresh
/// per-invocation file, which the pool drains into the invocation's capture
/// and unlinks when the call ends. A background writer from call N therefore
/// writes into call N's (already unlinked) file; it can never reach call N+1's
/// capture.
///
/// The member's stdout stays reserved for the sentinel: the channel is what
/// closes the bash attribution hole completely (`( … ) &` background jobs
/// write to the inherited stdout, i.e. into the call's own file).
///
/// Confidentiality: the file holds command output, never the nonce. It is
/// created `create_new` with mode 0600 on Unix, under an unguessable name,
/// inside the process's [private channel directory](channel_directory) — which
/// a same-user process cannot list, so a background writer from an earlier
/// call cannot find *any* channel file to append into (CL16).
///
/// Integrity: the directory is validated when the channel is created and
/// again **before every drain** (see [`channel_directory_is_intact`]). If it
/// stops matching the private shape mid-call (a same-user owner lifted the
/// deny ACE / changed the mode, or the path was replaced), the channel is
/// marked [`compromised`](Self::compromised): no further bytes are read, and
/// the call fails closed as a terminal loss that reports only the bytes
/// captured before the tampering was seen. The file is never treated as an
/// authoritative capture after that point.
struct OutputChannel {
    /// The pool directory this channel belongs to (for the live count and the
    /// last-drop removal), shared with the pool.
    dir: Arc<ChannelDir>,
    path: std::path::PathBuf,
    /// The pool's read-only handle, opened before the shell opens the file
    /// for writing so both sides agree the file exists; the shell truncates
    /// it, which is harmless because nothing was written yet. Read-only (not
    /// read+write) so the shell's own write open never hits a Windows sharing
    /// violation. `Option` so [`Drop`] can close it BEFORE unlinking: on
    /// Windows an unlinked file whose handle is still open keeps the directory
    /// entry in delete-pending state, so `remove_dir` of the now-empty
    /// private directory would fail.
    file: Option<std::fs::File>,
    /// Set once a drain finds the parent directory no longer validates.
    compromised: bool,
    /// Bytes already consumed from the channel (the per-file cap's basis).
    read_bytes: u64,
    /// Set once the channel file grew past [`OUTPUT_FILE_MAX_BYTES`].
    overflowed: bool,
}

impl OutputChannel {
    /// Create an empty channel for one invocation. `None` means the private
    /// channel directory or the file could not be created (or the directory
    /// failed validation); the caller then falls back to the pre-channel
    /// behavior (the member's stdout is the capture channel). The channel file
    /// is never placed in the shared temp directory: that layout was the CL16
    /// enumeration hole.
    fn create(dir: Arc<ChannelDir>) -> Option<Self> {
        sweep_output_files();
        // Creating the file and counting it happen under the lifecycle lock,
        // so a concurrent drop that removes the directory cannot interleave.
        let created = {
            let _lifecycle = dir
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let directory = channel_directory(&dir)?;
            let path = directory.join(format!(
                "{OUTPUT_FILE_PREFIX}{}-{}-{}",
                std::process::id(),
                OUTPUT_FILE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                uuid::Uuid::new_v4().simple()
            ));
            // Create the file first, then reopen it READ-ONLY for the pool.
            // The separate read handle matters on Windows: a handle that also
            // holds write access denies the shell's own `>` open (its share
            // mode does not admit another writer), while a read-only handle is
            // compatible with the shell's write open.
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(&path).ok()?;
            // Re-validate after the file exists: the directory was just
            // checked, but this narrows the window in which a same-user
            // attacker could have replaced it between the check and the open.
            // On failure remove only our own `create_new` file and fall back.
            if !channel_directory_is_intact(&directory) {
                let _ = std::fs::remove_file(&path);
                reject_channel_directory(
                    &dir,
                    &directory,
                    "private channel directory changed between creation and use",
                );
                return None;
            }
            let file = match std::fs::File::open(&path) {
                Ok(file) => file,
                Err(_) => {
                    let _ = std::fs::remove_file(&path);
                    return None;
                }
            };
            dir.live.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Self {
                dir: Arc::clone(&dir),
                path,
                file: Some(file),
                compromised: false,
                read_bytes: 0,
                overflowed: false,
            }
        };
        Some(created)
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this channel can no longer produce a trustworthy completion:
    /// a drain found the parent directory no longer private (its bytes are
    /// not trustworthy) or the file grew past [`OUTPUT_FILE_MAX_BYTES`] (the
    /// remainder is not read). Either way the caller fails the call closed as
    /// a terminal loss instead of reporting a completion.
    fn failed(&self) -> bool {
        self.compromised || self.overflowed
    }

    /// Append every byte written since the previous drain to `sink`.
    ///
    /// The parent directory is validated first. A failure marks the channel
    /// compromised and reads nothing: bytes that arrived after the directory
    /// lost its private shape cannot be attributed to the command.
    ///
    /// The channel file is capped at [`OUTPUT_FILE_MAX_BYTES`]: a larger file
    /// is never read into the capture and marks the channel overflowed (the
    /// caller terminates the member, so the writer stops and the drop removes
    /// the file).
    fn drain(&mut self, sink: &Capture) {
        if self.compromised || self.overflowed {
            return;
        }
        let Some(directory) = self.path.parent() else {
            self.compromised = true;
            return;
        };
        if !channel_directory_is_intact(directory) {
            self.compromised = true;
            reject_channel_directory(
                &self.dir,
                directory,
                "private channel directory was modified mid-call; discarding its output",
            );
            return;
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if file
            .metadata()
            .map(|metadata| metadata.len() > OUTPUT_FILE_MAX_BYTES)
            .unwrap_or(false)
        {
            self.overflowed = true;
            tracing::warn!(
                target: "pir::shell_pool",
                limit = OUTPUT_FILE_MAX_BYTES,
                "shell pool: per-invocation output channel exceeded its byte cap; ending the call"
            );
            return;
        }
        let mut buf = [0u8; 8192];
        loop {
            let remaining = OUTPUT_FILE_MAX_BYTES.saturating_sub(self.read_bytes);
            if remaining == 0 {
                self.overflowed = true;
                tracing::warn!(
                    target: "pir::shell_pool",
                    limit = OUTPUT_FILE_MAX_BYTES,
                    "shell pool: per-invocation output channel exceeded its byte cap; ending the call"
                );
                return;
            }
            let want = buf.len().min(remaining as usize);
            match file.read(&mut buf[..want]) {
                Ok(0) => break,
                Ok(count) => {
                    self.read_bytes = self.read_bytes.saturating_add(count as u64);
                    append_to(sink, &buf[..count]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    }
}

impl Drop for OutputChannel {
    fn drop(&mut self) {
        // Close the read handle FIRST: on Windows a delete of an open file
        // only marks it delete-pending, so the directory would not become
        // removable until the handle closed (and a handful of tests create a
        // new channel immediately, keeping it open past this drop).
        drop(self.file.take());
        // A directory that no longer validates is not ours to remove (it may
        // have been replaced by, or re-ACLed into, an attacker's path); the
        // channel file's unlink stays a no-op cleanup by exact name inside
        // whatever occupies it. Treating it as "nothing left to clean" also
        // keeps the retry loop below from spinning on a path that will never
        // become removable.
        let directory_intact = self.path.parent().is_some_and(channel_directory_is_intact);
        let (file_gone, directory_gone, last) = {
            let _lifecycle = self
                .dir
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let file_gone = remove_channel_file(&self.path);
            let live = self
                .dir
                .live
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            let last = live <= 1;
            // Process exit (or the last call in it): drop the now-empty
            // directory so it does not outlive the process.
            let directory_gone = if !directory_intact {
                true
            } else if last && file_gone {
                remove_empty_channel_directory(&self.path)
            } else {
                false
            };
            (file_gone, directory_gone, last)
        };
        // A directory that is no longer ours is left exactly as found (the
        // sweep only ever reaps directories whose pid is dead); the retry
        // loop would spin on a path we must not touch.
        if !directory_intact {
            return;
        }
        // Fully cleaned: the file is gone and either another live channel
        // still owns the directory or the directory itself is gone.
        if file_gone && (directory_gone || !last) {
            return;
        }
        // A writer that outlived the call (a `( … ) &` background job, or a
        // member whose kill is still propagating) may hold the file on
        // Windows without delete sharing; the delete then stays pending and
        // the directory cannot be removed either. Retry both briefly on the
        // pool's own runtime; a writer that lives longer is reaped by the age
        // sweep. On Unix the first unlink always succeeds.
        let path = self.path.clone();
        if let Some(runtime) = pool_runtime() {
            runtime.spawn(async move {
                for _ in 0..100 {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    let file_gone = remove_channel_file(&path);
                    // Only the last channel may remove the directory.
                    // `remove_dir` refuses a non-empty directory, so a
                    // concurrent create is never harmed.
                    let directory_gone = last && file_gone && remove_empty_channel_directory(&path);
                    if file_gone && (directory_gone || !last) {
                        break;
                    }
                }
            });
        }
    }
}

/// Remove the channel's (now empty) private directory. `true` when the
/// directory is gone, including when it never existed or another call
/// already removed it.
///
/// The directory is removed only while it still validates as the private
/// channel directory: if a same-user attacker replaced or re-ACLed it
/// mid-call, drop must not delete *their* path (nor ours through a link).
fn remove_empty_channel_directory(channel: &Path) -> bool {
    let Some(directory) = channel.parent() else {
        return true;
    };
    if !channel_directory_is_intact(directory) {
        return false;
    }
    match std::fs::remove_dir(directory) {
        Ok(()) => true,
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Unlink one channel file by its known path (never by listing — the private
/// directory cannot be listed). `true` when the file is gone.
fn remove_channel_file(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Remove channel files/directories that outlived their call (see
/// [`OutputChannel::drop`]). Throttled to [`OUTPUT_FILE_SWEEP_INTERVAL`]; best
/// effort by design — a temp directory that cannot be listed or an entry that
/// cannot be removed never fails a run.
///
/// Two layouts are handled:
/// * legacy flat `pir-pool-out-*` files from before the private-directory
///   change (removed once older than [`OUTPUT_FILE_MAX_AGE`], so upgrades do
///   not leave them forever);
/// * `pir-pool-<pid>-<random>` private directories. The sweep lists the
///   PARENT (the temp root) to find them; it never lists a private directory's
///   contents. A directory whose owning pid is dead is reaped right away (no
///   channel inside it can be live and a timed-out member always leaves one);
///   one whose owner is alive is left alone. Reaping makes the directory
///   owner-removable first (Unix restores `0700`, Windows rewrites the DACL to
///   a plain owner allow) and then deletes it with `remove_dir_all`; when that
///   is refused the directory is left in place and the refusal is logged once
///   per process.
fn sweep_output_files() {
    let throttle = LAST_OUTPUT_SWEEP.get_or_init(|| Mutex::new(None));
    let Ok(mut last) = throttle.lock() else {
        return;
    };
    if last
        .as_ref()
        .is_some_and(|at| at.elapsed() < OUTPUT_FILE_SWEEP_INTERVAL)
    {
        return;
    }
    *last = Some(std::time::Instant::now());
    drop(last);
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let own_pid = std::process::id().to_string();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let expired = |metadata: &std::fs::Metadata| {
            metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > OUTPUT_FILE_MAX_AGE)
        };
        // Legacy flat channel file (pre-private-directory layout).
        if name.starts_with(OUTPUT_FILE_PREFIX) {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_file() && expired(&metadata) {
                let _ = std::fs::remove_file(entry.path());
            }
            continue;
        }
        // Private channel directory: `pir-pool-<pid>-<random>`.
        let Some(pid) = private_dir_pid(name) else {
            continue;
        };
        if pid == own_pid {
            continue; // this process is live by definition; never touch its dir
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        // A LIVE owner's directory is never touched, however old (a >24 h
        // process may still be writing). A DEAD owner's directory is reaped
        // immediately — the pid is gone, no channel in it can be live, and
        // waiting 24 h would just leak it (a timed-out member always leaves
        // one). The age cap only bounds the cases where liveness cannot be
        // determined: a recycled pid keeps it one more cycle, and the
        // *legacy flat files* above still use the plain age rule.
        if process_is_alive(pid) {
            continue;
        }
        let path = entry.path();
        if !platform_dir::make_owner_removable(&path) {
            log_sweep_refusal(&path);
            continue;
        }
        if let Err(error) = std::fs::remove_dir_all(&path) {
            tracing::debug!(
                target: "pir::shell_pool",
                path = %path.display(),
                %error,
                "shell pool: orphaned channel directory could not be removed"
            );
            log_sweep_refusal(&path);
        }
    }
}

/// Whether `pid` still names a live process. Conservative on every unknown:
/// an unreadable process is reported alive, so the sweep never reaps a
/// directory that might still belong to a running pool.
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    // `kill(pid, 0)` is the POSIX liveness probe and has no safe wrapper in
    // std; this is the function's small unsafe boundary.
    #![allow(unsafe_code)]

    let Ok(pid) = i32::try_from(pid) else {
        return true;
    };
    // SAFETY: signal 0 performs only the existence/permission check and
    // delivers nothing; no memory is involved.
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    // `EPERM` means the process exists but belongs to another user.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    // The safe `std::fs`/`std` surface has no process-liveness probe; raw
    // Win32 calls are this function's small unsafe boundary.
    #![allow(unsafe_code)]

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, WAIT_OBJECT_0};
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: the pid is a plain identifier; the returned handle is checked
    // and closed below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
    if handle == 0 {
        // Access denied means the process exists at a higher integrity level.
        return std::io::Error::last_os_error().raw_os_error() == Some(ERROR_ACCESS_DENIED as i32);
    }
    // SAFETY: `handle` was just opened and is still owned here.
    let state = unsafe { WaitForSingleObject(handle, 0) };
    // SAFETY: the handle was opened above; nothing else holds it.
    unsafe {
        CloseHandle(handle);
    }
    state != WAIT_OBJECT_0
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

/// Log a denied sweep deletion once per process (see [`sweep_output_files`]).
fn log_sweep_refusal(path: &Path) {
    static LOGGED: OnceLock<()> = OnceLock::new();
    LOGGED.get_or_init(|| {
        tracing::warn!(
            target: "pir::shell_pool",
            path = %path.display(),
            "shell pool: permissions denied reaping an expired channel directory; leaving it in place"
        );
    });
}

// ---------------------------------------------------------------------------
// Sentinel scanning
// ---------------------------------------------------------------------------

/// Locate `RS rc ':' token RS` in the buffered stdout stream. Returns the
/// byte range `[start, end)` covering the sentinel plus the parsed rc. `None`
/// when absent — including when a possible partial sentinel still needs more
/// bytes (the scanner holds them back).
fn find_sentinel(buf: &[u8], token: &str) -> Option<(usize, usize, i64)> {
    let token_bytes = token.as_bytes();
    let mut from = 0;
    while let Some(rel) = buf[from..].iter().position(|&b| b == SENTINEL_RS) {
        let start = from + rel;
        let digits_start = start + 1;
        let mut i = digits_start;
        while i < buf.len() && buf[i].is_ascii_digit() && i - digits_start <= 10 {
            i += 1;
        }
        let digit_len = i - digits_start;
        if digit_len == 0 || digit_len > 10 {
            from = start + 1;
            continue;
        }
        let Some(&b':') = buf.get(i) else {
            // Either more digits are still in flight (≤10 kept back by the
            // tail buffer) or this is not a sentinel: wait for more bytes
            // when the colon could still arrive, otherwise skip.
            if digit_len == 10 || i < buf.len() {
                from = start + 1;
                continue;
            }
            return None;
        };
        i += 1;
        let token_end = i + token_bytes.len();
        match buf.get(token_end) {
            // Closing RS not arrived yet: hold and wait.
            None => return None,
            Some(&SENTINEL_RS) => {}
            Some(_) => {
                from = start + 1;
                continue;
            }
        }
        if &buf[i..token_end] != token_bytes {
            from = start + 1;
            continue;
        }
        let end = token_end + 1;
        let rc: i64 = std::str::from_utf8(&buf[digits_start..digits_start + digit_len])
            .ok()?
            .parse()
            .ok()?;
        return Some((start, end, rc));
    }
    None
}

fn append_to(sink: &Capture, chunk: &[u8]) {
    if chunk.is_empty() {
        return;
    }
    let mut guard = sink
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.append(chunk);
}

type Capture = Arc<Mutex<ShellCapture>>;

enum Signal {
    Done(i64),
    Lost,
}

/// The in-flight command's stdout scanner: bytes before the sentinel feed the
/// capture when the stream is an output channel (see `capture_stdout`), the
/// sentinel is consumed, and later bytes follow the same rule. With a
/// per-invocation [`OutputChannel`] the bash member's stdout carries only the
/// sentinel, so non-sentinel bytes there are stragglers and are dropped.
struct ActiveScan {
    sink: Capture,
    token: String,
    /// Whether stdout bytes are command output. With a per-invocation
    /// [`OutputChannel`] the bash member's stdout is a pure control stream
    /// (only the sentinel belongs there), so everything else is a straggler
    /// from an earlier call and is dropped. Without a channel (file creation
    /// failed) stdout is the capture stream, exactly as before the channel.
    capture_stdout: bool,
    pending: Vec<u8>,
    rc: Option<i64>,
    done: Option<oneshot::Sender<Signal>>,
    /// Last stdout data arrival; drives the stale-tail flush (small outputs
    /// would otherwise never exceed the sentinel holdback window and stream
    /// nothing until the command completes).
    last_data: std::time::Instant,
}

impl ActiveScan {
    fn new(
        sink: Capture,
        token: String,
        capture_stdout: bool,
        done: oneshot::Sender<Signal>,
    ) -> Self {
        Self {
            sink,
            token,
            capture_stdout,
            pending: Vec::new(),
            rc: None,
            done: Some(done),
            last_data: std::time::Instant::now(),
        }
    }

    /// Append command output to the capture — unless this stream is the
    /// control channel only (see `capture_stdout`).
    fn emit(&self, bytes: &[u8]) {
        if self.capture_stdout {
            append_to(&self.sink, bytes);
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        self.last_data = std::time::Instant::now();
        if self.rc.is_some() {
            self.emit(chunk);
            return;
        }
        self.pending.extend_from_slice(chunk);
        match find_sentinel(&self.pending, &self.token) {
            Some((start, end, rc)) => {
                let pre: Vec<u8> = self.pending.drain(..start).collect();
                self.emit(&pre);
                self.pending.drain(..end - start);
                self.rc = Some(rc);
                // Straggler bytes after the sentinel belong to the command's
                // output stream (background writers), like upstream's pipe.
                let rest: Vec<u8> = std::mem::take(&mut self.pending);
                self.emit(&rest);
                if let Some(done) = self.done.take() {
                    let _ = done.send(Signal::Done(rc));
                }
            }
            None => {
                let keep = SENTINEL_MAX_LEN - 1;
                if self.pending.len() > keep {
                    let cut = self.pending.len() - keep;
                    let flush: Vec<u8> = self.pending.drain(..cut).collect();
                    self.emit(&flush);
                }
            }
        }
    }

    /// Flush held-back bytes that cannot be part of a sentinel when no new
    /// data has arrived for [`STALE_FLUSH_INTERVAL`]: without this, output
    /// smaller than the sentinel holdback window never streams mid-run.
    /// Bytes from the last `�` onward stay held (a partial sentinel may
    /// be forming there).
    fn flush_stale(&mut self, now: std::time::Instant) {
        if self.pending.is_empty() || self.rc.is_some() {
            return;
        }
        if now.duration_since(self.last_data) < STALE_FLUSH_INTERVAL {
            return;
        }
        let cut = self
            .pending
            .iter()
            .rposition(|&byte| byte == 0x1e)
            .unwrap_or(self.pending.len());
        if cut > 0 {
            let flushed: Vec<u8> = self.pending.drain(..cut).collect();
            self.emit(&flushed);
        }
    }

    fn flush_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let flushed: Vec<u8> = std::mem::take(&mut self.pending);
        self.emit(&flushed);
    }
}

/// Routes pipe bytes for one member: the active command's scanner on stdout,
/// a straight capture append on stderr (the stderr stream never carries the
/// sentinel).
struct Gate {
    active: Mutex<Option<ActiveScan>>,
}

impl Gate {
    fn feed_stdout(&self, chunk: &[u8]) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(scan) = active.as_mut() {
            scan.feed(chunk);
        }
        // Between commands stdout has no call to attribute to (upstream would
        // have attached stragglers to the finished call's pipes); dropped.
    }

    /// Stale-tail flush for the active scanner (see
    /// [`ActiveScan::flush_stale`]); a no-op between commands.
    fn flush_stale(&self, now: std::time::Instant) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(scan) = active.as_mut() {
            scan.flush_stale(now);
        }
    }

    fn feed_stderr(&self, chunk: &[u8]) {
        let sink = {
            let active = self
                .active
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            active.as_ref().map(|scan| Arc::clone(&scan.sink))
        };
        if let Some(sink) = sink {
            append_to(&sink, chunk);
        }
    }

    fn on_stdout_eof(&self) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(scan) = active.as_mut() {
            if scan.rc.is_none() {
                scan.flush_pending();
                if let Some(done) = scan.done.take() {
                    let _ = done.send(Signal::Lost);
                }
            }
        }
    }

    fn flush_pending(&self) {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(scan) = active.as_mut() {
            if scan.rc.is_none() {
                scan.flush_pending();
            }
        }
    }

    fn clear(&self) {
        *self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

// ---------------------------------------------------------------------------
// Member lifecycle
// ---------------------------------------------------------------------------

/// One spawned persistent shell member — the product of the injectable
/// [`MemberSpawn`] seam. Construct with [`SpawnedMember::from_child`].
pub struct SpawnedMember {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    tree_kill: ProcessTreeKill,
    /// An opaque member tag for logs and diagnostics. It is deliberately NOT
    /// a sentinel secret: the completion nonce is generated per invocation
    /// (see [`compose_bash_script`]), so nothing shared across commands can be
    /// used to forge a completion.
    token: String,
    /// Whether the child is the embedded shell (`pir __brush`), which changes
    /// how the command script spells its null device on Windows.
    embedded: bool,
}

impl SpawnedMember {
    /// Attach the process-tree kill handle to a freshly spawned persistent
    /// shell child. `token` is the member's diagnostic tag (not a sentinel
    /// secret — see the field docs). The job carries kill-on-close: a pooled
    /// member is pool plumbing that must die with the process (per-call spawns
    /// keep the leave-backgrounded semantics via `ProcessTreeKill::attach`
    /// instead).
    pub fn from_child(
        child: tokio::process::Child,
        stdin: tokio::process::ChildStdin,
        token: String,
    ) -> Self {
        Self::from_child_with_embedded(child, stdin, token, false)
    }

    /// [`SpawnedMember::from_child`] for the embedded shell member: the flag
    /// selects the null-device spelling its command scripts use.
    pub fn from_child_with_embedded(
        child: tokio::process::Child,
        stdin: tokio::process::ChildStdin,
        token: String,
        embedded: bool,
    ) -> Self {
        let tree_kill = ProcessTreeKill::attach_for_pool(&child);
        Self {
            child,
            stdin,
            tree_kill,
            token,
            embedded,
        }
    }
}

/// The member-spawn seam: production spawns a real persistent shell; tests
/// inject a counting wrapper to observe spawn counts and failure modes.
pub trait MemberSpawn: Send + Sync + 'static {
    fn spawn(
        &self,
        protocol: ShellProtocol,
        program: &str,
        cwd: &Path,
    ) -> Result<SpawnedMember, String>;
}

struct RealSpawner;

/// The program (and `PATH` fixup) a member is actually spawned with: the
/// shared [`shell_spawn_target`] rules, with the pool's debug log.
///
/// A Git-for-Windows `bin\bash.exe` is a launcher/shim, not the MSYS bash: a
/// member spawned through it is a non-MSYS process, which was measured to
/// defeat every tree-kill mechanism (no MSYS identity for the subtree sweep,
/// descendants escaping the Job Object, stale Windows PPIDs for
/// `taskkill /T`) — an aborted or timed-out command then keeps running. The
/// member is therefore spawned with the real MSYS bash. Every other shell is
/// used verbatim.
fn member_spawn_target(program: &str) -> (String, Option<String>) {
    let target = shell_spawn_target(program);
    if target.1.is_some() {
        tracing::debug!(
            target: "pir::shell_pool",
            program,
            real = %target.0,
            "shell pool: spawning the real MSYS bash instead of the launcher shim"
        );
    }
    target
}

/// Resolve the program a bash member is actually spawned with. `resolve_shell_config`
/// already prefers an installed Git Bash (it carries coreutils/PATH); when it
/// fell back to the embedded shell (`pir __brush`), the pooled member must use
/// that same embedded program. `shellPath` always wins. The boolean is `true`
/// when the embedded program was chosen (the spawn arguments differ:
/// `__brush`).
fn resolve_member_program(program: &str) -> (String, bool) {
    #[cfg(feature = "brush")]
    {
        let prefs = crate::config::settings::current_shell_prefs();
        if prefs.shell_path.is_none() {
            if let Some(embedded) = crate::shell_embed::embedded_shell_program() {
                if program == embedded {
                    tracing::debug!(
                        target: "pir::shell_pool",
                        embedded = %embedded,
                        "shell pool: using the embedded brush shell for the bash member"
                    );
                    return (embedded, true);
                }
            }
        }
    }
    (program.to_owned(), false)
}

impl MemberSpawn for RealSpawner {
    fn spawn(
        &self,
        _protocol: ShellProtocol,
        program: &str,
        _cwd: &Path,
    ) -> Result<SpawnedMember, String> {
        let token = uuid::Uuid::new_v4().simple().to_string();
        let (member_program, embedded) = resolve_member_program(program);
        let (spawn_program, spawn_path) = if embedded {
            // The embedded program is this very executable: the MSYS shim
            // rules do not apply.
            (member_program, None)
        } else {
            member_spawn_target(&member_program)
        };
        let mut spawn = tokio::process::Command::new(&spawn_program);
        // `PIR_BIN` + the binary's directory prepended to `PATH` (the same
        // rule the per-call spawn applies), so `pir grep|sed|python …` works
        // inside a pooled command. An MSYS shim PATH override is the base
        // that gets the prefix, never something we replace.
        super::bash::apply_pir_bin_env(&mut spawn, spawn_path.as_deref());
        spawn
            .args(member_args_for(embedded))
            // Spawn the member in a neutral directory, NOT the first call's
            // cwd: the member outlives the call, and on Windows a directory
            // that is any live process's current directory cannot be deleted
            // or renamed. Upstream never pins one either (its fresh shells
            // exit right after the call); every command still runs at the
            // call's cwd via the composed script's cd/Set-Location.
            .current_dir(std::env::temp_dir())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        apply_tree_kill_spawn_attributes(&mut spawn);
        let mut child = spawn.spawn().map_err(|error| error.to_string())?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "stdin pipe unavailable".to_owned())?;
        if child.stdout.is_none() || child.stderr.is_none() {
            return Err("output pipe unavailable".to_owned());
        }
        Ok(SpawnedMember::from_child_with_embedded(
            child, stdin, token, embedded,
        ))
    }
}

struct PoolMember {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    tree_kill: ProcessTreeKill,
    /// The member runs the embedded shell (see [`SpawnedMember`]).
    embedded: bool,
    gate: Arc<Gate>,
    /// Reader tasks; they end at pipe EOF (i.e. when the member dies).
    _readers: (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>),
}

impl PoolMember {
    fn new(spawned: SpawnedMember) -> Self {
        let SpawnedMember {
            mut child,
            stdin,
            tree_kill,
            // The member tag is a diagnostic label only; no sentinel secret
            // lives on the member (each invocation generates its own).
            token: member_tag,
            embedded,
        } = spawned;
        tracing::debug!(
            target: "pir::shell_pool",
            member = %member_tag,
            pid = child.id().unwrap_or_default(),
            "shell pool: member started"
        );
        let gate = Arc::new(Gate {
            active: Mutex::new(None),
        });
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let gate_out = Arc::clone(&gate);
        let out_task = tokio::spawn(async move {
            if let Some(mut pipe) = stdout {
                let mut buf = [0u8; 8192];
                loop {
                    match pipe.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => gate_out.feed_stdout(&buf[..n]),
                    }
                }
            }
            gate_out.on_stdout_eof();
        });
        let gate_err = Arc::clone(&gate);
        let err_task = tokio::spawn(async move {
            if let Some(mut pipe) = stderr {
                let mut buf = [0u8; 8192];
                loop {
                    match pipe.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => gate_err.feed_stderr(&buf[..n]),
                    }
                }
            }
        });
        Self {
            child,
            stdin,
            tree_kill,
            embedded,
            gate,
            _readers: (out_task, err_task),
        }
    }

    /// Kill the member's whole descendant tree (upstream `killProcessTree`).
    /// The MSYS sweep runs BEFORE the root kill (the walk needs the root's
    /// process-table entry), and it is SYNCHRONOUS: the helper-process spawns
    /// block here while the member-slot guard is held (measured 150-300 ms
    /// idle, seconds under load). Callers must not assume this returns
    /// immediately; the block happens only on the abort/timeout path.
    fn terminate(&mut self) {
        if let Some(pid) = self.child.id() {
            self.tree_kill.terminate(pid);
        }
        let _ = self.child.start_kill();
    }
}

struct Entry {
    member: PoolMember,
    program: String,
}

/// The outcome of one pooled command.
pub(super) enum ProtocolOutcome {
    /// Sentinel received; `rc` is the command's exit code (the bash
    /// subshell's `$?`, signal deaths already mapped to 128+signal by bash).
    Completed { rc: i64, capture: Capture },
    /// The abort signal fired: member killed, next call respawns.
    Aborted { capture: Capture },
    /// The deadline expired: member killed, next call respawns.
    TimedOut { capture: Capture },
    /// The command script could not be written at all (zero bytes reached the
    /// member / stdin broken before the first byte): no command was
    /// dispatched, so the caller may safely fall back to the per-call spawn
    /// path. A *partial* write is never reported as `Lost` — see
    /// [`ProtocolOutcome::LostAfterWrite`].
    Lost,
    /// Sentinel protocol lost after bytes of the command script were handed to
    /// the member (or the member died mid-command): the command may have run
    /// (and produced side effects), so the caller must NOT replay it.
    /// `capture` holds whatever output arrived before the loss; the caller
    /// reports it as a terminated run.
    ///
    /// This is terminal for every shell kind. Only a zero-byte failed write
    /// (`Lost`) may fall back, so a lost pool member can never replay side
    /// effects.
    LostAfterWrite { capture: Capture },
}

enum PoolWait {
    Completed(i64),
    Lost,
    Aborted,
    Deadline,
    /// The CALLER's `execute` future was dropped while the pool task was
    /// still running (task abort / host teardown — e.g. the TUI aborting its
    /// prompt task without arming the session abort signal). The pool task
    /// detaches, so it must not keep holding the member slot until the
    /// command finishes naturally; it tears the member down like an abort.
    CallerGone,
}

/// Caller-life link for one pooled command: the sender lives in a local of
/// the `execute` future, the receiver rides into the spawned `execute_local`
/// task and is polled in its wait loop.
///
/// When the caller's future is dropped mid-await (no code in `execute` runs —
/// but every local of the future is dropped, so this guard's `Drop` fires),
/// the oneshot closes and `execute_local` treats the channel as caller-gone:
/// it kills the member, releases the slot, and exits instead of wedging the
/// slot for the natural lifetime of the command (potentially forever for
/// `sleep 100000`). On the normal path the sender drops after the pool task
/// already returned, so the closed channel is never observed.
struct CallerLink {
    _sender: oneshot::Sender<()>,
}

impl Drop for CallerLink {
    fn drop(&mut self) {
        // Dropping the oneshot sender is the signal itself: the receiver
        // resolves (or is already terminated). Nothing to send.
    }
}

/// The shell process pool: one persistent bash member, serialized behind its
/// mutex. (The PowerShell member was removed from the product — see the
/// module docs.)
pub struct ShellPool {
    spawner: Arc<dyn MemberSpawn>,
    bash: tokio::sync::Mutex<Option<Entry>>,
    /// This pool's private channel directory (see [`ChannelDir`]).
    channel_dir: Arc<ChannelDir>,
}

static GLOBAL_POOL: OnceLock<Arc<ShellPool>> = OnceLock::new();

/// The process-wide pool used by the shell tools. Arc so the execute hop can
/// move a handle onto the pool runtime (`self: &Arc<Self>` receivers).
pub(super) fn global() -> &'static Arc<ShellPool> {
    GLOBAL_POOL.get_or_init(|| {
        Arc::new(ShellPool::with_channel_dir(
            Arc::new(RealSpawner),
            global_channel_dir(),
        ))
    })
}

/// The pool's OWN IO runtime: every member (child process, pipes, reader
/// tasks) lives here for the whole process lifetime.
///
/// A pooled member's pipes are tokio IO objects bound to the runtime that
/// first registers them, and a tokio runtime drop waits on their in-flight
/// blocking reads. A global pool whose members are created lazily under
/// whatever runtime called first — a short-lived per-test tokio runtime,
/// or a host like the TUI that runs several single-thread runtimes — hangs
/// cross-runtime IO and teardown. One dedicated reactor removes the problem
/// structurally: member IO never leaves it, and the runtime never drops.
/// `None` (construction failure) disables the pool; callers fall back to the
/// per-call spawn path.
static POOL_RUNTIME: OnceLock<Option<tokio::runtime::Runtime>> = OnceLock::new();

fn pool_runtime() -> Option<&'static tokio::runtime::Runtime> {
    POOL_RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .thread_name("pir-shell-pool")
                .build()
                .ok()
        })
        .as_ref()
}

impl ShellPool {
    pub fn new(spawner: Arc<dyn MemberSpawn>) -> Self {
        Self::with_channel_dir(spawner, Arc::new(ChannelDir::process_default()))
    }

    fn with_channel_dir(spawner: Arc<dyn MemberSpawn>, dir: Arc<ChannelDir>) -> Self {
        Self {
            spawner,
            bash: tokio::sync::Mutex::new(None),
            channel_dir: dir,
        }
    }

    /// Test seam: a pool whose channel directory lives at `path`. Production
    /// pools always use their own process path (see [`channel_directory`]);
    /// this lets a security test pre-create a suspicious path (the CL16
    /// lifecycle attack) without touching the real channel directory shared
    /// by every other test in the same binary.
    #[doc(hidden)]
    pub fn with_channel_directory_for_tests(spawner: Arc<dyn MemberSpawn>, path: PathBuf) -> Self {
        Self::with_channel_dir(spawner, Arc::new(ChannelDir::at(path)))
    }

    /// Test seam: how many times this pool's channel directory was refused as
    /// suspicious (pre-existing path or mid-call tampering).
    #[doc(hidden)]
    pub fn channel_directory_rejections_for_tests(&self) -> u64 {
        self.channel_dir
            .rejections
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    fn slot(&self, protocol: ShellProtocol) -> &tokio::sync::Mutex<Option<Entry>> {
        match protocol {
            ShellProtocol::Bash => &self.bash,
        }
    }

    /// Idempotent get-or-start: ensure a live member for `protocol` and
    /// return its slot lock (held for the whole command — the serialization
    /// point the lazy path and the prewarm path both meet at). Respawns when
    /// no member exists or the shell program changed (settings `shellPath`).
    async fn ensure_member(
        &self,
        protocol: ShellProtocol,
        program: &str,
        cwd: &Path,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<Entry>>, String> {
        let mut slot = self.slot(protocol).lock().await;
        if slot.as_ref().is_some_and(|entry| entry.program == program) {
            return Ok(slot);
        }
        if let Some(mut old) = slot.take() {
            old.member.terminate();
        }
        let spawned = self
            .spawner
            .spawn(protocol, program, cwd)
            .map_err(|error| {
                tracing::warn!(
                    target: "pir::shell_pool",
                    error = %error,
                    "shell pool: member spawn failed; falling back to per-call spawn"
                );
                error
            })?;
        let member = PoolMember::new(spawned);
        *slot = Some(Entry {
            member,
            program: program.to_owned(),
        });
        Ok(slot)
    }

    /// Run one command through the pool. `None` means the pool is not usable
    /// for this call (runtime construction, spawn failure) and the caller
    /// must fall back to the per-call spawn path.
    ///
    /// The whole body runs on the pool's own runtime (see [`pool_runtime`]):
    /// the member's pipes belong to that reactor, so command I/O must never
    /// be polled from the caller's runtime.
    #[allow(
        clippy::too_many_arguments,
        reason = "one pooled command's full context: shell identity, isolation inputs, and the wait/abort/streaming knobs"
    )]
    pub(super) async fn execute(
        self: &Arc<Self>,
        protocol: ShellProtocol,
        program: &str,
        cwd: &Path,
        command: &str,
        session_env: Option<&ToolSessionEnv>,
        keep: usize,
        timeout_secs: Option<f64>,
        signal: Option<&AbortSignal>,
        on_update: Option<&ToolUpdateFn>,
    ) -> Option<ProtocolOutcome> {
        let runtime = pool_runtime()?;
        let this = Arc::clone(self);
        // Caller-life link: `_caller_link` is a local of THIS future, so a
        // caller dropping the future mid-await closes the channel the pool
        // task polls (see [`CallerLink`]) — the pool task then releases its
        // member slot instead of wedging it until the command ends.
        let (caller_tx, caller_rx) = oneshot::channel::<()>();
        let _caller_link = CallerLink { _sender: caller_tx };
        runtime
            .spawn(this.clone().execute_local(
                protocol,
                program.to_owned(),
                cwd.to_owned(),
                command.to_owned(),
                session_env.cloned(),
                keep,
                timeout_secs,
                signal.cloned(),
                on_update.cloned(),
                caller_rx,
            ))
            .await
            .ok()
            .flatten()
    }

    /// The execute body, polled on the pool runtime only. Owned `Arc` so the
    /// spawned future is `'static`.
    #[allow(
        clippy::too_many_arguments,
        reason = "one pooled command's full context: shell identity, isolation inputs, and the wait/abort/streaming knobs"
    )]
    async fn execute_local(
        self: Arc<Self>,
        protocol: ShellProtocol,
        program: String,
        cwd: std::path::PathBuf,
        command: String,
        session_env: Option<ToolSessionEnv>,
        keep: usize,
        timeout_secs: Option<f64>,
        signal: Option<AbortSignal>,
        on_update: Option<ToolUpdateFn>,
        mut caller_rx: oneshot::Receiver<()>,
    ) -> Option<ProtocolOutcome> {
        let program = program.as_str();
        let cwd = cwd.as_path();
        let command = command.as_str();
        let capture: Capture = Arc::new(Mutex::new(ShellCapture::new(keep)));
        // An abort that already fired must not touch the pool at all: no
        // member acquisition, no command write (the command would otherwise
        // start and be killed right after).
        if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
            return Some(ProtocolOutcome::Aborted {
                capture: Arc::clone(&capture),
            });
        }
        // Dispatch-liveness guard: acquiring the member must not block the
        // call invisibly. A long-running (or wedged) pooled command holds the
        // member mutex until it finishes, and the next call would otherwise
        // wait forever *before* emitting any update or start event — the
        // measured "accepted but never started" gap. On expiry the pool
        // reports "not usable" and the tool starts the command on a fresh
        // per-call shell (the pre-pool floor), logging why.
        let mut slot = match tokio::time::timeout(
            MEMBER_DISPATCH_TIMEOUT,
            self.ensure_member(protocol, program, cwd),
        )
        .await
        {
            Ok(Ok(slot)) => slot,
            Ok(Err(_)) => return None,
            Err(_) => {
                tracing::warn!(
                    target: "pir::shell_pool",
                    timeout_ms = MEMBER_DISPATCH_TIMEOUT.as_millis() as u64,
                    "shell pool: member not acquired within the dispatch-liveness window \
                     (a long-running command is holding it); falling back to a fresh shell"
                );
                return None;
            }
        };
        // Caller dropped while the member was being spawned:
        // release the freshly started (or about-to-be-reused) member right
        // away instead of running a command nobody waits for.
        if caller_rx.is_terminated() {
            if let Some(entry) = slot.as_mut() {
                entry.member.terminate();
            }
            *slot = None;
            tracing::debug!(
                target: "pir::shell_pool",
                "shell pool: caller dropped the pool call during member setup; member released"
            );
            return None;
        }
        // The abort may have fired while this call waited for the member
        // (up to MEMBER_DISPATCH_TIMEOUT): do not write the command at all.
        // The idle member is torn down like the running-abort path so the
        // "aborted => next call respawns" invariant holds.
        if signal.as_ref().is_some_and(|signal| signal.is_aborted()) {
            if let Some(entry) = slot.as_mut() {
                entry.member.terminate();
            }
            *slot = None;
            return Some(ProtocolOutcome::Aborted { capture });
        }
        let entry = slot.as_mut()?;
        let member = &mut entry.member;
        let gate = Arc::clone(&member.gate);
        // Fresh secrets for THIS invocation only (see `compose_bash_script`):
        // nothing derived from a previous command is reused, so a command that
        // learned one nonce (or its own random rc name) cannot forge the
        // completion of any other invocation.
        let nonce = fresh_nonce();
        let command_started_at = std::time::Instant::now();

        // Initial empty snapshot (upstream emits before exec; the per-call
        // path emits at the same point after its spawn succeeded).
        if let Some(emit) = on_update.as_ref() {
            let mut initial = serde_json::Map::new();
            initial.insert("content".to_owned(), serde_json::json!([]));
            emit(serde_json::Value::Object(initial));
        }

        // Install the per-invocation output channel and scanner, then write
        // the command script. The channel is what scopes output to THIS call:
        // a background writer left by an earlier call writes into that call's
        // file, never into this capture (see [`OutputChannel`]). Creation is
        // best effort; without it the call keeps the pre-channel behavior
        // (the member's stdout is the capture channel).
        let mut output = OutputChannel::create(Arc::clone(&self.channel_dir));
        let capture_stdout = output.is_none();
        let (done_tx, mut done_rx) = oneshot::channel();
        *gate
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(ActiveScan::new(
            Arc::clone(&capture),
            nonce.clone(),
            capture_stdout,
            done_tx,
        ));
        let script = compose_script(
            cwd,
            command,
            session_env.as_ref(),
            &nonce,
            member.embedded,
            output.as_ref().map(OutputChannel::path),
        );
        // Deliver the script. A failed `write_all` is NOT automatically
        // replay-safe: the write may have delivered a prefix of the script
        // before failing, and the prefix already contains the command (the
        // sentinel framing is the tail). Counting the delivered bytes makes
        // the distinction precise — zero bytes means nothing could have run
        // (safe fallback), any byte means the command may have started (a
        // terminal loss, never a replay). This closes the last replay window
        // of F14: the bash script is written through this same path, so a
        // partial write can never be reported as replay-safe.
        let mut written = 0usize;
        let mut write_failed = false;
        while written < script.len() {
            match member.stdin.write(&script.as_bytes()[written..]).await {
                Ok(0) => {
                    write_failed = true;
                    break;
                }
                Ok(count) => written += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    write_failed = true;
                    break;
                }
            }
        }
        if write_failed {
            if let Some(output) = output.as_mut() {
                output.drain(&capture);
            }
            gate.flush_pending();
            gate.clear();
            member.terminate();
            *slot = None;
            if written == 0 {
                tracing::warn!(
                    target: "pir::shell_pool",
                    "shell pool: member stdin write failed before any byte was delivered; falling back to per-call spawn"
                );
                return Some(ProtocolOutcome::Lost);
            }
            tracing::warn!(
                target: "pir::shell_pool",
                delivered = written,
                total = script.len(),
                "shell pool: member stdin write failed after a partial script was delivered; the command may have started, returning a terminal loss without replay"
            );
            return Some(ProtocolOutcome::LostAfterWrite { capture });
        }

        // Wait for the sentinel / timeout / abort, streaming throttled
        // rolling snapshots exactly like the per-call loop.
        let deadline = timeout_secs
            .map(|secs| std::time::Instant::now() + std::time::Duration::from_secs_f64(secs));
        let mut last_update_total = 0usize;
        // Heartbeat clock: reset on every output snapshot (see
        // `SHELL_HEARTBEAT_INTERVAL`).
        let mut last_heartbeat_at = std::time::Instant::now();
        // The sentinel wait, caller-gone, abort, deadline, member death and the
        // throttled streams each get their own branch: no 50 ms poll tick for
        // the first four, and the output snapshot keeps its ~100 ms cadence.
        let update_period = std::time::Duration::from_millis(UPDATE_THROTTLE_MS as u64);
        let mut update_ticker =
            tokio::time::interval_at(tokio::time::Instant::now() + update_period, update_period);
        update_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut maintenance_ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + STALE_FLUSH_INTERVAL,
            STALE_FLUSH_INTERVAL,
        );
        maintenance_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let abort_wait = async {
            match signal.as_ref() {
                Some(signal) => signal.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(abort_wait);
        let deadline_wait = async {
            match deadline {
                Some(deadline) => {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(deadline_wait);
        let wait = loop {
            tokio::select! {
                biased;
                // Caller-gone first: a dropped caller future closes the
                // channel, and the pool task must release the slot instead of
                // running on detached until the command finishes.
                _ = &mut caller_rx => {
                    break PoolWait::CallerGone;
                }
                signal_result = &mut done_rx => {
                    break match signal_result {
                        Ok(Signal::Done(rc)) => PoolWait::Completed(rc),
                        Ok(Signal::Lost) | Err(_) => PoolWait::Lost,
                    };
                }
                // The member process dying is protocol loss even when the
                // sentinel never arrives through the readers: the reader
                // tasks live on the member's OWNING runtime, so if that
                // runtime dropped (e.g. a concurrent caller's test runtime
                // exited) the pipe EOF would never reach the gate. Waiting on
                // the child notices the death immediately (the polled
                // `try_wait` used to add up to 50 ms). The script was already
                // written, so the command is never replayed.
                _ = member.child.wait() => {
                    break PoolWait::Lost;
                }
                _ = &mut abort_wait => {
                    break PoolWait::Aborted;
                }
                _ = &mut deadline_wait => {
                    break PoolWait::Deadline;
                }
                // Stale-tail flush for output smaller than the sentinel
                // holdback window (see `ActiveScan::flush_stale`) and the
                // per-invocation channel drain (file output streams on the
                // same cadence as the per-call path's pipe readers).
                _ = maintenance_ticker.tick() => {
                    if let Some(output) = output.as_mut() {
                        output.drain(&capture);
                    }
                    gate.flush_stale(std::time::Instant::now());
                    // A drain that found the channel directory subverted ends
                    // the call now: its bytes are untrustworthy from here on,
                    // so waiting for a completion would only report output we
                    // refuse to trust. The terminal loss below keeps what was
                    // captured before the tampering was seen.
                    if output.as_ref().is_some_and(OutputChannel::failed) {
                        break PoolWait::Lost;
                    }
                }
                _ = update_ticker.tick() => {
                    if let Some(output) = output.as_mut() {
                        output.drain(&capture);
                    }
                    if output.as_ref().is_some_and(OutputChannel::failed) {
                        break PoolWait::Lost;
                    }
                    if let Some(emit) = on_update.as_ref() {
                        let snapshot = capture
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .update_snapshot(&mut last_update_total);
                        match snapshot {
                            Some(payload) => {
                                emit(payload);
                                last_heartbeat_at = std::time::Instant::now();
                            }
                            None if last_heartbeat_at.elapsed() >= SHELL_HEARTBEAT_INTERVAL => {
                                let guard = capture
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                                emit(shell_heartbeat_payload(&guard, command_started_at.elapsed()));
                                drop(guard);
                                last_heartbeat_at = std::time::Instant::now();
                            }
                            None => {}
                        }
                    }
                }
            }
        };
        // Final channel drain: the shell writes the sentinel only after the
        // command subshell/scriptblock has finished, so everything the
        // command produced is in the channel by now. Then the final snapshot
        // flush (upstream `finishOutput`'s emit), for every outcome, exactly
        // like the per-call path.
        if let Some(output) = output.as_mut() {
            output.drain(&capture);
        }
        // A channel that stopped being private during the call never yields
        // an authoritative result, even if the sentinel arrived: report the
        // bytes captured before the tampering was detected as a terminal loss
        // (never a replay, never Completed).
        let wait = match wait {
            PoolWait::Completed(_) if output.as_ref().is_some_and(OutputChannel::failed) => {
                PoolWait::Lost
            }
            other => other,
        };
        if let Some(emit) = on_update.as_ref() {
            let guard = capture
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(payload) = guard.update_snapshot(&mut last_update_total) {
                emit(payload);
            }
        }

        match wait {
            PoolWait::Completed(rc) => {
                gate.clear();
                Some(ProtocolOutcome::Completed { rc, capture })
            }
            PoolWait::Lost => {
                // The script was already written: the command may have run,
                // so this is a terminal result, never a replay. Flush the
                // scanner's held-back tail so the returned capture is complete.
                gate.flush_pending();
                gate.clear();
                member.terminate();
                *slot = None;
                tracing::warn!(
                    target: "pir::shell_pool",
                    "shell pool: sentinel protocol lost after the command write; returning captured output without replay"
                );
                Some(ProtocolOutcome::LostAfterWrite { capture })
            }
            PoolWait::Aborted => {
                // Flush the scanner's held-back tail, kill the whole member
                // tree, and respawn on the next call (a killed command never
                // poisons the pool).
                gate.flush_pending();
                member.terminate();
                gate.clear();
                *slot = None;
                Some(ProtocolOutcome::Aborted { capture })
            }
            PoolWait::Deadline => {
                gate.flush_pending();
                member.terminate();
                gate.clear();
                *slot = None;
                Some(ProtocolOutcome::TimedOut { capture })
            }
            PoolWait::CallerGone => {
                // Same teardown as an abort: the caller's future was dropped
                // (task abort / host teardown), so nobody consumes the result
                // or streams updates. Kill the command's process tree and
                // release the slot so a queued call (this session or another)
                // is not wedged behind a command that nobody waits for; the
                // next call respawns a fresh member.
                gate.flush_pending();
                member.terminate();
                gate.clear();
                *slot = None;
                tracing::debug!(
                    target: "pir::shell_pool",
                    "shell pool: caller dropped the pool call; member terminated and slot released"
                );
                None
            }
        }
    }

    /// Public inspection/test surface: run one command through this pool
    /// (no session env, no abort signal, unbounded capture) and report the
    /// sentinel exit code plus the captured output.
    pub async fn run_test(
        self: &Arc<Self>,
        protocol: ShellProtocol,
        program: &str,
        cwd: &Path,
        command: &str,
        timeout_secs: Option<f64>,
    ) -> PooledTestOutcome {
        match self
            .execute(
                protocol,
                program,
                cwd,
                command,
                None,
                usize::MAX,
                timeout_secs,
                None,
                None,
            )
            .await
        {
            Some(ProtocolOutcome::Completed { rc, capture }) => {
                let output = capture
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .tail_string();
                PooledTestOutcome::Completed {
                    exit_code: rc,
                    output,
                }
            }
            Some(ProtocolOutcome::TimedOut { .. }) => PooledTestOutcome::TimedOut,
            Some(ProtocolOutcome::Aborted { .. }) => PooledTestOutcome::Aborted,
            Some(ProtocolOutcome::LostAfterWrite { capture }) => {
                PooledTestOutcome::LostAfterWrite {
                    output: capture
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .tail_string(),
                }
            }
            Some(ProtocolOutcome::Lost) | None => PooledTestOutcome::Lost,
        }
    }
}

/// Fire-and-forget pool warm-up for the bash member: when the pool is
/// enabled, start the persistent member in the background so the first real
/// tool call skips the process-creation cost. Idempotent with the lazy path
/// (both meet at the same `ensure_member` mutex, so there is no double-spawn
/// race); failures are ignored — the lazy path and the per-call fallback
/// remain.
///
/// Call from the session tool-loadout preparation when the bash tool is in
/// the enabled set (gating lives at the call site). There is no PowerShell
/// warm-up: that tool always spawns one-shot (see the module docs).
pub fn prewarm(protocol: ShellProtocol) {
    if !crate::config::settings::current_shell_prefs().pool {
        return;
    }
    // Members always live on the pool's own runtime (see [`pool_runtime`]);
    // construction failure leaves the pool disabled and the lazy per-call
    // fallback covers everything.
    let Some(runtime) = pool_runtime() else {
        return;
    };
    runtime.spawn(async move {
        let prefs = crate::config::settings::current_shell_prefs();
        // The external program is the member KEY (the tool passes the same
        // one); `RealSpawner` substitutes the embedded shell at spawn time
        // when `shellPath` is unset.
        let Ok(shell) = super::bash::resolve_shell_config(prefs.shell_path.as_deref()) else {
            return;
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let _ = global()
            .ensure_member(protocol, shell.program(), &cwd)
            .await;
    });
}

/// Test/inspection outcome of one pooled run.
#[derive(Debug)]
pub enum PooledTestOutcome {
    Completed {
        exit_code: i64,
        output: String,
    },
    TimedOut,
    Aborted,
    /// The pool could not dispatch the command at all (spawn failure,
    /// zero-byte script write): the caller may safely fall back to
    /// the per-call spawn path.
    Lost,
    /// Sentinel protocol lost after the script was delivered (a member death
    /// mid-call or a partial write): terminal, never replayed. `output` is
    /// whatever the call captured before the loss.
    LostAfterWrite {
        output: String,
    },
}
