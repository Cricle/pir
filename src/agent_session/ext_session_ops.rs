//! Host-backed `SessionManager` ops for the JS extension runtime
//! (`__pir_host.pirSession*` table, P1 collection).
//!
//! Upstream extensions reach the session tree two ways: the readonly
//! `ctx.sessionManager` snapshot (the prelude bridge over
//! `getSessionManagerJson`) and the SDK `SessionManager` class
//! (packages/coding-agent session-manager.ts), whose `open()` targets ANY
//! session file — not the live one. pir already owns a byte-compatible
//! session store ([`crate::session_manager::SessionManager`]); this module
//! exposes it to the shim over a small handle registry:
//!
//! - `pirSessionOpenJson` — the static constructors (`open` / `create` /
//!   `inMemory` / `continueRecent` / `forkFrom`), returning a handle plus the
//!   first snapshot;
//! - `pirSessionSnapshotJson` — the read face (entries, branch, tree, header,
//!   leaf, identity) that backs every `get*` method;
//! - `pirSessionCreateBranchedJson` — `createBranchedSession(leafId)`:
//!   forks the root-to-leaf path into a new session file under the manager's
//!   session dir and REPLACES the handle's manager in place (upstream
//!   semantics: the same object continues on the new file);
//! - `pirSessionFindByIdJson` / `pirSessionListJson` — discovery
//!   (`findById`, `list`, `listAll`);
//! - `pirSessionClose` — drops the handle (extensions open panels
//!   repeatedly; an unbounded registry would leak every parsed entry);
//! - `pirExecutablePath` — the running pir image path, so the shim's
//!   `createAgentSession` can spawn the CLI without PATH guesswork.
//!
//! All ops are synchronous file I/O on the runtime actor thread (the same
//! convention the fs bridge uses). The registry is process-global because the
//! ops are independent of any one runtime's `RuntimeState` — an opened
//! session file has no live-session binding.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use rquickjs::prelude::Func;
use rquickjs::Object;
use serde_json::Value as JsonValue;

use crate::session_manager::json_depth;
use crate::session_manager::SessionManager;

/// Maximum simultaneously-open extension session handles. Beyond the cap the
/// OLDEST handle is evicted (its next snapshot call reports the loss); the
/// extension consumers (pi-phone panels, pi-subagents forks) hold a handful
/// at a time, so this is a leak guard, not a working-set size.
const MAX_SESSION_HANDLES: usize = 64;

/// One open handle. The manager lives behind its OWN mutex: the process-global
/// table lock is taken only to look the handle up, so a deep snapshot or a
/// whole-file branch rewrite on one handle never blocks another handle's op
/// (or the same handle's close). The `Arc` also keeps an evicted/closed
/// handle's manager alive until an in-flight operation on it finishes.
type SessionHandle = Arc<StdMutex<SessionManager>>;

static SESSION_HANDLE_TABLE: StdMutex<BTreeMap<u64, SessionHandle>> =
    StdMutex::new(BTreeMap::new());
static SESSION_HANDLE_NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn with_table<R>(f: impl FnOnce(&mut BTreeMap<u64, SessionHandle>) -> R) -> R {
    let mut table = SESSION_HANDLE_TABLE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut table)
}

/// The `Arc` to one handle's manager, cloned under the table lock (released
/// before returning). Callers then lock the returned per-handle mutex for the
/// duration of their operation — never while holding the table lock.
fn get_session(handle: u64) -> Option<SessionHandle> {
    with_table(|table| table.get(&handle).cloned())
}

fn lock_session(session: &StdMutex<SessionManager>) -> std::sync::MutexGuard<'_, SessionManager> {
    session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn insert_session(session: SessionManager) -> u64 {
    with_table(|table| {
        while table.len() >= MAX_SESSION_HANDLES {
            // BTreeMap keys are ordered; the smallest id is the oldest handle.
            let Some(oldest) = table.keys().next().copied() else {
                break;
            };
            table.remove(&oldest);
        }
        let id = SESSION_HANDLE_NEXT_ID.fetch_add(1, Ordering::SeqCst);
        table.insert(id, Arc::new(StdMutex::new(session)));
        id
    })
}

fn take_session(handle: u64) -> Option<SessionHandle> {
    with_table(|table| table.remove(&handle))
}

/// The SDK session snapshot as JSON TEXT, with the nested tree rendered
/// iteratively.
///
/// Byte-identical to the former `serde_json::json!({...}).to_string()`: the
/// same keys in the same insertion order (`serde_json`'s `preserve_order` is
/// on), the same compact formatting, and every non-tree field still serialized
/// by `serde_json`. The `tree` field is the only deeply nested one — it nests
/// one node per entry, and routing it through a `Value` makes `serde_json`
/// recurse one serializer frame per entry (a ~1400-entry session overran a
/// 2 MiB thread), so the forest is written by
/// [`crate::session_manager::TreeNode::forest_to_json_iterative`] directly.
fn session_snapshot_json_text(session: &SessionManager) -> String {
    let entries = session.entries();
    let branch = session.branch_entries(None).unwrap_or_default();
    // The upstream `SessionHeader` carries the `type: "session"` tag (the
    // FileEntry variant marker pir keeps on the enum, not the inner struct).
    let mut header = serde_json::to_value(session.header()).unwrap_or(JsonValue::Null);
    if let Some(map) = header.as_object_mut() {
        map.entry("type".to_owned())
            .or_insert_with(|| JsonValue::String("session".to_owned()));
    }
    let mut out = String::with_capacity(1024 + entries.len() * 64);
    out.push('{');
    let mut first = true;
    json_depth::push_field(
        &mut out,
        &mut first,
        "isPersisted",
        &session.session_file().is_some(),
    );
    json_depth::push_field(&mut out, &mut first, "sessionId", &session.id());
    json_depth::push_field(
        &mut out,
        &mut first,
        "sessionFile",
        &session
            .session_file()
            .map(|path| path.to_string_lossy().into_owned()),
    );
    json_depth::push_field(
        &mut out,
        &mut first,
        "sessionDir",
        &session.session_dir().to_string_lossy(),
    );
    json_depth::push_field(&mut out, &mut first, "cwd", &session.cwd());
    json_depth::push_field(&mut out, &mut first, "leafId", &session.leaf_id());
    json_depth::push_field(&mut out, &mut first, "header", &header);
    json_depth::push_field(&mut out, &mut first, "entries", &entries);
    json_depth::push_field(&mut out, &mut first, "branch", &branch);
    // Nine fields precede the tree, so a separator is always due here (the
    // tree is written directly because of its nesting, see above).
    out.push(',');
    out.push_str("\"tree\":");
    out.push_str(&crate::session_manager::TreeNode::forest_to_json_iterative(
        session.tree(),
    ));
    out.push('}');
    out
}

/// One `{ok:false, error}` envelope for the JS bridge.
fn error_envelope(message: String) -> String {
    serde_json::json!({ "ok": false, "error": message }).to_string()
}

/// `pirSessionOpenJson(args)` — the static constructors. `args`:
/// `{kind, path?, sessionDir?, cwdOverride?, cwd?, source?, targetCwd?}` with
/// `kind` defaulting to `"open"`. Returns
/// `{ok, handle, snapshot}` or `{ok:false, error}`.
fn session_open_json(args: String) -> String {
    let parsed: JsonValue = match serde_json::from_str(&args) {
        Ok(value) => value,
        Err(error) => return error_envelope(format!("invalid session open args: {error}")),
    };
    let kind = parsed
        .get("kind")
        .and_then(JsonValue::as_str)
        .unwrap_or("open");
    let result = (|| -> crate::Result<u64> {
        let session = match kind {
            "open" => {
                let path = parsed
                    .get("path")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| {
                        anyhow::anyhow!("SessionManager.open requires a session file path")
                    })?;
                let session_dir = parsed
                    .get("sessionDir")
                    .and_then(JsonValue::as_str)
                    .map(std::path::PathBuf::from);
                let cwd_override = parsed.get("cwdOverride").and_then(JsonValue::as_str);
                // Upstream `open` resolves the path first and fails on a
                // non-session file; surface the same class of failure.
                SessionManager::open_with(path, session_dir.as_deref(), cwd_override)?
            }
            "create" => {
                let cwd = parsed.get("cwd").and_then(JsonValue::as_str).unwrap_or(".");
                let dir = parsed
                    .get("sessionDir")
                    .and_then(JsonValue::as_str)
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| crate::session_manager::default_session_dir(cwd));
                SessionManager::create(cwd, dir)?
            }
            "inMemory" => {
                let cwd = parsed.get("cwd").and_then(JsonValue::as_str).unwrap_or(".");
                SessionManager::in_memory(cwd, None)?
            }
            "continueRecent" => {
                let cwd = parsed.get("cwd").and_then(JsonValue::as_str).unwrap_or(".");
                let dir = parsed
                    .get("sessionDir")
                    .and_then(JsonValue::as_str)
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| crate::session_manager::default_session_dir(cwd));
                SessionManager::continue_recent(dir, cwd)?
            }
            "forkFrom" => {
                let source = parsed
                    .get("source")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| {
                        anyhow::anyhow!("SessionManager.forkFrom requires a source path")
                    })?;
                let target_cwd = parsed
                    .get("targetCwd")
                    .and_then(JsonValue::as_str)
                    .unwrap_or(".");
                let dir = parsed
                    .get("sessionDir")
                    .and_then(JsonValue::as_str)
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| crate::session_manager::default_session_dir(target_cwd));
                SessionManager::fork_from_source(
                    std::path::Path::new(source),
                    target_cwd,
                    &dir,
                    None,
                )?
            }
            other => {
                return Err(anyhow::anyhow!(
                    "unknown SessionManager constructor kind: {other}"
                ))
            }
        };
        Ok(insert_session(session))
    })();
    match result {
        Ok(handle) => {
            // One bridge call carries the first snapshot: the shim reads the
            // face from it without a second round-trip. The snapshot runs
            // under the handle's OWN lock — the table lock was released when
            // the handle was looked up, so no other handle waits on it.
            let snapshot = match get_session(handle) {
                Some(session) => session_snapshot_json_text(&lock_session(&session)),
                None => String::from("null"),
            };
            format!("{{\"ok\":true,\"handle\":{handle},\"snapshot\":{snapshot}}}")
        }
        Err(error) => error_envelope(error.to_string()),
    }
}

/// `pirSessionSnapshotJson(handle)` — the read face. Returns
/// `{ok, snapshot}`; an evicted/closed handle reports `{ok:false, ...}`.
/// The deep entries/branch/tree read and the JSON build run under the
/// HANDLE lock only (see [`SessionHandle`]); the process-global table lock is
/// not held.
fn session_snapshot_json(handle: f64) -> String {
    let handle = handle as u64;
    match get_session(handle) {
        Some(session) => {
            let session = lock_session(&session);
            format!(
                "{{\"ok\":true,\"snapshot\":{}}}",
                session_snapshot_json_text(&session)
            )
        }
        None => error_envelope(format!("SessionManager handle {handle} is closed")),
    }
}

/// `pirSessionCreateBranchedJson(handle, leafId)` — upstream
/// `createBranchedSession(leafId)`: fork the root-to-leaf path into a new
/// session file under the manager's session dir and REPLACE the handle's
/// manager in place (the same instance continues on the new file, so a
/// subsequent snapshot reflects the branched session). Returns
/// `{ok, sessionFile}` — `sessionFile` is `null` for in-memory managers
/// (upstream returns `undefined`).
///
/// The whole-file rewrite + fsync + rename runs under the HANDLE lock, not
/// the process-global table lock; `create_branched_session` refuses a paged
/// manager at entry (its resident window is not the session).
fn session_create_branched_json(handle: f64, leaf_id: String) -> String {
    let handle = handle as u64;
    let result = match get_session(handle) {
        Some(session) => {
            let mut session = lock_session(&session);
            session
                .create_branched_session(&leaf_id)
                .map(|path| path.map(|path| path.to_string_lossy().into_owned()))
        }
        None => Err(anyhow::anyhow!("SessionManager handle {handle} is closed")),
    };
    match result {
        Ok(Some(file)) => serde_json::json!({ "ok": true, "sessionFile": file }).to_string(),
        Ok(None) => serde_json::json!({ "ok": true, "sessionFile": JsonValue::Null }).to_string(),
        Err(error) => error_envelope(error.to_string()),
    }
}

/// `pirSessionFindByIdJson({cwd, id, sessionDir?})` — upstream
/// `SessionManager.findById(cwd, id, sessionDir?)`: the matching session
/// file path, or `null`.
fn session_find_by_id_json(args: String) -> String {
    let parsed: JsonValue = match serde_json::from_str(&args) {
        Ok(value) => value,
        Err(error) => return error_envelope(format!("invalid findById args: {error}")),
    };
    let cwd = parsed.get("cwd").and_then(JsonValue::as_str).unwrap_or(".");
    let id = match parsed.get("id").and_then(JsonValue::as_str) {
        Some(id) => id,
        None => return error_envelope("SessionManager.findById requires a session id".to_owned()),
    };
    let dir = parsed
        .get("sessionDir")
        .and_then(JsonValue::as_str)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| crate::session_manager::default_session_dir(cwd));
    let found = SessionManager::find_session_by_id(&dir, id)
        .map(|path| path.to_string_lossy().into_owned());
    serde_json::json!({ "ok": true, "path": found }).to_string()
}

/// `pirSessionListJson({kind, cwd?, sessionDir?})` — upstream `list` (one
/// directory) and `listAll` (every project directory under the sessions
/// root). Returns `{ok, sessions}` with the upstream `SessionInfo` fields
/// pir's discovery carries (`modified` as epoch milliseconds; `created` is
/// not collected by the header+peek scan and stays `null`).
fn session_list_json(args: String) -> String {
    let parsed: JsonValue = match serde_json::from_str(&args) {
        Ok(value) => value,
        Err(error) => return error_envelope(format!("invalid list args: {error}")),
    };
    let kind = parsed
        .get("kind")
        .and_then(JsonValue::as_str)
        .unwrap_or("list");
    let cwd = parsed
        .get("cwd")
        .and_then(JsonValue::as_str)
        .map(str::to_owned);
    let summarize = |sessions: Vec<crate::session_manager::SessionSummary>| -> Vec<JsonValue> {
        sessions
            .into_iter()
            .map(|summary| {
                let modified_ms = summary
                    .modified
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()
                    .map(|duration| duration.as_millis() as u64);
                serde_json::json!({
                    "path": summary.path.to_string_lossy(),
                    "id": summary.id,
                    "cwd": summary.cwd,
                    "name": summary.name,
                    "parentSessionPath": summary.parent_session_path,
                    "created": JsonValue::Null,
                    "modifiedMs": modified_ms,
                    "messageCount": summary.message_count,
                    "firstMessage": summary.first_message,
                })
            })
            .collect()
    };
    match kind {
        "list" => {
            let dir = match parsed.get("sessionDir").and_then(JsonValue::as_str) {
                Some(dir) => std::path::PathBuf::from(dir),
                None => match &cwd {
                    Some(cwd) => crate::session_manager::default_session_dir(cwd),
                    None => return error_envelope("SessionManager.list requires cwd".to_owned()),
                },
            };
            let sessions = SessionManager::list_sessions(&dir, cwd.as_deref());
            serde_json::json!({ "ok": true, "sessions": summarize(sessions) }).to_string()
        }
        "listAll" => {
            let mut all = Vec::new();
            let root = crate::session_manager::sessions_dir();
            let Ok(entries) = std::fs::read_dir(&root) else {
                return serde_json::json!({ "ok": true, "sessions": [] }).to_string();
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                all.extend(SessionManager::list_sessions(&path, None));
            }
            all.sort_by_key(|session| std::cmp::Reverse(session.modified));
            serde_json::json!({ "ok": true, "sessions": summarize(all) }).to_string()
        }
        other => error_envelope(format!("unknown session list kind: {other}")),
    }
}

/// `pirSessionClose(handle)` — drop the handle.
fn session_close(handle: f64) {
    let _ = take_session(handle as u64);
}

/// The running pir image path (`std::env::current_exe`), the spawn target for
/// the shim's `createAgentSession` child sessions.
fn executable_path() -> String {
    std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "pir".to_owned())
}

/// Register the `pirSession*` ops on the `__pir_host` table (called once from
/// the host function table setup; the ops are runtime-state-independent).
pub fn register_ext_session_ops(host: &Object) -> rquickjs::Result<()> {
    host.set("pirSessionOpenJson", Func::from(session_open_json))?;
    host.set("pirSessionSnapshotJson", Func::from(session_snapshot_json))?;
    host.set(
        "pirSessionCreateBranchedJson",
        Func::from(session_create_branched_json),
    )?;
    host.set(
        "pirSessionFindByIdJson",
        Func::from(session_find_by_id_json),
    )?;
    host.set("pirSessionListJson", Func::from(session_list_json))?;
    host.set("pirSessionClose", Func::from(session_close))?;
    host.set("pirExecutablePath", Func::from(executable_path))?;
    Ok(())
}
