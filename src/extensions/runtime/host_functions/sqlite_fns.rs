//! Host-side SQLite backend behind the primary dispatcher's `sqlite` module
//! (`__pir_host.node("sqlite", op, payloadJson) -> String`): the surface the
//! `node:sqlite` shim (`js/sqlite_shim.js.txt`, `DatabaseSync`/`StatementSync`)
//! and the `better-sqlite3` / `sqlite3` virtual package shims
//! (`js/better_sqlite3_shim.js.txt`, `js/sqlite3_shim.js.txt`) call
//! synchronously. This replaces the
//! former "permanent boundary" verdict — rusqlite's bundled amalgamation makes
//! real SQLite available in-process (FTS5 included, which context-mode's
//! driver probe requires).
//!
//! Same JSON envelope as `node_fns`: `{"ok":true,"value":...}` /
//! `{"ok":false,"code":...,"message":...}`.
//!
//! Permission model (mirrors fs): database opens are **read-global**,
//! **write-fenced** — a read-write open whose path lands outside the
//! effective roots (`node_fns::effective_roots`) is rejected with EACCES
//! before the file is touched; read-only opens (`readOnly`/`readonly`
//! options) skip the fence like fs reads. `:memory:` and the empty-path
//! private temporary database are always allowed.
//!
//! Statement lifecycle: the host owns `Connection` handles only; SQL strings
//! are re-prepared through rusqlite's per-connection statement cache on every
//! `run`/`get`/`all`/`columns` call. Handles live in a process-global table,
//! but a statement holds only its OWN connection's mutex — the table lock is
//! released after cloning the handle's `Arc<Mutex<Connection>>`, so statements
//! on different databases run concurrently. Parameter binding is
//! **explicit-index**
//! — the JS shim lexes the SQL's parameter list (`@name` / `:name` / `$name`
//! / `?` / `?NNN`) and sends `{index, value}` pairs, so all named-parameter
//! semantics (bare-name matching, unknown-name policy, mixed anonymous) stay
//! in JS where the Node behavior lives.
//!
//! JSON value tagging (BigInt/BLOB cannot ride JSON natively): out-of-band
//! integers travel as `{"$pirBig":"<digits>"}` and blobs as
//! `{"$pirBlob":"<base64>"}`; the JS side decodes both directions. INTEGER
//! cells are always tagged when the caller asked for BigInt reads, and
//! tagged-even-without-when they exceed the f64 safe range (the JS layer
//! raises Node's `ERR_OUT_OF_RANGE` in that case).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map as JsonMap, Value as JsonValue};

use super::node_fns::{normalize_for_check, path_within, NodeError};
use super::*;

type NodeResult = Result<JsonValue, NodeError>;

// ---------------------------------------------------------------------------
// Handle tables (same pattern as http_fns' fetch handles)
// ---------------------------------------------------------------------------

struct DbSlot {
    /// The connection is owned through an `Arc` so a statement keeps it alive
    /// without the process-global handle table lock: [`with_db`] clones the
    /// `Arc` under the table lock, releases the table, and only then locks the
    /// per-connection mutex for the statement. Two handles on DIFFERENT
    /// databases can therefore run statements concurrently.
    conn: Arc<StdMutex<Connection>>,
    readonly: bool,
    /// Runtime generation that opened this handle; released on teardown.
    generation: u64,
}

/// Hard cap on simultaneously open SQLite handles for one process.
const MAX_OPEN_DATABASES: usize = 256;

static DATABASES: OnceLock<StdMutex<HashMap<u64, DbSlot>>> = OnceLock::new();
static NEXT_DB_ID: AtomicU64 = AtomicU64::new(1);

fn databases() -> &'static StdMutex<HashMap<u64, DbSlot>> {
    DATABASES.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn take_db(id: u64) -> Result<DbSlot, NodeError> {
    databases()
        .lock()
        .map_err(|_| NodeError::new("EIO", "sqlite handle table poisoned"))?
        .remove(&id)
        .ok_or_else(|| NodeError::new("ERR_INVALID_STATE", "database is not open"))
}

/// The `Arc` to one handle's connection, cloned under the table lock (which is
/// released before returning). A concurrent `close`/eviction that removes the
/// slot afterwards cannot invalidate the returned connection: the `Arc` keeps
/// it alive until the operation finishes, and the deferred `Connection` drop
/// is what closes it.
fn db_connection(id: u64) -> Result<Arc<StdMutex<Connection>>, NodeError> {
    let map = databases()
        .lock()
        .map_err(|_| NodeError::new("EIO", "sqlite handle table poisoned"))?;
    map.get(&id)
        .map(|slot| Arc::clone(&slot.conn))
        .ok_or_else(|| NodeError::new("ERR_INVALID_STATE", "database is not open"))
}

fn with_db<T>(
    id: u64,
    f: impl FnOnce(&mut Connection) -> Result<T, NodeError>,
) -> Result<T, NodeError> {
    let conn = db_connection(id)?;
    // The table lock is no longer held: only this handle's connection lock is,
    // for the duration of the statement. Statements from independent handles
    // (and independent databases) never serialize on the global map.
    let mut conn = conn
        .lock()
        .map_err(|_| NodeError::new("EIO", "sqlite connection mutex poisoned"))?;
    f(&mut conn)
}

fn sqlite_error(error: &rusqlite::Error) -> NodeError {
    let message = match error {
        rusqlite::Error::SqliteFailure(_, message) => {
            message.clone().unwrap_or_else(|| error.to_string())
        }
        other => other.to_string(),
    };
    NodeError::new("ERR_SQLITE_ERROR", message)
}

// ---------------------------------------------------------------------------
// Value codecs
// ---------------------------------------------------------------------------

/// Decode one bind value from the payload. Tagged forms:
/// `{"$pirBig":"<digits>"}` (BigInt) and `{"$pirBlob":"<base64>"}` (Buffer/
/// TypedArray). Booleans are rejected — both Node and better-sqlite3 refuse
/// to bind them rather than silently coercing. TEXT borrows the payload:
/// `rusqlite`'s `ToSql for &str` binds exactly the bytes the owned form did,
/// so the per-parameter clone only copied the string.
enum BindValue<'a> {
    Int(i64),
    Real(f64),
    Text(&'a str),
    Blob(Vec<u8>),
    Null,
}

/// `index` is the one-based SQLite parameter index. It only ever appears in
/// error text, so it is formatted at the failure sites instead of into a
/// `String` on every bound parameter.
fn decode_bind(value: &JsonValue, index: usize) -> Result<BindValue<'_>, NodeError> {
    let invalid = |what: &str| {
        Err(NodeError::new(
            "ERR_INVALID_ARG_VALUE",
            format!("unsupported bind value at parameter {index}: {what}"),
        ))
    };
    match value {
        JsonValue::Null => Ok(BindValue::Null),
        JsonValue::Number(number) => {
            if let Some(int) = number.as_i64() {
                Ok(BindValue::Int(int))
            } else if let Some(real) = number.as_f64() {
                Ok(BindValue::Real(real))
            } else {
                invalid("non-finite number")
            }
        }
        JsonValue::String(text) => Ok(BindValue::Text(text.as_str())),
        JsonValue::Object(map) => {
            if let Some(digits) = map.get("$pirBig").and_then(JsonValue::as_str) {
                let parsed: i64 = digits.parse().map_err(|_| {
                    NodeError::new(
                        "ERR_OUT_OF_RANGE",
                        format!("bind value at parameter {index} exceeds the SQLite INTEGER range"),
                    )
                })?;
                return Ok(BindValue::Int(parsed));
            }
            if let Some(base64) = map.get("$pirBlob").and_then(JsonValue::as_str) {
                let bytes = STANDARD.decode(base64.as_bytes()).map_err(|error| {
                    NodeError::new("EINVAL", format!("invalid blob payload: {error}"))
                })?;
                return Ok(BindValue::Blob(bytes));
            }
            invalid("object (named-parameter objects must be resolved JS-side)")
        }
        JsonValue::Bool(_) => invalid("boolean (bind null/number/string/bigint/blob)"),
        JsonValue::Array(_) => invalid("array"),
    }
}

/// Encode one cell of a result row. `read_bigints` forces the tagged form for
/// INTEGER cells; without it, values outside the f64 safe range still travel
/// tagged so the JS layer can raise Node's `ERR_OUT_OF_RANGE`.
fn encode_cell(cell: ValueRef<'_>, read_bigints: bool) -> JsonValue {
    match cell {
        ValueRef::Null => JsonValue::Null,
        ValueRef::Integer(value) => {
            if read_bigints || !f64_safe_integer(value) {
                json!({ "$pirBig": value.to_string() })
            } else {
                json!(value)
            }
        }
        ValueRef::Real(value) => {
            // NaN/±Inf have no JSON form; Node surfaces them as null (JSON
            // serialization semantics) — matches what the bridge could carry.
            if value.is_finite() {
                json!(value)
            } else {
                JsonValue::Null
            }
        }
        ValueRef::Text(bytes) => JsonValue::String(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => json!({ "$pirBlob": STANDARD.encode(bytes) }),
    }
}

/// Whether an INTEGER survives a JS number round trip exactly (|v| <= 2^53).
fn f64_safe_integer(value: i64) -> bool {
    (-(1i64 << 53)..=(1i64 << 53)).contains(&value)
}

/// Encode a statement's numeric result fields (changes/lastInsertRowid) under
/// the same BigInt contract as cells.
fn encode_run_fields(changes: i64, last_rowid: i64, read_bigints: bool) -> JsonValue {
    let encode = |value: i64| -> JsonValue {
        if read_bigints || !f64_safe_integer(value) {
            json!({ "$pirBig": value.to_string() })
        } else {
            json!(value)
        }
    };
    json!({
        "changes": encode(changes),
        "lastInsertRowid": encode(last_rowid),
    })
}

// ---------------------------------------------------------------------------
// Open + permission fence
// ---------------------------------------------------------------------------

/// True for specifiers that never touch the filesystem (Node semantics):
/// `:memory:` and the empty string (private temporary database).
fn is_virtual_db_path(path: &str) -> bool {
    path.is_empty() || path == ":memory:"
}

// ---------------------------------------------------------------------------
// Op dispatcher
// ---------------------------------------------------------------------------

pub(super) fn sqlite_op(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    generation: u64,
    op: &str,
    payload: &JsonValue,
) -> NodeResult {
    match op {
        "open" => open(state, allowed_roots, payload, generation),
        "close" => close(payload),
        "exec" => exec(payload),
        "prepare" => prepare(payload),
        "run" => run(payload),
        "get" => query(payload, true),
        "all" => query(payload, false),
        "columns" => columns(payload),
        "status" => status(payload),
        other => Err(NodeError::new(
            "ENOSYS",
            format!("unknown sqlite host op `{other}`"),
        )),
    }
}

fn open(
    state: &Arc<StdMutex<RuntimeState>>,
    allowed_roots: &Arc<StdMutex<Vec<PathBuf>>>,
    payload: &JsonValue,
    generation: u64,
) -> NodeResult {
    let raw = super::node_fns::payload_str(payload, "path")?;
    let readonly = payload
        .get("readOnly")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false);
    let foreign_keys = payload
        .get("enableForeignKeyConstraints")
        .and_then(JsonValue::as_bool)
        .unwrap_or(true);
    let timeout_ms = payload
        .get("timeout")
        .and_then(JsonValue::as_f64)
        .unwrap_or(0.0)
        .max(0.0) as u64;

    let path = PathBuf::from(&raw);
    if !readonly && !is_virtual_db_path(&raw) {
        // Write-capable open: the database file (and its WAL/SHM siblings)
        // are writes — fence to the effective roots like an fs write target.
        let checked = normalize_for_check(&path);
        let roots = super::node_fns::effective_roots(state, allowed_roots);
        if !roots.iter().any(|root| path_within(&checked, root)) {
            return Err(NodeError::denied("open", &raw));
        }
    }

    let flags = if readonly {
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI
    };
    let conn = if is_virtual_db_path(&raw) {
        Connection::open_in_memory()
    } else {
        Connection::open_with_flags(&path, flags)
    }
    .map_err(|error| sqlite_error(&error))?;
    if !foreign_keys {
        // node:sqlite enables FK enforcement by default (SQLite's own
        // default is off); better-sqlite3 passes false explicitly.
        conn.execute_batch("PRAGMA foreign_keys = OFF")
            .map_err(|error| sqlite_error(&error))?;
    }
    if timeout_ms > 0 {
        conn.busy_timeout(Duration::from_millis(timeout_ms))
            .map_err(|error| sqlite_error(&error))?;
    }

    let id = NEXT_DB_ID.fetch_add(1, Ordering::Relaxed);
    let mut table = databases()
        .lock()
        .map_err(|_| NodeError::new("EIO", "sqlite handle table poisoned"))?;
    if table.len() >= MAX_OPEN_DATABASES {
        return Err(NodeError::new(
            "EMFILE",
            format!("sqlite handle limit reached ({MAX_OPEN_DATABASES})"),
        ));
    }
    table.insert(
        id,
        DbSlot {
            conn: Arc::new(StdMutex::new(conn)),
            readonly,
            generation,
        },
    );
    Ok(json!({ "id": id }))
}

/// Release every SQLite handle opened by a torn-down runtime generation.
pub(super) fn release_generation(generation: u64) {
    if let Ok(mut table) = databases().lock() {
        table.retain(|_, slot| slot.generation != generation);
    }
}

fn close(payload: &JsonValue) -> NodeResult {
    let id = payload_u64(payload, "id")?;
    let _slot = take_db(id)?;
    // Dropping the slot releases the table's `Arc`; the `Connection` closes
    // here when no statement is in flight. A running statement holds its own
    // `Arc` (taken by `with_db` before this removal), so the close cannot
    // invalidate it — the connection is dropped when that statement finishes.
    // Explicit `close()` failure semantics (busy WAL etc.) are best-effort —
    // Node surfaces close errors, but a clean drop keeps the exit-hook paths
    // context-mode registers safe.
    Ok(JsonValue::Null)
}

fn exec(payload: &JsonValue) -> NodeResult {
    let id = payload_u64(payload, "id")?;
    let sql = super::node_fns::payload_str(payload, "sql")?;
    with_db(id, |conn| {
        conn.execute_batch(&sql)
            .map_err(|error| sqlite_error(&error))?;
        Ok(JsonValue::Null)
    })
}

fn prepare(payload: &JsonValue) -> NodeResult {
    let id = payload_u64(payload, "id")?;
    let sql = super::node_fns::payload_str(payload, "sql")?;
    with_db(id, |conn| {
        // Validate now (Node throws on bad SQL at prepare time) through the
        // cache so later run/get/all calls reuse the compiled statement.
        conn.prepare_cached(&sql)
            .map_err(|error| sqlite_error(&error))?;
        Ok(JsonValue::Null)
    })
}

/// Bind the payload's `{index, value}` pairs onto a reset statement. The pairs
/// are read straight from the parsed payload (`&[JsonValue]`, no per-call deep
/// clone of the bind list), and values are borrowed where `rusqlite` can bind
/// them without owning them.
fn bind_all(stmt: &mut rusqlite::Statement<'_>, binds: &[JsonValue]) -> Result<(), NodeError> {
    for (position, bind) in binds.iter().enumerate() {
        let index = bind
            .get("index")
            .and_then(JsonValue::as_u64)
            .unwrap_or(position as u64 + 1) as usize;
        let value = bind.get("value").unwrap_or(&JsonValue::Null);
        match decode_bind(value, index)? {
            BindValue::Null => stmt.raw_bind_parameter(index, rusqlite::types::Null),
            BindValue::Int(int) => stmt.raw_bind_parameter(index, int),
            BindValue::Real(real) => stmt.raw_bind_parameter(index, real),
            BindValue::Text(text) => stmt.raw_bind_parameter(index, text),
            BindValue::Blob(bytes) => stmt.raw_bind_parameter(index, bytes),
        }
        .map_err(|error| sqlite_error(&error))?;
    }
    Ok(())
}

fn run(payload: &JsonValue) -> NodeResult {
    let id = payload_u64(payload, "id")?;
    let sql = super::node_fns::payload_str(payload, "sql")?;
    let read_bigints = payload_bool(payload, "readBigInts");
    let binds: &[JsonValue] = payload
        .get("binds")
        .and_then(JsonValue::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    with_db(id, |conn| {
        let mut stmt = conn
            .prepare_cached(&sql)
            .map_err(|error| sqlite_error(&error))?;
        bind_all(&mut stmt, binds)?;
        let changes = stmt.raw_execute().map_err(|error| sqlite_error(&error))?;
        let last_rowid = conn.last_insert_rowid();
        Ok(encode_run_fields(changes as i64, last_rowid, read_bigints))
    })
}

fn query(payload: &JsonValue, first_row_only: bool) -> NodeResult {
    let id = payload_u64(payload, "id")?;
    let sql = super::node_fns::payload_str(payload, "sql")?;
    let read_bigints = payload_bool(payload, "readBigInts");
    let return_arrays = payload_bool(payload, "returnArrays");
    let binds: &[JsonValue] = payload
        .get("binds")
        .and_then(JsonValue::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    with_db(id, |conn| {
        let mut stmt = conn
            .prepare_cached(&sql)
            .map_err(|error| sqlite_error(&error))?;
        bind_all(&mut stmt, binds)?;
        let column_count = stmt.column_count();
        let mut names: Vec<String> = Vec::with_capacity(column_count);
        for index in 0..column_count {
            names.push(
                stmt.column_name(index)
                    .map(str::to_owned)
                    .unwrap_or_else(|_| format!("column{index}")),
            );
        }
        let mut rows = stmt.raw_query();
        let mut collected: Vec<JsonValue> = Vec::new();
        while let Some(row) = rows.next().map_err(|error| sqlite_error(&error))? {
            let mut row_out: JsonValue = if return_arrays {
                JsonValue::Array(Vec::with_capacity(column_count))
            } else {
                JsonValue::Object(JsonMap::new())
            };
            for (index, name) in names.iter().enumerate().take(column_count) {
                let cell = row.get_ref(index).map_err(|error| sqlite_error(&error))?;
                let encoded = encode_cell(cell, read_bigints);
                if return_arrays {
                    if let JsonValue::Array(list) = &mut row_out {
                        list.push(encoded);
                    }
                } else if let JsonValue::Object(map) = &mut row_out {
                    map.insert(name.clone(), encoded);
                }
            }
            collected.push(row_out);
            if first_row_only {
                break;
            }
        }
        if first_row_only {
            return Ok(collected.into_iter().next().unwrap_or(JsonValue::Null));
        }
        Ok(JsonValue::Array(collected))
    })
}

fn columns(payload: &JsonValue) -> NodeResult {
    let id = payload_u64(payload, "id")?;
    let sql = super::node_fns::payload_str(payload, "sql")?;
    with_db(id, |conn| {
        let stmt = conn
            .prepare_cached(&sql)
            .map_err(|error| sqlite_error(&error))?;
        let mut out: Vec<JsonValue> = Vec::new();
        for index in 0..stmt.column_count() {
            let name = stmt
                .column_name(index)
                .map(str::to_owned)
                .unwrap_or_else(|_| String::new());
            // `column`/`table`/`database` origin metadata is only available
            // via the C column-database-name APIs rusqlite does not expose;
            // Node's StatementSync.columns() consumers read `name` and
            // `type`/nullability at most, and context-mode reads nothing.
            out.push(json!({
                "column": null,
                "database": null,
                "name": name,
                "table": null,
                "type": null,
            }));
        }
        Ok(JsonValue::Array(out))
    })
}

fn status(payload: &JsonValue) -> NodeResult {
    let id = payload_u64(payload, "id")?;
    // One table-lock acquisition reads both fields: a two-step lookup (read
    // `readonly`, release, then [`with_db`]) leaves a window in which a
    // concurrent `close` removes the slot, so status observes the database as
    // open and then fails with "database is not open". The `Arc` clone keeps
    // the connection valid after the table lock is released.
    let (conn, readonly) = {
        let map = databases()
            .lock()
            .map_err(|_| NodeError::new("EIO", "sqlite handle table poisoned"))?;
        let slot = map
            .get(&id)
            .ok_or_else(|| NodeError::new("ERR_INVALID_STATE", "database is not open"))?;
        (Arc::clone(&slot.conn), slot.readonly)
    };
    let conn = conn
        .lock()
        .map_err(|_| NodeError::new("EIO", "sqlite connection mutex poisoned"))?;
    Ok(json!({
        "autocommit": conn.is_autocommit(),
        "readonly": readonly,
    }))
}

fn payload_u64(payload: &JsonValue, key: &str) -> Result<u64, NodeError> {
    payload
        .get(key)
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| NodeError::new("EINVAL", format!("missing number payload field `{key}`")))
}

fn payload_bool(payload: &JsonValue, key: &str) -> bool {
    payload
        .get(key)
        .and_then(JsonValue::as_bool)
        .unwrap_or(false)
}
