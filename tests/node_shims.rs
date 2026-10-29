//! Node built-in module shims (LLRT-style phase 1) + the extension loader's
//! TS-resolutions fallback (`./x.js` -> `x.ts`, extensionless -> `.ts`/`.js`,
//! directory -> `index.ts`/`index.js`).
//!
//! Fixture extensions are generated at runtime into per-test temp dirs: the
//! repository hygiene gate forbids tracked `.ts` files.

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// See tests/extensions_runtime.rs: fixtures must be written at runtime.
fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pir-node-shims-{}-{test_name}", std::process::id()));
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

fn test_config_at(cwd: &Path) -> ExtensionConfig {
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

fn test_config() -> ExtensionConfig {
    test_config_at(&std::env::temp_dir())
}

async fn new_runtime() -> ExtensionRuntime {
    ExtensionRuntime::new(test_config())
        .await
        .expect("runtime construction")
}

/// Runtime whose cwd is `dir` and whose allowed roots include `dir` (the same
/// state the loader reaches via `allow_root` during `load`).
async fn new_runtime_at(dir: &Path) -> ExtensionRuntime {
    let rt = ExtensionRuntime::new(test_config_at(dir))
        .await
        .expect("runtime construction");
    rt.allow_root(dir.to_path_buf());
    rt
}

fn expected_platform() -> &'static str {
    if cfg!(windows) {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

/// One seeded extension exercising the whole phase-1 node surface; it reports
/// every value through `globalThis.__nodeShims` from `session_start`.
const SHIM_EXT_TS: &str = r#"
import path from "node:path";
import * as pathNs from "node:path";
import { join as joinBare } from "path";
import { EventEmitter } from "node:events";
import { pathToFileURL, fileURLToPath, URL as URLImpl, URLSearchParams as SP } from "node:url";
import { promisify, stripVTControlCharacters, format as utilFormat } from "node:util";
import { StringDecoder } from "node:string_decoder";
import assertStrict from "node:assert/strict";
import assertLoose from "node:assert";
import { performance } from "node:perf_hooks";
import proc from "node:process";
import { setTimeout as delay } from "node:timers/promises";
import * as timers from "node:timers";
import * as qs from "node:querystring";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const sep = path.sep;
    const out: any = {};

    out.path = {
      platform: proc.platform,
      sep: sep,
      delimiter: path.delimiter,
      join_abc: path.join("a", "b", "..", "c"),
      dirname: path.dirname(path.join("a", "b", "c")),
      basename: path.basename(path.join("a", "b", "c.txt")),
      extname: path.extname("a/b/c.tar.gz"),
      relative: path.relative(path.join("a", "b"), path.join("a", "c")),
      isAbsoluteRoot: path.isAbsolute(sep + "x"),
      normalize: path.normalize("a" + sep + "." + sep + "b"),
      resolveAbs: path.resolve(sep + "a", "b"),
      resolveNone: path.resolve(),
      nsJoin: pathNs.join("a", "b"),
      bareJoin: joinBare("a", "b"),
      posixJoin: path.posix.join("a", "b"),
      winJoin: path.win32.join("a", "b"),
      posixNormalize: path.posix.normalize("a/./b/../c"),
      winRelative: path.win32.relative("C:\\a\\b", "C:\\a\\c"),
      winResolve: path.win32.resolve("C:\\x", "y"),
      winExtname: path.win32.extname("C:\\x\\file.tar.gz"),
      winIsAbsolute: path.win32.isAbsolute("\\\\server\\share"),
      posixParseBase: path.posix.parse("/a/b/c.txt").base,
      winParseRoot: path.win32.parse("C:\\foo\\bar.txt").root,
    };

    const em = new EventEmitter();
    const order: string[] = [];
    const onFn = (v: number) => order.push("on" + v);
    em.on("t", onFn);
    const onceSelf = em.once("t", (v: number) => order.push("once" + v)) === em;
    em.emit("t", 1);
    em.emit("t", 2);
    const orderAfterTwo = order.join(",");
    em.removeListener("t", onFn);
    em.emit("t", 3);
    let unhandledError = "";
    try {
      em.emit("error", new Error("boom"));
    } catch (e: any) {
      unhandledError = e.message;
    }
    let unhandledCode = "";
    try {
      em.emit("error", "nope");
    } catch (e: any) {
      unhandledCode = e.code;
    }
    out.events = {
      onceSelf: onceSelf,
      orderAfterTwo: orderAfterTwo,
      orderAfterRemove: order.join(","),
      listenerCount: em.listenerCount("t"),
      unhandledError: unhandledError,
      unhandledCode: unhandledCode,
    };

    const filePath = proc.platform === "win32" ? "C:\\tmp dir\\中文 文件.txt" : "/tmp dir/中文 文件.txt";
    const fileUrl = pathToFileURL(filePath);
    const parsed = new URLImpl("https://example.com/a/b?q=1&r=2#h");
    const sp = new SP("a=1&b=x+y");
    sp.append("c", "3");
    out.url = {
      href: fileUrl.href,
      protocol: fileUrl.protocol,
      roundtrip: fileURLToPath(fileUrl) === filePath,
      parsed: {
        protocol: parsed.protocol,
        hostname: parsed.hostname,
        pathname: parsed.pathname,
        search: parsed.search,
        hash: parsed.hash,
      },
      sp: { a: sp.get("a"), b: sp.get("b"), text: sp.toString(), has: sp.has("c") },
    };

    const cbFn = (x: string, done: (err: any, value?: string) => void) => {
      done(null, "v:" + x);
    };
    const promised = promisify(cbFn);
    const customTarget: any = (done: any) => done(new Error("nope"));
    customTarget[promisify.custom] = () => Promise.resolve("custom");
    out.util = {
      promised: await promised("a"),
      custom: await promisify(customTarget)(),
      customPropagated: promisify(promised) === promised,
      stripped: stripVTControlCharacters("\u001b[31mred\u001b[0m text"),
      formatted: utilFormat("%s:%d", "a", 2),
    };

    const decoder = new StringDecoder("utf8");
    const emoji = [0xf0, 0x9f, 0x98, 0x80];
    const part1 = decoder.write(new Uint8Array(emoji.slice(0, 2)));
    const part2 = decoder.write(new Uint8Array(emoji.slice(2)));
    out.decoder = {
      part1Len: part1.length,
      joined: part1 + part2,
      ok: part1 + part2 === String.fromCodePoint(0x1f600),
    };

    const assertResults: any = {
      looseCallable: typeof assertLoose === "function",
      strictCallable: typeof assertStrict === "function",
      strictAlias: assertStrict.equal === assertStrict.strictEqual,
    };
    assertStrict(true);
    try {
      assertStrict.deepStrictEqual(
        { a: [1, 2], m: new Map([[1, { y: 2 }]]) },
        { a: [1, 2], m: new Map([[1, { y: 2 }]]) }
      );
      assertResults.deepOk = true;
    } catch (e) {
      assertResults.deepOk = false;
    }
    try {
      assertStrict.deepStrictEqual({ a: 1 }, { a: 2 });
      assertResults.deepBad = "no-throw";
    } catch (e: any) {
      assertResults.deepBad = e.name + ":" + e.code;
    }
    try {
      assertLoose.deepStrictEqual(-0, 0);
      assertResults.negZero = "no-throw";
    } catch (e) {
      assertResults.negZero = "threw";
    }
    try {
      assertLoose.deepEqual({ v: NaN }, { v: NaN });
      assertResults.looseNaN = true;
    } catch (e) {
      assertResults.looseNaN = false;
    }
    out.assert = assertResults;

    const nowMs = performance.now();
    out.perf = {
      finite: typeof nowMs === "number" && Number.isFinite(nowMs),
      hasOrigin: typeof performance.timeOrigin === "number",
    };
    out.process = {
      platform: proc.platform,
      arch: proc.arch,
      version: proc.version,
      node: proc.versions && proc.versions.node,
      hasCwd: typeof proc.cwd,
    };

    const timerPromise = delay(1, "ok");
    out.timersPromise = { isPromise: timerPromise instanceof Promise, value: await timerPromise };
    out.timersExport = timers.setTimeout === globalThis.setTimeout;

    out.qs = {
      parseA: JSON.stringify(qs.parse("a=1&a=2&b=x+y")),
      stringify: qs.stringify({ a: 1, b: "x y" }),
      escape: qs.escape("a b"),
      unescape: qs.unescape("a%20b+c"),
    };

    globalThis.__nodeShims = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn node_module_shims_surface() {
    let rt = new_runtime().await;
    rt.seed_module("pir-ext:node-shims.ts", SHIM_EXT_TS)
        .expect("seed shim extension");
    let loaded = rt.seeded_load("pir-ext:node-shims.ts").await;
    assert_eq!(loaded.expect("seeded load"), 1);

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors while exercising node shims: {:?}",
        outcome.errors
    );

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__nodeShims)")
        .await
        .expect("read shim results");
    let platform = expected_platform();
    let win = cfg!(windows);
    let sep = if win { "\\" } else { "/" };
    let delim = if win { ";" } else { ":" };

    // --- path -------------------------------------------------------------
    let p = &v["path"];
    assert_eq!(p["platform"], platform, "process.platform mismatch: {v}");
    assert_eq!(p["sep"], sep, "path.sep mismatch: {v}");
    assert_eq!(p["delimiter"], delim, "path.delimiter mismatch: {v}");
    assert_eq!(
        p["join_abc"],
        format!("a{sep}c"),
        "join('a','b','..','c') mismatch: {v}"
    );
    assert_eq!(p["dirname"], format!("a{sep}b"), "dirname mismatch: {v}");
    assert_eq!(p["basename"], "c.txt", "basename mismatch: {v}");
    assert_eq!(p["extname"], ".gz", "extname mismatch: {v}");
    assert_eq!(p["relative"], format!("..{sep}c"), "relative mismatch: {v}");
    assert_eq!(
        p["isAbsoluteRoot"], true,
        "isAbsolute(sep+'x') mismatch: {v}"
    );
    assert_eq!(
        p["normalize"],
        format!("a{sep}b"),
        "normalize mismatch: {v}"
    );
    // Node win32: resolve("\\a", "b") is `<cwd drive>:\a\b` — a rooted path
    // without a drive anchor still gains the cwd's drive (the old shim
    // returned a bare "\a\b" and this assertion encoded that deviation).
    let resolve_abs = p["resolveAbs"].as_str().unwrap_or_default();
    assert!(
        std::path::Path::new(resolve_abs).is_absolute()
            && resolve_abs.ends_with(&format!("a{sep}b")),
        "resolve(sep+'a','b') mismatch: {v}"
    );
    // Node semantics: a no-arg resolve() anchors at process.cwd() (the
    // phase-1 "." placeholder predates the cwd host function).
    let resolve_none = p["resolveNone"].as_str().unwrap_or_default();
    assert!(
        std::path::Path::new(resolve_none).is_absolute(),
        "resolve() must anchor at process.cwd(): {v}"
    );
    assert_eq!(
        p["nsJoin"],
        format!("a{sep}b"),
        "namespace import mismatch: {v}"
    );
    assert_eq!(
        p["bareJoin"],
        format!("a{sep}b"),
        "bare 'path' import mismatch: {v}"
    );
    assert_eq!(p["posixJoin"], "a/b", "path.posix.join mismatch: {v}");
    assert_eq!(p["winJoin"], "a\\b", "path.win32.join mismatch: {v}");
    assert_eq!(
        p["posixNormalize"], "a/c",
        "path.posix.normalize mismatch: {v}"
    );
    assert_eq!(
        p["winRelative"], "..\\c",
        "path.win32.relative mismatch: {v}"
    );
    assert_eq!(
        p["winResolve"], "C:\\x\\y",
        "path.win32.resolve mismatch: {v}"
    );
    assert_eq!(p["winExtname"], ".gz", "path.win32.extname mismatch: {v}");
    assert_eq!(
        p["winIsAbsolute"], true,
        "path.win32.isAbsolute(UNC) mismatch: {v}"
    );
    assert_eq!(
        p["posixParseBase"], "c.txt",
        "path.posix.parse mismatch: {v}"
    );
    assert_eq!(
        p["winParseRoot"], "C:\\",
        "path.win32.parse root mismatch: {v}"
    );

    // --- events -----------------------------------------------------------
    let e = &v["events"];
    assert_eq!(e["onceSelf"], true, "once() must return the emitter: {v}");
    assert_eq!(
        e["orderAfterTwo"], "on1,once1,on2",
        "on/once emit order mismatch: {v}"
    );
    assert_eq!(
        e["orderAfterRemove"], "on1,once1,on2",
        "removeListener must stop later emits: {v}"
    );
    assert_eq!(e["listenerCount"], 0, "listenerCount after remove: {v}");
    assert_eq!(
        e["unhandledError"], "boom",
        "unhandled 'error' must rethrow the Error: {v}"
    );
    assert_eq!(
        e["unhandledCode"], "ERR_UNHANDLED_ERROR",
        "unhandled non-Error 'error' code mismatch: {v}"
    );

    // --- url --------------------------------------------------------------
    let u = &v["url"];
    let expected_href = if win {
        "file:///C:/tmp%20dir/%E4%B8%AD%E6%96%87%20%E6%96%87%E4%BB%B6.txt"
    } else {
        "file:///tmp%20dir/%E4%B8%AD%E6%96%87%20%E6%96%87%E4%BB%B6.txt"
    };
    assert_eq!(u["protocol"], "file:", "file URL protocol mismatch: {v}");
    assert_eq!(u["href"], expected_href, "pathToFileURL href mismatch: {v}");
    assert_eq!(
        u["roundtrip"], true,
        "pathToFileURL/fileURLToPath roundtrip mismatch: {v}"
    );
    assert_eq!(
        u["parsed"],
        json!({
            "protocol": "https:",
            "hostname": "example.com",
            "pathname": "/a/b",
            "search": "?q=1&r=2",
            "hash": "#h",
        }),
        "URL field parsing mismatch: {v}"
    );
    assert_eq!(
        u["sp"],
        json!({ "a": "1", "b": "x y", "text": "a=1&b=x+y&c=3", "has": true }),
        "URLSearchParams mismatch: {v}"
    );

    // --- util -------------------------------------------------------------
    let ut = &v["util"];
    assert_eq!(ut["promised"], "v:a", "promisify(cb) mismatch: {v}");
    assert_eq!(ut["custom"], "custom", "promisify.custom mismatch: {v}");
    assert_eq!(
        ut["customPropagated"], true,
        "promisified fn must expose .custom (propagation): {v}"
    );
    assert_eq!(
        ut["stripped"], "red text",
        "stripVTControlCharacters mismatch: {v}"
    );
    assert_eq!(ut["formatted"], "a:2", "util.format mismatch: {v}");

    // --- string_decoder ---------------------------------------------------
    let d = &v["decoder"];
    assert_eq!(
        d["part1Len"], 0,
        "first half of a split 4-byte emoji must decode to '': {v}"
    );
    assert_eq!(
        d["joined"], "\u{1F600}",
        "split emoji must reassemble across write() calls: {v}"
    );
    assert_eq!(d["ok"], true, "decoder emoji check failed: {v}");

    // --- assert -----------------------------------------------------------
    let a = &v["assert"];
    assert_eq!(
        a["strictCallable"], true,
        "assert/strict default must be callable: {v}"
    );
    assert_eq!(
        a["looseCallable"], true,
        "assert default must be callable: {v}"
    );
    assert_eq!(
        a["strictAlias"], true,
        "assert/strict .equal must be strictEqual: {v}"
    );
    assert_eq!(
        a["deepOk"], true,
        "deepStrictEqual positive case failed: {v}"
    );
    assert_eq!(
        a["deepBad"], "AssertionError:ERR_ASSERTION",
        "deepStrictEqual negative case mismatch: {v}"
    );
    assert_eq!(
        a["negZero"], "threw",
        "deepStrictEqual(-0, 0) must throw (Object.is): {v}"
    );
    assert_eq!(a["looseNaN"], true, "deepEqual NaN case failed: {v}");

    // --- perf_hooks / process / timers ------------------------------------
    assert_eq!(
        v["perf"]["finite"], true,
        "performance.now() must be finite: {v}"
    );
    assert_eq!(
        v["perf"]["hasOrigin"], true,
        "performance.timeOrigin missing: {v}"
    );
    let pr = &v["process"];
    assert_eq!(pr["platform"], platform, "process.platform mismatch: {v}");
    assert!(
        ["win32", "linux", "darwin"].contains(&pr["platform"].as_str().unwrap_or("")),
        "process.platform out of the phase-1 domain: {v}"
    );
    // One compat baseline across the surface: process.version, versions.node,
    // `node --version` (js_mode) and the vendored official tests all report
    // the pinned v24.19.0.
    assert_eq!(
        pr["version"], "v24.19.0",
        "process.version compat label: {v}"
    );
    assert_eq!(
        pr["node"], "24.19.0",
        "process.versions.node compat label: {v}"
    );
    assert_eq!(
        pr["hasCwd"], "function",
        "process.cwd() must be host-backed from phase 2 on: {v}"
    );
    assert_eq!(
        v["timersPromise"]["isPromise"], true,
        "timers/promises setTimeout must return a Promise: {v}"
    );
    assert_eq!(
        v["timersPromise"]["value"], "ok",
        "timers/promises setTimeout value mismatch: {v}"
    );
    assert_eq!(
        v["timersExport"], true,
        "timers.setTimeout must re-export the global: {v}"
    );

    // --- querystring ------------------------------------------------------
    let q = &v["qs"];
    assert_eq!(
        q["parseA"], "{\"a\":[\"1\",\"2\"],\"b\":\"x y\"}",
        "querystring.parse mismatch: {v}"
    );
    assert_eq!(
        q["stringify"], "a=1&b=x%20y",
        "querystring.stringify mismatch: {v}"
    );
    assert_eq!(q["escape"], "a%20b", "querystring.escape mismatch: {v}");
    assert_eq!(q["unescape"], "a b+c", "querystring.unescape mismatch: {v}");
}

#[tokio::test(flavor = "current_thread")]
async fn loader_resolution_fallback_loads_ts_neighbours() {
    let rt = new_runtime().await;
    let dir = write_temp_fixture(
        "resolution-fallback",
        &[
            (
                "index.ts",
                r#"
import { greet } from "./lib.js";
import { x } from "./helper";
import { y } from "./sub";
import { m } from "./mod.mjs";

export default (pi: any) => {
  pi.on("session_start", () => {
    globalThis.__fallback = { greet: greet("w"), x: x, y: y, m: m };
  });
};
"#,
            ),
            (
                "lib.ts",
                r#"
export function greet(name: string): string {
  return "hi " + name;
}
"#,
            ),
            ("helper.ts", "export const x: number = 41;\n"),
            ("sub/index.ts", "export const y: number = 7;\n"),
            ("mod.mts", "export const m: string = \"mjs-ok\";\n"),
        ],
    );

    let result = rt
        .load(std::slice::from_ref(&dir))
        .await
        .expect("load returns");
    assert!(
        result.errors.is_empty(),
        "resolution fallback must load every import: {:?}",
        result.errors
    );
    assert_eq!(
        result.loaded.len(),
        1,
        "expected one loaded entry: {result:?}"
    );

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__fallback)")
        .await
        .expect("read fallback results");
    assert_eq!(
        v["greet"], "hi w",
        "'./lib.js' -> lib.ts not resolved/transpiled: {v}"
    );
    assert_eq!(
        v["x"], 41,
        "extensionless './helper' -> helper.ts not resolved: {v}"
    );
    assert_eq!(v["y"], 7, "'./sub' -> sub/index.ts not resolved: {v}");
    assert_eq!(v["m"], "mjs-ok", "'.mjs' -> .mts rename not resolved: {v}");
}

// ---------------------------------------------------------------------------
// Phase 2: fs / crypto / buffer / os / process host-backed surface
// ---------------------------------------------------------------------------

/// fs round trip over the sync + promises + callback surfaces, all inside the
/// fixture directory (an allowed root), plus outside-root read probes (runtime
/// reads are global; the write fence has its own test).
const PHASE2_FS_TS: &str = r#"
import fs from "node:fs";
import { promises as fsp } from "node:fs";
import * as fsPromises from "node:fs/promises";
import { join } from "node:path";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const root = process.cwd();
    const file = join(root, "a.txt");

    fs.writeFileSync(file, "hello 世界", "utf8");
    out.text = fs.readFileSync(file, "utf8");
    const buf = fs.readFileSync(file);
    out.bufIsBuffer = Buffer.isBuffer(buf);
    out.bufLen = buf.length;
    out.bufHexTail = buf.toString("hex", buf.length - 3);
    fs.appendFileSync(file, "!");
    out.appended = fs.readFileSync(file, "utf8");

    fs.writeFileSync(join(root, "b.bin"), Buffer.from([1, 2, 3]));
    out.binLen = fs.statSync(join(root, "b.bin")).size;
    out.bIsFile = fs.statSync(join(root, "b.bin")).isFile();
    out.bIsDir = fs.statSync(join(root, "b.bin")).isDirectory();
    out.mtimeIsNumber = typeof fs.statSync(file).mtimeMs === "number";
    out.lstatIsLink = fs.lstatSync(file).isSymbolicLink();

    fs.mkdirSync(join(root, "sub", "deep"), { recursive: true });
    out.deepExists = fs.existsSync(join(root, "sub", "deep"));
    fs.mkdirSync(join(root, "plain"));
    out.plainExists = fs.existsSync(join(root, "plain"));

    out.names = fs.readdirSync(root).sort();
    out.dirents = fs.readdirSync(root, { withFileTypes: true })
      .map((d: any) => d.name + ":" + d.isFile())
      .sort();

    fs.renameSync(join(root, "b.bin"), join(root, "c.bin"));
    out.renamed = fs.existsSync(join(root, "c.bin")) && !fs.existsSync(join(root, "b.bin"));
    fs.copyFileSync(join(root, "c.bin"), join(root, "d.bin"));
    out.copied = fs.readFileSync(join(root, "d.bin")).toString("hex");
    out.realpathOk = fs.realpathSync(root).length > 0;
    fs.accessSync(file, fs.constants.R_OK);
    out.accessOk = true;

    fs.unlinkSync(join(root, "d.bin"));
    out.unlinked = !fs.existsSync(join(root, "d.bin"));
    fs.rmSync(join(root, "c.bin"));
    out.rmFile = !fs.existsSync(join(root, "c.bin"));
    fs.rmSync(join(root, "plain"), { recursive: true, force: true });
    out.rmDir = !fs.existsSync(join(root, "plain"));
    fs.rmSync(join(root, "missing"), { force: true });
    out.rmMissingForced = true;

    out.promiseText = await fsp.readFile(file, "utf8");
    out.promiseReaddir = (await fsPromises.readdir(root)).length;
    out.promiseStat = (await fsPromises.stat(file)).isFile();
    await fsp.mkdir(join(root, "pm"), { recursive: true });
    await fsp.rename(join(root, "pm"), join(root, "pm2"));
    await fsp.rm(join(root, "pm2"), { recursive: true });
    out.promiseOk = true;

    out.cbText = await new Promise((resolve, reject) => {
      fs.readFile(file, "utf8", (err: any, data: any) => (err ? reject(err) : resolve(data)));
    });
    out.cbErrCode = await new Promise((resolve) => {
      fs.readFile(join(root, "nope.txt"), (err: any) => resolve(err ? err.code : "no-error"));
    });
    out.cbReaddir = await new Promise((resolve, reject) => {
      fs.readdir(root, (err: any, names: any) => (err ? reject(err) : resolve(names.length)));
    });

    // Reads are global (writes are the fenced side): an outside-root system
    // file must read, guarding for hosts where the first candidate is absent.
    const systemCandidates = process.platform === "win32"
      ? [
          "C:\\Windows\\win.ini",
          (process.env.SystemRoot || "C:\\Windows") + "\\win.ini",
          (process.env.SystemRoot || "C:\\Windows") + "\\System32\\drivers\\etc\\hosts",
        ]
      : ["/etc/hostname", "/etc/hosts"];
    let systemFile = "";
    for (const candidate of systemCandidates) {
      if (fs.existsSync(candidate)) { systemFile = candidate; break; }
    }
    out.systemFile = systemFile;
    out.outsideRead = "none";
    if (systemFile !== "") {
      try {
        out.outsideRead = fs.readFileSync(systemFile, "utf8").length > 0 ? "ok" : "empty";
      } catch (e: any) {
        out.outsideRead = e.code;
      }
    }
    out.outsideExists = systemFile !== "" && fs.existsSync(systemFile);
    out.outsideStat = systemFile !== "" && fs.statSync(systemFile).isFile();

    // The project-root walk (pi-subagents' findProjectRootCandidates): cwd
    // ancestors beyond every allowed root must be stat/readdir-able.
    const upTwo = join(root, "..", "..");
    out.ancestorStat = fs.statSync(upTwo).isDirectory();
    out.ancestorReaddir = Array.isArray(fs.readdirSync(upTwo));

    globalThis.__phase2fs = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn node_phase2_fs_roundtrip_and_sandbox() {
    let dir = write_temp_fixture("phase2-fs", &[]);
    let rt = new_runtime_at(&dir).await;
    rt.seed_module("pir-ext:phase2-fs.ts", PHASE2_FS_TS)
        .expect("seed fs extension");
    let loaded = rt.seeded_load("pir-ext:phase2-fs.ts").await;
    assert_eq!(loaded.expect("seeded load"), 1);

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors while exercising fs shim: {:?}",
        outcome.errors
    );

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__phase2fs)")
        .await
        .expect("read fs results");

    assert_eq!(v["text"], "hello 世界", "utf8 roundtrip mismatch: {v}");
    assert_eq!(
        v["bufIsBuffer"], true,
        "readFileSync must return a Buffer: {v}"
    );
    assert_eq!(v["bufLen"], 12, "byte length mismatch: {v}");
    assert_eq!(v["bufHexTail"], "e7958c", "hex tail mismatch: {v}");
    assert_eq!(v["appended"], "hello 世界!", "append mismatch: {v}");
    assert_eq!(v["binLen"], 3, "written binary size mismatch: {v}");
    assert_eq!(v["bIsFile"], true, "statSync().isFile mismatch: {v}");
    assert_eq!(v["bIsDir"], false, "statSync().isDirectory mismatch: {v}");
    assert_eq!(v["mtimeIsNumber"], true, "mtimeMs must be a number: {v}");
    assert_eq!(
        v["lstatIsLink"], false,
        "lstatSync().isSymbolicLink mismatch: {v}"
    );
    assert_eq!(v["deepExists"], true, "mkdirSync(recursive) mismatch: {v}");
    assert_eq!(v["plainExists"], true, "mkdirSync(plain) mismatch: {v}");
    assert_eq!(
        v["names"],
        json!(["a.txt", "b.bin", "plain", "sub"]),
        "readdirSync names mismatch: {v}"
    );
    assert_eq!(
        v["dirents"],
        json!(["a.txt:true", "b.bin:true", "plain:false", "sub:false"]),
        "readdirSync withFileTypes mismatch: {v}"
    );
    assert_eq!(v["renamed"], true, "renameSync mismatch: {v}");
    assert_eq!(v["copied"], "010203", "copyFileSync mismatch: {v}");
    assert_eq!(v["realpathOk"], true, "realpathSync mismatch: {v}");
    assert_eq!(v["accessOk"], true, "accessSync mismatch: {v}");
    assert_eq!(v["unlinked"], true, "unlinkSync mismatch: {v}");
    assert_eq!(v["rmFile"], true, "rmSync(file) mismatch: {v}");
    assert_eq!(v["rmDir"], true, "rmSync(dir, recursive) mismatch: {v}");
    assert_eq!(
        v["rmMissingForced"], true,
        "rmSync(missing, force) mismatch: {v}"
    );
    assert_eq!(
        v["promiseText"], "hello 世界!",
        "promises.readFile mismatch: {v}"
    );
    assert!(
        v["promiseReaddir"].as_u64().unwrap_or(0) >= 1,
        "promises.readdir mismatch: {v}"
    );
    assert_eq!(v["promiseStat"], true, "promises.stat mismatch: {v}");
    assert_eq!(
        v["promiseOk"], true,
        "promises mkdir/rename/rm mismatch: {v}"
    );
    assert_eq!(
        v["cbText"], "hello 世界!",
        "callback readFile mismatch: {v}"
    );
    assert_eq!(
        v["cbErrCode"], "ENOENT",
        "callback error code mismatch: {v}"
    );
    assert!(
        v["cbReaddir"].as_u64().unwrap_or(0) >= 1,
        "callback readdir mismatch: {v}"
    );
    assert_ne!(
        v["systemFile"], "",
        "no readable system file candidate was found on this host: {v}"
    );
    assert_eq!(
        v["outsideRead"], "ok",
        "outside-root reads must succeed (reads are global): {v}"
    );
    assert_eq!(
        v["outsideExists"], true,
        "existsSync outside the roots must report the file's real state: {v}"
    );
    assert_eq!(
        v["outsideStat"], true,
        "statSync outside the roots must succeed: {v}"
    );
    assert_eq!(
        v["ancestorStat"], true,
        "stat on a cwd ancestor beyond every root must succeed: {v}"
    );
    assert_eq!(
        v["ancestorReaddir"], true,
        "readdir on a cwd ancestor beyond every root must succeed: {v}"
    );
}

/// The fs permission model in one fixture: reads are global, write targets
/// (including write-capable opens and mixed-op destinations) stay rooted.
const FS_POLICY_TS: &str = r#"
import fs from "node:fs";
import { join } from "node:path";

export default (pi: any) => {
  pi.on("session_start", () => {
    const out: any = {};
    const root = process.cwd();
    const tryCode = (fn: () => any) => {
      try {
        fn();
        return "no-throw";
      } catch (e: any) {
        return e.code || e.message;
      }
    };

    const systemCandidates = process.platform === "win32"
      ? [
          "C:\\Windows\\win.ini",
          (process.env.SystemRoot || "C:\\Windows") + "\\win.ini",
          (process.env.SystemRoot || "C:\\Windows") + "\\System32\\drivers\\etc\\hosts",
        ]
      : ["/etc/hostname", "/etc/hosts"];
    let systemFile = "";
    for (const candidate of systemCandidates) {
      if (fs.existsSync(candidate)) { systemFile = candidate; break; }
    }
    out.systemFile = systemFile;

    // The write side stays fenced: system-directory write targets are denied.
    const denied = process.platform === "win32" ? "C:\\Windows\\pir-denied-test.txt" : "/pir-denied-test.txt";
    out.writeDenied = tryCode(() => fs.writeFileSync(denied, "x"));
    out.appendDenied = tryCode(() => fs.appendFileSync(denied, "x"));
    out.mkdirDenied = tryCode(() => fs.mkdirSync(denied));
    out.rmDenied = tryCode(() => fs.rmSync(denied, { force: true }));

    // cwd writes stay allowed (the fixture dir is an allowed root).
    fs.writeFileSync(join(root, "ok.txt"), "ok");
    out.cwdWrite = fs.readFileSync(join(root, "ok.txt"), "utf8");

    // rename: a fenced target is denied before the source is touched.
    fs.writeFileSync(join(root, "src.txt"), "s");
    out.renameDenied = tryCode(() => fs.renameSync(join(root, "src.txt"), denied));
    out.renameSourceIntact = fs.existsSync(join(root, "src.txt"));

    // copyFile: the outside-root source is a plain read; the cwd target lands.
    out.copyFromSystem = tryCode(() => fs.copyFileSync(systemFile, join(root, "sys-copy.txt")));
    out.copyLen = fs.statSync(join(root, "sys-copy.txt")).size > 0;

    // open flags: `r` reads globally; `r+` and `w` write or create (fenced).
    const rfd = fs.openSync(systemFile, "r");
    const probe = Buffer.alloc(4);
    out.openReadOk = fs.readSync(rfd, probe, 0, 4, 0) > 0;
    fs.closeSync(rfd);
    out.openRwDenied = tryCode(() => fs.openSync(systemFile, "r+"));
    out.openWDenied = tryCode(() => fs.openSync(denied, "w"));

    // access: R_OK is a read probe; W_OK fences the target like a write.
    fs.accessSync(systemFile, fs.constants.R_OK);
    out.accessReadOk = true;
    out.accessWriteDenied = tryCode(() => fs.accessSync(systemFile, fs.constants.W_OK));

    // The pi-subagents walk-up pattern (findProjectRootCandidates minus its
    // `.native` alias call): stat + realpath + exists must work on every
    // ancestor of cwd up to the filesystem root.
    let walked = 0;
    let walker = root;
    for (let i = 0; i < 64; i++) {
      fs.statSync(walker);
      fs.realpathSync(walker);
      fs.existsSync(join(walker, ".git"));
      const parent = join(walker, "..");
      if (parent === walker) break;
      walker = parent;
      walked++;
    }
    out.walkUp = walked > 0;

    globalThis.__fsPolicy = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn fs_reads_are_global_writes_stay_rooted() {
    let dir = write_temp_fixture("fs-policy", &[]);
    let rt = new_runtime_at(&dir).await;
    rt.seed_module("pir-ext:fs-policy.ts", FS_POLICY_TS)
        .expect("seed fs policy extension");
    let loaded = rt.seeded_load("pir-ext:fs-policy.ts").await;
    assert_eq!(loaded.expect("seeded load"), 1);

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors while exercising the fs policy: {:?}",
        outcome.errors
    );

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__fsPolicy)")
        .await
        .expect("read fs policy results");

    assert_ne!(
        v["systemFile"], "",
        "no readable system file candidate was found on this host: {v}"
    );
    for key in [
        "writeDenied",
        "appendDenied",
        "mkdirDenied",
        "rmDenied",
        "renameDenied",
        "openRwDenied",
        "openWDenied",
        "accessWriteDenied",
    ] {
        assert_eq!(v[key], "EACCES", "{key} must be denied: {v}");
    }
    let denied = if cfg!(windows) {
        PathBuf::from("C:\\Windows\\pir-denied-test.txt")
    } else {
        PathBuf::from("/pir-denied-test.txt")
    };
    assert!(
        !denied.exists(),
        "a denied write must not create {denied:?} on the host"
    );

    // Read side: global reads, copies and read-opens all succeed.
    assert_eq!(v["cwdWrite"], "ok", "cwd writes stay allowed: {v}");
    assert_eq!(
        v["renameSourceIntact"], true,
        "a denied rename must not move the source: {v}"
    );
    assert_eq!(
        v["copyFromSystem"], "no-throw",
        "an outside-root copy source must read: {v}"
    );
    assert_eq!(v["copyLen"], true, "the copy must carry bytes: {v}");
    assert_eq!(
        v["openReadOk"], true,
        "open('r') on a system file must read: {v}"
    );
    assert_eq!(
        v["accessReadOk"], true,
        "access(R_OK) on a system file must succeed: {v}"
    );
    assert_eq!(
        v["walkUp"], true,
        "the walk-up-to-root read pattern must complete: {v}"
    );
}

/// crypto + buffer + os + process in one pass.
const PHASE2_MISC_TS: &str = r#"
import crypto from "node:crypto";
import { randomUUID as randomUUIDNs } from "node:crypto";
import { Buffer as B } from "node:buffer";
import * as os from "node:os";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    out.sha256 = crypto.createHash("sha256").update("abc").digest("hex");
    out.sha256Buf = crypto.createHash("sha256").update("abc").digest().toString("hex");
    out.sha512Len = crypto.createHash("sha512").update("abc").digest().length;
    out.sha1 = crypto.createHash("sha1").update("abc").digest("hex");
    out.md5 = crypto.createHash("md5").update("abc").digest("hex");
    out.hmac = crypto.createHmac("sha256", "key")
      .update("The quick brown fox jumps over the lazy dog").digest("hex");
    out.hmacMd5Len = crypto.createHmac("md5", "key").update("msg").digest().length;
    out.hmacChain = crypto.createHmac("sha256", "key").update("The quick brown ").update("fox jumps over the lazy dog").digest("hex");
    out.uuidOk = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(crypto.randomUUID());
    out.uuidNsLen = randomUUIDNs().length;
    out.randomLen = crypto.randomBytes(17).length;
    out.randomIsBuffer = Buffer.isBuffer(crypto.randomBytes(4));
    out.hashes = crypto.getHashes().join(",");
    out.timingEqual = crypto.timingSafeEqual(Buffer.from("abc"), Buffer.from("abc"));
    out.timingDiff = crypto.timingSafeEqual(Buffer.from("abc"), Buffer.from("abd"));
    let rangeErr = "none";
    try { crypto.timingSafeEqual(Buffer.from("ab"), Buffer.from("abc")); } catch (e: any) { rangeErr = e.name; }
    out.timingRange = rangeErr;

    const b64 = Buffer.from("你好", "utf8").toString("base64");
    out.b64 = b64;
    out.roundtrip = Buffer.from(b64, "base64").toString("utf8");
    out.hexRoundTrip = Buffer.from("48656c6c6f", "hex").toString("utf8");
    out.hexOut = Buffer.from("Hello").toString("hex");
    const cat = Buffer.concat([Buffer.from("ab"), Buffer.from([0x63])]);
    out.concat = cat.toString("utf8");
    out.concatIsBuffer = Buffer.isBuffer(cat);
    out.isBufferPlain = Buffer.isBuffer(new Uint8Array(3));
    out.byteLength = Buffer.byteLength("你好", "utf8");
    out.allocFill = Buffer.alloc(3, "ab").toString("utf8");
    const shared = Buffer.from("abcd");
    const view = shared.slice(1, 3);
    view[0] = 0x5a;
    out.sliceShares = shared.toString("utf8");
    out.subarrayIsBuffer = Buffer.isBuffer(Buffer.from("x").subarray(0, 1));
    out.globalBufferType = typeof globalThis.Buffer;
    out.namedBufferIsGlobal = B === globalThis.Buffer;
    out.indexOf = Buffer.from("hello").indexOf("ll");
    out.writeCount = (() => { const w = Buffer.alloc(4); return w.write("ab"); })();

    const subtle = await crypto.webcrypto.subtle.digest("SHA-256", Buffer.from("abc"));
    out.subtleLen = subtle.byteLength;
    out.subtleHex = Buffer.from(new Uint8Array(subtle)).toString("hex");

    out.osPlatform = os.platform();
    out.osType = os.type();
    out.osEolLen = os.EOL.length;
    out.osHomedir = os.homedir().length > 0;
    out.osTmpdir = os.tmpdir().length > 0;
    out.osHostname = typeof os.hostname() === "string" && os.hostname().length > 0;
    out.osArch = os.arch();
    out.osArchMatches = os.arch() === process.arch;
    out.osCpus = os.cpus().length >= 1;
    out.osEndian = os.endianness();
    out.osUserIsString = typeof os.userInfo().username === "string";
    out.osNet = Object.keys(os.networkInterfaces()).length;

    out.cwd = process.cwd();
    out.envKeys = Object.keys(process.env).length > 0;
    out.envIsObject = typeof process.env === "object" && process.env !== null;
    out.pidType = typeof process.pid;
    out.pidPositive = process.pid > 0;

    globalThis.__phase2misc = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn node_phase2_crypto_buffer_os_process() {
    let dir = write_temp_fixture("phase2-misc", &[]);
    let rt = new_runtime_at(&dir).await;
    rt.seed_module("pir-ext:phase2-misc.ts", PHASE2_MISC_TS)
        .expect("seed misc extension");
    let loaded = rt.seeded_load("pir-ext:phase2-misc.ts").await;
    assert_eq!(loaded.expect("seeded load"), 1);

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors while exercising crypto/buffer/os: {:?}",
        outcome.errors
    );

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__phase2misc)")
        .await
        .expect("read misc results");

    // --- crypto (known vectors) -------------------------------------------
    let sha256_abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(v["sha256"], sha256_abc, "sha256(\"abc\") mismatch: {v}");
    assert_eq!(
        v["sha256Buf"], sha256_abc,
        "digest() without encoding must return a Buffer: {v}"
    );
    assert_eq!(v["sha512Len"], 64, "sha512 digest size mismatch: {v}");
    assert_eq!(
        v["sha1"], "a9993e364706816aba3e25717850c26c9cd0d89d",
        "sha1(\"abc\") mismatch: {v}"
    );
    assert_eq!(
        v["md5"], "900150983cd24fb0d6963f7d28e17f72",
        "md5(\"abc\") mismatch: {v}"
    );
    // RFC-style HMAC-SHA256 vector (key "key", the well-known fox sentence).
    assert_eq!(
        v["hmac"], "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8",
        "hmac-sha256 vector mismatch: {v}"
    );
    assert_eq!(
        v["hmacChain"], v["hmac"],
        "chained hmac update mismatch: {v}"
    );
    assert_eq!(v["hmacMd5Len"], 16, "hmac-md5 digest size mismatch: {v}");
    assert_eq!(v["uuidOk"], true, "randomUUID format mismatch: {v}");
    assert_eq!(v["uuidNsLen"], 36, "named randomUUID export mismatch: {v}");
    assert_eq!(v["randomLen"], 17, "randomBytes length mismatch: {v}");
    assert_eq!(
        v["randomIsBuffer"], true,
        "randomBytes must return a Buffer: {v}"
    );
    assert_eq!(
        v["hashes"], "md5,sha1,sha256,sha512",
        "getHashes mismatch: {v}"
    );
    assert_eq!(v["timingEqual"], true, "timingSafeEqual equal case: {v}");
    assert_eq!(v["timingDiff"], false, "timingSafeEqual diff case: {v}");
    assert_eq!(
        v["timingRange"], "RangeError",
        "timingSafeEqual length guard: {v}"
    );
    assert_eq!(v["subtleLen"], 32, "webcrypto digest byteLength: {v}");
    assert_eq!(v["subtleHex"], sha256_abc, "webcrypto digest value: {v}");

    // --- buffer -----------------------------------------------------------
    assert_eq!(v["b64"], "5L2g5aW9", "utf8->base64 mismatch: {v}");
    assert_eq!(
        v["roundtrip"], "你好",
        "base64->utf8 roundtrip mismatch: {v}"
    );
    assert_eq!(v["hexRoundTrip"], "Hello", "hex decode mismatch: {v}");
    assert_eq!(v["hexOut"], "48656c6c6f", "hex encode mismatch: {v}");
    assert_eq!(v["concat"], "abc", "concat mismatch: {v}");
    assert_eq!(
        v["concatIsBuffer"], true,
        "concat result must be a Buffer: {v}"
    );
    assert_eq!(
        v["isBufferPlain"], false,
        "isBuffer(Uint8Array) must be false: {v}"
    );
    assert_eq!(v["byteLength"], 6, "byteLength(你好, utf8) mismatch: {v}");
    assert_eq!(v["allocFill"], "aba", "Buffer.alloc fill mismatch: {v}");
    assert_eq!(
        v["sliceShares"], "aZcd",
        "Buffer.slice must share memory: {v}"
    );
    assert_eq!(
        v["subarrayIsBuffer"], true,
        "subarray must return a Buffer: {v}"
    );
    assert_eq!(
        v["globalBufferType"], "function",
        "globalThis.Buffer backfill missing: {v}"
    );
    assert_eq!(
        v["namedBufferIsGlobal"], true,
        "node:buffer export must alias globalThis.Buffer: {v}"
    );
    assert_eq!(v["indexOf"], 2, "Buffer.indexOf mismatch: {v}");
    assert_eq!(v["writeCount"], 2, "Buffer.write mismatch: {v}");

    // --- os ---------------------------------------------------------------
    let platform = expected_platform();
    assert_eq!(v["osPlatform"], platform, "os.platform mismatch: {v}");
    let expected_type = if cfg!(windows) {
        "Windows_NT"
    } else if cfg!(target_os = "macos") {
        "Darwin"
    } else {
        "Linux"
    };
    assert_eq!(v["osType"], expected_type, "os.type mismatch: {v}");
    assert_eq!(
        v["osEolLen"],
        if cfg!(windows) { 2 } else { 1 },
        "os.EOL length mismatch: {v}"
    );
    assert_eq!(v["osHomedir"], true, "os.homedir() must be non-empty: {v}");
    assert_eq!(v["osTmpdir"], true, "os.tmpdir() must be non-empty: {v}");
    assert_eq!(
        v["osHostname"], true,
        "os.hostname() must be non-empty: {v}"
    );
    assert_eq!(
        v["osArchMatches"], true,
        "os.arch() must match process.arch: {v}"
    );
    assert_eq!(
        v["osCpus"], true,
        "os.cpus() must return at least one entry: {v}"
    );
    assert!(
        ["LE", "BE"].contains(&v["osEndian"].as_str().unwrap_or("")),
        "os.endianness mismatch: {v}"
    );
    assert_eq!(
        v["osUserIsString"], true,
        "os.userInfo().username must be a string: {v}"
    );
    assert_eq!(v["osNet"], 0, "os.networkInterfaces() must be empty: {v}");

    // --- process ----------------------------------------------------------
    assert_eq!(
        v["cwd"].as_str().unwrap_or(""),
        dir.display().to_string(),
        "process.cwd() must equal the configured cwd: {v}"
    );
    assert_eq!(
        v["envKeys"], true,
        "process.env snapshot must have keys: {v}"
    );
    assert_eq!(v["envIsObject"], true, "process.env must be an object: {v}");
    assert_eq!(v["pidType"], "number", "process.pid must be a number: {v}");
    assert_eq!(v["pidPositive"], true, "process.pid must be > 0: {v}");
}

/// The real-world extension shape (pi-tool-repair's config loader):
/// `existsSync(configPath)` + `readFileSync(configPath, "utf8")` + JSON.parse,
/// loaded through `rt.load` so the loader-populated allowed roots are what
/// grants access.
const REAL_WORLD_INDEX_TS: &str = r#"
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

export default (pi: any) => {
  pi.on("session_start", () => {
    const out: any = { hit: false };
    const present = join(process.cwd(), "config.json");
    if (existsSync(present)) {
      try {
        const parsed = JSON.parse(readFileSync(present, "utf8"));
        out.hit = true;
        out.mode = parsed.grammarRepair.mode;
      } catch (error: any) {
        out.hit = "error:" + error.message;
      }
    }
    out.miss = existsSync(join(process.cwd(), "absent.json"));
    globalThis.__realworld = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn node_phase2_real_world_config_load_pattern() {
    let dir = write_temp_fixture(
        "phase2-realworld",
        &[
            ("index.ts", REAL_WORLD_INDEX_TS),
            (
                "config.json",
                r#"{ "grammarRepair": { "enabled": true, "mode": "recover" } }"#,
            ),
        ],
    );
    let rt = ExtensionRuntime::new(test_config_at(&dir))
        .await
        .expect("runtime construction");
    // No manual allow_root: `load` fills the roots from the loaded path.
    let result = rt
        .load(std::slice::from_ref(&dir))
        .await
        .expect("load returns");
    assert!(
        result.errors.is_empty(),
        "real-world fixture must load: {:?}",
        result.errors
    );
    assert_eq!(
        result.loaded.len(),
        1,
        "expected one loaded entry: {result:?}"
    );

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);

    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__realworld)")
        .await
        .expect("read real-world results");
    assert_eq!(v["hit"], true, "present config must be read: {v}");
    assert_eq!(v["mode"], "recover", "parsed config field mismatch: {v}");
    assert_eq!(v["miss"], false, "missing config must report absent: {v}");
}

// ---------------------------------------------------------------------------
// Phase 3: child_process / stream / zlib + the URL and allowed-root fixes
// ---------------------------------------------------------------------------

/// Skip guard for fixtures that run real commands (the shell_totals_fix
/// convention: absent tooling skips instead of failing).
fn command_available(program: &str) -> bool {
    std::process::Command::new(program)
        .arg(if cfg!(windows) { "/?" } else { "--help" })
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// Load one phase-3 fixture extension and run its `session_start` hook.
async fn run_phase3_fixture(rt: &ExtensionRuntime, name: &str, ts: &str) -> Value {
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
    rt.eval_json(&format!("JSON.stringify(globalThis.__phase3{name})"))
        .await
        .expect("read fixture results")
}

/// `spawn` streams stdout, fires exit then close, and a missing command
/// surfaces as an asynchronous 'error' event (never a synchronous throw).
const PHASE3_SPAWN_TS: &str = r#"
import { spawn } from "node:child_process";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const isWin = process.platform === "win32";
    const cmd = isWin ? "cmd.exe" : "sh";
    const args = isWin ? ["/C", "echo CHILD_OK"] : ["-c", "echo CHILD_OK"];

    out.echo = await new Promise((resolve, reject) => {
      const child = spawn(cmd, args, { windowsHide: true });
      let stdout = "";
      const events: string[] = [];
      child.stdout.on("data", (chunk: any) => { stdout += chunk.toString("utf8"); });
      child.stdout.on("end", () => { events.push("stdout-end"); });
      child.stderr.on("data", () => {});
      child.on("error", reject);
      child.on("exit", (code: any) => { events.push("exit:" + code); });
      child.on("close", (code: any) => {
        events.push("close:" + code);
        resolve({
          stdout: stdout,
          events: events,
          code: code,
          killed: child.killed,
          pidPositive: child.pid > 0,
          spawnfile: child.spawnfile,
        });
      });
    });

    out.missing = await new Promise((resolve) => {
      let threw = false;
      let child: any;
      try {
        // An absolute miss avoids PATH lookup differences between the host
        // and a WSL interop PATH (a bare name can surface EACCES there).
        const missing = process.platform === "win32"
          ? "C:\\pir-no-such-command-xyz"
          : "/pir-no-such-command-xyz";
        child = spawn(missing, []);
      } catch (error) {
        threw = true;
      }
      child.on("error", (error: any) => resolve({
        threw: threw,
        code: error.code,
        pidUndefined: child.pid === undefined,
      }));
    });

    globalThis.__phase3spawn = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn child_process_spawn_streams_and_errors_async() {
    let rt = new_runtime().await;
    let v = run_phase3_fixture(&rt, "spawn", PHASE3_SPAWN_TS).await;

    assert_eq!(
        v["echo"]["stdout"].as_str().unwrap_or("").trim(),
        "CHILD_OK",
        "spawn must deliver stdout through the 'data' event: {v}"
    );
    assert_eq!(
        v["echo"]["events"],
        json!(["exit:0", "stdout-end", "close:0"]),
        "exit, pipe end and close must fire in Node's order: {v}"
    );
    assert_eq!(v["echo"]["code"], 0, "close code mismatch: {v}");
    assert_eq!(
        v["echo"]["killed"], false,
        "a naturally exited child is not killed: {v}"
    );
    assert_eq!(
        v["echo"]["pidPositive"], true,
        "child.pid must be the OS pid: {v}"
    );
    assert_eq!(
        v["echo"]["spawnfile"],
        if cfg!(windows) { "cmd.exe" } else { "sh" },
        "spawnfile mismatch: {v}"
    );
    assert_eq!(
        v["missing"]["threw"], false,
        "a spawn failure must not throw synchronously: {v}"
    );
    assert_eq!(
        v["missing"]["code"], "ENOENT",
        "a missing command must report ENOENT on the 'error' event: {v}"
    );
    assert_eq!(
        v["missing"]["pidUndefined"], true,
        "a failed spawn leaves the process object inert: {v}"
    );
}

/// stdin roundtrip through a real reader child (`findstr .` / `cat`).
const PHASE3_STDIN_TS: &str = r#"
import { spawn } from "node:child_process";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const isWin = process.platform === "win32";
    const command = isWin ? "findstr.exe" : "cat";
    const args = isWin ? ["."] : [];
    const result = await new Promise((resolve, reject) => {
      const child = spawn(command, args, { windowsHide: true });
      let stdout = "";
      child.stdout.on("data", (chunk: any) => { stdout += chunk.toString("utf8"); });
      child.stderr.on("data", () => {});
      child.on("error", reject);
      child.on("close", (code: any) => resolve({ stdout: stdout, code: code }));
      child.stdin.write("PING\n");
      child.stdin.end();
    });
    globalThis.__phase3stdin = result;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn child_process_stdin_write_and_end() {
    let reader = if cfg!(windows) { "findstr.exe" } else { "cat" };
    if !command_available(reader) {
        return;
    }
    let rt = new_runtime().await;
    let v = run_phase3_fixture(&rt, "stdin", PHASE3_STDIN_TS).await;

    assert_eq!(
        v["stdout"].as_str().unwrap_or("").trim(),
        "PING",
        "the child must read what stdin.write sent before end() closed the pipe: {v}"
    );
    assert_eq!(v["code"], 0, "reader child exit code mismatch: {v}");
}

/// exec / execSync / execFileSync / spawnSync: callback + blocking shapes,
/// error fields, and the no-throw contract of spawnSync.
const PHASE3_EXEC_TS: &str = r#"
import { exec, execSync, execFileSync, spawnSync } from "node:child_process";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const isWin = process.platform === "win32";

    out.sync = execSync("echo EXEC_OK", { windowsHide: true }).trim();
    out.syncBuffer = Buffer.isBuffer(execSync("echo EXEC_OK", { encoding: "buffer", windowsHide: true }));

    try {
      execSync("exit 3", { windowsHide: true });
      out.fail = "no-throw";
    } catch (error: any) {
      out.fail = {
        status: error.status,
        hasStdout: typeof error.stdout === "string",
        hasStderr: typeof error.stderr === "string",
        message: String(error.message).indexOf("Command failed") === 0,
      };
    }

    const result = spawnSync("exit 4", [], { shell: true, encoding: "utf8", windowsHide: true });
    out.spawnSync = { status: result.status, stdout: result.stdout, threw: false };

    out.callback = await new Promise((resolve) => {
      exec("echo EXEC_CB", { windowsHide: true }, (error: any, stdout: any, stderr: any) => {
        resolve({
          error: error === null || error === undefined,
          stdout: String(stdout).trim(),
          stderrIsString: typeof stderr === "string",
        });
      });
    });

    out.callbackFail = await new Promise((resolve) => {
      exec("exit 5", { windowsHide: true }, (error: any, stdout: any) => {
        resolve({ hasError: !!error, code: error ? error.code : null, stdout: String(stdout) });
      });
    });

    const file = isWin ? "cmd.exe" : "sh";
    const fileArgs = isWin ? ["/C", "echo FILE_OK"] : ["-c", "echo FILE_OK"];
    out.fileSync = execFileSync(file, fileArgs, { windowsHide: true }).trim();

    globalThis.__phase3exec = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn child_process_exec_and_blocking_variants() {
    let rt = new_runtime().await;
    let v = run_phase3_fixture(&rt, "exec", PHASE3_EXEC_TS).await;

    assert_eq!(v["sync"], "EXEC_OK", "execSync utf8 stdout mismatch: {v}");
    assert_eq!(
        v["syncBuffer"], true,
        "execSync with encoding buffer must return a Buffer: {v}"
    );
    let fail = &v["fail"];
    assert_eq!(
        fail["status"], 3,
        "execSync must throw with err.status: {v}"
    );
    assert_eq!(
        fail["hasStdout"], true,
        "the thrown error must carry stdout: {v}"
    );
    assert_eq!(
        fail["hasStderr"], true,
        "the thrown error must carry stderr: {v}"
    );
    assert_eq!(fail["message"], true, "error message shape mismatch: {v}");
    assert_eq!(
        v["spawnSync"]["status"], 4,
        "spawnSync must report the non-zero status without throwing: {v}"
    );
    assert_eq!(
        v["callback"]["error"], true,
        "exec must call back with null error on success: {v}"
    );
    assert_eq!(
        v["callback"]["stdout"], "EXEC_CB",
        "exec callback stdout mismatch: {v}"
    );
    assert_eq!(
        v["callback"]["stderrIsString"], true,
        "exec stderr must be a string: {v}"
    );
    assert_eq!(
        v["callbackFail"]["hasError"], true,
        "exec must call back with an error on a non-zero exit: {v}"
    );
    assert_eq!(
        v["callbackFail"]["code"], 5,
        "exec error must carry the exit code: {v}"
    );
    assert_eq!(
        v["fileSync"], "FILE_OK",
        "execFileSync stdout mismatch: {v}"
    );
}

/// kill(): the exit event must arrive promptly after the kill, `killed` flips.
const PHASE3_KILL_TS: &str = r#"
import { spawn } from "node:child_process";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const isWin = process.platform === "win32";
    const command = isWin ? "ping.exe" : "sleep";
    const args = isWin ? ["-n", "10", "127.0.0.1"] : ["5"];
    const started = Date.now();
    const result = await new Promise((resolve, reject) => {
      const child = spawn(command, args, { windowsHide: true });
      child.stdout.on("data", () => {});
      child.stderr.on("data", () => {});
      child.on("error", reject);
      child.on("close", (code: any, signal: any) => resolve({
        code: code,
        signal: signal,
        killed: child.killed,
        elapsedMs: Date.now() - started,
      }));
      globalThis.setTimeout(() => { child.kill(); }, 150);
    });
    globalThis.__phase3kill = result;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn child_process_kill_stops_a_long_running_child() {
    let rt = new_runtime().await;
    let v = run_phase3_fixture(&rt, "kill", PHASE3_KILL_TS).await;

    assert_eq!(v["killed"], true, "kill() must flip `killed`: {v}");
    let elapsed = v["elapsedMs"].as_u64().unwrap_or(u64::MAX);
    assert!(
        elapsed < 4_000,
        "the exit event must arrive right after kill() (elapsed {elapsed}ms): {v}"
    );
}

/// zlib: sync + callback roundtrips, the unzip alias, and the failure code.
const PHASE3_ZLIB_TS: &str = r#"
import zlib from "node:zlib";
import { gzipSync, gunzipSync, unzip } from "node:zlib";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const text = "hello zlib 世界";
    const gz = gzipSync(Buffer.from(text, "utf8"));
    out.gzipLength = gz.length;
    out.roundtrip = gunzipSync(gz).toString("utf8");
    out.deflateRoundtrip = zlib.inflateSync(zlib.deflateSync(Buffer.from(text))).toString("utf8");
    // node: `unzip` is a distinct container-sniffing codec, not an alias of
    // gunzip (`zlib.unzip !== zlib.gunzip`); gunzip stays gzip-strict while
    // unzip also accepts raw zlib streams.
    out.unzipDistinct = unzip !== zlib.gunzip;
    out.unzipSniffs = zlib.unzipSync(zlib.deflateSync(Buffer.from(text))).toString("utf8");
    try {
      gunzipSync(zlib.deflateSync(Buffer.from(text)));
      out.gunzipStrict = false;
    } catch (error: any) {
      out.gunzipStrict = error.code === "Z_DATA_ERROR";
    }
    out.cb = await new Promise((resolve) => {
      zlib.gzip(Buffer.from("cb"), (error: any, result: any) => {
        resolve({
          error: error === null || error === undefined,
          text: result ? zlib.gunzipSync(result).toString("utf8") : "",
        });
      });
    });
    try {
      gunzipSync(Buffer.from("not a gzip stream"));
      out.bad = "no-throw";
    } catch (error: any) {
      out.bad = error.code;
    }
    globalThis.__phase3zlib = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn zlib_gzip_deflate_roundtrip() {
    let rt = new_runtime().await;
    let v = run_phase3_fixture(&rt, "zlib", PHASE3_ZLIB_TS).await;

    assert!(
        v["gzipLength"].as_u64().unwrap_or(0) > 0,
        "gzipSync must produce bytes: {v}"
    );
    assert_eq!(
        v["roundtrip"], "hello zlib 世界",
        "gzipSync/gunzipSync byte roundtrip mismatch: {v}"
    );
    assert_eq!(
        v["deflateRoundtrip"], "hello zlib 世界",
        "deflateSync/inflateSync roundtrip mismatch: {v}"
    );
    assert_eq!(
        v["unzipDistinct"], true,
        "unzip must be a distinct sniffer (node: unzip !== gunzip): {v}"
    );
    assert_eq!(
        v["unzipSniffs"], "hello zlib 世界",
        "unzip must accept raw zlib streams (node semantics): {v}"
    );
    assert_eq!(
        v["gunzipStrict"], true,
        "gunzip must stay gzip-strict (Z_DATA_ERROR on raw zlib): {v}"
    );
    assert_eq!(
        v["cb"],
        json!({ "error": true, "text": "cb" }),
        "callback gzip/gunzip mismatch: {v}"
    );
    assert_eq!(
        v["bad"], "Z_DATA_ERROR",
        "a bad stream must throw with err.code Z_DATA_ERROR: {v}"
    );
}

/// Task 4a: `pir-ext:` module URLs resolve relative references and convert back
/// to paths; the `file:` behaviour must not regress.
const PHASE3_URL_TS: &str = r#"
import { URL as URLImpl, fileURLToPath, pathToFileURL } from "node:url";

export default (pi: any) => {
  pi.on("session_start", () => {
    const out: any = {};
    const sibling = new URLImpl("./x.js", "pir-ext:C:/dir/f.ts");
    out.href = sibling.href;
    out.protocol = sibling.protocol;
    out.siblingPath = fileURLToPath(sibling);

    const up = new URLImpl("../../session/db.js", "pir-ext:C:/Users/me/pkg/build/adapters/pi/extension.js");
    out.upHref = up.href;
    out.upPath = fileURLToPath(up);

    const withTail = new URLImpl("../mod.js?q=1#frag", "pir-ext:C:/a/b/c.js");
    out.tailHref = withTail.href;
    out.search = withTail.search;
    out.hash = withTail.hash;

    const filePath = process.platform === "win32" ? "C:\\tmp dir\\x.txt" : "/tmp dir/x.txt";
    const fileUrl = pathToFileURL(filePath);
    out.fileProtocol = fileUrl.protocol;
    out.fileRoundtrip = fileURLToPath(fileUrl) === filePath;

    try {
      const bad = new URLImpl("./x.js", "https://example.com/a/");
      out.relativeAgainstHttp = bad.href;
    } catch (error: any) {
      out.relativeAgainstHttp = "threw";
    }

    globalThis.__phase3url = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn url_resolves_pir_ext_module_bases() {
    let rt = new_runtime().await;
    let v = run_phase3_fixture(&rt, "url", PHASE3_URL_TS).await;
    let sep = if cfg!(windows) { "\\" } else { "/" };

    assert_eq!(
        v["href"], "pir-ext:C:/dir/x.js",
        "relative href against a pir-ext base mismatch: {v}"
    );
    assert_eq!(
        v["protocol"], "pir-ext:",
        "the resolved URL keeps its scheme: {v}"
    );
    assert_eq!(
        v["siblingPath"],
        format!("C:{sep}dir{sep}x.js"),
        "fileURLToPath must accept a pir-ext module URL: {v}"
    );
    assert_eq!(
        v["upHref"], "pir-ext:C:/Users/me/pkg/build/session/db.js",
        "parent-directory resolution mismatch (context-mode shape): {v}"
    );
    assert_eq!(
        v["upPath"],
        format!("C:{sep}Users{sep}me{sep}pkg{sep}build{sep}session{sep}db.js"),
        "path conversion of the resolved parent reference mismatch: {v}"
    );
    assert_eq!(
        v["tailHref"], "pir-ext:C:/a/mod.js?q=1#frag",
        "query/fragment handling of a relative reference mismatch: {v}"
    );
    assert_eq!(v["search"], "?q=1", "search mismatch: {v}");
    assert_eq!(v["hash"], "#frag", "hash mismatch: {v}");
    assert_eq!(v["fileProtocol"], "file:", "file: protocol mismatch: {v}");
    assert_eq!(
        v["fileRoundtrip"], true,
        "pathToFileURL/fileURLToPath roundtrip regressed: {v}"
    );
    // Task: URL pad http(s) base — special-scheme bases now merge like real
    // Node (WHATWG): the base authority is kept, the path merges.
    assert_eq!(
        v["relativeAgainstHttp"], "https://example.com/a/x.js",
        "relative references against an http base resolve Node-style: {v}"
    );
}

/// Buffer argument validation / `inspect` and the file-URL family's node
/// parity surface: the validation errors (ERR_INVALID_ARG_TYPE /
/// ERR_OUT_OF_RANGE / ERR_BUFFER_OUT_OF_BOUNDS), the base64 decoder's URL-safe
/// alphabet, `Buffer.prototype.inspect`, the legacy `parent`/`offset`
/// prototype getters, `urlToHttpOptions` over a `{...url}` copy, the
/// `pathToFileURL` windows/posix flavours (UNC host-state terminators
/// included), `fileURLToPath`'s URL brand check and `url.format`'s input
/// validation. Every case is platform-independent (`pathToFileURL` is driven
/// through its `{ windows }` option).
const PHASE3_BUFFERURL_TS: &str = r#"
import { URL as URLImpl, fileURLToPath, pathToFileURL, urlToHttpOptions, format as urlFormat } from "node:url";
import { isDeepStrictEqual } from "node:util";
import { partialDeepStrictEqual } from "node:assert";

export default (pi: any) => {
  pi.on("session_start", () => {
    const out: any = {};
    const grab = (fn: () => any) => {
      try {
        return { ok: true, value: fn() };
      } catch (error: any) {
        return { ok: false, name: error.name, code: error.code, message: error.message };
      }
    };
    const B = String.fromCharCode(92);

    // --- Buffer.equals / compare validation --------------------------------
    out.equalsString = grab(() => Buffer.alloc(1).equals("abc"));
    out.compareString = grab(() => Buffer.alloc(1).compare("abc"));
    out.staticCompareBuf2 = grab(() => Buffer.compare(Buffer.alloc(1), "abc"));
    out.staticCompareBuf1 = grab(() => Buffer.compare("abc", Buffer.alloc(1)));
    out.compareRange = Buffer.from("abc").compare(Buffer.from("abcd"), 0, 4, 0, 3);
    out.compareRangeShort = Buffer.from("abc").compare(Buffer.from("abcd"), 3, 1);
    out.compareOffsetErr = grab(() => Buffer.from("abc").compare(Buffer.from("a"), -1));
    out.compareOffsetTypeErr = grab(() => Buffer.from("abc").compare(Buffer.from("a"), "x"));
    out.equalsTypedArray = Buffer.from([1, 2]).equals(new Uint8Array([1, 2]));

    // --- fixed-size reads: ERR_BUFFER_OUT_OF_BOUNDS on a short buffer ------
    out.readEmptyU8 = grab(() => Buffer.alloc(0).readUInt8(0));
    out.readShortU32 = grab(() => Buffer.alloc(3).readUInt32BE(0));
    out.readEmptyFloat = grab(() => Buffer.alloc(0).readFloatBE(0));
    out.readNonInteger = grab(() => Buffer.alloc(4).readUInt8(1.5));

    // --- base64 takes the URL-safe alphabet in both modes ------------------
    out.base64UrlSafe = Buffer.from("__--_--_--__", "base64").toString("hex");
    out.base64Standard = Buffer.from("//++/++/++//", "base64").toString("hex");

    // --- inspect() + the legacy prototype getters --------------------------
    out.inspect = Buffer.from([0x81, 0xa3, 0x66, 0x6f]).inspect();
    out.inspectEmpty = Buffer.alloc(0).inspect();
    out.protoGetters = Buffer.prototype.parent === undefined && Buffer.prototype.offset === undefined;
    out.instanceParent = Buffer.allocUnsafe(4).parent instanceof ArrayBuffer;

    // --- urlToHttpOptions over a real URL and a `{...url}` copy ------------
    const httpUrl = new URLImpl("http://user:pass@foo.bar.com:21/aaa/zzz?l=24#test");
    out.urlOwnKeys = Object.keys(httpUrl).length;
    const realOpts: any = urlToHttpOptions(httpUrl);
    out.httpOptsReal = [realOpts.protocol, realOpts.auth, realOpts.hostname, realOpts.port, realOpts.path, realOpts.hash];
    out.httpOptsNullProto = Object.getPrototypeOf(realOpts) === null;
    const copiedOpts: any = urlToHttpOptions({ ...httpUrl } as any);
    out.httpOptsCopied = [
      copiedOpts.protocol === undefined, copiedOpts.hostname === undefined,
      Number.isNaN(copiedOpts.port), copiedOpts.path, copiedOpts.href === undefined,
    ];
    out.httpOptsBad = grab(() => urlToHttpOptions("http://127.0.0.1" as any));

    // --- URLs compare by href in the deep-equal core (node's isURL branch) --
    out.urlDeepEqualSame = isDeepStrictEqual(new URLImpl("http://foo"), new URLImpl("http://foo"));
    out.urlDeepEqualDiff = isDeepStrictEqual(new URLImpl("http://foo"), new URLImpl("http://bar"));
    out.urlPartialDiff = grab(() => partialDeepStrictEqual(new URLImpl("http://foo"), new URLImpl("http://bar")));

    // --- pathToFileURL: windows / posix flavours ---------------------------
    out.p2fPosixBackslash = pathToFileURL("/foo" + B + "bar", { windows: false } as any).href;
    out.p2fPosixTilde = pathToFileURL("/foo~bar", { windows: false } as any).href;
    out.p2fWinDrive = pathToFileURL("C:" + B + "dir" + B, { windows: true } as any).href;
    out.p2fUncExt = pathToFileURL(B + B + "?" + B + "UNC" + B + "server" + B + "share" + B + "f.txt", { windows: true } as any).href;
    out.p2fUncHash = pathToFileURL(B + B + "host#name" + B + "share" + B + "file.txt", { windows: true } as any).href;
    out.p2fUncQuestion = pathToFileURL(B + B + "host?name" + B + "share" + B + "file.txt", { windows: true } as any).href;
    out.p2fUncSlash = pathToFileURL(B + B + "host/name" + B + "share" + B + "file.txt", { windows: true } as any).href;
    out.p2fUncNewline = pathToFileURL(B + B + "host\nname" + B + "share" + B + "file.txt", { windows: true } as any).href;
    out.p2fUncBadHost = grab(() => pathToFileURL(B + B + "exa mple" + B + "s" + B + "f.txt", { windows: true } as any));
    out.p2fUncMissing = grab(() => pathToFileURL(B + B + "host", { windows: true } as any));
    out.p2fUncEmptyServer = grab(() => pathToFileURL(B + B + B + "no-server", { windows: true } as any));

    // --- fileURLToPath brand check + url.format validation -----------------
    out.f2pPlainObject = grab(() => fileURLToPath({} as any));
    out.f2pArray = grab(() => fileURLToPath([] as any));
    out.f2pScheme = grab(() => fileURLToPath("https://a/b/c"));
    out.formatEmptyObject = urlFormat({} as any);
    out.formatBad = grab(() => urlFormat(undefined as any));

    globalThis.__phase3bufferurl = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn buffer_url_validation_and_file_url_family() {
    let rt = new_runtime().await;
    let v = run_phase3_fixture(&rt, "bufferurl", PHASE3_BUFFERURL_TS).await;

    // Buffer.equals / compare validation.
    assert_eq!(v["equalsString"]["code"], "ERR_INVALID_ARG_TYPE", "{v}");
    assert_eq!(
        v["equalsString"]["message"],
        "The \"otherBuffer\" argument must be an instance of Buffer or Uint8Array. Received type string ('abc')",
        "equals() argument-validation message mismatch: {v}"
    );
    assert_eq!(v["compareString"]["code"], "ERR_INVALID_ARG_TYPE", "{v}");
    assert_eq!(
        v["compareString"]["message"],
        "The \"target\" argument must be an instance of Buffer or Uint8Array. Received type string ('abc')",
        "Buffer.prototype.compare target message mismatch: {v}"
    );
    assert_eq!(
        v["staticCompareBuf1"]["code"], "ERR_INVALID_ARG_TYPE",
        "{v}"
    );
    assert_eq!(
        v["staticCompareBuf2"]["code"], "ERR_INVALID_ARG_TYPE",
        "{v}"
    );
    assert_eq!(
        v["staticCompareBuf1"]["message"],
        "The \"buf1\" argument must be an instance of Buffer or Uint8Array. Received type string ('abc')",
        "Buffer.compare buf1 message mismatch: {v}"
    );
    assert_eq!(v["compareRange"], -1, "5-argument compare mismatch: {v}");
    assert_eq!(
        v["compareRangeShort"], 1,
        "empty target range compares 1: {v}"
    );
    assert_eq!(v["compareOffsetErr"]["code"], "ERR_OUT_OF_RANGE", "{v}");
    assert_eq!(
        v["compareOffsetTypeErr"]["code"], "ERR_INVALID_ARG_TYPE",
        "{v}"
    );
    assert_eq!(
        v["equalsTypedArray"], true,
        "equals(Uint8Array) mismatch: {v}"
    );

    // Fixed-size reads on a too-short buffer: ERR_BUFFER_OUT_OF_BOUNDS.
    for key in ["readEmptyU8", "readShortU32", "readEmptyFloat"] {
        assert_eq!(v[key]["code"], "ERR_BUFFER_OUT_OF_BOUNDS", "{key}: {v}");
        assert_eq!(
            v[key]["message"], "Attempt to access memory outside buffer bounds",
            "{key} message mismatch: {v}"
        );
    }
    assert_eq!(v["readNonInteger"]["code"], "ERR_OUT_OF_RANGE", "{v}");
    assert_eq!(
        v["readNonInteger"]["message"],
        "The value of \"offset\" is out of range. It must be an integer. Received 1.5",
        "non-integer offset message mismatch: {v}"
    );

    // base64 accepts the URL-safe alphabet in both modes.
    assert_eq!(v["base64UrlSafe"], "ffffbeffefbffbefff", "{v}");
    assert_eq!(v["base64Standard"], "ffffbeffefbffbefff", "{v}");

    // inspect() and the legacy prototype getters.
    assert_eq!(v["inspect"], "<Buffer 81 a3 66 6f>", "{v}");
    assert_eq!(v["inspectEmpty"], "<Buffer >", "{v}");
    assert_eq!(v["protoGetters"], true, "parent/offset must not throw: {v}");
    assert_eq!(v["instanceParent"], true, "{v}");

    // urlToHttpOptions.
    assert_eq!(
        v["urlOwnKeys"], 0,
        "URL instances carry no own enumerable keys: {v}"
    );
    assert_eq!(v["httpOptsReal"][0], "http:", "{v}");
    assert_eq!(v["httpOptsReal"][1], "user:pass", "{v}");
    assert_eq!(v["httpOptsReal"][2], "foo.bar.com", "{v}");
    assert_eq!(v["httpOptsReal"][3], 21, "port must be a number: {v}");
    assert_eq!(v["httpOptsReal"][4], "/aaa/zzz?l=24", "{v}");
    assert_eq!(v["httpOptsReal"][5], "#test", "{v}");
    assert_eq!(
        v["httpOptsNullProto"], true,
        "options are null-prototype: {v}"
    );
    assert_eq!(
        v["httpOptsCopied"],
        json!([true, true, true, "", true]),
        "a copied URL yields undefined fields and port NaN: {v}"
    );
    assert_eq!(v["httpOptsBad"]["code"], "ERR_INVALID_ARG_TYPE", "{v}");

    // URLs compare by href in the deep-equal core (node's isURL branch).
    assert_eq!(
        v["urlDeepEqualSame"], true,
        "identical URLs are deep-equal: {v}"
    );
    assert_eq!(
        v["urlDeepEqualDiff"], false,
        "different URLs are not deep-equal: {v}"
    );
    assert_eq!(
        v["urlPartialDiff"]["code"], "ERR_ASSERTION",
        "partialDeepStrictEqual must reject different URLs: {v}"
    );

    // pathToFileURL flavours + UNC host handling.
    assert_eq!(
        v["p2fPosixBackslash"], "file:///foo%5Cbar",
        "posix keeps backslashes: {v}"
    );
    assert_eq!(
        v["p2fPosixTilde"], "file:///foo%7Ebar",
        "~ is percent-encoded: {v}"
    );
    assert_eq!(
        v["p2fWinDrive"], "file:///C:/dir/",
        "trailing separator is restored: {v}"
    );
    assert_eq!(
        v["p2fUncExt"], "file://server/share/f.txt",
        "extended UNC prefix: {v}"
    );
    assert_eq!(
        v["p2fUncHash"], "file://host/share/file.txt",
        "host ends at '#': {v}"
    );
    assert_eq!(
        v["p2fUncQuestion"], "file://host/share/file.txt",
        "host ends at '?': {v}"
    );
    assert_eq!(
        v["p2fUncSlash"], "file://host/share/file.txt",
        "host ends at '/': {v}"
    );
    assert_eq!(
        v["p2fUncNewline"], "file://hostname/share/file.txt",
        "tab/LF/CR are stripped: {v}"
    );
    assert_eq!(v["p2fUncBadHost"]["code"], "ERR_INVALID_URL", "{v}");
    assert_eq!(v["p2fUncMissing"]["code"], "ERR_INVALID_ARG_VALUE", "{v}");
    assert!(
        v["p2fUncMissing"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("Missing UNC resource path"),
        "{v}"
    );
    assert_eq!(
        v["p2fUncEmptyServer"]["code"], "ERR_INVALID_ARG_VALUE",
        "{v}"
    );
    assert!(
        v["p2fUncEmptyServer"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("Empty UNC servername"),
        "{v}"
    );

    // fileURLToPath brand check + url.format validation.
    assert_eq!(v["f2pPlainObject"]["code"], "ERR_INVALID_ARG_TYPE", "{v}");
    assert_eq!(v["f2pArray"]["code"], "ERR_INVALID_ARG_TYPE", "{v}");
    assert_eq!(v["f2pScheme"]["code"], "ERR_INVALID_URL_SCHEME", "{v}");
    assert_eq!(
        v["formatEmptyObject"], "",
        "format of an empty object is the empty string: {v}"
    );
    assert_eq!(v["formatBad"]["code"], "ERR_INVALID_ARG_TYPE", "{v}");
}

/// `import.meta.url` parity: the loader populates the module meta object
/// with the file URL of the on-disk module (plus `filename`/`dirname`), so
/// Node-style sibling computation (`new URL(rel, import.meta.url)`) works
/// (`pi-subagents` pattern).
const PHASE3_META_TS: &str = r#"
import { fileURLToPath } from "node:url";

export default (pi: any) => {
  pi.on("session_start", () => {
    let resolved = "threw";
    let sibling = "threw";
    try {
      const url = new URL("./sib.js", import.meta.url);
      resolved = url.href;
      sibling = fileURLToPath(url);
    } catch (error) {
      resolved = "threw";
    }
    globalThis.__phase3meta = {
      url: String(import.meta.url ?? ""),
      filename: String(import.meta.filename ?? ""),
      resolved: resolved,
      sibling: sibling,
    };
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn import_meta_url_is_a_file_url() {
    let rt = new_runtime().await;
    let v = run_phase3_fixture(&rt, "meta", PHASE3_META_TS).await;

    let url = v["url"].as_str().unwrap_or_default();
    assert!(
        url.starts_with("file:///") && url.ends_with("meta.ts"),
        "import.meta.url must be the module's file URL: {v}"
    );
    assert!(
        v["resolved"]
            .as_str()
            .unwrap_or_default()
            .ends_with("/sib.js"),
        "relative resolution against import.meta.url must work: {v}"
    );
    assert!(
        v["sibling"]
            .as_str()
            .unwrap_or_default()
            .ends_with("sib.js"),
        "fileURLToPath of the sibling URL must work: {v}"
    );
    assert!(
        v["filename"]
            .as_str()
            .unwrap_or_default()
            .ends_with("meta.ts"),
        "import.meta.filename must be populated: {v}"
    );
}

/// Minimal `stream` pad: PassThrough data path, pipe forwarding, finish/end.
///
/// Node delivers piped data and 'finish' asynchronously (the flowing read and
/// the finish tick land after the pushing frame returns), so the fixture awaits
/// a tick before reading those results back — asserting the synchronous values
/// would re-encode the old pad's eager-emit behaviour.
const PHASE3_STREAM_TS: &str = r#"
import stream, { PassThrough, Readable, Writable } from "node:stream";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const tick = () => new Promise<void>((resolve) => setTimeout(resolve, 5));

    const pass = new PassThrough();
    const chunks: string[] = [];
    pass.on("data", (chunk: any) => chunks.push(String(chunk)));
    pass.write("hello");
    pass.end(" world");
    out.passThrough = chunks.join("");
    out.passThroughEnded = pass.writableEnded;

    const readable = new Readable();
    const published: string[] = [];
    const sink = new PassThrough();
    sink.on("data", (chunk: any) => published.push(String(chunk)));
    readable.pipe(sink);
    readable.push("a");
    readable.push("b");
    readable.push(null);
    await tick();
    out.pipe = published.join("");
    out.readableEnded = readable.readableEnded;

    const writable = new Writable();
    let finished = false;
    writable.on("finish", () => { finished = true; });
    writable.write("x");
    writable.end();
    await tick();
    out.writableFinish = finished;

    out.defaultIsStream = typeof stream === "function" &&
      stream.Readable === Readable &&
      stream.PassThrough === PassThrough;

    globalThis.__phase3stream = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn stream_minimal_pad_moves_data() {
    let rt = new_runtime().await;
    let v = run_phase3_fixture(&rt, "stream", PHASE3_STREAM_TS).await;

    assert_eq!(
        v["passThrough"], "hello world",
        "PassThrough data path: {v}"
    );
    assert_eq!(v["passThroughEnded"], true, "PassThrough end mismatch: {v}");
    assert_eq!(v["pipe"], "ab", "Readable.pipe forwarding mismatch: {v}");
    assert_eq!(v["readableEnded"], true, "push(null) must end: {v}");
    assert_eq!(
        v["writableFinish"], true,
        "Writable end must emit finish: {v}"
    );
    assert_eq!(
        v["defaultIsStream"], true,
        "the default export must be the Stream constructor carrying the classes: {v}"
    );
}

/// Loaded-extension shape (the pi-codegraph pattern): a real extension file
/// loaded through `rt.load` imports `node:child_process`, registers a tool, and
/// the tool runs a real echo command whose output the test reads back.
const PHASE3_LOADED_INDEX_TS: &str = r#"
import { spawn } from "node:child_process";

export default (pi: any) => {
  pi.registerTool({
    name: "run_echo",
    description: "Run a platform echo through node:child_process",
    parameters: { type: "object", properties: {} },
    execute: async () => {
      const isWin = process.platform === "win32";
      const text = await new Promise<string>((resolve, reject) => {
        const child = spawn(
          isWin ? "cmd.exe" : "sh",
          isWin ? ["/C", "echo CHILD_OK"] : ["-c", "echo CHILD_OK"],
          { windowsHide: true },
        );
        let stdout = "";
        child.stdout.on("data", (chunk: any) => { stdout += chunk.toString("utf8"); });
        child.stderr.on("data", () => {});
        child.on("error", (error: any) => reject(error));
        child.on("close", () => resolve(stdout.trim()));
      });
      return { content: [{ type: "text", text: text }] };
    },
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn child_process_works_from_a_loaded_extension() {
    let dir = write_temp_fixture("phase3-loaded", &[("index.ts", PHASE3_LOADED_INDEX_TS)]);
    let rt = ExtensionRuntime::new(test_config_at(&dir))
        .await
        .expect("runtime construction");
    let result = rt
        .load(std::slice::from_ref(&dir))
        .await
        .expect("load returns");
    assert!(
        result.errors.is_empty(),
        "the child_process fixture must load: {:?}",
        result.errors
    );
    assert_eq!(
        result.loaded.len(),
        1,
        "expected one loaded entry: {result:?}"
    );

    let tools = rt.tool_defs();
    assert!(
        tools.iter().any(|tool| tool.name == "run_echo"),
        "the fixture tool must register: {tools:?}"
    );

    let outcome = rt
        .call_tool("run_echo", "call-phase3", &json!({}))
        .await
        .expect("tool executes");
    assert_eq!(outcome.content.len(), 1);
    assert_eq!(
        outcome.content[0].kind, "text",
        "tool result shape mismatch: {outcome:?}"
    );
    assert_eq!(
        outcome.content[0].text, "CHILD_OK",
        "the tool must return the spawned echo output: {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// Phase 4a.1: fs fd family + misc, fs/promises open/opendir/mkdtemp,
// stream/promises, process events + exit teardown, events class identity
// ---------------------------------------------------------------------------

/// Load one phase-4a.1 fixture extension and run its `session_start` hook;
/// the results are read back from the named global (the phase-3 helper reads
/// a fixed `__phase3<name>` slot, these fixtures use their own).
async fn run_phase41_fixture(rt: &ExtensionRuntime, name: &str, ts: &str, global: &str) -> Value {
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
    rt.eval_json(&format!("JSON.stringify(globalThis.{global})"))
        .await
        .expect("read fixture results")
}

/// fd round trip: `w+` create/write (string + positioned Buffer), explicit vs
/// sequential reads, fstat/fsync/close, EBADF on closed fds, and a `r+`
/// reopen exercising the host-tracked sequential offset.
const PHASE41_FD_TS: &str = r#"
import fs from "node:fs";
import { join } from "node:path";

export default (pi: any) => {
  pi.on("session_start", () => {
    const out: any = {};
    const root = process.cwd();
    const file = join(root, "fd.txt");

    const fd = fs.openSync(file, "w+");
    out.fdOk = typeof fd === "number" && fd >= 3;
    out.wroteString = fs.writeSync(fd, "0123456789");
    out.wroteAtPosition = fs.writeSync(fd, Buffer.from("AB"), 0, 2, 2);
    out.sequentialAppend = fs.writeSync(fd, "XY");
    out.sizeAfterWrites = fs.fstatSync(fd).size;
    out.fstatIsFile = fs.fstatSync(fd).isFile();

    const all = Buffer.alloc(12);
    out.readExplicit = fs.readSync(fd, all, 0, 12, 0) + ":" + all.toString("utf8");
    out.readSeqAtEof = fs.readSync(fd, Buffer.alloc(4), 0, 4, null);
    fs.fsyncSync(fd);
    fs.closeSync(fd);
    try {
      fs.readSync(fd, Buffer.alloc(4), 0, 4, 0);
      out.ebadf = "no-throw";
    } catch (e: any) {
      out.ebadf = e.code;
    }
    try {
      fs.closeSync(fd);
      out.ebadfClose = "no-throw";
    } catch (e: any) {
      out.ebadfClose = e.code;
    }

    const rfd = fs.openSync(file, "r+");
    const s1 = Buffer.alloc(4);
    const s2 = Buffer.alloc(4);
    const s3 = Buffer.alloc(4);
    out.seq = [
      fs.readSync(rfd, s1, 0, 4, null) + ":" + s1.toString("utf8"),
      fs.readSync(rfd, s2, 0, 4, null) + ":" + s2.toString("utf8"),
      fs.readSync(rfd, s3, 0, 4, null) + ":" + s3.toString("utf8"),
      fs.readSync(rfd, Buffer.alloc(4), 0, 4, null),
    ];
    const explicit = Buffer.alloc(2);
    out.explicitRead = fs.readSync(rfd, explicit, 0, 2, 0) + ":" + explicit.toString("utf8");
    out.wroteAfterSeq = fs.writeSync(rfd, "Z");
    const tail = Buffer.alloc(2);
    const tailLen = fs.readSync(rfd, tail, 0, 2, 12);
    out.tailRead = tailLen + ":" + tail.toString("utf8", 0, tailLen);
    out.sizeFinal = fs.fstatSync(rfd).size;
    fs.closeSync(rfd);

    const rofd = fs.openSync(file, "r");
    let readOnlyWrite = "";
    try {
      fs.writeSync(rofd, "x");
      readOnlyWrite = "no-throw";
    } catch (e: any) {
      readOnlyWrite = e.code;
    }
    out.readOnlyWrite = readOnlyWrite;
    fs.closeSync(rofd);

    globalThis.__phase41fd = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn fs_fd_family_roundtrip() {
    let dir = write_temp_fixture("phase41-fd", &[("index.ts", PHASE41_FD_TS)]);
    let rt = new_runtime_at(&dir).await;
    let v = run_phase41_fixture(&rt, "fd41", PHASE41_FD_TS, "__phase41fd").await;

    assert_eq!(v["fdOk"], true, "openSync must return a numeric fd: {v}");
    assert_eq!(
        v["wroteString"], 10,
        "sequential string write mismatch: {v}"
    );
    assert_eq!(
        v["wroteAtPosition"], 2,
        "positioned Buffer write mismatch: {v}"
    );
    assert_eq!(
        v["sequentialAppend"], 2,
        "sequential write after positioned write must append at the tracked offset: {v}"
    );
    assert_eq!(v["sizeAfterWrites"], 12, "fstatSync size mismatch: {v}");
    assert_eq!(v["fstatIsFile"], true, "fstatSync().isFile mismatch: {v}");
    assert_eq!(
        v["readExplicit"], "12:01AB456789XY",
        "positioned read mismatch (buffer write must have landed at 2): {v}"
    );
    assert_eq!(
        v["readSeqAtEof"], 0,
        "sequential read at the tracked EOF must return 0 bytes: {v}"
    );
    assert_eq!(v["ebadf"], "EBADF", "closed-fd read must report EBADF: {v}");
    assert_eq!(
        v["ebadfClose"], "EBADF",
        "double close must report EBADF: {v}"
    );
    assert_eq!(
        v["seq"],
        json!(["4:01AB", "4:4567", "4:89XY", 0]),
        "r+ sequential reads must walk the tracked offset: {v}"
    );
    assert_eq!(
        v["explicitRead"], "2:01",
        "positioned read on r+ mismatch: {v}"
    );
    assert_eq!(
        v["wroteAfterSeq"], 1,
        "sequential write must continue at the tracked offset (not at an explicit-read position): {v}"
    );
    assert_eq!(v["tailRead"], "1:Z", "tail read after append mismatch: {v}");
    assert_eq!(v["sizeFinal"], 13, "final size mismatch: {v}");
    assert_ne!(
        v["readOnlyWrite"], "no-throw",
        "writing through an 'r' descriptor must fail: {v}"
    );
}

/// `chmod` readonly toggle (observed between two calls), recursive cp,
/// rmdir empty/non-empty, mkdtemp uniqueness, symlink (permission errors are
/// tolerated — the platform needs developer mode / admin for symlinks).
const PHASE41_MISC_TS: &str = r#"
import fs from "node:fs";
import { join } from "node:path";

export default (pi: any) => {
  pi.on("session_start", () => {
    const out: any = {};
    const root = process.cwd();
    const file = join(root, "mode.txt");
    fs.writeFileSync(file, "m");
    globalThis.__phase41chmod = (mode: number) => fs.chmodSync(file, mode);
    globalThis.__phase41fchmod = (mode: number) => {
      const fd = fs.openSync(file, "r+");
      fs.fchmodSync(fd, mode);
      fs.closeSync(fd);
    };

    fs.mkdirSync(join(root, "src", "nested"), { recursive: true });
    fs.writeFileSync(join(root, "src", "a.txt"), "A");
    fs.writeFileSync(join(root, "src", "nested", "b.txt"), "B");
    fs.cpSync(join(root, "src"), join(root, "dst"), { recursive: true });
    out.cpDeep = fs.readFileSync(join(root, "dst", "nested", "b.txt"), "utf8");
    fs.cpSync(join(root, "src", "a.txt"), join(root, "a-copy.txt"));
    out.cpFile = fs.readFileSync(join(root, "a-copy.txt"), "utf8");
    try {
      fs.cpSync(join(root, "src"), join(root, "dst2"));
      out.cpNoRecurse = "no-throw";
    } catch (e: any) {
      out.cpNoRecurse = e.code;
    }

    fs.mkdirSync(join(root, "empty"));
    fs.rmdirSync(join(root, "empty"));
    out.rmdirEmpty = !fs.existsSync(join(root, "empty"));
    fs.mkdirSync(join(root, "nonempty"));
    fs.writeFileSync(join(root, "nonempty", "x"), "x");
    try {
      fs.rmdirSync(join(root, "nonempty"));
      out.rmdirNonEmpty = "no-throw";
    } catch (e: any) {
      out.rmdirNonEmpty = e.code;
    }
    fs.rmdirSync(join(root, "nonempty"), { recursive: true });
    out.rmdirRecursive = !fs.existsSync(join(root, "nonempty"));

    const t1 = fs.mkdtempSync("pir-int-sync-");
    const t2 = fs.mkdtempSync("pir-int-sync-");
    out.mkdtempDistinct = t1 !== t2 && t1.indexOf("pir-int-sync-") !== -1;
    out.mkdtempPaths = [t1, t2];

    try {
      fs.symlinkSync(file, join(root, "link.txt"), "file");
      out.symlink = "created";
      out.symlinkIsLink = fs.lstatSync(join(root, "link.txt")).isSymbolicLink();
      out.symlinkTarget = fs.readlinkSync(join(root, "link.txt")) === file;
      out.symlinkContent = fs.readFileSync(join(root, "link.txt"), "utf8");
    } catch (e: any) {
      out.symlink = e.code || "error";
    }

    globalThis.__phase41misc = out;
  });
};
"#;

fn assert_readonly(path: &Path, expected: bool) {
    let perms = std::fs::metadata(path)
        .expect("stat for readonly check")
        .permissions();
    #[cfg(windows)]
    assert_eq!(
        perms.readonly(),
        expected,
        "readonly attribute mismatch on {path:?}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            perms.mode() & 0o222 == 0,
            expected,
            "write-bit expectation mismatch on {path:?}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn fs_misc_chmod_cp_rmdir_mkdtemp_symlink() {
    let dir = write_temp_fixture("phase41-misc", &[("index.ts", PHASE41_MISC_TS)]);
    let rt = new_runtime_at(&dir).await;
    let v = run_phase41_fixture(&rt, "misc41", PHASE41_MISC_TS, "__phase41misc").await;

    // chmod: 0o444 -> readonly, 0o644 -> writable (observed between calls).
    let mode_file = dir.join("mode.txt");
    rt.eval_json("(globalThis.__phase41chmod(292), '\"ok\"')")
        .await
        .expect("chmod 0o444");
    assert_readonly(&mode_file, true);
    rt.eval_json("(globalThis.__phase41chmod(420), '\"ok\"')")
        .await
        .expect("chmod 0o644");
    assert_readonly(&mode_file, false);

    // fchmodSync through the open descriptor (the file is writable at open
    // time; Windows cannot reopen a readonly file for writing, so the
    // toggle-back goes through the path form).
    rt.eval_json("(globalThis.__phase41fchmod(292), '\"ok\"')")
        .await
        .expect("fchmod 0o444");
    assert_readonly(&mode_file, true);
    rt.eval_json("(globalThis.__phase41chmod(420), '\"ok\"')")
        .await
        .expect("chmod back to 0o644");
    assert_readonly(&mode_file, false);

    assert_eq!(v["cpDeep"], "B", "recursive cp must copy nested files: {v}");
    assert_eq!(v["cpFile"], "A", "plain file cp mismatch: {v}");
    assert_eq!(
        v["cpNoRecurse"], "ERR_FS_EISDIR",
        "directory cp without recursive must fail with ERR_FS_EISDIR: {v}"
    );
    assert_eq!(v["rmdirEmpty"], true, "rmdirSync(empty dir) mismatch: {v}");
    assert_eq!(
        v["rmdirNonEmpty"], "ENOTEMPTY",
        "rmdirSync(non-empty) must report ENOTEMPTY: {v}"
    );
    assert_eq!(
        v["rmdirRecursive"], true,
        "rmdirSync recursive mismatch: {v}"
    );
    assert_eq!(
        v["mkdtempDistinct"], true,
        "mkdtempSync must produce distinct dirs with the prefix: {v}"
    );
    // Clean the host-created temp dirs (outside the runtime sandbox).
    for path in v["mkdtempPaths"].as_array().into_iter().flatten() {
        if let Some(path) = path.as_str() {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    let symlink = v["symlink"].as_str().unwrap_or_default();
    if symlink == "created" {
        assert_eq!(v["symlinkIsLink"], true, "lstat must see the link: {v}");
        assert_eq!(v["symlinkTarget"], true, "readlink mismatch: {v}");
        assert_eq!(v["symlinkContent"], "m", "reading through the link: {v}");
    } else {
        // Windows without developer mode / admin: the raw platform error is
        // allowed through; note it instead of failing.
        eprintln!("[test note] symlinkSync not creatable on this host: code={symlink}");
    }
}

/// `fs/promises`: FileHandle write/read/stat/sync/close, readFile/writeFile,
/// Dir.read walk + async iteration, mkdtemp distinctness.
const PHASE41_PROMISES_TS: &str = r#"
import { open, opendir, mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { join } from "node:path";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const root = process.cwd();

    const fh: any = await open(join(root, "ph.txt"), "w+");
    out.fdIsNumber = typeof fh.fd === "number";
    const wr = await fh.write("promise-data");
    out.wrote = wr.bytesWritten;
    out.wroteBuffer = Buffer.isBuffer(wr.buffer);
    out.sizeAfterWrite = (await fh.stat()).size;
    const buf = Buffer.alloc(12);
    const rd = await fh.read(buf, 0, 12, 0);
    out.read = rd.bytesRead + ":" + buf.toString("utf8");
    await fh.writeFile(" rewritten");
    out.afterWriteFile = await fh.readFile("utf8");
    await fh.sync();
    await fh.close();
    let afterClose = "";
    try {
      await fh.stat();
      afterClose = "no-throw";
    } catch (e: any) {
      afterClose = e.code;
    }
    out.afterClose = afterClose;

    const pdir = join(root, "pdir");
    await mkdir(pdir);
    await writeFile(join(pdir, "1.txt"), "1");
    await writeFile(join(pdir, "2.txt"), "2");
    await mkdir(join(pdir, "sub"));
    const dir: any = await opendir(pdir);
    const names: string[] = [];
    for (;;) {
      const entry = await dir.read();
      if (entry === null) break;
      names.push(entry.name + ":" + entry.isFile());
    }
    out.dirRead = names.length + ":" + names.sort().join(",");
    out.dirReadAfterNull = await dir.read();
    await dir.close();
    // Node's contract (asserted by the official test-fs-opendir.js): reading
    // from a closed handle rejects ERR_DIR_CLOSED, it does not resolve null.
    let closedRead = "";
    try {
      await dir.read();
      closedRead = "no-throw";
    } catch (e: any) {
      closedRead = e.code;
    }
    out.dirClosedRead = closedRead;

    const dir2: any = await opendir(pdir);
    const iterated: string[] = [];
    for await (const entry of dir2) iterated.push(entry.name);
    out.dirIterated = iterated.length;
    // The async iterator closes the handle when it runs dry, so an explicit
    // close() afterwards reports ERR_DIR_CLOSED (real-node behaviour).
    let iterClose = "";
    try {
      await dir2.close();
      iterClose = "no-throw";
    } catch (e: any) {
      iterClose = e.code;
    }
    out.dirIterClosed = iterClose;

    const d1 = await mkdtemp("pir-int-prom-");
    const d2 = await mkdtemp("pir-int-prom-");
    out.mkdtempDistinct = d1 !== d2 && typeof d1 === "string";
    out.mkdtempPaths = [d1, d2];

    globalThis.__phase41promises = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn fs_promises_open_opendir_mkdtemp() {
    let dir = write_temp_fixture("phase41-promises", &[("index.ts", PHASE41_PROMISES_TS)]);
    let rt = new_runtime_at(&dir).await;
    let v = run_phase41_fixture(&rt, "promises41", PHASE41_PROMISES_TS, "__phase41promises").await;

    assert_eq!(v["fdIsNumber"], true, "FileHandle.fd must be numeric: {v}");
    assert_eq!(v["wrote"], 12, "FileHandle.write byte count mismatch: {v}");
    assert_eq!(v["wroteBuffer"], true, "write() must echo a Buffer: {v}");
    assert_eq!(
        v["sizeAfterWrite"], 12,
        "FileHandle.stat size mismatch: {v}"
    );
    assert_eq!(
        v["read"], "12:promise-data",
        "FileHandle.read positioned mismatch: {v}"
    );
    assert_eq!(
        v["afterWriteFile"], "promise-data rewritten",
        "FileHandle.writeFile must continue at the tracked offset and readFile must expose the whole file: {v}"
    );
    assert_eq!(
        v["afterClose"], "EBADF",
        "FileHandle.stat after close must report EBADF: {v}"
    );
    assert_eq!(
        v["dirRead"], "3:1.txt:true,2.txt:true,sub:false",
        "Dir.read walk mismatch: {v}"
    );
    assert_eq!(
        v["dirReadAfterNull"],
        Value::Null,
        "Dir.read at the end must resolve null: {v}"
    );
    assert_eq!(
        v["dirClosedRead"], "ERR_DIR_CLOSED",
        "Dir.read after close must reject ERR_DIR_CLOSED (Node's contract): {v}"
    );
    assert_eq!(v["dirIterated"], 3, "Dir async iteration mismatch: {v}");
    assert_eq!(
        v["dirIterClosed"], "ERR_DIR_CLOSED",
        "Dir.close after the iterator ran dry must reject ERR_DIR_CLOSED: {v}"
    );
    assert_eq!(
        v["mkdtempDistinct"], true,
        "fsp.mkdtemp must produce distinct dirs: {v}"
    );
    for path in v["mkdtempPaths"].as_array().into_iter().flatten() {
        if let Some(path) = path.as_str() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// `stream/promises`: finished on flags and events, error rejection,
/// pipeline end-to-end and mid-chain error.
const PHASE41_STREAM_PROMISES_TS: &str = r#"
import { PassThrough } from "node:stream";
import { finished, pipeline } from "node:stream/promises";
import streamPromises from "node:stream/promises";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    const done = new PassThrough();
    done.end("done");
    out.finishedResolved = await finished(done).then(() => true, () => false);

    const failing = new PassThrough();
    const pending = finished(failing);
    failing.destroy(new Error("boom"));
    out.errorRejected = await pending.then(() => "resolved", (e: any) => e.message);

    const s1 = new PassThrough();
    const s2 = new PassThrough();
    const chunks: string[] = [];
    s2.on("data", (chunk: any) => chunks.push(String(chunk)));
    const line = pipeline(s1, s2);
    s1.end("pipe-data");
    out.pipeline = await line.then(() => chunks.join(""), () => "rejected");

    const e1 = new PassThrough();
    const e2 = new PassThrough();
    const failingLine = pipeline(e1, e2);
    e1.destroy(new Error("mid-fail"));
    out.pipelineError = await failingLine.then(() => "resolved", (e: any) => e.message);

    out.defaultShape = streamPromises.finished === finished &&
      typeof streamPromises.pipeline === "function";

    globalThis.__phase41sp = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn stream_promises_finished_and_pipeline() {
    let rt = new_runtime().await;
    let v = run_phase41_fixture(&rt, "sp41", PHASE41_STREAM_PROMISES_TS, "__phase41sp").await;

    assert_eq!(
        v["finishedResolved"], true,
        "finished() must resolve after end() (minimal-pad flag semantics): {v}"
    );
    assert_eq!(
        v["errorRejected"], "boom",
        "an error event must reject finished() with the original error: {v}"
    );
    assert_eq!(
        v["pipeline"], "pipe-data",
        "pipeline must pipe the chain and resolve on the last stream: {v}"
    );
    assert_eq!(
        v["pipelineError"], "mid-fail",
        "any stream's error must reject pipeline: {v}"
    );
    assert_eq!(
        v["defaultShape"], true,
        "the default export must carry the same finished/pipeline: {v}"
    );
}

/// `process.on`/`once`/`off` + the exit handler that must run when the
/// runtime tears down (the test drops the `ExtensionRuntime`, the same Drop
/// the session-quit / `/reload` actor path hits).
const PHASE41_PROCESS_EVENTS_TS: &str = r#"
import fs from "node:fs";
import { join } from "node:path";

export default (pi: any) => {
  pi.on("session_start", () => {
    const out: any = {};
    const order: string[] = [];
    const h1 = () => order.push("h1");
    const h2 = () => order.push("h2");
    process.on("evt", h1);
    process.once("evt", h2);
    out.afterAdd = process.listeners("evt").length;
    out.emit1 = process.emit("evt");
    out.emit2 = process.emit("evt");
    out.order = order.join(",");
    process.off("evt", h1);
    out.afterOnceConsumed = process.listeners("evt").length;
    process.on("evt", h1);
    process.removeAllListeners("evt");
    out.afterRemoveAll = process.listeners("evt").length;
    out.emitNone = process.emit("evt");

    const marker = join(process.cwd(), "exit-marker.txt");
    process.on("exit", (code: number) => {
      fs.writeFileSync(marker, "code=" + code);
    });
    out.exitRegistered = process.listenerCount("exit");

    globalThis.__phase41exit = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn process_events_and_exit_dispatch_on_teardown() {
    let dir = write_temp_fixture("phase41-exit", &[("index.ts", PHASE41_PROCESS_EVENTS_TS)]);
    let rt = new_runtime_at(&dir).await;
    let v = run_phase41_fixture(&rt, "exit41", PHASE41_PROCESS_EVENTS_TS, "__phase41exit").await;

    assert_eq!(v["afterAdd"], 2, "on+once listener count mismatch: {v}");
    assert_eq!(
        v["emit1"], true,
        "emit with listeners must return true: {v}"
    );
    assert_eq!(
        v["emit2"], true,
        "emit after once-consumption returns true: {v}"
    );
    assert_eq!(v["order"], "h1,h2,h1", "on/once emit order mismatch: {v}");
    assert_eq!(
        v["afterOnceConsumed"], 0,
        "once wrapper and off() must remove both listeners: {v}"
    );
    assert_eq!(
        v["afterRemoveAll"], 0,
        "removeAllListeners must clear the event: {v}"
    );
    assert_eq!(
        v["emitNone"], false,
        "emit without listeners must be false: {v}"
    );
    assert_eq!(v["exitRegistered"], 1, "exit handler must register: {v}");

    let marker = dir.join("exit-marker.txt");
    assert!(
        !marker.exists(),
        "the exit handler must not run before teardown"
    );
    drop(rt);
    let content = std::fs::read_to_string(&marker)
        .expect("exit handler must have written the marker at teardown");
    assert_eq!(content, "code=0", "exit handler must receive the exit code");
}

/// `require("events")` returns the class itself (Node semantics); other
/// builtins keep returning the namespace object; `os.devNull` per platform.
const PHASE41_MISC2_TS: &str = r#"
import { createRequire } from "node:module";
import * as os from "node:os";

export default (pi: any) => {
  pi.on("session_start", () => {
    const out: any = {};
    const require = createRequire(import.meta.url);
    const EE = require("events");
    out.isClass = typeof EE === "function";
    out.selfRef = EE.EventEmitter === EE;
    out.nodePrefixSame = require("node:events") === EE;
    const em = new EE();
    let hits = 0;
    em.on("x", () => { hits++; });
    em.emit("x");
    out.emitterWorks = hits === 1;
    const pathNs = require("path");
    // Node semantics: require("path") IS the active namespace object (on
    // Windows that is win32 itself), with no `.default` wrapper.
    out.otherBuiltinNamespace =
      typeof pathNs.join === "function" &&
      pathNs.default === undefined &&
      require("node:path") === pathNs;
    out.devNull = os.devNull;
    globalThis.__phase41misc2 = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn require_events_returns_class_and_os_dev_null() {
    let rt = new_runtime().await;
    let v = run_phase41_fixture(&rt, "misc2", PHASE41_MISC2_TS, "__phase41misc2").await;

    assert_eq!(
        v["isClass"], true,
        "require('events') must be the EventEmitter class: {v}"
    );
    assert_eq!(
        v["selfRef"], true,
        "EventEmitter.EventEmitter must self-reference: {v}"
    );
    assert_eq!(
        v["nodePrefixSame"], true,
        "require('node:events') must be the same class: {v}"
    );
    assert_eq!(
        v["emitterWorks"], true,
        "the class must construct + emit: {v}"
    );
    assert_eq!(
        v["otherBuiltinNamespace"], true,
        "other builtins keep returning the module namespace: {v}"
    );
    let expected_dev_null = if cfg!(windows) {
        r"\\.\nul"
    } else {
        "/dev/null"
    };
    assert_eq!(
        v["devNull"], expected_dev_null,
        "os.devNull platform value mismatch: {v}"
    );
}

// ---------------------------------------------------------------------------
// Phase 4b: pi-tui minimal components, worker_threads stub, Intl pad
// ---------------------------------------------------------------------------

/// `@earendil-works/pi-tui` minimal surface: Text construction/render line
/// counts, ANSI-aware `truncateToWidth`/`visibleWidth`, Container/Box/Spacer.
const PHASE4B_TUI_TS: &str = r#"
import { Text, truncateToWidth, visibleWidth, Container, Box, Spacer } from "@earendil-works/pi-tui";

export default (pi: any) => {
  pi.on("session_start", () => {
    const out: any = {};
    const text = new Text("hello\nworld", 0, 0);
    const lines = text.render(10);
    out.lineCount = lines.length;
    out.firstLine = lines[0];
    out.secondLine = lines[1];
    out.getText = text.getText();
    text.setText("changed");
    out.changed = text.getText();
    out.blankLines = new Text("", 0, 0).render(5).length;

    const padded = new Text("x", 1, 1).render(5);
    out.paddedCount = padded.length;
    out.paddedMiddle = padded[1];

    out.truncated = truncateToWidth("hello world", 5, "\u2026");
    out.truncatedDefaultEllipsis = truncateToWidth("hello world", 8);
    const ansi = truncateToWidth("\u001b[31mhello world\u001b[0m", 8, "\u2026");
    out.ansi = ansi;
    out.ansiWidth = visibleWidth(ansi);
    out.plainWidth = visibleWidth("ab\u4e2d");
    out.styledWidth = visibleWidth("\u001b[31mabc\u001b[0m");

    const container = new Container();
    container.addChild(new Text("a", 0, 0));
    container.addChild(new Text("b", 0, 0));
    out.containerLines = container.render(3).length;
    out.containerAfterRemove = (() => {
      const child = new Text("z", 0, 0);
      container.addChild(child);
      container.removeChild(child);
      return container.render(3).length;
    })();

    out.spacerLines = new Spacer(2).render(3).length;

    const box = new Box(1, 0);
    box.addChild(new Text("q", 0, 0));
    const boxLines = box.render(5);
    out.boxLines = boxLines.length;
    out.boxLine = boxLines[0];

    globalThis.__phase4bTui = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn pi_tui_text_and_truncate_are_usable() {
    let rt = new_runtime().await;
    let v = run_phase41_fixture(&rt, "tui4b", PHASE4B_TUI_TS, "__phase4bTui").await;

    assert_eq!(v["lineCount"], 2, "two input lines render two lines: {v}");
    assert_eq!(
        v["firstLine"], "hello     ",
        "render must pad each line to the width: {v}"
    );
    assert_eq!(
        v["secondLine"], "world     ",
        "second line must carry its own text: {v}"
    );
    assert_eq!(v["getText"], "hello\nworld", "{v}");
    assert_eq!(v["changed"], "changed", "setText/getText roundtrip: {v}");
    assert_eq!(
        v["blankLines"], 0,
        "blank text renders nothing (upstream early return): {v}"
    );
    assert_eq!(
        v["paddedCount"], 3,
        "paddingY=1 adds one blank line above and below: {v}"
    );
    assert_eq!(
        v["paddedMiddle"], " x   ",
        "paddingX=1 indents and the line pads to full width: {v}"
    );
    assert_eq!(
        v["truncated"], "hell\u{2026}",
        "truncation keeps width minus ellipsis columns: {v}"
    );
    assert_eq!(
        v["truncatedDefaultEllipsis"], "hello...",
        "default ellipsis is upstream's \"...\": {v}"
    );
    let ansi = v["ansi"].as_str().unwrap_or_default();
    assert!(
        ansi.starts_with("\u{1b}[31mhello w"),
        "ANSI prefix must survive while columns are counted: {ansi:?}"
    );
    assert!(
        ansi.ends_with("\u{1b}[0m\u{2026}"),
        "a reset must close the truncated span before the ellipsis: {ansi:?}"
    );
    assert_eq!(
        v["ansiWidth"], 8,
        "escape sequences must not count toward width: {v}"
    );
    assert_eq!(
        v["plainWidth"], 3,
        "code-point counting (CJK counts as 1, documented limit): {v}"
    );
    assert_eq!(v["styledWidth"], 3, "styled text measures its content: {v}");
    assert_eq!(
        v["containerLines"], 2,
        "Container concatenates child renders: {v}"
    );
    assert_eq!(
        v["containerAfterRemove"], 2,
        "removeChild must drop the child again: {v}"
    );
    assert_eq!(v["spacerLines"], 2, "Spacer(n) renders n blank lines: {v}");
    assert_eq!(v["boxLines"], 1, "Box renders its child lines: {v}");
    assert_eq!(
        v["boxLine"], " q   ",
        "Box paddingX indents and pads to full width: {v}"
    );
}

/// `node:worker_threads`: real in-process workers (see
/// `tests/worker_threads_real.rs` for the live-thread surface). This fixture
/// pins the main-thread defaults plus the Node-parity failure shape: a
/// worker whose entry file does not exist fails asynchronously through the
/// `error` event (construction stays silent), `terminate()` resolves with
/// exit code 1, and postMessage on a dead worker is a silent no-op.
const PHASE4B_WORKER_TS: &str = r#"
import { Worker, isMainThread, parentPort, workerData, threadId, getEnvironmentData, setEnvironmentData } from "node:worker_threads";
import { setTimeout as delay } from "node:timers/promises";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    out.isMainThread = isMainThread;
    out.parentPort = parentPort;
    out.workerData = workerData;
    out.threadId = threadId;
    setEnvironmentData("probe", "value");
    out.envData = getEnvironmentData("probe");

    const worker = new Worker("./worker.js");
    out.constructed = worker instanceof Worker;
    let errorMessage = null;
    worker.on("error", (error: any) => { errorMessage = error.message; });
    out.terminated = await worker.terminate();
    worker.postMessage({ ping: 1 });
    worker.unref();
    await delay(20);
    out.errorMessage = errorMessage;
    globalThis.__phase4bWorker = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn worker_threads_main_thread_stub_loads_and_signals_async() {
    let rt = new_runtime().await;
    let v = run_phase41_fixture(&rt, "worker4b", PHASE4B_WORKER_TS, "__phase4bWorker").await;

    assert_eq!(v["isMainThread"], true, "{v}");
    assert!(v["parentPort"].is_null(), "parentPort must be null: {v}");
    assert!(v["workerData"].is_null(), "workerData must be null: {v}");
    assert_eq!(v["threadId"], 0, "{v}");
    assert_eq!(v["envData"], "value", "environment-data map roundtrip: {v}");
    assert_eq!(
        v["constructed"], true,
        "construction must not throw (lazy failure): {v}"
    );
    assert_eq!(
        v["terminated"], 1,
        "terminate() resolves with exit code 1 (start failed): {v}"
    );
    assert!(
        v["errorMessage"]
            .as_str()
            .unwrap_or_default()
            .contains("Cannot find module './worker.js'"),
        "the missing-entry error must arrive asynchronously with the Node message: {v}"
    );
}

/// `Intl.Segmenter` pad: construction, code-point segmentation, containing()
/// and the empty-list helpers.
const PHASE4B_INTL_TS: &str = r#"
export default (pi: any) => {
  pi.on("session_start", () => {
    const out: any = {};
    const segmenter = new Intl.Segmenter(undefined, { granularity: "grapheme" });
    const resolved = segmenter.resolvedOptions();
    out.granularity = resolved.granularity;
    out.locale = resolved.locale;
    const parts: string[] = [];
    let bIndex = -1;
    for (const item of segmenter.segment("ab\u4e2d")) {
      parts.push(item.segment);
      if (item.segment === "b") bIndex = item.index;
    }
    out.parts = parts;
    out.bIndex = bIndex;
    const containing = segmenter.segment("xy").containing(1);
    out.containing = containing ? containing.segment : null;
    out.containingMissing = segmenter.segment("xy").containing(7) === undefined;
    out.canonical = Intl.getCanonicalLocales("en-US");
    out.supported = Intl.supportedValuesOf("calendar");
    globalThis.__phase4bIntl = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn intl_segmenter_pad_segments_by_code_point() {
    let rt = new_runtime().await;
    let v = run_phase41_fixture(&rt, "intl4b", PHASE4B_INTL_TS, "__phase4bIntl").await;

    assert_eq!(v["granularity"], "grapheme", "{v}");
    assert_eq!(v["locale"], "en", "{v}");
    assert_eq!(
        v["parts"],
        json!(["a", "b", "\u{4e2d}"]),
        "code-point iteration order: {v}"
    );
    assert_eq!(v["bIndex"], 1, "index must advance by code point: {v}");
    assert_eq!(
        v["containing"], "y",
        "containing(1) must find the item: {v}"
    );
    assert_eq!(
        v["containingMissing"], true,
        "containing() past the end returns undefined: {v}"
    );
    assert_eq!(v["canonical"], json!([]), "{v}");
    assert_eq!(v["supported"], json!([]), "{v}");
}

/// Phase 4c fs policy: extension data dirs under the pi config root
/// (`agent_dir`'s parent, i.e. `~/.pi/<ext>` — context-mode mkdirs
/// `~/.pi/context-mode/sessions`) are writable through the fs shim (the
/// system-directory write fence is covered by
/// `fs_reads_are_global_writes_stay_rooted`). The fixture creates a temp
/// subdir under the real agent dir's parent, writes through it, then the host
/// removes it.
#[tokio::test(flavor = "current_thread")]
async fn fs_policy_allows_pi_config_root_data_dirs() {
    let agent_dir = pir::ai::providers::agent_dir();
    let Some(pi_root) = agent_dir.parent().map(Path::to_path_buf) else {
        eprintln!(
            "[node_shims] agent dir `{}` has no parent; skipping pi-root fs case",
            agent_dir.display()
        );
        return;
    };
    if !pi_root.is_absolute() {
        eprintln!(
            "[node_shims] agent dir `{}` is not absolute; skipping pi-root fs case",
            agent_dir.display()
        );
        return;
    }
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let subdir = format!("pir-fs-policy-{}-{unique}", std::process::id());
    let target = pi_root.join(&subdir);
    let dir_literal =
        serde_json::to_string(&target.to_string_lossy()).expect("json-escape fixture dir");

    let ts = format!(
        r#"
import fs from "node:fs";
import {{ join }} from "node:path";

export default (pi: any) => {{
  pi.on("session_start", async () => {{
    const out: any = {{}};
    // Exactly the context-mode shape: mkdir ~/.pi/<ext>/sessions + write.
    const dir = {dir_literal};
    fs.mkdirSync(dir, {{ recursive: true }});
    const file = join(dir, "probe.txt");
    fs.writeFileSync(file, "pi-root-ok", "utf8");
    out.text = fs.readFileSync(file, "utf8");
    out.exists = fs.existsSync(file);
    globalThis.__piRootFs = out;
  }});
}};
"#,
    );

    let rt = new_runtime_at(&std::env::temp_dir()).await;
    let v = run_phase41_fixture(&rt, "pi-root-fs", &ts, "__piRootFs").await;

    // Host-visible result, then delete the temp subdir (created by the shim).
    let host_file = target.join("probe.txt");
    assert!(host_file.is_file(), "host must see the written file: {v}");
    let _ = std::fs::remove_dir_all(&target);
    assert!(
        !target.exists(),
        "temp pi-root subdir must be removed again"
    );

    assert_eq!(v["text"], "pi-root-ok", "{v}");
    assert_eq!(v["exists"], true, "{v}");
}

// ---------------------------------------------------------------------------
// Phase 4e: net (real isIP) / dns/promises (real lookup) / async_hooks
// (AsyncLocalStorage approximation) / http/https/tls/sqlite stubs / crypto
// pbkdf2Sync + decipher/sign load-compatible faces
// ---------------------------------------------------------------------------

/// One fixture linking every phase-4e surface: real isIP verdicts, the
/// loopback-real net socket/server faces (see tests/net_shims.rs for the E2E),
/// a real dns lookup of localhost, the AsyncLocalStorage approximation probes,
/// the http/https/tls/sqlite stub texts, and the PBKDF2 vectors.
const PHASE4E_NODE_TS: &str = r#"
import net, { isIP, Socket as NetSocket, SocketAddress } from "node:net";
import dns, { lookup as dnsLookup, resolve4 as dnsResolve4 } from "node:dns/promises";
import http, { createServer as httpCreateServer, IncomingMessage, ServerResponse, STATUS_CODES, METHODS } from "node:http";
import https, { createServer as httpsCreateServer } from "node:https";
import tls from "node:tls";
import sqlite, { DatabaseSync, StatementSync } from "node:sqlite";
import { AsyncLocalStorage, AsyncResource, executionAsyncId, triggerAsyncId } from "node:async_hooks";
import { pbkdf2Sync, createSign, createVerify, createDecipheriv } from "node:crypto";
import { setTimeout as delay } from "node:timers/promises";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};

    // --- net: real isIP ---
    out.isIP = {
      ipv4: isIP("127.0.0.1"),
      v4TooBig: isIP("256.1.1.1"),
      v4FiveParts: isIP("1.2.3.4.5"),
      v4LeadingZero: isIP("1.1.1.01"),
      ipv6Loopback: isIP("::1"),
      ipv6Full: isIP("2001:db8::1"),
      ipv6Zone: isIP("fe80::1%eth0"),
      ipv6EmbeddedV4: isIP("::ffff:127.0.0.1"),
      ipv6NineGroups: isIP("1:2:3:4:5:6:7:8:9"),
      notAnIp: isIP("not-an-ip"),
      empty: isIP(""),
      defaultFace: net.isIP("10.0.0.1"),
    };

    try {
      new NetSocket();
      out.netSocket = "no-throw";
    } catch (error: any) {
      out.netSocket = error.message;
      out.netSocketCode = error.code;
    }
    try {
      const server = net.createServer();
      out.netServer = typeof server.listen === "function" && typeof server.close === "function" ? "no-throw" : "missing-methods";
    } catch (error: any) {
      out.netServer = error.message;
    }
    // The listen/connect surface is loopback-real; a non-loopback host is the
    // one synchronous rejection (TLS/Unix sockets are out of tier too).
    try {
      net.connect({ port: 9, host: "198.51.100.7" });
      out.netConnect = "no-throw";
    } catch (error: any) {
      out.netConnect = error.message;
      out.netConnectCode = error.code;
    }
    const socketAddress = new SocketAddress({ address: "127.0.0.1", port: 80, family: "ipv4" });
    out.socketAddress = [socketAddress.address, socketAddress.port, socketAddress.family].join("|");

    // --- dns/promises: real lookup + rejected resolve faces ---
    try {
      const resolved = await dnsLookup("localhost");
      out.dns = { address: resolved.address, family: resolved.family };
      out.dnsAllIsArray = Array.isArray(await dns.lookup("localhost", { all: true }));
    } catch (error: any) {
      out.dnsError = String(error && error.code ? error.code + ": " + error.message : error);
    }
    try {
      await dnsResolve4("localhost");
      out.dnsResolve4 = "resolved";
    } catch (error: any) {
      out.dnsResolve4 = error.message;
    }

    // --- async_hooks: AsyncLocalStorage approximation ---
    const storage = new AsyncLocalStorage();
    out.alsBefore = storage.getStore() === undefined;
    let seenInside;
    const returned = storage.run({ tag: 1 }, () => {
      seenInside = storage.getStore().tag;
      return "ret";
    });
    out.alsSync = [seenInside, returned, storage.getStore() === undefined];

    const order: number[] = [];
    await storage.run({ tag: 2 }, async () => {
      order.push(storage.getStore().tag);
      await delay(10);
      order.push(storage.getStore().tag);
    });
    out.alsAsync = [order.join(","), storage.getStore() === undefined];

    out.alsNested = storage.run({ tag: 3 }, () => {
      const outer = storage.getStore().tag;
      const inner = storage.run({ tag: 4 }, () => storage.getStore().tag);
      return [outer, inner, storage.getStore().tag];
    });

    storage.enterWith({ tag: 5 });
    out.alsEnterWith = storage.getStore().tag;
    out.alsExitHidden = storage.exit(() => storage.getStore() === undefined);
    out.alsExitRestored = storage.getStore().tag;
    storage.disable();
    out.alsDisabled = storage.getStore() === undefined;

    out.asyncResource = new AsyncResource("probe").runInAsyncScope(() => 42);
    out.asyncIds = executionAsyncId() === 0 && triggerAsyncId() === 0;
    try {
      AsyncLocalStorage.snapshot();
      out.alsSnapshot = "no-throw";
    } catch (error: any) {
      out.alsSnapshot = error.message;
    }

    // --- http / https / tls stubs ---
    try {
      httpCreateServer(() => {});
      out.httpCreateServer = "no-throw";
    } catch (error: any) {
      out.httpCreateServer = error.message;
      out.httpCreateServerCode = error.code;
    }
    try {
      httpsCreateServer(() => {});
      out.httpsCreateServer = "no-throw";
    } catch (error: any) {
      out.httpsCreateServer = error.message;
    }
    try {
      tls.connect({ host: "localhost" });
      out.tlsConnect = "no-throw";
    } catch (error: any) {
      out.tlsConnect = error.message;
    }
    out.httpSkeletons = [new IncomingMessage().headers !== undefined, new ServerResponse().statusCode];
    out.httpTables = [STATUS_CODES[426], METHODS.indexOf("GET") !== -1, METHODS.indexOf("CONNECT") !== -1, METHODS.length];
    out.httpDefaultFace = typeof http.createServer === "function" && typeof https.request === "function";

    // --- sqlite (real since the sqlite wave: DatabaseSync opens, direct
    // StatementSync construction stays illegal like real Node) ---
    try {
      new DatabaseSync(":memory:").close();
      out.sqlite = "no-throw";
    } catch (error: any) {
      out.sqlite = error.message;
    }
    let statementCtor = "";
    try {
      new StatementSync();
    } catch (error: any) {
      statementCtor = error.message;
    }
    out.sqliteStatementSkeleton = statementCtor;
    out.sqliteDefaultFace = typeof sqlite.DatabaseSync === "function";

    // --- crypto: real pbkdf2Sync + load-compatible decipher/sign faces ---
    const sha256Key = pbkdf2Sync("password", "salt", 1, 32, "sha256");
    out.pbkdf2 = {
      sha256: sha256Key.toString("hex"),
      sha256IsBuffer: Buffer.isBuffer(sha256Key),
      sha1Rfc6070: pbkdf2Sync("password", "salt", 4096, 20, "sha1").toString("hex"),
      bufferInputs: pbkdf2Sync(Buffer.from("password"), Buffer.from("salt"), 1, 32, "SHA-256").toString("hex"),
    };
    try {
      createSign("RSA-SHA256").update("data");
      out.sign = "no-throw";
    } catch (error: any) {
      out.sign = error.message;
    }
    try {
      createVerify("RSA-SHA256").verify({}, "sig");
      out.verify = "no-throw";
    } catch (error: any) {
      out.verify = error.message;
    }
    try {
      createDecipheriv("aes-256-cbc", Buffer.alloc(32), Buffer.alloc(16)).update(Buffer.alloc(16));
      out.decipher = "no-throw";
    } catch (error: any) {
      out.decipher = error.message;
    }

    globalThis.__phase4eNode = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn phase4e_net_dns_async_hooks_stubs_and_pbkdf2() {
    let dir = write_temp_fixture("phase4e-node", &[("index.ts", PHASE4E_NODE_TS)]);
    let rt = new_runtime_at(&dir).await;
    let v = run_phase41_fixture(&rt, "phase4e-node", PHASE4E_NODE_TS, "__phase4eNode").await;

    // --- isIP verdicts (Node semantics: 4/6/0) ---
    assert_eq!(v["isIP"]["ipv4"], 4, "{v}");
    assert_eq!(v["isIP"]["v4TooBig"], 0, "{v}");
    assert_eq!(v["isIP"]["v4FiveParts"], 0, "{v}");
    assert_eq!(
        v["isIP"]["v4LeadingZero"], 0,
        "leading zeros are not IPv4: {v}"
    );
    assert_eq!(v["isIP"]["ipv6Loopback"], 6, "{v}");
    assert_eq!(v["isIP"]["ipv6Full"], 6, "{v}");
    assert_eq!(v["isIP"]["ipv6Zone"], 6, "zone suffix is still IPv6: {v}");
    assert_eq!(v["isIP"]["ipv6EmbeddedV4"], 6, "{v}");
    assert_eq!(v["isIP"]["ipv6NineGroups"], 0, "{v}");
    assert_eq!(v["isIP"]["notAnIp"], 0, "{v}");
    assert_eq!(v["isIP"]["empty"], 0, "{v}");
    assert_eq!(
        v["isIP"]["defaultFace"], 4,
        "default export carries isIP: {v}"
    );

    // --- net: the socket/server faces are real loopback TCP now ---
    assert_eq!(
        v["netSocket"], "no-throw",
        "new net.Socket() is a real (unconnected) socket: {v}"
    );
    assert_eq!(
        v["netServer"], "no-throw",
        "net.createServer() is a real server: {v}"
    );
    assert_eq!(
        v["netConnectCode"], "ERR_NOT_SUPPORTED",
        "non-loopback hosts are out of tier: {v}"
    );
    assert!(
        v["netConnect"]
            .as_str()
            .unwrap_or_default()
            .contains("loopback"),
        "the rejection must name the loopback rule: {v}"
    );
    assert_eq!(v["socketAddress"], "127.0.0.1|80|ipv4", "{v}");

    // --- dns lookup: either a real localhost resolution or a guarded skip ---
    if let Some(error) = v["dnsError"].as_str() {
        eprintln!("[node_shims] dns lookup guard failed ({error}); skipping localhost assertions");
    } else {
        let address = v["dns"]["address"].as_str().unwrap_or_default();
        assert!(
            address == "127.0.0.1" || address == "::1",
            "localhost must resolve to a loopback address: {v}"
        );
        let family = v["dns"]["family"].as_u64().unwrap_or(0);
        assert!(
            (family == 4 && address == "127.0.0.1") || (family == 6 && address == "::1"),
            "family must match the address: {v}"
        );
        assert_eq!(v["dnsAllIsArray"], true, "all:true returns an array: {v}");
    }
    assert!(
        v["dnsResolve4"]
            .as_str()
            .unwrap_or_default()
            .contains("dns.promises.resolve4 is not supported by the pir extension runtime"),
        "unsupported resolve faces reject with the clear text: {v}"
    );

    // --- AsyncLocalStorage approximation ---
    assert_eq!(v["alsBefore"], true, "no store before the first run: {v}");
    assert_eq!(
        v["alsSync"],
        json!([1, "ret", true]),
        "sync run: store visible inside, restored after: {v}"
    );
    assert_eq!(
        v["alsAsync"],
        json!(["2,2", true]),
        "the store must survive an await inside run: {v}"
    );
    assert_eq!(
        v["alsNested"],
        json!([3, 4, 3]),
        "nested run must restore the outer store: {v}"
    );
    assert_eq!(v["alsEnterWith"], 5, "{v}");
    assert_eq!(v["alsExitHidden"], true, "exit hides the store: {v}");
    assert_eq!(v["alsExitRestored"], 5, "exit restores what it hid: {v}");
    assert_eq!(v["alsDisabled"], true, "disable clears the store: {v}");
    assert_eq!(v["asyncResource"], 42, "{v}");
    assert_eq!(v["asyncIds"], true, "async ids report 0: {v}");
    assert_eq!(
        v["alsSnapshot"],
        "AsyncLocalStorage.snapshot is not supported by the pir extension runtime",
        "{v}"
    );

    // --- http/https/tls stubs ---
    // node:http is real now (client + minimal loopback server): createServer
    // constructs instead of throwing.
    assert_eq!(v["httpCreateServer"], "no-throw", "{v}");
    // The TLS server face stays throws-tier.
    assert_eq!(
        v["httpsCreateServer"],
        "node:https.createServer is not supported by the pir extension runtime (the runtime hosts no TLS server)",
        "{v}"
    );
    assert!(
        v["tlsConnect"]
            .as_str()
            .unwrap_or_default()
            .contains("node:tls.connect is not supported by the pir extension runtime"),
        "{v}"
    );
    assert_eq!(v["httpSkeletons"], json!([true, 200]), "{v}");
    assert_eq!(
        v["httpTables"],
        json!(["Upgrade Required", true, true, 34]),
        "status table + Node's METHODS array: {v}"
    );
    assert_eq!(v["httpDefaultFace"], true, "{v}");

    // --- sqlite real (sqlite wave) ---
    assert_eq!(v["sqlite"], "no-throw", "{v}");
    assert_eq!(v["sqliteStatementSkeleton"], "Illegal constructor", "{v}");
    assert_eq!(v["sqliteDefaultFace"], true, "{v}");

    // --- crypto: PBKDF2 vectors (RFC 6070 sha1; the fixed sha256 vector) ---
    assert_eq!(
        v["pbkdf2"]["sha256"], "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b",
        "PBKDF2-HMAC-SHA256(\"password\",\"salt\",1,32) vector: {v}"
    );
    assert_eq!(
        v["pbkdf2"]["sha1Rfc6070"], "4b007901b765489abead49d926f721d065a429c1",
        "RFC 6070 PBKDF2-HMAC-SHA1 case 5 (c=4096): {v}"
    );
    assert_eq!(v["pbkdf2"]["sha256IsBuffer"], true, "{v}");
    assert_eq!(
        v["pbkdf2"]["bufferInputs"], v["pbkdf2"]["sha256"],
        "Buffer inputs and \"SHA-256\" spelling must agree with the string form: {v}"
    );
    assert_eq!(v["sign"], "no-throw", "createSign is a real face now: {v}");
    assert!(
        v["verify"]
            .as_str()
            .unwrap_or_default()
            .contains("argument must be"),
        "createVerify is a real face now and validates the key: {v}"
    );
    assert_eq!(
        v["decipher"], "no-throw",
        "createDecipheriv is a real face now: {v}"
    );
}

// ---------------------------------------------------------------------------
// fs/os argument-validation family (node-suite backlog B): the shapes Node's
// own official tests assert — deprecated constant getters, chmod modes as
// octal strings, readFile flags, stream byte windows, mkdtemp path kinds and
// the Dir handle contract.
// ---------------------------------------------------------------------------

const PHASE41_FS_VALIDATION_TS: &str = r#"
import fs from "node:fs";
import os from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const out: any = {};
    const root = process.cwd();
    const file = join(root, "val.txt");
    fs.writeFileSync(file, "abc\ndef");

    // fs.constants: Node builds it on a null prototype.
    out.constantsProtoNull = Object.getPrototypeOf(fs.constants) === null;
    out.constantsS_IRUSR = fs.constants.S_IRUSR;

    // The single-letter constants are deprecated getters on `fs` itself: no
    // setter (a strict-mode write is a TypeError) and a DEP0176 read warning.
    const warnings: any[] = [];
    process.on("warning", (w: any) => warnings.push(w.name + ":" + w.code));
    let constantWrite = "no-throw";
    try {
      fs.F_OK = "overwritten";
    } catch (e: any) {
      constantWrite = e.name;
    }
    out.constantWrite = constantWrite;
    out.constantValue = fs.F_OK === fs.constants.F_OK;
    const constantDesc: any = Object.getOwnPropertyDescriptor(fs, "F_OK");
    out.constantDesc =
      [constantDesc.enumerable, constantDesc.configurable, typeof constantDesc.set].join(",");
    await new Promise((resolve) => setTimeout(resolve, 5));
    out.constantWarnings = warnings.join(",");

    // os.EOL is a non-writable, still-configurable data property.
    out.eolWritable = Object.getOwnPropertyDescriptor(os, "EOL").writable;
    let eolWrite = "no-throw";
    try {
      os.EOL = 123;
    } catch (e: any) {
      eolWrite = e.name;
    }
    out.eolWrite = eolWrite;
    out.eolValue = os.EOL === (process.platform === "win32" ? "\r\n" : "\n");

    // chmod accepts an octal string and stat reports libuv's mode bits.
    fs.chmodSync(file, "400");
    out.modeReadOnly = ((fs.statSync(file).mode & 0o777) & 0o400) !== 0;
    fs.chmodSync(file, 0o600);
    out.modeWritable = ((fs.statSync(file).mode & 0o777) & 0o200) !== 0;
    const fd = fs.openSync(file, "r+");
    fs.fchmodSync(fd, "400");
    out.fchmodMode = ((fs.fstatSync(fd).mode & 0o777) & 0o400) !== 0;
    fs.closeSync(fd);
    fs.chmodSync(file, 0o600);

    // Argument validation: exact Node messages and the `TypeError [CODE]: msg`
    // stringification the official tests match with regexes.
    const invalidArg = (run: () => void) => {
      try {
        run();
        return "no-throw";
      } catch (e: any) {
        return e.name + "|" + e.code + "|" + String(e);
      }
    };
    out.chmodBadPath = invalidArg(() => fs.chmodSync({}, 1));
    out.chmodBadMode = invalidArg(() => fs.fchmodSync(1, {}));
    out.streamBadStart = invalidArg(() => fs.createReadStream(file, { start: "4" }));
    out.opendirNoCallback = invalidArg(() => (fs as any).opendir(file));
    out.opendirBadBuffer = invalidArg(() => fs.opendirSync(root, { bufferSize: 0 }));

    // readFile flags: a+ creates, ax+ refuses an existing file, r is ENOENT.
    const created = join(root, "created-by-flag");
    out.readFlagCreate = fs.readFileSync(created, { encoding: "utf8", flag: "a+" }) === "";
    out.readFlagExists = fs.existsSync(created);
    try {
      fs.readFileSync(created, { encoding: "utf8", flag: "ax+" });
      out.readFlagExclusive = "no-throw";
    } catch (e: any) {
      out.readFlagExclusive = e.code;
    }
    try {
      fs.readFileSync(join(root, "missing-entirely"), "utf8");
      out.readFlagMissing = "no-throw";
    } catch (e: any) {
      out.readFlagMissing = e.code;
    }

    // Streams honour the inclusive start/end byte window.
    const chunks: string[] = [];
    await new Promise<void>((resolve) => {
      const rs = fs.createReadStream(file, { start: 4, end: 6 });
      rs.on("data", (chunk: any) => chunks.push(String(chunk)));
      rs.on("end", () => resolve());
    });
    out.streamWindow = chunks.join("");

    // mkdtemp takes every path kind and replaces the template tail.
    const dirs = [
      fs.mkdtempSync(join(root, "pre.")),
      fs.mkdtempSync(pathToFileURL(join(root, "url."))),
      fs.mkdtempSync(Buffer.from(join(root, "buf."))),
      fs.mkdtempSync(new TextEncoder().encode(join(root, "u8."))),
    ];
    out.mkdtempNames = dirs.map((d) => d.slice(root.length + 1).length).join(",");
    out.mkdtempAllExist = dirs.every((d) => fs.existsSync(d));

    // Dir handle: one class for the sync and promise faces, with Node's
    // closed/concurrent/invalid-this error codes.
    const dir = fs.opendirSync(root);
    out.dirIsDir = dir instanceof fs.Dir;
    const dirents: any[] = [];
    for (;;) {
      const entry: any = dir.readSync();
      if (entry === null) break;
      dirents.push(entry);
    }
    out.direntsAreDirents = dirents.every((d) => d instanceof fs.Dirent);
    out.direntParentPath = dirents.every((d) => d.parentPath === root);
    dir.closeSync();
    out.dirClosedRead = invalidArg(() => dir.readSync());
    out.dirPathThis = invalidArg(() => (dir.constructor.prototype as any).path);
    out.dirOnFile = invalidArg(() => fs.opendirSync(file));
    out.dirLchmod = typeof (fs as any).lchmod;

    globalThis.__phase41fsValidation = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn fs_argument_validation_family() {
    let dir = write_temp_fixture(
        "phase41-fs-validation",
        &[("index.ts", PHASE41_FS_VALIDATION_TS)],
    );
    let rt = new_runtime_at(&dir).await;
    let v = run_phase41_fixture(
        &rt,
        "fsValidation",
        PHASE41_FS_VALIDATION_TS,
        "__phase41fsValidation",
    )
    .await;

    assert_eq!(v["constantsProtoNull"], true, "fs.constants prototype: {v}");
    assert_eq!(v["constantsS_IRUSR"], 256, "fs.constants.S_IRUSR: {v}");
    assert_eq!(
        v["constantWrite"], "TypeError",
        "writing a deprecated constant must be a strict-mode TypeError: {v}"
    );
    assert_eq!(
        v["constantValue"], true,
        "fs.F_OK must equal constants.F_OK: {v}"
    );
    assert_eq!(
        v["constantDesc"], "false,false,undefined",
        "deprecated constants are non-enumerable, non-configurable getters (Node parity): {v}"
    );
    assert_eq!(
        v["constantWarnings"], "DeprecationWarning:DEP0176",
        "reading fs.F_OK must emit DEP0176 exactly once: {v}"
    );
    assert_eq!(v["eolWritable"], false, "os.EOL must not be writable: {v}");
    assert_eq!(
        v["eolWrite"], "TypeError",
        "os.EOL assignment must throw: {v}"
    );
    assert_eq!(v["eolValue"], true, "os.EOL value: {v}");
    assert_eq!(
        v["modeReadOnly"], true,
        "chmod '400' must clear the write bit: {v}"
    );
    assert_eq!(
        v["modeWritable"], true,
        "chmod 0o600 must set the write bit: {v}"
    );
    assert_eq!(
        v["fchmodMode"], true,
        "fchmod '400' must clear the write bit: {v}"
    );
    assert!(
        v["chmodBadPath"]
            .as_str()
            .unwrap_or_default()
            .starts_with("TypeError|ERR_INVALID_ARG_TYPE|TypeError [ERR_INVALID_ARG_TYPE]: The \"path\" argument must be of type string or an instance of Buffer or URL. Received an instance of Object"),
        "chmod path validation shape: {v}"
    );
    assert!(
        v["chmodBadMode"]
            .as_str()
            .unwrap_or_default()
            .starts_with("TypeError|ERR_INVALID_ARG_TYPE|TypeError [ERR_INVALID_ARG_TYPE]: The \"mode\" argument must be of type number. Received an instance of Object"),
        "fchmod mode validation shape: {v}"
    );
    assert!(
        v["streamBadStart"]
            .as_str()
            .unwrap_or_default()
            .contains("The \"start\" argument must be of type number. Received type string ('4')"),
        "createReadStream start validation: {v}"
    );
    assert!(
        v["opendirNoCallback"]
            .as_str()
            .unwrap_or_default()
            .contains("TypeError [ERR_INVALID_ARG_TYPE]: The \"callback\" argument must be of type function. Received undefined"),
        "opendir callback validation: {v}"
    );
    assert!(
        v["opendirBadBuffer"]
            .as_str()
            .unwrap_or_default()
            .starts_with("RangeError|ERR_OUT_OF_RANGE|"),
        "opendir bufferSize range validation: {v}"
    );
    assert_eq!(v["readFlagCreate"], true, "readFile with a+ creates: {v}");
    assert_eq!(
        v["readFlagExists"], true,
        "readFile with a+ must land the file: {v}"
    );
    assert_eq!(
        v["readFlagExclusive"], "EEXIST",
        "readFile with ax+ on an existing file: {v}"
    );
    assert_eq!(
        v["readFlagMissing"], "ENOENT",
        "readFile default flag on a missing file: {v}"
    );
    assert_eq!(
        v["streamWindow"], "def",
        "the inclusive start/end window: {v}"
    );
    assert_eq!(
        v["mkdtempNames"], "10,10,10,9",
        "mkdtemp template replacement across string/URL/Buffer/Uint8Array: {v}"
    );
    assert_eq!(v["mkdtempAllExist"], true, "mkdtemp results exist: {v}");
    assert_eq!(v["dirIsDir"], true, "opendirSync must return a Dir: {v}");
    assert_eq!(
        v["direntsAreDirents"], true,
        "readSync must return Dirents: {v}"
    );
    assert_eq!(v["direntParentPath"], true, "Dirent.parentPath: {v}");
    assert!(
        v["dirClosedRead"]
            .as_str()
            .unwrap_or_default()
            .contains("ERR_DIR_CLOSED"),
        "readSync on a closed handle: {v}"
    );
    assert!(
        v["dirPathThis"]
            .as_str()
            .unwrap_or_default()
            .contains("ERR_INVALID_THIS"),
        "Dir.prototype.path without a receiver: {v}"
    );
    assert!(
        v["dirOnFile"]
            .as_str()
            .unwrap_or_default()
            .contains("ENOTDIR"),
        "opendir on a file must be ENOTDIR: {v}"
    );
    #[cfg(windows)]
    assert_eq!(
        v["dirLchmod"], "undefined",
        "fs.lchmod exists only on macOS (Node exposes the name, not a value): {v}"
    );
}

// ---------------------------------------------------------------------------
// fs write text-channel boundaries (`dataText` vs base64 `data`)
//
// Every fs write call site funnels through `pirWriteData` (fs_shim.js.txt:266)
// and its guard `pirWriteTextEncodable` (://253): a string whose bytes are
// exactly its UTF-8 encoding rides the JSON `dataText` channel, and any other
// payload — Buffer/typed array, an encoding that does not normalize to
// utf8/utf-8, or a lone surrogate — keeps the base64 `data` channel. The
// write stream applies the same guard chunk by chunk (`WriteStream.write`,
// ://1462) and commits the whole aggregate to base64 once one chunk is
// refused. These cases pin the fast path to the bytes the base64 channel
// always produced, read back from disk by the harness (not through the JS
// read path, which would hide a byte-level mistake).
//
// If red: (1) a case that should be base64 (lone/low, latin1, split) failing
// means the text channel was taken — the host then wrote the UTF-8 image of a
// string that JSON could have mangled; (2) a text-channel case (pair, upper)
// failing means the guard over-rejected and the base64 fallback changed the
// bytes; (3) a missing file means the host call itself threw (start with the
// `outcome.errors` assertion below).
// ---------------------------------------------------------------------------

const FS_WRITE_CHANNEL_TS: &str = r#"
import fs from "node:fs";
import { promises as fsp } from "node:fs";
import { join } from "node:path";

export default (pi: any) => {
  pi.on("session_start", async () => {
    const root = process.cwd();
    const out: any = {};

    // Lone surrogates: the JSON text channel would carry the \uD800 escape
    // (which the host parser rejects), so the base64 channel must carry U+FFFD.
    fs.writeFileSync(join(root, "lone-high.txt"), "\uD800");
    fs.writeFileSync(join(root, "lone-low.txt"), "A\uDC00B");

    // Pure text: pair + ASCII (text channel), uppercase utf8 spelling.
    fs.writeFileSync(join(root, "pair.txt"), "ok\uD83D\uDE00");
    fs.writeFileSync(join(root, "upper.txt"), "h\u00E9llo", "UTF-8");

    // Non-utf8 encoding: latin1 semantics, not the UTF-8 image.
    fs.writeFileSync(join(root, "latin1.txt"), "caf\u00E9", "latin1");

    // Empty payload: zero bytes, no channel fields that matter.
    fs.writeFileSync(join(root, "empty.txt"), "");

    // appendFileSync is a second pirWriteData call site: the append payload
    // alone is refused, the earlier text write stays as it was written.
    fs.writeFileSync(join(root, "append.txt"), "ok\uD83D\uDE00");
    fs.appendFileSync(join(root, "append.txt"), "\uDC00");

    // promises.writeFile delegates to writeFileSync (fs_shim.js.txt:1585).
    await fsp.writeFile(join(root, "promise.txt"), "A\uD800B");

    // WriteStream: the text channel commits the aggregate as text; a pair split
    // across write()/end() is refused chunk-by-chunk and lands as 2x U+FFFD.
    await new Promise((resolve: any) => {
      const text = fs.createWriteStream(join(root, "stream-text.txt"));
      text.on("close", resolve);
      text.on("error", resolve);
      text.end("ok\uD83D\uDE00");
    });
    await new Promise((resolve: any) => {
      const split = fs.createWriteStream(join(root, "stream-split.txt"));
      split.on("close", resolve);
      split.on("error", resolve);
      split.write("\uD83D");
      split.end("\uDE00");
    });

    out.done = true;
    globalThis.__fsWriteChannel = out;
  });
};
"#;

#[tokio::test(flavor = "current_thread")]
async fn fs_write_text_channel_is_byte_identical() {
    let dir = write_temp_fixture("fs-write-channel", &[]);
    let rt = new_runtime_at(&dir).await;
    rt.seed_module("pir-ext:fs-write-channel.ts", FS_WRITE_CHANNEL_TS)
        .expect("seed fs write fixture");
    let loaded = rt.seeded_load("pir-ext:fs-write-channel.ts").await;
    assert_eq!(loaded.expect("seeded load"), 1);

    let mut payload = json!({ "reason": "startup" });
    let outcome = rt
        .emit("session_start", &mut payload)
        .await
        .expect("emit session_start");
    assert!(
        outcome.errors.is_empty(),
        "handler errors while writing fixtures: {:?}",
        outcome.errors
    );
    let v: Value = rt
        .eval_json("JSON.stringify(globalThis.__fsWriteChannel)")
        .await
        .expect("read fs write results");
    assert_eq!(v["done"], true, "handler finished writing: {v}");

    // (file, bytes on disk == the base64 `data` channel's image)
    let cases: [(&str, Vec<u8>); 10] = [
        ("lone-high.txt", vec![0xef, 0xbf, 0xbd]),
        ("lone-low.txt", vec![0x41, 0xef, 0xbf, 0xbd, 0x42]),
        ("pair.txt", vec![0x6f, 0x6b, 0xf0, 0x9f, 0x98, 0x80]),
        ("upper.txt", vec![0x68, 0xc3, 0xa9, 0x6c, 0x6c, 0x6f]),
        ("latin1.txt", vec![0x63, 0x61, 0x66, 0xe9]),
        ("empty.txt", vec![]),
        (
            "append.txt",
            vec![0x6f, 0x6b, 0xf0, 0x9f, 0x98, 0x80, 0xef, 0xbf, 0xbd],
        ),
        ("promise.txt", vec![0x41, 0xef, 0xbf, 0xbd, 0x42]),
        ("stream-text.txt", vec![0x6f, 0x6b, 0xf0, 0x9f, 0x98, 0x80]),
        ("stream-split.txt", vec![0xef, 0xbf, 0xbd, 0xef, 0xbf, 0xbd]),
    ];
    for (name, expected) in cases {
        let path = dir.join(name);
        let bytes = std::fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "read {} after the fixture wrote it: {error}",
                path.display()
            )
        });
        assert_eq!(bytes, expected, "bytes written to {name}: {bytes:02x?}");
    }
}
