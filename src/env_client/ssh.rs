//! Deploying and starting the daemon over the system `ssh` — upstream
//! `packages/env/src/ssh.ts`, the same surface the JS shim carries
//! (`pi_env_shim.js.txt`, `SshTarget`/`sshArguments`/`scanHostKey`/
//! `acceptHostKey`/`forgetHostKey`/`detectPlatform`/`deployDaemon`/
//! `sshConnection`/`connectSsh`).
//!
//! The daemon is verified by its SHA-256 **before it ever runs**, deployed under
//! a content-named path (so versions never collide), and started through an
//! `ssh` invocation with no prompts, no forwarding of any kind, no shared
//! connections, no commands from the configuration, and host keys checked
//! strictly against the application's own known-hosts file under a fixed alias.
//!
//! Differences from the JS shim, all documented consequences of being Rust and
//! of being in-process rather than in the JS sandbox:
//!
//! - `packagedDaemon`'s platform/arch check and messages are upstream's; the
//!   uploaded image is pir's own (`PIR_EXECUTABLE`/`current_exe()`), started as
//!   `<path> env-daemon serve …`, i.e. `daemon_args` defaults to `["env-daemon"]`
//!   exactly as the shim's does.
//! - `scanHostKey`'s temporary directory is named with a UUID instead of
//!   `mkdtemp` (std has no such call); it is removed on every exit path.
//! - `changeKnownHosts` serializes with one process-wide lock instead of a
//!   per-file map: strictly stronger, and there is one known-hosts file per
//!   target in practice.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;
use futures_util::future::BoxFuture;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;

use super::connection::{lock, CommandFn, Connection, ConnectionOptions, LogFn};
use super::remote_env::{RemoteExecutionEnv, RemoteExecutionEnvOptions};

/// The platforms a daemon can be sent to, in the daemon's `hello` spelling.
const REMOTE_PLATFORMS: [(&str, &str); 4] = [
    ("win32", "windows"),
    ("linux", "linux"),
    ("darwin", "darwin"),
    ("android", "android"),
];

/// How long a detection, check or upload over `ssh` may take before it is abandoned.
const SSH_TIMEOUT: Duration = Duration::from_secs(60);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// A failure of the SSH surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshFailure {
    /// The remote host's key is not in the known-hosts file yet.
    HostKeyUnknown { host: String },
    /// The remote host's key differs from the trusted one; it is never
    /// accepted automatically.
    HostKeyChanged { host: String },
    /// An `ssh` invocation that failed for another reason.
    Ssh {
        message: String,
        exit_code: Option<i32>,
        stderr: String,
    },
    /// A client-side failure (an invalid field, a missing hash tool, …).
    Other(String),
}

impl SshFailure {
    fn other(message: impl Into<String>) -> SshFailure {
        SshFailure::Other(message.into())
    }

    pub fn message(&self) -> String {
        match self {
            SshFailure::HostKeyUnknown { host } => {
                format!("The host key of {host} is not trusted yet")
            }
            SshFailure::HostKeyChanged { host } => {
                format!("The host key of {host} changed; remove the old key to continue")
            }
            SshFailure::Ssh { message, .. } | SshFailure::Other(message) => message.clone(),
        }
    }
}

impl std::fmt::Display for SshFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message())
    }
}

impl std::error::Error for SshFailure {}

/// How to reach a machine with the system `ssh` (upstream `SshTarget`).
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub host: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
    /// Host keys this application trusts; `accept_host_key` adds to it.
    pub known_hosts_file: String,
    /// The name keys are stored under, independent of aliases, ports and jump
    /// hosts, e.g. `pi-env-<env name>`.
    pub host_key_alias: String,
    /// The `ssh` program; default `ssh`.
    pub ssh: Option<String>,
    /// An `ssh` configuration file instead of `~/.ssh/config` (`-F`).
    pub config_file: Option<String>,
}

impl SshTarget {
    pub fn new(
        host: impl Into<String>,
        known_hosts_file: impl Into<String>,
        alias: impl Into<String>,
    ) -> SshTarget {
        SshTarget {
            host: host.into(),
            user: None,
            port: None,
            identity_file: None,
            known_hosts_file: known_hosts_file.into(),
            host_key_alias: alias.into(),
            ssh: None,
            config_file: None,
        }
    }
}

/// Options for `ssh_connection`/`connect_ssh`: the target, plus the binary to
/// deploy (default: pir's own image, i.e. `packaged_daemon`).
#[derive(Clone)]
pub struct SshConnectOptions {
    pub target: SshTarget,
    /// A packaged `pi-env` daemon binary (upstream's contract); the daemon's
    /// arguments then default to none.
    pub binary: Option<String>,
    /// The arguments between the deployed program and `serve`.
    pub daemon_args: Option<Vec<String>>,
    /// Run the daemon through the remote login shell (`exec "$SHELL" -lc …`).
    pub login_shell: bool,
    pub on_log: Option<LogFn>,
}

impl SshConnectOptions {
    pub fn new(target: SshTarget) -> SshConnectOptions {
        SshConnectOptions {
            target,
            binary: None,
            daemon_args: None,
            login_shell: false,
            on_log: None,
        }
    }
}

/// What detection found on the remote machine (upstream `RemotePlatform`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePlatform {
    /// `linux`, `android`, `darwin` or `windows` — the daemon's `hello` spelling.
    pub platform: String,
    /// `x64` or `arm64`.
    pub arch: String,
    /// The remote home directory, in the remote system's own spelling.
    pub home: String,
    /// Windows: the shell `sshd` runs commands with (`DefaultShell`).
    pub shell: Option<String>,
    /// Problems worth telling the owner about, such as Termux without `termux-exec`.
    pub warnings: Vec<String>,
}

/// A host-key scan: its known-hosts lines and `ssh-keygen -lf` fingerprints.
/// Showing a fingerprint is not authentication: compare it with one obtained
/// out of band before accepting it.
#[derive(Debug, Clone)]
pub struct HostKeyScan {
    pub lines: Vec<String>,
    pub fingerprints: Vec<String>,
}

/// The connection that starts the daemon over `ssh`, plus the detected platform
/// once a start has detected it (upstream `sshConnection`'s `remote()`).
pub struct SshEnv {
    pub connection: Connection,
    state: Arc<Mutex<SshState>>,
}

impl SshEnv {
    /// The detected system, once a start has detected it.
    pub fn remote(&self) -> Option<RemotePlatform> {
        lock(&self.state).remote.clone()
    }
}

/// No leading `-` (it would be read as an `ssh` option) and no whitespace or
/// control characters.
fn check_field(name: &str, value: &str) -> Result<(), SshFailure> {
    let invalid = value.is_empty()
        || value.starts_with('-')
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control());
    if invalid {
        return Err(SshFailure::other(format!(
            "Invalid {name}: {}",
            serde_json::json!(value)
        )));
    }
    Ok(())
}

/// A path for an `ssh` option that expands `%` tokens: quoted, with `%` literal.
fn config_path(name: &str, path: &str) -> Result<String, SshFailure> {
    if path.is_empty() || path.contains('"') || path.chars().any(char::is_control) {
        return Err(SshFailure::other(format!(
            "Invalid {name}: {}",
            serde_json::json!(path)
        )));
    }
    Ok(format!("\"{}\"", path.replace('%', "%%")))
}

/// Arguments for `ssh` up to the host: no prompts, no forwarding of any kind, no
/// shared connections, no commands from the configuration, no locale forwarding
/// (the remote uses its own), and host keys checked strictly against the
/// application's own file under a fixed alias.
pub fn ssh_arguments(
    target: &SshTarget,
    strict_host_keys: bool,
    known_hosts_file: Option<&str>,
) -> Result<Vec<String>, SshFailure> {
    let known_hosts = known_hosts_file.unwrap_or(&target.known_hosts_file);
    check_field("host", &target.host)?;
    check_field("host key alias", &target.host_key_alias)?;
    if let Some(user) = &target.user {
        check_field("user", user)?;
    }
    if let Some(port) = target.port {
        if port == 0 {
            return Err(SshFailure::other(format!("Invalid port: {port}")));
        }
    }
    let option = |setting: String| vec!["-o".to_string(), setting];
    let mut args: Vec<String> = Vec::new();
    if let Some(config_file) = &target.config_file {
        args.push("-F".to_string());
        args.push(config_file.clone());
    }
    args.push("-T".to_string());
    args.push("-a".to_string());
    args.push("-x".to_string());
    for setting in [
        "BatchMode=yes".to_string(),
        "ClearAllForwardings=yes".to_string(),
        "ForwardAgent=no".to_string(),
        "ForwardX11=no".to_string(),
        "ControlMaster=no".to_string(),
        "ControlPath=none".to_string(),
        "RemoteCommand=none".to_string(),
        "PermitLocalCommand=no".to_string(),
        "SendEnv=-*".to_string(),
        "ServerAliveInterval=15".to_string(),
        format!(
            "StrictHostKeyChecking={}",
            if strict_host_keys {
                "yes"
            } else {
                "accept-new"
            }
        ),
        format!(
            "UserKnownHostsFile={}",
            config_path("known hosts file", known_hosts)?
        ),
        "GlobalKnownHostsFile=none".to_string(),
        "HashKnownHosts=no".to_string(),
        format!("HostKeyAlias={}", target.host_key_alias),
    ] {
        args.extend(option(setting));
    }
    if let Some(user) = &target.user {
        args.push("-l".to_string());
        args.push(user.clone());
    }
    if let Some(port) = target.port {
        args.push("-p".to_string());
        args.push(port.to_string());
    }
    if let Some(identity_file) = &target.identity_file {
        args.extend(option(format!(
            "IdentityFile={}",
            config_path("identity file", identity_file)?
        )));
        args.extend(option("IdentitiesOnly=yes".to_string()));
    }
    args.push("--".to_string());
    args.push(target.host.clone());
    Ok(args)
}

/// One `ssh` invocation's options.
#[derive(Default)]
struct RunSshOptions {
    args: Option<Vec<String>>,
    stdin: Vec<u8>,
    timeout: Option<Duration>,
}

/// Run one remote command over `ssh`, feeding `stdin`; stdout on success.
async fn run_ssh(
    target: &SshTarget,
    command: &str,
    options: RunSshOptions,
) -> Result<String, SshFailure> {
    let args = match options.args {
        Some(args) => args,
        None => ssh_arguments(target, true, None)?,
    };
    let program = target.ssh.clone().unwrap_or_else(|| "ssh".to_string());
    let mut child = Command::new(&program)
        .args(&args)
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| SshFailure::Ssh {
            message: error.to_string(),
            exit_code: None,
            stderr: String::new(),
        })?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(&options.stdin).await;
        drop(stdin);
    }
    let timeout = options.timeout.unwrap_or(SSH_TIMEOUT);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = stdout.map(|pipe| tokio::spawn(read_all(pipe)));
    let stderr_task = stderr.map(|pipe| tokio::spawn(read_all(pipe)));
    let status = tokio::select! {
        status = child.wait() => Some(status),
        () = tokio::time::sleep(timeout) => None,
    };
    let timed_out = status.is_none();
    if timed_out {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    let out = join_pipe(stdout_task).await;
    let diagnostics = join_pipe(stderr_task).await;
    if timed_out {
        return Err(SshFailure::Ssh {
            message: format!(
                "ssh {} did not finish within {} s: {}",
                target.host,
                timeout.as_secs(),
                diagnostics.trim()
            ),
            exit_code: None,
            stderr: diagnostics,
        });
    }
    let code = status.and_then(Result::ok).and_then(|status| status.code());
    if code == Some(0) {
        return Ok(out);
    }
    if diagnostics.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        return Err(SshFailure::HostKeyChanged {
            host: target.host.clone(),
        });
    }
    if diagnostics.contains("Host key verification failed")
        || diagnostics.contains("host key is known")
    {
        return Err(SshFailure::HostKeyUnknown {
            host: target.host.clone(),
        });
    }
    Err(SshFailure::Ssh {
        message: format!(
            "ssh {} failed with exit code {}: {}",
            target.host,
            code.map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string()),
            diagnostics.trim()
        ),
        exit_code: code,
        stderr: diagnostics,
    })
}

async fn read_all(mut pipe: impl tokio::io::AsyncRead + Unpin) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = pipe.read_to_end(&mut bytes).await;
    bytes
}

async fn join_pipe(task: Option<tokio::task::JoinHandle<Vec<u8>>>) -> String {
    match task {
        Some(task) => String::from_utf8_lossy(&task.await.unwrap_or_default()).into_owned(),
        None => String::new(),
    }
}

/// Connect once with `accept-new` against a temporary known-hosts file to
/// capture the key the host presents, through the same route (`~/.ssh/config`,
/// jump hosts) as real connections.
pub async fn scan_host_key(target: &SshTarget) -> Result<HostKeyScan, SshFailure> {
    let directory = std::env::temp_dir().join(format!("pi-env-hostkey-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(|error| SshFailure::other(error.to_string()))?;
    let result = scan_host_key_in(target, &directory).await;
    let _ = tokio::fs::remove_dir_all(&directory).await;
    result
}

async fn scan_host_key_in(target: &SshTarget, directory: &Path) -> Result<HostKeyScan, SshFailure> {
    let scanned = directory.join("known_hosts");
    let scanned = scanned.to_string_lossy().into_owned();
    // Authentication may fail without the key; the key is recorded before
    // authentication. A changed key is the one failure that must surface.
    match run_ssh(
        target,
        "exit 0",
        RunSshOptions {
            args: Some(ssh_arguments(target, false, Some(&scanned))?),
            ..RunSshOptions::default()
        },
    )
    .await
    {
        Ok(_) => {}
        Err(SshFailure::Ssh { .. } | SshFailure::HostKeyUnknown { .. }) => {}
        Err(other) => return Err(other),
    }
    let lines: Vec<String> = match tokio::fs::read_to_string(&scanned).await {
        Ok(text) => text
            .split('\n')
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    };
    if lines.is_empty() {
        return Err(SshFailure::other(format!(
            "No host key received from {}",
            target.host
        )));
    }
    let output = Command::new("ssh-keygen")
        .args(["-lf", &scanned])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| SshFailure::other(error.to_string()))?;
    let fingerprints = String::from_utf8_lossy(&output.stdout)
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect();
    Ok(HostKeyScan {
        lines,
        fingerprints,
    })
}

/// A known-hosts line for `alias`: `alias type key [comment]`, no markers,
/// patterns or hashed names.
fn parse_host_key_line(alias: &str, line: &str) -> Result<(String, String), SshFailure> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    let (host, key_type, key) = (
        parts.first().copied().unwrap_or_default(),
        parts.get(1).copied().unwrap_or_default(),
        parts.get(2).copied().unwrap_or_default(),
    );
    if host != alias
        || key_type.is_empty()
        || key.is_empty()
        || !valid_key_type(key_type)
        || !valid_key(key)
    {
        return Err(SshFailure::other(format!(
            "Not a host key line for {alias}: {}",
            serde_json::json!(line.trim())
        )));
    }
    Ok((key_type.to_string(), key.to_string()))
}

/// `^(ssh-[a-z0-9-]+|ecdsa-sha2-[a-z0-9-]+|sk-[a-z0-9@.-]+)$`.
fn valid_key_type(key_type: &str) -> bool {
    let body = |prefix: &str, allow_at: bool| match key_type.strip_prefix(prefix) {
        Some(rest) if !rest.is_empty() => rest.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || character == '-'
                || (allow_at && character == '@')
        }),
        _ => false,
    };
    body("ssh-", false) || body("ecdsa-sha2-", false) || body("sk-", true)
}

/// `^[A-Za-z0-9+/]+={0,2}$`: base64 with at most two padding characters.
fn valid_key(key: &str) -> bool {
    let body = key.trim_end_matches('=');
    key.len() - body.len() <= 2
        && !body.is_empty()
        && body.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '+' || character == '/'
        })
}

/// One process-wide lock for known-hosts changes: the JS shim serializes per
/// file; serializing all files is strictly stronger and equivalent in practice.
static KNOWN_HOSTS: AsyncMutex<()> = AsyncMutex::const_new(());

/// Rewrite the known-hosts file one change at a time, atomically: a fresh
/// temporary file next to it, then a rename.
async fn change_known_hosts<F>(file: &str, change: F) -> Result<(), SshFailure>
where
    F: FnOnce(Vec<String>) -> Result<Vec<String>, SshFailure>,
{
    let _guard = KNOWN_HOSTS.lock().await;
    if let Some(parent) = Path::new(file).parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| SshFailure::other(error.to_string()))?;
    }
    let existing: Vec<String> = match tokio::fs::read_to_string(file).await {
        Ok(text) => text
            .split('\n')
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    };
    let lines = change(existing)?;
    let temporary = format!("{file}.{}.tmp", hex(&uuid::Uuid::new_v4().into_bytes()));
    let content = if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    };
    let write = async {
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut handle = options
            .open(&temporary)
            .await
            .map_err(|error| SshFailure::other(error.to_string()))?;
        handle
            .write_all(content.as_bytes())
            .await
            .map_err(|error| SshFailure::other(error.to_string()))?;
        drop(handle);
        // The file holds the application's trusted host keys: owner-only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
                .await;
        }
        Ok::<(), SshFailure>(())
    }
    .await;
    if let Err(error) = write {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(error);
    }
    if let Err(error) = tokio::fs::rename(&temporary, file).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(SshFailure::other(error.to_string()));
    }
    Ok(())
}

/// Trust host-key lines from `scan_host_key` by adding them to the target's
/// known-hosts file. Only plain lines for the target's alias are accepted. A
/// key that differs from a trusted key of the same type is refused with
/// `HostKeyChanged`; `forget_host_key` must remove the old keys first.
pub async fn accept_host_key(target: &SshTarget, lines: &[String]) -> Result<(), SshFailure> {
    let alias = target.host_key_alias.clone();
    let mut accepted: Vec<(String, String, String)> = Vec::new();
    for line in lines {
        let (key_type, key) = parse_host_key_line(&alias, line)?;
        accepted.push((key_type, key, line.trim().to_string()));
    }
    let host = target.host.clone();
    change_known_hosts(&target.known_hosts_file, move |existing| {
        let mut trusted: Vec<(String, String)> = Vec::new();
        for line in &existing {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.first() == Some(&alias.as_str()) && parts.len() >= 3 {
                trusted.push((parts[1].to_string(), parts[2].to_string()));
            }
        }
        let mut added: Vec<String> = Vec::new();
        for (key_type, key, line) in accepted {
            match trusted.iter().find(|(known, _)| *known == key_type) {
                Some((_, known)) if *known == key => continue,
                Some(_) => {
                    return Err(SshFailure::HostKeyChanged { host });
                }
                None => {
                    trusted.push((key_type, key));
                    added.push(line);
                }
            }
        }
        Ok([existing, added].concat())
    })
    .await
}

/// Stop trusting every key stored for the target's alias, e.g. after the owner
/// confirmed a changed host key.
pub async fn forget_host_key(target: &SshTarget) -> Result<(), SshFailure> {
    let alias = target.host_key_alias.clone();
    change_known_hosts(&target.known_hosts_file, move |existing| {
        Ok(existing
            .into_iter()
            .filter(|line| line.split_whitespace().next() != Some(alias.as_str()))
            .collect())
    })
    .await
}

/// POSIX detection, run by the remote login shell; a Windows host answers
/// through PowerShell instead.
const POSIX_PROBE: &str = "sh -c 'echo PI-ENV-PROBE; uname -s; uname -m; uname -o 2>/dev/null || echo -; printf \"%s\\n\" \"$HOME\" \"${TMPDIR:--}\" \"${LD_PRELOAD:--}\"'";

/// `powershell -NoProfile -NonInteractive -EncodedCommand <base64 of the UTF-16LE script>`,
/// so the script survives any console code page.
fn powershell(script: &str) -> String {
    let mut utf16: Vec<u8> = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        utf16.extend_from_slice(&unit.to_le_bytes());
    }
    format!(
        "powershell -NoProfile -NonInteractive -EncodedCommand {}",
        base64::engine::general_purpose::STANDARD.encode(utf16)
    )
}

const WINDOWS_PROBE: &str = "'PI-ENV-PROBE'; 'Windows'; [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString(); '-'; $HOME; '-'; '-'";

fn normalize_arch(machine: &str) -> Result<String, SshFailure> {
    match machine.to_lowercase().as_str() {
        "x86_64" | "amd64" | "x64" => Ok("x64".to_string()),
        "aarch64" | "arm64" => Ok("arm64".to_string()),
        _ => Err(SshFailure::other(format!(
            "Unsupported remote architecture: {machine}"
        ))),
    }
}

struct Probe {
    system: String,
    machine: String,
    os: String,
    home: String,
    tmpdir: String,
    preload: String,
}

fn parse_probe(host: &str, output: &str) -> Result<Probe, SshFailure> {
    // Login shells may print a banner first.
    let lines: Vec<&str> = output
        .split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .collect();
    let Some(start) = lines.iter().position(|line| *line == "PI-ENV-PROBE") else {
        return Err(SshFailure::other(format!(
            "Unexpected answer from {host}: {}",
            output.trim()
        )));
    };
    let rest = &lines[start + 1..];
    let at = |index: usize| rest.get(index).copied().unwrap_or_default().to_string();
    Ok(Probe {
        system: at(0),
        machine: at(1),
        os: at(2),
        home: at(3),
        tmpdir: rest.get(4).map_or_else(
            || "-".to_string(),
            |value| {
                if value.is_empty() {
                    "-".to_string()
                } else {
                    value.to_string()
                }
            },
        ),
        preload: rest.get(5).map_or_else(
            || "-".to_string(),
            |value| {
                if value.is_empty() {
                    "-".to_string()
                } else {
                    value.to_string()
                }
            },
        ),
    })
}

/// Termux works only with its own `TMPDIR` and with `termux-exec`, which makes
/// `#!/usr/bin/env` shebangs work.
fn termux_warnings(probe: &Probe) -> Vec<String> {
    let mut warnings = Vec::new();
    if probe.tmpdir == "-" {
        warnings
            .push("TMPDIR is not set; Termux's sshd normally sets it to $PREFIX/tmp.".to_string());
    }
    if !probe.preload.contains("termux-exec") {
        warnings.push(
            "termux-exec is not loaded (LD_PRELOAD); scripts with #!/usr/bin/env shebangs will fail."
                .to_string(),
        );
    }
    warnings.push(
        "Android may suspend Termux; run termux-wake-lock on the device to keep the connection alive."
            .to_string(),
    );
    warnings
}

/// Which system the target runs: `uname` through the login shell, or PowerShell
/// on Windows.
pub async fn detect_platform(target: &SshTarget) -> Result<RemotePlatform, SshFailure> {
    let posix = match run_ssh(target, POSIX_PROBE, RunSshOptions::default()).await {
        Ok(output) => parse_probe(&target.host, &output),
        Err(error) => Err(error),
    };
    let probe = match posix {
        Ok(probe) => probe,
        // An untrusted or changed host key is not something the other probe can fix.
        Err(error @ (SshFailure::HostKeyUnknown { .. } | SshFailure::HostKeyChanged { .. })) => {
            return Err(error);
        }
        // cmd.exe or PowerShell as the remote shell: no `sh`, or one that gets
        // the probe's quotes wrong.
        Err(_) => {
            let output =
                run_ssh(target, &powershell(WINDOWS_PROBE), RunSshOptions::default()).await?;
            parse_probe(&target.host, &output)?
        }
    };
    // Git Bash as Windows' default SSH shell: ask PowerShell for Windows' own
    // architecture and home spelling.
    let probe = if probe.system.starts_with("MINGW")
        || probe.system.starts_with("MSYS")
        || probe.system.starts_with("CYGWIN")
    {
        let output = run_ssh(target, &powershell(WINDOWS_PROBE), RunSshOptions::default()).await?;
        parse_probe(&target.host, &output)?
    } else {
        probe
    };
    let arch = normalize_arch(&probe.machine)?;
    if probe.system == "Windows" {
        // cmd.exe expands %OS%; PowerShell prints it as is.
        let shell = if run_ssh(target, "echo %OS%", RunSshOptions::default())
            .await
            .unwrap_or_default()
            .contains("Windows_NT")
        {
            "cmd"
        } else {
            "powershell"
        };
        return Ok(RemotePlatform {
            platform: "windows".to_string(),
            arch,
            home: probe.home,
            shell: Some(shell.to_string()),
            warnings: Vec::new(),
        });
    }
    if probe.system == "Darwin" {
        return Ok(RemotePlatform {
            platform: "darwin".to_string(),
            arch,
            home: probe.home,
            shell: None,
            warnings: Vec::new(),
        });
    }
    if probe.system == "Linux" {
        let android = probe.os == "Android";
        let warnings = if android {
            termux_warnings(&probe)
        } else {
            Vec::new()
        };
        return Ok(RemotePlatform {
            platform: if android { "android" } else { "linux" }.to_string(),
            arch,
            home: probe.home,
            shell: None,
            warnings,
        });
    }
    Err(SshFailure::other(format!(
        "Unsupported remote system: {}",
        probe.system
    )))
}

/// The daemon command for this machine (`localDaemon`): `PIR_EXECUTABLE`, else
/// the running image, then the `env-daemon` subcommand (the client appends
/// `serve --token <hex>`, so the full argv is `<image> env-daemon serve --token
/// <hex>`).
///
/// **Contract**: `current_exe()` must be the pir binary itself. When pir's
/// client code is embedded in another host — an integration-test harness, an
/// embedding application — `current_exe()` is *that* program, and this returns
/// `[<host>, "env-daemon"]`, a command that will never speak the protocol (the
/// handshake then fails only when the 60 s start timeout expires). Such callers
/// must either set `PIR_EXECUTABLE` to the pir image, or build their connection
/// from an explicit argv: [`crate::env_client::connection::Connection::argv`]
/// with `[<pir image>, "env-daemon"]` (what the integration tests in
/// `tests/env_client_e2e.rs` do, from `CARGO_BIN_EXE_pir`).
pub fn local_daemon() -> Vec<String> {
    let image = std::env::var("PIR_EXECUTABLE").unwrap_or_default();
    if !image.is_empty() {
        return vec![image, "env-daemon".to_string()];
    }
    match std::env::current_exe() {
        Ok(executable) => vec![
            executable.to_string_lossy().into_owned(),
            "env-daemon".to_string(),
        ],
        Err(_) => vec!["pir".to_string(), "env-daemon".to_string()],
    }
}

/// The daemon image for a remote system: `PI_ENV_DAEMON` (a standalone
/// `pi-env` binary of any version — protocol 1 is the contract), else pir's own
/// image when the target runs the platform this build was compiled for.
/// Upstream ships one prebuilt daemon per platform; pir builds per platform
/// instead, so a cross-platform deploy is refused rather than guessed.
pub fn packaged_daemon(remote: &RemotePlatform) -> Result<String, SshFailure> {
    let override_path = std::env::var("PI_ENV_DAEMON").unwrap_or_default();
    if !override_path.is_empty() {
        return Ok(override_path);
    }
    let platform = REMOTE_PLATFORMS
        .iter()
        .find(|(node, _)| *node == std::env::consts::OS)
        .map(|(_, daemon)| *daemon)
        .unwrap_or(std::env::consts::OS);
    let arch = normalize_arch(std::env::consts::ARCH)?;
    if remote.platform != platform || remote.arch != arch {
        return Err(SshFailure::other(format!(
            "No pi-env daemon for {}-{}: this pir build is {}-{}. \
             Build pir for that target, or set PI_ENV_DAEMON to a pi-env binary and have it deployed.",
            remote.platform, remote.arch, platform, arch
        )));
    }
    let image = std::env::var("PIR_EXECUTABLE").unwrap_or_default();
    if !image.is_empty() {
        return Ok(image);
    }
    std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|error| SshFailure::other(error.to_string()))
}

fn quote_posix(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn quote_power_shell(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// Where a daemon with this content lives on the remote machine: named by its
/// SHA-256, so versions never collide.
pub fn daemon_path(remote: &RemotePlatform, sha256: &str) -> String {
    let name = format!("pi-env-{}", &sha256[..sha256.len().min(32)]);
    if remote.platform == "windows" {
        format!("{}\\.pi\\mobile\\tools\\{name}.exe", remote.home)
    } else {
        format!("{}/.pi/mobile/tools/{name}", remote.home)
    }
}

const POSIX_HASH: &str = r#"hash() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1; elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 "$1" | sed 's/.*= //'; else echo none; fi; }"#;

/// Whether the remote file has this content: `present`, `missing`, or (POSIX
/// without a hash tool) `nohash`.
async fn check_daemon(
    target: &SshTarget,
    remote: &RemotePlatform,
    file: &str,
    sha256: &str,
) -> Result<String, SshFailure> {
    if remote.platform == "windows" {
        let check = powershell(&format!(
            "$f = {}; if ((Test-Path -LiteralPath $f) -and ((Get-FileHash -Algorithm SHA256 -LiteralPath $f).Hash.ToLower() -eq '{}')) {{ 'present' }} else {{ 'missing' }}",
            quote_power_shell(file),
            sha256
        ));
        let output = run_ssh(target, &check, RunSshOptions::default()).await?;
        return Ok(if output.trim().ends_with("present") {
            "present".to_string()
        } else {
            "missing".to_string()
        });
    }
    let check = format!(
        "{POSIX_HASH}; f={}; if [ -f \"$f\" ] && [ \"$(hash \"$f\")\" = {sha256} ]; then echo present; elif [ \"$(hash /dev/null)\" = none ]; then echo nohash; else echo missing; fi",
        quote_posix(file)
    );
    let output = run_ssh(
        target,
        &format!("sh -c {}", quote_posix(&check)),
        RunSshOptions::default(),
    )
    .await?;
    Ok(output
        .trim()
        .split('\n')
        .next_back()
        .unwrap_or("missing")
        .to_string())
}

/// Make sure the daemon is on the remote machine, verified by its SHA-256
/// before it ever runs, and remove daemons of other contents. The upload goes
/// to a new temporary file and is renamed into place only once its hash
/// matches. Returns the remote path of the binary.
pub async fn deploy_daemon(
    target: &SshTarget,
    remote: &RemotePlatform,
    binary: Option<&str>,
) -> Result<String, SshFailure> {
    let file0 = match binary {
        Some(binary) => binary.to_string(),
        None => packaged_daemon(remote)?,
    };
    if !Path::new(&file0).is_file() {
        return Err(SshFailure::other(format!(
            "No pi-env daemon for {}-{} at {}",
            remote.platform, remote.arch, file0
        )));
    }
    let bytes = tokio::fs::read(&file0)
        .await
        .map_err(|error| SshFailure::other(error.to_string()))?;
    let sha256 = sha256_hex(&bytes);
    let file = daemon_path(remote, &sha256);
    let state = check_daemon(target, remote, &file, &sha256).await?;
    if state == "present" {
        return Ok(file);
    }
    if state == "nohash" {
        return Err(SshFailure::other(format!(
            "{} has no sha256sum, shasum or openssl to verify pi-env",
            target.host
        )));
    }
    if remote.platform == "windows" {
        let upload = powershell(&[
            "$ErrorActionPreference = 'Stop'".to_string(),
            format!("$f = {}", quote_power_shell(&file)),
            "$d = Split-Path -Parent $f".to_string(),
            "New-Item -ItemType Directory -Force -Path $d | Out-Null".to_string(),
            "$t = Join-Path $d ('.pi-env-' + [guid]::NewGuid().ToString() + '.tmp')".to_string(),
            // PowerShell reads redirected stdin itself, as text lines for
            // `$input`, so the binary comes as base64 lines up to an end
            // marker: Windows' sshd may never pass on the end of stdin.
            "$text = New-Object System.Text.StringBuilder".to_string(),
            "foreach ($line in $input) { if ($line -eq 'PI-ENV-END') { break }; [void]$text.Append($line) }"
                .to_string(),
            "$bytes = [Convert]::FromBase64String($text.ToString())".to_string(),
            "$out = [IO.File]::Open($t, 'CreateNew', 'Write', 'None'); $out.Write($bytes, 0, $bytes.Length); $out.Close()"
                .to_string(),
            format!(
                "if ((Get-FileHash -Algorithm SHA256 -LiteralPath $t).Hash.ToLower() -ne '{sha256}') {{ Remove-Item -LiteralPath $t; throw 'pi-env upload is corrupt' }}"
            ),
            // A running daemon or a virus scanner can hold the old file for a moment.
            "for ($i = 0; ; $i++) { try { Move-Item -Force -LiteralPath $t -Destination $f; break } catch { if ($i -ge 20) { throw }; Start-Sleep -Milliseconds 250 } }"
                .to_string(),
            // Older daemons; one that is running stays until it exits.
            "Get-ChildItem -LiteralPath $d -Filter 'pi-env-*.exe' | Where-Object { $_.FullName -ne $f } | ForEach-Object { Remove-Item -LiteralPath $_.FullName -ErrorAction SilentlyContinue }"
                .to_string(),
            "'deployed'".to_string(),
        ]
        .join("; "));
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let mut lines = String::new();
        for chunk in encoded.as_bytes().chunks(76) {
            lines.push_str(&String::from_utf8_lossy(chunk));
            lines.push('\n');
        }
        lines.push_str("PI-ENV-END\n");
        run_ssh(
            target,
            &upload,
            RunSshOptions {
                stdin: lines.into_bytes(),
                timeout: Some(UPLOAD_TIMEOUT),
                ..RunSshOptions::default()
            },
        )
        .await?;
        return Ok(file);
    }
    let upload = [
        "set -e".to_string(),
        POSIX_HASH.to_string(),
        format!("f={}", quote_posix(&file)),
        "d=$(dirname \"$f\")".to_string(),
        "mkdir -p \"$d\"".to_string(),
        "chmod 700 \"$d\"".to_string(),
        "t=$(mktemp \"$d/.pi-env.XXXXXX\")".to_string(),
        "cat > \"$t\"".to_string(),
        format!(
            "if [ \"$(hash \"$t\")\" != {sha256} ]; then rm -f \"$t\"; echo \"pi-env upload is corrupt\" >&2; exit 1; fi"
        ),
        "chmod 700 \"$t\"".to_string(),
        "mv -f \"$t\" \"$f\"".to_string(),
        // Older daemons; running ones keep their file open until they exit.
        "for old in \"$d\"/pi-env-*; do [ \"$old\" = \"$f\" ] || rm -f \"$old\"; done".to_string(),
        "echo deployed".to_string(),
    ]
    .join("\n");
    run_ssh(
        target,
        &format!("sh -c {}", quote_posix(&upload)),
        RunSshOptions {
            stdin: bytes,
            timeout: Some(UPLOAD_TIMEOUT),
            ..RunSshOptions::default()
        },
    )
    .await?;
    Ok(file)
}

/// The remote command that starts the daemon at `file`, before the `serve`
/// arguments.
pub fn launch_command(remote: &RemotePlatform, file: &str, login_shell: bool) -> String {
    if remote.platform == "windows" {
        // PowerShell runs a quoted path only with the call operator; cmd.exe
        // keeps one pair of quotes around a program.
        if remote.shell.as_deref() == Some("powershell") {
            return format!("& {}", quote_power_shell(file));
        }
        return if file.contains(' ') {
            format!("\"{file}\"")
        } else {
            file.to_string()
        };
    }
    if login_shell {
        // `$0` is the daemon, `"$@"` its arguments; `$SHELL` is the login
        // shell's own name for itself.
        return format!(
            "exec \"$SHELL\" -lc 'exec \"$0\" \"$@\"' {}",
            quote_posix(file)
        );
    }
    quote_posix(file)
}

/// The arguments between the deployed program and `serve`: a pir image is asked
/// for its daemon subcommand (`pir env-daemon serve …`), while an upstream
/// `pi-env` binary takes `serve` directly. `options.binary` means a packaged
/// `pi-env` daemon (upstream's contract).
pub fn daemon_args(options: &SshConnectOptions, packaged_binary: Option<&str>) -> Vec<String> {
    if let Some(daemon_args) = &options.daemon_args {
        return daemon_args.clone();
    }
    if options.binary.is_none() && packaged_binary.is_none() {
        vec!["env-daemon".to_string()]
    } else {
        Vec::new()
    }
}

/// The state a connection's starts share: the detected platform (once) and a
/// deployment that just verified the binary (counts for the first start).
#[derive(Default)]
struct SshState {
    remote: Option<RemotePlatform>,
    verified: Option<String>,
}

fn ssh_command_for(options: SshConnectOptions, state: Arc<Mutex<SshState>>) -> CommandFn {
    Arc::new(move || {
        let options = options.clone();
        let state = state.clone();
        Box::pin(async move {
            let remote = {
                let known = lock(&state).remote.clone();
                match known {
                    Some(remote) => remote,
                    None => {
                        let remote = detect_platform(&options.target)
                            .await
                            .map_err(|error| error.message())?;
                        for warning in &remote.warnings {
                            if let Some(on_log) = &options.on_log {
                                on_log(&format!("{warning}\n"));
                            }
                        }
                        lock(&state).remote = Some(remote.clone());
                        remote
                    }
                }
            };
            // A deployment that just verified the binary counts for the first start.
            let verified = lock(&state).verified.take();
            let file = match verified {
                Some(file) => file,
                None => {
                    let packaged = packaged_daemon(&remote).ok();
                    let binary = options.binary.clone().or(packaged);
                    deploy_daemon(&options.target, &remote, binary.as_deref())
                        .await
                        .map_err(|error| error.message())?
                }
            };
            let mut command = vec![options
                .target
                .ssh
                .clone()
                .unwrap_or_else(|| "ssh".to_string())];
            command.extend(
                ssh_arguments(&options.target, true, None).map_err(|error| error.message())?,
            );
            command.push(launch_command(&remote, &file, options.login_shell));
            command.extend(daemon_args(&options, options.binary.as_deref()));
            Ok(command)
        }) as BoxFuture<'static, Result<Vec<String>, String>>
    })
}

/// A `Connection` to the target that does nothing until its first request. Each
/// start detects the remote system (the first time), verifies the daemon and
/// deploys it if it is missing or changed, then starts it over `ssh`.
pub fn ssh_connection(options: SshConnectOptions) -> SshEnv {
    let state = Arc::new(Mutex::new(SshState::default()));
    let command = ssh_command_for(options.clone(), state.clone());
    // The daemon runs on the other machine: `exec` must not forward this
    // process's environment (that host's daemon inherits its own).
    let mut connection_options = ConnectionOptions::dynamic(command).remote();
    if let Some(on_log) = options.on_log.clone() {
        connection_options = connection_options.on_log(on_log);
    }
    SshEnv {
        connection: Connection::new(connection_options),
        state,
    }
}

/// Detect the remote system and deploy the daemon now, then return a connection
/// that starts it over `ssh`. Failures reject here: host keys must already be
/// trusted (`scan_host_key`, `accept_host_key`).
pub async fn connect_ssh(options: SshConnectOptions) -> Result<SshEnv, SshFailure> {
    let remote = detect_platform(&options.target).await?;
    let packaged = packaged_daemon(&remote).ok();
    let binary = options.binary.clone().or(packaged);
    let verified = deploy_daemon(&options.target, &remote, binary.as_deref()).await?;
    for warning in &remote.warnings {
        if let Some(on_log) = &options.on_log {
            on_log(&format!("{warning}\n"));
        }
    }
    let state = Arc::new(Mutex::new(SshState {
        remote: Some(remote.clone()),
        verified: Some(verified),
    }));
    let command = ssh_command_for(options.clone(), state.clone());
    // The daemon runs on the other machine: `exec` must not forward this
    // process's environment (that host's daemon inherits its own).
    let mut connection_options = ConnectionOptions::dynamic(command).remote();
    if let Some(on_log) = &options.on_log {
        connection_options = connection_options.on_log(on_log.clone());
    }
    Ok(SshEnv {
        connection: Connection::new(connection_options),
        state,
    })
}

/// The environment of another machine over `ssh`, with the daemon deployed.
pub async fn ssh_remote_env(
    options: SshConnectOptions,
    cwd: Option<String>,
) -> Result<RemoteExecutionEnv, SshFailure> {
    let id = format!("pi-env:ssh:{}", options.target.host);
    let env = connect_ssh(options).await?;
    let mut env_options = RemoteExecutionEnvOptions::new(id, env.connection);
    env_options.cwd = cwd;
    Ok(RemoteExecutionEnv::new(env_options))
}

/// Lowercase hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// SHA-256 of `bytes`, lowercase hex (`createHash("sha256").update(bytes).digest("hex")`).
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}
