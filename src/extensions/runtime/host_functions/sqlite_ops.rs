//! Typed C-ABI entries for the SQLite row paths.
//!
//! `better-sqlite3`/`node:sqlite` calls otherwise cross the generic
//! `__pirNode.invoke` envelope: a payload object per call, a `serde_json`
//! tree for every row, and a second pass in JS to build the row objects.
//! A no-op `.get()` measured ~8µs end-to-end, and a 1000-row `.all()` spent
//! most of its time materializing rows twice.
//!
//! [`sqlite_query`] binds parameters from a live JS array and builds the
//! result values directly, so one query is one C call and each row object is
//! written once. The Rust side (`crate::pir_node::modules::sqlite::query_typed`)
//! returns neutral rows; this layer turns them into JS objects/arrays/scalars
//! with no JSON text in between.
//!
//! Contract with the JS shims (same discipline as `bytes_ops`):
//!
//! - Every shape this tier does not own answers `undefined`: a non-numeric
//!   handle, a non-string `sql`, an unsupported `mode`, a parameter value that
//!   is not `null`/`undefined`/boolean/number/bigint/string/Uint8Array, a
//!   named key that is not a parameter of the statement, or an INTEGER cell
//!   that needs Node's `ERR_OUT_OF_RANGE`. The shim then runs its existing
//!   `columnar`/`legacy` path, so no behaviour depends on this tier existing.
//! - After the shape was accepted, failures are *thrown* as the `Error` shape
//!   the shims already reconstruct (`code = ERR_SQLITE_ERROR` + SQLite's
//!   message, `ERR_INVALID_STATE` for a handle that vanished). Never silently
//!   fall back: a `run` may already have written.
//!
//! Result shapes (`mode` + `flags`):
//!
//! | mode      | default                | `raw`         | `pluck`       |
//! |-----------|------------------------|---------------|---------------|
//! | `all`     | array of row objects   | array of arrays | array of first-column values |
//! | `get`     | first row object or `undefined` | first row array or `undefined` | first value or `undefined` |
//! | `iterate` | same as `all` (the shim materializes) | same as `all` | same as `all` |
//! | `run`     | `{changes, lastInsertRowid}` | —       | —             |
//!
//! `columnar` replaces the per-row shape with `{columns, rows:[[...]]}`
//! (`{columns, row:[...]|null}` for `get`); `expand` groups `table.column`
//! keys into nested null-prototype objects; `safeIntegers` renders INTEGER
//! cells (and the `run` counters) as BigInt. Row objects use a null prototype,
//! matching `node:sqlite`; BLOB cells are zero-copy `Uint8Array` views.

use rquickjs::prelude::Opt;
use rquickjs::{Array, Atom, Ctx, Object, Result, String as JsString, Type, TypedArray, Value};

use crate::pir_node::modules::sqlite::{
    BindSet, QueryFlags, QueryMode, QueryOutcome, QueryRows, SqlBind, SqlCell,
};

/// `__pirSqliteQuery(handle, sql, params, mode, flags)`.
///
/// `params` is a live array (positional, flat) or a plain object (named
/// binds); `mode` is `"all" | "get" | "run" | "iterate"`; `flags` is an
/// optional object read with strict `=== true` semantics.
pub(super) fn sqlite_query<'js>(
    ctx: Ctx<'js>,
    handle: Value<'js>,
    sql: Value<'js>,
    params: Value<'js>,
    mode: Value<'js>,
    flags: Opt<Value<'js>>,
    status: Opt<Value<'js>>,
) -> Result<Value<'js>> {
    // `status.owned` tells the shim whether this tier executed the call. A
    // `get` that finds no row answers `undefined`, exactly like "not owned",
    // so the shim cannot use the value to decide whether to fall back: saying
    // so explicitly keeps it from re-running a write (`CREATE`, `INSERT ...
    // RETURNING` with no rows).
    let status = status.0.as_ref().and_then(Value::as_object);
    let mark = |owned: bool| {
        if let Some(status) = &status {
            let _ = status.set("owned", owned);
        }
    };
    let Some(handle) = handle_number(&handle) else {
        mark(false);
        return Ok(Value::new_undefined(ctx));
    };
    // `JS_ToCStringLen` copies a JS string into a NUL-terminated buffer;
    // reading it as `&str` avoids a second Rust `String` allocation per call.
    let Some(sql) = sql_string(&sql) else {
        mark(false);
        return Ok(Value::new_undefined(ctx));
    };
    let Some(mode) = query_mode(&mode) else {
        mark(false);
        return Ok(Value::new_undefined(ctx));
    };
    let flags = query_flags(&flags)?;
    let Some(binds) = param_set(&ctx, &params)? else {
        mark(false);
        return Ok(Value::new_undefined(ctx));
    };
    match crate::pir_node::modules::sqlite::query_typed(handle, sql.as_str(), binds, mode, flags) {
        Ok(Some(outcome)) => {
            mark(true);
            build_result(&ctx, outcome, mode, flags)
        }
        Ok(None) => {
            mark(false);
            Ok(Value::new_undefined(ctx))
        }
        Err(error) => Err(super::bytes_ops::raise(&ctx, &error)),
    }
}

/// Error channel for [`bind_live_positional`]: a JS read can fail through
/// rquickjs, binding through rusqlite.
enum BindFailure {
    Node(crate::pir_node::NodeError),
    Js(rquickjs::Error),
}

impl From<crate::pir_node::NodeError> for BindFailure {
    fn from(error: crate::pir_node::NodeError) -> Self {
        BindFailure::Node(error)
    }
}

impl From<rquickjs::Error> for BindFailure {
    fn from(error: rquickjs::Error) -> Self {
        BindFailure::Js(error)
    }
}

/// Bind a live positional array straight onto the cached statement.
///
/// Unlike [`param_set`] this allocates no `Vec<SqlBind>` and copies neither
/// TEXT (a `CString` borrows the JS string) nor BLOB bytes (rusqlite binds a
/// `&[u8]` with SQLITE_TRANSIENT, which copies once inside SQLite). `Ok(false)`
/// is "not this shape" and is decided before the statement is stepped, so the
/// envelope fallback remains safe.
#[allow(unsafe_code)]
fn bind_live_positional(
    params: &Value<'_>,
    stmt: &mut rusqlite::Statement<'_>,
) -> std::result::Result<bool, BindFailure> {
    let Some(array) = params.as_array() else {
        return Ok(false);
    };
    for index in 0..array.len() {
        let item: Value = array.get(index)?;
        let slot = index + 1;
        let bound = match item.type_of() {
            Type::Null => stmt
                .raw_bind_parameter(slot, rusqlite::types::Null)
                .map(|_| ()),
            Type::Undefined | Type::Uninitialized => return Ok(false),
            Type::Bool => stmt
                .raw_bind_parameter(slot, i64::from(item.as_bool() == Some(true)))
                .map(|_| ()),
            Type::Int | Type::Float => match item.as_number() {
                Some(number) => stmt.raw_bind_parameter(slot, number).map(|_| ()),
                None => return Ok(false),
            },
            Type::BigInt => match item
                .clone()
                .into_big_int()
                .and_then(|big| big.to_i64().ok())
            {
                Some(value) => stmt.raw_bind_parameter(slot, value).map(|_| ()),
                None => return Ok(false),
            },
            Type::String => match item
                .clone()
                .into_string()
                .and_then(|text| text.to_cstring().ok())
            {
                Some(text) => stmt.raw_bind_parameter(slot, text.as_str()).map(|_| ()),
                None => return Ok(false),
            },
            Type::Object => {
                let Some(typed) = item
                    .as_object()
                    .and_then(|object| object.as_typed_array::<u8>())
                else {
                    return Ok(false);
                };
                // SAFETY: live attached typed array; no JS runs while the
                // borrow is held, and rusqlite copies the bytes (TRANSIENT).
                let Some(bytes) = (unsafe { typed.as_bytes() }) else {
                    return Ok(false);
                };
                stmt.raw_bind_parameter(slot, bytes).map(|_| ())
            }
            _ => return Ok(false),
        };
        bound.map_err(|error| crate::pir_node::modules::sqlite::sqlite_error(&error))?;
    }
    Ok(true)
}

fn sql_string<'js>(value: &Value<'js>) -> Option<rquickjs::CString<'js>> {
    value.as_string()?.clone().to_cstring().ok()
}

/// `__pirSqliteRun(handle, sql, params, safeIntegers)` — the statement-write
/// hot path, with no `flags` object and no `status` side channel.
///
/// `params` is a live positional array. Every value the typed tier does not
/// own (named object, unknown handle, non-string SQL, unsupported value)
/// answers `undefined`, which is unambiguous here: unlike `get`, a write never
/// has a "no row" answer, so `undefined` can only mean "run the envelope
/// fallback". A failure after the shape was accepted is thrown (the write may
/// already have landed; re-running it would duplicate it).
pub(super) fn sqlite_run<'js>(
    ctx: Ctx<'js>,
    handle: Value<'js>,
    sql: Value<'js>,
    params: Value<'js>,
    safe_integers: Opt<Value<'js>>,
) -> Result<Value<'js>> {
    let Some(handle) = handle_number(&handle) else {
        return Ok(Value::new_undefined(ctx));
    };
    let Some(sql) = sql_string(&sql) else {
        return Ok(Value::new_undefined(ctx));
    };
    let safe_integers = safe_integers
        .0
        .as_ref()
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let flags = QueryFlags {
        safe_integers,
        ..QueryFlags::default()
    };
    // Bind straight from the live array: no `BindSet`/`SqlBind` copies on the
    // 20k-insert path (TEXT borrows the JS string, BLOB binds the view bytes).
    match crate::pir_node::modules::sqlite::query_bound(
        handle,
        sql.as_str(),
        QueryMode::Run,
        flags,
        |stmt| bind_live_positional(&params, stmt),
    ) {
        Ok(Some(QueryOutcome::Run {
            changes,
            last_insert_rowid,
        })) => {
            let object = Object::new(ctx.clone())?;
            object.set("changes", int_value(&ctx, changes, safe_integers)?)?;
            object.set(
                "lastInsertRowid",
                int_value(&ctx, last_insert_rowid, safe_integers)?,
            )?;
            Ok(object.into_value())
        }
        // Never fall back after an accepted `Run`: the statement has already
        // been stepped and may have written. `Run` mode cannot produce rows
        // (see `query_typed`); if that ever changes, surface it instead of
        // re-executing the write.
        Ok(Some(QueryOutcome::Rows(_))) => Err(super::bytes_ops::raise(
            &ctx,
            &crate::pir_node::NodeError::new("EIO", "sqlite run unexpectedly returned rows"),
        )),
        Ok(None) => Ok(Value::new_undefined(ctx)),
        Err(BindFailure::Node(error)) => Err(super::bytes_ops::raise(&ctx, &error)),
        Err(BindFailure::Js(error)) => Err(error),
    }
}

/// A handle id is a non-negative integer JS number (the table's ids are small).
fn handle_number(value: &Value<'_>) -> Option<u64> {
    let number = value.as_number()?;
    if number.is_finite() && number >= 0.0 && number.fract() == 0.0 && number <= 9007199254740991.0
    {
        Some(number as u64)
    } else {
        None
    }
}

fn query_mode(value: &Value<'_>) -> Option<QueryMode> {
    // The shims pass a small integer code on the hot path: `JS_ToCStringLen`
    // would otherwise copy and UTF-8-validate the mode string on every
    // statement. String spellings stay accepted for older shim blobs.
    if let Some(code) = value.as_int() {
        return match code {
            0 => Some(QueryMode::All),
            1 => Some(QueryMode::Get),
            2 => Some(QueryMode::Run),
            3 => Some(QueryMode::Iterate),
            _ => None,
        };
    }
    let text = value.as_string()?.clone().to_cstring().ok()?;
    Some(match text.as_str() {
        "all" => QueryMode::All,
        "get" => QueryMode::Get,
        "run" => QueryMode::Run,
        "iterate" => QueryMode::Iterate,
        _ => return None,
    })
}

fn query_flags<'js>(value: &Opt<Value<'js>>) -> Result<QueryFlags> {
    let Some(object) = value.0.as_ref().and_then(Value::as_object) else {
        return Ok(QueryFlags::default());
    };
    // Strict `=== true`, matching the shims' `enabled === true` switches. A
    // throwing getter is a real exception, not "not owned": propagate it.
    let flag =
        |key: &str| -> Result<bool> { Ok(object.get::<_, Value>(key)?.as_bool().unwrap_or(false)) };
    Ok(QueryFlags {
        raw: flag("raw")?,
        pluck: flag("pluck")?,
        safe_integers: flag("safeIntegers")?,
        columnar: flag("columnar")?,
        expand: flag("expand")?,
    })
}

/// Convert the live `params` value into an owned bind set. `None` = a shape
/// this tier does not own (the shim's envelope path owns validation); an
/// engine-level exception (a getter on a Proxy/array) is propagated instead.
fn param_set<'js>(ctx: &Ctx<'js>, value: &Value<'js>) -> Result<Option<BindSet>> {
    if let Some(array) = value.as_array() {
        let mut binds = Vec::with_capacity(array.len());
        for index in 0..array.len() {
            let item: Value = array.get(index)?;
            match bind_value(&item) {
                Some(bind) => binds.push(bind),
                None => return Ok(None),
            }
        }
        return Ok(Some(BindSet::Positional(binds)));
    }
    let Some(object) = value.as_object() else {
        return Ok(None);
    };
    // Named binds: only a plain (or null-prototype) object; a typed array,
    // Date, Map, ... is not a parameter shape this tier interprets.
    if !is_plain_object(ctx, object)? {
        return Ok(None);
    }
    let mut binds = Vec::with_capacity(object.len());
    for key in object.keys::<String>() {
        let key = key?;
        let item: Value = object.get(key.as_str())?;
        match bind_value(&item) {
            Some(bind) => binds.push((key, bind)),
            None => return Ok(None),
        }
    }
    Ok(Some(BindSet::Named(binds)))
}

/// Named-bind objects must be plain (`Object.prototype` or null prototype).
/// Anything else is a shape this tier does not interpret; the shim's envelope
/// path owns argument validation and its own error wording.
fn is_plain_object<'js>(ctx: &Ctx<'js>, object: &Object<'js>) -> Result<bool> {
    match object.get_prototype() {
        None => Ok(true),
        Some(prototype) => {
            let global: Value = ctx.globals().get("Object")?;
            let Some(constructor) = global.as_object() else {
                return Ok(false);
            };
            let builtin: Value = constructor.get("prototype")?;
            Ok(builtin
                .as_object()
                .map(|builtin| &prototype == builtin)
                .unwrap_or(false))
        }
    }
}

/// One JS value → one bound parameter. Numbers follow the envelope bridge's
/// exact rule (`number_from_f64`): an integral `f64` in i64 range binds as
/// INTEGER, everything else as REAL. Unsupported values answer `None` so the
/// fallback path raises its own `TypeError`/`RangeError`.
#[allow(unsafe_code)]
fn bind_value(value: &Value<'_>) -> Option<SqlBind> {
    match value.type_of() {
        // node:sqlite rejects `undefined` (ERR_INVALID_ARG_TYPE) but binds
        // `null` as SQL NULL. Hand `undefined` back to the shim, which raises
        // Node's exact message with the parameter index.
        Type::Null => Some(SqlBind::Null),
        Type::Undefined | Type::Uninitialized => None,
        // node:sqlite binds booleans as INTEGER 0/1 (probed on v24).
        Type::Bool => Some(SqlBind::Int(if value.as_bool()? { 1 } else { 0 })),
        // Every JS number binds through `sqlite3_bind_double` (probed:
        // `SELECT typeof(?)` answers `real`). NaN/Infinity are accepted too;
        // SQLite stores NaN as NULL.
        Type::Int | Type::Float => Some(SqlBind::Real(value.as_number()?)),
        Type::BigInt => {
            let big = value.clone().into_big_int()?;
            // Out-of-i64 BigInt: `to_i64` wraps, so the shim range-checks
            // against the i64 bounds before calling this tier (it raises
            // Node's ERR_INVALID_ARG_VALUE instead).
            Some(SqlBind::Int(big.to_i64().ok()?))
        }
        Type::String => {
            let text = value.clone().into_string()?.to_string().ok()?;
            Some(SqlBind::Text(text))
        }
        Type::Object => {
            // Buffer is a real Uint8Array here (the shim only patches the
            // prototype), so this one branch serves both.
            let typed = value.as_object()?.as_typed_array::<u8>()?;
            // SAFETY: live attached typed array; no JS runs while the borrow
            // is copied (same contract as `buffer_ops::typed_read`).
            let bytes = unsafe { typed.as_bytes() }?;
            Some(SqlBind::Blob(bytes.to_vec()))
        }
        _ => None,
    }
}

fn build_result<'js>(
    ctx: &Ctx<'js>,
    outcome: QueryOutcome,
    mode: QueryMode,
    flags: QueryFlags,
) -> Result<Value<'js>> {
    match outcome {
        QueryOutcome::Run {
            changes,
            last_insert_rowid,
        } => {
            let object = Object::new(ctx.clone())?;
            object.set("changes", int_value(ctx, changes, flags.safe_integers)?)?;
            object.set(
                "lastInsertRowid",
                int_value(ctx, last_insert_rowid, flags.safe_integers)?,
            )?;
            Ok(object.into_value())
        }
        QueryOutcome::Rows(rows) => build_rows(ctx, rows, mode, flags),
    }
}

fn build_rows<'js>(
    ctx: &Ctx<'js>,
    rows: QueryRows,
    mode: QueryMode,
    flags: QueryFlags,
) -> Result<Value<'js>> {
    let QueryRows {
        columns,
        cells,
        column_count,
    } = rows;
    let single = mode == QueryMode::Get;
    let mut cells = cells.into_iter();
    if flags.columnar {
        let names = columns_array(ctx, &columns)?;
        if single {
            let object = Object::new(ctx.clone())?;
            object.set("columns", names)?;
            if column_count > 0 && cells.len() >= column_count {
                let row = row_array(ctx, cells.by_ref().take(column_count), flags.safe_integers)?;
                object.set("row", row)?;
            } else {
                object.set("row", Value::new_null(ctx.clone()))?;
            }
            return Ok(object.into_value());
        }
        let out = Array::new(ctx.clone())?;
        let mut index = 0;
        while column_count > 0 && cells.len() >= column_count {
            out.set(
                index,
                row_array(ctx, cells.by_ref().take(column_count), flags.safe_integers)?,
            )?;
            index += 1;
        }
        let object = Object::new(ctx.clone())?;
        object.set("columns", names)?;
        object.set("rows", out)?;
        return Ok(object.into_value());
    }
    // Object rows reuse one interned atom per column instead of re-interning
    // the name for every cell of every row.
    let atoms: Option<Vec<Atom<'js>>> = if flags.raw || flags.pluck || flags.expand {
        None
    } else {
        Some(
            columns
                .iter()
                .map(|name| Atom::from_str(ctx.clone(), name))
                .collect::<Result<_>>()?,
        )
    };
    if single {
        if column_count == 0 || cells.len() < column_count {
            return Ok(Value::new_undefined(ctx.clone()));
        }
        return row_value(
            ctx,
            &columns,
            atoms.as_deref(),
            cells.take(column_count),
            flags,
        );
    }
    let out = Array::new(ctx.clone())?;
    let mut index = 0;
    while column_count > 0 && cells.len() >= column_count {
        out.set(
            index,
            row_value(
                ctx,
                &columns,
                atoms.as_deref(),
                cells.by_ref().take(column_count),
                flags,
            )?,
        )?;
        index += 1;
    }
    Ok(out.into_value())
}

/// One row in the requested non-columnar shape. Cells are consumed straight
/// from the flat vector, so a row never allocates a temporary `Vec`.
fn row_value<'js>(
    ctx: &Ctx<'js>,
    columns: &[String],
    atoms: Option<&[Atom<'js>]>,
    cells: impl Iterator<Item = SqlCell>,
    flags: QueryFlags,
) -> Result<Value<'js>> {
    // `raw` and `pluck` are mutually exclusive modes on the shim side; when a
    // direct caller sets both, `raw` wins (arrays are the wider shape).
    if flags.raw {
        return Ok(row_array(ctx, cells, flags.safe_integers)?.into_value());
    }
    if flags.pluck {
        return match cells.into_iter().next() {
            Some(cell) => cell_value(ctx, cell, flags.safe_integers),
            None => Ok(Value::new_undefined(ctx.clone())),
        };
    }
    let object = Object::new_proto(ctx.clone(), None)?;
    let mut index = 0;
    for cell in cells {
        let value = cell_value(ctx, cell, flags.safe_integers)?;
        let name = columns.get(index);
        if flags.expand {
            if let Some(name) = name {
                if let Some(dot) = name.find('.') {
                    let (table, column) = (&name[..dot], &name[dot + 1..]);
                    let existing: Value = object.get(table)?;
                    let nested = match existing.as_object() {
                        Some(nested) => nested.clone(),
                        None => {
                            // Same rule as the better-sqlite3 shim: a
                            // non-object (or absent) entry becomes a fresh
                            // group object.
                            let nested = Object::new_proto(ctx.clone(), None)?;
                            object.set(table, nested.clone())?;
                            nested
                        }
                    };
                    nested.set(column, value)?;
                    index += 1;
                    continue;
                }
            }
        }
        if let Some(atom) = atoms.and_then(|atoms| atoms.get(index)) {
            object.set(atom.clone(), value)?;
        } else if let Some(name) = name {
            object.set(name.as_str(), value)?;
        }
        index += 1;
    }
    Ok(object.into_value())
}

fn row_array<'js>(
    ctx: &Ctx<'js>,
    cells: impl Iterator<Item = SqlCell>,
    safe_integers: bool,
) -> Result<Array<'js>> {
    let out = Array::new(ctx.clone())?;
    for (index, cell) in cells.enumerate() {
        out.set(index, cell_value(ctx, cell, safe_integers)?)?;
    }
    Ok(out)
}

fn columns_array<'js>(ctx: &Ctx<'js>, columns: &[String]) -> Result<Array<'js>> {
    let out = Array::new(ctx.clone())?;
    for (index, name) in columns.iter().enumerate() {
        out.set(index, JsString::from_str(ctx.clone(), name)?.into_value())?;
    }
    Ok(out)
}

fn cell_value<'js>(ctx: &Ctx<'js>, cell: SqlCell, safe_integers: bool) -> Result<Value<'js>> {
    Ok(match cell {
        SqlCell::Null => Value::new_null(ctx.clone()),
        SqlCell::Int(value) => int_value(ctx, value, safe_integers)?,
        SqlCell::Real(value) => Value::new_float(ctx.clone(), value),
        SqlCell::Text(text) => JsString::from_str(ctx.clone(), &text)?.into_value(),
        // `TypedArray::new` hands the `Vec` allocation to QuickJS — no base64,
        // no copy through a second buffer.
        SqlCell::Blob(bytes) => TypedArray::<u8>::new(ctx.clone(), bytes)?.into_value(),
    })
}

fn int_value<'js>(ctx: &Ctx<'js>, value: i64, as_bigint: bool) -> Result<Value<'js>> {
    if as_bigint {
        Ok(Value::new_big_int(ctx.clone(), value)?)
    } else {
        Ok(Value::new_number(ctx.clone(), value as f64))
    }
}

/// Install the SQLite fast-path globals.
pub(super) fn register_sqlite_ops(ctx: &Ctx<'_>) -> Result<()> {
    ctx.globals().set(
        "__pirSqliteQuery",
        rquickjs::prelude::Func::from(sqlite_query),
    )?;
    // Positional `run` only: the common write shape gets a 4-argument entry
    // (no `flags` object, no `status` property writes). Shims probe it and
    // fall back to `__pirSqliteQuery` when it is absent.
    ctx.globals().set(
        "__pirSqliteRun",
        rquickjs::prelude::Func::from(sqlite_run),
    )
}
