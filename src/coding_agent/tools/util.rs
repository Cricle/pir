//! Shared helpers for the core tools: workspace path confinement, bounded
//! source-file reads, and atomic local writes.
//!
//! Compiled under the union of its users (`tool-read`, `tool-write`,
//! `tool-edit`, `tool-ls`, `grep-find`) and `tui`: the TUI edit preview uses
//! the same path/limits helpers. A partial build leaves the helpers of the
//! compiled-out tools unreachable, so dead-code analysis is relaxed whenever
//! the full user set is not present.
#![cfg_attr(
    not(all(
        feature = "tool-read",
        feature = "tool-write",
        feature = "tool-edit",
        feature = "tool-ls",
        feature = "grep-find"
    )),
    allow(dead_code)
)]

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::ai::types::AbortSignal;

/// The read-before-edit gate for one mutation target (`edit` / `write`).
///
/// The ledger lives in the session environment; a bare `ToolContext` (tool
/// tests, embedders without a session) has none and keeps the pre-existing
/// behavior. Only an EXISTING regular file is gated: a new-file write is
/// always allowed, and a path that the tool's own confinement check will
/// reject later is not pre-empted here. Remote arms never reach a local file
/// through this helper, so their behavior is unchanged.
pub(super) fn read_before_edit_gate(
    ctx: &crate::agent::tool_execution::ToolContext,
    path: &str,
    action: &str,
) -> crate::agent::read_ledger::EditGate {
    let Some(session_env) = ctx.session_env.as_ref() else {
        return crate::agent::read_ledger::EditGate::Allowed;
    };
    let Ok(parts) = workspace_path_parts(&ctx.cwd, path) else {
        // The tool branch reports the confinement error with its established
        // wording; do not replace it with a read-ledger refusal.
        return crate::agent::read_ledger::EditGate::Allowed;
    };
    match std::fs::metadata(&parts.candidate) {
        Ok(metadata) if metadata.is_file() => session_env
            .read_ledger
            .gate_for_edit(&parts.candidate, action),
        // New file (or a non-file target whose own error the tool reports).
        _ => crate::agent::read_ledger::EditGate::Allowed,
    }
}

/// Attach a [`crate::agent::read_ledger::EditGate::Warn`] warning to a
/// successful tool result (model-facing content and, when present, the
/// structured string). A failed result keeps its own error untouched.
pub(super) fn append_read_gate_warning(
    mut result: crate::agent::tool_batch::ToolResult,
    warning: Option<String>,
) -> crate::agent::tool_batch::ToolResult {
    let Some(warning) = warning else {
        return result;
    };
    if result.is_error {
        return result;
    }
    if !result.content.is_empty() {
        result.content.push_str("\n\n");
    }
    result.content.push_str(&warning);
    if let Some(serde_json::Value::String(text)) = result.structured_content.as_mut() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&warning);
    }
    result
}

/// Snapshot a mutation tool's target paths before the mutation.
///
/// The snapshot reads whole files and hashes/writes them, so it runs on
/// `tokio`'s blocking pool: a large file must not stall the async runtime while
/// the tool awaits it. The per-file ceiling makes an oversized target fail the
/// snapshot cleanly (no partial checkpoint is published); like every other
/// checkpoint failure, an error is logged as a warning and the mutation
/// proceeds. Sessions without a session file (in-memory/read-only) have no
/// checkpoint store and are skipped.
pub(super) async fn checkpoint_before_mutation(
    ctx: &crate::agent::tool_execution::ToolContext,
    tool: &str,
    paths: &[PathBuf],
) {
    let Some(session_file) = ctx
        .session_env
        .as_ref()
        .and_then(|env| env.session_file.as_deref())
    else {
        return;
    };
    let Some(store) =
        crate::checkpoints::CheckpointStore::for_session_file(Path::new(session_file))
    else {
        return;
    };
    let tool_name = tool.to_owned();
    let log_tool = tool_name.clone();
    let paths = paths.to_vec();
    match tokio::task::spawn_blocking(move || store.snapshot(&tool_name, &paths)).await {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, tool = %log_tool, "file checkpoint failed; the mutation proceeds");
        }
        Err(error) => {
            tracing::warn!(%error, tool = %log_tool, "file checkpoint task failed; the mutation proceeds");
        }
    }
}

/// Snapshot one mutation target from bytes the caller already read and
/// validated.
///
/// `edit` uses this after a successful content validation so the checkpoint
/// records the exact pre-edit bytes the replacement was computed from, with no
/// second read of a file that could have changed in between. The caller passes
/// the original (pre-transformation) buffer. Like
/// [`checkpoint_before_mutation`], a snapshot failure is logged and the
/// mutation proceeds.
pub(super) async fn checkpoint_before_mutation_from_bytes(
    ctx: &crate::agent::tool_execution::ToolContext,
    tool: &str,
    path: &Path,
    content: Vec<u8>,
) {
    let Some(session_file) = ctx
        .session_env
        .as_ref()
        .and_then(|env| env.session_file.as_deref())
    else {
        return;
    };
    let Some(store) =
        crate::checkpoints::CheckpointStore::for_session_file(Path::new(session_file))
    else {
        return;
    };
    let tool_name = tool.to_owned();
    let log_tool = tool_name.clone();
    let path = path.to_path_buf();
    match tokio::task::spawn_blocking(move || {
        store.snapshot_with_content(&tool_name, &path, &content)
    })
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            tracing::warn!(%error, tool = %log_tool, "file checkpoint failed; the mutation proceeds");
        }
        Err(error) => {
            tracing::warn!(%error, tool = %log_tool, "file checkpoint task failed; the mutation proceeds");
        }
    }
}

/// Default ceiling for a tool read that must materialize the whole source
/// file (`edit`, image decode, grep context). Text reads from the `read` tool
/// stream instead and are not capped by this value.
///
/// `PIR_TOOL_MAX_SOURCE_BYTES` overrides it per process. A malformed or zero
/// value falls back to the default rather than disabling the guard.
pub(super) const DEFAULT_MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;

/// Process-unique sequence for atomic-write temp names. The UUID suffix makes
/// collisions (and pre-planted names) practically impossible; `create_new`
/// remains the actual protection.
static ATOMIC_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Extract the `path` argument shared by the single-file mutation tools
/// (write/edit): the raw string (for user-facing messages) plus the mutation
/// path candidate, where empty or absolute paths are used verbatim and
/// relative paths resolve against `cwd`. The candidate is normalized with
/// [`normalize_tool_path`] so two spellings of the same file (`@x`, `x`) map
/// to one scheduler mutation key; the raw string is kept for messages.
pub(super) fn single_path_arg(args: &serde_json::Value, cwd: &Path) -> (String, Vec<PathBuf>) {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let normalized = normalize_tool_path(&path);
    let candidate = if normalized.is_empty() || Path::new(normalized.as_str()).is_absolute() {
        PathBuf::from(normalized.as_str())
    } else {
        cwd.join(normalized.as_str())
    };
    (path, vec![candidate])
}

/// Whether `character` is one of the Unicode space variants tools normalize to
/// an ASCII space (upstream `utils/paths.ts:7`): no-break, the en/em quad
/// family, narrow no-break, medium mathematical, and ideographic spaces.
fn is_unicode_space(character: char) -> bool {
    matches!(
        character,
        '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

/// The user's home directory for `~` expansion (upstream `os.homedir()`).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            let mut home = PathBuf::from(drive);
            home.push(path);
            Some(home)
        })
}

/// Normalize a model-supplied tool path before workspace confinement
/// (upstream `normalizePath` with `normalizeUnicodeSpaces` + `stripAtPrefix`,
/// reached through `resolveToCwd` / `resolveReadPath`):
///
/// * Unicode space variants become ASCII spaces,
/// * one leading CLI `@` is stripped (`@file` paths),
/// * `~` / `~/x` / `~\x` expand against the home directory.
///
/// This only normalizes the spelling. The result still goes through
/// [`workspace_path_parts`], so an expanded home outside the workspace is
/// refused exactly like any other outside path.
pub(super) fn normalize_tool_path(requested: &str) -> String {
    let normalized: String = requested
        .chars()
        .map(|character| {
            if is_unicode_space(character) {
                ' '
            } else {
                character
            }
        })
        .collect();
    let normalized = normalized.strip_prefix('@').unwrap_or(&normalized);
    if normalized == "~" {
        if let Some(home) = home_dir() {
            return home.to_string_lossy().into_owned();
        }
        return normalized.to_owned();
    }
    if let Some(rest) = normalized.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    #[cfg(windows)]
    if let Some(rest) = normalized.strip_prefix("~\\") {
        if let Some(home) = home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    // The bash tool runs under MSYS/Git Bash, whose `pwd` and absolute paths
    // use POSIX spellings. The file tools resolve native Windows paths, so a
    // model that copies a `/tmp/...` or `/c/...` path from bash would
    // otherwise be told it escaped the workspace. Map those two stable
    // spellings back to their Windows equivalents; a `..` is still rejected
    // by the confinement walk afterwards.
    #[cfg(windows)]
    {
        if normalized == "/tmp" {
            return std::env::temp_dir().to_string_lossy().into_owned();
        }
        if let Some(rest) = normalized.strip_prefix("/tmp/") {
            return std::env::temp_dir()
                .join(rest)
                .to_string_lossy()
                .into_owned();
        }
        if let Some(rest) = normalized.strip_prefix("/c/") {
            return format!("C:/{rest}");
        }
        if let Some(rest) = normalized
            .strip_prefix("/mnt/c/")
            .or_else(|| normalized.strip_prefix("/cygdrive/c/"))
        {
            return format!("C:/{rest}");
        }
    }
    normalized.to_owned()
}

/// The remote-side subset of [`normalize_tool_path`]: Unicode spaces and a
/// leading `@`, but no `~` expansion (the remote environment's home directory
/// is not this process's home, so expanding locally would name the wrong
/// file).
pub(super) fn normalize_tool_path_remote(requested: &str) -> String {
    let normalized: String = requested
        .chars()
        .map(|character| {
            if is_unicode_space(character) {
                ' '
            } else {
                character
            }
        })
        .collect();
    normalized
        .strip_prefix('@')
        .unwrap_or(&normalized)
        .to_owned()
}

/// Resolve a tool path below the context's configured workspace root.
///
/// Absolute paths remain supported for compatibility, but must resolve inside
/// `cwd`. Parent-directory components are rejected even when they would
/// eventually land back inside the workspace. This conservative rule also
/// avoids ambiguity for paths that do not exist yet (notably write targets).
///
/// Every component below the root is inspected with `symlink_metadata`
/// (`lstat`), so a symbolic link in any position — including a dangling final
/// link that `canonicalize` cannot resolve — is refused. Writing through such
/// a link would otherwise let `create_dir_all`/`fs::write` escape the
/// workspace even though the longest existing prefix canonicalized inside it.
///
/// The name-based check here is necessary but not sufficient on its own: it can
/// race a concurrent component swap. The mutation tools therefore re-establish
/// it against open handles through
/// [`ConfinedTarget`](super::confine::ConfinedTarget) before they touch the
/// file; this function still owns the model-facing errors.
pub(super) fn workspace_path(cwd: &Path, requested: &str) -> Result<PathBuf, String> {
    workspace_path_parts(cwd, requested).map(|parts| parts.candidate)
}

/// The three views one mutation target needs:
/// * `root` — the canonical workspace root (the handle anchor),
/// * `candidate` — the requested spelling joined to the root (the path the
///   read-only tools use),
/// * `relative` — the components below the root, which is what the
///   handle-relative walk consumes (it never re-resolves `candidate`).
///
/// `#[doc(hidden)] pub`: the return type of the [`workspace_path_parts`] test
/// seam below.
#[doc(hidden)]
pub struct WorkspacePathParts {
    #[doc(hidden)]
    pub root: PathBuf,
    #[doc(hidden)]
    pub candidate: PathBuf,
    #[doc(hidden)]
    pub relative: PathBuf,
}

/// [`workspace_path`], also returning the canonical root and the relative
/// component path the candidate was checked against.
///
/// `#[doc(hidden)] pub`: asserted directly by
/// `tests/suites/coding_agent_units.rs` (the shell-substitution refusal).
#[doc(hidden)]
pub fn workspace_path_parts(cwd: &Path, requested: &str) -> Result<WorkspacePathParts, String> {
    // Tool paths are literal filesystem paths, not shell input. A model that
    // copies `$(pwd)/file` from a bash command must be corrected instead of
    // creating a literal `$(pwd)` directory or reporting an ENOENT edit.
    if requested.contains("$(") || requested.contains("${") || requested.contains('`') {
        return Err(format!(
            "Path '{}' must be a literal filesystem path; do not use shell substitutions such as $(pwd) or backticks",
            requested
        ));
    }
    let normalized = normalize_tool_path(requested);
    workspace_path_parts_normalized(cwd, Path::new(&normalized), requested)
}

/// [`workspace_path_parts`] for an already-spelled path: the confinement walk
/// without the [`normalize_tool_path`] spelling pass. The read tool's macOS
/// compatibility variants use this so a variant's narrow no-break space or
/// decomposed characters are not normalized away again.
pub(super) fn workspace_path_parts_normalized(
    cwd: &Path,
    requested_path: &Path,
    requested: &str,
) -> Result<WorkspacePathParts, String> {
    if requested_path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!("Path '{}' escapes the workspace root", requested));
    }

    let root = std::fs::canonicalize(cwd)
        .map_err(|e| format!("Cannot access workspace root '{}': {}", cwd.display(), e))?;
    let candidate = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        root.join(requested_path)
    };

    let relative = relative_below_root(&root, &candidate, requested)?;
    reject_symlink_components(&root, &relative, requested)?;
    Ok(WorkspacePathParts {
        root,
        candidate,
        relative,
    })
}

/// The components of `candidate` below the canonical `root`.
fn relative_below_root(root: &Path, candidate: &Path, requested: &str) -> Result<PathBuf, String> {
    if let Ok(relative) = candidate.strip_prefix(root) {
        return Ok(relative.to_path_buf());
    }
    // Absolute tool paths are often spelled in the platform's native form
    // (`C:\ws\x`) while `canonicalize` returns a verbatim Windows path
    // (`\\?\C:\ws`). Strip the verbatim marker before the textual
    // compare so the normal case keeps the original component spelling (the
    // symlink walk below must see the names the user asked for).
    #[cfg(windows)]
    {
        let root_text = root.as_os_str().to_string_lossy();
        if let Some(native) = root_text.strip_prefix(r"\\?\") {
            if let Ok(relative) = candidate.strip_prefix(Path::new(native)) {
                return Ok(relative.to_path_buf());
            }
        }
    }
    // Aliased spellings (8.3 names, links, a differently-normalized root):
    // find the deepest ancestor -- INCLUDING the candidate itself -- whose
    // canonical form lies inside the root. Starting at `candidate.parent()`
    // missed every existing path equal to the root: its parent is above the
    // root, so the walk never found a prefix.
    let mut ancestor = Some(candidate);
    while let Some(path) = ancestor {
        if let Ok(canonical) = std::fs::canonicalize(path) {
            if let Ok(prefix) = canonical.strip_prefix(root) {
                let tail = candidate
                    .strip_prefix(path)
                    .map_err(|_| format!("Path '{}' escapes the workspace root", requested))?;
                let mut relative = prefix.to_path_buf();
                relative.push(tail);
                return Ok(relative);
            }
        }
        ancestor = path.parent();
    }
    Err(format!("Path '{}' escapes the workspace root", requested))
}

/// Reject a symbolic link in any component from the root down. A dangling
/// link is caught here too: `symlink_metadata` describes the link itself and
/// only fails with `NotFound` when the path component truly does not exist.
fn reject_symlink_components(root: &Path, relative: &Path, requested: &str) -> Result<(), String> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        match component {
            Component::Normal(name) => current.push(name),
            Component::CurDir => continue,
            _ => return Err(format!("Path '{}' escapes the workspace root", requested)),
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "Path '{}' must not traverse a symbolic link",
                    requested
                ));
            }
            Ok(_) => {}
            // The first missing component ends the walk: nothing below a
            // missing directory can exist. A dangling link is not `NotFound`
            // here, so it was rejected above.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(format!("Cannot access '{}': {}", requested, error)),
        }
    }
    Ok(())
}

/// Quote `value` as one POSIX shell word (single quotes, with embedded
/// quotes closed/reopened). Used when a tool suggests a `bash` command that
/// embeds a user-controlled path.
pub(super) fn bash_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Render a path for a MODEL-FACING message: the same spelling the caller and
/// the OS use natively, without the Windows verbatim marker.
///
/// The file tools resolve their target against `canonicalize(cwd)`, which on
/// Windows returns a `\\?\C:\…` path. Printing that prefix in `Path not found:
/// …` / `Not a directory: …` made pir's error text differ from upstream pi's
/// (`C:\…`) for every Windows path and pointed the model at a spelling Git
/// Bash tools (`sed`, `cat`) do not accept. `config::trust::normalize_cwd`
/// applies the same rule for workspace identity, so this mirrors it.
pub(super) fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    #[cfg(windows)]
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return rest.to_owned();
    }
    text.into_owned()
}

/// A path-stat error that preserves the useful OS distinction: a missing path
/// is not the same failure as an unreadable one (EACCES/ENOTDIR).
pub(super) fn path_error_message(path: &Path, error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => format!("Path not found: {}", display_path(path)),
        std::io::ErrorKind::PermissionDenied => {
            format!("Permission denied: {}", display_path(path))
        }
        std::io::ErrorKind::NotADirectory => format!("Not a directory: {}", display_path(path)),
        _ => format!("Cannot access '{}': {}", display_path(path), error),
    }
}

/// The remote form of the atomic local write [`ConfinedTarget::write_atomically`]
/// (formerly `write_atomically`): write a temp sibling, flush it through the
/// daemon, then rename it over the target.
///
/// Without this the remote arm streams chunks straight into the live file, so
/// an abort mid-write left a truncated file — the local arm has never allowed
/// that. The temp name mirrors the local one (dot-prefixed, process- and
/// UUID-tagged) so it is recognizably ours, and the daemon's `rename` maps to
/// `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` / POSIX `rename`, which replaces
/// atomically on both (same directory ⇒ same volume/filesystem). An abort or
/// failure before the rename leaves the original target untouched; the temp
/// file is removed best effort.
///
/// Known limitations: the daemon's `write` creates the temp with the process
/// default mode/ACL, and the protocol exposes no `stat` mode or `chmod`
/// operation, so the rename replaces the target's permissions/ACL with those
/// defaults. The local arm copies the target's permissions best effort; the
/// remote arm cannot. This is a metadata difference only — the content
/// replacement itself is atomic. A process crash after the temp write but
/// before the rename leaves a `.pir-write-*` sibling behind (there is no
/// remote temp sweep); it is never mistaken for the target. The rename is
/// atomic but, without a directory-fsync operation in the protocol, crash
/// durability of the name swap itself is not guaranteed. Remote path
/// confinement stays name/`lstat`-based: the daemon protocol has no
/// handle-relative open, so a component swapped on the remote machine between
/// the check and this temp write is outside what the client can close.
///
/// `Ok(())` is a completed replacement and `Err` the failure to report; the
/// abort cases surface as [`FileErrorCode::Aborted`](crate::env_client::FileErrorCode),
/// so the caller maps them to its own "Operation aborted" text.
pub(super) async fn write_remote_file_atomically(
    environment: &crate::env_client::RemoteExecutionEnv,
    file_path: &str,
    content: &[u8],
    context: &crate::env_client::Context,
) -> Result<(), crate::env_client::FileError> {
    use crate::env_client::{FileError, FileErrorCode};

    if context.aborted() {
        return Err(FileError::new(
            FileErrorCode::Aborted,
            "The operation was aborted",
            file_path.to_string(),
        ));
    }
    let paths = environment.paths().await.map_err(|error| {
        FileError::new(FileErrorCode::Unknown, error.message, file_path.to_string())
    })?;
    let directory = paths.dirname(file_path);
    let temp_path = paths.join(&[
        &directory,
        &format!(
            ".pir-write-{}-{}-{}",
            std::process::id(),
            ATOMIC_TEMP_SEQ.fetch_add(1, Ordering::Relaxed),
            uuid::Uuid::new_v4().simple()
        ),
    ]);
    if let Err(error) = environment.write_file(&temp_path, content, context).await {
        // A partial temp file is not worth keeping; the original target is
        // untouched either way.
        let _ = environment.remove(&temp_path, false, true, context).await;
        return Err(error);
    }
    if let Err(error) = environment.flush_file(&temp_path, context).await {
        let _ = environment.remove(&temp_path, false, true, context).await;
        return Err(error);
    }
    if context.aborted() {
        let _ = environment.remove(&temp_path, false, true, context).await;
        return Err(FileError::new(
            FileErrorCode::Aborted,
            "The operation was aborted",
            file_path.to_string(),
        ));
    }
    match environment
        .rename_file(&temp_path, file_path, context)
        .await
    {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = environment.remove(&temp_path, false, true, context).await;
            Err(error)
        }
    }
}

/// The remote form of the symlink-component rule: the remote
/// [`workspace_path_remote`](crate::env_client::workspace_path_remote) check
/// (parent components, canonicalized longest existing prefix, root
/// containment), followed by an `lstat` walk of every component below the
/// remote root. The daemon's `lstat` (`file_info`) sees a dangling link as a
/// link, exactly like the local `symlink_metadata` walk above.
pub(super) async fn workspace_path_remote_checked(
    environment: &crate::env_client::RemoteExecutionEnv,
    requested: &str,
) -> Result<String, String> {
    use crate::env_client::{FileErrorCode, FileKind, RemotePathFlavor};

    let candidate = crate::env_client::workspace_path_remote(environment, requested).await?;
    let paths = environment
        .paths()
        .await
        .map_err(|error| format!("Cannot access workspace root: {}", error.message))?;
    let root = environment
        .root()
        .await
        .map_err(|error| format!("Cannot access workspace root: {}", error.message))?;
    let root = paths.normalize(&root);
    let candidate = paths.normalize(&candidate);

    let components = |path: &str| -> Vec<String> {
        path.split(|character| paths.is_separator(character))
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let matches = |left: &str, right: &str| -> bool {
        if paths == RemotePathFlavor::Win32 {
            left.eq_ignore_ascii_case(right)
        } else {
            left == right
        }
    };

    let root_parts = components(&root);
    let candidate_parts = components(&candidate);
    if candidate_parts.len() < root_parts.len()
        || !candidate_parts
            .iter()
            .zip(&root_parts)
            .all(|(candidate, root)| matches(candidate, root))
    {
        return Err(format!("Path '{}' escapes the workspace root", requested));
    }

    let mut current = root;
    for part in &candidate_parts[root_parts.len()..] {
        current = paths.join(&[current.as_str(), part.as_str()]);
        let context = crate::env_client::Context::default();
        match environment.file_info(&current, &context).await {
            Ok(info) if info.kind == FileKind::Symlink => {
                return Err(format!(
                    "Path '{}' must not traverse a symbolic link",
                    requested
                ));
            }
            Ok(_) => {}
            Err(error) if error.code == FileErrorCode::NotFound => break,
            Err(error) => return Err(format!("Cannot access '{}': {}", requested, error.message)),
        }
    }
    Ok(candidate)
}

/// The configured ceiling for a whole-file source read. See
/// [`DEFAULT_MAX_SOURCE_BYTES`] for the default and the environment override.
pub(super) fn max_source_bytes() -> u64 {
    std::env::var("PIR_TOOL_MAX_SOURCE_BYTES")
        .ok()
        .as_deref()
        .and_then(parse_source_limit)
        .unwrap_or(DEFAULT_MAX_SOURCE_BYTES)
}

/// Parse a positive byte limit; zero, whitespace-only, and malformed values
/// are ignored so the guard cannot be disabled accidentally.
fn parse_source_limit(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok().filter(|bytes| *bytes > 0)
}

/// A consistent model-facing explanation for a source file over the limit.
pub(super) fn source_limit_error(size: u64) -> String {
    format!(
        "file is {size} bytes, exceeds the {} byte source-size limit (PIR_TOOL_MAX_SOURCE_BYTES)",
        max_source_bytes()
    )
}

/// A fresh same-directory temp name for an atomic replacement. The UUID suffix
/// makes collisions (and pre-planted names) practically impossible; the
/// platform creates the file with exclusive-create semantics, which remains the
/// actual protection.
pub(super) fn atomic_temp_name() -> String {
    format!(
        ".pir-write-{}-{}-{}",
        std::process::id(),
        ATOMIC_TEMP_SEQ.fetch_add(1, Ordering::Relaxed),
        uuid::Uuid::new_v4().simple()
    )
}

/// Race one filesystem await against the abort signal. `None` means the signal
/// fired first; otherwise the await's own result is returned unchanged.
pub(super) async fn cancellable<T>(
    signal: Option<&AbortSignal>,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        value = future => Some(value),
        _ = wait_for_abort(signal) => None,
    }
}

/// [`cancellable`] spelling used by the chunked atomic writer.
pub(super) async fn cancellable_for_write<T>(
    signal: Option<&AbortSignal>,
    future: impl std::future::Future<Output = T>,
) -> Option<T> {
    cancellable(signal, future).await
}

async fn wait_for_abort(signal: Option<&AbortSignal>) {
    match signal {
        Some(signal) => signal.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}
