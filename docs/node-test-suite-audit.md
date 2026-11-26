# Node.js test-suite audit for pir's Node surface

Date: 2026-11-20
Auditor: research agent (tree treated read-only; the original audit wrote only this file)
Runtime under test: `C:\wp\empty\_pir_node_wt\target\debug\pir.exe` (`pir js`, claims `v24.19.0`)
Reference runtime: `C:\wp\empty\tools\node-v24.22.1-win-x64\node.exe` (v24.22.1)
Corpus: nodejs/node tag `v24.22.1`, sparse clone at `C:\wp\empty\_scratch\node-suite-probe\node`

> **Historical snapshot.** Sections 6.1-6.3 and the blocker/tier counts are measurements of the earlier
> 12:08/13:06/13:08/15:08 binaries. Section 6.4 is the pinned final run of the 15:08 debug binary
> (`sha256 ed86f4…`). Shim sources and AOT blobs were regenerated later (20:36-22:36), so the current
> worktree can be newer than these recorded verdicts; notes below flag the places where that is known to
> matter. Numbers are left as recorded rather than rewritten.

## Verdict

**Partially practical, and clearly worth doing.** The suite is a usable completeness/correctness instrument for the public, in-process part of pir's Node surface, but not for the whole suite.

- 99.9% of `test/parallel/*.js` require `test/common`, and Node's real `test/common/index.js` loads under pir after a **3-line shim** (`process.config.variables` default). It does not need `internalBinding`, `primordials`, or `process.binding` on the default path.
- With that shim plus `NODE_TEST_KNOWN_GLOBALS=0`, a curated 58-test pure-API sample was **100% valid on Node (58/58 pass)** and on pir produced **25 pass, 12 assertion failures, 14 other errors, 6 unsupported, 1 timeout**. 57/58 (98%) produced a meaningful verdict.
- A 438-test "pure public API, no flags, no child/worker/internal" subset produced **174 pass (39.7%), 241 fail, 7 explicitly unsupported, 6 skip, 10 timeout**; **422/438 (96.3%) produced a meaningful verdict**. Numbers were stable across two in-flight builds (175 vs 174 pass).
- A random 298-file sample of all of `test/parallel` (includes http/net/crypto/worker/CLI) gave **41 pass (13.8%), 80 skip (26.8%), 23 assert, 92 error, 29 unsupported, 33 timeout**. The lower rate reflects out-of-scope protocol/native/harness features, not the pure API layer.
- Heuristic partition of all 4,183 `test/parallel/*.js`: **Tier A 5 (0.1%), Tier B 2,640 (63.1%), Tier C 1,027 (24.5%), Tier D 511 (12.2%)**. Additionally, 456 `.mjs` tests were blocked at audit time by an ESM resolver boundary; the shipped harness now stages a `test/package.json` boundary in the copied work root (`tools/node-suite/runner.py::prepare_work_root`), which addresses the parent-relative import case and would move most of them into Tier B/C.

Practical target: **~2,300-2,600 of 4,183 parallel tests (55-62%) are runnable with a vendored `common` shim**, and every failure is a real measurement of a surface gap. Do not expect a clean "Node passes 100%" number; use the suite as a coverage/gap ledger with a baseline allowlist.

## 1. Method and caveats

Commands actually run (all numbers below come from these shapes):

```sh
git clone --depth 1 --filter=blob:none --sparse --branch v24.22.1 \
  https://github.com/nodejs/node C:/wp/empty/_scratch/node-suite-probe/node
git -C C:/wp/empty/_scratch/node-suite-probe/node sparse-checkout set test tools/doc

# one test, from the prepared test root:
PIR_EXT_BYTECODE=0 PIR_AGENT_DIR=<scratch>/agent NODE_TEST_KNOWN_GLOBALS=0 \
  TEST_THREAD_ID=0 timeout -k 2 6 \
  C:/wp/empty/_pir_node_wt/target/debug/pir.exe js parallel/<test>.js

# reference validation of the same copy:
node parallel/<test>.js
```

Caveats:

- The pir binary was being rebuilt during the audit (mtime 12:08, then 13:06, then 13:08; it changed again to 13:14 after the measured runs). The curated 58 and the 438 subset were re-run on the 13:06/13:08 builds; the 298-file sample is from the 12:08 build. The 438 result moved by one test (175 -> 174 pass), so the numbers are stable, but any harness must record binary mtime/hash per run.
- Suite is v24.22.1; pir reports `v24.19.0`. Version-gated tests may legitimately differ.
- Windows 10 x64 only. Node's runner normally runs from the repo root; the prototype ran from a copied `test`-shaped root (`work/`) with `parallel/`, `common/`, `fixtures/`. Only 3 tests reference literal `test/fixtures`; 43 use `process.cwd()`, so this is a minor difference.
- `pir js` accepts a file and args, but **no Node CLI flags**. Any test with a non-trivial `// Flags:` header is not directly runnable (Tier C) unless the flag is ignorable (`--no-warnings`, `--pending-deprecation`).
- No `cargo` was run. The debug binary is 328 MB and slower to start than a release build.

## 2. Suite layout and what a typical test needs

| Path | Size | Role |
|---|---|---|
| `test/parallel/` | 4,641 files (4,183 `.js`, 456 `.mjs`, 24 MB) | Main in-process suite; parallel-safe |
| `test/sequential/` | 112 `.js` + 9 `.mjs` | Subprocess/CLI/ordering tests; mostly Tier C/D |
| `test/pummel/` | 64 `.js` | Stress/long-running; 6x timeout |
| `test/internet/` | 38 `.js` | Needs real network; out of scope |
| `test/fixtures/` | 51 MB, 290 entries | Test data; `common/fixtures` resolves `../fixtures` |
| `test/common/` | 50 modules, 389 KB | Harness: assertions, tmpdir, fixtures, spawn helpers, inspector helpers |
| `test/common/index.js` | 1,103 lines | The gatekeeper every test loads |
| `test/testpy/` (+ `tools/test.py`, 1,867 lines) | Python runner | Parses `// Flags:`/`// Env:`, status files, sharding, timeouts |
| `test/*.status`, `parallel/parallel.status` | status files | Skip/flaky lists; e.g. shadow-realm tests are SKIP |

A typical parallel test:

```js
'use strict';
const common = require('../common');        // 4,178 / 4,183 .js files
const assert = require('assert');
const fixtures = require('../common/fixtures'); // 720 tests
const tmpdir = require('../common/tmpdir');     // 478 tests
tmpdir.refresh();
fs.readFile(fixtures.path('x.txt'), common.mustCall(...));
```

Requirements, in frequency order (textual occurrence counts over `test/parallel/*.js`):

- `common.mustCall` 9,927 refs, `mustNotCall` 1,653, `skip` 1,397, `hasCrypto` 894, `mustSucceed` 685, `mustCallAtLeast` 394, `expectsError` 225, `platformTimeout` 144, `spawnPromisified` 102, `expectWarning` 70.
- `// Flags:` header in 653 files (15.6%): `--expose-internals` 365, `--no-warnings` 78, `--expose-gc` 65, `--permission` 46, `--allow-fs-read=*` 45, `--experimental-stream-iter` 44, `--experimental-vm-modules` 26.
- `node:test` (`describe/it/test`) in 185 files (4.4%).
- `worker_threads` in 343, `child_process` in 587, `process.execPath` in 475.
- `require('internal/...')` in 365; 454 `.mjs` tests import `../common/index.mjs`.

`test/common/index.js` top-level dependencies: `assert`, `fs`, `net`, `path`, `util` (`inspect`, `getCallSites`), `worker_threads.isMainThread`, `buffer.atob/btoa`, `./tmpdir`, `process.config.variables`, `process.umask`, and `net.getDefaultAutoSelectFamilyAttemptTimeout`. `os`, `child_process`, `async_hooks`, and `internal/test/binding` are required lazily or behind `NODE_TEST_WITH_ASYNC_HOOKS`. **There is no default dependency on `internalBinding`, `primordials`, or `process.binding`.**

The Python runner invokes `node <flags> test/<section>/<file>.js` with `TEST_SERIAL_ID`, `TEST_THREAD_ID`, `TEST_PARALLEL`, and `NODE_SKIP_FLAG_CHECK`; default timeout is 120 s (scaled for debug/ARM; 6x for pummel). It uses the `.status` files for skips/flakes and prints/parses TAP only where tests use `node:test`.

## 3. Blocker counts (verified on the clone)

All counts over `test/parallel`, non-exclusive unless noted:

| Blocker | Files | Notes |
|---|---|---|
| `require('../common')` | 4,178 / 4,183 `.js` | 5 exceptions: 3 pass as-is, 2 fail |
| `require('internal/...')` | 365 | Tier D for surface measurement |
| `--expose-internals` flag | 382 | overlaps internal |
| `node:test` (`describe/it/test`) | 185 | Tier C at audit time (the 15:08 binary's shim stub threw "no test-runner host"); the current shim is a real in-process runner |
| `worker_threads` | 343 | Tier C until workers are complete |
| `child_process` / `process.execPath` | 587 / 475 | Tier C: self-spawn is not node-compatible |
| `// Flags:` header | 653 (15.6%) | `pir js` has no flag parser |
| test name `*inspector*` | 82 | Tier D (inspector disabled) |
| test name `test-v8-*` | 23 | Tier D (V8 internals/JIT) |
| addon/napi/wasi/sea names | 4/2/21 | Tier D (native/out-of-scope) |
| `.mjs` files | 456 | ESM works; at audit time parent-relative imports failed (below). The current harness stages the `test/package.json` boundary |

## 4. Tiers (first-match partition of 4,183 `.js`)

| Tier | Count | % | Definition |
|---|---:|---:|---|
| A | 5 | 0.1% | Runs under stock `pir js` with no shim: the 5 tests that do not require `../common`. 3 pass (`test-global-console-exists`, `test-regression-object-prototype`, `test-zlib-no-stream`), 2 fail at audit time (`test-sqlite-config`: the audit binary had no sqlite — `node:sqlite` is now a real `DatabaseSync` shim; `test-vm-not-strict`: sloppy-mode semantics). |
| B | 2,640 | 63.1% | Needs only the vendored `common` shim (3-line config patch + `NODE_TEST_KNOWN_GLOBALS=0`). Of these, 2,339 use only `common/index`, `fixtures`, `tmpdir`, `countdown`, `arraystream`; 301 also require `common/crypto` (154), `common/repl` (39), `common/debugger` (23), `common/dns` (16), etc. |
| C | 1,027 | 24.5% | Needs a missing harness/runtime feature: `node:test` 159, worker_threads 286, child_process/execPath 468, non-trivial flags 114 (categories overlap). |
| D | 511 | 12.2% | Not applicable to this surface: internal/`--expose-internals` 382, inspector 81, addons/platform (napi/wasi/sea) 25, `test-v8-*` 23. |

These are audit-time category sums and do not partition disjointly. The current `tools/node-suite/tiers.py`
disjoint rules produce different B/C/D totals (`tools/node-suite/README.md` documents the current partition).

Plus **456 `.mjs`** outside the `.js` partition: ESM itself works (`import`/`export`, `common/index.mjs` imports fine from a sibling), but at audit time `import '../common/index.mjs'` from `parallel/` failed with `Error loading module ...: module source not found` (a 3-file probe: parent import fails, same-directory import succeeds). This was a module-boundary issue, not an ESM limitation; the current harness stages `<work>/test/package.json` so the copied tree is the entry package (`tools/node-suite/runner.py:228-237`), which moves those imports back inside the granted root.

"Tier B" means **runnable**, not **passing**. Most Tier B failures are the measurement signal (missing APIs, validation, error codes). Empirically, 438 pure-API Tier B tests were 40% pass / 55% fail / 5% other.

## 5. The `common` shim

Do **not** reimplement Node's common: vendor `test/common` (Node is MIT) and patch the first block of `common/index.js`:

```js
'use strict';
const process = globalThis.process;
// pir: Node always exposes process.config.variables.
if (!process.config) process.config = {};
if (!process.config.variables) process.config.variables = {};
if (!process.config.target_defaults) process.config.target_defaults = {};
```

That was the only change needed for `common/index.js` to load at audit time (it is now a harmless compatibility default; the stub publishes `process.config`). `process.umask(0o022)`, `buffer.atob/btoa`, `util.getCallSites`, `worker_threads.isMainThread`, `net.getDefaultAutoSelectFamilyAttemptTimeout`, `tmpdir`, and `fixtures` all work.

Mandatory environment: **`NODE_TEST_KNOWN_GLOBALS=0`**. pir leaks ~150 `__pir*`/host globals onto `globalThis`. With the default check enabled, `common`'s exit handler throws on the leak *before* `mustCall` accounting runs, which silently turns missing-call failures into exit code 0. Verified: a test that never calls a `common.mustCall` wrapper exits 0 without the env var and exits 1 with it.

Other shim/runtime values worth setting deliberately:

- At audit time (`process.versions` was the 15:08 binary's `{ node: '24.19.0' }`) `openssl`/`sqlite` were absent, so `common.hasCrypto` was false and **58 of 298** sampled tests skipped with `1..0 # Skipped: missing crypto`. The current stub (`crates/pir-core/src/extensions/api/js/process_global_stub.js.txt:137-151`) reports `openssl`, `sqlite`, `uv`, `zlib` and the rest, so a rebuilt binary no longer skips for that reason; the recorded run still shows its one missing-crypto skip.
- `process.config.variables.v8_enable_i18n_support` is still `0` in the current stub (no locale data beyond the segmenter/format pads), so `hasIntl` is false and Intl tests skip truthfully; `node_shared`/`asan` are falsey. Same trade-off.
- `process.execPath` points at `pir.exe`, where `<exe> file.js` launches the agent, not `pir js file.js`. 475 tests reference `process.execPath` and many re-spawn themselves; those hang or fail. Tier C until a node-style launcher exists (or the harness substitutes a wrapper and records that as a deviation).
- `process.argv[1]`/`require.main` are correct when the test file itself is passed to `pir js`; do not use a boot/require wrapper (it breaks `getCallSites`/`__filename` tests).
- `tmpdir` works: it uses `TEST_SERIAL_ID`/`TEST_THREAD_ID` and `NODE_TEST_DIR`; give each parallel worker a distinct id/dir.
- NODE_TEST_KNOWN_GLOBALS aside, the exports that matter (with occurrence counts): `mustCall` (9,927), `mustNotCall` (1,653), `skip` (1,397), `hasCrypto` (894), `mustSucceed` (685), `mustCallAtLeast` (394), `isWindows` (295), `expectsError` (225), `skipIfInspectorDisabled` (188), `platformTimeout` (144), `printSkipMessage` (143), `mustNotMutateObjectDeep` (120), `spawnPromisified` (102), `invalidArgTypeHelper` (100), `expectWarning` (70), `PIPE` (68), `hasIntl` (56), `getArrayBufferViews` (21), `canCreateSymLink` (14), `runWithInvalidFD` (13), `allowGlobals` (13), `pwdCommand` (10), `childShouldThrowAndAbort` (10), `isAlive` (6), `nodeProcessAborted` (5), `getBufferSources` (5).

Recommendation: ship the vendored common plus a small `overlay/` patch, not a 30-function reimplementation. The real semantics of `mustCall` (exit-time accounting, `mustNotCall`, warning capture) are subtle and are exactly what makes the tests meaningful.

## 6. Prototype results

### 6.1 Curated 58-test API sample (current build, 13:08)

Selection: small files from `test/parallel` matching `test-(fs|path|util|events|assert|querystring|url|buffer|string-decoder|stream|zlib)-*`, no worker/inspector/internal/child-process, mostly no flags. Each file was copied to a `test`-shaped work root with `common/` and `fixtures/`, then run directly.

Node v24.22.1 on the same 58 files: **58 pass, 0 fail**.

| Result | Count | Share | Meaning |
|---|---:|---:|---|
| PASS | 25 | 43.1% | Behavior matches Node |
| ASSERT | 12 | 20.7% | Wrong value/identity/validation |
| ERROR | 14 | 24.1% | TypeError/RangeError, missing method, internal-module miss |
| UNSUPPORTED | 6 | 10.3% | `node:test`, `vm.SourceTextModule` |
| TIMEOUT | 1 | 1.7% | `url.parse` test re-spawns `process.execPath` |
| **Meaningful verdict** | **57** | **98.3%** | Everything except the self-spawn timeout |

Median per-test wall time: **129 ms**; total for all 58 including the one 30 s timeout: 40 s (about 10 s excluding it). This is debug-build startup on Windows.

Pass/fail by module family in the 438-test subset (not the 58; see 6.3): fs 48/164, stream 69/145, zlib 20/48, buffer 22/47, path 6/11, util 5/8, events 1/5, assert 1/4, querystring 2/2, string_decoder 1/2, url 0/2. querystring and path are the strongest; url, events, assert, fs are the weakest.

### 6.2 Random 298-file sample of all `test/parallel` (build 12:08)

Every 14th file, 6 s timeout. Includes http/net/crypto/worker/CLI tests and ignores flag headers, so this is a lower bound for "whole suite usefulness", not for API coverage.

| Result | Count | Share |
|---|---:|---:|
| PASS | 41 | 13.8% |
| SKIP | 80 | 26.8% |
| ASSERT | 23 | 7.7% |
| ERROR | 92 | 30.9% |
| UNSUPPORTED | 29 | 9.7% |
| TIMEOUT | 33 | 11.1% |

Of the 80 skips, **58 are `missing crypto`** caused solely by `process.versions.openssl` being absent; 12 are the (truthful) `V8 inspector is disabled`. Timed-out tests are dominated by self-spawning child-process tests.

### 6.3 Pure-API subset, 438 tests (current build)

Selection: the 717 files matching the 11 public API prefixes, minus internal/`--expose-internals`, `node:test`, worker_threads, child_process/execPath, cluster/dgram/net/http/tls/http2, inspector, addons, and non-trivial flags. This is the cleanest estimate of "public JS API coverage with a common shim".

| Result | Count | Share |
|---|---:|---:|
| PASS | 174 | 39.7% |
| ASSERT | 70 | 16.0% |
| ERROR | 171 | 39.0% |
| UNSUPPORTED | 7 | 1.6% |
| SKIP | 6 | 1.4% |
| TIMEOUT | 10 | 2.3% |
| **Meaningful verdict** | **422** | **96.3%** |

Failure taxonomy (258 non-pass, non-skip outcomes): assertion/behavior mismatch 99, missing validation or rejection 54, event/warning never delivered (`mustCall`) 47, missing API method 39, range/limit semantics 10, explicitly unsupported 7, encoding semantics 2.

### 6.4 Re-run on 2026-11-20 15:08:39 +08 / ed86f49a3386

Run time: 2026-11-20 17:23 +08. Runtime: `C:\wp\empty\_pir_node_wt\target\debug\pir.exe`, size 328,803,840 bytes, mtime `2026-11-20T15:08:39.512208+08:00`, sha256 `ed86f49a3386c63557a9d7cea65f899d993b06e2874526f400568284dde360ee`, reports `v24.19.0`. Corpus: nodejs/node `v24.22.1` (`ce7aaf1cee78b8cf0ce17364260535c418d3ed26`). Harness: `tools/node-suite/runner.py` + `tools/node-suite/baseline/tier-b-api.txt` (438 tests), 8 s per-test timeout, 5 jobs, one process per test, per-test `TEST_SERIAL_ID`/`NODE_TEST_DIR`/`PIR_AGENT_DIR`. Raw run: `C:\wp\empty\_scratch\ns-agent\runs\20261120-cabi-aot-final3`; baseline: `tools/node-suite/baseline/known-failures.json`.

#### Why these numbers use embedded shims, not live source shims

The instructed probe environment is `PIR_EXT_BYTECODE=0 PIR_EXT_SHIM_SRC=1`. During this audit another process was editing `crates/pir-core/src/extensions/api/js/*.txt` continuously (newest mtime changed every 10-20 s between 16:36 and 17:02). Source-shim runs therefore did not describe a single snapshot: the same binary produced different verdicts between runs, and one run had all 438 tests fail at boot while `fs_shim.js` was mid-edit (`fs.realpathSync` not a function). The harness now hashes the shim tree before and after every run and records `stable: false` if it changed.

The primary numbers below therefore use the binary's **embedded bytecode shims** (`PIR_EXT_SHIM_SRC=0 PIR_EXT_BYTECODE=1`), which are pinned to the recorded binary. They were stable across six runs: 178 / 176 / 178 / 178 / 177 / 178 PASS; everything else was identical except three `fs.watch` tests (see Stability). Re-run with source shims once that tree is quiet; the harness will record the tree hash and the numbers will attach to it.

#### Overall counts

| Result | Count | Share | Meaning |
|---|---:|---:|---|
| PASS | 178 | 40.6% | behavior matches Node |
| ASSERT | 112 | 25.6% | wrong value, identity, or missing expected throw |
| ERROR | 124 | 28.3% | TypeError/RangeError, missing method, unhandled rejection |
| UNSUPPORTED | 8 | 1.8% | `... is not supported by the pir extension runtime` |
| TIMEOUT | 10 | 2.3% | did not finish in 8 s; process tree killed |
| SKIP | 6 | 1.4% | platform skip (5) or missing crypto (1) |
| **Meaningful verdict** | **422** | **96.3%** | PASS + ASSERT + ERROR + UNSUPPORTED |

The 254 non-pass outcomes are 112 assertions, 124 errors, 8 explicitly unsupported APIs, and 10 hangs. The previous 13:06/13:08-binary audit recorded 174 PASS / 70 ASSERT / 171 ERROR / 7 UNSUPPORTED / 6 SKIP / 10 TIMEOUT; PASS is up by 4 after reclassification and binary changes, while the ASSERT/ERROR split moved because the harness now looks for `AssertionError` anywhere in the output instead of only the first matching line. The old PASS set is a strict subset of the new one: no test that passed on the old build fails on this one.

#### Per-module results

| Module | Total | PASS | ASSERT | ERROR | UNSUP | TIMEOUT | SKIP | Pass rate |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| fs | 164 | 50 | 47 | 50 | 6 | 6 | 5 | 30.5% |
| stream | 145 | 69 | 23 | 48 | 2 | 3 | 0 | 47.6% |
| zlib | 48 | 20 | 12 | 15 | 0 | 0 | 1 | 41.7% |
| buffer | 47 | 23 | 19 | 5 | 0 | 0 | 0 | 48.9% |
| path | 11 | 6 | 5 | 0 | 0 | 0 | 0 | 54.5% |
| util | 8 | 4 | 2 | 1 | 0 | 1 | 0 | 50.0% |
| events | 5 | 2 | 0 | 3 | 0 | 0 | 0 | 40.0% |
| assert | 4 | 1 | 1 | 2 | 0 | 0 | 0 | 25.0% |
| querystring | 2 | 2 | 0 | 0 | 0 | 0 | 0 | 100.0% |
| string_decoder | 2 | 1 | 1 | 0 | 0 | 0 | 0 | 50.0% |
| url | 2 | 0 | 2 | 0 | 0 | 0 | 0 | 0.0% |

`querystring` is the only fully passing family. `fs` is the weakest large family (50/164, 30.5%), followed by `zlib` (41.7%), `stream` (47.6%), and `buffer` (48.9%). `url` is 0/2 on validation and a missing `revokeObjectURL`.

#### Cost and stability

Per-test wall time over the six runs: median 46 ms, p90 109 ms, p99 9.1 s, max 10.4 s. In the final run: median 47 ms, p90 125 ms; 438 tests cost 155.0 s of summed process time and 33.8 s wall at 5-way concurrency including the 10 eight-second timeouts. Debug-build startup dominates the median.

Six embedded-shim runs produced 178 PASS in four runs; the other two had 176 and 177 PASS, entirely accounted for by the flaky `fs.watch` tests below. The only per-test flips were `test-fs-watch-recursive-add-file.js`, `test-fs-watch-recursive-add-file-to-existing-subfolder.js`, and `test-fs-watch-recursive-add-file-to-new-folder.js`; each passed in 5/6 runs and failed once with a filesystem notification error. All other 435 tests were stable across all six runs.

#### Failure Top-15

Clusters below are disjoint and cover 217 of the 254 failures. "Count" is the number of tests in that cluster; the representative test is the first in the run.

| # | Count | Attribution | Representative test | Primary shim |
|---:|---:|---|---|---|
| 1 | 33 | fs value/semantic mismatch | `test-fs-append-file-sync.js` | `fs_shim.js` |
| 2 | 33 | stream value/semantic mismatch | `test-stream-catch-rejections.js` | `stream_shim.js` |
| 3 | 30 | fs argument/type validation missing | `test-fs-append-file.js` | `fs_shim.js` |
| 4 | 20 | fs promise/FileHandle behavior | `test-fs-promises-appendfile.js` | `fs_promises_shim.js` |
| 5 | 17 | buffer value/encoding mismatch | `test-buffer-arraybuffer.js` | `buffer_global.js` |
| 6 | 16 | stream callback/event never delivered | `test-stream-duplex-end.js` | `stream_shim.js` |
| 7 | 14 | stream missing helper/API | `test-stream-filter.js` | `stream_shim.js` |
| 8 | 10 | zlib missing method | `test-zlib-brotli-flush.js` | `zlib_shim.js` |
| 9 | 9 | zlib validation/error semantics | `test-zlib-brotli-dictionary.js` | `zlib_shim.js` |
| 10 | 7 | zlib value/semantic mismatch | `test-zlib-brotli-16GB.js` | `zlib_shim.js` |
| 11 | 6 | fs API explicitly unsupported | `test-fs-chown-negative-one.js` | `fs_shim.js` |
| 12 | 6 | fs watch/read-stream hang | `test-fs-read-stream-fd-leak.js` | `fs_shim.js` |
| 13 | 6 | fs stream constructor without `new` | `test-fs-stream-construct-compat-error-read.js` | `fs_shim.js` |
| 14 | 5 | fs Windows symlink privilege | `test-fs-lchown-negative-one.js` | `fs_shim.js` |
| 15 | 5 | path export identity/semantics | `test-path-dirname.js` | `path_shim.js` |

The remaining 37 failures are smaller clusters: buffer validation/size limits (4), stream export identity (4), stream constructor/property behavior (4), assert CallTracker (3), buffer deprecation warnings (3), events EventTarget/AbortSignal (3), fs callback delivery (3), stream iterator/pipeline hangs (3), util inspect/parseEnv/getCallSites (4), stream explicitly unsupported APIs (2), url validation/missing API (2), string_decoder fuzz (1), zlib callback delivery (1).

#### Representative failure attribution (72 cases)

Selection: all 8 unsupported tests, all 10 timeouts, and representatives of every module/signature family (at least 7 per major module). Full logs are in the run directory. "First error" is normalized (run paths to `<out>`/`<testtmp>`, long numbers to `<n>`) and truncated here.

| Test | Result | Shim | Attribution | First error |
|---|---|---|---|---|
| `test-fs-chown-negative-one.js` | UNSUPPORTED | `fs_shim.js` | explicitly unsupported | Error: test/parallel/test-fs-chown-negative-one.js: fs.chown is not supported by the pir ext... |
| `test-fs-fchown-negative-one.js` | UNSUPPORTED | `fs_shim.js` | explicitly unsupported | Error: test/parallel/test-fs-fchown-negative-one.js: fs.fchown is not supported by the pir e... |
| `test-fs-link.js` | UNSUPPORTED | `fs_shim.js` | explicitly unsupported | Error: test/parallel/test-fs-link.js: fs.link is not supported by the pir extension runtime |
| `test-fs-statfs.js` | UNSUPPORTED | `fs_shim.js` | explicitly unsupported | Error: test/parallel/test-fs-statfs.js: fs.statfs is not supported by the pir extension runtime |
| `test-fs-readfile.js` | UNSUPPORTED | `fs_shim.js` | explicitly unsupported | Error: test/parallel/test-fs-readfile.js: fs.statfsSync is not supported by the pir extensio... |
| `test-fs-fchown.js` | UNSUPPORTED | `fs_shim.js` | explicitly unsupported | test/parallel/test-fs-fchown.js: Expected values to be strictly deep-equal: |
| `test-stream-compose.js` | UNSUPPORTED | `stream_shim.js` | explicitly unsupported | Error: test/parallel/test-stream-compose.js: stream.compose is not supported by the pir exte... |
| `test-stream-duplexpair.js` | UNSUPPORTED | `stream_shim.js` | explicitly unsupported | Error: test/parallel/test-stream-duplexpair.js: stream.duplexPair is not supported by the pi... |
| `test-fs-read-stream-fd-leak.js` | TIMEOUT | `fs_shim.js` | hang / no completion | testing for leaks from fs.createReadStream().close()... |
| `test-fs-watch-close-when-destroyed.js` | TIMEOUT | `fs_shim.js` | hang / no completion | exit code 1 with no output |
| `test-fs-watch-encoding.js` | TIMEOUT | `fs_shim.js` | hang / no completion | exit code 1 with no output |
| `test-fs-watch-recursive-add-file-with-url.js` | TIMEOUT | `fs_shim.js` | hang / no completion | exit code 1 with no output |
| `test-fs-watch-recursive-promise.js` | TIMEOUT | `fs_shim.js` | hang / no completion | exit code 1 with no output |
| `test-fs-watch-ref-unref.js` | TIMEOUT | `fs_shim.js` | hang / no completion | exit code 1 with no output |
| `test-stream-drop-take.js` | TIMEOUT | `stream_shim.js` | hang / no completion | exit code 1 with no output |
| `test-stream-pipe-manual-resume.js` | TIMEOUT | `stream_shim.js` | hang / no completion | exit code 1 with no output |
| `test-stream-readable-infinite-read.js` | TIMEOUT | `stream_shim.js` | hang / no completion | exit code 1 with no output |
| `test-util-inspect-long-running.js` | TIMEOUT | `util_shim.js` | hang / no completion | exit code 1 with no output |
| `test-fs-append-file.js` | ASSERT | `fs_shim.js` | missing validation/throw | test/parallel/test-fs-append-file.js: Missing expected exception. |
| `test-fs-assert-encoding-error.js` | ASSERT | `fs_shim.js` | missing validation/throw | test/parallel/test-fs-assert-encoding-error.js: Missing expected exception (TypeError). |
| `test-fs-append-file-sync.js` | ASSERT | `fs_shim.js` | value/semantic mismatch | test/parallel/test-fs-append-file-sync.js: Expected values to be strictly deep-equal: |
| `test-fs-open-mode-mask.js` | ASSERT | `fs_shim.js` | value/semantic mismatch | test/parallel/test-fs-open-mode-mask.js: Expected values to be strictly equal: |
| `test-fs-promises-file-handle-read.js` | ERROR | `fs_promises_shim.js` | missing API method | Unhandled promise rejection: not a function |
| `test-fs-promises-appendfile.js` | ERROR | `fs_promises_shim.js` | async rejection path | Unhandled promise rejection: Missing expected rejection (AbortError). |
| `test-fs-promises-readfile-with-fd.js` | ERROR | `fs_promises_shim.js` | value/semantic mismatch | Unhandled promise rejection: Expected values to be strictly equal: |
| `test-fs-stream-construct-compat-error-read.js` | ERROR | `fs_shim.js` | constructor called without new | TypeError: test/parallel/test-fs-stream-construct-compat-error-read.js: class constructors m... |
| `test-fs-write-stream.js` | ERROR | `fs_shim.js` | constructor called without new | TypeError: test/parallel/test-fs-write-stream.js: class constructors must be invoked with 'new' |
| `test-fs-lchown-negative-one.js` | ERROR | `fs_shim.js` | Windows symlink privilege | Error: test/parallel/test-fs-lchown-negative-one.js: EPERM: operation not permitted, symlink... |
| `test-fs-symlink.js` | ERROR | `fs_shim.js` | path argument validation in fs.symlink | TypeError: test/parallel/test-fs-symlink.js: The "path" argument must be of type string. Rec... |
| `test-fs-read-stream-double-close.js` | ERROR | `fs_shim.js` | callback/event never delivered | Mismatched noop function calls. Expected exactly 1, actual 0. |
| `test-fs-empty-readStream.js` | ERROR | `fs_shim.js` | missing API method | Unhandled promise rejection: not a function |
| `test-fs-promises-exists.js` | ASSERT | `fs_promises_shim.js` | export identity/shape | test/parallel/test-fs-promises-exists.js: Expected "actual" to be reference-equal to "expect... |
| `test-stream-filter.js` | ERROR | `stream_shim.js` | missing API method | TypeError: test/parallel/test-stream-filter.js: not a function |
| `test-stream-map.js` | ERROR | `stream_shim.js` | missing API method | TypeError: test/parallel/test-stream-map.js: not a function |
| `test-stream-once-readable-pipe.js` | ERROR | `stream_shim.js` | callback/event never delivered | Mismatched <anonymous> function calls. Expected exactly 1, actual 0. |
| `test-stream-err-multiple-callback-construction.js` | ERROR | `stream_shim.js` | callback/event never delivered | Mismatched noop function calls. Expected exactly 1, actual 0. |
| `test-stream-aliases-legacy.js` | ASSERT | `stream_shim.js` | value/semantic mismatch | test/parallel/test-stream-aliases-legacy.js: Expected values to be strictly equal: |
| `test-stream-end-of-streams.js` | ASSERT | `stream_shim.js` | value/semantic mismatch | test/parallel/test-stream-end-of-streams.js: Expected values to be strictly deep-equal: |
| `test-stream-duplex-destroy.js` | ERROR | `stream_shim.js` | property/accessor shape | TypeError: test/parallel/test-stream-duplex-destroy.js: no setter for property |
| `test-stream-pipe-cleanup.js` | ERROR | `stream_shim.js` | constructor called without new | TypeError: test/parallel/test-stream-pipe-cleanup.js: class constructors must be invoked wit... |
| `test-stream-consumers.js` | ERROR | `stream_consumers_shim.js` | value/semantic mismatch | Unhandled promise rejection: Expected values to be strictly deep-equal: |
| `test-stream-readable-to-web-byob.js` | ASSERT | `web_streams.js` | value/semantic mismatch | test/parallel/test-stream-readable-to-web-byob.js: Expected values to be strictly deep-equal: |
| `test-stream-readable-strategy-option.js` | ERROR | `web_streams.js` | missing web-streams global | ReferenceError: test/parallel/test-stream-readable-strategy-option.js: ByteLengthQueuingStra... |
| `test-stream-catch-rejections.js` | ERROR | `stream_shim.js` | async rejection path | Unhandled promise rejection: kaboom |
| `test-buffer-arraybuffer.js` | ASSERT | `buffer_global.js` | value/semantic mismatch | test/parallel/test-buffer-arraybuffer.js: Expected values to be strictly deep-equal: |
| `test-buffer-ascii.js` | ASSERT | `buffer_global.js` | value/semantic mismatch | test/parallel/test-buffer-ascii.js: Expected values to be strictly equal: |
| `test-buffer-readdouble.js` | ASSERT | `buffer_global.js` | value/semantic mismatch | test/parallel/test-buffer-readdouble.js: Expected values to be strictly deep-equal: |
| `test-buffer-writeint.js` | ASSERT | `buffer_global.js` | value/semantic mismatch | test/parallel/test-buffer-writeint.js: Expected values to be strictly deep-equal: |
| `test-buffer-constants.js` | ASSERT | `buffer_global.js` | missing validation/throw | test/parallel/test-buffer-constants.js: Missing expected exception. |
| `test-buffer-constructor-deprecation-error.js` | ERROR | `buffer_global.js` | callback/event never delivered | Mismatched <anonymous> function calls. Expected exactly 1, actual 0. |
| `test-buffer-tostring-range.js` | ERROR | `buffer_global.js` | encoding path | TypeError: test/parallel/test-buffer-tostring-range.js: Unknown encoding: ascii |
| `test-zlib-dictionary.js` | ERROR | `zlib_shim.js` | missing API method | TypeError: test/parallel/test-zlib-dictionary.js: not a function |
| `test-zlib-deflate-raw-inherits.js` | ERROR | `zlib_shim.js` | missing API method | TypeError: test/parallel/test-zlib-deflate-raw-inherits.js: not a function |
| `test-zlib-brotli.js` | ASSERT | `zlib_shim.js` | missing validation/throw | test/parallel/test-zlib-brotli.js: Missing expected exception (RangeError). |
| `test-zlib-brotli-dictionary.js` | ASSERT | `zlib_shim.js` | missing validation/throw | test/parallel/test-zlib-brotli-dictionary.js: Missing expected exception. |
| `test-zlib-invalid-input.js` | ASSERT | `zlib_shim.js` | value/semantic mismatch | test/parallel/test-zlib-invalid-input.js: Expected values to be strictly deep-equal: |
| `test-zlib-failed-init.js` | ASSERT | `zlib_shim.js` | value/semantic mismatch | test/parallel/test-zlib-failed-init.js: Expected values to be strictly deep-equal: |
| `test-zlib-from-gzip-with-trailing-garbage.js` | ERROR | `zlib_shim.js` | codec/input validation | Error: test/parallel/test-zlib-from-gzip-with-trailing-garbage.js: invalid gzip header |
| `test-zlib-brotli-16GB.js` | ERROR | `zlib_shim.js` | other behavior mismatch | TypeError: test/parallel/test-zlib-brotli-16GB.js: cannot read property 'buffer' of undefined |
| `test-path-dirname.js` | ASSERT | `path_shim.js` | value/semantic mismatch | test/parallel/test-path-dirname.js: Expected values to be strictly equal: |
| `test-path-posix-exists.js` | ASSERT | `path_shim.js` | export identity/shape | test/parallel/test-path-posix-exists.js: Expected "actual" to be reference-equal to "expected": |
| `test-path-win32-exists.js` | ASSERT | `path_shim.js` | export identity/shape | test/parallel/test-path-win32-exists.js: Expected "actual" to be reference-equal to "expected": |
| `test-path-win32-normalize-device-names.js` | ASSERT | `path_shim.js` | other behavior mismatch | AssertionError [ERR_ASSERTION]: path.win32.normalize("COM9.") === ".\\COM9.", but got "COM9." |
| `test-events-getmaxlisteners.js` | ERROR | `events_shim.js` | other behavior mismatch | TypeError: test/parallel/test-events-getmaxlisteners.js: The "emitter" argument must be an i... |
| `test-events-once.js` | ERROR | `events_shim.js` | async rejection path | Unhandled promise rejection: The operation was aborted |
| `test-util-parse-env.js` | ASSERT | `util_shim.js` | value/semantic mismatch | test/parallel/test-util-parse-env.js: Expected values to be strictly deep-equal: |
| `test-util-getcallsites-preparestacktrace.js` | ERROR | `util_shim.js` | value/semantic mismatch | test/parallel/test-util-getcallsites-preparestacktrace.js: Expected values to be strictly eq... |
| `test-util-exception-with-host-port.js` | ASSERT | `util_shim.js` | value/semantic mismatch | test/parallel/test-util-exception-with-host-port.js: Expected values to be strictly equal: |
| `test-url-invalid-file-url-path-input.js` | ASSERT | `url_shim.js` | missing validation/throw | test/parallel/test-url-invalid-file-url-path-input.js: Missing expected exception. |
| `test-url-revokeobjecturl.js` | ASSERT | `url_shim.js` | value/semantic mismatch | test/parallel/test-url-revokeobjecturl.js: Expected values to be strictly deep-equal: |
| `test-assert-calltracker-calls.js` | ASSERT | `assert_shim.js` | value/semantic mismatch | test/parallel/test-assert-calltracker-calls.js: Expected values to be strictly deep-equal: |
| `test-string-decoder-fuzz.js` | ASSERT | `string_decoder_shim.js` | other behavior mismatch | AssertionError [ERR_ASSERTION]: Mismatch: |

#### What the failures say

- **fs argument validation is the single largest actionable gap.** 30 tests fail because a bad argument does not throw at all or throws the wrong error: `fs.write` without an fd, non-numeric offsets/lengths, bad encodings, `fs.close/fsync/chmod/fchmod/readlink/rename/rmdir/truncate/unlink` type checks, `fs.readFile` encoding validation, `options.fs.open` validation in stream constructors. Tests assert `code`, `name`, and sometimes the message.
- **FileHandle/promises were mostly unimplemented at audit time.** 20 failures: `FileHandle.read/write/appendFile/chmod/truncate/sync/dispose/stream`, `fs.promises` identity, and abort-signal rejection paths (`AbortError` never produced). The current `FileHandle` class implements `read/write/readv/writev/readFile/writeFile/appendFile/chmod/chown/utimes/truncate/sync/datasync/stat/close/createReadStream/createWriteStream/asyncDispose` (`crates/pir-core/src/extensions/api/js/fs_shim.js.txt:1672-1930`), and the fs/promises shim seeds the live object; re-run for the remaining abort/semantic gaps.
- **Streams lack the helper layer and reliable callback delivery.** 14 tests call `Readable.from(...).map/filter/flatMap/forEach/reduce/toArray`, `stream.compose`/`duplexPair` (explicitly unsupported), and `pipeline` overloads. Another 16 end in `Mismatched ... function calls` because a `close`/`error`/`end` listener or callback never fires. Export identity (`require('stream')` shape, legacy aliases) accounts for 4 more.
- **Buffer numeric/encoding semantics are wrong, not missing.** 19 assertions compare `read*`/`write*` results, ASCII/UTF-8 detection, generic-method behavior, and `toString` limits. `Buffer` also does not emit the constructor/deprecation warnings that `common.mustCall` expects (3 tests), and `toString('ascii')` can report `Unknown encoding: ascii` on an internal path.
- **zlib had the largest missing-method surface in this run.** `params`, `flush`, `_processChunk`, zstd/brotli helpers and several stream methods were absent (10), argument validation was incomplete (9), and codec behavior differed (7: invalid gzip headers, 16 GB path, failed init). The current shim implements `params`, `_processChunk` and the zstd/brotli families (`crates/pir-core/src/extensions/api/js/zlib_shim.js.txt:223-400,550-710,770-790`); re-run for the validation/codec gaps.
- **Windows symlink tests fail for an environment reason plus a harness reason.** `fs.symlink` calls return `EPERM` without Developer Mode, and `common.canCreateSymLink` does not detect that and skip; 5 tests are lost this way. Treat this as a `common`/`fs` compatibility issue, not a core fs bug.
- **Ten hangs are real.** Five `fs.watch*` tests, `fs.createReadStream().close()` FD accounting, three stream iterator/pipeline tests (`drop/take`, `pipe` manual resume, infinite read), and `util.inspect` on a long-running structure never complete in 8 s. They are not self-spawn tests.
- **Unsupported APIs are explicit and cheap to enumerate.** Six of the eight are fs-shim gaps covering five distinct APIs (`fs.chown`, `fs.fchown`, `fs.link`, `fs.statfs`, `fs.statfsSync`), plus `stream.compose` and `stream.duplexPair`. `test-fs-fchown.js` still reaches an assertion because it expects a different failure shape after the unsupported call.
- **path subpath identity was broken in this run; win32 normalization may remain.** `require('path/posix') !== require('path').posix`, same for win32; `path.win32.normalize('COM9.')` returns `COM9.` instead of `.\COM9.`; `path.dirname`/`relative` edge cases differ. The current boot seeds the posix/win32 cache entries with the live objects (`crates/pir-core/src/extensions/api/node_shims.rs:474-490`), so only the normalize/dirname/relative gaps are expected to remain.

#### Known deviations of this run

- Suite is v24.22.1; the binary reports v24.19.0. Version-gated behavior may legitimately differ.
- `NODE_TEST_KNOWN_GLOBALS=0` is mandatory: pir leaks host/`__pir*` globals, and with the check enabled `common`'s exit handler runs before `mustCall` accounting, converting missing calls into false PASSes. The leak itself is still a defect; this flag hides it.
- The 438 subset is flag-free. `pir js` has no flag parser; tests with a non-ignorable `// Flags:` header are Tier C and are not part of this number.
- `process.execPath` is not node-compatible (`pir.exe file.js` starts the agent), so self-spawning tests are excluded from this subset. The 10 timeouts are watch/stream/util hangs, not self-spawn.
- In this recorded run `process.versions` was `{ node: '24.19.0' }`, so `common.hasCrypto` was false and one test skips as "missing crypto"; the current stub populates the full version table (`openssl`, `sqlite`, …). Five other skips are platform-specific (lchmod, mkdir mode, readdir buffer/ucs2, recursive watcher).
- The work root is repo-shaped (`<work>/test/{parallel,common,fixtures}`), so `../../test/common/...` and `test/fixtures` resolve. Only Node's own `test/`-relative paths work; npm/CLI/permission-model tests remain out of scope.
- Timeout is 8 s, not Node's 120 s. Every timeout was confirmed as a hang, not a slow success. The whole process tree is killed on Windows with `taskkill /F /T`.
- Source-shim mode was not a coherent snapshot (live edits). The harness records `shims.before/after.sha256` and `stable` in `summary.json`; use embedded mode for a number pinned to the binary, or wait for a quiet tree before comparing source-shim runs.

## 7. Top gaps the suite exposes immediately

Ordered by breadth/impact, each observed in the prototype:

1. **`process.config.variables` was missing at audit time.** Node's `test/common` crashed at load without it. The current stub publishes `process.config.variables` and `target_defaults` (`crates/pir-core/src/extensions/api/js/process_global_stub.js.txt:170-219`), so the overlay patch is now only a compatibility default for older binaries. Re-run before treating this as a live blocker.
2. **ESM parent-relative imports failed at audit time.** `import '../common/index.mjs'` from `parallel/` -> `module source not found`; a sibling import worked. This affected all 456 `.mjs` tests that import common. The current harness stages a `<work>/test/package.json` boundary (`tools/node-suite/runner.py::prepare_work_root`), so re-run before treating this as a live blocker.
3. **Builtin subpath identity was wrong at audit time.** `require('path/posix') !== require('path').posix`, same for `path/win32`; `require('fs/promises') !== require('fs').promises` and carried an extra own `default`; `require('stream')` had a different shape and lacked the `_stream_readable`/`_stream_writable` aliases. The current boot seeds the path and fs/promises cache entries with the live objects (`crates/pir-core/src/extensions/api/node_shims.rs:474-490`; `crates/pir-core/src/extensions/api/js/fs_promises_shim.js.txt:40-46`); stream aliases should be re-checked before treating this as live.
4. **`node:test` was a throwing stub at audit time** (`node:test.test is not supported ... no test-runner host`); 185 parallel tests use it. The current `crates/pir-core/src/extensions/api/js/test_shim.js.txt` is a real in-process `test/describe/it` runner with hooks, `t.plan`, subtests, mocks and TAP output. Snapshot assertions and `mock.module` still throw `ERR_NOT_SUPPORTED`, and there is no `--test` file-discovery CLI; re-run the corpus before counting this as a 4.4% blocker.
5. **Argument validation and error codes are incomplete.** 54 tests fail because a bad input does not throw/reject at all: `new Buffer(42, 'utf8')`, `fs.write` with no fd, `fs.readFile` with a bad/`buffer` encoding, `fs.close/fsync/chmod` type checks, `url.pathToFileURL` invalid paths, Brotli invalid params, `url.URL` invalid inputs. Tests assert `code`, `name`, and sometimes the exact message.
6. **Missing methods.** 39 tests fail with "not a function": `FileHandle` methods (`read`, `write`, `readv`, `writev`, `appendFile`, `chmod`, `truncate`, `sync`, `stream`, `dispose`), stream helpers (`map`, `filter`, `flatMap`, `forEach`, `reduce`, `toArray`, several `pipeline` overloads), zlib internals (`_processChunk`, `params`, `flush`, zstd), `assert.CallTracker#report`, `url.revokeObjectURL`. The recorded binary also lacked `stream.compose`/`duplexPair` (two of the eight UNSUPPORTED results). The worktree source now implements `compose`, `duplexPair`, the ordered/unordered operator helpers, a `CallTracker` port, `revokeObjectURL` argument validation, the `FileHandle` family and zlib `params`/`_processChunk`/zstd-brotli (`crates/pir-core/src/extensions/api/js/stream_shim.js.txt:2264,2762,3027,3200`; `crates/pir-core/src/extensions/api/js/assert_shim.js.txt:2885-3056`; `crates/pir-core/src/extensions/api/js/url_shim.js.txt:1088-1094`; `crates/pir-core/src/extensions/api/js/fs_shim.js.txt:1672-1930`; `crates/pir-core/src/extensions/api/js/zlib_shim.js.txt:223-400,550-790`), so re-run before counting these as missing.
7. **Events/warnings did not fire in this run.** 47 tests end with `Mismatched ... function calls` because a listener or warning never arrives: deprecation warnings from Buffer/`url.parse` are not emitted; `setMaxListeners`/`getMaxListeners` on `EventTarget` are no-ops (10 instead of 101); readable `error`/`end` ordering differs. The recorded `listenerCount(type, onceWrapper)` failure is now handled by matching the wrapper's `.listener` (`crates/pir-core/src/extensions/api/js/events_shim.js.txt:429-441`).
8. **`process.versions` was nearly empty at audit time** (`{ node }` only), which made `common.hasCrypto` false and 58/298 sampled tests skip as "missing crypto"; sqlite, uv, zlib, napi/modules fields were also absent. The current stub populates those fields (`process_global_stub.js.txt:137-151`); `hasIntl` stays false because ICU locale data is genuinely absent, not because the field is missing.
9. **Global surface pollution and `process.execPath`.** ~150 `__pir*`/host globals leak onto `globalThis`, defeating common's leak check (hence `NODE_TEST_KNOWN_GLOBALS=0`), and `process.execPath` cannot run a file node-style (`pir.exe file.js` starts the agent; only `pir js file.js` works). 475 tests re-spawn themselves; 33/298 sampled tests timed out on this.
10. **Encoding/limit and `vm` semantics.** `StringDecoder` mishandles high-bit bytes in `ascii`; `Buffer#toString` does not coerce a non-string encoding object and reports `Unknown encoding: ascii` in that path; `MAX_STRING_LENGTH` does not match QuickJS string limits; `vm.runInThisContext` is strict where Node is sloppy (`test-vm-not-strict`); `vm.SourceTextModule` is missing. Also `require(esm)` of a module that declares `const require` fails (affects `test/common/index.mjs`, 2 tests).
11. **Stack/call-site filenames regressed between builds.** `util.getCallSites()` returned the test path on the 12:08 build and `node_globals.js.txt` on the 13:06 build, breaking `test-util-getcallsites-preparestacktrace`. Worth a dedicated harness regression check; the suite catches it.

## 8. Recommended harness design

### Layout and storage

- Harness scripts: `tools/node-suite/` (runner, tier manifest, shim patch, README). Not part of `cargo test`.
- Corpus cache: outside the repo, e.g. `%LOCALAPPDATA%\pir-node-suite\node-v24.22.1\` or `target/node-suite/corpus/` (gitignored). 83 MB test + 51 MB fixtures; do not commit.
- Results: `target/node-suite/runs/<git-rev>-<pir-mtime>/` with `results.jsonl`, per-test `stdout.log`/`stderr.log`, and `summary.json`. Keep the last N runs for diffing.
- Fetch: sparse clone at the tag matching the claimed Node compatibility version; verify `pir js --version` and record it. Re-clone or `git fetch --depth 1` when the tag changes.
- Prepare: copy `test/{common,fixtures,parallel,...}` into a work root and apply the 3-line `common/index.js` patch (or keep a pristine clone and apply a patch, resetting afterwards).

### Execution

- Run each test directly: `pir js <test-root>/parallel/<file>.js`. **Do not** use a boot/require wrapper: it breaks `process.argv[1]`, `require.main`, `__filename`, and `getCallSites`.
- Env per test: `PIR_EXT_BYTECODE=0`, scratch `PIR_AGENT_DIR` (unique per worker), `NODE_TEST_KNOWN_GLOBALS=0`, `TEST_THREAD_ID=<slot>`, `TEST_SERIAL_ID=<slot>`, `NODE_TEST_DIR=<slot dir>`, `NODE_COMMON_PORT=12346+<slot>` (do not set `TEST_PARALLEL`, which makes `common.PORT` throw).
- cwd: the test root (`.../test`-shaped), matching Node's runner closely enough.
- Timeout: 120 s default (Node's runner default), 60 s for smoke/PR runs, 6x for pummel. Kill the **process tree** on Windows (`taskkill /T /F`); `timeout` alone may take 20 s to terminate a stuck pir child and leave grandchildren.
- Parallelism: 4-8 workers. Unique tmpdir, agent dir, and port per worker. At ~130-160 ms median startup, the 438-test subset took **200 s (3.3 min) single-threaded** (p50 160 ms, p90 567 ms); 4,183 tests are ~11 min single-threaded, roughly 3-4 min at 4-way with slow/timeout outliers.
- Flags: parse `// Flags:` and `// Env:`. Run only no-flag tests plus an allowlist (`--no-warnings`, `--pending-deprecation`); classify the rest `FLAGS_UNSUPPORTED` without running. Same for `// Env:` (record it).
- Enable/disable tiers by manifest: `A` (no common), `B` (common core), `B-ext` (`common/crypto`, `repl`, `debugger`, ...), `C` (node:test/worker/child/flags), `D` (excluded). Default PR job: Tier A+B-core pure API; nightly: Tier B fully; never Tier D.

### Classification

`PASS` (exit 0, no skip marker) | `SKIP` (`1..0 # Skipped`) | `TIMEOUT` | `ASSERT` (AssertionError) | `UNSUPPORTED_PIR` ("not supported by the pir extension runtime") | `NODE_TEST_MISSING` | `INTERNAL_MISSING` (`cannot find package 'internal'`) | `FLAGS_UNSUPPORTED` | `SELF_SPAWN` (source contains `process.execPath` and timed out) | `CRASH` (signal/exit > 128). Store the first error line and the full stderr.

Crucially, run the same corpus copy under the reference `node` for a sample (we validated all 58; a 5% rolling sample is enough) to prove failures are pir's, not platform/corpus errors. Maintain a Windows skip list and a baseline allowlist (`known-failures.json`) with an issue link per entry; fail the job only on regressions outside the baseline.

### Keeping it out of `cargo test`

- Separate script/xtask: `./tools/node-suite/run.sh --tier b-core` or `cargo xtask node-suite` (not an inline `#[test]`).
- CI job only (nightly and release-gate), reading the cached corpus; no network at test time.
- `#[ignore]` would still compile and run under `cargo test -- --ignored`; prefer a standalone binary/script and a `tests/node-suite` directory excluded from Cargo's test auto-discovery or gated behind a feature.

## 9. Answer to "how much is realistically usable"

- **As-is, no shim:** 5 of 4,183 parallel `.js` (0.1%); 3 pass.
- **With a vendored `common` + 3 lines + `NODE_TEST_KNOWN_GLOBALS=0`:** 2,640 (63.1%) were harness-runnable at audit time; 2,339 of those need only common/fixtures/tmpdir. On the clean 438-test public-API subset, 96.3% produced a meaningful verdict; the earlier build passed 174 (39.7%), and the pinned 6.4 run passes 178 (40.6%).
- **Plus modest feature work (`node:test` runner, self-spawn launcher, flag parser, worker support):** up to ~3,600 (86%) become runnable, but many will then fail for real API reasons. `node:test` is now a real in-process runner; the self-spawn launcher, flag parser and worker completeness are still gaps.
- **Not usable for this surface:** 511 (12.2%) internal/V8/inspector/addon tests, plus the 38 internet tests, most of sequential (`subprocess`/CLI), and pummel (stress).
- **ESM:** 456 tests are a separate, high-value 10.9%; the harness now stages a `test/package.json` entry boundary that addresses the parent-relative import failure recorded here.

Bottom line: use `test/parallel` as the main corpus, start with the 438 pure-API Tier B core, and grow the allowlist as the surface fills in. It will not give a single "Node-compatible" percentage, but it gives a precise, reproducible gap ledger that is already exposing real defects (subpath identity, `process.config`, deprecation warnings, `node:test`, FileHandle, stream helpers, ESM resolution).

## 10. Prototype artifacts (outside the repo) and the in-repo harness

- Report: `C:\wp\empty\_pir_node_wt\docs\node-test-suite-audit.md`
- Corpus clone: `C:\wp\empty\_scratch\node-suite-probe\node`
- Prepared test root: `C:\wp\empty\_scratch\node-suite-probe\work` (patched `common/`, `fixtures/`, all `parallel` tests)
- Runners: `run_proto.sh`, `run_list.sh`, `run_sample.sh`
- 58-test result (current build): `proto-results5/results.tsv`; Node validation: `node-58.tsv` (58/58 pass)
- 438-test pure-API result (current build): `clean-results2/results.tsv`
- 298-file random sample (earlier build): `sample-results/results.tsv`
- Candidate lists: `clean-candidates.txt`, `tiers-ab.txt`

The reusable harness and its baseline now live inside the repo (the prototype
scripts above remain only as historical artifacts):

- `tools/node-suite/runner.py` — prepare, run, classify, summarize; records binary sha256/mtime, suite tag, per-test duration, and shim-tree stability.
- `tools/node-suite/tiers.py` — rebuilds Tier A/B/C/D and `baseline/tier-b-api.txt` (438).
- `tools/node-suite/overlay/common-index-process-config.patch` — the only `test/common` change.
- `tools/node-suite/baseline/known-failures.json` — 438-test baseline from the run in 6.4.
- Raw run for 6.4: `C:\wp\empty\_scratch\ns-agent\runs\20261120-cabi-aot-final3` (see also `-bytecode*` for the stability runs).
