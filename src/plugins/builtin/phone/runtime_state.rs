//! Persisted per-port phone runtime state (port of `phone-runtime.ts`).
//!
//! Several pi sessions on one machine each run their own phone server
//! (8787, 8788, ...). A small JSON file per `host:port` records which process
//! owns that server, so a later `/phone-start` can tell "the owner crashed"
//! from "another live pi session owns this port" without ever reaching across
//! sessions.
//!
//! The file lives at `<temp_dir>/pi-phone-extension/<hostKey>-<port>.json`:
//!
//! ```json
//! {
//!   "pid": 1234,
//!   "host": "127.0.0.1",
//!   "port": 8787,
//!   "controlTokenDigest": "<sha-256 hex>",
//!   "startedAt": "2026-10-15T12:00:00Z"
//! }
//! ```
//!
//! Only a SHA-256 digest of the per-start control token is written; the token
//! itself never touches disk (the reference `digestControlToken`). Reads are
//! tolerant: a malformed or partial file is treated as absent, never an error.
//!
//! [`RuntimeStore`] carries its root directory so tests can point it at a
//! temporary directory without touching the process-wide environment.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value as JsonValue};

/// Directory name under the system temp dir (the reference `runtimeStateDir`).
pub const RUNTIME_STATE_DIR: &str = "pi-phone-extension";

/// Upper bound for one process-liveness probe.
const PROCESS_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// One persisted runtime record (the reference `PersistedPhoneRuntime`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedPhoneRuntime {
    pub pid: u32,
    pub host: String,
    pub port: u16,
    /// SHA-256 hex of the runtime control token; never the token.
    pub control_token_digest: String,
    /// RFC3339 timestamp of the start.
    pub started_at: String,
}

/// A state directory. The default is `<temp_dir>/pi-phone-extension`; tests
/// create one rooted in a unique temporary directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeStore {
    root: PathBuf,
}

impl Default for RuntimeStore {
    fn default() -> Self {
        Self::new(std::env::temp_dir().join(RUNTIME_STATE_DIR))
    }
}

impl RuntimeStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/<hostKey>-<port>.json` (the reference
    /// `getPersistedRuntimeStatePath`).
    ///
    /// Keyed by port as well as host: concurrent sessions must not overwrite
    /// each other's record.
    pub fn path_for(&self, host: &str, port: u16) -> PathBuf {
        self.root.join(format!("{}-{port}.json", host_key(host)))
    }

    /// Read the record, tolerating a missing or malformed file (`None`).
    pub fn read(&self, host: &str, port: u16) -> Option<PersistedPhoneRuntime> {
        let payload = std::fs::read_to_string(self.path_for(host, port)).ok()?;
        let parsed: JsonValue = serde_json::from_str(&payload).ok()?;
        let object = parsed.as_object()?;

        // The reference requires these three fields to be well-typed before it
        // accepts a record at all.
        let host = object.get("host")?.as_str()?.to_owned();
        let port = u16::try_from(object.get("port")?.as_u64()?).ok()?;
        let control_token_digest = object.get("controlTokenDigest")?.as_str()?.to_owned();

        let pid = object
            .get("pid")
            .and_then(JsonValue::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0);
        let started_at = match object.get("startedAt") {
            Some(JsonValue::String(value)) => value.clone(),
            Some(JsonValue::Number(number)) => number
                .as_i64()
                .map(epoch_number_to_rfc3339)
                .unwrap_or_default(),
            _ => String::new(),
        };

        Some(PersistedPhoneRuntime {
            pid,
            host,
            port,
            control_token_digest,
            started_at,
        })
    }

    /// Write the current process as the owner of `host:port`.
    ///
    /// Only the digest of `control_token` is persisted.
    pub fn write(&self, host: &str, port: u16, control_token: &str) -> io::Result<PathBuf> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.path_for(host, port);
        let payload = json!({
            "pid": std::process::id(),
            "host": host,
            "port": port,
            "controlTokenDigest": digest_control_token(control_token),
            "startedAt": rfc3339_now(),
        });
        let text = serde_json::to_string_pretty(&payload).map_err(io::Error::other)?;
        std::fs::write(&path, text)?;
        Ok(path)
    }

    /// Remove the record for `host:port`; a missing file is not an error.
    pub fn remove(&self, host: &str, port: u16) -> io::Result<()> {
        self.remove_path(&self.path_for(host, port))
    }

    /// Remove one record by path; a missing file is not an error.
    pub fn remove_path(&self, path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// SHA-256 hex of the runtime control token (the reference
/// `digestControlToken`); what is persisted is one-way and never the token.
pub fn digest_control_token(control_token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(control_token.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// The stable file-name key for a host.
///
/// Loopback/unspecified hosts collapse to `local` (so `localhost`,
/// `127.0.0.1`, `::1` and `0.0.0.0` are one bucket); every other host is
/// `encodeURIComponent`-encoded.
pub fn host_key(host: &str) -> String {
    let lowered = host.to_ascii_lowercase();
    if matches!(
        lowered.as_str(),
        "127.0.0.1" | "localhost" | "::1" | "[::1]" | "0.0.0.0" | "::" | "[::]"
    ) {
        return "local".to_owned();
    }
    percent_encode_host(host)
}

/// The outcome of inspecting the persisted record for one `host:port`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconcile {
    /// No record exists (or it was unreadable) — nothing to clean up.
    None,
    /// The record belonged to this process and was removed.
    RemovedOwn,
    /// The owning process is gone; the record was removed.
    RemovedDead { pid: u32 },
    /// A live process other than us owns the port. Never touched.
    OtherSession { pid: u32 },
}

/// Inspect (and, where safe, clean up) the record for `host:port`.
///
/// Deliberately never stops or kills anything: the phone server dies with its
/// own pi process, and a second session must never reach across sessions. The
/// caller removes a dangling Tailscale route only in the [`Reconcile::RemovedDead`]
/// case, where no live owner can be hurt.
pub async fn reconcile_runtime(store: &RuntimeStore, host: &str, port: u16) -> Reconcile {
    let Some(runtime) = store.read(host, port) else {
        return Reconcile::None;
    };
    let path = store.path_for(host, port);

    if runtime.pid == std::process::id() {
        // Our own leftover state from an earlier (crashed) start.
        let _ = store.remove_path(&path);
        return Reconcile::RemovedOwn;
    }

    if !is_process_running(runtime.pid).await {
        let _ = store.remove_path(&path);
        return Reconcile::RemovedDead { pid: runtime.pid };
    }

    Reconcile::OtherSession { pid: runtime.pid }
}

/// The reference ownership message: another live pi session keeps the port.
pub fn ownership_message(host: &str, port: u16, pid: u32) -> String {
    format!(
        "Pi Phone on {host}:{port} belongs to another pi session (pid {pid}). \
         It keeps running; start yours without a port to get the next free one."
    )
}

/// Platform process-liveness probe (the reference `isProcessRunning`).
///
/// Unix uses `kill -0`; Windows uses `tasklist` filtered to the pid. Both are
/// spawned directly (no shell) with a short timeout, off the async executor.
/// Invalid pids (`0`, above the signed-pid range) are never running.
pub async fn is_process_running(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    process_exists(pid).await
}

#[cfg(unix)]
async fn process_exists(pid: u32) -> bool {
    let pid_text = pid.to_string();
    let mut command = tokio::process::Command::new("kill");
    command
        .arg("-0")
        .arg(pid_text)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(PROCESS_CHECK_TIMEOUT, command.status()).await {
        Ok(Ok(status)) => status.success(),
        _ => false,
    }
}

#[cfg(windows)]
async fn process_exists(pid: u32) -> bool {
    let filter = format!("PID eq {pid}");
    // Resolve the trusted System32 binary and keep the current directory out
    // of the executable search (a planted `tasklist.exe` must never run).
    let system_root = std::env::var_os("SystemRoot")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| std::ffi::OsString::from(r"C:\Windows"));
    let mut command = tokio::process::Command::new(
        std::path::Path::new(&system_root)
            .join("System32")
            .join("tasklist.exe"),
    );
    command
        .args(["/FI", &filter, "/FO", "CSV", "/NH"])
        .env("NoDefaultCurrentDirectoryInExePath", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    {
        // CREATE_NO_WINDOW: `tasklist` must not flash a console.
        command.creation_flags(0x0800_0000);
    }
    let Ok(Ok(output)) = tokio::time::timeout(PROCESS_CHECK_TIMEOUT, command.output()).await else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let wanted = pid.to_string();
    // CSV row: "image.exe","1234","Console","1","1,234 K". A "no tasks" line
    // has no second field and never matches.
    stdout.lines().any(|line| {
        line.split(',')
            .nth(1)
            .map(|field| field.trim().trim_matches('"') == wanted)
            .unwrap_or(false)
    })
}

#[cfg(not(any(unix, windows)))]
async fn process_exists(pid: u32) -> bool {
    pid == std::process::id()
}

fn percent_encode_host(host: &str) -> String {
    let mut encoded = String::with_capacity(host.len());
    for byte in host.bytes() {
        let character = byte as char;
        // The `encodeURIComponent` unreserved set.
        if character.is_ascii_alphanumeric()
            || matches!(
                character,
                '-' | '_' | '.' | '!' | '~' | '*' | '\'' | '(' | ')'
            )
        {
            encoded.push(character);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn epoch_number_to_rfc3339(value: i64) -> String {
    let seconds = if value.abs() >= 100_000_000_000 {
        value.div_euclid(1000)
    } else {
        value
    };
    epoch_seconds_to_rfc3339(seconds)
}

fn rfc3339_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    epoch_seconds_to_rfc3339(seconds)
}

/// Format Unix seconds as an RFC3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`).
fn epoch_seconds_to_rfc3339(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;

    // Howard Hinnant's civil-from-days algorithm.
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = if month <= 2 { year + 1 } else { year };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}
