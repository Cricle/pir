//! `@earendil-works/chord` virtual shim: the portable kernel + `/context` + `/delta`
//! entries, driven through the extension runtime (the real target environment:
//! QuickJS, the typebox shims, the host JSON-Schema subset validator).
//!
//! The fixture is the translation of a representative slice of upstream
//! `packages/chord/test/*.test.ts` (`3daa20280`, including the wire generation of
//! `90f17f59a`/`018c87ddc`/`db6497c3d`: `flow.test.ts` and the packed-payload
//! suites of `link.test.ts`/`websocket.test.ts`) into one self-checking
//! scenario list; every case names the upstream file it came from. The probe
//! imports the published specifiers (`@earendil-works/chord`,
//! `/context`, `/delta`, `/node`), so this test also covers the
//! `shims.rs` registration table. The same case list passes under node against
//! (a) the real `typebox` 1.3.27 package and (b) this runtime's typebox shims —
//! see the wave report; the cases that need Node-only surfaces (loader, child
//! hosts, `ws`, worker-GC benchmarks) are not translated, and `/node` is
//! asserted for its throws contract instead.
//!
//! Fixtures are seeded from source strings at runtime (the repo hygiene gate
//! forbids tracked `.js`/`.ts` sources), mirroring tests/node_shims.rs.

use std::path::{Path, PathBuf};

use pir::extensions::{ExtensionConfig, ExtensionRuntime};
use serde_json::{json, Value};

/// The probe module: the translated upstream cases plus their assertions.
const PROBE_MJS: &str = r####"// chord shim probe: the representative upstream chord cases, translated to run inside the extension runtime.
//
// Source of every case: upstream `packages/chord/test/*.test.ts` (`3daa20280`); the case titles name the file.
// The flow/packed cases at the end come from the wire generation of `90f17f59a`/`018c87ddc`/`db6497c3d`
// (schema-derived frames + `ack`, end-to-end flow control, deflated >64 KiB payloads), i.e. upstream
// `test/flow.test.ts` and the "packed payloads" suites of `test/link.test.ts` / `test/websocket.test.ts`.
// Cases that need Node-only surfaces (loader/manager/ws server/worker GC benchmarks) are not translated — the
// `/node` entry's throws contract is asserted instead. The probe imports the virtual modules by their published
// specifiers, so it also exercises the `shims.rs` registration table.

import {
  ChordError,
  bind,
  cell,
  copyJson,
  coverage,
  createHost,
  defineRegistry,
  derivation,
  forbidden,
  hostPrincipal,
  invalid,
  isChordError,
  isJsonValue,
  listener,
  local,
  acceptWebSockets,
  memoryLinkPair,
  method,
  opaque,
  remote,
  replicatedState,
  resource,
  webSocketLink,
} from "@earendil-works/chord";
// The namespace, so the D33 additions (`PACK_THRESHOLD`, `inflateMessage`) can be probed without breaking the
// fixture on a shim that predates them: the cases assert the names are present where upstream exports them.
import * as chordRoot from "@earendil-works/chord";
import { deflateRawSync, inflateRawSync } from "node:zlib";
import { Type } from "typebox";
import { BACKGROUND_CONTEXT, createContextKey, withCancel, withContextValue } from "@earendil-works/chord/context";
import {
  PathError,
  RESERVED_SEGMENTS,
  UnsafePathError,
  apply,
  applyImmutable,
  applyImmutableBatches,
  assertSafePath,
  assertValidOp,
  assertValidWireOp,
  decoder,
  diffRevisions,
  encoder,
  isBase,
  isReplace,
  overlap,
  track,
} from "@earendil-works/chord/delta";
import {
  childControl,
  createHostManager,
  forkDriver,
  nodeLoader,
  processLock,
} from "@earendil-works/chord/node";

const results = [];
const deepEqual = (a, b) => JSON.stringify(a) === JSON.stringify(b);
const assert = (condition, message) => {
  if (!condition) throw new Error(message ?? "assertion failed");
};
const assertEqual = (actual, expected, message) => {
  if (actual !== expected) throw new Error((message ?? "values differ") + ": " + JSON.stringify(actual) + " !== " + JSON.stringify(expected));
};
const assertDeep = (actual, expected, message) => {
  if (!deepEqual(actual, expected)) throw new Error((message ?? "values differ") + ": " + JSON.stringify(actual) + " !== " + JSON.stringify(expected));
};
const assertThrows = (fn, pattern, message) => {
  let thrown;
  try {
    fn();
  } catch (error) {
    thrown = error;
  }
  assert(thrown !== undefined, (message ?? "expected a throw") + " (nothing thrown)");
  if (pattern !== undefined) assert(pattern.test(String(thrown.message)), `${message ?? "throw"}: ${thrown.message}`);
  return thrown;
};
const assertRejects = async (promise, pattern, message) => {
  let thrown;
  try {
    await promise;
  } catch (error) {
    thrown = error;
  }
  assert(thrown !== undefined, (message ?? "expected a rejection") + " (resolved)");
  if (pattern !== undefined) assert(pattern.test(String(thrown.message)), `${message ?? "rejection"}: ${thrown.message}`);
  return thrown;
};
const defer = () => {
  const box = {};
  box.promise = new Promise((resolve, reject) => {
    box.resolve = resolve;
    box.reject = reject;
  });
  return box;
};
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
const ticks = async (count) => {
  for (let index = 0; index < count; index++) await tick();
};
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
/** Polls until `check` holds (upstream flow.test.ts's helper: the socket/link paths need event-loop turns). */
const until = async (check, attempts = 400) => {
  for (let i = 0; i < attempts && !check(); i++) await sleep(5);
  assert(check(), "condition not reached");
};
/** The wire form of a deflated payload field: raw DEFLATE, base64 (what an upstream peer puts in `z`). */
const rawDeflateJson = (value) => Buffer.from(deflateRawSync(JSON.stringify(value), { level: 1 })).toString("base64");
const inflateJson = (bytes) => JSON.parse(inflateRawSync(bytes).toString("utf8"));

const record = (name, error) => {
  results.push({ name, ok: error === undefined, error: error === undefined ? undefined : String(error && error.message ? error.message : error) });
};
const check = (name, fn) => {
  try {
    fn();
    record(name);
  } catch (error) {
    record(name, error);
  }
};
const checkAsync = async (name, fn) => {
  try {
    await fn();
    record(name);
  } catch (error) {
    record(name, error);
  }
};

// ─── delta ───────────────────────────────────────────────────────────────────

function deltaCases() {
  check("delta-clone: track takes ownership in O(1) and keeps structure shared", () => {
    const input = { point: { x: 3, y: 7 }, rows: [{ values: [0, false, null, "text", { n: 1 }] }] };
    const tracker = track(input);
    assertEqual(tracker.value, input, "root identity");
    assertEqual(tracker.value.point, input.point, "child identity");
    assertEqual(tracker.value.rows[0].values[4], input.rows[0].values[4], "grandchild identity");
    assertEqual(Object.isFrozen(tracker.value.rows[0].values), false, "no freezing");
    const dictionary = Object.assign(Object.create(null), { enabled: true });
    const nested = { dictionary, left: { n: 1 }, right: { n: 1 } };
    const second = track(nested);
    assertEqual(Object.getPrototypeOf(second.value.dictionary), null, "null prototype preserved");
  });

  check("delta-tracker: change/prepare/adopt lifecycle, revision and double adopt", () => {
    const tracker = track({ rows: [], meta: { revision: 0 } });
    assertEqual(tracker.revision, 0, "initial revision");
    const change = tracker.beginChange();
    change.state.rows.push({ id: 1 });
    change.state.meta.revision = 1;
    const prepared = change.prepare();
    assertEqual(tracker.value.meta.revision, 0, "prepare does not adopt");
    assertDeep(prepared.base, { rows: [], meta: { revision: 0 } }, "base is the previous revision");
    tracker.adopt(prepared);
    assertDeep(tracker.value, { rows: [{ id: 1 }], meta: { revision: 1 } }, "adopted value");
    assertEqual(tracker.revision, 1, "revision advanced");
    assertDeep(applyImmutable(prepared.base, prepared.ops), prepared.value, "ops replay to the prepared value");
    assertThrows(() => tracker.adopt(prepared), /already been used/, "double adopt");
  });

  check("delta-tracker: adopting one change stales its competitor", () => {
    const tracker = track({ n: 0 });
    const winner = tracker.beginChange();
    const loser = tracker.beginChange();
    winner.state.n = 1;
    const prepared = winner.prepare();
    tracker.adopt(prepared);
    assertThrows(() => loser.prepare(), /settled/, "stale draft");
  });

  check("delta-tracker: restoring the original value normalizes to an empty batch", () => {
    const tracker = track({ n: 1 });
    const change = tracker.beginChange();
    change.state.n = 2;
    change.state.n = 1;
    const prepared = change.prepare();
    assertDeep(prepared.ops, [], "no-op batch");
    assertEqual(prepared.value, prepared.base, "no-op keeps the base");
    tracker.adopt(prepared);
    assertEqual(tracker.revision, 1, "a no-op adoption still advances the revision");
  });

  check("delta-tracker: prepareReplace is whole-root and a deep-equal replacement is a no-op", () => {
    const tracker = track({ a: 1 });
    tracker.adopt(tracker.prepareReplace({ a: 2, b: 3 }));
    const equal = tracker.prepareReplace({ a: 2, b: 3 });
    assertDeep(equal.ops, [], "deep-equal replacement is a no-op");
    assertEqual(equal.value, tracker.value, "the current root is kept");
    assertDeep(tracker.prepareReplace({ a: 9 }).ops, [["r", { a: 9 }]], "replacement is one r op");
  });

  check("delta-tracker: undefined deletes a key, holes and undefined placements throw", () => {
    const tracker = track({ a: 1, b: 2, list: [1] });
    const change = tracker.beginChange();
    change.state.b = undefined;
    assertEqual("b" in change.state, false, "undefined deletes the key");
    assertThrows(() => { change.state.list[3] = 1; }, /holes|array indices/, "sparse write");
    assertThrows(() => { change.state.list[0] = undefined; }, /strict JSON/, "undefined element");
    assertDeep(Object.keys(change.prepare().value).sort(), ["a", "list"], "deleted key gone");
  });

  check("delta-tracker: placements are validated clones", () => {
    const tracker = track({ rows: [] });
    const change = tracker.beginChange();
    const assigned = { nested: { value: 1 } };
    change.state.rows.push(assigned);
    assigned.nested.value = 9;
    assertEqual(change.state.rows[0].nested.value, 1, "placed object cloned");
    const cyclic = {};
    cyclic.self = cyclic;
    assertThrows(() => { change.state.rows.push(cyclic); }, /cycles/, "cyclic placement");
    assertThrows(() => { change.state.rows.push(() => {}); }, /strict JSON/, "function placement");
  });

  check("delta-tracker: array mutators stay narrow and replay exactly", () => {
    const tracker = track({ rows: [{ id: 1 }, { id: 2 }, { id: 3 }] });
    const change = tracker.beginChange();
    change.state.rows.splice(0, 1);
    change.state.rows.push({ id: 4 });
    const prepared = change.prepare();
    assertDeep(prepared.value.rows, [{ id: 2 }, { id: 3 }, { id: 4 }], "splice + push");
    assertDeep(applyImmutable(prepared.base, prepared.ops), prepared.value, "replay");
    const reverse = tracker.beginChange();
    reverse.state.rows.reverse();
    assertDeep(reverse.prepare().value.rows.map((row) => row.id), [3, 2, 1], "reverse");
    const sorted = tracker.beginChange();
    sorted.state.rows.sort((left, right) => (left.id > right.id ? -1 : 1));
    assertDeep(sorted.prepare().value.rows.map((row) => row.id), [3, 2, 1], "descending sort");
    const filled = tracker.beginChange();
    filled.state.rows.fill({ id: 0 }, 0, 1);
    assertDeep(filled.prepare().value.rows.map((row) => row.id), [0, 2, 3], "fill");
  });

  check("delta-tracker: array length grows with nulls and shrinks by removal", () => {
    const tracker = track({ list: [1, 2] });
    const change = tracker.beginChange();
    change.state.list.length = 4;
    assertDeep(change.state.list, [1, 2, null, null], "growth fills nulls");
    const prepared = change.prepare();
    assertDeep(applyImmutable(prepared.base, prepared.ops), prepared.value, "replay");
    const shrink = tracker.beginChange();
    shrink.state.list.length = 1;
    assertDeep(shrink.prepare().value.list, [1], "shrink removes");
  });

  check("delta: encoder/decoder round trip with id interning and arity omission", () => {
    const ops = [
      ["s", ["a"], 1],
      ["s", ["a"], 2],
      ["d", ["b", 0]],
      ["a", ["b", 0], "suffix"],
      ["t", ["b", 0], 2],
      ["p", ["list"], 1, 0, [9]],
      ["m", ["list"], [1, 0]],
    ];
    const wire = encoder().encode(ops);
    assertDeep(wire[0], ["s", ["a"], 1], "first use inlines the path");
    assertDeep(wire[1], ["s", 2], "the same path as the previous op omits the ref: " + JSON.stringify(wire[1]));
    assertDeep(wire[3], ["a", "suffix"], "short form again for the repeated path");
    assertDeep(wire[2], ["d", ["b", 0]], "a new path inlines");
    assertDeep(wire[6], ["m", [1, 0]], "p then m on the same path");
    assertDeep(decoder().decode(wire), ops, "decoded ops equal the originals");
    const base = encoder().encode([["s", ["x"], 1], ["r", { x: 1 }], ["s", ["x"], 2]]);
    assertDeep(base, [["s", ["x"], 1], ["r", { x: 1 }], ["s", ["x"], 2]], "r resets the encoder dictionaries");
    assertDeep(decoder().decode(base), [["s", ["x"], 1], ["r", { x: 1 }], ["s", ["x"], 2]], "r resets the decoder");
    assert(isBase([["r", {}]]), "isBase after r");
    assertEqual(isBase([["s", ["a"], 1]]), false, "no r, no base");
    assert(isReplace(["r", 1]), "isReplace");
    const interned = encoder().encode([["s", ["k"], 1], ["d", ["j"]], ["s", ["k"], 2]]);
    assertDeep(interned[2], ["#", 0, ["k"]], "the second use defines an id first: " + JSON.stringify(interned));
    assertDeep(interned[3], ["s", 0, 2], "then references it");
  });

  check("delta: reserved keys never reach a path and the appliers refuse them", () => {
    for (const key of RESERVED_SEGMENTS) {
      assertThrows(() => assertSafePath([key]), /unsafe path segment/, "assertSafePath " + key);
      assertThrows(() => apply({}, [["s", [key, "x"], 1]]), /unsafe path segment/, "apply " + key);
      assertThrows(() => decoder().decode([["s", [key], 1]]), /unsafe path segment/, "decoder " + key);
    }
    assertDeep(apply({ a: 1 }, [["s", ["a"], 2]]), { a: 2 }, "a safe path still applies");
    assertEqual(Object.prototype.polluted, undefined, "no prototype pollution");
  });

  check("delta: op shape validation and applier errors", () => {
    assertValidOp(["s", ["a"], 1]);
    assertThrows(() => assertValidOp(["s", ["a"]]), /arity/, "s arity");
    assertThrows(() => assertValidOp(["?", ["a"]]), /unknown op verb/, "unknown verb");
    assertThrows(() => assertValidOp(["m", ["a"], [0, 0]]), /bijection/, "m bijection");
    assertThrows(() => assertValidOp(["p", ["a"], -1, 0, []]), /index/, "p index");
    assertValidWireOp(["d"]);
    assertValidWireOp(["#", 0, ["a"]]);
    assertThrows(() => assertValidWireOp(["d", "a"]), /path is not an array/, "a string is not a path");
    assertThrows(() => apply({ a: 1 }, [["p", ["a"], 0, 1, []]]), /unresolvable/, "splice a non-array");
    assertThrows(() => apply({}, [["s", ["nested", "a"], 1]]), /unresolvable/, "missing parent");
    assertThrows(() => applyImmutable({ a: 1 }, [["s", ["b", "c"], 1]]), /unresolvable/, "immutable path");
    assertEqual(typeof PathError, "function", "PathError exported");
    assertEqual(typeof UnsafePathError, "function", "UnsafePathError exported");
  });

  check("delta: diffRevisions vectors (string, array, permutation, reserved)", () => {
    assertDeep(diffRevisions("a", "abc"), [["r", "abc"]], "a root string replaces (s/t/a cannot target the root)");
    assertDeep(diffRevisions({ s: "a" }, { s: "abc" }), [["a", ["s"], "bc"]], "string append");
        assertDeep(diffRevisions({ s: "abcdef" }, { s: "abc" }), [["s", ["s"], "abc"]], "no suffix/prefix overlap is a set");
    assertDeep(
      diffRevisions({ s: "abcXYZ" }, { s: "XYZdef" }),
      [["t", ["s"], 3], ["a", ["s"], "def"]],
      "an overlapping suffix truncates then appends",
    );
    assertDeep(diffRevisions([1, 2, 3], [1, 3]), [["p", [], 1, 1, []]], "array removal");
    assertDeep(diffRevisions([1, 2], [2, 1]), [["m", [], [1, 0]]], "permutation");
    assertDeep(diffRevisions({ a: 1 }, { a: 1, b: 2 }), [["s", ["b"], 2]], "added key");
    assertDeep(diffRevisions({ a: 1, b: 2 }, { a: 1 }), [["d", ["b"]]], "removed key");
    assertEqual(diffRevisions({}, JSON.parse('{"__proto__":{"x":1}}'))[0][0], "r", "reserved key folds into a root replacement");
  });

  check("delta: overlap helper", () => {
    assertEqual(overlap("abc", "bcd", 65_536), 2, "shifted tail");
    assertEqual(overlap("abcXYZ", "XYZdef", 65_536), 3, "longest suffix that is a prefix");
    assertEqual(overlap("abcdef", "abc", 65_536), 0, "a suffix that is not a prefix");
    assertEqual(overlap("abc", "xyz", 65_536), 0, "no overlap");
    assertEqual(overlap("", "abc", 65_536), 0, "empty");
  });

  check("delta: applyImmutableBatches replays a backlog to the final result", () => {
    const tracker = track({ entries: [], meta: { revision: 0 } });
    const batches = [];
    let replica = tracker.value;
    for (let index = 1; index <= 3; index++) {
      const change = tracker.beginChange();
      change.state.entries.push({ index });
      change.state.meta.revision = index;
      const prepared = change.prepare();
      tracker.adopt(prepared);
      batches.push(prepared.ops);
      assertDeep(applyImmutable(replica, prepared.ops), prepared.value, "single-batch replay " + index);
      replica = prepared.value;
    }
    assertDeep(
      applyImmutableBatches({ entries: [], meta: { revision: 0 } }, batches),
      tracker.value,
      "backlog replay equals the authority",
    );
  });
}

// ─── context ─────────────────────────────────────────────────────────────────

function contextCases() {
  check("context: values nest, replace by key identity and hide unknown keys", () => {
    const key = createContextKey("probe");
    const other = createContextKey("probe");
    const base = withContextValue(key, 1, BACKGROUND_CONTEXT);
    assertEqual(base.value(key), 1, "own value");
    assertEqual(base.value(other), undefined, "the token, not the description, is the identity");
    const replaced = withContextValue(key, 2, base);
    assertEqual(replaced.value(key), 2, "child replaces the parent value");
    assertEqual(base.value(key), 1, "parent unchanged");
    assert(String(replaced).includes("WithValue"), "toString");
    assertEqual(BACKGROUND_CONTEXT.abortSignal, undefined, "the background context carries no signal");
  });

  check("context: withCancel aborts only its own branch", () => {
    const cancel = withCancel(BACKGROUND_CONTEXT);
    assertEqual(cancel.context.abortSignal.aborted, false, "not aborted yet");
    cancel.cancel(new Error("stop"));
    assertEqual(cancel.context.abortSignal.aborted, true, "aborted");
  });
}

// ─── json ────────────────────────────────────────────────────────────────────

function jsonCases() {
  check("json: copyJson builds an alias-free strict-JSON tree", () => {
    const source = { a: [1, "two", null, true], b: { c: { d: 4 } } };
    const copy = copyJson(source);
    assertDeep(copy, source, "same content");
    assertEqual(copy.b === source.b, false, "no shared container");
    assertEqual(copy.a === source.a, false, "no shared array");
    assertEqual(isJsonValue(copy), true, "strict JSON");
    assertEqual(isJsonValue({ a: undefined }), false, "undefined is not JSON");
    assertEqual(isJsonValue({ a: Number.NaN }), false, "NaN is not JSON");
    assertThrows(() => copyJson({ a: undefined }), /strict JSON/, "undefined member");
    const cyclic = {};
    cyclic.self = cyclic;
    assertThrows(() => copyJson(cyclic), /cycles/, "cycle");
    assertThrows(() => copyJson({ a: () => {} }), /non-JSON/, "function member");
  });
}

// ─── contracts ───────────────────────────────────────────────────────────────

function contractCases() {
  const Item = local("t.items", {
    version: 1,
    input: Type.Object({ id: Type.String() }),
    members: {
      title: Type.String(),
      rename: method(Type.Object({ title: Type.String() }), Type.Null()),
      handle: opaque("native-thing@1"),
    },
  });

  check("contract: local description, fingerprint and validation", () => {
    assertEqual(Item.scope, "local", "scope");
    assertEqual(Item.version, 1, "version");
    assertEqual(typeof Item.fingerprint, "string", "fingerprint");
    const members = Item.description.variants[""];
    assertEqual(members.title.kind, "state", "state member");
    assertEqual(members.rename.kind, "method", "method member");
    assertEqual(members.handle.kind, "opaque", "opaque member");
    assertEqual(members.handle.id, "native-thing@1", "opaque id");
    assertThrows(
      () => local("t.bad", { version: 1, input: Type.Object({}), members: { status: Type.Number() } }),
      /reserved/,
      "reserved member name",
    );
    assertThrows(() => local("", { version: 1, input: Type.Object({}) }), /name is empty/, "empty name");
    assertThrows(() => local("t.bad", { version: 0, input: Type.Object({}) }), /positive integer/, "version");
    assertThrows(() => opaque("no-version"), /name@version/, "opaque id");
    assertThrows(
      () => local("t.bad", { version: 1, input: Type.Object({}), members: { m: opaque("x") } }),
      /name@version/,
      "opaque member id",
    );
    assertThrows(() => remote("t.r", { version: 1, input: Type.Object({}), variants: {} }), /no variants/, "remote without variants");
    assertThrows(
      () =>
        remote("t.r", {
          version: 1,
          input: Type.Object({}),
          variants: { kind: { methods: { bad: Type.Number() } } },
        }),
      /not a method/,
      "methods entry that is not method()",
    );
    assertThrows(
      () =>
        remote("t.r", {
          version: 1,
          input: Type.Object({}),
          variants: { kind: { state: { dup: Type.Number() }, methods: { dup: method(Type.Null(), Type.Null()) } } },
        }),
      /both state and method/,
      "state/method name clash",
    );
  });

  check("contract: coverage and bind report fits, adapters and mismatches", () => {
    const provider = local("t.cover", {
      version: 1,
      input: Type.Object({ id: Type.String() }),
      members: { value: Type.Number() },
    });
    const consumer = local("t.cover", {
      version: 1,
      input: Type.Object({ id: Type.String() }),
      members: { value: Type.Number() },
    });
    assertEqual(coverage(provider.description, consumer.description), undefined, "identical descriptions cover");
    assertEqual(bind(provider, undefined, consumer), undefined, "identical fingerprints bind directly");
    const narrower = local("t.cover", {
      version: 1,
      input: Type.Object({ id: Type.String() }),
      members: { value: Type.Number(), extra: Type.Number() },
    });
    assertEqual(typeof coverage(provider.description, narrower.description), "string", "a missing member is reported");
    assertThrows(() => bind(provider, undefined, narrower), /contract_mismatch/, "same version, no adapter");
    const bumped = local("t.cover", {
      version: 2,
      input: Type.Object({ id: Type.String() }),
      members: { value: Type.Number() },
    });
    assertThrows(() => bind(provider, undefined, bumped), /no accepts adapter/, "bumped without accepts");
    const adapter = (current) => ({ value: current.value });
    assertEqual(bind(provider, { 2: adapter }, bumped), adapter, "accepts adapter found");
  });

  check("contract: errors carry code, reason, retryable and message prefix", () => {
    const error = invalid("args", "bad args", [{ path: "/x" }]);
    assert(isChordError(error, "invalid"), "isChordError by code");
    assertEqual(error.reason, "args", "reason");
    assertEqual(error.retryable, false, "invalid is not retryable");
    assertEqual(String(error.message).startsWith("invalid/args: "), true, "message prefix: " + error.message);
    assertEqual(isChordError(forbidden(), "forbidden"), true, "forbidden()");
    assertEqual(new ChordError("superseded", "epoch retired").retryable, true, "superseded is retryable");
    assertEqual(isChordError(new Error("plain")), false, "a plain Error is not a ChordError");
  });
}

// ─── host ────────────────────────────────────────────────────────────────────

const quietHost = (name, errors) => createHost({ name, dev: true, onError: (error) => errors.push(error) });

async function hostCases() {
  await checkAsync("host: cells, derivations, methods and the handle lifecycle", async () => {
    const Counter = local("t.counter", {
      version: 1,
      input: Type.Object({ id: Type.String() }),
      members: { count: Type.Number(), doubled: Type.Number(), bump: method(Type.Null(), Type.Number()) },
    });
    const host = quietHost("probe-counter", []);
    host.serve(Counter, (_input, _principal, self) => {
      const state = self.state({ count: cell(Type.Number(), 0) });
      const derived = self.derive({ doubled: derivation([state.count], (count) => count * 2) });
      return {
        count: state.count,
        doubled: derived.doubled,
        bump: (_args, ctx) => {
          state.count.change(ctx, (count) => count + 1);
          return state.count.value;
        },
      };
    });
    const handle = host.get(Counter, { id: "a" });
    assertEqual(handle.status, "loading", "loading before ready");
    await handle.ready;
    assertEqual(handle.status, "live", "live after ready");
    assertDeep([...handle.shape.members].sort(), ["bump", "count", "doubled"], "shape members include methods");
    assertEqual(handle.kind, undefined, "a local variant has no kind");
    assertEqual(typeof handle.shape.contract, "string", "shape carries the provider fingerprint");
    assertEqual(await host.root((ctx) => handle.bump(null, ctx)), 1, "method call returns the new value");
    await host.root((ctx) => handle.bump(null, ctx));
    assertEqual(handle.count.value, 2, "state member readable");
    assertEqual(handle.doubled.value, 4, "derivation followed the write");
    const seen = [];
    const stop = handle.count.subscribe((value) => seen.push(value));
    await host.root((ctx) => handle.bump(null, ctx));
    stop();
    assertDeep(seen, [2, 3], "a subscription starts with the current value, then updates");
    const events = [];
    const stopEvents = handle.count.events((event) => events.push(event.type));
    stopEvents();
    assertDeep(events, ["snap"], "events start with a snapshot");
    assertEqual(host.get(Counter, { id: "a" }).record, handle.record, "the same address reuses the record");
    const statuses = [];
    const stopStatus = handle.onStatus((status) => statuses.push(status));
    handle.release();
    await tick();
    stopStatus();
    assertDeep(statuses, ["released"], "release notifies status listeners");
    await host.close();
  });

  await checkAsync("host: an unserved type rejects unavailable/unknown_type", async () => {
    const Bare = local("t.bare", { version: 1, input: Type.Object({}), members: { ok: Type.Number() } });
    const host = quietHost("probe-bare", []);
    const failure = assertThrows(() => host.get(Bare, {}), /is not served by/, "unserved type");
    assert(isChordError(failure, "unavailable"), "unavailable code");
    assertEqual(failure.reason, "unknown_type", "reason");
    await host.close();
  });

  await checkAsync("host: a factory failure keeps its code and releases what it opened", async () => {
    const errors = [];
    const host = quietHost("probe-failing", errors);
    const Leaf = local("t.leaf", { version: 1, input: Type.Object({}) });
    const Bad = local("t.bad", { version: 1, input: Type.Object({ async: Type.Boolean() }) });
    let live = 0;
    host.serve(Leaf, (_input, _principal, self) => {
      self.resources({
        r: resource({
          rev: 1,
          open: () => {
            live++;
          },
          close: () => {
            live--;
          },
        }),
      });
      return {};
    });
    host.serve(Bad, ({ async: asyncFlag }, _principal, self) => {
      self.get(Leaf, {});
      if (asyncFlag) return Promise.resolve({});
      throw new Error("factory failed");
    });
    await assertRejects(host.get(Bad, { async: false }).ready, /factory failed/, "sync failure");
    await assertRejects(host.get(Bad, { async: true }).ready, /synchronous/, "async factory");
    await ticks(2);
    assertEqual(live, 0, "the dependency record was unwound");
    await host.close();
  });

  await checkAsync("host: rejects duplicate declaration names and detects hold cycles", async () => {
    const host = quietHost("probe-dup", []);
    const Dup = local("t.dup", { version: 1, input: Type.Object({}) });
    host.serve(Dup, (_input, _principal, self) => {
      self.state({ a: cell(Type.Object({}), {}) });
      self.state({ a: cell(Type.Object({}), {}) });
      return {};
    });
    await assertRejects(host.get(Dup, {}).ready, /duplicate state name a/, "duplicate names");
    const A = local("t.cycle-a", { version: 1, input: Type.Object({}) });
    const B = local("t.cycle-b", { version: 1, input: Type.Object({}) });
    host.serve(A, (_input, _principal, self) => {
      self.get(B, {});
      return {};
    });
    host.serve(B, (_input, _principal, self) => {
      self.get(A, {});
      return {};
    });
    await assertRejects(host.get(A, {}).ready, /cycle/, "hold cycle");
    await host.close();
  });

  await checkAsync("host: closes resources in reverse order and aggregates close errors", async () => {
    const errors = [];
    const host = quietHost("probe-resources", errors);
    const log = [];
    const Multi = local("t.multi", { version: 1, input: Type.Object({}) });
    host.serve(Multi, (_input, _principal, self) => {
      const first = self.resources({
        a: resource({ rev: 1, open: () => "a", close: () => void log.push("close a") }),
      });
      self.resources({
        b: resource({
          rev: 1,
          deps: [first.a],
          open: () => "b",
          close: () => {
            log.push("close b");
            throw new Error("b failed");
          },
        }),
        c: resource({
          rev: 1,
          open: () => "c",
          close: () => {
            log.push("close c");
            throw new Error("c failed");
          },
        }),
      });
      return {};
    });
    const handle = host.get(Multi, {});
    await handle.ready;
    handle.release();
    await ticks(3);
    assertEqual(log[log.length - 1], "close a", "the dependency closes last: " + JSON.stringify(log));
    assertEqual(log.length, 3, "all closes ran");
    assert(errors[0] instanceof AggregateError, "close failures aggregate");
    assertEqual(errors[0].errors.length, 2, "both failures collected");
    await host.close();
  });

  await checkAsync("host: listeners react in a fresh root and tasks start fresh calls", async () => {
    const Source = local("t.listen-source", {
      version: 1,
      input: Type.Object({}),
      members: { value: Type.Number(), set: method(Type.Number(), Type.Null()) },
    });
    const Watcher = local("t.listen-watcher", {
      version: 1,
      input: Type.Object({}),
      members: { seen: Type.Number(), seeded: Type.Number() },
    });
    const host = quietHost("probe-listen", []);
    host.serve(Source, (_input, _principal, self) => {
      const state = self.state({ value: cell(Type.Number(), 0) });
      return {
        value: state.value,
        set: (next, ctx) => {
          state.value.replace(ctx, next);
          return null;
        },
      };
    });
    const seeded = defer();
    host.serve(Watcher, (_input, _principal, self) => {
      const upstream = self.get(Source, {});
      const state = self.state({ seen: cell(Type.Number(), 0), seeded: cell(Type.Number(), 0) });
      self.listen({
        value: listener(upstream.value, (value, ctx) => {
          state.seen.replace(ctx, value);
        }),
      });
      self.tasks({
        seed: async (ctx) => {
          await upstream.set(5, ctx);
          state.seeded.replace(ctx, 1);
          seeded.resolve();
        },
      });
      return { seen: state.seen, seeded: state.seeded };
    });
    const watcher = host.get(Watcher, {});
    await watcher.ready;
    const source = host.get(Source, {});
    await source.ready;
    await host.root((ctx) => source.set(7, ctx));
    await ticks(3);
    assertEqual(watcher.seen.value, 7, "the listener wrote through a fresh root");
    await seeded.promise;
    await ticks(2);
    assertEqual(source.value.value, 7, "the in-process write is visible to the other handle of the same record");
    assertEqual(watcher.seeded.value, 1, "the activation task ran through its own root");
    watcher.release();
    source.release();
    await host.close();
  });

  await checkAsync("host: eager records, registries and principal-scoped records", async () => {
    const Seen = defineRegistry("t.seen");
    const Store = local("t.store", { version: 1, input: Type.Object({}), members: { who: Type.String() } });
    const host = quietHost("probe-eager", []);
    let factories = 0;
    host.serve(
      Store,
      (_input, principal, self) => {
        factories += 1;
        const label = principal === hostPrincipal ? "host" : String(principal);
        // Only eager records may register.
        if (principal === hostPrincipal) self.register(Seen, { current: label });
        return { who: self.state({ who: cell(Type.String(), label) }).who };
      },
      { eager: true },
    );
    await host.ready();
    assertEqual(factories, 1, "the eager record activated once");
    assertEqual(host.registry(Seen).get("current"), "host", "the eager record is the host principal's");
    const guest = host.get(Store, {}, { principal: "guest" });
    await guest.ready;
    assertEqual(guest.who.value, "guest", "a principal-scoped record");
    const again = host.get(Store, {}, { principal: "guest" });
    await again.ready;
    assertEqual(factories, 2, "the same principal reuses its record");
    const other = host.get(Store, {}, { principal: "other" });
    await other.ready;
    assertEqual(factories, 3, "another principal activates another record");
    assertEqual(other.who.value, "other", "and sees its own view");
    assertEqual(host.registry(Seen).get("current"), "host", "another record's non-eager registration was refused");
    guest.release();
    again.release();
    other.release();
    await host.close();
  });

  await checkAsync("host: invalid arguments and results are rejected for remote-scope contracts", async () => {
    let called = 0;
    const Strict = remote("t.strict", {
      version: 1,
      input: Type.Object({}),
      variants: {
        plain: {
          methods: {
            add: method(Type.Object({ text: Type.String() }), Type.String()),
            bad: method(Type.Null(), Type.String()),
          },
        },
        extra: { methods: { missing: method(Type.Null(), Type.String()) } },
      },
    });
    const host = quietHost("probe-strict", []);
    host.serve(Strict, () => ({
      kind: "plain",
      add: ({ text }) => {
        called += 1;
        return text;
      },
      bad: () => 42,
    }));
    const handle = host.get(Strict, {});
    await handle.ready;
    assertEqual(await host.root((ctx) => handle.add({ text: "ok" }, ctx)), "ok", "valid call");
    const failure = await assertRejects(host.root((ctx) => handle.add({ text: 123 }, ctx)), /args/, "malformed args");
    assert(isChordError(failure, "invalid"), "invalid code for args");
    assertEqual(called, 1, "the implementation was not reached");
    const resultFailure = await assertRejects(host.root((ctx) => handle.bad(null, ctx)), /result/, "malformed result");
    assert(isChordError(resultFailure, "invalid"), "invalid code for the result");
    assertEqual(handle.nope, undefined, "an unknown member reads undefined");
    const other = await assertRejects(host.root((ctx) => handle.missing(null, ctx)), /not a method/, "member of another variant");
    assert(isChordError(other, "not_in_variant"), "not_in_variant for a member of another variant");
    handle.release();
    await host.close();
  });
}

// ─── replicated state ────────────────────────────────────────────────────────

async function stateCases() {
  await checkAsync("state: mutable state publishes a hydration then ordered updates", async () => {
    const state = replicatedState({ value: 0, rows: [] });
    const seen = [];
    const stop = state.subscribe((value, _context, delivery) => {
      seen.push(delivery.kind + ":" + value.value);
    });
    state.change(BACKGROUND_CONTEXT, (draft) => {
      draft.value = 1;
      draft.rows.push({ index: 1 });
    });
    state.replace(BACKGROUND_CONTEXT, { value: 2, rows: [{ index: 1 }] });
    await tick();
    stop();
    assertDeep(seen, ["hydrate:0", "update:1", "update:2"], "delivery kinds and order");
    assertEqual(state.value.value, 2, "authoritative value");
  });

  await checkAsync("state: a no-op change publishes nothing", async () => {
    const state = replicatedState({ value: 1 });
    const delivered = [];
    state.subscribe((_value, _context, delivery) => delivered.push(delivery.kind));
    state.change(BACKGROUND_CONTEXT, (draft) => {
      draft.value = 1;
    });
    await tick();
    assertDeep(delivered, ["hydrate"], "only the hydration");
  });

  await checkAsync("state: reentrant changes and async callbacks are refused", async () => {
    const state = replicatedState({ value: 0 });
    assertThrows(
      () =>
        state.change(BACKGROUND_CONTEXT, (draft) => {
          draft.value = 1;
          state.change(BACKGROUND_CONTEXT, (inner) => {
            inner.value = 2;
          });
        }),
      /reentrantly/,
      "reentrant change",
    );
    assertThrows(
      () => state.change(BACKGROUND_CONTEXT, () => Promise.resolve()),
      /synchronous/,
      "async callback",
    );
    assertEqual(state.value.value, 0, "the failed change did not land");
  });

  await checkAsync("state: an attached source hydrates, orders frames and reports a cursor gap", async () => {
    const queued = [];
    let deliver;
    const source = {
      attach() {
        return {
          snapshot: { value: { value: 0 }, cursor: 5 },
          activate(listener) {
            deliver = (frame) => listener(frame);
            for (const frame of queued) listener(frame);
          },
          dispose() {
            deliver = undefined;
          },
        };
      },
    };
    const publish = (frame) => {
      queued.push(frame);
      if (deliver !== undefined) deliver(frame);
    };
    const errors = [];
    const state = replicatedState(source, { onError: (error) => errors.push(error) });
    const deliveries = [];
    state.subscribe((value, _context, delivery) => deliveries.push(delivery.kind + ":" + value.value));
    assertEqual(state.value.value, 0, "the snapshot hydrates synchronously");
    publish({ cursor: 6, value: { value: 1 }, ops: [["s", ["value"], 1]], context: BACKGROUND_CONTEXT });
    await tick();
    assertDeep(deliveries, ["hydrate:0", "update:1"], "a frame is delivered as an update");
    publish({ cursor: 8, value: { value: 2 }, ops: [], context: BACKGROUND_CONTEXT });
    await tick();
    assertEqual(errors.length, 1, "the gap was reported once");
    assert(/gap/.test(String(errors[0].message)), "gap wording: " + String(errors[0].message));
    state.dispose();
  });
}

// ─── links ───────────────────────────────────────────────────────────────────

async function linkCases() {
  const Todos = remote("t.link-todos", {
    version: 1,
    input: Type.Object({ list: Type.String() }),
    variants: {
      default: {
        state: { rows: Type.Array(Type.Object({ text: Type.String() })) },
        methods: { add: method(Type.Object({ text: Type.String() }), Type.Null()) },
      },
    },
  });

  await checkAsync("link: a client host calls a remote object and follows its state", async () => {
    const server = quietHost("server", []);
    server.serve(Todos, (_input, _principal, self) => {
      const state = self.state({ rows: cell(Type.Array(Type.Object({ text: Type.String() })), []) });
      return {
        kind: "default",
        rows: state.rows,
        add: ({ text }, ctx) => {
          state.rows.change(ctx, (rows) => {
            rows.push({ text });
          });
          return null;
        },
      };
    });
    const [down, up] = memoryLinkPair();
    server.accept(down, { principal: "guest", kind: "client" });
    const browser = createHost({ name: "browser", kind: "client", upstream: up });
    const todos = browser.get(Todos, { list: "family" }, { at: "server" });
    const hydrated = defer();
    const updated = defer();
    const stop = todos.rows.subscribe((rows) => {
      if (rows !== undefined) hydrated.resolve();
      if (rows !== undefined && rows.length === 1) updated.resolve();
    });
    await todos.ready;
    await hydrated.promise;
    assertDeep(todos.rows.value, [], "remote state hydrated");
    await browser.root((ctx) => todos.add({ text: "Buy milk" }, ctx));
    await updated.promise;
    assertDeep(todos.rows.value, [{ text: "Buy milk" }], "remote state delta applied");
    const failure = await assertRejects(todos.add({ text: 123 }), /args/, "malformed remote args");
    assert(isChordError(failure, "invalid"), "invalid code over the link");
    stop();
    todos.release();
    await browser.close();
    await server.close();
  });

  await checkAsync("link: a closed link re-holds or goes away", async () => {
    const Echo = remote("t.link-echo", {
      version: 1,
      input: Type.Object({}),
      variants: { default: { methods: { echo: method(Type.String(), Type.String()) } } },
    });
    const server = quietHost("server2", []);
    server.serve(Echo, () => ({ kind: "default", echo: (text) => text }));
    const [down, up] = memoryLinkPair();
    server.accept(down, { principal: "guest", kind: "client" });
    const browser = createHost({ name: "browser2", kind: "client", upstream: up });
    const echo = browser.get(Echo, {}, { at: "server2" });
    await echo.ready;
    assertEqual(await echo.echo("hi"), "hi", "remote call");
    up.close("done");
    await ticks(3);
    assertEqual(echo.status === "loading" || echo.status === "gone", true, "loading again or gone, got " + echo.status);
    echo.release();
    await browser.close();
    await server.close();
  });
}

async function webSocketCases() {
  // A browser-style WebSocket pair: `webSocketLink` + `acceptWebSockets` without the `ws` package
  // (upstream websocket.test.ts needs a real server; the link semantics are what is under test here).
  const socketPair = () => {
    const make = () => ({
      readyState: 1,
      listeners: {},
      peer: undefined,
      /** Messages that arrived before a listener attached: a real WebSocket queues them (undici/browsers do). */
      pending: [],
      send(data) {
        const target = this.peer;
        setTimeout(() => {
          const listeners = target.listeners.message ?? [];
          if (listeners.length === 0) {
            target.pending.push(data);
            return;
          }
          for (const listener of listeners) listener({ data });
        }, 0);
      },
      close() {
        this.readyState = 3;
        for (const listener of this.listeners.close ?? []) listener();
      },
      addEventListener(type, listener) {
        if (this.listeners[type] === undefined) this.listeners[type] = [];
        this.listeners[type].push(listener);
        if (type === "message" && this.pending.length > 0) {
          const queued = this.pending.splice(0);
          setTimeout(() => {
            for (const data of queued) for (const handler of this.listeners.message) handler({ data });
          }, 0);
        }
      },
    });
    const left = make();
    const right = make();
    left.peer = right;
    right.peer = left;
    return [left, right];
  };

  const Board = remote("t.board", {
    version: 1,
    input: Type.Object({}),
    variants: {
      main: {
        state: { notes: Type.Array(Type.String()) },
        methods: { add: method(Type.String(), Type.Null()), who: method(Type.Null(), Type.Unknown()) },
      },
    },
  });

  await checkAsync("websocket: acceptWebSockets stamps the authenticated principal and serves state", async () => {
    const server = quietHost("ws-server", []);
    server.serve(Board, (_input, principal, self) => {
      const state = self.state({ notes: cell(Type.Array(Type.String()), []) });
      return {
        kind: "main",
        notes: state.notes,
        add: (text, ctx) => {
          state.notes.change(ctx, (draft) => {
            draft.push(text);
          });
          return null;
        },
        who: () => principal,
      };
    });
    let accept = () => {};
    acceptWebSockets(
      { on: (event, listener) => { if (event === "connection") accept = listener; } },
      server,
      { principal: (request) => (request.token === "ok" ? { guest: "web" } : undefined) },
    );
    const [serverSide, clientSide] = socketPair();
    accept(serverSide, { token: "ok" });
    await ticks(1);
    const browser = createHost({ name: "ws-browser", kind: "client", upstream: webSocketLink(clientSide) });
    const board = browser.get(Board, {}, { at: "ws-server" });
    await board.ready;
    // Upstream always passes an explicit argument: `args: undefined` would be dropped by JSON.stringify and the
    // peer refuses a frame without the required `args` key (the same holds with real typebox).
    assertDeep(await board.who(null), { guest: "web" }, "the socket was stamped by the authenticator");
    const seen = [];
    board.notes.subscribe((value) => seen.push(value));
    await browser.root((ctx) => board.add("note", ctx));
    // Upstream polls: the subscription and the delta need a socket round trip.
    for (let attempt = 0; attempt < 50 && (board.notes.value === undefined || board.notes.value.length !== 1); attempt++) {
      await sleep(5);
    }
    assertDeep(board.notes.value, ["note"], "remote state over the WebSocket link");
    assertDeep(seen[seen.length - 1], ["note"], "the subscription saw the delta");
    board.release();
    await browser.close();
    const [refusedServerSide, refusedClientSide] = socketPair();
    accept(refusedServerSide, { token: "bad" });
    await ticks(1);
    const refused = createHost({ name: "ws-refused", kind: "client", upstream: webSocketLink(refusedClientSide) });
    const refusedBoard = refused.get(Board, {}, { at: "ws-server" });
    await ticks(3);
    assertEqual(refusedBoard.status === "gone" || refusedBoard.status === "loading", true, "an unauthorized socket never goes live: " + refusedBoard.status);
    refusedBoard.release();
    await refused.close();
    await server.close();
  });

  await checkAsync("websocket: large payloads arrive as binary messages that inflateMessage restores", async () => {
    const Big = remote("t.ws-big", {
      version: 1,
      input: Type.Object({}),
      variants: {
        main: {
          state: { notes: Type.Array(Type.String()) },
          methods: { echo: method(Type.Array(Type.String()), Type.Array(Type.String())) },
        },
      },
    });
    const notes = Array.from({ length: 40 }, (_, i) => i + ":" + "x".repeat(4000));
    const server = quietHost("ws-big-server", []);
    server.serve(Big, (_input, _principal, self) => {
      const state = self.state({ notes: cell(Type.Array(Type.String()), notes) });
      return { kind: "main", notes: state.notes, echo: (value) => value };
    });
    let accept = () => {};
    acceptWebSockets(
      { on: (event, listener) => { if (event === "connection") accept = listener; } },
      server,
      { principal: () => ({ guest: "web" }) },
    );
    const [serverSide, clientSide] = socketPair();
    accept(serverSide, { token: "ok" });
    await ticks(1);
    const browser = createHost({ name: "ws-big-browser", kind: "client", upstream: webSocketLink(clientSide) });
    // The recorder attaches after the link, so it sees every message the link does (and the queued hello was
    // flushed to the link's listener when the link attached).
    const binary = [];
    clientSide.addEventListener("message", (event) => {
      if (typeof event.data !== "string") binary.push(event.data);
    });
    const big = browser.get(Big, {}, { at: "ws-big-server" });
    assertDeep(await big.echo(notes), notes, "large args crossed the socket and arrived intact");
    big.notes.subscribe(() => {});
    await until(() => binary.length > 0 && big.notes.value !== undefined);
    assertDeep(big.notes.value, notes, "the large snapshot arrived over the socket");
    assert(binary.length > 0, "the server's large frames arrived as binary messages (compress: true)");
    assertEqual(typeof chordRoot.inflateMessage, "function", "inflateMessage is exported (upstream index.ts)");
    assertEqual(chordRoot.PACK_THRESHOLD, 64 * 1024, "PACK_THRESHOLD is exported (upstream index.ts)");
    const restored = binary.map((message) => JSON.parse(chordRoot.inflateMessage(message)));
    assert(
      restored.some((frame) => frame.t === "result" && deepEqual(frame.value, notes)),
      "the deflated result frame inflates back to its text form",
    );
    assert(
      restored.some((frame) => frame.t === "state" && deepEqual(frame.snap, notes)),
      "the deflated snapshot inflates back to its text form",
    );
    big.release();
    await browser.close();
    await server.close();
  });
}

// ─── packed payloads (upstream 018c87ddc) ───────────────────────────────────

async function packedCases() {
  const Echo = remote("t.pack-echo", {
    version: 1,
    input: Type.Object({}),
    variants: {
      main: {
        methods: {
          size: method(Type.Array(Type.String()), Type.Number()),
          echo: method(Type.Unknown(), Type.Unknown()),
        },
      },
    },
  });
  const bigNotes = () => Array.from({ length: 40 }, (_, i) => i + ":" + "x".repeat(4000));

  await checkAsync("link: a packed call payload from an upstream peer is inflated and served", async () => {
    const server = quietHost("pack-server", []);
    server.serve(Echo, () => ({ kind: "main", size: (notes) => notes.length, echo: (value) => value }));
    const [down, raw] = memoryLinkPair();
    server.accept(down, { principal: "guest", kind: "client" });
    const frames = [];
    raw.receive((frame) => frames.push(frame));
    raw.send({ t: "hello", name: "raw", kind: "client", epoch: 0 });
    raw.send({ t: "hold", id: 1, at: "pack-server", token: Echo.description, input: {} });
    await until(() => frames.some((frame) => frame.t === "shape"));
    const notes = bigNotes();
    // The upstream wire form of a payload above 64 KiB: the payload field is absent and `z: [field, base64 raw
    // deflate]` carries it (018c87ddc). Before this port the shim judged the frame malformed (no `t`-carrying
    // payload field) and closed the link, so a mixed deployment broke on the first large call.
    raw.send({ t: "call", id: 2, hold: 1, member: "size", z: ["args", rawDeflateJson(notes)] });
    await until(() => frames.some((frame) => frame.t === "result" && frame.id === 2));
    const result = frames.find((frame) => frame.t === "result" && frame.id === 2);
    assertEqual(result.value, notes.length, "the packed args were inflated, not refused");
    assert(!raw.closed, "the link stayed open");
    raw.close();
    await server.close();
  });

  await checkAsync("link: payloads above 64 KiB are deflated, at the threshold they are sent as JSON", async () => {
    const server = quietHost("threshold-server", []);
    server.serve(Echo, () => ({ kind: "main", size: (notes) => notes.length, echo: (value) => value }));
    const [down, raw] = memoryLinkPair();
    server.accept(down, { principal: "guest", kind: "client" });
    const frames = [];
    raw.receive((frame) => frames.push(frame));
    raw.send({ t: "hello", name: "raw", kind: "client", epoch: 0 });
    raw.send({ t: "hold", id: 1, at: "threshold-server", token: Echo.description, input: {} });
    await until(() => frames.some((frame) => frame.t === "shape"));
    assertEqual(chordRoot.PACK_THRESHOLD, 64 * 1024, "upstream's PACK_THRESHOLD");
    // The rule counts the payload's own JSON text: `<= PACK_THRESHOLD` is sent as JSON.
    const atThreshold = "y".repeat(64 * 1024 - 2);
    const aboveThreshold = atThreshold + "y";
    assertEqual(JSON.stringify(atThreshold).length, 64 * 1024, "the boundary payload's JSON is exactly the threshold");
    raw.send({ t: "call", id: 2, hold: 1, member: "echo", args: atThreshold });
    raw.send({ t: "call", id: 3, hold: 1, member: "echo", args: aboveThreshold });
    await until(() => frames.some((frame) => frame.t === "result" && frame.id === 3));
    const small = frames.find((frame) => frame.t === "result" && frame.id === 2);
    const large = frames.find((frame) => frame.t === "result" && frame.id === 3);
    assertEqual(small.value, atThreshold, "at the threshold the payload is the plain value");
    assert(
      large.value !== null && typeof large.value === "object" && typeof large.value.bytes === "object",
      "above the threshold the payload arrives packed (the form `unpack` inflates)",
    );
    assertEqual(inflateJson(large.value.bytes), aboveThreshold, "the deflated payload inflates to the sent value");
    // A relay forwards the packed bytes unread: sending the received frame over another link re-encodes them as-is.
    const [relayDown, relayUp] = memoryLinkPair();
    const forwarded = [];
    relayUp.receive((frame) => forwarded.push(frame));
    relayDown.send({ ...large, id: 9 });
    await until(() => forwarded.length > 0);
    assertDeep(
      Array.from(forwarded[0].value.bytes),
      Array.from(large.value.bytes),
      "the forwarded bytes are identical (the relay did not inflate/re-encode)",
    );
    raw.close();
    relayDown.close();
    await server.close();
  });
}

// ─── flow control (upstream db6497c3d) ──────────────────────────────────────

async function flowCases() {
  const Counter = remote("t.flow-counter", {
    version: 1,
    input: Type.Object({}),
    variants: {
      main: {
        state: { count: Type.Object({ n: Type.Number() }) },
        methods: { add: method(Type.Number(), Type.Number()), wait: method(Type.Null(), Type.Null()) },
      },
    },
  });
  /** A client end that can hold back its acknowledgements, recording what arrives (upstream flow.test.ts's AckGate). */
  const ackGate = (inner) => {
    const gate = {
      inner,
      received: [],
      paused: false,
      held: undefined,
      get closed() {
        return inner.closed;
      },
      send(frame) {
        if (frame.t === "ack" && gate.paused) {
          gate.held = frame;
          return 0;
        }
        return inner.send(frame);
      },
      release() {
        gate.paused = false;
        if (gate.held !== undefined) inner.send(gate.held);
        gate.held = undefined;
      },
      receive(handler) {
        inner.receive((frame) => {
          gate.received.push(frame);
          handler(frame);
        });
      },
      onClose(handler) {
        inner.onClose(handler);
      },
      close(reason) {
        inner.close(reason);
      },
    };
    return gate;
  };
  let releaseCalls = () => {};
  const waiting = () =>
    new Promise((resolve) => {
      const previous = releaseCalls;
      releaseCalls = () => {
        previous();
        resolve(null);
      };
    });
  const serveCounter = (host) => {
    host.serve(Counter, (_input, _principal, self) => {
      const state = self.state({ count: cell(Type.Object({ n: Type.Number() }), { n: 0 }) });
      return {
        kind: "main",
        count: state.count,
        add: (delta, ctx) => {
          state.count.change(ctx, (draft) => {
            draft.n += delta;
          });
          return state.count.value.n;
        },
        wait: waiting,
      };
    });
  };
  const client = (server, window) => {
    const [down, up] = memoryLinkPair();
    server.accept(down, { principal: { user: "u" }, window });
    const gate = ackGate(up);
    const host = createHost({ name: "browser-flow", kind: "client", upstream: gate });
    return { host, gate };
  };
  const stateFrames = (frames) => frames.filter((frame) => frame.t === "state");

  await checkAsync("flow: hello announces acks and the peer's processed frames are acknowledged", async () => {
    const server = quietHost("ack-server", []);
    serveCounter(server);
    const [down, raw] = memoryLinkPair();
    server.accept(down, { principal: "guest", kind: "client" });
    const frames = [];
    raw.receive((frame) => frames.push(frame));
    await until(() => frames.some((frame) => frame.t === "hello"));
    const hello = frames.find((frame) => frame.t === "hello");
    assertEqual(hello.acks, true, "the shim's hello announces acks (upstream db6497c3d)");
    raw.send({ t: "hello", name: "raw", kind: "client", epoch: 0, acks: true });
    raw.send({ t: "hold", id: 1, at: "ack-server", token: Counter.description, input: {} });
    await until(() => frames.some((frame) => frame.t === "ack"));
    const acks = frames.filter((frame) => frame.t === "ack").map((frame) => frame.n);
    assert(
      acks[acks.length - 1] >= 2,
      "the cumulative count covers every non-ack frame processed (hello + hold), got " + JSON.stringify(acks),
    );
    for (let i = 1; i < acks.length; i++) assert(acks[i] >= acks[i - 1], "acknowledgements are cumulative");
    // A peer that did not announce `acks` is neither acknowledged nor windowed (the upstream degradation rule).
    const [down2, raw2] = memoryLinkPair();
    server.accept(down2, { principal: "guest", kind: "client" });
    const frames2 = [];
    raw2.receive((frame) => frames2.push(frame));
    raw2.send({ t: "hello", name: "raw2", kind: "client", epoch: 0 });
    raw2.send({ t: "hold", id: 1, at: "ack-server", token: Counter.description, input: {} });
    await until(() => frames2.some((frame) => frame.t === "shape"));
    await sleep(10);
    assertEqual(frames2.filter((frame) => frame.t === "ack").length, 0, "no acks to a peer that did not announce them");
    raw.close();
    raw2.close();
    await server.close();
  });

  await checkAsync("flow: state waits behind the window, calls are answered, one coalesced frame follows", async () => {
    const server = quietHost("flow-server", []);
    serveCounter(server);
    const { host, gate } = client(server, 200);
    const counter = host.get(Counter, {}, { at: "flow-server" });
    const values = [];
    counter.count.subscribe((value) => values.push(value));
    await until(() => values.at(-1) !== undefined);
    gate.paused = true;
    const results = [];
    for (let i = 0; i < 40; i++) results.push(await counter.add(1));
    assertEqual(results.at(-1), 40, "every call was answered while state waited");
    assert(values.at(-1).n < 40, "state waits behind the window, last value " + values.at(-1).n);
    const before = stateFrames(gate.received).length;
    gate.release();
    await until(() => values.at(-1).n === 40);
    assertEqual(stateFrames(gate.received).length - before, 1, "the waiting ops arrive as one coalesced frame");
    await host.close();
    await server.close();
  });

  await checkAsync("flow: calls beyond maxCalls are answered with unavailable/too_many_calls", async () => {
    const server = quietHost("max-server", []);
    serveCounter(server);
    const [down, up] = memoryLinkPair();
    server.accept(down, { principal: { user: "u" }, maxCalls: 2 });
    const host = createHost({ name: "browser-max", kind: "client", upstream: up });
    const counter = host.get(Counter, {}, { at: "max-server" });
    await counter.ready;
    releaseCalls = () => {};
    const first = counter.wait(null);
    const second = counter.wait(null);
    const refused = await assertRejects(
      counter.add(1),
      /Too many calls in progress/,
      "the call beyond maxCalls is refused",
    );
    assert(isChordError(refused, "unavailable"), "unavailable code, got " + String(refused.code));
    assertEqual(refused.reason, "too_many_calls", "the upstream reason");
    releaseCalls();
    await Promise.all([first, second]);
    assertEqual(await counter.add(1), 1, "the link stays open and later calls run");
    await host.close();
    await server.close();
  });
}

// ─── /node: the throws tier ──────────────────────────────────────────────────

async function nodeCases() {
  check("node: every entry point throws ERR_NOT_SUPPORTED", () => {
    for (const [name, entry, args] of [
      ["nodeLoader", nodeLoader, []],
      ["forkDriver", forkDriver, [{}]],
      ["createHostManager", createHostManager, [{}]],
      ["childControl", childControl, []],
    ]) {
      const error = assertThrows(() => entry(...args), /not supported by the pir extension runtime/, name);
      assertEqual(error.code, "ERR_NOT_SUPPORTED", name + " code");
    }
    assertEqual(typeof processLock, "function", "processLock is exported");
  });

  await checkAsync("node: processLock rejects with the unified code", async () => {
    const failure = await assertRejects(processLock("probe"), /not supported by the pir extension runtime/, "processLock");
    assertEqual(failure.code, "ERR_NOT_SUPPORTED", "processLock code");
  });
}

/** Runs every case and returns the report (`{ total, passed, failed }`). */
export async function runChordProbe() {
  results.length = 0;
  deltaCases();
  contextCases();
  jsonCases();
  contractCases();
  await hostCases();
  await stateCases();
  await linkCases();
  await webSocketCases();
  await packedCases();
  await flowCases();
  await nodeCases();
  const failed = results.filter((entry) => !entry.ok).map((entry) => entry.name + " :: " + entry.error);
  return { total: results.length, passed: results.length - failed.length, failed };
}
"####;

/// The extension entry: one tool that runs the probe and answers with its report.
const INDEX_TS: &str = r##"import { runChordProbe } from "./chord_probe.mjs";

export default (pi: any) => {
  pi.registerTool({
    name: "chord_probe",
    description: "Run the chord shim case list and report each case",
    parameters: { type: "object", properties: {} },
    execute: async () => {
      const report = await runChordProbe();
      return { content: [{ type: "text", text: JSON.stringify(report) }] };
    },
  });
};
"##;

/// The export surface every entry must expose (the union of upstream
/// `index.ts`, `context/index.ts` and `delta/index.ts` runtime exports).
const EXPECTED_SURFACE: &[&str] = &[
    "BACKGROUND_CONTEXT",
    "ChordError",
    "PACK_THRESHOLD",
    "PathError",
    "RESERVED_SEGMENTS",
    "TODO_CONTEXT",
    "UnsafePathError",
    "acceptWebSockets",
    "apply",
    "applyImmutable",
    "applyImmutableBatches",
    "assertSafePath",
    "assertValidOp",
    "assertValidWireOp",
    "awaitWithContext",
    "bind",
    "cell",
    "copyJson",
    "coverage",
    "createContextKey",
    "createHost",
    "decoder",
    "defineFacet",
    "defineRegistry",
    "derivation",
    "diffRevisions",
    "encoder",
    "forbidden",
    "hostPrincipal",
    "inflateMessage",
    "invalid",
    "ipcLink",
    "isBase",
    "isChordError",
    "isJsonValue",
    "isReplace",
    "listener",
    "local",
    "memoryLinkPair",
    "method",
    "opaque",
    "overlap",
    "remote",
    "replicatedState",
    "resource",
    "track",
    "webSocketLink",
    "withAbortSignal",
    "withCancel",
    "withContextValue",
    "withoutAbortSignal",
];

/// A second fixture that only touches the entry surfaces and the `/node` throws
/// contract, so a registration or export-list regression names the entry.
const SURFACE_TS: &str = r##"
import * as chord from "@earendil-works/chord";
import * as context from "@earendil-works/chord/context";
import * as delta from "@earendil-works/chord/delta";
import * as node from "@earendil-works/chord/node";

export default (pi: any) => {
  pi.registerTool({
    name: "chord_surface",
    description: "Report the chord entry surfaces and the /node throws contract",
    parameters: { type: "object", properties: {} },
    execute: async () => {
      const report: any = {
        root: Object.keys(chord).sort(),
        context: Object.keys(context).sort(),
        delta: Object.keys(delta).sort(),
        node: Object.keys(node).sort(),
        nodeThrows: {},
      };
      for (const [name, entry] of [
        ["nodeLoader", node.nodeLoader],
        ["forkDriver", node.forkDriver],
        ["createHostManager", node.createHostManager],
        ["childControl", node.childControl],
      ] as const) {
        try {
          (entry as any)({});
          report.nodeThrows[name] = "resolved";
        } catch (error: any) {
          report.nodeThrows[name] = String(error.code) + ":" + String(error.message);
        }
      }
      const lock = node.processLock("probe") as Promise<unknown>;
      report.nodeThrows.processLock = await lock.then(
        () => "resolved",
        (error: any) => String(error.code),
      );
      return { content: [{ type: "text", text: JSON.stringify(report) }] };
    },
  });
};
"##;

/// See tests/node_shims.rs: fixtures must be written at runtime.
fn write_temp_fixture(test_name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pir-chord-{}-{test_name}", std::process::id()));
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

async fn run_probe(test_name: &str, files: &[(&str, &str)], tool: &str) -> Value {
    let dir = write_temp_fixture(test_name, files);
    let rt = ExtensionRuntime::new(test_config_at(&dir))
        .await
        .expect("runtime construction");
    let loaded = rt
        .load(std::slice::from_ref(&dir))
        .await
        .expect("load returns");
    assert!(
        loaded.errors.is_empty(),
        "the chord fixture must link every entry (shim registration + export surface): {:?}",
        loaded.errors
    );
    assert_eq!(loaded.loaded.len(), 1, "one loaded entry: {loaded:?}");
    let outcome = rt
        .call_tool(tool, "call-chord-probe", &json!({}))
        .await
        .expect("tool executes");
    assert_eq!(outcome.content.len(), 1);
    assert_eq!(
        outcome.content[0].kind, "text",
        "tool result shape: {outcome:?}"
    );
    serde_json::from_str(&outcome.content[0].text).expect("probe answers JSON")
}

/// The translated upstream cases: contracts/declarations/host/handles,
/// replicated state, memory links, the whole delta tracker and the `/node`
/// throws contract. A failure names the upstream case it came from.
#[tokio::test(flavor = "current_thread")]
async fn chord_shim_upstream_cases_pass() {
    let report = run_probe(
        "cases",
        &[("index.ts", INDEX_TS), ("chord_probe.mjs", PROBE_MJS)],
        "chord_probe",
    )
    .await;
    let failed = report["failed"].as_array().cloned().unwrap_or_default();
    assert!(
        failed.is_empty(),
        "every chord case must pass, failures: {failed:#?}"
    );
    let total = report["total"].as_u64().unwrap_or_default();
    assert_eq!(
        report["passed"].as_u64().unwrap_or_default(),
        total,
        "passed must equal total: {report}"
    );
    assert_eq!(total, 44, "the case list must not shrink: {report}");
}

/// The registration table: the three portable entries resolve to one module with
/// exactly the upstream export surface; `/node` resolves to the throws tier.
#[tokio::test(flavor = "current_thread")]
async fn chord_shim_entry_surfaces_and_node_throws() {
    let report = run_probe("surface", &[("index.ts", SURFACE_TS)], "chord_surface").await;
    let names = |key: &str| -> Vec<String> {
        report[key]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .map(|value| value.as_str().unwrap_or_default().to_string())
            .collect()
    };
    assert_eq!(names("root"), EXPECTED_SURFACE, "the root entry surface");
    assert_eq!(
        names("context"),
        names("root"),
        "/context is resolved by the same module"
    );
    assert_eq!(
        names("delta"),
        names("root"),
        "/delta is resolved by the same module"
    );
    assert_eq!(
        names("node"),
        vec![
            "childControl".to_string(),
            "createHostManager".to_string(),
            "default".to_string(),
            "forkDriver".to_string(),
            "nodeLoader".to_string(),
            "processLock".to_string(),
        ],
        "the /node entry surface"
    );
    for entry in [
        "nodeLoader",
        "forkDriver",
        "createHostManager",
        "childControl",
    ] {
        let value = report["nodeThrows"][entry].as_str().unwrap_or_default();
        assert!(
            value.starts_with("ERR_NOT_SUPPORTED:")
                && value.contains("is not supported by the pir extension runtime"),
            "/node.{entry} must throw the unified unsupported error: {value}"
        );
    }
    assert_eq!(
        report["nodeThrows"]["processLock"]
            .as_str()
            .unwrap_or_default(),
        "ERR_NOT_SUPPORTED",
        "/node.processLock must reject with the unified code"
    );
}
