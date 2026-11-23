# Runtime support matrix

Support level of the Node/V8 surface today, grounded in the shims/host code and the recorded
Node-suite runs. This is a coverage ledger, not a compatibility percentage.

## Verification basis

- **Latest pinned run (2026-11-22, iteration 7):** the last successful release-fast/AOT tree before the
  plugin-index/cache source optimization was `_scratch/ns-fs/full-plugin-final`: 438 selected cases,
  **430 PASS, 0 ASSERT, 0 ERROR, 0 TIMEOUT, 8 SKIP**. The current optimization tree has source/debug
  focused evidence (Node 7/7, loader 17/17, extension runtime 40/40) and freshly generated 89 AOT blobs,
  but its final release-fast link was not completed in this environment; no new AOT number is claimed.
  The plugin-scale acceptance scope is the synthetic N=1/10/100 target; N=500/1000 are outside this
  iteration. The real 100-plugin result remains 88/100 loaded (see REPORT.md §7b).
- **Measurement correction:** earlier 410-PASS runs were partially inflated: `process.chdir`
  threw, so `test/common/tmpdir.js`'s `process.on('exit')` cleanup threw first and every later
  exit listener — including `common.mustCall`'s final counter check — was skipped for any test
  using `tmpdir`. `process.chdir` is now implemented (runtime cwd override), the exit event is
  re-entrancy-safe, and `mustCall` checks run; the 423 baseline was the first with those checks
  active, and iterations 4–5 closed the gaps it exposed (fs stream lifecycle, paused readable +
  readline, V8 string cap, Buffer DEP0005 stack, WHATWG BYOB backpressure).
- **Targeted tests** cover areas the 438-test public-API subset excludes (v8, vm, sqlite, http,
  child_process, worker_threads, node:test); see the rows below.
- Older pinned runs (`ns-agent/runs/20261120-cabi-aot-final3`, `node-release-audit-UrPHDC`,
  `ns-iter2`) are historical baselines only.

## Matrix

| Area | Status | How verified | Known gaps |
|---|---|---|---|
| `fs` | partial | 438-suite `fs` family: 164 tests, **157 PASS** (96%); `tests/node_surface.rs::fs_parity`, `tests/fs_numeric_open_flags.rs`, `tests/fs_watch_*.rs` | Implemented this iteration: Node-shaped `ReadStream`/`WriteStream` lifecycle (real fd open on construct, async `finish`/`close`, `open` still delivered after destroy, explicit `close()` under `autoClose:false`, writev batching, `options.fs`/monkey-patched `fs` routing, AbortSignal), `FileHandle`-backed read/write streams, `fs.statfs`/`statfsSync`, `fs.lutimes`/`lutimesSync`, `utimes` on directories, directory `fsync` as a no-op, unprivileged directory junctions + ADS-marker file links (stat/lstat/readlink/exists/utimes/unlink), symlink-leaf fence exemption for entry-only ops (unlink/rmdir/rm/rename source/lutimes), `ERR_FS_FILE_TOO_LARGE` for >2 GiB reads, and `process.chdir`. Still known: `fs.watch` is a polling snapshot diff (event coalescing differs under load); emulated file links are not transparent to `open`/`readFile` (Windows file symlinks need SeCreateSymbolicLinkPrivilege); `chown`/`fchown`/`lchown` are Windows no-ops; some fd/EBADF edges outside the selected tests. |
| `fs.promises` | partial | Included in the 164-test `fs` family (`test-fs-promises-*`); `tests/node_surface.rs` | FileHandle method surface plus `lutimes`/`statfs` and `createReadStream`/`createWriteStream` work; `FileHandle` streams use `handle.read`/`handle.write`, honour AbortSignal (with `cause`), and couple `handle.close()` to stream close. Abort reason/cause and invalid-signal validation match Node. |
| `stream` | partial | `stream` family: 145 tests, **145 PASS**; `tests/stream_shim_contract.rs`, `tests/suites/extensions_events.rs` | Consumers/compose/pipeline, operators (`take`/`drop`/`filter`/`map`/`flatMap`, concurrency + AbortSignal rejection), event-name seeding, `removeListener('readable')` next-tick flow, paused `Readable` (`readable`/`read(n)`/`unshift`, readline line splitting) and the pull-based WHATWG pad (byte-stream facade with pause/desiredSize backpressure and a real BYOB read) all pass the selection. Broader boundaries remain: the pad is not a spec-complete `ReadableStream` (`tee`/`pipeThrough`/streaming `_write` backpressure are approximated), and `fs` read streams buffer whole files rather than streaming. |
| `buffer` | partial | `buffer` family: 47 tests, **46 PASS** + 1 platform SKIP (>2 GiB allocation); `tests/node_shims.rs`, `tests/differential_node.rs`, `tests/node_tests_wave_b.rs` | Numeric read/write families, `isAscii`/`isUtf8` (with `structuredClone` transfer), `writeInt`/`writeUInt` range text, `inspect` identity and the Uint spellings match. The V8 2^29−24 string cap (`String.prototype.repeat` RangeError) is installed with the global `Buffer` backfill, and `new Buffer()`'s DEP0005 warning carries the `vm.runIn*Context` filename for node_modules suppression and stack reporting. Remaining outside the selection: `buffer.transcode` throws `ERR_NOT_SUPPORTED` (only utf8/hex/base64/latin1/utf16le codecs exist). |
| `zlib` | implemented | `zlib` family: 48 tests, **48 PASS**; `tests/zlib_crypto_shims.rs`, `tests/zlib_native.rs`, `tests/crypto_zlib_alignment.rs` | Brotli custom dictionaries, decoder error codes, non-finalizing `finishFlush` (partial output), multi-member gunzip with ignored trailing garbage and stream dictionary errors all pass. Decompressing streams (`Inflate`/`InflateRaw`/`BrotliDecompress`) now run on a **stateful host decoder session** (`crates/pir-node/src/modules/child/zlib_stream.rs`): `write()` can complete the frame and end the readable side without `end()`, output stops at the readable high-water mark and resumes from the same decoder state (the 16 GB brotli case no longer buffers), and `bytesWritten` is the decoder's consumed `total_in`. The codec crate is brotli 9.1.3 (brotli-decompressor 6.3.2), whose invalid-block code matches Node's bundled decoder (`-17 BLOCK_SWITCH`); the earlier `-9 BLOCK_LENGTH_1` mismatch is gone. `zstd` streams still use the whole-input codec. |
| `url` | implemented | `url` family: 2/2 PASS; `tests/node_shims.rs` | `pathToFileURL`/`revokeObjectURL` validation and formatting match; `URLPattern` remains an `ERR_NOT_SUPPORTED` stub (outside the 438 selection). |
| `util` | implemented | `util` family: 8/8 PASS; `tests/node_tests_wave_b.rs::util_call_sites_and_deprecate_contract`, `tests/node_surface.rs` | `inspect` (including long-running structures), `parseEnv`, `getCallSites`/`prepareStackTrace` and `diff` pass the selected tests; exotic inspect formatting beyond them is not claimed. |
| `querystring` | implemented | `querystring` family: 2/2 PASS; `tests/differential_node.rs`, `tests/perf_parity.rs` | No known suite gaps. |
| `events` | implemented | `events` family: 5/5 PASS; `tests/suites/extensions_events.rs`, `tests/node_tests_wave_b.rs::events_unhandled_error_and_async_resource_contract` | `EventTarget`/`AbortSignal`, `setMaxListeners`/`getMaxListeners`, `onceWrapper.listener` counting and async-resource contracts pass. Uncaught module-evaluation errors now route through `process.on('uncaughtException')` with a V8-shaped stack. |
| `assert` | implemented | `assert` family: 4/4 PASS; `tests/assert_shim.rs` | CallTracker (`report`/`calls`/`verify`), validation and error shapes pass the selected tests. |
| `path` | implemented | `path` family: 11/11 PASS; `tests/node_surface.rs::path_os_parity` | Subpath identity, `path.win32.normalize('COM9.')`, `dirname`/`relative` edge cases pass; boot seeds `path/posix`/`path/win32` with the live namespace objects. |
| `string_decoder` | implemented | `string_decoder` family: 2/2 PASS; `tests/node_surface.rs`, `tests/differential_node.rs` | Fuzz/ASCII semantics pass the selected tests. |
| `v8` | partial | `tests/v8_diagnostics.rs` (3 tests: heap statistics, uncapped/GCProfiler, expose-gc) | Diagnostic-only surface. `getHeapStatistics` maps QuickJS bytes; 10 V8 fields are zero placeholders. `GCProfiler.start/stop`, heap snapshots, `Serializer`/`Deserializer`, `serialize`/`deserialize`, `queryObjects`, `startupSnapshot.*` and `startCpuProfile` throw `ERR_NOT_SUPPORTED`; `setFlagsFromString('--expose-gc')` only affects new vm contexts. |
| `vm` | partial | `tests/node_surface.rs::vm_sandbox_semantics`; audit `test-vm-not-strict` | Same-realm sandbox approximation, not true context isolation: top-level `var`/function declarations do not persist on the sandbox, implicit globals escape, `let`/`const` are per-run, and `timeout`/`codeGeneration`/`cachedData` are accepted but ignored. `SourceTextModule`/`SyntheticModule` throw `ERR_NOT_SUPPORTED`; `runInThisContext` is strict where Node is sloppy; `runInNewContext` filenames do not reach real stack frames (they are published for the duration of the run so the Buffer DEP0005 shim can report the call site). |
| `sqlite` | partial | `tests/sqlite_shims.rs` (DatabaseSync CRUD/types, FTS5, better-sqlite3, multi-handle, file fence); audit `test-sqlite-config` | Core `DatabaseSync`/`StatementSync` and `better-sqlite3` consumption surface are real. `backup`, `Session`, `createSession`/`applyChangeset`, `function`/`aggregate`, `loadExtension`, `serialize`/`deserialize`/`setAuthorizer` and `limits` throw per the shim's documented gaps. |
| `http` | partial | `tests/http_shims.rs`, `tests/http_poll_pacing.rs`, `tests/http_body_stream_chunks.rs` | Real client and loopback HTTP/1.1 server. No TLS server (`https.createServer` throws), `net.Server` is throws-tier (http.Server is a custom mirror), `http.WebSocket` and `http._connectionListener` throw, `Agent.createConnection` throws, and request bodies are aggregated before send (no streaming `write`). |
| `child_process` | partial | `tests/child_process_stdio.rs`, `tests/child_process_stream_encoding.rs` | `fork`/IPC unsupported; `_forkChild` throws `ERR_NOT_SUPPORTED`; `spawnSync` keeps draining but drops oldest bytes past 8 MiB with `truncated: true`; self-spawn must use `<pir> js <file>` (`process.execPath file.js` starts the agent). |
| `worker_threads` | partial | `tests/worker_threads_real.rs` (eval/file workers, workerData, messages, terminate, clone rejection, mailbox bounds) | One OS thread + independent QuickJS runtime per worker; message bridge is JSON/structured-clone-minimal (functions/symbols/class instances/cycles throw `DataCloneError`, no transferList/TypedArray transfer). `moveMessagePortToContext` and `postMessageToThread` throw `ERR_NOT_SUPPORTED`; 8 MiB mailbox per direction drops newest data messages. |
| `test` (`node:test`) | partial | `tests/pir_js_runner.rs::node_test_*`, `tests/node_surface.rs` surface-name lock | Real in-process `test/describe/it` runner with hooks, `t.plan`, subtests, mocks and TAP output. Snapshot assertions and `mock.module` throw `ERR_NOT_SUPPORTED`; there is no `--test` file-discovery CLI. |

## Remaining non-pass tests (0 + 8 platform/infra skips)

Every test in the selected 430 executable cases passes. The eight SKIPs are platform/memory
conditions (macOS/Linux-only tests, symlink privilege, >2 GiB Buffer allocation), not
implementation gaps.

## Verdict counts

- implemented: 8 (`assert`, `events`, `path`, `querystring`, `string_decoder`, `url`, `util`,
  `zlib`); each is 100% of its selected 438 tests
- partial: 11 (`fs`, `fs.promises`, `stream`, `buffer`, `v8`, `vm`, `sqlite`, `http`,
  `child_process`, `worker_threads`, `test`)
- unsupported: 0 (no listed area is entirely absent; the throwing sub-surfaces are called out above)
