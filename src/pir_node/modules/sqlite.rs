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
//! `run`/`get`/`all`/`columns` call (the cache is per handle, so it behaves
//! like better-sqlite3's prepared-statement reuse; it dies with the handle on
//! `close`). Handles live in a process-global table, but a statement holds only
//! its OWN connection's mutex — the table lock is released after cloning the
//! handle's `Arc<Mutex<Connection>>`, so statements on different databases run
//! concurrently. Parameter binding is **explicit-index**: the JS shim lexes the
//! SQL's parameter list (`@name` / `:name` / `$name` / `?` / `?NNN`) and sends
//! flat value/index arrays, so all named-parameter semantics (bare-name
//! matching, unknown-name policy, mixed anonymous) stay in JS where the Node
//! behavior lives.
//!
//! JSON value tagging (BigInt/BLOB cannot ride JSON natively): out-of-band
//! integers travel as `{"$pirBig":"<digits>"}` and blobs as
//! `{"$pirBlob":"<base64>"}`; the JS side decodes both directions. INTEGER
//! cells are always tagged when the caller asked for BigInt reads, and
//! tagged-even-without-when they exceed the f64 safe range (the JS layer
//! raises Node's `ERR_OUT_OF_RANGE` in that case).
//!
//! # Fast path (columnar transfer + flat bindings)
//!
//! The original wire format materialized one JSON object per row with the
//! column names repeated in every row, and one `{index,value}` object per
//! bound parameter. Opt-in request flags remove that repetition; callers
//! that omit them keep the exact legacy shape (stale AOT shim blobs keep
//! working):
//!
//! - `"columnar": true` on `get`/`all` returns
//!   `{"columns":[names],"rows":[[cell,...],...]}` (or `{"columns":[...],
//!   "row":[...]|null}` for `get`). Column names cross once per call instead
//!   of once per row; the JS shim assembles the row objects (or hands the
//!   arrays straight back for `setReturnArrays`).
//! - `"values":[...]` (plus optional `"indices":[...]`) on
//!   `run`/`get`/`all` binds positional values in one flat array. `indices`
//!   is only needed when the bound set is not exactly `1..=values.len()`
//!   (named-parameter mode with gaps). `"binds":[{"index","value"}]` remains
//!   the fallback.
//! - `"jsonText": true` (with `"columnar"`) serializes the columnar result
//!   and returns one JSON string; the shim `JSON.parse`s it. Crossing a
//!   multi-row result as one string is measurably faster than constructing
//!   every row/cell through the rquickjs object bridge (~2.2x on 1000x5
//!   reads); the live-object form stays the default.
//!
//! Statement metadata (column names) is read from the live prepared statement
//! on every call: a schema change recompiles the statement inside SQLite
//! (`sqlite3_prepare_v2` semantics, which rusqlite's cached statements keep),
//! and reading the names from the statement that produced the rows keeps row
//! keys correct even when another connection altered the schema. The
//! per-connection compiled-statement cache is what makes repeated calls cheap;
//! its capacity is raised at open time because better-sqlite3 retains every
//! prepared statement for the life of a `Database`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use rusqlite::config::DbConfig;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map as JsonMap, Value as JsonValue};

use crate::pir_node::bridge::{normalize_for_check, path_within, payload_str};
use crate::pir_node::error::{NodeError, NodeResult};
use crate::pir_node::runtime::NodeRuntime;

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

/// Compiled-statement cache capacity per connection (rusqlite default is 16).
/// better-sqlite3 reuses every prepared statement for the life of the
/// `Database`, so give the cache room for a typical application's statement
/// set.
const STATEMENT_CACHE_CAPACITY: usize = 128;

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

/// The `Arc` to one handle's connection, cloned under the table lock (which
/// is released before returning). A concurrent `close`/eviction that removes
/// the slot afterwards cannot invalidate the returned connection: the `Arc`
/// keeps it alive until the operation finishes, and the deferred `Connection`
/// drop is what closes it.
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

/// Column names as SQLite reports them for the prepared statement (the names
/// belong to the statement that produced the rows). `column_name` only fails
/// for an out-of-range index, which never happens here.
fn statement_columns(stmt: &rusqlite::Statement<'_>) -> Vec<String> {
    let count = stmt.column_count();
    let mut names = Vec::with_capacity(count);
    for index in 0..count {
        names.push(
            stmt.column_name(index)
                .map(str::to_owned)
                .unwrap_or_else(|_| format!("column{index}")),
        );
    }
    names
}

pub fn sqlite_error(error: &rusqlite::Error) -> NodeError {
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
        // node:sqlite binds every JavaScript `number` through
        // `sqlite3_bind_double` (probed on v24: `SELECT typeof(?)` with `42`
        // answers `real`), and better-sqlite3 does the same. INTEGER-affinity
        // columns still convert an integral REAL losslessly, so the observable
        // row values are unchanged while expressions see the same storage
        // class Node stores.
        JsonValue::Number(number) => match number.as_f64() {
            Some(real) => Ok(BindValue::Real(real)),
            None => invalid("non-finite number"),
        },
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

/// Whether an INTEGER survives a JS number round trip exactly
/// (|v| <= Number.MAX_SAFE_INTEGER = 2^53 - 1).
fn f64_safe_integer(value: i64) -> bool {
    const LIMIT: i64 = (1i64 << 53) - 1;
    (-LIMIT..=LIMIT).contains(&value)
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

/// `jsonText` mode: serialize the query result here and let the JS shim
/// `JSON.parse` it. A large result then crosses the C-ABI as one string
/// instead of thousands of individually constructed QuickJS objects/arrays;
/// the engine's own JSON parser does the tree building. Callers that omit the
/// flag get the live object (the default).
fn query_result(value: JsonValue, json_text: bool) -> NodeResult {
    if !json_text {
        return Ok(value);
    }
    let text = serde_json::to_string(&value)
        .map_err(|error| NodeError::new("EIO", format!("sqlite result encode failed: {error}")))?;
    Ok(JsonValue::String(text))
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

pub fn sqlite_op(runtime: &NodeRuntime, op: &str, payload: &JsonValue) -> NodeResult {
    let generation = runtime.context.generation();
    match op {
        "open" => open(runtime, payload, generation),
        "close" => close(payload),
        "exec" => exec(payload),
        "prepare" => prepare(payload),
        "run" => run(payload),
        "get" => query(payload, true),
        "all" => query(payload, false),
        "columns" => columns(payload),
        "expanded" => expanded(payload),
        "status" => status(payload),
        other => Err(NodeError::new(
            "ENOSYS",
            format!("unknown sqlite host op `{other}`"),
        )),
    }
}

fn open(runtime: &NodeRuntime, payload: &JsonValue, generation: u64) -> NodeResult {
    let raw = payload_str(payload, "path");
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
    if !readonly && !is_virtual_db_path(raw) {
        // Write-capable open: the database file (and its WAL/SHM siblings)
        // are writes — fence to the effective roots like an fs write target.
        let checked = normalize_for_check(&path);
        let roots = runtime.effective_roots();
        if !roots.iter().any(|_root| path_within(&checked, &roots)) {
            return Err(NodeError::denied("open", raw));
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
    let conn = if is_virtual_db_path(raw) {
        Connection::open_in_memory()
    } else {
        Connection::open_with_flags(&path, flags)
    }
    .map_err(|error| sqlite_error(&error))?;
    // better-sqlite3 compiles a statement once per `Database` and reuses it for
    // every `.run/.get/.all/.iterate`; `prepare_cached` is the rusqlite
    // equivalent, but its default capacity (16) is easy to thrash.
    conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
    if !foreign_keys {
        // node:sqlite enables FK enforcement by default (SQLite's own
        // default is off); better-sqlite3 passes false explicitly.
        conn.execute_batch("PRAGMA foreign_keys = OFF")
            .map_err(|error| sqlite_error(&error))?;
    }
    // node:sqlite disables double-quoted string literals by default (v24
    // probes `SELECT "foo"` as `no such column: "foo"`). `None` keeps SQLite's
    // build default so the better-sqlite3 shim, which does not send the flag,
    // is unaffected.
    if let Some(enabled) = payload
        .get("enableDoubleQuotedStringLiterals")
        .and_then(JsonValue::as_bool)
    {
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DML, enabled)
            .map_err(|error| sqlite_error(&error))?;
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DDL, enabled)
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
pub fn release_generation(generation: u64) {
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
    let sql = payload_str(payload, "sql");
    with_db(id, |conn| {
        conn.execute_batch(sql)
            .map_err(|error| sqlite_error(&error))?;
        Ok(JsonValue::Null)
    })
}

fn prepare(payload: &JsonValue) -> NodeResult {
    let id = payload_u64(payload, "id")?;
    let sql = payload_str(payload, "sql");
    with_db(id, |conn| {
        // Validate now (Node throws on bad SQL at prepare time) through the
        // cache so later run/get/all calls reuse the compiled statement. The
        // column count is returned for the shim's `StatementSync.reader` and
        // for its "does this statement produce rows" decisions.
        let stmt = conn
            .prepare_cached(sql)
            .map_err(|error| sqlite_error(&error))?;
        Ok(json!({ "columns": stmt.column_count() }))
    })
}

/// Bind the payload's parameters onto a reset statement. Two wire shapes are
/// accepted:
///
/// - fast path: `"values":[...]` with an optional parallel `"indices":[...]`
///   (one-based SQLite parameter indexes). When `indices` is absent the
///   values bind to `1..=n` in order — the shape every positional call sends,
///   so the common case crosses as one flat array with no per-parameter
///   object.
/// - legacy: `"binds":[{"index":n,"value":v},...]`.
///
/// Values are read straight from the parsed payload (no per-call deep clone)
/// and borrowed where `rusqlite` can bind them without owning them.
fn bind_all(stmt: &mut rusqlite::Statement<'_>, payload: &JsonValue) -> Result<(), NodeError> {
    if let Some(values) = payload.get("values").and_then(JsonValue::as_array) {
        let indices = payload.get("indices").and_then(JsonValue::as_array);
        for (position, value) in values.iter().enumerate() {
            let index = match indices {
                Some(list) => list
                    .get(position)
                    .and_then(JsonValue::as_u64)
                    .ok_or_else(|| {
                        NodeError::new(
                            "EINVAL",
                            "flat sqlite bind `indices` must match `values` in length",
                        )
                    })? as usize,
                None => position + 1,
            };
            bind_one(stmt, index, value)?;
        }
        return Ok(());
    }
    let binds: &[JsonValue] = payload
        .get("binds")
        .and_then(JsonValue::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for (position, bind) in binds.iter().enumerate() {
        let index = bind
            .get("index")
            .and_then(JsonValue::as_u64)
            .unwrap_or(position as u64 + 1) as usize;
        let value = bind.get("value").unwrap_or(&JsonValue::Null);
        bind_one(stmt, index, value)?;
    }
    Ok(())
}

fn bind_one(
    stmt: &mut rusqlite::Statement<'_>,
    index: usize,
    value: &JsonValue,
) -> Result<(), NodeError> {
    match decode_bind(value, index)? {
        BindValue::Null => stmt.raw_bind_parameter(index, rusqlite::types::Null),
        BindValue::Int(int) => stmt.raw_bind_parameter(index, int),
        BindValue::Real(real) => stmt.raw_bind_parameter(index, real),
        BindValue::Text(text) => stmt.raw_bind_parameter(index, text),
        BindValue::Blob(bytes) => stmt.raw_bind_parameter(index, bytes),
    }
    .map_err(|error| sqlite_error(&error))?;
    Ok(())
}

fn run(payload: &JsonValue) -> NodeResult {
    let id = payload_u64(payload, "id")?;
    let sql = payload_str(payload, "sql");
    let read_bigints = payload_bool(payload, "readBigInts");
    with_db(id, |conn| {
        let (changes, last_rowid) = {
            let mut stmt = conn
                .prepare_cached(sql)
                .map_err(|error| sqlite_error(&error))?;
            bind_all(&mut stmt, payload)?;
            // Node's run() steps every row and discards them (a `SELECT` is
            // allowed, and `INSERT ... RETURNING` runs to completion); the
            // counters are then the connection's `sqlite3_changes` /
            // `sqlite3_last_insert_rowid`, which a SELECT leaves at the
            // previous statement's values. `raw_execute` refuses row-returning
            // statements, so drive `raw_query` manually.
            {
                let mut rows = stmt.raw_query();
                while rows.next().map_err(|error| sqlite_error(&error))?.is_some() {}
            }
            (conn.changes() as i64, conn.last_insert_rowid())
        };
        Ok(encode_run_fields(changes, last_rowid, read_bigints))
    })
}

fn encode_cells(
    row: &rusqlite::Row<'_>,
    column_count: usize,
    read_bigints: bool,
) -> Result<Vec<JsonValue>, NodeError> {
    let mut cells = Vec::with_capacity(column_count);
    for index in 0..column_count {
        let cell = row.get_ref(index).map_err(|error| sqlite_error(&error))?;
        cells.push(encode_cell(cell, read_bigints));
    }
    Ok(cells)
}

/// One non-columnar result row (`returnArrays` or a named object).
fn encode_query_row(
    row: &rusqlite::Row<'_>,
    column_count: usize,
    names: &[String],
    return_arrays: bool,
    read_bigints: bool,
) -> Result<JsonValue, NodeError> {
    if return_arrays {
        return Ok(JsonValue::Array(encode_cells(
            row,
            column_count,
            read_bigints,
        )?));
    }
    // One row object with exactly one key per column: pre-size so the map does
    // not grow (and rehash) while the row is assembled.
    let mut map = JsonMap::with_capacity(column_count);
    for index in 0..column_count {
        let cell = row.get_ref(index).map_err(|error| sqlite_error(&error))?;
        let name = names
            .get(index)
            .cloned()
            .unwrap_or_else(|| format!("column{index}"));
        map.insert(name, encode_cell(cell, read_bigints));
    }
    Ok(JsonValue::Object(map))
}

fn query(payload: &JsonValue, first_row_only: bool) -> NodeResult {
    let id = payload_u64(payload, "id")?;
    let sql = payload_str(payload, "sql");
    let read_bigints = payload_bool(payload, "readBigInts");
    let return_arrays = payload_bool(payload, "returnArrays");
    let columnar = payload_bool(payload, "columnar");
    let json_text = payload_bool(payload, "jsonText");
    with_db(id, |conn| {
        let mut stmt = conn
            .prepare_cached(sql)
            .map_err(|error| sqlite_error(&error))?;
        bind_all(&mut stmt, payload)?;
        // `SELECT *` after `ALTER TABLE` changes its column set; SQLite
        // recompiles the cached statement on the first step, so the column
        // count and names must be read from the stepped statement (Node's
        // StatementSync reflects the schema change in the rows it returns).
        let mut rows = stmt.raw_query();
        let first = rows.next().map_err(|error| sqlite_error(&error))?;
        let (column_count, names) = match &first {
            Some(row) => {
                let live: &rusqlite::Statement<'_> = row.as_ref();
                (
                    live.column_count(),
                    if columnar || !return_arrays {
                        statement_columns(live)
                    } else {
                        Vec::new()
                    },
                )
            }
            // Empty result: no row was stepped, so there is nothing to read
            // metadata from here. `columns()` compiles fresh and still answers
            // the current column set.
            None => (0, Vec::new()),
        };
        if columnar {
            if first_row_only {
                let row = match first {
                    Some(row) => JsonValue::Array(encode_cells(row, column_count, read_bigints)?),
                    None => JsonValue::Null,
                };
                return query_result(json!({ "columns": names, "row": row }), json_text);
            }
            let mut collected: Vec<JsonValue> = Vec::new();
            if let Some(row) = first {
                collected.push(JsonValue::Array(encode_cells(
                    row,
                    column_count,
                    read_bigints,
                )?));
            }
            while let Some(row) = rows.next().map_err(|error| sqlite_error(&error))? {
                collected.push(JsonValue::Array(encode_cells(
                    row,
                    column_count,
                    read_bigints,
                )?));
            }
            return query_result(json!({ "columns": names, "rows": collected }), json_text);
        }

        // `get` returns at most one row; encode it directly instead of
        // pushing it into a one-element `Vec` that is immediately unwrapped.
        if first_row_only {
            return Ok(match first {
                Some(row) => {
                    encode_query_row(row, column_count, &names, return_arrays, read_bigints)?
                }
                None => JsonValue::Null,
            });
        }
        let mut collected: Vec<JsonValue> = Vec::new();
        if let Some(row) = first {
            collected.push(encode_query_row(
                row,
                column_count,
                &names,
                return_arrays,
                read_bigints,
            )?);
        }
        while let Some(row) = rows.next().map_err(|error| sqlite_error(&error))? {
            collected.push(encode_query_row(
                row,
                column_count,
                &names,
                return_arrays,
                read_bigints,
            )?);
        }
        Ok(JsonValue::Array(collected))
    })
}

fn columns(payload: &JsonValue) -> NodeResult {
    let id = payload_u64(payload, "id")?;
    let sql = payload_str(payload, "sql");
    with_db(id, |conn| {
        let stmt = conn
            .prepare_cached(sql)
            .map_err(|error| sqlite_error(&error))?;
        // `column`/`table`/`database` origin metadata is only available
        // via the C column-database-name APIs rusqlite does not expose;
        // Node's StatementSync.columns() consumers read `name` and
        // `type`/nullability at most, and context-mode reads nothing.
        let out: Vec<JsonValue> = statement_columns(&stmt)
            .iter()
            .map(|name| {
                json!({
                    "column": null,
                    "database": null,
                    "name": name,
                    "table": null,
                    "type": null,
                })
            })
            .collect();
        Ok(JsonValue::Array(out))
    })
}

/// `StatementSync.expandedSQL`: bind the supplied parameters onto the cached
/// statement and ask SQLite for its expanded text. Unbound parameters render
/// as `NULL`, matching Node (`stmt.expandedSQL` before any execution).
fn expanded(payload: &JsonValue) -> NodeResult {
    let id = payload_u64(payload, "id")?;
    let sql = payload_str(payload, "sql");
    with_db(id, |conn| {
        let mut stmt = conn
            .prepare_cached(sql)
            .map_err(|error| sqlite_error(&error))?;
        bind_all(&mut stmt, payload)?;
        Ok(match stmt.expanded_sql() {
            Some(text) => JsonValue::String(text),
            None => JsonValue::String(sql.to_owned()),
        })
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

// ---------------------------------------------------------------------------
// Typed query entry (C-ABI fast path)
// ---------------------------------------------------------------------------
//
// `__pirSqliteQuery(handle, sql, params, mode, flags)` in
// `host_functions/sqlite_ops.rs` converts live JS values into these neutral
// Rust shapes and builds the JS result once, with no `serde_json` tree and no
// second materialization pass in JS:
//
// - parameters arrive as owned [`SqlBind`] values (the C-ABI layer converts a
//   live `Uint8Array`/string/bigint without base64 or a JSON object per bind);
// - results leave as [`QueryRows`] (column names once, cells per row); the
//   C-ABI layer decides between row objects, arrays, and scalar lists;
// - an INTEGER cell that cannot survive an f64 round trip while
//   `safe_integers` is off yields `None` (the JS layer falls back to the
//   envelope path, which raises Node's `ERR_OUT_OF_RANGE`), as does any shape
//   this tier does not own (unknown handle, named key that is not a parameter).

/// One bound parameter, converted from a live JS value by the C-ABI layer.
pub enum SqlBind {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// The statement's parameter set.
pub enum BindSet {
    /// Bind to SQLite parameter indexes `1..=n` in order — the flat array the
    /// shims send for every hot-path call.
    Positional(Vec<SqlBind>),
    /// Bind by parameter name: `@x` / `:x` / `$x` resolve verbatim, a bare
    /// name resolves against those three prefixes in that order. A key that
    /// names no parameter makes the whole call fall back (the JS shim owns the
    /// unknown-name policy).
    Named(Vec<(String, SqlBind)>),
}

/// One result cell.
pub enum SqlCell {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// Column names once per call plus the row-major cells (`column_count` per
/// row). One flat vector means neither side allocates a `Vec` per row.
pub struct QueryRows {
    pub columns: Vec<String>,
    pub cells: Vec<SqlCell>,
    pub column_count: usize,
}

pub enum QueryOutcome {
    Rows(QueryRows),
    Run {
        changes: i64,
        last_insert_rowid: i64,
    },
}

/// Output-shape switches (the C-ABI `flags` object).
#[derive(Debug, Clone, Copy, Default)]
pub struct QueryFlags {
    /// Rows as arrays instead of objects (`setReturnArrays`, better-sqlite3
    /// `.raw()`).
    pub raw: bool,
    /// First column per row.
    pub pluck: bool,
    /// INTEGER cells (and `changes`/`lastInsertRowid`) as BigInt.
    pub safe_integers: bool,
    /// `{columns, rows}` / `{columns, row}` instead of assembled objects.
    pub columnar: bool,
    /// Group `table.column` names into nested objects (better-sqlite3
    /// `.expand()`).
    pub expand: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMode {
    All,
    Get,
    Run,
    Iterate,
}

/// Execute one statement for the typed C-ABI entry.
///
/// `Ok(None)` means "not this tier's shape" — the caller answers `undefined`
/// and the JS shim runs its envelope fallback. After the shape was accepted a
/// SQLite failure is returned as an error (never re-run: binding may have
/// happened, and re-executing a statement is observably different for writes).
pub fn query_typed(
    handle: u64,
    sql: &str,
    binds: BindSet,
    mode: QueryMode,
    flags: QueryFlags,
) -> Result<Option<QueryOutcome>, NodeError> {
    query_bound(handle, sql, mode, flags, |stmt| bind_typed(stmt, &binds))
}

/// [`query_typed`] with the binding step supplied by the caller.
///
/// The caller can bind straight from live JS values while the cached statement
/// is borrowed, avoiding the `BindSet`/`SqlBind` copies the typed envelope
/// path builds first. The closure answers `Ok(false)` for a shape it does not
/// own (nothing has been stepped yet, so the fallback is safe).
pub fn query_bound<E, F>(
    handle: u64,
    sql: &str,
    mode: QueryMode,
    flags: QueryFlags,
    bind: F,
) -> Result<Option<QueryOutcome>, E>
where
    E: From<NodeError>,
    F: FnOnce(&mut rusqlite::Statement<'_>) -> Result<bool, E>,
{
    // One table-lock acquisition: the slot's `Arc` keeps the connection alive
    // for the statement even if a concurrent `close` removes the slot. An
    // unknown handle is not this tier's shape — the envelope path raises
    // Node's own "database is not open" (`ERR_INVALID_STATE`) error.
    let conn = {
        let map = databases()
            .lock()
            .map_err(|_| NodeError::new("EIO", "sqlite handle table poisoned"))?;
        match map.get(&handle) {
            Some(slot) => Arc::clone(&slot.conn),
            None => return Ok(None),
        }
    };
    let mut conn = conn
        .lock()
        .map_err(|_| NodeError::new("EIO", "sqlite connection mutex poisoned"))?;
    let conn = &mut *conn;
    let mut stmt = conn
        .prepare_cached(sql)
        .map_err(|error| sqlite_error(&error))?;
    if !bind(&mut stmt)? {
        return Ok(None);
    }
    if mode == QueryMode::Run {
        // Node's run() steps through every returned row and discards it (see
        // the envelope `run` op); `raw_execute` would refuse SELECT and
        // `... RETURNING`.
        {
            let mut rows = stmt.raw_query();
            while rows.next().map_err(|error| sqlite_error(&error))?.is_some() {}
        }
        let changes = conn.changes() as i64;
        return Ok(Some(QueryOutcome::Run {
            changes,
            last_insert_rowid: conn.last_insert_rowid(),
        }));
    }
    // Column names/count are read from the stepped statement: a cached
    // `SELECT *` recompiles on the first step after a schema change.
    let names_needed = flags.columnar || flags.expand || (!flags.raw && !flags.pluck);
    let mut rows = stmt.raw_query();
    let first = rows.next().map_err(|error| sqlite_error(&error))?;
    let (column_count, columns) = match &first {
        Some(row) => {
            let live: &rusqlite::Statement<'_> = row.as_ref();
            (
                live.column_count(),
                if names_needed {
                    statement_columns(live)
                } else {
                    Vec::new()
                },
            )
        }
        None => (0, Vec::new()),
    };
    let single = mode == QueryMode::Get;
    // `.get` knows its row width up front; `.all` grows amortically.
    let mut cells: Vec<SqlCell> = Vec::with_capacity(if single { column_count } else { 0 });
    if let Some(row) = first {
        encode_typed_row(row, column_count, flags.safe_integers, &mut cells)?;
    }
    if !single {
        while let Some(row) = rows.next().map_err(|error| sqlite_error(&error))? {
            encode_typed_row(row, column_count, flags.safe_integers, &mut cells)?;
        }
    }
    Ok(Some(QueryOutcome::Rows(QueryRows {
        columns,
        cells,
        column_count,
    })))
}

/// Append one row's cells. An INTEGER beyond the f64-safe range without BigInt
/// reads is an error, not a fallback: the statement has already been stepped
/// (and may have written), so re-running it in the envelope path would execute
/// `INSERT ... RETURNING` twice.
fn encode_typed_row(
    row: &rusqlite::Row<'_>,
    column_count: usize,
    safe_integers: bool,
    cells: &mut Vec<SqlCell>,
) -> Result<(), NodeError> {
    for index in 0..column_count {
        let cell = row.get_ref(index).map_err(|error| sqlite_error(&error))?;
        let Some(cell) = typed_cell(cell, safe_integers) else {
            let value = match row.get_ref(index) {
                Ok(rusqlite::types::ValueRef::Integer(value)) => value.to_string(),
                _ => "?".to_owned(),
            };
            return Err(NodeError::new(
                "ERR_OUT_OF_RANGE",
                format!("Value is too large to be represented as a JavaScript number: {value}"),
            ));
        };
        cells.push(cell);
    }
    Ok(())
}

/// Bind every parameter. `false` = a named key is not a parameter of this
/// statement (fall back), which is decided before anything executes.
fn bind_typed(stmt: &mut rusqlite::Statement<'_>, binds: &BindSet) -> Result<bool, NodeError> {
    match binds {
        BindSet::Positional(values) => {
            for (position, value) in values.iter().enumerate() {
                bind_typed_value(stmt, position + 1, value)?;
            }
        }
        BindSet::Named(entries) => {
            let count = stmt.parameter_count();
            // Bare names of the statement's named parameters (anonymous `?`
            // slots have no name). This is the set Node's unknown-name policy
            // compares object keys against.
            let mut bare_names: Vec<String> = Vec::new();
            for index in 1..=count {
                if let Some(name) = stmt.parameter_name(index) {
                    bare_names.push(bare_parameter_name(name).to_owned());
                }
            }
            for (key, _) in entries {
                if !bare_names
                    .iter()
                    .any(|name| name == bare_parameter_name(key))
                {
                    // Not a parameter of this statement: the shim owns the
                    // unknown-name policy (and its error wording).
                    return Ok(false);
                }
            }
            // Bind per parameter, Node-style: the exact prefixed key first,
            // then a bare key (so `SELECT @x, :x` with `{x: 1}` binds both
            // slots, as the envelope path does).
            let mut plan: Vec<(usize, usize)> = Vec::new();
            for index in 1..=count {
                let Some(name) = stmt.parameter_name(index) else {
                    continue;
                };
                let bare = bare_parameter_name(name);
                let found = entries
                    .iter()
                    .position(|(key, _)| key.as_str() == name)
                    .or_else(|| entries.iter().position(|(key, _)| key.as_str() == bare));
                if let Some(entry) = found {
                    plan.push((index, entry));
                }
            }
            for (index, entry) in plan {
                bind_typed_value(stmt, index, &entries[entry].1)?;
            }
        }
    }
    Ok(true)
}

/// SQLite wants the `@`/`:`/`$` prefix for a name lookup; a bare key matches
/// whichever prefix the statement used (Node's bare-name rule).
fn bare_parameter_name(name: &str) -> &str {
    match name.as_bytes().first() {
        Some(b'@' | b':' | b'$') => &name[1..],
        _ => name,
    }
}

fn bind_typed_value(
    stmt: &mut rusqlite::Statement<'_>,
    index: usize,
    value: &SqlBind,
) -> Result<(), NodeError> {
    let bound = match value {
        SqlBind::Null => stmt.raw_bind_parameter(index, rusqlite::types::Null),
        SqlBind::Int(int) => stmt.raw_bind_parameter(index, *int),
        SqlBind::Real(real) => stmt.raw_bind_parameter(index, *real),
        SqlBind::Text(text) => stmt.raw_bind_parameter(index, text.as_str()),
        SqlBind::Blob(bytes) => stmt.raw_bind_parameter(index, bytes.as_slice()),
    };
    bound.map_err(|error| sqlite_error(&error))
}

/// `None` when the cell needs the envelope path's RangeError (see
/// [`query_typed`]).
fn typed_cell(cell: ValueRef<'_>, safe_integers: bool) -> Option<SqlCell> {
    match cell {
        ValueRef::Null => Some(SqlCell::Null),
        ValueRef::Integer(value) => {
            if !safe_integers && !f64_safe_integer(value) {
                return None;
            }
            Some(SqlCell::Int(value))
        }
        // NaN/±Inf have no JS number form Node can hand back either; the
        // envelope path serializes them as NULL and so does this one.
        ValueRef::Real(value) if value.is_finite() => Some(SqlCell::Real(value)),
        ValueRef::Real(_) => Some(SqlCell::Null),
        ValueRef::Text(bytes) => Some(SqlCell::Text(String::from_utf8_lossy(bytes).into_owned())),
        ValueRef::Blob(bytes) => Some(SqlCell::Blob(bytes.to_vec())),
    }
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
