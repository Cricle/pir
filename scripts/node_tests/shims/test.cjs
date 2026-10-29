// Minimal `node:test` implementation for the pir Node-test-suite harness.
//
// Purpose: let vendored Node lib tests that do `require('node:test')` run
// under `pir js` (and system node) with real pass/fail accounting. This is
// harness infrastructure — it lives in the runner's shims/ directory and is
// never part of the compatibility surface being measured.
//
// Supported: test/it/describe/suite (callback + promise styles), subtests
// (test() inside a running test), top-level hooks, skip / todo options,
// TestContext (t.test, t.skip, t.diagnostic, t.name, t.beforeEach /
// t.afterEach). Not supported (unused by the vendored subset): mock,
// snapshot, reporters, TAP output, concurrency.
//
// End-of-run contract: prints one marker line
//   __NODERESULT__ {"pass":N,"fail":N,"skipped":N,"todo":N}
// on stdout and sets process.exitCode to 1 if any test failed. The runner
// parses that marker; plain-script failures surface as nonzero exit codes.

'use strict';

const assert = require('node:assert');

// ---------------------------------------------------------------------------
// bookkeeping
// ---------------------------------------------------------------------------
const results = { pass: 0, fail: 0, skipped: 0, todo: 0 };
const failures = [];
let runStarted = false;

function scheduleRun() {
  if (runStarted) return;
  runStarted = true;
  // Defer past the end of synchronous registration (the main module body),
  // like node:test which runs the suite after the main module finishes.
  const schedule = typeof setImmediate === 'function'
    ? setImmediate
    : (f) => setTimeout(f, 0);
  schedule(() => { runSuite(rootSuite).then(finish, finish); });
}

async function runSuite(suite) {
  for (const hook of suite.before) await invokeHook(hook, '(before)');
  for (const node of suite.children) {
    if (node.kind === 'suite') {
      await runNode(node);
      continue;
    }
    for (const hook of suite.beforeEach) await invokeHook(hook, `(beforeEach) ${node.name}`);
    await runNode(node);
    for (const hook of suite.afterEach) await invokeHook(hook, `(afterEach) ${node.name}`);
  }
  for (const hook of suite.after) await invokeHook(hook, '(after)');
}

async function invokeHook(hook, name) {
  try {
    await hook();
  } catch (error) {
    results.fail += 1;
    failures.push({ name, error });
  }
}

async function runNode(node) {
  if (node.kind === 'suite') {
    const skip = skipReason(node.opts);
    if (skip) {
      results.skipped += 1;
      emitStatus('SKIP', node.path, skip);
      return;
    }
    await runSuite(node.suite);
    return;
  }

  const skip = skipReason(node.opts);
  if (skip) {
    if (skip === 'todo') results.todo += 1; else results.skipped += 1;
    emitStatus('SKIP', node.path, skip);
    return;
  }

  const ctx = makeContext(node);
  const t0 = Date.now();
  try {
    const { fn } = node;
    if (fn.length >= 1) {
      // node:test callback style: fn(t, done) — done(err) marks failure.
      await new Promise((resolve, reject) => {
        let settled = false;
        const done = (err) => {
          if (!settled) { settled = true; err ? reject(err) : resolve(); }
        };
        let r;
        try { r = fn(ctx, done); } catch (error) { settled = true; reject(error); return; }
        if (r && typeof r.then === 'function') {
          r.then(() => { if (!settled) { settled = true; resolve(); } }, reject);
        }
      });
    } else {
      await fn(ctx);
    }
    results.pass += 1;
    emitStatus('PASS', node.path, null, Date.now() - t0);
  } catch (error) {
    if (error && error.__nodeTestSkip) {
      results.skipped += 1;
      emitStatus('SKIP', node.path, error.message);
      return;
    }
    results.fail += 1;
    failures.push({ name: node.path, error });
    emitStatus('FAIL', node.path, error, Date.now() - t0);
  }
}

function skipReason(opts) {
  if (!opts) return null;
  if (opts.todo) return 'todo';
  if (opts.skip) {
    if (typeof opts.skip === 'string') return 'skip: ' + opts.skip;
    if (typeof opts.skip === 'function') return 'skip: (predicate)';
    return 'skip';
  }
  return null;
}

function emitStatus(status, name, detail, ms) {
  const tail = ms !== undefined ? ` (${ms}ms)` : '';
  if (status === 'PASS') console.log(`ok 1 - ${name}${tail}`);
  else if (status === 'SKIP') console.log(`ok 1 - # SKIP ${name} — ${detail}`);
  else console.log(`not ok 1 - ${name}`);
}

// ---------------------------------------------------------------------------
// context
// ---------------------------------------------------------------------------
function makeContext(node) {
  const suite = node.kind === 'suite' ? node.suite : node.parent;
  return {
    name: node.name,
    signal: undefined,
    diagnostic(...args) { console.log('#', args.map(fmt).join(' ')); },
    skip(message) {
      const e = new Error(message || 'skipped');
      e.__nodeTestSkip = true;
      throw e;
    },
    test: (name, opts, fn) => {
      const sub = makeNode('test', name, opts, fn, node.path);
      suite.children.push(sub);
      scheduleRun();
      return { then(onF, onR) { return Promise.resolve().then(onF, onR); } };
    },
    beforeEach: (fn) => { if (suite) suite.beforeEach.push(fn); },
    afterEach: (fn) => { if (suite) suite.afterEach.push(fn); },
  };
}

function fmt(v) {
  if (typeof v === 'string') return v;
  try { return JSON.stringify(v); } catch { return String(v); }
}

// ---------------------------------------------------------------------------
// registration
// ---------------------------------------------------------------------------
const rootSuite = { kind: 'suite', name: '(root)', path: '', opts: {}, children: [], before: [], after: [], beforeEach: [], afterEach: [] };
let currentSuite = rootSuite;
let currentPath = '';

function makeNode(kind, name, opts, fn, parentPath) {
  let n = 'unnamed';
  let o = {};
  let f = null;
  for (const arg of [name, opts, fn]) {
    if (typeof arg === 'string') n = arg;
    else if (typeof arg === 'function') f = arg;
    else if (arg && typeof arg === 'object') o = arg;
  }
  const path = parentPath ? `${parentPath} > ${n}` : n;
  const node = { kind, name: n, path, opts: o, fn: f, parent: currentSuite, suite: null };
  if (kind === 'suite') {
    node.suite = { kind: 'suite', name: n, path, opts: o, children: [], before: [], after: [], beforeEach: [], afterEach: [] };
  }
  return node;
}

function register(kind, name, opts, fn) {
  const node = makeNode(kind, name, opts, fn, currentPath);
  currentSuite.children.push(node);
  scheduleRun();
  if (kind === 'suite' && typeof node.fn === 'function') {
    // Suite body function: nested test() calls must register into this suite.
    const prevSuite = currentSuite;
    const prevPath = currentPath;
    currentSuite = node.suite;
    currentPath = node.path;
    const r = node.fn();
    currentSuite = prevSuite;
    currentPath = prevPath;
    if (r && typeof r.then === 'function') return r;
  }
  return { then(onF, onR) { return Promise.resolve().then(onF, onR); } };
}

// ---------------------------------------------------------------------------
// public surface
// ---------------------------------------------------------------------------
function test(name, opts, fn) {
  return register('test', name, opts, fn);
}
const it = test;

test.skip = (name, opts, fn) => test(name, { ...(opts && typeof opts === 'object' ? opts : {}), skip: true }, fn);
test.todo = (name, opts, fn) => test(name, { ...(opts && typeof opts === 'object' ? opts : {}), todo: true }, fn);
test.only = test; // no isolation in the harness
test.describe = describe;
test.it = it;
test.run = () => Promise.resolve();
test.mock = undefined;

function describe(name, opts, fn) {
  return register('suite', name, opts, fn);
}
describe.skip = (name, opts, fn) => describe(name, { ...(opts && typeof opts === 'object' ? opts : {}), skip: true }, fn);
describe.todo = (name, opts, fn) => describe(name, { ...(opts && typeof opts === 'object' ? opts : {}), todo: true }, fn);
describe.only = describe;

function before(fn) { rootSuite.before.push(fn); }
function after(fn) { rootSuite.after.push(fn); }
function beforeEach(fn) { currentSuite.beforeEach.push(fn); }
function afterEach(fn) { currentSuite.afterEach.push(fn); }

function run() { return Promise.resolve(); }

function finish() {
  console.log('__NODERESULT__ ' + JSON.stringify({
    pass: results.pass,
    fail: results.fail,
    skipped: results.skipped,
    todo: results.todo,
  }));
  if (failures.length) {
    console.error(`node:test shim: ${failures.length} failing test(s):`);
    for (const f of failures) {
      const stack = f.error && f.error.stack ? String(f.error.stack).split('\n').slice(0, 6).join('\n') : String(f.error);
      console.error(`--- ${f.name}\n${stack}`);
    }
  }
  if (results.fail > 0) process.exitCode = 1;
}

module.exports = test;
module.exports.default = test;
module.exports.test = test;
module.exports.it = it;
module.exports.describe = describe;
module.exports.suite = describe;
module.exports.before = before;
module.exports.after = after;
module.exports.beforeEach = beforeEach;
module.exports.afterEach = afterEach;
module.exports.run = run;
module.exports.assert = assert;
