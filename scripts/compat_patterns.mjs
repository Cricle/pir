// compat_patterns.mjs — engine-level "pattern" differential probes: pir vs real
// node, one fixture per JavaScript/npm idiom, every fixture printing
// deterministic `key: value` lines.
//
// Why this exists: the other assets measure (a) whole npm packages
// (eco_census), (b) official node test files (node_tests) and (c) named
// exports (node_surface). None of them isolate the *language-level reflexes*
// npm code relies on — own-vs-inherited property copies, prototype chains,
// accessor semantics, Symbol.toStringTag, iterator protocols, error shape.
// Those reflexes are where the iconv-lite/safer-buffer regression lived.
//
// Method: the same fixture runs under the system node and under `pir js`; both
// stdouts are parsed as `key: value` maps and compared key by key:
//   - node itself fails        -> `node-error` (probe invalid, not counted)
//   - pir fails to run         -> `pir-error`
//   - key/value sets differ    -> `mismatch` (with the per-key diff)
//   - identical                -> `match`
// Only `match`/`mismatch`/`pir-error` enter the score; patterns whose real
// behavior pir deliberately does not chase (Intl, V8 internals) carry
// `skip_reason` and are reported as `skip`.
//
// Usage:
//   node scripts/compat_patterns.mjs [--pir <path>] [--only id,id]
//        [--out <report.md>] [--verbose]
// Exit code 0 always — the report is the artifact (same contract as
// eco_census.mjs). Fixtures are generated into %TEMP%/pir-compat-patterns/.

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = join(dirname(fileURLToPath(import.meta.url)), "..");
const OUT_DIR = join(process.env.TEMP || process.env.TMP || ".", "pir-compat-patterns");
const WORK = join(OUT_DIR, "work");
const REPORT = join(OUT_DIR, "report.md");
const TIMEOUT_MS = 20_000;

function findPir() {
  const i = process.argv.indexOf("--pir");
  if (i !== -1 && process.argv[i + 1]) return resolve(process.argv[i + 1]);
  for (const c of [
    process.env.PIR_EXE,
    // debug first per the working directive: release rebuilds cost ~10 min
    // and the pass/fail verdicts are identical (same order as eco_census.mjs).
    join(REPO, "target-alt/debug/pir.exe"),
    join(REPO, "target-alt/release/pir.exe"),
  ].filter(Boolean)) {
    if (existsSync(c)) return c;
  }
  throw new Error("pir binary not found; pass --pir <path>");
}

const onlyArg = (() => {
  const i = process.argv.indexOf("--only");
  return i !== -1 ? new Set(process.argv[i + 1].split(",").map((s) => s.trim())) : null;
})();
const outArg = (() => {
  const i = process.argv.indexOf("--out");
  return i !== -1 ? process.argv[i + 1] : REPORT;
})();
const verbose = process.argv.includes("--verbose");

// ---------------------------------------------------------------------------
// Pattern fixtures. Every fixture must be deterministic and end in
// `console.log("PASS")` so a silent partial run is still detectable.
// ---------------------------------------------------------------------------
const PATTERNS = [
  {
    id: "own-vs-inherited",
    cat: "reflection",
    why: "safer-buffer/iconv-lite: for..in + hasOwnProperty copies statics; statics living only on the prototype chain are silently dropped",
    js: `function Base() {}
Base.staticOwn = function () { return "own"; };
class C extends Base {}
C.staticClass = function () { return "class"; };
const copied = {};
for (const k in C) if (Object.prototype.hasOwnProperty.call(C, k)) copied[k] = C[k];
console.log("copied-keys: " + Object.keys(copied).sort().join(","));
console.log("own-static: " + (Object.prototype.hasOwnProperty.call(C, "staticClass")));
console.log("inherited-static-in-chain: " + (typeof C.staticOwn === "function"));
console.log("inherited-not-own: " + Object.prototype.hasOwnProperty.call(C, "staticOwn"));
// builtins: npm code copies Buffer statics this way
const bufCopy = {};
for (const k in Buffer) if (Object.prototype.hasOwnProperty.call(Buffer, k)) bufCopy[k] = Buffer[k];
console.log("buffer-copy-isBuffer: " + (typeof bufCopy.isBuffer));
console.log("buffer-copy-from: " + (typeof bufCopy.from));
console.log("buffer-copy-concat: " + (typeof bufCopy.concat));
console.log("buffer-own-isBuffer: " + Object.prototype.hasOwnProperty.call(Buffer, "isBuffer"));
console.log("PASS");`,
  },
  {
    id: "prototype-chain",
    cat: "reflection",
    why: "readable-stream/util.inherits: prototype identity + super_ + constructor after inherits",
    js: `const util = require("util");
function Base() {}
function Sub() { Base.call(this); }
util.inherits(Sub, Base);
class CSub extends Error {}
console.log("inherits-chain: " + (Object.getPrototypeOf(Sub.prototype) === Base.prototype));
console.log("inherits-instanceof: " + (new Sub() instanceof Base));
console.log("inherits-super_: " + (Sub.super_ === Base));
console.log("inherits-ctor: " + (Sub.prototype.constructor === Sub));
console.log("class-chain: " + (Object.getPrototypeOf(CSub.prototype) === Error.prototype));
console.log("class-instanceof: " + (new CSub("m") instanceof Error));
console.log("class-name: " + new CSub("m").name);
console.log("getproto-null: " + (Object.getPrototypeOf(Object.create(null)) === null));
console.log("setproto: " + (() => { const o = {}; Object.setPrototypeOf(o, { tag: 7 }); return o.tag; })());
console.log("PASS");`,
  },
  {
    id: "hasinstance",
    cat: "reflection",
    why: "node stream classes define own Symbol.hasInstance on Writable; custom hasInstance classes are an npm subclassing idiom",
    js: `class Duck { static [Symbol.hasInstance](v) { return !!(v && typeof v.quack === "function"); } }
console.log("custom-hasinstance: " + ({ quack() {} } instanceof Duck));
console.log("custom-hasinstance-bad: " + ({} instanceof Duck));
console.log("writable-own-hasinstance: " + (Object.getOwnPropertySymbols(require("stream").Writable).indexOf(Symbol.hasInstance) !== -1));
console.log("writable-instanceof-duplex: " + (new (require("stream").Duplex)() instanceof require("stream").Writable));
console.log("readable-own-hasinstance: " + (Object.getOwnPropertySymbols(require("stream").Readable).indexOf(Symbol.hasInstance) !== -1));
console.log("PASS");`,
  },
  {
    id: "stream-proto-chain",
    cat: "stream",
    why: "yazl/archiver/ws: instanceof Transform/Readable over package-produced streams",
    js: `const S = require("stream");
const pt = new S.PassThrough();
const chainHas = (x, k) => { let p = Object.getPrototypeOf(x), n = 0; while (p && n++ < 8) { if (p === S[k].prototype) return true; p = Object.getPrototypeOf(p); } return false; };
console.log("pt-instanceof-passthrough: " + (pt instanceof S.PassThrough));
console.log("pt-instanceof-transform: " + (pt instanceof S.Transform));
console.log("pt-instanceof-duplex: " + (pt instanceof S.Duplex));
console.log("pt-instanceof-readable: " + (pt instanceof S.Readable));
console.log("pt-instanceof-writable: " + (pt instanceof S.Writable));
console.log("pt-instanceof-stream: " + (pt instanceof S.Stream));
console.log("pt-chain-has-transform: " + chainHas(pt, "Transform"));
console.log("transform-instanceof-duplex: " + (new S.Transform() instanceof S.Duplex));
console.log("PASS");`,
  },
  {
    id: "accessors",
    cat: "reflection",
    why: "lazy/derived exports are defineProperty getters (iconv-lite encodings, debug names) — for..in must list and read them",
    js: `const o = { plain: 1 };
let reads = 0, writes = [];
Object.defineProperty(o, "lazy", {
  enumerable: true, configurable: true,
  get() { reads++; return "v" + reads; },
  set(v) { writes.push(v); },
});
console.log("getter: " + o.lazy);
o.lazy = "x";
console.log("setter: " + writes.join(","));
console.log("enumerable: " + (Object.keys(o).indexOf("lazy") !== -1));
const copy = {};
for (const k in o) copy[k] = o[k];
console.log("copy-lazy: " + copy.lazy);
console.log("copy-plain: " + copy.plain);
console.log("delete: " + (delete o.lazy) + "," + ("lazy" in o) + "," + Object.keys(o).join(","));
// accessor on a prototype (class getter) is not own
class WithGetter { get computed() { return 42; } }
const wg = new WithGetter();
console.log("proto-getter: " + wg.computed);
console.log("proto-getter-own: " + Object.prototype.hasOwnProperty.call(wg, "computed"));
console.log("PASS");`,
  },
  {
    id: "freeze-readonly",
    cat: "semantics",
    why: "frozen config objects and read-only exports: strict-mode writes must throw TypeError (test-zlib-const shape)",
    js: `"use strict";
const frozen = Object.freeze({ a: 1, o: { b: 2 } });
let threw = false;
try { frozen.a = 2; } catch (e) { threw = e instanceof TypeError; }
console.log("frozen-write-throws: " + threw);
frozen.o.b = 3; // shallow: nested write is allowed
console.log("frozen-shallow: " + frozen.o.b + "," + Object.isFrozen(frozen) + "," + Object.isFrozen(frozen.o));
const sealed = Object.seal({ x: 1 });
let sealedThrew = false;
try { sealed.x = 2; } catch { sealedThrew = true; }
console.log("sealed-write-ok: " + (!sealedThrew && sealed.x === 2));
console.log("sealed-add-throws: " + (() => { try { sealed.y = 1; return false; } catch { return true; } })());
const ro = {};
Object.defineProperty(ro, "v", { value: 1, writable: false, enumerable: true, configurable: false });
let roThrew = false;
try { ro.v = 9; } catch (e) { roThrew = e instanceof TypeError; }
console.log("readonly-throws: " + roThrew + "," + ro.v);
console.log("prevent-extensions: " + (() => { const p = {}; Object.preventExtensions(p); let t = false; try { p.z = 1; } catch { t = true; } return t + "," + Object.isExtensible(p); })());
console.log("PASS");`,
  },
  {
    id: "tostringtag",
    cat: "reflection",
    why: "is-plain-object / qs / iconv-lite sniff types via Object.prototype.toString",
    js: `const cases = [
  ["plain", {}], ["array", []], ["buffer", Buffer.from("x")], ["u8", new Uint8Array(1)],
  ["map", new Map()], ["set", new Set()], ["date", new Date(0)], ["regexp", /x/],
  ["null", null], ["undef", undefined], ["fn", function () {}], ["urlp", new URLSearchParams()],
  ["promise", Promise.resolve()], ["error", new Error("e")],
];
for (const [name, v] of cases) console.log("tag-" + name + ": " + Object.prototype.toString.call(v));
console.log("buffer-tag-direct: " + Buffer.from("x")[Symbol.toStringTag]);
console.log("PASS");`,
  },
  {
    id: "iteration-protocols",
    cat: "iteration",
    why: "for..of / spread over builtins npm iterates; a URLSearchParams iterator that never advances hangs the process",
    js: `const out = [];
const buf = Buffer.from("ab");
out.push("buffer:" + [...buf].join(","));
out.push("map:" + [...new Map([["a", 1], ["b", 2]])].map(([k, v]) => k + v).join(","));
out.push("set:" + [...new Set([1, 1, 2])].join(","));
out.push("string:" + [..."a\\u{1F600}b"].length);
out.push("uint8:" + [...new Uint8Array([1, 2])].join(","));
const usp = new URLSearchParams("a=1&b=2");
const it = usp[Symbol.iterator]();
const pairs = [];
for (let i = 0; i < 5; i++) { const r = it.next(); pairs.push(r.done ? "done" : r.value.join("=")); if (r.done) break; }
out.push("usp-next:" + pairs.join("|"));
out.push("usp-spread:" + [...usp].map(([k, v]) => k + "=" + v).join("&"));
out.push("usp-arrayfrom:" + JSON.stringify(Array.from(usp)));
for (const line of out) console.log(line.replace(/^([a-z0-9-]+):/, "$1: "));
console.log("PASS");`,
  },
  {
    id: "async-iteration",
    cat: "iteration",
    why: "async generators + for await drive modern stream/pipeline consumer code",
    js: `async function main() {
  async function* agen() { yield 1; yield 2; yield 3; }
  const got = [];
  for await (const v of agen()) got.push(v);
  console.log("for-await: " + got.join(","));
  const obj = { async *[Symbol.asyncIterator]() { yield "a"; yield "b"; } };
  const got2 = [];
  for await (const v of obj) got2.push(v);
  console.log("async-iterator-symbol: " + got2.join(","));
  const it = agen();
  console.log("iterator-next: " + JSON.stringify(await it.next()));
  console.log("PASS");
}
main().catch((e) => { console.log("error: " + e.message); process.exit(1); });`,
  },
  {
    id: "error-shape",
    cat: "errors",
    why: "err.code branching + stack first line: nearly every npm error path and every official node assert reads these",
    js: `const shapes = [];
try { null.x; } catch (e) { shapes.push(["typeerror", e.name, e instanceof TypeError, e instanceof Error, e.constructor.name]); }
try { new Array(-1); } catch (e) { shapes.push(["rangeerror", e.name, e instanceof RangeError, e instanceof Error, e.constructor.name]); }
try { JSON.parse("{"); } catch (e) { shapes.push(["syntaxerror", e.name, e instanceof SyntaxError, e.constructor.name]); }
for (const [tag, name, inst, base, ctor] of shapes) console.log(tag + ": " + name + "," + inst + "," + base + "," + ctor);
const e = new Error("boom");
const firstLine = String(e.stack).split(/\\r?\\n/)[0];
console.log("stack-first-line: " + firstLine);
console.log("stack-starts-with-name: " + firstLine.startsWith("Error: boom"));
console.log("message-prop: " + e.message);
console.log("cause: " + (new Error("outer", { cause: 42 }).cause === 42));
console.log("aggregate: " + (() => { const a = new AggregateError([new Error("i")], "agg"); return (a instanceof Error) + "," + a.errors.length + "," + a.name; })());
console.log("code-prop: " + (() => { const t = new TypeError("m"); t.code = "ERR_X"; return t.code + "," + (Object.keys(t).indexOf("code") !== -1); })());
console.log("PASS");`,
  },
  {
    id: "buffer-typedarray",
    cat: "buffer",
    why: "Buffer is a Uint8Array subclass sharing the global constructor; two copies break isBuffer across dependencies",
    js: `const fromBuffer = require("buffer");
console.log("global-identity: " + (fromBuffer.Buffer === globalThis.Buffer));
console.log("node-prefix-identity: " + (require("node:buffer") === fromBuffer));
console.log("proto-chain: " + (Object.getPrototypeOf(Buffer.prototype) === Uint8Array.prototype));
console.log("instanceof: " + (Buffer.from("ab") instanceof Uint8Array) + "," + (Buffer.from("ab") instanceof Buffer));
console.log("isBuffer-u8: " + Buffer.isBuffer(new Uint8Array(1)));
console.log("from-view: " + Buffer.from(new Uint8Array([104, 105])).toString());
const b = Buffer.from("ab");
console.log("view-roundtrip: " + new Uint8Array(b.buffer, b.byteOffset, b.byteLength).length);
console.log("subarray-is-buffer: " + Buffer.isBuffer(b.subarray(0, 1)));
console.log("slice-is-buffer: " + Buffer.isBuffer(b.slice(0, 1)));
console.log("static-own: " + ["from", "alloc", "isBuffer", "concat", "byteLength", "compare"].map((k) => Object.prototype.hasOwnProperty.call(Buffer, k)).join(","));
console.log("PASS");`,
  },
  {
    id: "module-identity",
    cat: "modules",
    why: "one instance per builtin: a second copy makes cross-module instanceof/isBuffer fail",
    js: `console.log("buffer-node-prefix: " + (require("buffer") === require("node:buffer")));
console.log("fs-node-prefix: " + (require("fs") === require("node:fs")));
console.log("events-node-prefix: " + (require("events").EventEmitter === require("node:events").EventEmitter));
console.log("util-types-identity: " + (require("util").types === require("util/types")));
console.log("util-types-node-prefix: " + (require("util/types") === require("node:util/types")));
console.log("double-require: " + (require("util") === require("util")));
console.log("assert-strict-differ: " + (require("assert") !== require("assert/strict")));
console.log("PASS");`,
  },
  {
    id: "json-edges",
    cat: "semantics",
    why: "config/serialization code: reviver, toJSON, bigint/circular errors, key order",
    js: `console.log("reviver: " + JSON.stringify(JSON.parse('{"a":1,"b":2}', (k, v) => (k === "a" ? undefined : v))));
console.log("tojson: " + JSON.stringify({ toJSON() { return "custom"; } }));
console.log("undefined-dropped: " + JSON.stringify({ a: undefined, b: 1 }));
console.log("key-order: " + Object.keys(JSON.parse('{"b":1,"2":2,"a":3}')).join(","));
console.log("bigint-throws: " + (() => { try { JSON.stringify({ n: 1n }); return false; } catch (e) { return e instanceof TypeError; } })());
console.log("circular-throws: " + (() => { const c = {}; c.c = c; try { JSON.stringify(c); return false; } catch (e) { return e instanceof TypeError; } })());
console.log("stringify-space: " + JSON.stringify({ a: 1 }, null, 1).replace(/\\n/g, "|"));
console.log("PASS");`,
  },
  {
    id: "regexp-features",
    cat: "semantics",
    why: "modern parsers (yaml, fast-xml-parser, ajv) use named groups/lookbehind/unicode property escapes",
    js: `const named = /(?<y>\\d{4})-(?<m>\\d{2})/.exec("2026-10");
console.log("named-groups: " + (named.groups.y + "," + named.groups.m));
console.log("lookbehind: " + /(?<=\\$)\\d+/.exec("$42")[0]);
console.log("lookahead: " + /\\d+(?=px)/.exec("10px")[0]);
console.log("up-escape: " + /\\p{Lu}/u.test("aB"));
console.log("unicode-sets: " + (() => { try { return new RegExp("[\\\\p{Lu}&&\\\\p{Script=Latin}]", "v").test("A") + "," + new RegExp("[\\\\p{Lu}--[A]]", "v").test("B"); } catch { return "unsupported"; } })());
console.log("sticky: " + (() => { const r = /a/y; r.lastIndex = 1; return r.test("ba") + "," + r.lastIndex; })());
console.log("flags-order: " + /x/gimsy.flags);
console.log("named-replace: " + "2026-10".replace(/(?<y>\\d{4})/, "[$<y>]"));
console.log("matchall: " + [..."a1b2".matchAll(/([a-z])(\\d)/g)].map((m) => m[2]).join(","));
console.log("split-limit: " + "a,b,c".split(",", 2).join("|"));
console.log("PASS");`,
  },
  {
    id: "map-set-semantics",
    cat: "semantics",
    why: "caches/dedup maps: SameValueZero, insertion order, forEach arg order",
    js: `const m = new Map();
m.set(NaN, "nan").set(-0, "zero").set("k", 1);
console.log("svz: " + m.get(NaN) + "," + m.get(0) + "," + m.size);
m.set("k", 2);
console.log("overwrite-order: " + [...m.keys()].map(String).join("|"));
const s = new Set([3, 1, 3, NaN, NaN]);
console.log("set-order: " + [...s].map(String).join(","));
const args = [];
new Map([["a", 1]]).forEach((v, k, map) => args.push(k + "=" + v + ":" + (map instanceof Map)));
console.log("foreach: " + args.join(","));
console.log("delete-has: " + m.delete("k") + "," + m.has("k") + "," + m.size);
const weakOk = typeof WeakMap === "function" && typeof WeakSet === "function";
console.log("weak: " + weakOk + "," + typeof WeakRef);
console.log("PASS");`,
  },
  {
    id: "proxy-reflect",
    cat: "reflection",
    why: "schema/framework code (zod, ajv wrappers) uses Proxy traps and Reflect forwarding",
    js: `const log = [];
const p = new Proxy({ a: 1 }, {
  get(t, k, r) { log.push("get:" + String(k)); return Reflect.get(t, k, r); },
  set(t, k, v) { log.push("set:" + String(k)); return Reflect.set(t, k, v); },
  has(t, k) { log.push("has:" + String(k)); return Reflect.has(t, k); },
  ownKeys(t) { log.push("ownKeys"); return Reflect.ownKeys(t); },
});
console.log("get: " + p.a);
p.b = 2;
console.log("has: " + ("a" in p) + "," + Object.keys(p).join(","));
console.log("traps: " + log.join(","));
const revocable = Proxy.revocable({}, {});
revocable.revoke();
console.log("revoked-throws: " + (() => { try { revocable.proxy.x; return false; } catch (e) { return e instanceof TypeError; } })());
console.log("reflect-apply: " + Reflect.apply(Math.max, null, [1, 5, 3]));
console.log("reflect-construct: " + (Reflect.construct(Error, ["m"]) instanceof Error));
console.log("PASS");`,
  },
  {
    id: "array-string-modern",
    cat: "semantics",
    why: "utility code: at/flat/findLast/replaceAll/padStart/trimStart and friends",
    js: `const a = [1, [2, [3, [4]]]];
console.log("flat: " + a.flat(2).join(",") + "|" + a.flat(Infinity).join(","));
console.log("flatmap: " + [1, 2].flatMap((x) => [x, x * 10]).join(","));
console.log("at: " + [1, 2, 3].at(-1) + "," + "abc".at(-1));
console.log("findlast: " + [1, 2, 3, 4].findLast((x) => x % 2 === 0) + "," + [1, 2, 3, 4].findLastIndex((x) => x % 2 === 0));
console.log("replaceall: " + "a-b-c".replaceAll("-", "+"));
console.log("pad: " + "5".padStart(3, "0") + "," + "5".padEnd(3, "0"));
console.log("trim: " + JSON.stringify("  x  ".trimStart()) + "," + JSON.stringify("  x  ".trimEnd()));
console.log("includes-neg: " + "abc".includes("b", -2) + "," + "abc".indexOf("b", -2));
console.log("sort-stable: " + [{ k: 1, v: "a" }, { k: 0, v: "b" }, { k: 1, v: "c" }].sort((x, y) => x.k - y.k).map((x) => x.v).join(""));
console.log("structured-clone: " + (typeof structuredClone === "function" ? structuredClone({ a: [1, new Map([["k", "v"]])] }).a[1].get("k") : "missing"));
console.log("PASS");`,
  },
  {
    id: "timers-microtasks",
    cat: "async",
    why: "ordering guarantees: nextTick < promise < setImmediate/setTimeout — many npm libs assume it",
    js: `const order = [];
const done = () => {
  console.log("order: " + order.join(","));
  console.log("PASS");
  process.exit(0);
};
setTimeout(() => { order.push("timeout"); done(); }, 10);
setImmediate(() => order.push("immediate"));
queueMicrotask(() => order.push("microtask"));
Promise.resolve().then(() => order.push("promise"));
if (typeof process.nextTick === "function") process.nextTick(() => order.push("nextTick"));`,
  },
  {
    id: "symbols-and-keys",
    cat: "reflection",
    why: "key enumeration order + symbol-keyed metadata (Symbol.for registries)",
    js: `const s1 = Symbol.for("pir.test");
const s2 = Symbol.for("pir.test");
console.log("symbol-registry: " + (s1 === s2) + "," + Symbol.keyFor(s1));
const o = { b: 1, 2: "two", a: 2, [s1]: 3, 1: "one" };
console.log("keys: " + Object.keys(o).join(","));
console.log("entries: " + JSON.stringify(Object.entries(o)));
console.log("symbols: " + Object.getOwnPropertySymbols(o).length);
console.log("json-skips-symbols: " + JSON.stringify(o));
console.log("description: " + Symbol("d").description);
console.log("well-known: " + (typeof Symbol.iterator === "symbol") + "," + (Symbol.asyncIterator !== undefined));
console.log("PASS");`,
  },
  {
    id: "number-string-format",
    cat: "semantics",
    why: "toFixed/toPrecision/toString(radix)/JSON number round-trips in config and crypto code",
    js: `console.log("tofixed: " + (1.005).toFixed(2) + "," + (2.5).toFixed(0));
console.log("precision: " + (123.456).toPrecision(4));
console.log("radix: " + (255).toString(16) + "," + (255).toString(2) + "," + (255).toString(36));
console.log("parse: " + parseInt("08", 10) + "," + Number("1e3") + "," + Number.isInteger(1e21));
console.log("seq: " + [0.1 + 0.2, 1 / 3, 1e21, -0, Number.MAX_SAFE_INTEGER].map(String).join("|"));
console.log("toString-round: " + JSON.stringify(JSON.parse(JSON.stringify({ a: 0.1, b: 1e21, c: 1e-7 }))));
console.log("bigint: " + (typeof BigInt === "function" ? (123456789012345678901234567890n * 2n).toString() : "missing"));
console.log("locale-independent: " + (1.5).toString() + "," + (1234.5678).toFixed(1));
console.log("PASS");`,
  },
  {
    id: "class-semantics",
    cat: "semantics",
    why: "no-new constructor calls must throw the engine TypeError; private fields/static blocks appear in modern npm code",
    js: `class NeedsNew { constructor() { this.ok = true; } }
let threw = false, msg = "";
try { NeedsNew(); } catch (e) { threw = e instanceof TypeError; msg = e.message; }
console.log("no-new-throws: " + threw);
console.log("no-new-message-has-class: " + /class constructor|without 'new'/i.test(msg));
class Priv { #v = 1; get v() { return this.#v; } static { Priv.ready = true; } static field = 2; }
const p = new Priv();
console.log("private-field: " + p.v);
console.log("static-block: " + Priv.ready + "," + Priv.field);
class Base2 { constructor() { this.kind = "base"; } }
class Derived extends Base2 { constructor() { super(); this.kind += "+derived"; } }
console.log("super: " + new Derived().kind);
console.log("new-target: " + (() => { function F() { return new.target === F ? "direct" : "other"; } return new F() + "," + F(); })());
console.log("PASS");`,
  },
  {
    id: "getter-throw-and-inspect",
    cat: "errors",
    why: "util.inspect of objects with throwing getters is a documented npm-debugging path (test-util-inspect-getters-accessing-this)",
    js: `const util = require("util");
const o = { get boom() { throw new Error("getter-exploded"); }, safe: 1 };
let inspectOk = false, text = "";
try { text = util.inspect(o); inspectOk = true; } catch { inspectOk = false; }
console.log("inspect-survives-getter: " + inspectOk);
console.log("inspect-format: " + util.inspect({ a: 1, b: [2, 3] }, { breakLength: Infinity }).replace(/\\s+/g, " "));
console.log("format-pct: " + util.format("%s|%d|%j|%%", "x", 42, { k: 1 }));
console.log("format-extra: " + util.format("no-placeholder", "extra", 1));
console.log("types: " + [util.types.isDate(new Date()), util.types.isRegExp(/x/), util.types.isUint8Array(Buffer.from("x")), util.types.isPromise(Promise.resolve())].join(","));
console.log("PASS");`,
  },
  {
    id: "global-web-primitives",
    cat: "web",
    why: "Web primitives the extension runtime exposes globally (atob/URL/Blob/fetch) must agree on shape with node",
    js: `const out = [];
out.push("atob: " + atob(btoa("pir")));
out.push("url: " + new URL("/p?q=1", "https://x.test/a/b").href);
out.push("urlsearchparams: " + new URLSearchParams({ a: "1", b: "2" }).toString());
out.push("textencoder: " + new TextEncoder().encode("é").length);
out.push("textdecoder: " + new TextDecoder().decode(new Uint8Array([104, 105])));
out.push("blob: " + (typeof Blob === "function") + "," + (typeof File === "function"));
out.push("fetch: " + typeof fetch);
out.push("structuredclone: " + structuredClone({ m: new Map([["k", 1]]) }).m.get("k"));
out.push("abort: " + typeof AbortController + "," + typeof AbortSignal);
for (const line of out) console.log(line.replace(/^([a-z]+):/, "$1: "));
console.log("PASS");`,
  },
  {
    id: "globals-integrity",
    cat: "reflection",
    why: "bootstraps and polyfill loaders assign globals and rely on writability/descriptors",
    js: `const desc = Object.getOwnPropertyDescriptor(globalThis, "process");
console.log("process-desc: " + (!!desc) + "," + (desc ? desc.writable + "," + desc.enumerable + "," + desc.configurable : "n/a"));
const w = Object.getOwnPropertyDescriptor(globalThis, "undefined");
console.log("undefined-desc: " + (w ? w.writable + "," + w.enumerable + "," + w.configurable : "n/a"));
const before = globalThis.__pirCompatTemp;
globalThis.__pirCompatTemp = 1;
console.log("global-write: " + globalThis.__pirCompatTemp);
delete globalThis.__pirCompatTemp;
console.log("global-delete: " + (globalThis.__pirCompatTemp === undefined) + ",was=" + before);
console.log("global-object: " + (Object.getPrototypeOf(globalThis) === Object.prototype));
console.log("typeof-missing: " + typeof globalThis.__noSuchGlobal__);
console.log("PASS");`,
  },
];

// ---------------------------------------------------------------------------
function parseLines(stdout) {
  const map = new Map();
  for (const line of (stdout || "").split(/\r?\n/)) {
    const i = line.indexOf(": ");
    if (i > 0) map.set(line.slice(0, i).trim(), line.slice(i + 2));
  }
  return map;
}

const PIR = findPir();
mkdirSync(WORK, { recursive: true });
const list = PATTERNS.filter((p) => !onlyArg || onlyArg.has(p.id));
const results = [];

for (const pattern of list) {
  const file = join(WORK, pattern.id + ".cjs");
  writeFileSync(file, pattern.js + "\n", "utf8");
  const env = { ...process.env, NO_COLOR: "1", PI_EXT_RELOAD: "1", PIR_AGENT_DIR: join(OUT_DIR, "agent-dir") };
  const nodeRun = spawnSync(process.execPath, [file], { encoding: "utf8", timeout: TIMEOUT_MS, cwd: WORK, env: { ...process.env, NO_COLOR: "1" } });
  const pirRun = spawnSync(PIR, ["js", file], { encoding: "utf8", timeout: TIMEOUT_MS, cwd: WORK, env, stdio: ["ignore", "pipe", "pipe"] });
  const nodeTimedOut = nodeRun.error && nodeRun.error.code === "ETIMEDOUT";
  const pirTimedOut = pirRun.error && pirRun.error.code === "ETIMEDOUT";

  let verdict, detail = "";
  const nodeMap = parseLines(nodeRun.stdout);
  const pirMap = parseLines(pirRun.stdout);
  if (nodeTimedOut || nodeRun.status !== 0 || !(nodeRun.stdout || "").includes("PASS")) {
    verdict = "node-error";
    detail = "node control failed: " + (nodeTimedOut ? "timeout" : "exit " + nodeRun.status);
  } else if (pirTimedOut) {
    verdict = "pir-error";
    detail = "pir js exceeded " + TIMEOUT_MS / 1000 + "s";
  } else if (pirRun.status !== 0 || !(pirRun.stdout || "").includes("PASS")) {
    verdict = "pir-error";
    detail = "exit " + pirRun.status + " :: " + ((pirRun.stderr || "").trim().split(/\r?\n/).slice(0, 2).join(" | ") || "(no stderr)").slice(0, 220);
  } else {
    const diffs = [];
    for (const [k, v] of nodeMap) {
      if (!pirMap.has(k)) diffs.push(`  ${k}: node=${v} pir=<missing>`);
      else if (pirMap.get(k) !== v) diffs.push(`  ${k}: node=${v} pir=${pirMap.get(k)}`);
    }
    for (const [k, v] of pirMap) if (!nodeMap.has(k)) diffs.push(`  ${k}: node=<missing> pir=${v}`);
    if (diffs.length) { verdict = "mismatch"; detail = diffs.join("\n"); }
    else verdict = "match";
  }
  results.push({ id: pattern.id, cat: pattern.cat, why: pattern.why, verdict, detail });
  const tag = { match: "MATCH", mismatch: "DIFF ", "pir-error": "ERR  ", "node-error": "NODE!" }[verdict];
  console.log(`[${tag}] ${pattern.id}${verdict === "match" ? "" : " — " + (detail.split("\n")[0] || "")}`);
}

// ---------------------------------------------------------------------------
const match = results.filter((r) => r.verdict === "match");
const mismatch = results.filter((r) => r.verdict === "mismatch");
const pirErr = results.filter((r) => r.verdict === "pir-error");
const nodeErr = results.filter((r) => r.verdict === "node-error");
const denom = match.length + mismatch.length + pirErr.length;
const pct = (n, d) => (d ? ((100 * n) / d).toFixed(1) + "%" : "n/a");

const lines = [];
lines.push("# pir × real node — 模式级差分探针报告");
lines.push("");
lines.push(`- 日期: ${new Date().toISOString().slice(0, 10)} · pir: \`${PIR}\` · node: ${process.version}`);
lines.push(`- 探针: ${results.length} 条 · 逐条走「真 node 先跑通 → pir 跑 → key: value 全量对拍」`);
lines.push(`- 结果: match ${match.length} / mismatch ${mismatch.length} / pir-error ${pirErr.length} (合计 ${pct(match.length, denom)})${nodeErr.length ? ` · node 侧无效 ${nodeErr.length}` : ""}`);
lines.push("");
lines.push("## 逐条结果");
lines.push("");
lines.push("| 探针 | 类别 | 结果 | 说明 |");
lines.push("|---|---|---|---|");
for (const r of results) lines.push(`| ${r.id} | ${r.cat} | ${r.verdict} | ${r.why.replace(/\|/g, "\\|")} |`);
lines.push("");
if (mismatch.length || pirErr.length) {
  lines.push("## 分歧明细");
  lines.push("");
  for (const r of [...mismatch, ...pirErr]) {
    lines.push(`### ${r.id} (${r.verdict})`);
    lines.push("");
    lines.push("```");
    lines.push(r.detail.trim().slice(0, 2000) || "(no detail)");
    lines.push("```");
    lines.push("");
  }
}
if (nodeErr.length) {
  lines.push("## node 侧无效探针（不计入分母）");
  lines.push("");
  for (const r of nodeErr) lines.push(`- ${r.id}: ${r.detail}`);
  lines.push("");
}
writeFileSync(outArg, lines.join("\n") + "\n", "utf8");
writeFileSync(join(OUT_DIR, "results.json"), JSON.stringify(results, null, 1), "utf8");
if (verbose) for (const r of [...mismatch, ...pirErr]) console.log(`\n--- ${r.id} ---\n${r.detail}`);
console.log(`\nSUMMARY: patterns=${results.length} match=${match.length} mismatch=${mismatch.length} pirError=${pirErr.length} nodeError=${nodeErr.length}`);
console.log(`report: ${outArg}`);
