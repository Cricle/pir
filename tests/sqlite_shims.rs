//! node:sqlite (DatabaseSync real) + the better-sqlite3 virtual package shim,
//! both backed by the host SQLite bridge (`__pir_host.node("sqlite", ...)` —
//! rusqlite bundled SQLite, FTS5 included).
//!
//! Fixtures are seeded at runtime (repository hygiene gate; same pattern as
//! `tests/node_shims.rs` / `tests/http_shims.rs`). Scenarios mirror the
//! semantics probed against real Node v24 `node:sqlite` and the only in-matrix
//! better-sqlite3 consumer (context-mode's db-base.js SQLiteBase).

use std::path::{Path, PathBuf};

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
