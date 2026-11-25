//! node:sqlite (DatabaseSync real) + the better-sqlite3 virtual package shim,
//! both backed by the host SQLite bridge (`__pir_host.node("sqlite", ...)` —
//! rusqlite bundled SQLite, FTS5 included).
//!
//! Fixtures are seeded at runtime (repository hygiene gate; same pattern as
//! `tests/node_shims.rs` / `tests/http_shims.rs`). Scenarios mirror the
//! semantics probed against real Node v24 `node:sqlite` and the only in-matrix
//! better-sqlite3 consumer (context-mode's db-base.js SQLiteBase).

use std::path::{Path, PathBuf};
use std::process::Command;

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// See tests/extensions_runtime.rs: fixtures must be written at runtime.
fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-sqlite-shims-{}-{test_name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    for (name, content) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture subdir");
        }
        std::fs::write(&path, content).expect("write fixture");
    }
    dir
}

fn test_config(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "headless".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: Some("test-session".to_string()),
        system_prompt: "You are a test.".to_string(),
        is_idle: true,
        limits: Some(Default::default()),
        allow_process_exit: false,
    }
}

async fn new_runtime_at(cwd: &Path) -> ExtensionRuntime {
    ExtensionRuntime::new(test_config(cwd))
        .await
        .expect("runtime construction")
}

async fn run_fixture(rt: &ExtensionRuntime, name: &str, ts: &str, global: &str) -> Value {
    let module = format!("pir-ext:{name}.ts");
    rt.seed_module(&module, ts).expect("seed fixture");
    let loaded = rt.seeded_load(&module).await;
    assert_eq!(loaded.expect("seeded load"), 1, "fixture {name} must load");
    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors in {name}: {:?}",
        outcome.errors
    );
    if global.is_empty() {
        return Value::Null;
    }
    rt.eval_json(&format!("JSON.stringify(globalThis.{global})"))
        .await
        .expect("read fixture results")
}

// ---------------------------------------------------------------------------
// node:sqlite DatabaseSync
// ---------------------------------------------------------------------------

#[tokio::test]
async fn node_sqlite_databasesync_crud_and_types() {
    let dir = write_temp_fixture("crud", &[]);
    let rt = new_runtime_at(&dir).await;

    const FIXTURE: &str = r#"
import { DatabaseSync } from "node:sqlite";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const db = new DatabaseSync(":memory:");

    // exec: multi-statement, returns undefined.
    out.execReturn = db.exec(
      "CREATE TABLE t(a INTEGER, b REAL, c TEXT, d BLOB); " +
      "INSERT INTO t VALUES (1, 1.5, 'x', x'0102');"
    );

    // run() result shape.
    const info = db.prepare("INSERT INTO t VALUES (?, ?, ?, ?)").run(2, 2.5, "y", new Uint8Array([3, 4]));
    out.runShape = {
      changes: info.changes,
      lastInsertRowid: info.lastInsertRowid,
      keys: Object.keys(info).sort().join(","),
    };

    // get/all + no-match semantics.
    const row = db.prepare("SELECT * FROM t WHERE a = ?").get(1);
    out.getRow = { a: row.a, b: row.b, c: row.c, d0: row.d[0] };
    out.getProto = Object.getPrototypeOf(row) === null;
    out.noMatchGet = db.prepare("SELECT * FROM t WHERE a = ?").get(999);
    out.noMatchAll = db.prepare("SELECT * FROM t WHERE a = ?").all(999);
    out.allCount = db.prepare("SELECT * FROM t").all().length;

    // NULL cells decode to null.
    db.exec("INSERT INTO t VALUES (3, NULL, NULL, NULL)");
    out.nullRow = db.prepare("SELECT * FROM t WHERE a = 3").get();

    // BigInt reads: statement flag + out-of-range guard.
    db.exec("CREATE TABLE bigs(v INTEGER)");
    db.prepare("INSERT INTO bigs VALUES (?)").run(9007199254740993n);
    const bigStmt = db.prepare("SELECT v FROM bigs");
    let threw = "";
    try { bigStmt.get(); } catch (e: any) { threw = e.code; }
    out.bigIntRangeGuard = threw;
    bigStmt.setReadBigInts(true);
    out.bigIntRead = typeof bigStmt.get().v === "bigint" && String(bigStmt.get().v) === "9007199254740993";

    // Named parameters: @a / :a / $a with bare keys, mixed anonymous.
    const named = db.prepare("SELECT @x a, :y b, $z c, ? d");
    out.named = named.get({ x: 1, y: 2, z: 3 }, 4);
    let unknown = "";
    try { db.prepare("SELECT @a v").get({ zzz: 1 }); } catch (e: any) { unknown = e.code + ":" + e.message; }
    out.unknownNamed = unknown.indexOf("Unknown named parameter 'zzz'") !== -1;
    out.prefixedKey = db.prepare("SELECT @a v").get({ "@a": 7 }).v;

    // Statement reuse with fresh bindings.
    const reusable = db.prepare("SELECT ? v");
    out.reuse = [reusable.get(1).v, reusable.get(2).v];

    // Transactions via exec: rollback discards the insert.
    db.exec("BEGIN");
    out.isTransaction = db.isTransaction;
    db.exec("INSERT INTO t VALUES (42, NULL, NULL, NULL)");
    db.exec("ROLLBACK");
    out.afterRollback = {
      isTransaction: db.isTransaction,
      gone: db.prepare("SELECT COUNT(*) c FROM t WHERE a = 42").get().c,
    };

    // iterate().
    const iterated: any[] = [];
    for (const row of db.prepare("SELECT a FROM t ORDER BY a").iterate()) iterated.push(row.a);
    out.iterate = iterated;

    // columns().
    out.columns = db.prepare("SELECT 1 AS x").columns().map((c: any) => c.name).join(",");

    // FTS5 (the probe context-mode runs before committing to node:sqlite).
    let fts5 = false;
    try {
      db.exec("CREATE VIRTUAL TABLE __fts5_probe USING fts5(x)");
      fts5 = true;
    } catch { /* no FTS5 */ }
    out.fts5 = fts5;

    // Closed-handle errors.
    db.close();
    out.isOpenAfterClose = db.isOpen;
    let closed = "";
    try { db.prepare("SELECT 1"); } catch (e: any) { closed = e.code; }
    out.closedPrepare = closed;

    globalThis.__sqliteCrud = out;
  });
};
"#;
    let value = run_fixture(&rt, "crud", FIXTURE, "__sqliteCrud").await;

    assert_eq!(value["execReturn"], Value::Null, "exec returns undefined");
    assert_eq!(value["runShape"]["changes"], 1);
    assert_eq!(value["runShape"]["lastInsertRowid"], 2);
    assert_eq!(value["runShape"]["keys"], "changes,lastInsertRowid");
    assert_eq!(value["getRow"]["a"], 1);
    assert_eq!(value["getRow"]["b"], 1.5);
    assert_eq!(value["getRow"]["c"], "x");
    assert_eq!(value["getRow"]["d0"], 1);
    assert_eq!(value["getProto"], true, "rows are null-prototype");
    assert_eq!(
        value["noMatchGet"],
        Value::Null,
        "no-match get -> undefined"
    );
    assert_eq!(value["noMatchAll"], json!([]));
    assert_eq!(value["allCount"], 2);
    assert_eq!(value["nullRow"]["b"], Value::Null);
    assert_eq!(value["nullRow"]["d"], Value::Null);
    assert_eq!(value["bigIntRangeGuard"], "ERR_OUT_OF_RANGE");
    assert_eq!(value["bigIntRead"], true);
    assert_eq!(value["named"]["a"], 1);
    assert_eq!(value["named"]["b"], 2);
    assert_eq!(value["named"]["c"], 3);
    assert_eq!(value["named"]["d"], 4);
    assert_eq!(value["unknownNamed"], true);
    assert_eq!(value["prefixedKey"], 7);
    assert_eq!(value["reuse"], json!([1, 2]));
    assert_eq!(value["isTransaction"], true);
    assert_eq!(value["afterRollback"]["isTransaction"], false);
    assert_eq!(
        value["afterRollback"]["gone"], 0,
        "ROLLBACK discards the insert"
    );
    assert_eq!(value["iterate"], json!([1, 2, 3]));
    assert_eq!(value["columns"], "x");
    assert_eq!(value["fts5"], true, "bundled SQLite must include FTS5");
    assert_eq!(value["isOpenAfterClose"], false);
    assert_eq!(value["closedPrepare"], "ERR_INVALID_STATE");
}

// ---------------------------------------------------------------------------
// better-sqlite3 virtual package (context-mode consumption surface)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn better_sqlite3_consumption_surface() {
    let dir = write_temp_fixture("better", &[]);
    let rt = new_runtime_at(&dir).await;

    const FIXTURE: &str = r#"
import { createRequire } from "node:module";
import Database from "better-sqlite3";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const fixtureDir = "__PIR_FENCE_DIR__";
    try {

    // CJS require path (context-mode does createRequire(...)(...)).
    const require = createRequire(import.meta.url);
    const required = require("better-sqlite3");
    out.cjsShape = {
      callable: typeof required === "function",
      defaultExport: typeof required.default === "function",
      staticExport: typeof required.Database === "function",
      verbose: typeof required.verbose === "function",
    };

    // Constructor options context-mode passes: { timeout: 30000 }.
    const db = new Database(":memory:", { timeout: 30000 });
    out.open = db.open;
    out.inTransactionInitial = db.inTransaction;

    // WAL-style pragma sequence (applyWALPragmas): scalar returns.
    db.exec("CREATE TABLE t(a)");
    out.pragmaUserVersion = db.pragma("user_version = 7");
    out.pragmaReadBack = db.pragma("user_version");
    const multi = db.pragma("table_info(t)");
    out.pragmaRows = Array.isArray(multi) && multi.length === 1 && multi[0].name === "a";

    // prepare/run/get/all + info numbers.
    const insert = db.prepare("INSERT INTO t VALUES (?)");
    const info = insert.run(1);
    out.info = { changes: info.changes, rowid: Number(info.lastInsertRowid) };
    out.get = db.prepare("SELECT a FROM t").get().a;
    out.all = db.prepare("SELECT a FROM t").all().length;
    out.noMatch = db.prepare("SELECT a FROM t WHERE a = 9").get();

    // iterate (store.js getDistinctiveTerms streams rows this way).
    db.exec("INSERT INTO t VALUES (2), (3)");
    const iterated: number[] = [];
    for (const row of db.prepare("SELECT a FROM t").iterate()) iterated.push(row.a);
    out.iterate = iterated;

    // Named parameters + bigint binding.
    out.named = db.prepare("SELECT @x a, :y b").get({ x: 1, y: 2 });
    db.exec("CREATE TABLE bigs(v INTEGER)");
    db.prepare("INSERT INTO bigs VALUES (?)").run(9007199254740993n);
    const bigStmt = db.prepare("SELECT v FROM bigs");
    bigStmt.setReadBigInts(true);
    out.bigint = String(bigStmt.get().v);
    bigStmt.setReadBigInts(false);
    let rangeGuard = "";
    try { bigStmt.get(); } catch (e: any) { rangeGuard = e.code; }
    out.rangeGuard = rangeGuard;

    // defaultSafeIntegers flips future statements to BigInt reads.
    db.defaultSafeIntegers(true);
    out.safeIntegers = typeof db.prepare("SELECT v FROM bigs").get().v === "bigint";
    db.defaultSafeIntegers(false);

    // transaction(): nested calls become SAVEPOINTs (better-sqlite3 model,
    // exercised by context-mode's session/db.js batched writes).
    const push = db.prepare("INSERT INTO t VALUES (?)");
    const batch = db.transaction((values: number[]) => {
      for (const v of values) push.run(v);
      return values.length;
    });
    out.transactionResult = batch([10, 11, 12]);
    out.transactionCount = db.prepare("SELECT COUNT(*) c FROM t").get().c;

    const nested = db.transaction(() => {
      const inner = db.transaction(() => {
        push.run(99);
        throw new Error("inner rollback");
      });
      try { inner(); } catch { /* swallowed */ }
      push.run(88);
      return "outer-committed";
    });
    out.nestedResult = nested();
    out.nestedSurvived = db.prepare("SELECT COUNT(*) c FROM t WHERE a = 99").get().c === 0;
    out.nestedCommitted = db.prepare("SELECT COUNT(*) c FROM t WHERE a = 88").get().c === 1;
    out.notInTransactionAfter = db.inTransaction === false;

    // deferred variant (variants live on the wrapped function).
    const defFactory = db.transaction((value: number) => {
      push.run(value);
      return value;
    });
    out.deferred = defFactory.deferred(13) === 13;
    out.deferredRow = db.prepare("SELECT COUNT(*) c FROM t WHERE a = 13").get().c === 1;

    // transaction rollback propagates.
    const failing = db.transaction(() => {
      push.run(77);
      throw new Error("fail");
    });
    let rolled = false;
    try { failing(); } catch { rolled = true; }
    out.rollbackPropagates = rolled;
    out.rollbackGone = db.prepare("SELECT COUNT(*) c FROM t WHERE a = 77").get().c === 0;

    // fileMustExist.
    let missing = "";
    try {
      new Database(fixtureDir + "/pir-sqlite-missing.db", { fileMustExist: true });
    } catch (e: any) { missing = "threw"; }
    out.fileMustExist = missing;

    // close + use-after-close.
    db.close();
    out.openAfterClose = db.open;
    let closed = "";
    try { db.prepare("SELECT 1"); } catch (e: any) { closed = e.code; }
    out.closedPrepare = closed;

    // verbose() returns the same constructor shape.
    const verboseDb = required.verbose().Database;
    out.verbose = typeof verboseDb === "function";
    } catch (err: any) {
      out.err = (err && err.stack) ? err.stack : String(err);
    }

    globalThis.__sqliteBetter = out;
  });
};
"#;
    let fixture = &FIXTURE.replace(
        "__PIR_FENCE_DIR__",
        &dir.to_string_lossy().replace('\\', "/"),
    );
    let value = run_fixture(&rt, "better", fixture, "__sqliteBetter").await;

    assert_eq!(value["cjsShape"]["callable"], true);
    assert_eq!(value["cjsShape"]["defaultExport"], true);
    assert_eq!(value["cjsShape"]["staticExport"], true);
    assert_eq!(value["cjsShape"]["verbose"], true);
    assert_eq!(value["open"], true);
    assert_eq!(value["inTransactionInitial"], false);
    // Assignment pragmas return no rows — better-sqlite3 yields undefined
    // (the key drops out of the JSON dump); reading it back yields the value.
    assert_eq!(value["pragmaUserVersion"], Value::Null);
    assert_eq!(value["pragmaReadBack"], 7);
    assert_eq!(value["pragmaRows"], true);
    assert_eq!(value["info"]["changes"], 1);
    assert_eq!(value["info"]["rowid"], 1);
    assert_eq!(value["get"], 1);
    assert_eq!(value["all"], 1);
    assert_eq!(value["noMatch"], Value::Null);
    assert_eq!(value["iterate"], json!([1, 2, 3]));
    assert_eq!(value["named"]["a"], 1);
    assert_eq!(value["named"]["b"], 2);
    assert_eq!(value["bigint"], "9007199254740993");
    assert_eq!(value["rangeGuard"], "ERR_OUT_OF_RANGE");
    assert_eq!(value["safeIntegers"], true);
    assert_eq!(value["transactionResult"], 3);
    assert_eq!(
        value["transactionCount"], 6,
        "1+2 prior rows + 3 batch rows"
    );
    assert_eq!(value["nestedResult"], "outer-committed");
    assert_eq!(value["nestedSurvived"], true, "inner savepoint rollback");
    assert_eq!(value["nestedCommitted"], true);
    assert_eq!(value["notInTransactionAfter"], true);
    assert_eq!(
        value["deferred"], true,
        "deferred variant ran and returned 13"
    );
    assert_eq!(value["deferredRow"], true);
    assert_eq!(value["rollbackPropagates"], true);
    assert_eq!(value["rollbackGone"], true);
    assert_eq!(value["fileMustExist"], "threw");
    assert_eq!(value["openAfterClose"], false);
    assert_eq!(value["closedPrepare"], "ERR_INVALID_STATE");
    assert_eq!(value["verbose"], true);
}

// ---------------------------------------------------------------------------
// Multi-handle concurrency + registry boot
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sqlite_multi_handle_and_boot_registry() {
    let dir = write_temp_fixture("multi", &[]);
    let rt = new_runtime_at(&dir).await;

    const FIXTURE: &str = r#"
import { DatabaseSync } from "node:sqlite";
import betterSqlite3 from "better-sqlite3";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    // Two independent handles stay isolated; interleaved use is fine.
    const a = new DatabaseSync(":memory:");
    const b = new DatabaseSync(":memory:");
    a.exec("CREATE TABLE t(v)");
    b.exec("CREATE TABLE t(v)");
    a.prepare("INSERT INTO t VALUES (?)").run(1);
    b.prepare("INSERT INTO t VALUES (?)").run(2);
    out.aOnly = a.prepare("SELECT COUNT(*) c FROM t").get().c;
    out.bOnly = b.prepare("SELECT COUNT(*) c FROM t").get().c;
    // better-sqlite3 handle alongside node:sqlite handles.
    const c = new betterSqlite3(":memory:");
    c.exec("CREATE TABLE t(v)");
    c.prepare("INSERT INTO t VALUES (?)").run(3);
    out.cOnly = c.prepare("SELECT COUNT(*) c FROM t").get().c;
    a.close();
    b.close();
    c.close();

    // Boot registry: CJS require("better-sqlite3") is preloaded at startup
    // and reports no boot failure.
    const mod = globalThis.__pirNodeModules["better-sqlite3"];
    out.registryEntry = typeof mod === "object" && mod !== null && typeof mod.default === "function";
    const bootErrors = globalThis.__pirBootErrors || [];
    out.bootErrors = bootErrors.filter((line: string) => line.indexOf("better-sqlite3") !== -1 || line.indexOf("sqlite") !== -1);

    globalThis.__sqliteMulti = out;
  });
};
"#;
    let value = run_fixture(&rt, "multi", FIXTURE, "__sqliteMulti").await;

    assert_eq!(value["aOnly"], 1);
    assert_eq!(value["bOnly"], 1);
    assert_eq!(value["cOnly"], 1);
    assert_eq!(value["registryEntry"], true);
    assert_eq!(value["bootErrors"], json!([]));
}

// ---------------------------------------------------------------------------
// fs write fence: database files outside the effective roots
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sqlite_file_write_fence() {
    let dir = write_temp_fixture("fence", &[]);
    let rt = new_runtime_at(&dir).await;

    // A path guaranteed outside every effective root (temp dir, cwd, ~/.pi,
    // the agent dir): the filesystem root itself.
    let outside = if cfg!(windows) {
        "C:/pir-sqlite-fence-outside.db".to_string()
    } else {
        "/pir-sqlite-fence-outside.db".to_string()
    };
    let inside = dir.join("inside.db").to_string_lossy().replace('\\', "/");

    const FIXTURE_TEMPLATE: &str = r#"
import { DatabaseSync } from "node:sqlite";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const outside = __PIR_OUTSIDE__;
    const inside = __PIR_INSIDE__;

    // Read-write open outside the roots: fenced like an fs write target.
    try {
      new DatabaseSync(outside);
      out.outsideRw = "opened";
    } catch (e: any) {
      out.outsideRw = e.code;
    }

    // Read-only opens are reads — the fence must NOT reject them (the failure
    // mode becomes SQLite's own CANTOPEN, not EACCES).
    try {
      new DatabaseSync(outside, { readOnly: true });
      out.outsideRo = "opened";
    } catch (e: any) {
      out.outsideRo = e.code;
    }

    // Read-write open inside the roots: works end to end (create + persist +
    // reopen with a second handle).
    const db = new DatabaseSync(inside);
    db.exec("CREATE TABLE t(v)");
    db.prepare("INSERT INTO t VALUES (?)").run(5);
    db.close();
    const again = new DatabaseSync(inside);
    out.reopened = again.prepare("SELECT v FROM t").get().v;
    again.close();

    // better-sqlite3 rides the same fence.
    try {
      new (globalThis.__pirNodeModules["better-sqlite3"].default)(outside);
      out.betterOutside = "opened";
    } catch (e: any) {
      out.betterOutside = e.code;
    }

    globalThis.__sqliteFence = out;
  });
};
"#;
    let fixture = &FIXTURE_TEMPLATE
        .replace("__PIR_OUTSIDE__", &format!("{outside:?}"))
        .replace("__PIR_INSIDE__", &format!("{inside:?}"));
    let value = run_fixture(&rt, "fence", fixture, "__sqliteFence").await;

    assert_eq!(value["outsideRw"], "EACCES", "read-write open is fenced");
    assert_eq!(
        value["outsideRo"], "ERR_SQLITE_ERROR",
        "read-only open skips the fence; SQLite itself fails to open the missing file"
    );
    assert_eq!(value["reopened"], 5, "file db persists across handles");
    assert_eq!(value["betterOutside"], "EACCES");
}

// ---------------------------------------------------------------------------
// sqlite3 virtual package (native node_sqlite3.node re-created on the bridge)
// ---------------------------------------------------------------------------

/// The `sqlite3` shim: the census probe (`require("sqlite3")`) plus the real
/// consumer shape — `new sqlite3.Database(file, cb)` followed by
/// `db.run("CREATE TABLE ...", cb)` — and the callback/Statement surface the
/// addon's users (knex/typeorm-style drivers, migration scripts) rely on.
#[tokio::test]
async fn sqlite3_virtual_package_surface() {
    let dir = write_temp_fixture("sqlite3", &[]);
    let rt = new_runtime_at(&dir).await;
    let file = dir
        .join("sqlite3-shim.db")
        .to_string_lossy()
        .replace('\\', "/");

    const FIXTURE_TEMPLATE: &str = r#"
import sqlite3 from "sqlite3";
import { createRequire } from "node:module";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const file = __PIR_SQLITE3_FILE__;

    // Constants: the real binding's export values.
    out.constants = [
      sqlite3.OPEN_READONLY, sqlite3.OPEN_READWRITE, sqlite3.OPEN_CREATE,
      sqlite3.OPEN_FULLMUTEX, sqlite3.OPEN_SHAREDCACHE, sqlite3.OPEN_PRIVATECACHE,
      sqlite3.OPEN_URI, sqlite3.OK, sqlite3.CONSTRAINT, sqlite3.BUSY, sqlite3.RANGE,
      sqlite3.LIMIT_WORKER_THREADS,
    ];
    out.verbose = typeof sqlite3.verbose() === "object"
      && sqlite3.verbose().Database === sqlite3.Database;

    // CJS require path (the census probe's own shape).
    const require = createRequire(import.meta.url);
    const required = require("sqlite3");
    out.cjs = [typeof required, typeof required.Database, typeof required.verbose, typeof required.cached.Database];
    out.cjsSameClass = required.Database === sqlite3.Database;

    // `new Database(file, cb)` + `db.run("CREATE TABLE ...", cb)`.
    let opened: any = null;
    const openSeen = await new Promise<any>((resolve) => {
      opened = new sqlite3.Database(file, function (err: any) { resolve([err, this === opened]); });
    });
    out.open = [openSeen[0], openSeen[1], opened.open];

    const db = new sqlite3.Database(file);
    const runResult = await new Promise<any>((resolve) => {
      db.run("CREATE TABLE t (x INTEGER, y TEXT)", function (err: any) {
        resolve([err, this.lastID, this.changes, this instanceof sqlite3.Statement, this.constructor.name]);
      });
    });
    out.create = runResult;

    // Spread params, array params and named params all bind.
    const inserts = await Promise.all([
      new Promise<any>((resolve) => db.run("INSERT INTO t (x,y) VALUES (?,?)", 1, "one", function (err: any) { resolve([err, this.lastID, this.changes]); })),
      new Promise<any>((resolve) => db.run("INSERT INTO t (x,y) VALUES (?,?)", [2, "two"], function (err: any) { resolve([err, this.lastID]); })),
      new Promise<any>((resolve) => db.run("INSERT INTO t (x,y) VALUES ($x,$y)", { $x: 3, $y: "three" }, function (err: any) { resolve([err, this.lastID]); })),
    ]);
    out.inserts = inserts;

    // get / all / each / map with plain-prototype rows and Buffer blobs.
    out.get = await new Promise<any>((resolve) => db.get("SELECT x,y FROM t WHERE x=?", 2, (err: any, row: any) => resolve([err, row, Object.getPrototypeOf(row) === Object.prototype])));
    out.noRow = await new Promise<any>((resolve) => db.get("SELECT x FROM t WHERE x=?", 99, (err: any, row: any) => resolve([err, row === undefined])));
    out.all = await new Promise<any>((resolve) => db.all("SELECT x FROM t ORDER BY x", (err: any, rows: any) => resolve(rows)));
    const eachRows: any[] = [];
    const eachDone = await new Promise<any>((resolve) => {
      db.each("SELECT x FROM t ORDER BY x", (err: any, row: any) => { if (row) eachRows.push(row.x); }, (err: any, count: any) => resolve([err, count]));
    });
    out.each = [eachRows, eachDone];
    out.map = await new Promise<any>((resolve) => db.map("SELECT y,x FROM t ORDER BY x LIMIT 2", (err: any, map: any) => resolve(map)));
    out.blob = await new Promise<any>((resolve) => db.get("SELECT ? AS v", Buffer.from("ab"), (err: any, row: any) => resolve([Buffer.isBuffer(row.v), row.v.toString()])));

    // prepare / bind / reset / finalize.
    const st = db.prepare("SELECT x FROM t WHERE x>?", (err: any) => { out.prepareCb = err; });
    out.prepared = st.constructor.name;
    out.stmtAll = await new Promise<any>((resolve) => st.all(1, (err: any, rows: any) => resolve(rows.map((r: any) => r.x))));
    out.stmtBound = await new Promise<any>((resolve) => {
      st.bind(2, () => { st.all((err: any, rows: any) => resolve(rows.map((r: any) => r.x))); });
    });
    out.finalizeReturnsDb = st.finalize() === db;

    // Error shape: SQLITE_* code + errno + the addon's "<CODE>: <message>".
    out.error = await new Promise<any>((resolve) => db.run("INSERT INTO missing VALUES (1)", (err: any) => resolve([err.message, err.code, err.errno])));
    db.run("CREATE TABLE u (x UNIQUE)");
    db.run("INSERT INTO u VALUES (1)");
    out.constraint = await new Promise<any>((resolve) => db.run("INSERT INTO u VALUES (1)", (err: any) => resolve([err.message, err.code, err.errno])));

    // exec (multi-statement), events, serialize/parallelize, close.
    out.exec = await new Promise<any>((resolve) => {
      db.exec("CREATE TABLE v (z); INSERT INTO v VALUES (7);", function (err: any) { resolve([err, this === db]); });
    });
    const seen: any = { trace: 0, profile: 0, change: [] };
    db.on("trace", () => { seen.trace++; });
    db.on("profile", (sql: any, ms: any) => { if (typeof ms === "number") seen.profile++; });
    db.on("change", (type: any, dbase: any, table: any, rowid: any) => seen.change.push([type, dbase, table, rowid]));
    db.run("INSERT INTO v VALUES (8)");
    db.configure("busyTimeout", 200);
    out.busyTimeout = await new Promise<any>((resolve) => db.all("PRAGMA busy_timeout", (err: any, rows: any) => resolve(rows[0].timeout)));
    db.serialize(() => { seen.serialized = true; });
    db.parallelize(() => { seen.parallelized = true; });
    out.events = seen;
    await new Promise<any>((resolve) => db.close(() => resolve(null)));
    out.closedOpen = db.open;

    // Reopen the same file: rows persisted through the host SQLite backend.
    out.reopened = await new Promise<any>((resolve) => {
      const again = new sqlite3.Database(file, () => {
        again.all("SELECT COUNT(*) c FROM t", (err: any, rows: any) => { again.close(() => resolve(rows[0].c)); });
      });
    });

    // cached.Database returns the same handle for a file.
    const cachedA = sqlite3.cached.Database(file);
    out.cached = sqlite3.cached.Database(file) === cachedA && cachedA instanceof sqlite3.Database;
    cachedA.close();

    // Boot registry: the pre-imported virtual module + no boot failure.
    const mod = (globalThis as any).__pirNodeModules["sqlite3"];
    out.registry = typeof mod === "object" && mod !== null && typeof mod.Database === "function";
    out.bootErrors = ((globalThis as any).__pirBootErrors || []).filter((line: string) => line.indexOf("sqlite3") !== -1);

    globalThis.__sqlite3Surface = out;
  });
};
"#;
    let fixture = &FIXTURE_TEMPLATE.replace("__PIR_SQLITE3_FILE__", &format!("{file:?}"));
    let value = run_fixture(&rt, "sqlite3", fixture, "__sqlite3Surface").await;

    assert_eq!(
        value["constants"],
        json!([1, 2, 4, 65536, 131072, 262144, 64, 0, 19, 5, 25, 11])
    );
    assert_eq!(value["verbose"], true);
    assert_eq!(
        value["cjs"],
        json!(["object", "function", "function", "function"])
    );
    assert_eq!(value["cjsSameClass"], true);
    assert_eq!(value["open"], json!([Value::Null, true, true]));
    assert_eq!(
        value["create"],
        json!([Value::Null, 0, 0, true, "Statement"])
    );
    assert_eq!(
        value["inserts"],
        json!([[Value::Null, 1, 1], [Value::Null, 2], [Value::Null, 3],])
    );
    assert_eq!(
        value["get"],
        json!([Value::Null, {"x": 2, "y": "two"}, true])
    );
    assert_eq!(value["noRow"], json!([Value::Null, true]));
    assert_eq!(value["all"], json!([{"x": 1}, {"x": 2}, {"x": 3}]));
    assert_eq!(value["each"], json!([[1, 2, 3], [Value::Null, 3]]));
    assert_eq!(value["map"], json!({"one": 1, "two": 2}));
    assert_eq!(value["blob"], json!([true, "ab"]));
    assert_eq!(value["prepareCb"], Value::Null);
    assert_eq!(value["prepared"], "Statement");
    assert_eq!(value["stmtAll"], json!([2, 3]));
    assert_eq!(value["stmtBound"], json!([3]));
    assert_eq!(value["finalizeReturnsDb"], true);
    assert_eq!(
        value["error"],
        json!(["SQLITE_ERROR: no such table: missing", "SQLITE_ERROR", 1])
    );
    assert_eq!(
        value["constraint"],
        json!([
            "SQLITE_CONSTRAINT: UNIQUE constraint failed: u.x",
            "SQLITE_CONSTRAINT",
            19
        ])
    );
    assert_eq!(value["exec"], json!([Value::Null, true]));
    assert_eq!(value["busyTimeout"], 200);
    assert!(
        value["events"]["trace"].as_u64().unwrap_or(0) > 0,
        "trace events fire"
    );
    assert!(
        value["events"]["profile"].as_u64().unwrap_or(0) > 0,
        "profile events fire"
    );
    // 'change' is emitted for the single-statement INSERT that ran *after* the
    // listener was registered; the earlier `t` inserts predate it and the
    // multi-statement exec is typed by its first keyword ("CREATE"), so
    // neither shows up (documented approximation of sqlite3_update_hook).
    assert_eq!(
        value["events"]["change"],
        json!([["insert", "main", "v", 2]])
    );
    assert_eq!(value["events"]["serialized"], true);
    assert_eq!(value["events"]["parallelized"], true);
    assert_eq!(value["closedOpen"], false);
    assert_eq!(value["reopened"], 3);
    assert_eq!(value["cached"], true);
    assert_eq!(value["registry"], true);
    assert_eq!(value["bootErrors"], json!([]));
}

// ---------------------------------------------------------------------------
// Columnar fast path + legacy wire-format fallback (host contract)
// ---------------------------------------------------------------------------

/// The new shim asks for `columnar` rows and flat `values` bindings; the host
/// must still answer the legacy `binds` + row-object format that a stale AOT
/// shim blob (built before this change) sends. Both are exercised directly on
/// the host op so the fallback is diff-tested rather than assumed.
#[tokio::test]
async fn sqlite_columnar_and_legacy_wire_formats() {
    let dir = write_temp_fixture("wire", &[]);
    let rt = new_runtime_at(&dir).await;

    const FIXTURE: &str = r#"
import { DatabaseSync } from "node:sqlite";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const db = new DatabaseSync(":memory:");
    db.exec("CREATE TABLE t(a INTEGER, b TEXT)");
    db.exec("INSERT INTO t VALUES (1,'x'), (2,'y'), (3,NULL)");
    const dbId = (db as any)._pirId;

    // Fast path requested explicitly: columns once, rows as arrays.
    out.columnarAll = globalThis.__pirNodeCall("sqlite", "all", {
      id: dbId, sql: "SELECT a, b FROM t WHERE a > ? ORDER BY a",
      values: [1], columnar: true,
    });
    out.columnarGet = globalThis.__pirNodeCall("sqlite", "get", {
      id: dbId, sql: "SELECT a, b FROM t WHERE a = ?", values: [2], columnar: true,
    });
    out.columnarGetMissing = globalThis.__pirNodeCall("sqlite", "get", {
      id: dbId, sql: "SELECT a, b FROM t WHERE a = ?", values: [99], columnar: true,
    });

    // Named-parameter index maps (flat values + non-sequential indices).
    db.exec("CREATE TABLE n(x INTEGER, y INTEGER)");
    out.flatIndices = globalThis.__pirNodeCall("sqlite", "all", {
      id: dbId, sql: "SELECT ?2 AS b, ?1 AS a",
      values: [20], indices: [2], columnar: true,
    });

    // Legacy format: object rows built host-side from `binds`.
    out.legacyAll = globalThis.__pirNodeCall("sqlite", "all", {
      id: dbId, sql: "SELECT a, b FROM t WHERE a > ? ORDER BY a",
      binds: [{ index: 1, value: 1 }],
    });
    out.legacyGet = globalThis.__pirNodeCall("sqlite", "get", {
      id: dbId, sql: "SELECT a, b FROM t WHERE a = ?",
      binds: [{ index: 1, value: 2 }],
    });
    out.legacyGetMissing = globalThis.__pirNodeCall("sqlite", "get", {
      id: dbId, sql: "SELECT a FROM t WHERE a = ?",
      binds: [{ index: 1, value: 99 }],
    });
    out.legacyArrays = globalThis.__pirNodeCall("sqlite", "all", {
      id: dbId, sql: "SELECT a FROM t ORDER BY a",
      binds: [], returnArrays: true,
    });
    out.legacyRun = globalThis.__pirNodeCall("sqlite", "run", {
      id: dbId, sql: "INSERT INTO t VALUES (?, ?)",
      binds: [{ index: 1, value: 4 }, { index: 2, value: "w" }],
    });

    // Schema change: column names come from the live prepared statement.
    // SQLite recompiles a cached statement on its next step, so a metadata
    // read *before* stepping legitimately reports the old shape (the previous
    // shim behaved the same way); stepping refreshes it.
    const before = db.prepare("SELECT * FROM t");
    out.beforeColumns = before.columns().map((c: any) => c.name).join(",");
    db.exec("ALTER TABLE t ADD COLUMN c TEXT");
    out.afterColumnsBeforeStep = before.columns().map((c: any) => c.name).join(",");
    const refreshed = before.get();
    out.afterStepRowKeys = refreshed === undefined ? "undefined" : Object.keys(refreshed).join(",");
    out.afterStepColumns = before.columns().map((c: any) => c.name).join(",");
    out.afterRowKeys = Object.keys(db.prepare("SELECT * FROM t WHERE a = 1").get()).join(",");

    // Flat named bindings with a gap: only $z is provided, the others are NULL.
    out.namedGap = db.prepare("SELECT $x a, $y b, $z c").get({ z: 9 });
    // ?NNN positional map.
    out.numbered = db.prepare("SELECT ?2 AS b, ?1 AS a").get(1, 2);
    // Node binds booleans as INTEGER 0/1.
    out.booleanBind = db.prepare("SELECT ? a, ? b").get(true, false);

    globalThis.__sqliteWire = out;
  });
};
"#;
    let value = run_fixture(&rt, "wire", FIXTURE, "__sqliteWire").await;

    assert_eq!(value["columnarAll"]["columns"], json!(["a", "b"]));
    assert_eq!(
        value["columnarAll"]["rows"],
        json!([[2, "y"], [3, Value::Null]])
    );
    assert_eq!(value["columnarGet"]["columns"], json!(["a", "b"]));
    assert_eq!(value["columnarGet"]["row"], json!([2, "y"]));
    assert_eq!(value["columnarGetMissing"]["row"], Value::Null);
    assert_eq!(value["flatIndices"]["columns"], json!(["b", "a"]));
    assert_eq!(value["flatIndices"]["rows"], json!([[20, Value::Null]]));

    assert_eq!(
        value["legacyAll"],
        json!([{"a": 2, "b": "y"}, {"a": 3, "b": Value::Null}])
    );
    assert_eq!(value["legacyGet"], json!({"a": 2, "b": "y"}));
    assert_eq!(value["legacyGetMissing"], Value::Null);
    assert_eq!(value["legacyArrays"], json!([[1], [2], [3]]));
    assert_eq!(value["legacyRun"]["changes"], 1);
    assert_eq!(value["legacyRun"]["lastInsertRowid"], 4);

    assert_eq!(value["beforeColumns"], "a,b");
    assert_eq!(
        value["afterColumnsBeforeStep"], "a,b",
        "metadata before the recompile step reports the old shape"
    );
    // Node reflects a schema change in the very execution that triggers
    // SQLite's recompile: the row set and the live column list both report
    // `a,b,c` after the first post-ALTER step.
    assert_eq!(value["afterStepRowKeys"], "a,b,c");
    assert_eq!(value["afterStepColumns"], "a,b,c");
    assert_eq!(value["afterRowKeys"], "a,b,c");
    assert_eq!(
        value["namedGap"],
        json!({"a": Value::Null, "b": Value::Null, "c": 9})
    );
    assert_eq!(value["numbered"], json!({"a": 1, "b": 2}));
    assert_eq!(value["booleanBind"], json!({"a": 1, "b": 0}));

    // Closed handles drop the cache with the connection.
    let _ = &rt;
}

// ---------------------------------------------------------------------------
// better-sqlite3 row modes over the columnar path
// ---------------------------------------------------------------------------

/// `.pluck()`, `.raw()`, `.expand()`, `.columns()` and `.safeIntegers()` are
/// shaped by the virtual better-sqlite3 shim from the rows `node:sqlite`
/// hands it. This pins their behavior now that those rows arrive as columnar
/// arrays assembled JS-side.
#[tokio::test]
async fn better_sqlite3_row_modes() {
    let dir = write_temp_fixture("rowmodes", &[]);
    let rt = new_runtime_at(&dir).await;

    const FIXTURE: &str = r#"
import Database from "better-sqlite3";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const step = (name: string, fn: () => any) => {
      try { out[name] = fn(); } catch (e: any) { out[name] = "ERR: " + e.message; }
    };
    const db = new Database(":memory:");
    db.exec(
      "CREATE TABLE a(id INTEGER, name TEXT);" +
      "INSERT INTO a VALUES (1,'x'), (2,'y');" +
      "CREATE TABLE b(aid INTEGER, v TEXT);" +
      "INSERT INTO b VALUES (1,'p'), (1,'q');"
    );

    step("pluck", () => db.prepare("SELECT name FROM a ORDER BY id").pluck().all());
    step("pluckGet", () => db.prepare("SELECT name FROM a WHERE id = ?").pluck().get(2));

    const raw = db.prepare("SELECT id, name FROM a ORDER BY id").raw();
    step("rawAll", () => raw.all());
    step("rawGet", () => raw.get());
    step("rawColumns", () => raw.columns().map((c: any) => c.name));

    const expand = db.prepare(
      "SELECT a.id AS 'a.id', a.name AS 'a.name', b.v AS 'b.v' " +
      "FROM a JOIN b ON b.aid = a.id WHERE a.id = 1"
    ).expand();
    step("expand", () => expand.all());

    step("columns", () => db.prepare("SELECT id AS x, name AS y FROM a").columns().map((c: any) => c.name));

    db.defaultSafeIntegers(true);
    step("safe", () => typeof db.prepare("SELECT id FROM a").get().id);
    db.defaultSafeIntegers(false);
    step("backToNumbers", () => typeof db.prepare("SELECT id FROM a").get().id);

    // Toggling raw off returns to object rows.
    step("rawOff", () => db.prepare("SELECT id FROM a ORDER BY id").raw(false).all());

    db.close();
    globalThis.__sqliteRowModes = out;
  });
};
"#;
    let value = run_fixture(&rt, "rowmodes", FIXTURE, "__sqliteRowModes").await;

    assert_eq!(value["pluck"], json!(["x", "y"]));
    assert_eq!(value["pluckGet"], "y");
    assert_eq!(value["rawAll"], json!([[1, "x"], [2, "y"]]));
    assert_eq!(value["rawGet"], json!([1, "x"]));
    assert_eq!(value["rawColumns"], json!(["id", "name"]));
    assert_eq!(
        value["expand"],
        json!([
            {"a": {"id": 1, "name": "x"}, "b": {"v": "p"}},
            {"a": {"id": 1, "name": "x"}, "b": {"v": "q"}}
        ])
    );
    assert_eq!(value["columns"], json!(["x", "y"]));
    assert_eq!(value["safe"], "bigint");
    assert_eq!(value["backToNumbers"], "number");
    assert_eq!(value["rawOff"], json!([{"id": 1}, {"id": 2}]));
}

// ---------------------------------------------------------------------------
// Typed C-ABI entry: __pirSqliteQuery
// ---------------------------------------------------------------------------

/// The typed entry (`host_functions/sqlite_ops.rs`) is the path the shims now
/// prefer: live params in, live rows out, no JSON envelope. This pins its
/// contract independently of the shim (who owns which shape, what falls back,
/// and what throws after acceptance) and proves the shim actually rides it
/// (BLOB cells are zero-copy `Uint8Array`s, not the envelope path's Buffers).
#[tokio::test]
async fn sqlite_typed_query_entry_contract() {
    let dir = write_temp_fixture("typed", &[]);
    let rt = new_runtime_at(&dir).await;

    const FIXTURE: &str = r#"
import { DatabaseSync } from "node:sqlite";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const q = (globalThis as any).__pirSqliteQuery;
    out.present = typeof q === "function";

    const db = new DatabaseSync(":memory:");
    db.exec("CREATE TABLE t(a INTEGER, b TEXT, c REAL, d BLOB, big INTEGER)");
    db.exec("INSERT INTO t VALUES (1,'x',1.5,x'0102',9007199254740993), (2,'y',-0.5,NULL,NULL)");
    const dbId = (db as any)._pirId;
    const allSql = "SELECT a, b, c, d FROM t ORDER BY a";

    // Default shape: array of null-prototype row objects.
    const rows = q(dbId, allSql, [], "all", {});
    out.allLen = rows.length;
    out.allValues = [rows[0].a, rows[0].b, rows[0].c, rows[1].a, rows[1].b];
    out.allProto = Object.getPrototypeOf(rows[0]) === null;
    out.blobType = rows[0].d instanceof Uint8Array ? "u8" : typeof rows[0].d;
    out.blobIsBuffer = Buffer.isBuffer(rows[0].d);
    out.blobBytes = [rows[0].d[0], rows[0].d[1]];
    out.blobNull = rows[1].d === null;

    // Columnar / raw / pluck shapes.
    const columnar = q(dbId, "SELECT a, b FROM t ORDER BY a", [], "all", { columnar: true });
    out.columnar = columnar;
    out.raw = q(dbId, "SELECT a, b FROM t ORDER BY a", [], "all", { raw: true });
    out.pluck = q(dbId, "SELECT b FROM t ORDER BY a", [], "all", { pluck: true });
    out.columnarGet = q(dbId, "SELECT a, b FROM t WHERE a = 1", [], "get", { columnar: true });
    out.columnarGetMiss = q(dbId, "SELECT a FROM t WHERE a = 99", [], "get", { columnar: true });

    // get hit/miss; run counters.
    out.getHit = q(dbId, "SELECT a, b FROM t WHERE a = ?", [2], "get", {});
    out.getMiss = q(dbId, "SELECT a FROM t WHERE a = ?", [99], "get", {}) === undefined;
    out.run = q(dbId, "INSERT INTO t VALUES (?,?,?,?,?)", [3, "z", 2.5, null, null], "run", {});
    const runSafe = q(dbId, "INSERT INTO t VALUES (?,?,?,?,?)", [4, "w", null, null, null], "run", { safeIntegers: true });
    out.runSafe = {
      changes: typeof runSafe.changes + ":" + String(runSafe.changes),
      rowid: typeof runSafe.lastInsertRowid + ":" + String(runSafe.lastInsertRowid),
    };
    out.iterate = q(dbId, "SELECT a FROM t ORDER BY a", [], "iterate", {}).map((r: any) => r.a);

    // safeIntegers: INTEGER cells and run counters as BigInt.
    const safe = q(dbId, "SELECT big FROM t WHERE a = 1", [], "get", { safeIntegers: true });
    out.safeBig = typeof safe.big === "bigint" && String(safe.big);

    // Out-of-f64-range INTEGER without BigInt reads: the entry has already
    // stepped the statement, so it must THROW (falling back would execute the
    // statement twice — audit finding B2). The shim propagates the same
    // RangeError Node raises.
    let direct = "";
    try { q(dbId, "SELECT big FROM t WHERE a = 1", [], "get", {}); } catch (e: any) { direct = e.code; }
    out.unsafeDirect = direct;
    let range = "";
    try { db.prepare("SELECT big FROM t WHERE a = 1").get(); } catch (e: any) { range = e.code; }
    out.rangeGuard = range;
    let allRange = "";
    try { db.prepare("SELECT big FROM t").all(); } catch (e: any) { allRange = e.code; }
    out.allRangeGuard = allRange;

    // Regression (audit B2): a statement that both writes and returns an
    // out-of-range INTEGER must not run twice. The error is raised after the
    // step, and exactly one row is inserted.
    db.exec("CREATE TABLE ins(v INTEGER)");
    const returning = db.prepare("INSERT INTO ins(v) VALUES (1) RETURNING v, 9007199254740993 AS big");
    let insRange = "";
    try { returning.all(); } catch (e: any) { insRange = e.code; }
    out.insertRange = insRange;
    out.insertCount = db.prepare("SELECT count(*) AS c FROM ins").get().c;

    // Named binds: bare keys resolve against @/:/$; prefixed keys verbatim.
    out.named = q(dbId, "SELECT @x a, :y b", { x: 7, y: 8 }, "get", {});
    out.namedPrefixed = q(dbId, "SELECT @x a, :y b", { "@x": 7, ":y": 8 }, "get", {});
    out.namedMismatch = q(dbId, "SELECT @x a, :y b", { "@y": 8 }, "get", {});
    // The shim path must agree (Node's unknown-name check strips the prefix,
    // so "@y" is a known name that matches no slot: both columns stay NULL).
    out.namedMismatchShim = db.prepare("SELECT @x a, :y b").get({ "@y": 8 });
    // One bare key binds every slot whose bare name matches (Node semantics).
    out.namedBoth = q(dbId, "SELECT @x a, :x b", { x: 5 }, "get", {});
    out.namedNull = q(dbId, "SELECT @x a", {}, "get", {}).a;

    // Not-owned shapes answer undefined.
    out.badHandle = q(999999, "SELECT 1", [], "all", {}) === undefined;
    out.badMode = q(dbId, "SELECT 1", [], "bogus", {}) === undefined;
    out.badSql = q(dbId, 42, [], "all", {}) === undefined;
    out.badParam = q(dbId, "SELECT ?", [() => 1], "get", {}) === undefined;
    out.badParamObject = q(dbId, "SELECT 1", { d: new Date() }, "get", {}) === undefined;
    out.unknownNamed = q(dbId, "SELECT @x a", { zzz: 1 }, "get", {}) === undefined;

    // Accepted shape + failing SQL: throws the envelope-shaped error.
    let failure: any = null;
    try { q(dbId, "INSERT INTO nope VALUES (1)", [], "run", {}); } catch (e: any) { failure = e; }
    out.throws = failure !== null && failure.code === "ERR_SQLITE_ERROR" &&
      String(failure.message).indexOf("no such table") !== -1;

    // Statement reuse: the compiled statement is cached across typed calls.
    q(dbId, "INSERT INTO t VALUES (10, 'q', 0, NULL, NULL)", [], "run", {});
    out.reuse = q(dbId, "SELECT COUNT(*) c FROM t WHERE a >= 10", [], "get", {}).c;

    // The shim's row modes over the typed rows.
    const arrayStmt = db.prepare("SELECT a, b FROM t ORDER BY a LIMIT 2");
    arrayStmt.setReturnArrays(true);
    out.rawShim = arrayStmt.all();
    arrayStmt.setReturnArrays(false);
    out.pluckBetter = (globalThis as any).__pirNodeModules["better-sqlite3"].default;
    const better = new out.pluckBetter(":memory:");
    better.exec("CREATE TABLE p(a INTEGER, b TEXT); INSERT INTO p VALUES (1,'u'),(2,'v')");
    out.betterPluck = better.prepare("SELECT b FROM p ORDER BY a").pluck().all();
    out.betterRaw = better.prepare("SELECT a, b FROM p ORDER BY a").raw().all();
    out.betterCols = better.prepare("SELECT a AS x FROM p").columns().map((c: any) => c.name);
    better.defaultSafeIntegers(true);
    out.betterSafe = typeof better.prepare("SELECT a FROM p").get().a;
    better.defaultSafeIntegers(false);
    better.close();

    db.close();
    globalThis.__sqliteTyped = out;
  });
};
"#;
    let value = run_fixture(&rt, "typed", FIXTURE, "__sqliteTyped").await;

    assert_eq!(value["present"], true, "typed entry is installed");
    assert_eq!(value["allLen"], 2);
    assert_eq!(value["allValues"], json!([1, "x", 1.5, 2, "y"]));
    assert_eq!(value["allProto"], true, "typed rows are null-prototype");
    assert_eq!(value["blobType"], "u8");
    assert_eq!(
        value["blobIsBuffer"], false,
        "BLOB cells are Uint8Array views, not envelope Buffers -> the shim used the typed path"
    );
    assert_eq!(value["blobBytes"], json!([1, 2]));
    assert_eq!(value["blobNull"], true);

    assert_eq!(value["columnar"]["columns"], json!(["a", "b"]));
    assert_eq!(value["columnar"]["rows"], json!([[1, "x"], [2, "y"]]));
    assert_eq!(value["raw"], json!([[1, "x"], [2, "y"]]));
    assert_eq!(value["pluck"], json!(["x", "y"]));
    assert_eq!(value["columnarGet"]["row"], json!([1, "x"]));
    assert_eq!(value["columnarGetMiss"]["row"], Value::Null);

    assert_eq!(value["getHit"], json!({"a": 2, "b": "y"}));
    assert_eq!(value["getMiss"], true);
    assert_eq!(value["run"], json!({"changes": 1, "lastInsertRowid": 3}));
    assert_eq!(
        value["runSafe"],
        json!({"changes": "bigint:1", "rowid": "bigint:4"}),
        "run counters are BigInts when safeIntegers is set"
    );
    assert_eq!(value["iterate"], json!([1, 2, 3, 4]));
    assert_eq!(value["safeBig"], "9007199254740993");
    assert_eq!(
        value["unsafeDirect"], "ERR_OUT_OF_RANGE",
        "the entry throws after stepping instead of falling back"
    );
    assert_eq!(value["rangeGuard"], "ERR_OUT_OF_RANGE");
    assert_eq!(value["allRangeGuard"], "ERR_OUT_OF_RANGE");
    // Audit B2: INSERT ... RETURNING <big int> must not run twice.
    assert_eq!(value["insertRange"], "ERR_OUT_OF_RANGE");
    assert_eq!(
        value["insertCount"], 1,
        "statement must execute exactly once"
    );
    assert_eq!(value["named"], json!({"a": 7, "b": 8}));
    assert_eq!(value["namedPrefixed"], json!({"a": 7, "b": 8}));
    assert_eq!(
        value["namedMismatch"],
        json!({"a": Value::Null, "b": Value::Null})
    );
    assert_eq!(
        value["namedMismatchShim"],
        json!({"a": Value::Null, "b": Value::Null}),
        "typed and envelope paths agree on an off-prefix named key"
    );
    assert_eq!(
        value["namedBoth"],
        json!({"a": 5, "b": 5}),
        "a bare key binds every same-named slot"
    );
    assert_eq!(value["namedNull"], Value::Null);

    assert_eq!(value["badHandle"], true);
    assert_eq!(value["badMode"], true);
    assert_eq!(value["badSql"], true);
    assert_eq!(value["badParam"], true, "a function bind is not owned");
    assert_eq!(value["badParamObject"], true, "a Date bind is not owned");
    assert_eq!(
        value["unknownNamed"], true,
        "an unknown named key falls back"
    );
    assert_eq!(value["throws"], true, "accepted shape + failing SQL throws");
    assert_eq!(value["reuse"], 1);

    assert_eq!(value["rawShim"], json!([[1, "x"], [2, "y"]]));
    assert_eq!(value["betterPluck"], json!(["u", "v"]));
    assert_eq!(value["betterRaw"], json!([[1, "u"], [2, "v"]]));
    assert_eq!(value["betterCols"], json!(["x"]));
    assert_eq!(value["betterSafe"], "bigint");
}

// ---------------------------------------------------------------------------
// Throughput benchmark: node:sqlite shim (host rusqlite) vs real Node 24
// ---------------------------------------------------------------------------
//
// The fair baseline is Node 24's built-in `node:sqlite` (`DatabaseSync`), which
// does the same work through the same engine (SQLite) — better-sqlite3 is not
// installed in this environment, so it cannot serve as the reference. The
// in-runtime measurement runs inside the extension VM; the Node measurement
// runs in a child `node` process when one is on PATH. Ignored by default so
// ordinary `cargo test` runs do not pay for it; `--include-ignored` runs it.
//
// Numbers printed:
//   insert1000   one transaction, 1000 rows x 5 columns, fresh :memory: db
//   select1000   .all() over 1000 rows x 5 columns
//   iterate1000  .iterate() over the same rows
//   get1000      a batch of 1000 `.get()` calls with a bound key (5-col row)
//   getConst1000 a batch of 1000 `.get()` calls with a constant key (5-col)
//
// Every sample times a batch of operations (see `bench`), because the
// extension VM's `performance.now()` is Date.now-based (1 ms resolution).

const BENCH_NODE_SCRIPT: &str = r#"
const { DatabaseSync } = require("node:sqlite");
// Batched timing: each sample runs `batch` operations, so a sub-ms operation
// is still measured over a span well above timer resolution. The in-runtime
// benchmark uses exactly the same shape.
function bench(warm, iters, batch, fn) {
  for (let i = 0; i < warm; i++) fn();
  const samples = [];
  for (let i = 0; i < iters; i++) {
    const t0 = performance.now();
    for (let b = 0; b < batch; b++) fn();
    samples.push((performance.now() - t0) / batch);
  }
  samples.sort((a, b) => a - b);
  return { median: samples[samples.length >> 1], min: samples[0] };
}
const out = {};
out.insert1000 = bench(1, 3, 10, () => {
  const db = new DatabaseSync(":memory:");
  db.exec("CREATE TABLE t(a INTEGER, b INTEGER, c TEXT, d REAL, e TEXT)");
  const ins = db.prepare("INSERT INTO t VALUES (?,?,?,?,?)");
  db.exec("BEGIN");
  for (let i = 0; i < 1000; i++) ins.run(i, i * 2, "s" + i, i * 0.5, "x");
  db.exec("COMMIT");
  db.close();
});
const db = new DatabaseSync(":memory:");
db.exec("CREATE TABLE t(a INTEGER, b INTEGER, c TEXT, d REAL, e TEXT)");
const ins = db.prepare("INSERT INTO t VALUES (?,?,?,?,?)");
db.exec("BEGIN");
for (let i = 0; i < 1000; i++) ins.run(i, i * 2, "s" + i, i * 0.5, "x");
db.exec("COMMIT");
const sel = db.prepare("SELECT a,b,c,d,e FROM t");
out.select1000 = bench(3, 3, 30, () => { const rows = sel.all(); if (rows.length !== 1000) throw new Error("len"); });
out.iterate1000 = bench(3, 3, 30, () => { let n = 0; for (const row of sel.iterate()) n++; if (n !== 1000) throw new Error("n"); });
const getBound = db.prepare("SELECT a,b,c,d,e FROM t WHERE a = ?");
out.get1000 = bench(1, 3, 5, () => { for (let i = 0; i < 1000; i++) getBound.get(i); });
const getConst = db.prepare("SELECT a,b,c,d,e FROM t WHERE a = 1");
out.getConst1000 = bench(1, 3, 5, () => { for (let i = 0; i < 1000; i++) getConst.get(); });
db.exec("CREATE INDEX pir_idx_a ON t(a)");
out.getIndexed1000 = bench(1, 3, 5, () => { for (let i = 0; i < 1000; i++) getBound.get(i); });
out.getIndexedConst1000 = bench(1, 3, 5, () => { for (let i = 0; i < 1000; i++) getConst.get(); });
db.close();
process.stdout.write(JSON.stringify(out));
"#;

fn find_node() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(if cfg!(windows) { "node.exe" } else { "node" });
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[tokio::test]
#[ignore = "throughput benchmark; run with --include-ignored"]
async fn sqlite_columnar_benchmark() {
    let dir = write_temp_fixture("bench", &[]);
    let rt = new_runtime_at(&dir).await;

    const FIXTURE: &str = r#"
import { DatabaseSync } from "node:sqlite";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const now = (typeof performance === "object" && performance !== null &&
      typeof (performance as any).now === "function")
      ? () => (performance as any).now()
      : () => Date.now();
    function bench(warm: number, iters: number, batch: number, fn: () => void) {
      for (let i = 0; i < warm; i++) fn();
      const samples: number[] = [];
      for (let i = 0; i < iters; i++) {
        const t0 = now();
        for (let b = 0; b < batch; b++) fn();
        samples.push((now() - t0) / batch);
      }
      samples.sort((a, b) => a - b);
      return { median: samples[samples.length >> 1], min: samples[0] };
    }
    const out: any = {};
    out.insert1000 = bench(1, 3, 10, () => {
      const db = new DatabaseSync(":memory:");
      db.exec("CREATE TABLE t(a INTEGER, b INTEGER, c TEXT, d REAL, e TEXT)");
      const ins = db.prepare("INSERT INTO t VALUES (?,?,?,?,?)");
      db.exec("BEGIN");
      for (let i = 0; i < 1000; i++) ins.run(i, i * 2, "s" + i, i * 0.5, "x");
      db.exec("COMMIT");
      db.close();
    });
    const db = new DatabaseSync(":memory:");
    db.exec("CREATE TABLE t(a INTEGER, b INTEGER, c TEXT, d REAL, e TEXT)");
    const ins = db.prepare("INSERT INTO t VALUES (?,?,?,?,?)");
    db.exec("BEGIN");
    for (let i = 0; i < 1000; i++) ins.run(i, i * 2, "s" + i, i * 0.5, "x");
    db.exec("COMMIT");
    const sel = db.prepare("SELECT a,b,c,d,e FROM t");
    out.select1000 = bench(3, 3, 30, () => {
      const rows = sel.all();
      if (rows.length !== 1000) throw new Error("len");
    });
    out.iterate1000 = bench(3, 3, 30, () => {
      let n = 0;
      for (const _row of sel.iterate()) n++;
      if (n !== 1000) throw new Error("n");
    });
    const getBound = db.prepare("SELECT a,b,c,d,e FROM t WHERE a = ?");
    out.get1000 = bench(1, 3, 5, () => {
      for (let i = 0; i < 1000; i++) getBound.get(i);
    });
    const getConst = db.prepare("SELECT a,b,c,d,e FROM t WHERE a = 1");
    out.getConst1000 = bench(1, 3, 5, () => {
      for (let i = 0; i < 1000; i++) getConst.get();
    });
    // Diagnostics: the same work through the raw host op, to separate shim
    // overhead from the Rust/bridge cost.
    const dbId = (db as any)._pirId;
    const boundReq: any = {
      id: dbId, sql: "SELECT a,b,c,d,e FROM t WHERE a = ?",
      values: [0], readBigInts: false, columnar: true,
    };
    out.hostGetBound1000 = bench(1, 3, 5, () => {
      for (let i = 0; i < 1000; i++) {
        boundReq.values[0] = i;
        globalThis.__pirNodeCall("sqlite", "get", boundReq);
      }
    });
    const constReq: any = {
      id: dbId, sql: "SELECT a,b,c,d,e FROM t WHERE a = 1",
      values: [], readBigInts: false, columnar: true,
    };
    out.hostGetConst1000 = bench(1, 3, 5, () => {
      for (let i = 0; i < 1000; i++) globalThis.__pirNodeCall("sqlite", "get", constReq);
    });
    out.hostAll = bench(3, 3, 30, () => {
      const r = globalThis.__pirNodeCall("sqlite", "all", {
        id: dbId, sql: "SELECT a,b,c,d,e FROM t", values: [], columnar: true,
      });
      if (r.rows.length !== 1000) throw new Error("host len");
    });
    out.hostAllJson = bench(3, 3, 30, () => {
      const text = globalThis.__pirNodeCall("sqlite", "all", {
        id: dbId, sql: "SELECT a,b,c,d,e FROM t", values: [], columnar: true, jsonText: true,
      });
      if (typeof text !== "string" || text.length === 0) throw new Error("host json len");
    });
    const jsonReq: any = {
      id: dbId, sql: "SELECT a,b,c,d,e FROM t WHERE a = ?",
      values: [0], columnar: true, jsonText: true,
    };
    out.hostGetJson1000 = bench(1, 3, 5, () => {
      for (let i = 0; i < 1000; i++) {
        jsonReq.values[0] = i;
        globalThis.__pirNodeCall("sqlite", "get", jsonReq);
      }
    });

    // Typed C-ABI entry: params as a live array, rows built host-side. Three
    // result shapes so the shim's chosen one can be compared against the
    // envelope path above.
    const typedAllSql = "SELECT a,b,c,d,e FROM t";
    out.typedAllObjects = bench(3, 3, 30, () => {
      const r = globalThis.__pirSqliteQuery(dbId, typedAllSql, [], "all", {});
      if (r.length !== 1000) throw new Error("typed len");
    });
    out.typedAllColumnar = bench(3, 3, 30, () => {
      const r = globalThis.__pirSqliteQuery(dbId, typedAllSql, [], "all", { columnar: true });
      if (r.rows.length !== 1000) throw new Error("typed columnar len");
    });
    out.typedAllRaw = bench(3, 3, 30, () => {
      const r = globalThis.__pirSqliteQuery(dbId, typedAllSql, [], "all", { raw: true });
      if (r.length !== 1000) throw new Error("typed raw len");
    });
    out.typedIterate = bench(3, 3, 30, () => {
      const r = globalThis.__pirSqliteQuery(dbId, typedAllSql, [], "iterate", {});
      if (r.length !== 1000) throw new Error("typed iterate len");
    });
    const typedBoundParams = [0];
    out.typedGetBound1000 = bench(1, 3, 5, () => {
      for (let i = 0; i < 1000; i++) {
        typedBoundParams[0] = i;
        globalThis.__pirSqliteQuery(dbId, "SELECT a,b,c,d,e FROM t WHERE a = ?", typedBoundParams, "get", {});
      }
    });
    out.typedGetConst1000 = bench(1, 3, 5, () => {
      for (let i = 0; i < 1000; i++) {
        globalThis.__pirSqliteQuery(dbId, "SELECT a,b,c,d,e FROM t WHERE a = 1", [], "get", {});
      }
    });
    out.typedGetColumnar1000 = bench(1, 3, 5, () => {
      for (let i = 0; i < 1000; i++) {
        typedBoundParams[0] = i;
        globalThis.__pirSqliteQuery(dbId, "SELECT a,b,c,d,e FROM t WHERE a = ?", typedBoundParams, "get", { columnar: true });
      }
    });
    // Fixed-overhead probes: bad handle (C call + number check only) and a
    // one-column row (call + parse + object build) bracket the per-call cost.
    out.typedHandleOnly1000 = bench(1, 3, 5, () => {
      for (let i = 0; i < 1000; i++) {
        globalThis.__pirSqliteQuery(-1, "SELECT 1", [], "get", {});
      }
    });
    out.typedGetOneCol1000 = bench(1, 3, 5, () => {
      for (let i = 0; i < 1000; i++) {
        globalThis.__pirSqliteQuery(dbId, "SELECT 1 AS x", [], "get", {});
      }
    });
    out.typedInsert1000 = bench(1, 2, 3, () => {
      const d = new DatabaseSync(":memory:");
      d.exec("CREATE TABLE q(a INTEGER, b INTEGER, c TEXT, d REAL, e TEXT)");
      const id2 = (d as any)._pirId;
      const params = [0, 0, "s", 0.5, "x"];
      d.exec("BEGIN");
      for (let i = 0; i < 1000; i++) {
        params[0] = i;
        globalThis.__pirSqliteQuery(id2, "INSERT INTO q VALUES (?,?,?,?,?)", params, "run", {});
      }
      d.exec("COMMIT");
      d.close();
    });

    // Same host ops over the legacy wire shape (per-parameter objects, row
    // objects built host-side) — the "before" side of the columnar change.
    const legacyGetReq: any = {
      id: dbId, sql: "SELECT a,b,c,d,e FROM t WHERE a = ?",
      binds: [{ index: 1, value: 0 }],
    };
    out.hostGetLegacy1000 = bench(1, 2, 5, () => {
      for (let i = 0; i < 1000; i++) {
        legacyGetReq.binds[0].value = i;
        globalThis.__pirNodeCall("sqlite", "get", legacyGetReq);
      }
    });
    const legacyAllReq: any = {
      id: dbId, sql: "SELECT a,b,c,d,e FROM t", binds: [],
    };
    out.hostAllLegacy = bench(2, 2, 10, () => {
      const r = globalThis.__pirNodeCall("sqlite", "all", legacyAllReq);
      if (r.length !== 1000) throw new Error("legacy len");
    });

    // Insert round-trip: flat `values` vs legacy `{index,value}` objects.
    // Each sample uses a fresh :memory: database so both variants insert into
    // an identically sized table (and pay the same open/prepare cost).
    out.hostInsertValues1000 = bench(1, 2, 3, () => {
      const d = new DatabaseSync(":memory:");
      d.exec("CREATE TABLE q(a INTEGER, b INTEGER, c TEXT, d REAL, e TEXT)");
      const id2 = (d as any)._pirId;
      const req: any = {
        id: id2, sql: "INSERT INTO q VALUES (?,?,?,?,?)",
        values: [0, 0, "s", 0.5, "x"],
      };
      globalThis.__pirNodeCall("sqlite", "exec", { id: id2, sql: "BEGIN" });
      for (let i = 0; i < 1000; i++) {
        req.values[0] = i;
        globalThis.__pirNodeCall("sqlite", "run", req);
      }
      globalThis.__pirNodeCall("sqlite", "exec", { id: id2, sql: "COMMIT" });
      d.close();
    });
    out.hostInsertBinds1000 = bench(1, 2, 3, () => {
      const d = new DatabaseSync(":memory:");
      d.exec("CREATE TABLE q(a INTEGER, b INTEGER, c TEXT, d REAL, e TEXT)");
      const id2 = (d as any)._pirId;
      const req: any = {
        id: id2, sql: "INSERT INTO q VALUES (?,?,?,?,?)",
        binds: [
          { index: 1, value: 0 }, { index: 2, value: 0 }, { index: 3, value: "s" },
          { index: 4, value: 0.5 }, { index: 5, value: "x" },
        ],
      };
      globalThis.__pirNodeCall("sqlite", "exec", { id: id2, sql: "BEGIN" });
      for (let i = 0; i < 1000; i++) {
        req.binds[0].value = i;
        globalThis.__pirNodeCall("sqlite", "run", req);
      }
      globalThis.__pirNodeCall("sqlite", "exec", { id: id2, sql: "COMMIT" });
      d.close();
    });
    // Indexed key lookup: the `.get()` target scenario (the unindexed benches
    // above measure a 1000-row scan, which dominates both engines).
    db.exec("CREATE INDEX pir_idx_a ON t(a)");
    out.getIndexed1000 = bench(1, 3, 5, () => {
      for (let i = 0; i < 1000; i++) getBound.get(i);
    });
    out.getIndexedConst1000 = bench(1, 3, 5, () => {
      for (let i = 0; i < 1000; i++) getConst.get();
    });
    out.typedGetIndexed1000 = bench(1, 3, 5, () => {
      for (let i = 0; i < 1000; i++) {
        typedBoundParams[0] = i;
        globalThis.__pirSqliteQuery(dbId, "SELECT a,b,c,d,e FROM t WHERE a = ?", typedBoundParams, "get", {});
      }
    });
    out.typedGetIndexedConst1000 = bench(1, 3, 5, () => {
      for (let i = 0; i < 1000; i++) {
        globalThis.__pirSqliteQuery(dbId, "SELECT a,b,c,d,e FROM t WHERE a = 1", [], "get", {});
      }
    });
    db.close();
    globalThis.__sqliteBench = out;
  });
};
"#;
    let value = run_fixture(&rt, "bench", FIXTURE, "__sqliteBench").await;

    fn line(name: &str, v: &Value) -> (f64, f64) {
        let median = v["median"].as_f64().unwrap_or(f64::NAN);
        let min = v["min"].as_f64().unwrap_or(f64::NAN);
        eprintln!("  pir {name:<12} median {median:>8.3} ms   min {min:>8.3} ms");
        (median, min)
    }

    eprintln!("[sqlite benchmark] node:sqlite shim (rusqlite host):");
    let keys = [
        "insert1000",
        "select1000",
        "iterate1000",
        "get1000",
        "getConst1000",
        "getIndexed1000",
        "getIndexedConst1000",
    ];
    let mut ours = Vec::new();
    for key in keys {
        ours.push((key, line(key, &value[key])));
    }

    if let Some(node) = find_node() {
        let output = Command::new(&node)
            .arg("-e")
            .arg(BENCH_NODE_SCRIPT)
            .output()
            .expect("spawn node benchmark");
        if !output.status.success() {
            // An older Node (no `node:sqlite`) or a restricted spawn should not
            // fail the benchmark; report why and keep the in-runtime numbers.
            eprintln!(
                "[sqlite benchmark] node baseline skipped (child failed): {}",
                String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .next()
                    .unwrap_or("")
            );
            return;
        }
        let node_value: Value = match serde_json::from_slice(&output.stdout) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("[sqlite benchmark] node baseline skipped (bad output: {error})");
                return;
            }
        };
        eprintln!("[sqlite benchmark] real Node 24 node:sqlite:");
        let mut node_times = Vec::new();
        for key in keys {
            node_times.push((key, line(key, &node_value[key])));
        }
        eprintln!("[sqlite benchmark] ratios (pir / node, median):");
        for ((name, (ours_median, _)), (_, (node_median, _))) in ours.iter().zip(node_times.iter())
        {
            let (ours_median, node_median) = (*ours_median, *node_median);
            eprintln!(
                "  {name:<12} {:.2}x{}",
                ours_median / node_median,
                if ours_median <= node_median * 1.2 {
                    "  (within 1.2x)"
                } else {
                    ""
                }
            );
        }
        let indexed_us = value["getIndexed1000"]["median"]
            .as_f64()
            .unwrap_or(f64::NAN)
            * 1000.0;
        eprintln!(
            "  indexed .get() per-op: {indexed_us:.2} us (target <= 5 us){}",
            if indexed_us <= 5.0 { "  (met)" } else { "" }
        );
    } else {
        eprintln!("[sqlite benchmark] node not on PATH; baseline comparison skipped");
    }

    eprintln!("[sqlite benchmark] raw host-op diagnostics (no shim objects):");
    for key in [
        "hostGetBound1000",
        "hostGetConst1000",
        "hostGetLegacy1000",
        "hostGetJson1000",
        "hostAll",
        "hostAllLegacy",
        "hostAllJson",
        "hostInsertValues1000",
        "hostInsertBinds1000",
    ] {
        line(key, &value[key]);
    }

    eprintln!("[sqlite benchmark] typed C-ABI entry (__pirSqliteQuery):");
    for key in [
        "typedAllObjects",
        "typedAllColumnar",
        "typedAllRaw",
        "typedIterate",
        "typedGetBound1000",
        "typedGetConst1000",
        "typedGetColumnar1000",
        "typedHandleOnly1000",
        "typedGetOneCol1000",
        "typedGetIndexed1000",
        "typedGetIndexedConst1000",
        "typedInsert1000",
    ] {
        line(key, &value[key]);
    }

    // Targets from the typed-path brief (reported, not asserted: the shared
    // worktree's compile profile and load make absolute timing unstable).
    let target = |label: &str, measured: f64, limit: f64, unit: &str| {
        eprintln!(
            "  target {label:<28} {measured:>8.3} {unit} (limit {limit} {unit}){}",
            if measured <= limit { "  (met)" } else { "" }
        );
    };
    let median = |key: &str| value[key]["median"].as_f64().unwrap_or(f64::NAN);
    target("SELECT 1000x5 .all()", median("select1000"), 0.9, "ms");
    target(
        "indexed .get() per-op",
        median("getIndexed1000") * 1000.0,
        3.0,
        "us",
    );
    target("INSERT 1000 (one txn)", median("insert1000"), 2.5, "ms");
    target(".iterate() 1000 rows", median("iterate1000"), 1.5, "ms");

    // Loose sanity bounds (timing tests are environment-sensitive; the strict
    // 1.2x target is reported above, not asserted here).
    for key in ["insert1000", "select1000", "get1000"] {
        assert!(
            value[key]["median"].as_f64().unwrap_or(f64::INFINITY) < 500.0,
            "{key} should finish well under 500 ms"
        );
    }
}
