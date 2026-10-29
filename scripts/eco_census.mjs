// eco_census.mjs — npm-package compatibility census for the pir JS extension
// runtime (QuickJS + node-compat shims). Read-only wrt the repo: writes only
// to %TEMP%/pir-eco-census/ and produces %TEMP%/pir-eco-census/report.md.
//
// Per package:
//   1. npm-install into an isolated temp prefix (each prefix gets a generated
//      package.json — npm otherwise walks up and may install into the user
//      home when a stray package.json exists there).
//   2. run handwritten smoke script(s) (.mjs for ESM, .cjs for CJS) via
//      `pir js`, 10s timeout, PI_EXT_RELOAD=1, PIR_AGENT_DIR redirected so
//      the real ~/.pi is never touched.
//   3. classify exit/stdout/stderr and accumulate a Markdown report.
//
// On top of the per-package smokes, the IDIOMS table runs in-package "reflex"
// pattern probes (duck-typing copies, prototype chains, error shape, iteration
// protocols, module identity, ESM/CJS interop) inside an installed prefix;
// each idiom is gated on a system-node control run first.
//
// Usage:  node scripts/eco_census.mjs [--pir <path>] [--only <id,id>]
//         --only accepts package ids, idiom ids, `idiom:<id>`, or a host id.
// Exit code 0 even when packages fail — the report is the artifact.

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync, appendFileSync, readFileSync } from "node:fs";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = join(dirname(fileURLToPath(import.meta.url)), "..");
const OUT_DIR = join(process.env.TEMP || process.env.TMP || ".", "pir-eco-census");
const PKG_ROOT = join(OUT_DIR, "pkgs");
const AGENT_DIR = join(OUT_DIR, "agent-dir");
const REPORT = join(OUT_DIR, "report.md");
// 30s: debug builds boot slower (TypeScript's 9MB lib especially).
const PIR_TIMEOUT_MS = 30_000;
const NPM_TIMEOUT_MS = 300_000;

function findPir() {
  const flagIndex = process.argv.indexOf("--pir");
  if (flagIndex !== -1 && process.argv[flagIndex + 1]) {
    // Resolve against the invocation cwd: probes spawn the binary from the
    // package directory, so a relative --pir would ENOENT.
    return resolve(process.argv[flagIndex + 1]);
  }
  for (const c of [
    process.env.PIR_EXE,
    // debug first per the working directive: release rebuilds cost ~10 min
    // and the pass/fail verdicts are identical.
    join(REPO, "target-alt/debug/pir.exe"),
    join(REPO, "target-alt/release/pir.exe"),
    join(REPO, "target/release/pir.exe"),
  ].filter(Boolean)) if (existsSync(c)) return c;
  throw new Error("pir binary not found; pass --pir <path>");
}

const onlyArg = (() => {
  const i = process.argv.indexOf("--only");
  return i !== -1 ? new Set(process.argv[i + 1].split(",")) : null;
})();

// ---------------------------------------------------------------------------
// Package table. `esm`/`cjs` are smoke script bodies; each must end by
// printing `PASS`. The native-addon group (better-sqlite3, sqlite3, bcrypt)
// probes each package's real consumer shape against its virtual shim; a
// failure there still classifies as "native" when the .node loader complains.
// ---------------------------------------------------------------------------
const PKGS = [
  // --- pure-JS utilities ---------------------------------------------------
  { id: "ms", spec: "ms", cat: "util", esm: `import ms from "ms"; if (ms("1d") !== 86400000) throw new Error("bad"); console.log("PASS");`, cjs: `const ms = require("ms"); if (ms("1d") !== 86400000) throw new Error("bad"); console.log("PASS");` },
  { id: "semver", spec: "semver", cat: "util", esm: `import { valid, gt } from "semver"; if (valid("1.2.3") !== "1.2.3" || !gt("2.0.0","1.0.0")) throw new Error("bad"); console.log("PASS");`, cjs: `const { valid, gt } = require("semver"); if (valid("1.2.3") !== "1.2.3" || !gt("2.0.0","1.0.0")) throw new Error("bad"); console.log("PASS");` },
  { id: "uuid", spec: "uuid", cat: "util", esm: `import { v4, validate } from "uuid"; if (!validate(v4())) throw new Error("bad"); console.log("PASS");`, cjs: `const { v4, validate } = require("uuid"); if (!validate(v4())) throw new Error("bad"); console.log("PASS");` },
  { id: "nanoid", spec: "nanoid", cat: "util", esm: `import { nanoid } from "nanoid"; if (nanoid().length !== 21) throw new Error("bad"); console.log("PASS");` },
  { id: "chalk5", spec: "chalk@5", cat: "util", esm: `import chalk from "chalk"; if (typeof chalk.red !== "function") throw new Error("bad"); chalk.red("x"); console.log("PASS");` },
  { id: "chalk4", spec: "chalk@4", cat: "util", cjs: `const chalk = require("chalk"); if (typeof chalk.red !== "function") throw new Error("bad"); chalk.red("x"); console.log("PASS");` },
  { id: "picocolors", spec: "picocolors", cat: "util", cjs: `const pc = require("picocolors"); if (typeof pc.red !== "function") throw new Error("bad"); pc.red("x"); console.log("PASS");` },
  { id: "lodash-es", spec: "lodash-es", cat: "util", esm: `import { chunk, camelCase } from "lodash-es"; if (JSON.stringify(chunk([1,2,3],2)) !== "[[1,2],[3]]" || camelCase("foo bar") !== "fooBar") throw new Error("bad"); console.log("PASS");` },
  { id: "lodash", spec: "lodash", cat: "util", cjs: `const _ = require("lodash"); if (JSON.stringify(_.chunk([1,2,3],2)) !== "[[1,2],[3]]") throw new Error("bad"); console.log("PASS");` },
  { id: "dayjs", spec: "dayjs", cat: "util", cjs: `const dayjs = require("dayjs"); if (dayjs("2020-01-02").format("YYYY-MM-DD") !== "2020-01-02") throw new Error("bad"); console.log("PASS");` },
  { id: "debug", spec: "debug", cat: "util", cjs: `const d = require("debug"); const log = d("t"); if (typeof log !== "function") throw new Error("bad"); log("hi"); console.log("PASS");` },
  { id: "clsx", spec: "clsx", cat: "util", cjs: `const clsx = require("clsx"); if (clsx("a", { b: 1, c: 0 }) !== "a b") throw new Error("bad"); console.log("PASS");` },
  { id: "zod", spec: "zod", cat: "util", esm: `import { z } from "zod"; if (z.string().parse("x") !== "x") throw new Error("bad"); console.log("PASS");`, cjs: `const { z } = require("zod"); if (z.string().parse("x") !== "x") throw new Error("bad"); console.log("PASS");` },

  // --- parsing -------------------------------------------------------------
  { id: "yaml", spec: "yaml", cat: "parse", esm: `import { parse } from "yaml"; if (parse("a: 1").a !== 1) throw new Error("bad"); console.log("PASS");` },
  { id: "js-yaml", spec: "js-yaml", cat: "parse", cjs: `const y = require("js-yaml"); if (y.load("a: 1").a !== 1) throw new Error("bad"); console.log("PASS");` },
  { id: "marked", spec: "marked", cat: "parse", esm: `import { marked } from "marked"; if (!marked.parse("# hi").includes("<h1")) throw new Error("bad"); console.log("PASS");` },
  { id: "csv-parse", spec: "csv-parse", cat: "parse", esm: `import { parse } from "csv-parse/sync"; const rows = parse("a,b\\n1,2"); if (rows[0][0] !== "a") throw new Error("bad"); console.log("PASS");` },
  { id: "iconv-lite", spec: "iconv-lite", cat: "parse", cjs: `const iconv = require("iconv-lite"); const b = iconv.encode("é", "utf8"); if (iconv.decode(b, "utf8") !== "é") throw new Error("bad"); console.log("PASS");` },
  { id: "mime-types", spec: "mime-types", cat: "parse", cjs: `const mt = require("mime-types"); if (mt.lookup("a.json") !== "application/json") throw new Error("bad"); console.log("PASS");` },
  { id: "ajv", spec: "ajv", cat: "parse", cjs: `const Ajv = require("ajv"); const v = new Ajv().compile({ type: "object" }); if (!v({})) throw new Error("bad"); console.log("PASS");` },
  { id: "json5", spec: "json5", cat: "parse", cjs: `const JSON5 = require("json5"); if (JSON5.parse("{a:1}").a !== 1) throw new Error("bad"); console.log("PASS");` },
  { id: "fast-xml-parser", spec: "fast-xml-parser", cat: "parse", cjs: `const { XMLParser } = require("fast-xml-parser"); if (new XMLParser().parse("<a>1</a>").a !== 1) throw new Error("bad"); console.log("PASS");` },
  { id: "ejs", spec: "ejs", cat: "parse", cjs: `const ejs = require("ejs"); if (ejs.render("<%= a %>", { a: 1 }) !== "1") throw new Error("bad"); console.log("PASS");` },
  { id: "highlight.js", spec: "highlight.js", cat: "parse", cjs: `const hljs = require("highlight.js"); if (typeof hljs.highlight !== "function") throw new Error("bad"); hljs.highlight("const x = 1", { language: "js" }); console.log("PASS");` },
  { id: "cheerio", spec: "cheerio", cat: "parse", cjs: `const cheerio = require("cheerio"); const $ = cheerio.load("<div>x</div>"); if ($("div").text() !== "x") throw new Error("bad"); console.log("PASS");` },

  // --- fs / path heavy -----------------------------------------------------
  { id: "minimatch", spec: "minimatch", cat: "fs", cjs: `const mm = require("minimatch"); if (!mm.minimatch("a/b/c", "**/*")) throw new Error("bad"); console.log("PASS");` },
  { id: "glob", spec: "glob", cat: "fs", esm: `import { mkdirSync, writeFileSync } from "node:fs"; import { globSync } from "glob"; mkdirSync("d", { recursive: true }); writeFileSync("d/x.txt", "hi"); const r = globSync("d/*.txt"); if (r.length !== 1) throw new Error("got " + r.length); console.log("PASS");` },
  { id: "fs-extra", spec: "fs-extra", cat: "fs", cjs: `const fse = require("fs-extra"); fse.outputFileSync("sub/y.txt", "x"); if (fse.readFileSync("sub/y.txt", "utf8") !== "x") throw new Error("bad"); console.log("PASS");` },
  { id: "rimraf", spec: "rimraf", cat: "fs", esm: `import { writeFileSync } from "node:fs"; import { rimrafSync } from "rimraf"; writeFileSync("gone.txt", "x"); rimrafSync("gone.txt"); console.log("PASS");` },
  { id: "graceful-fs", spec: "graceful-fs", cat: "fs", cjs: `const fs = require("graceful-fs"); if (fs.readFileSync("package.json", "utf8").length < 2) throw new Error("bad"); console.log("PASS");` },
  { id: "fast-glob", spec: "fast-glob", cat: "fs", cjs: `const fg = require("fast-glob"); if (fg.sync(["*.cjs"], { cwd: "." }).length < 1) throw new Error("bad"); console.log("PASS");` },
  { id: "mkdirp", spec: "mkdirp", cat: "fs", cjs: `const m = require("mkdirp"); m.mkdirp("probe-" + Date.now() + "/a/b/c").then((made) => { if (!made) throw new Error("bad"); console.log("PASS"); });` },
  { id: "shelljs", spec: "shelljs", cat: "fs", cjs: `const sh = require("shelljs"); if (sh.echo("hi").toString().indexOf("hi") === -1) throw new Error("bad"); console.log("PASS");` },
  { id: "resolve", spec: "resolve", cat: "fs", cjs: `const resolve = require("resolve"); const p = resolve.sync("./package.json", { basedir: process.cwd() }); if (!p.endsWith("package.json")) throw new Error("bad"); console.log("PASS");` },

  // --- network -------------------------------------------------------------
  { id: "express", spec: "express", cat: "net", cjs: `const express = require("express"); const app = express(); app.get("/", (q, s) => s.send("ok")); const srv = app.listen(0, "127.0.0.1", () => { srv.close(); console.log("PASS"); });` },
  { id: "ws", spec: "ws", cat: "net", cjs: `const { WebSocketServer } = require("ws"); const wss = new WebSocketServer({ port: 0, host: "127.0.0.1" }, () => { wss.close(); console.log("PASS"); });` },
  { id: "axios", spec: "axios", cat: "net", cjs: `const axios = require("axios"); axios.get("http://127.0.0.1:9/", { timeout: 2000 }).catch(() => console.log("PASS"));` },
  { id: "node-fetch3", spec: "node-fetch@3", cat: "net", esm: `import fetch from "node-fetch"; if (typeof fetch !== "function") throw new Error("bad"); fetch("http://127.0.0.1:9/").catch(() => console.log("PASS"));` },
  { id: "undici", spec: "undici", cat: "net", esm: `import { request } from "undici"; if (typeof request !== "function") throw new Error("bad"); console.log("PASS");` },
  { id: "got", spec: "got", cat: "net", esm: `import got from "got"; if (typeof got !== "function") throw new Error("bad"); console.log("PASS");` },
  { id: "http-proxy", spec: "http-proxy", cat: "net", cjs: `const hp = require("http-proxy"); const p = hp.createProxyServer({}); if (typeof p.listen !== "function") throw new Error("bad"); console.log("PASS");` },
  { id: "qs", spec: "qs", cat: "net", cjs: `const qs = require("qs"); if (qs.parse("a=1&b=x").a !== "1") throw new Error("bad"); console.log("PASS");` },
  { id: "cross-spawn", spec: "cross-spawn", cat: "net", cjs: `const cs = require("cross-spawn"); const r = cs.sync("node", ["-v"]); if (r.status !== 0) throw new Error("bad"); console.log("PASS");` },

  // --- crypto --------------------------------------------------------------
  { id: "noble-hashes", spec: "@noble/hashes@1", cat: "crypto", esm: `import { sha256 } from "@noble/hashes/sha256"; const h = sha256(new TextEncoder().encode("abc")); if (h[0] !== 0xba || h.length !== 32) throw new Error("bad"); console.log("PASS");` },
  { id: "bcryptjs", spec: "bcryptjs", cat: "crypto", cjs: `const b = require("bcryptjs"); if (!b.hashSync("pw", 4).startsWith("$2")) throw new Error("bad"); console.log("PASS");` },
  { id: "jose", spec: "jose", cat: "crypto", esm: `import { SignJWT, jwtVerify } from "jose"; if (typeof SignJWT !== "function" || typeof jwtVerify !== "function") throw new Error("bad"); console.log("PASS");` },

  // --- streams / compression ------------------------------------------------
  { id: "pako", spec: "pako", cat: "stream", cjs: `const pako = require("pako"); const d = pako.deflate("hello"); const r = pako.inflate(d, { toText: true }); if (r !== "hello") throw new Error("got " + JSON.stringify(r)); console.log("PASS");` },
  { id: "yazl", spec: "yazl", cat: "stream", cjs: `const yazl = require("yazl"); const zf = new yazl.ZipFile(); zf.addBuffer(Buffer.from("hi"), "a.txt"); zf.end(); if (!zf.outputStream.on) throw new Error("bad"); console.log("PASS");` },
  { id: "tar-stream", spec: "tar-stream", cat: "stream", cjs: `const tar = require("tar-stream"); const pack = tar.pack(); if (typeof pack.entry !== "function" || typeof pack.finalize !== "function") throw new Error("bad"); console.log("PASS");` },
  { id: "pump", spec: "pump", cat: "stream", cjs: `const pump = require("pump"); if (typeof pump !== "function") throw new Error("bad"); console.log("PASS");` },
  { id: "archiver", spec: "archiver", cat: "stream", cjs: `const { ZipArchive } = require("archiver"); const a = new ZipArchive(); if (typeof a.append !== "function" || typeof a.finalize !== "function") throw new Error("bad"); console.log("PASS");` },

  // --- build-artifact style parsers ------------------------------------------
  { id: "acorn", spec: "acorn", cat: "builder", esm: `import * as acorn from "acorn"; const ast = acorn.parse("1+1", { ecmaVersion: 2020 }); if (ast.type !== "Program") throw new Error("bad"); console.log("PASS");` },
  { id: "babel-parser", spec: "@babel/parser", cat: "builder", cjs: `const p = require("@babel/parser"); if (p.parse("1+1").type !== "File") throw new Error("bad"); console.log("PASS");` },
  { id: "typescript", spec: "typescript@5", cat: "builder", cjs: `const ts = require("typescript"); const out = ts.transpileModule("const a: number = 1", { compilerOptions: { target: 7 } }).outputText; if (!out.includes("const a")) throw new Error("got: " + JSON.stringify(out)); console.log("PASS");` },
  { id: "mcp-sdk", spec: "@modelcontextprotocol/sdk", cat: "builder", esm: `import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js"; if (typeof McpServer !== "function") throw new Error("bad"); console.log("PASS");` },

  // --- native-addon group: all three served by virtual shims now -----------
  // The on-disk packages are NAPI addons (.node) that can never load, so each
  // one's real consumer shape runs against the host-backed virtual shim.
  { id: "better-sqlite3", spec: "better-sqlite3", cat: "native", cjs: `const Database = require("better-sqlite3"); const db = new Database(":memory:"); db.exec("create table t (x)"); console.log("PASS");` },
  { id: "sqlite3", spec: "sqlite3", cat: "native", cjs: `const s = require("sqlite3"); const db = new s.Database(":memory:", (err) => { if (err) throw err; db.run("CREATE TABLE t (x)", function (err2) { if (err2) throw err2; db.all("SELECT name FROM sqlite_master", (err3, rows) => { if (err3) throw err3; if (!rows || rows.length !== 1) throw new Error("bad rows"); console.log("PASS"); }); }); });` },
  { id: "bcrypt", spec: "bcrypt", cat: "native", cjs: `const b = require("bcrypt"); const h = b.hashSync("pw", 4); if (!h.startsWith("$2") || h.length !== 60) throw new Error("bad hash"); if (!b.compareSync("pw", h) || b.compareSync("nope", h)) throw new Error("bad compare"); console.log("PASS");` },
];

// ---------------------------------------------------------------------------
// IDIOMS — in-package "reflex" pattern probes the per-package smokes above do
// not reach: duck-typing copies (for..in + hasOwnProperty), prototype-chain and
// instanceof checks, error object shape, TypedArray/Buffer mixing, iteration
// protocols, module identity, ESM/CJS interop. Each entry is `{ id, host, ext,
// cjs|esm }` and runs INSIDE the already-installed census prefix `host`, so it
// exercises that package's real dependency tree rather than a synthetic
// fixture.
//
// Methodology: every idiom is first executed with the system node. A node
// failure classifies the probe itself as `probe-invalid` and removes it from
// the pir denominator — an idiom only measures pir once real Node agrees it is
// a valid program. (probe #1 locks down the iconv-lite/safer-buffer class.)
// ---------------------------------------------------------------------------
const IDIOMS = [
  {
    id: "duck-hasown-statics", host: "iconv-lite", ext: "cjs",
    cjs: `"use strict";
// safer-buffer copies Buffer statics with for..in + hasOwnProperty; an
// incomplete copy silently strips every static method from iconv-lite's
// Buffer alias (the 61/62 regression this probe locks down).
const safer = require("safer-buffer");
const internals = require("buffer").Buffer;
const SBuffer = safer.Buffer;
if (typeof SBuffer !== "object") throw new Error("safer.Buffer typeof " + typeof SBuffer);
const need = ["from", "alloc", "isBuffer", "byteLength", "compare", "concat", "isEncoding"];
const missing = need.filter((k) => typeof SBuffer[k] !== "function");
if (missing.length) throw new Error("statics lost in for..in copy: " + missing.join(","));
if (SBuffer.prototype !== internals.prototype) throw new Error("prototype identity");
if (SBuffer.isBuffer(internals.from("x")) !== true) throw new Error("SBuffer.isBuffer");
const iconv = require("iconv-lite");
if (iconv.decode(iconv.encode("h\\u00e9llo", "utf8"), "utf8") !== "h\\u00e9llo") throw new Error("roundtrip");
console.log("PASS");
`,
  },
  {
    id: "stream-instanceof-chain", host: "yazl", ext: "cjs",
    cjs: `"use strict";
// Streaming npm packages instanceof-check against the shared stream classes;
// a second module instance of stream would break the chain.
const Stream = require("stream");
const yazl = require("yazl");
const zf = new yazl.ZipFile();
const out = zf.outputStream;
let proto = out, hops = 0, reaches = false;
while (proto && hops < 12) {
  if (proto === Stream.Readable.prototype) { reaches = true; break; }
  proto = Object.getPrototypeOf(proto); hops++;
}
if (!reaches) throw new Error("prototype chain never reaches Stream.Readable.prototype");
if (!(out instanceof Stream.Readable) || !(out instanceof Stream.Stream)) throw new Error("instanceof Readable/Stream");
if (typeof out.pipe !== "function" || typeof out.on !== "function") throw new Error("duck surface");
if (!(new Stream.PassThrough() instanceof Stream.Transform)) throw new Error("PassThrough !instanceof Transform");
if (Object.getPrototypeOf(Stream.Readable.prototype) === null) throw new Error("Readable.prototype has no proto");
console.log("PASS");
`,
  },
  {
    id: "ducktype-pipe-pump", host: "pump", ext: "cjs",
    cjs: `"use strict";
// pump() drives pipe/end/on with duck typing; axios uses the same predicate
// (isObject(v) && isFunction(v.pipe)) for stream request bodies.
const pump = require("pump");
const { PassThrough } = require("stream");
const src = new PassThrough();
const mid = new PassThrough();
const dst = new PassThrough();
let got = "";
dst.on("data", (c) => { got += c.toString(); });
if (typeof src.pipe !== "function" || typeof src.on !== "function") throw new Error("duck surface");
if (Object.prototype.toString.call(src) !== "[object Object]") throw new Error("stream toStringTag " + Object.prototype.toString.call(src));
pump(src, mid, dst, (err) => {
  if (err) throw err;
  if (got !== "pumped") throw new Error("body " + JSON.stringify(got));
  console.log("PASS");
});
src.end("pumped");
`,
  },
  {
    id: "error-object-shape", host: "js-yaml", ext: "cjs",
    cjs: `"use strict";
// Error shape contract: instanceof + name/constructor.name + code fields.
// Modern consumers branch on err.code, not on the message text.
const yaml = require("js-yaml");
let y = null;
try { yaml.load("a: [1,\\n  b: 2"); } catch (e) { y = e; }
if (!y) throw new Error("yaml did not throw");
if (!(y instanceof Error)) throw new Error("not instanceof Error");
if (y.name !== "YAMLException" || y.constructor.name !== "YAMLException") throw new Error("name " + y.name + "/" + y.constructor.name);
if (typeof y.message !== "string" || y.message.length === 0) throw new Error("message");
if (typeof y.stack !== "string" || y.stack.indexOf("YAMLException") === -1) throw new Error("stack");
const fs = require("fs");
let enoent = null;
try { fs.readFileSync("definitely-missing-xyz.json"); } catch (e) { enoent = e; }
if (!enoent || enoent.code !== "ENOENT") throw new Error("fs code " + (enoent && enoent.code));
if (typeof enoent.errno !== "number") throw new Error("errno " + enoent.errno);
if (enoent.syscall !== "open") throw new Error("syscall " + enoent.syscall);
if (!(enoent instanceof Error)) throw new Error("fs error class");
let badUrl = null;
try { new URL("not a url"); } catch (e) { badUrl = e; }
if (!(badUrl instanceof TypeError) || badUrl.code !== "ERR_INVALID_URL") throw new Error("url code " + (badUrl && badUrl.code));
console.log("PASS");
`,
  },
  {
    id: "proto-identity-inherits", host: "archiver", ext: "cjs",
    cjs: `"use strict";
// Legacy util.inherits + class-extends-builtin: both must produce identical
// prototype-chain and instanceof semantics (readable-stream/archiver style).
const util = require("util");
const { EventEmitter } = require("events");
const { Transform } = require("stream");
function Base() {}
function Sub() { Base.call(this); }
util.inherits(Sub, Base);
if (Object.getPrototypeOf(Sub.prototype) !== Base.prototype) throw new Error("Sub chain");
if (Sub.super_ !== Base) throw new Error("super_");
if (!(new Sub() instanceof Base)) throw new Error("inherits instanceof");
class CSub extends EventEmitter {}
const c = new CSub();
if (Object.getPrototypeOf(CSub.prototype) !== EventEmitter.prototype) throw new Error("class chain");
if (!(c instanceof EventEmitter) || c.constructor !== CSub) throw new Error("class instanceof/ctor");
class CTransform extends Transform { _transform(chunk, enc, cb) { cb(null, chunk); } }
if (!(new CTransform() instanceof Transform) || Object.getPrototypeOf(CTransform.prototype) !== Transform.prototype) throw new Error("class extends Transform");
// npm reality: a package bundling its own stream base is still duck-typed
// (archiver extends a readable-stream copy, so instanceof node's Transform is
// legitimately false on real node too) — only the chain name may be asserted.
const { ZipArchive } = require("archiver");
const z = new ZipArchive();
if (!(z instanceof ZipArchive) || z.constructor !== ZipArchive) throw new Error("ZipArchive ctor");
if (typeof z.pipe !== "function" || typeof z.write !== "function" || typeof z.on !== "function") throw new Error("ZipArchive duck surface");
let zp = z, sawTransform = false, zHops = 0;
while (zp && zHops++ < 8) { if (zp.constructor && zp.constructor.name === "Transform") { sawTransform = true; break; } zp = Object.getPrototypeOf(zp); }
if (!sawTransform) throw new Error("ZipArchive chain has no Transform");
const b = Buffer.from("ab");
if (Object.getPrototypeOf(b) !== Buffer.prototype) throw new Error("Buffer proto");
if (Object.getPrototypeOf(Buffer.prototype) !== Uint8Array.prototype) throw new Error("Buffer->Uint8Array chain");
if (!(b instanceof Uint8Array) || !(b instanceof Buffer)) throw new Error("Buffer instanceof");
console.log("PASS");
`,
  },
  {
    id: "buffer-typedarray-mix", host: "pako", ext: "cjs",
    cjs: `"use strict";
// Buffer<->TypedArray mixing: pako speaks Uint8Array, npm consumers wrap it in
// Buffer (and vice versa); Buffer.isBuffer must stay false for plain views.
const pako = require("pako");
const bytes = new Uint8Array([104, 105]);
const deflated = pako.deflate(bytes);
if (!(deflated instanceof Uint8Array)) throw new Error("deflate result is not Uint8Array");
const infl = pako.inflate(deflated);
if (!(infl instanceof Uint8Array)) throw new Error("inflate result is not Uint8Array");
const buf = Buffer.from(infl);
if (!Buffer.isBuffer(buf) || buf.toString() !== "hi") throw new Error("Buffer.from(uint8array)");
if (Buffer.isBuffer(infl) !== false) throw new Error("Buffer.isBuffer(plain Uint8Array) must be false");
if (!(buf.buffer instanceof ArrayBuffer)) throw new Error("buf.buffer");
const view = new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
if (view.length !== buf.length || view[0] !== 104) throw new Error("view over Buffer");
if (Buffer.from(new Uint8Array([1, 2])).length !== 2) throw new Error("copy from view");
if (buf.subarray(0, 1).toString() !== "h") throw new Error("subarray");
if (Buffer.from("ab")[1] !== 98) throw new Error("index access");
console.log("PASS");
`,
  },
  {
    id: "iteration-protocols", host: "fast-glob", ext: "cjs",
    cjs: `"use strict";
// Symbol.iterator / asyncIterator coverage on builtins npm code iterates.
const failed = [];
const check = (name, ok) => { if (!ok) failed.push(name); };
const buf = Buffer.from("ab");
check("buffer-spread", [...buf].join(",") === "97,98");
const m = new Map([["a", 1], ["b", 2]]);
check("map-entries", [...m].map(([k, v]) => k + v).join(",") === "a1,b2");
check("map-keys-values", [...m.keys()].join(",") === "a,b" && [...m.values()].join(",") === "1,2");
check("set", [...new Set([1, 1, 2])].join(",") === "1,2");
const usp = new URLSearchParams("a=1&b=2");
check("urlsearchparams", [...usp].map(([k, v]) => k + "=" + v).join("&") === "a=1&b=2");
check("string-codepoints", [..."a\\u{1F600}b"].length === 3);
function* gen() { yield 1; yield 2; }
check("generator", [...gen()].join(",") === "1,2");
check("array-from-ta", Array.from(new Uint8Array([1, 2])).join(",") === "1,2");
check("object-entries", Object.entries({ x: 1, y: 2 }).map(([k, v]) => k + v).join(",") === "x1,y2");
(async () => {
  async function* agen() { yield 1; yield 2; }
  const got = [];
  for await (const v of agen()) got.push(v);
  check("for-await", got.join(",") === "1,2");
  const gen2 = (async function* () { yield "a"; })();
  check("asyncIterator-symbol", typeof gen2[Symbol.asyncIterator] === "function");
  if (failed.length) throw new Error("iteration failures: " + failed.join(","));
  console.log("PASS");
})();
`,
  },
  {
    id: "getter-and-readonly", host: "iconv-lite", ext: "cjs",
    cjs: `"use strict";
// defineProperty getters/setters, for..in over accessor props, delete, and
// frozen/read-only write semantics (strict mode must throw TypeError).
const target = { a: 1 };
let reads = 0, writes = [];
Object.defineProperty(target, "lazy", {
  enumerable: true, configurable: true,
  get() { reads++; return "lazy-value"; },
  set(v) { writes.push(v); },
});
if (target.lazy !== "lazy-value" || reads !== 1) throw new Error("getter");
target.lazy = "next";
if (writes.join(",") !== "next") throw new Error("setter");
if (Object.keys(target).indexOf("lazy") === -1) throw new Error("accessor not enumerable");
const copied = {};
for (const k in target) copied[k] = target[k];
if (copied.lazy !== "lazy-value") throw new Error("for..in copy through getter");
delete target.lazy;
if ("lazy" in target || Object.keys(target).length !== 1) throw new Error("delete accessor");
const frozen = Object.freeze({ k: 1 });
let froze = false;
try { frozen.k = 2; } catch (e) { froze = e instanceof TypeError; }
if (!froze || frozen.k !== 1 || !Object.isFrozen(frozen)) throw new Error("frozen write");
const ro = {};
Object.defineProperty(ro, "x", { value: 1, writable: false, enumerable: true });
let roThrew = false;
try { ro.x = 2; } catch (e) { roThrew = e instanceof TypeError; }
if (!roThrew || ro.x !== 1) throw new Error("read-only write");
const iconv = require("iconv-lite");
const merged = {};
for (const k in iconv) merged[k] = iconv[k];
if (typeof merged.decode !== "function" || typeof merged.encode !== "function") throw new Error("exports lost in for..in copy");
console.log("PASS");
`,
  },
  {
    id: "module-identity", host: "iconv-lite", ext: "cjs",
    cjs: `"use strict";
// One instance per builtin: node: prefix and bare specifier must be the same
// object, and a Buffer produced inside a dependency must be recognized by the
// main module's Buffer (the iconv-lite/safer-buffer failure class).
const bufA = require("buffer");
const bufB = require("node:buffer");
if (bufA !== bufB) throw new Error("node:buffer !== buffer");
if (bufA.Buffer !== globalThis.Buffer) throw new Error("require('buffer').Buffer !== globalThis.Buffer");
if (require("iconv-lite") !== require("iconv-lite")) throw new Error("double require identity");
if (require("events").EventEmitter !== require("node:events").EventEmitter) throw new Error("events class identity");
if (require("util").types !== require("util/types")) throw new Error("util.types !== util/types");
const fsA = require("fs"), fsB = require("node:fs");
if (fsA !== fsB || fsA.readFileSync !== fsB.readFileSync) throw new Error("fs identity");
const made = require("iconv-lite").encode("x", "utf8");
if (!Buffer.isBuffer(made)) throw new Error("dependency Buffer not recognized by global Buffer");
console.log("PASS");
`,
  },
  {
    id: "esm-named-interop", host: "express", ext: "mjs",
    esm: `// ESM consumers import named bindings out of CJS packages (cjs-module-lexer
// shape): authored exports.x = ... must surface as named exports and the
// namespace default must be the same module.exports object.
import iconvDefault from "iconv-lite";
import * as iconvNs from "iconv-lite";
import { decode, encode } from "iconv-lite";
if (typeof decode !== "function" || typeof encode !== "function") throw new Error("named imports");
if (iconvNs.default !== iconvDefault) throw new Error("namespace default identity");
if (iconvNs.decode !== decode || iconvDefault.decode !== decode) throw new Error("named/default/namespace disagreement");
if (decode(encode("h\\u00e9", "utf8"), "utf8") !== "h\\u00e9") throw new Error("roundtrip");
if (decode(encode("h\\u00e9", "utf8"), "utf8").length !== 2) throw new Error("decoded length");
console.log("PASS");
`,
  },
];

// ---------------------------------------------------------------------------
const PIR = findPir();
mkdirSync(PKG_ROOT, { recursive: true });
mkdirSync(AGENT_DIR, { recursive: true });

function log(msg) {
  console.log(msg);
  appendFileSync(join(OUT_DIR, "progress.log"), msg + "\n", "utf8");
}

function npmInstall(dir, spec) {
  writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "census-prefix", private: true }, null, 1), "utf8");
  const npm = process.platform === "win32" ? "npm.cmd" : "npm";
  const r = spawnSync(npm, ["install", spec, "--no-audit", "--no-fund", "--no-package-lock", "--loglevel=error"], {
    cwd: dir, encoding: "utf8", timeout: NPM_TIMEOUT_MS, shell: true,
    stdio: ["ignore", "pipe", "pipe"],
  });
  return { ok: r.status === 0, err: (r.stderr || "").slice(-800), out: (r.stdout || "").slice(-400) };
}

const PIR_ENV = {
  ...process.env,
  PI_EXT_RELOAD: "1",
  PIR_AGENT_DIR: AGENT_DIR,
  FORCE_COLOR: "0",
  NO_COLOR: "1",
};

function classify(res, expect) {
  const { code, stdout, stderr, timedOut } = res;
  const err = stderr || "";
  const s = stdout || "";
  if (timedOut) return { cls: "timeout", why: "pir js exceeded 10s" };
  if (code === 0 && s.includes("PASS")) return { cls: "success" };
  if (code === 0 && !s.includes("PASS")) return { cls: "soft-fail", why: "exit 0 but no PASS marker (swallowed error?)" };
  if (/not supported by the pir extension runtime/i.test(err)) return { cls: "throws-hit" };
  if (/\.node|bindings file|node-pre-gyp|node-gyp|NAPI|napi_|native addon/i.test(err)) return { cls: "native" };
  if (/EACCES|EPERM/i.test(err)) return { cls: "sandbox-root", why: firstLine(err) };
  if (/Could not find|Cannot find|not found|Unable to resolve|ERR_MODULE_NOT_FOUND|ModuleNotFound|no such file/i.test(err)) {
    return { cls: "load-fail", why: firstLine(err) };
  }
  if (/SyntaxError|Unexpected token|ParseError|Invalid/i.test(err)) return { cls: "load-fail", why: firstLine(err) };
  if (/TypeError|ReferenceError|is not a function|undefined is not|has no property/i.test(err)) {
    return { cls: expect === "native" ? "native" : "runtime-fail", why: firstLine(err) };
  }
  return { cls: expect === "native" ? "native" : "other", why: firstLine(err) };
}

function firstLine(s) {
  const line = (s || "").split(/\r?\n/).filter(Boolean).find((l) => !/^\s+at /.test(l));
  return (line || "").slice(0, 220);
}

function runPir(scriptPath) {
  const r = spawnSync(PIR, ["js", scriptPath], {
    encoding: "utf8", timeout: PIR_TIMEOUT_MS, env: PIR_ENV,
    stdio: ["ignore", "pipe", "pipe"], cwd: dirname(scriptPath),
  });
  const timedOut = r.error && r.error.code === "ETIMEDOUT";
  return { code: r.status, stdout: r.stdout || "", stderr: r.stderr || "", timedOut };
}

// ---------------------------------------------------------------------------
writeFileSync(REPORT, "", "utf8");
const results = [];
const list = PKGS.filter((p) => !onlyArg || onlyArg.has(p.id));
log(`eco-census: ${list.length} packages, pir=${PIR}`);

for (const entry of list) {
  const dir = join(PKG_ROOT, entry.id);
  mkdirSync(dir, { recursive: true });
  const formats = [];
  if (entry.esm) formats.push(["mjs", entry.esm]);
  if (entry.cjs) formats.push(["cjs", entry.cjs]);

  // install once per package
  let install = null;
  if (!existsSync(join(dir, "node_modules"))) {
    install = npmInstall(dir, entry.spec);
    if (!install.ok) {
      log(`[install-fail] ${entry.id}: ${firstLine(install.err)}`);
      results.push({ ...entry, probe: "install", cls: "install-fail", why: firstLine(install.err) });
      continue;
    }
  }

  for (const [ext, body] of formats) {
    const scriptPath = join(dir, `smoke.${ext}`);
    writeFileSync(scriptPath, body + "\n", "utf8");
    const res = runPir(scriptPath);
    const c = classify(res, entry.expect);
    results.push({ ...entry, probe: ext, cls: c.cls, why: c.why || "", stderr: (res.stderr || "").slice(-1200), stdout: (res.stdout || "").slice(-300) });
    log(`[${c.cls}] ${entry.id} (${ext})${c.why ? " — " + c.why : ""}`);
  }
}

// ---------------------------------------------------------------------------
// Idiom phase: each probe runs inside its `host` census prefix (installing the
// host if this run selected only idioms). Real-node control first; only a
// probe that real Node accepts is allowed to judge pir.
// ---------------------------------------------------------------------------
const idiomResults = [];
const idiomSelected = (p) =>
  !onlyArg || onlyArg.has(p.id) || onlyArg.has("idiom:" + p.id) || onlyArg.has(p.host);

for (const p of IDIOMS) {
  if (!idiomSelected(p)) continue;
  const dir = join(PKG_ROOT, p.host);
  if (!existsSync(join(dir, "node_modules"))) {
    mkdirSync(dir, { recursive: true });
    const host = PKGS.find((x) => x.id === p.host);
    const inst = npmInstall(dir, host ? host.spec : p.host);
    if (!inst.ok) {
      log(`[install-fail] idiom:${p.id} (host ${p.host}): ${firstLine(inst.err)}`);
      idiomResults.push({ id: p.id, host: p.host, ext: p.ext, cls: "install-fail", control: "n/a", why: firstLine(inst.err) });
      continue;
    }
  }
  const scriptPath = join(dir, `idiom-${p.id}.${p.ext}`);
  writeFileSync(scriptPath, (p.cjs || p.esm) + "\n", "utf8");

  // Real-node control: an idiom that node does not accept measures nothing.
  const nr = spawnSync(process.execPath, [scriptPath], {
    encoding: "utf8", timeout: PIR_TIMEOUT_MS, stdio: ["ignore", "pipe", "pipe"], cwd: dir,
    env: { ...process.env, NO_COLOR: "1" },
  });
  const controlPass = nr.status === 0 && (nr.stdout || "").includes("PASS");
  if (!controlPass) {
    const why = firstLine(nr.stderr) || "node control produced no PASS";
    log(`[probe-invalid] idiom:${p.id} — node ${process.version} rejects it: ${why}`);
    idiomResults.push({ id: p.id, host: p.host, ext: p.ext, cls: "probe-invalid", control: "fail", why, stderr: (nr.stderr || "").slice(-800) });
    continue;
  }

  const res = runPir(scriptPath);
  const c = classify(res, "idiom");
  idiomResults.push({ id: p.id, host: p.host, ext: p.ext, cls: c.cls, control: "pass", why: c.why || "", stderr: (res.stderr || "").slice(-1200), stdout: (res.stdout || "").slice(-300) });
  log(`[${c.cls}] idiom:${p.id} (${p.ext}, host ${p.host})${c.why ? " — " + c.why : ""}`);
}

// ---------------------------------------------------------------------------
// report.md
// ---------------------------------------------------------------------------
const by = (cls) => results.filter((r) => r.cls === cls);
const probes = results.filter((r) => r.probe !== "install");
const ok = by("success");
const native = by("native");
const loadFail = by("load-fail");
const runtimeFail = by("runtime-fail");
const throwsHit = by("throws-hit");
const installFail = by("install-fail");
const other = [...by("timeout"), ...by("soft-fail"), ...by("sandbox-root"), ...by("other")];
const pct = (n, d) => (d ? ((100 * n) / d).toFixed(1) + "%" : "n/a");
const pkgsInstalled = new Set(results.filter((r) => r.cls !== "install-fail").map((r) => r.id)).size;

const lines = [];
lines.push("# pir 生态 npm 包兼容率普查报告");
lines.push("");
lines.push(`- 日期: ${new Date().toISOString().slice(0, 10)} · pir: \`${PIR}\` · node(npm 侧): ${process.version}`);
lines.push(`- 采样: ${PKGS.length} 个包清单 / 实际探测 ${results.length} 条 (含双格式)`);
lines.push(`- 安装成功: ${pkgsInstalled}/${PKGS.length} · 安装失败: ${installFail.length}`);
lines.push(`- 探测总数: ${probes.length} → 加载+运行通过: ${ok.length} (${pct(ok.length, probes.length)})`);
lines.push(`- 原生对照组(预期失败): ${native.length} · 失败分布: 加载失败 ${loadFail.length}, 运行失败 ${runtimeFail.length}, throws档命中 ${throwsHit.length}, 安装失败 ${installFail.length}, 其他 ${other.length}`);
if (idiomResults.length) {
  const idi = idiomResults.filter((r) => r.control === "pass");
  const idiOk = idi.filter((r) => r.cls === "success");
  const invalid = idiomResults.filter((r) => r.cls === "probe-invalid");
  lines.push(`- 惯用法探针: ${idiomResults.length} 条 · 真 node 对照通过 ${idi.length} · pir 通过 ${idiOk.length}/${idi.length} (${pct(idiOk.length, idi.length)})${invalid.length ? ` · 探针无效(真 node 不过) ${invalid.length}` : ""}`);
}
lines.push("");
lines.push("## 逐包结果");
lines.push("");
lines.push("| 包 | 类别 | 格式 | 结果 | 说明 |");
lines.push("|---|---|---|---|---|");
for (const r of results) {
  lines.push(`| ${r.id} | ${r.cat} | ${r.probe} | ${r.cls} | ${(r.why || "").replace(/\|/g, "\\|")} |`);
}
lines.push("");
lines.push("## 惯用法探针（真 node 先验证）");
lines.push("");
if (!idiomResults.length) {
  lines.push("- （本次 --only 未选中任何惯用法探针）");
} else {
  lines.push("| 探针 | 宿主包 | 格式 | 真 node 对照 | pir | 说明 |");
  lines.push("|---|---|---|---|---|---|");
  for (const r of idiomResults) {
    lines.push(`| ${r.id} | ${r.host} | ${r.ext} | ${r.control} | ${r.cls} | ${(r.why || "").replace(/\|/g, "\\|")} |`);
  }
  lines.push("");
  lines.push("探针语义：`success`=pir 通过；`probe-invalid`=系统 node 自己就不过（探针无效，不计入 pir 分母）；其余分类同逐包结果。");
}
lines.push("");
lines.push("## 失败样本 stderr 尾部");
lines.push("");
for (const r of results) {
  if (r.cls === "success") continue;
  lines.push(`### ${r.id} (${r.probe}) — ${r.cls}`);
  lines.push("```");
  lines.push((r.stderr || r.stdout || "(no output)").trim().slice(-1000) || "(empty)");
  lines.push("```");
  lines.push("");
}
writeFileSync(REPORT, lines.join("\n") + "\n", "utf8");

// machine-readable dump for the deep-dive step
writeFileSync(join(OUT_DIR, "results.json"), JSON.stringify(results, null, 1), "utf8");
writeFileSync(join(OUT_DIR, "idioms.json"), JSON.stringify(idiomResults, null, 1), "utf8");
log(`report written: ${REPORT}`);
const idi = idiomResults.filter((r) => r.control === "pass");
console.log(`\nSUMMARY: probes=${probes.length} success=${ok.length} loadFail=${loadFail.length} runtimeFail=${runtimeFail.length} throwsHit=${throwsHit.length} native=${native.length} installFail=${installFail.length} other=${other.length}`);
if (idiomResults.length) {
  console.log(`IDIOMS: probes=${idiomResults.length} node-valid=${idi.length} pirSuccess=${idi.filter((r) => r.cls === "success").length} invalid=${idiomResults.filter((r) => r.cls === "probe-invalid").length}`);
}
