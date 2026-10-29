use super::*;

// File helpers
// ---------------------------------------------------------------------------

/// Cheap change detector for one configuration file: length plus modification
/// time, taken from a single `metadata` call. A config-derived cache reuses an
/// entry only while this stamp is unchanged, so an edited, replaced or deleted
/// file is re-read instead of a stale value being served.
///
/// Deliberately NOT a content hash: validating a cached value must not read the
/// file (the callers run on request/config-load paths). A same-length rewrite
/// inside the filesystem's mtime granularity is the one edit this cannot see
/// (NTFS/ext4 report 100ns/tick timestamps, so this is a corner case); the
/// `!command` consumer also exposes
/// `providers::config_value::clear_config_value_cache` for a caller that knows
/// it must not miss such an edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    /// File length in bytes.
    pub len: u64,
    /// Last modification time, when the platform reports one.
    pub modified: Option<std::time::SystemTime>,
}

/// The current [`FileStamp`] of `path`, or `None` when it cannot be stat'ed
/// (absent, unreadable, or not a regular file). `None` is itself a stable
/// state: the file appearing later changes the stamp.
pub fn file_stamp(path: &Path) -> Option<FileStamp> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    Some(FileStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

/// Read a text file, stripping a UTF-8 BOM; `None` on any read error.
pub(super) fn read_text_file(path: &Path) -> Option<String> {
    read_text_file_checked(path).ok().flatten()
}

/// Read a text file, stripping a UTF-8 BOM, distinguishing a missing file
/// (`Ok(None)`) from a real read failure (`Err`): a settings file that exists
/// but cannot be read (permissions, invalid UTF-8, a directory, …) must be
/// reported as a startup diagnostic instead of silently treated as absent.
/// `read_text_file` keeps the lossy `Option` shape for callers that only want
/// the content (context-file discovery, persistence).
pub(super) fn read_text_file_checked(path: &Path) -> std::io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(
            text.strip_prefix('\u{feff}').unwrap_or(&text).to_string(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Bounded acquisition attempts for the settings-file lock (the same retry
/// shape as the trust store's lock).
const SETTINGS_LOCK_ATTEMPTS: u32 = 10;
/// Delay between settings-lock acquisition attempts.
const SETTINGS_LOCK_RETRY_DELAY: Duration = Duration::from_millis(20);
/// A lock older than this window is considered abandoned (its owner died
/// mid-write) and is taken over, matching the trust store's stale-lock policy.
const SETTINGS_LOCK_STALE_AFTER: Duration = Duration::from_secs(10);

/// RAII release for the settings-file lock. The owner token is checked on
/// release: when another process took over a stale lock, the previous owner
/// must not delete the new owner's lock.
struct SettingsFileLock {
    path: PathBuf,
    token: String,
}

impl Drop for SettingsFileLock {
    fn drop(&mut self) {
        if fs::read_to_string(&self.path)
            .ok()
            .is_some_and(|contents| contents == self.token)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Acquire an exclusive advisory lock on `path` (a sibling `<path>.lock`
/// marker, upstream `proper-lockfile` equivalent) and run `f`, releasing
/// afterwards. Retries up to 10 times with a 20ms delay before giving up and
/// running `f` anyway (the write is last-writer-wins, matching degraded
/// upstream behavior when locking is impossible). A lock left by a crashed
/// owner is taken over after [`SETTINGS_LOCK_STALE_AFTER`] instead of slowing
/// every later write for the rest of the installation's life. Shared with the
/// other agent-dir file stores (models-store, trust).
pub fn with_settings_file_lock<T>(path: &Path, f: impl FnOnce() -> T) -> T {
    with_file_lock(path, f)
}

pub(super) fn with_file_lock<T>(path: &Path, f: impl FnOnce() -> T) -> T {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static LOCK_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let lock_path = path.with_extension(format!(
        "{}lock",
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| format!("{ext}."))
            .unwrap_or_default()
    ));
    let token = format!(
        "{}-{}",
        std::process::id(),
        LOCK_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let mut guard: Option<SettingsFileLock> = None;
    for attempt in 0..SETTINGS_LOCK_ATTEMPTS {
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                if file.write_all(token.as_bytes()).is_ok() {
                    guard = Some(SettingsFileLock {
                        path: lock_path.clone(),
                        token,
                    });
                } else {
                    let _ = fs::remove_file(&lock_path);
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&lock_path)
                    .and_then(|metadata| metadata.modified())
                    .map(|modified| {
                        modified.elapsed().unwrap_or_default() > SETTINGS_LOCK_STALE_AFTER
                    })
                    .unwrap_or(false);
                if stale {
                    let _ = fs::remove_file(&lock_path);
                    continue;
                }
                if attempt + 1 < SETTINGS_LOCK_ATTEMPTS {
                    std::thread::sleep(SETTINGS_LOCK_RETRY_DELAY);
                }
            }
            Err(_) => break,
        }
    }
    let result = f();
    drop(guard);
    result
}

/// Write `contents` to `path` atomically: serialize into a same-directory
/// temp file, then rename it over the target. A crash mid-write leaves the
/// previous contents intact, and a concurrent reader (another process loading
/// settings) never observes a truncated document. On Unix the target's
/// existing permission bits are carried over to the replacement (a newly
/// created file otherwise gets the process default mode).
pub fn write_text_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    write_text_atomic_impl(path, contents, None)
}

/// [`write_text_atomic`] for a file that may contain secrets (`auth.json`):
/// a newly created file is `0o600` on Unix (an existing file keeps its mode).
pub fn write_private_text_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    write_text_atomic_impl(path, contents, Some(0o600))
}

fn write_text_atomic_impl(
    path: &Path,
    contents: &str,
    create_mode: Option<u32>,
) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings.json".to_owned());
    let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_path = parent.join(format!(".{file_name}.tmp-{}-{unique}", std::process::id()));
    let outcome = (|| {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mode = fs::metadata(path)
                .ok()
                .map(|metadata| {
                    use std::os::unix::fs::PermissionsExt;
                    metadata.permissions().mode() & 0o777
                })
                .or(create_mode)
                .unwrap_or(0o666);
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&temp_path)?;
            file.write_all(contents.as_bytes())?;
            file.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            let _ = create_mode;
            fs::write(&temp_path, contents)?;
        }
        // Same-directory rename is an atomic replace (POSIX rename; Windows
        // `MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`).
        fs::rename(&temp_path, path)
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    outcome
}

/// Canonicalize a path for identity comparisons, stripping the Windows `\\?\`
/// / `\\?\UNC\` extended-length prefixes that `fs::canonicalize` emits.
pub(super) fn canonicalize_lossy(path: &Path) -> PathBuf {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = canonical.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        canonical
    }
}
