# node_tests 扩量选材记录（candidates / 排除理由）

本文件记录 2026-10-13 那一波扩量（26 → 88 个测试文件）的**选材口径**、**控制组已通过但未入选**的
候选（下一波可直接取用）、以及**排除理由**。它是 Deno `tests/node_compat/config.jsonc` 里
`ignore`/`reason` 的等价物，也是换 Node tag 后重新选材的起点。

## 1. 流程与判据

1. **静态筛选**（nodejs/node `v24.19.0` `test/parallel` 全量 3970 个 `test-*.js`）：命中任一
   “不可纯 JS 被测”特征即排除，括号内为全量命中数：`fixtures/*` 依赖(673)、非 NODE_KNOWN 模块(664)、
   `child_process`/spawn(548)、worker_threads/cluster(408)、`--expose-internals`(343)、`internal/*` 私有
   模块(213)、`internalBinding`(126)、inspector(100)、`--experimental-*`(67)、`--allow-natives-syntax`(16)、
   primordials(6)、napi/`.node`(3)。**静态干净 = 1916 个**。
2. **人工取池**：只保留 pir 有 shim 且值得度量的主题（assert / buffer / path / util / url / querystring /
   string_decoder / events / timers / process / os / module / require / console / perf_hooks /
   diagnostics_channel / async_hooks / zlib / crypto / stream / fs），共 **338 个候选**入池。
3. **控制组硬门槛**：入池文件先用系统 `node v24.19.0` 跑（`runner.mjs --node`）：301 通过 / 37 未过；
   37 个未过里有 21 个是“缺 `../common/<子模块>`”，被本波新增的两条改写规则修好后转绿。
   → **控制组可过 322 / 338**，不可过 16 个（第 4 节逐条列原因）。
4. **入选 60 个**（覆盖 20 个模块，逐文件见 MANIFEST.txt）；其余控制组可过的 262 个
   留作下一波（第 3 节，全部可直接取用）。
5. 纪律：控制组不过的文件**一律不入 MANIFEST**（宁缺毋滥）；入 MANIFEST 的文件逐字节等于上游 tag
   （本波用 v24.19.0 tarball 全量比对，92/92 字节一致）。

## 2. 本波 runner 适配（最小扩展，理由同步写在 runner.mjs 注释里）

- `REWRITES` 新增两条：`../common/<x>` → `./common/<x>.js`，`../../test/common/<x>` → `./common/<x>.js`
  （上游 21 个文件直接 require `common/tmpdir` 等子模块，此前两侧都是 MODULE_NOT_FOUND）。
- `COMMON_FILES` 新增 `hijackstdio.js`（32 行、零依赖；解锁 console/process 的 3 个文件）。
- 新增 `COMMON_SELF_REWRITES`：common 目录内 helper 自身的 `../common`、`./` 自引用改为同目录 `./index.js`
  （staged 后 common 位于 work/test/parallel/common/，比测试文件深一层）。
- 新 vendor 的 common 基础设施：`tmpdir.js`（此前已 vendor 但无人使用）、`hijackstdio.js`。
- **未**引入 `common/crypto.js`（164 行，5 个 crypto 文件依赖，控制组仍全挂）、`common/gc.js`（需 `--expose-gc`）、
  `common/countdown.js`（其唯一使用者在控制组仍失败）——保持 harness 最小。

## 3. 控制组已通过、未入选（下一波候选）

### 3.0 第二波（2026-10-13 下午）已取走 40 个 + 17 个已验证可过的落选

第二波选材口径：**先补覆盖矩阵里的弱模块**（async_hooks / diagnostics_channel /
perf_hooks / url / zlib / console / module / stream / events / process / timers），
一次抓取 57 个候选、全部过控制组后按“与既有资产重叠度”裁到 40 个入选
（逐文件见 MANIFEST.txt）。**262 → 222 个剩余候选**。

- 入选 40：async_hooks 3（asyncresource-constructor / enable-disable /
  execution-async-resource）、console 4（assign-undefined / issue-43095 /
  log-throw-primitive / no-swallow-stack-overflow）、crypto 2
  （encoding-validation-error / hash-stream-pipe）、diagnostics_channel 5
  （bind-store / has-subscribers / object-channel-pub-sub /
  safe-subscriber-errors / symbol-named）、event+eventemitter 3
  （event-emitter-emit-context / event-emitter-errors /
  eventemitter-asyncresource）、fs 1（fs-constants）、module 3
  （globalpaths-nodepath / nodemodulepaths / setsourcemapssupport）、
  perf+performance 5（perf-hooks-timerify-basic / -return-value /
  perf-hooks-usertiming / performance-many-marks / performance-measure-detail）、
  process 2（beforeexit / cpuUsage）、stream 4（construct /
  decoder-objectmode / destroy-event-order / duplex）、timers 1
  （clear-null-does-not-throw-error）、url 4（domain-ascii-unicode /
  fileurltopath / format-invalid-input / format-whatwg）、zlib 3
  （const / convenience-methods / deflate-constructors）。
- 已验证控制组可过但**本波落选 17 个**（下一波可直接取用，不需重新验证）：
  test-async-hooks-enable-disable-enable.js、test-console-clear.js、
  test-crypto-aes-wrap.js、test-crypto-des3-wrap.js、test-crypto-from-binary.js、
  test-event-emitter-add-listeners.js、test-events-listener-count-with-listener.js、
  test-fs-buffertype-writesync.js、test-fs-close.js、test-fs-exists.js、
  test-module-version.js、test-perf-hooks-timerify-invalid-args.js、
  test-stream-duplex-props.js、test-timers-api-refs.js、test-timers-immediate.js、
  test-url-invalid-file-url-path-input.js、test-zlib-create-raw.js。
  落选理由：与已入选文件或既有资产（census 探针 / differential 用例）重叠，
  或属纯 surface 列举（如 timers-api-refs）。

共 262 个文件控制组可过（“静态干净 + 系统 node 实测通过”；含本波改写后转绿的 9 个 fs 文件）。
下表按模块给出优先候选（每模块最多 5 个，按文件名排序）；`pir 探测` 列是本次顺手用 pir 跑过的结果，`—` = 未探。
取用方式：拷进 `vendored/` → 追加 `MANIFEST.txt` → `node runner.mjs --node --only <name>` 复验 → 再跑 pir。

| 文件 | 模块 | pir 探测 |
|---|---|---|
| test-assert-calltracker-calls.js | assert | — |
| test-assert-calltracker-getCalls.js | assert | — |
| test-assert-calltracker-report.js | assert | — |
| test-assert-calltracker-verify.js | assert | — |
| test-assert-checktag.js | assert | — |
| test-events-getmaxlisteners.js | events | FAIL（行为差异(assert)） |
| test-events-listener-count-with-listener.js | events | — |
| test-events-uncaught-exception-stack.js | events | — |
| test-stream-construct.js | stream | — |
| test-stream-decoder-objectmode.js | stream | — |
| test-stream-destroy-event-order.js | stream | — |
| test-stream-duplex-props.js | stream | — |
| test-stream-duplex.js | stream | — |
| test-timers-api-refs.js | timers | — |
| test-timers-clear-null-does-not-throw-error.js | timers | — |
| test-timers-clear-object-does-not-throw-error.js | timers | — |
| test-timers-clear-timeout-interval-equivalent.js | timers | — |
| test-timers-immediate.js | timers | — |
| test-process-available-memory.js | process | — |
| test-process-beforeexit.js | process | — |
| test-process-constants-noatime.js | process | — |
| test-process-constrained-memory.js | process | — |
| test-process-cpuUsage.js | process | — |
| test-fs-assert-encoding-error.js | fs | FAIL（行为差异(assert)） |
| test-fs-buffertype-writesync.js | fs | — |
| test-fs-close.js | fs | — |
| test-fs-constants.js | fs | — |
| test-fs-exists.js | fs | — |
| test-util-exception-with-host-port.js | util | — |
| test-util-inspect-long-running.js | util | — |
| test-util-primordial-monkeypatching.js | util | — |
| test-util-styletext-hex.js | util | — |
| test-util-styletext.js | util | FAIL（行为差异(assert)） |
| test-url-domain-ascii-unicode.js | url | — |
| test-url-fileurltopath.js | url | — |
| test-url-format-invalid-input.js | url | — |
| test-url-format-whatwg.js | url | — |
| test-url-invalid-file-url-path-input.js | url | — |
| test-zlib-const.js | zlib | FAIL（行为差异(assert)） |
| test-zlib-convenience-methods.js | zlib | — |
| test-zlib-create-raw.js | zlib | — |
| test-zlib-deflate-constructors.js | zlib | — |
| test-zlib-dictionary.js | zlib | FAIL（API缺失） |
| test-crypto-aes-wrap.js | crypto | — |
| test-crypto-des3-wrap.js | crypto | — |
| test-crypto-encoding-validation-error.js | crypto | — |
| test-crypto-from-binary.js | crypto | — |
| test-crypto-hash-stream-pipe.js | crypto | — |
| test-console-assign-undefined.js | console | — |
| test-console-clear.js | console | — |
| test-console-issue-43095.js | console | — |
| test-console-log-throw-primitive.js | console | — |
| test-console-no-swallow-stack-overflow.js | console | — |
| test-module-globalpaths-nodepath.js | module | — |
| test-module-nodemodulepaths.js | module | — |
| test-module-setsourcemapssupport.js | module | — |
| test-module-version.js | module | — |
| test-path-posix-exists.js | path | — |
| test-path-posix-relative-on-windows.js | path | — |
| test-path-win32-exists.js | path | — |
| test-path-win32-normalize-device-names.js | path | — |
| test-path-zero-length-strings.js | path | PASS |
| test-perf-hooks-timerify-basic.js | perf | — |
| test-perf-hooks-timerify-invalid-args.js | perf | — |
| test-perf-hooks-timerify-return-value.js | perf | — |
| test-perf-hooks-usertiming.js | perf | — |
| test-diagnostics-channel-bind-store.js | diagnostics | — |
| test-diagnostics-channel-has-subscribers.js | diagnostics | — |
| test-diagnostics-channel-object-channel-pub-sub.js | diagnostics | FAIL（行为差异(assert)） |
| test-diagnostics-channel-safe-subscriber-errors.js | diagnostics | — |
| test-diagnostics-channel-symbol-named.js | diagnostics | — |
| test-async-hooks-asyncresource-constructor.js | async | — |
| test-async-hooks-enable-disable-enable.js | async | — |
| test-async-hooks-enable-disable.js | async | — |
| test-async-hooks-enable-recursive.js | async | — |
| test-async-hooks-execution-async-resource.js | async | — |
| test-os-constants-signals.js | os | FAIL（行为差异(assert)） |
| test-querystring-maxKeys-non-finite.js | querystring | — |
| test-querystring-multichar-separator.js | querystring | — |
| test-string-decoder-fuzz.js | string | FAIL（其他） |
| test-buffer-arraybuffer.js | buffer | — |
| test-buffer-badhex.js | buffer | — |
| test-buffer-bigint64.js | buffer | — |
| test-buffer-constants.js | buffer | — |
| test-buffer-constructor-outside-node-modules.js | buffer | — |
| test-require-process.js | require | — |
| test-require-resolve-invalid-paths.js | require | — |
| test-event-capture-rejections.js | event | — |
| test-event-emitter-add-listeners.js | event | — |
| test-event-emitter-check-listener-leaks.js | event | — |
| test-event-emitter-emit-context.js | event | — |
| test-event-emitter-errors.js | event | — |
| test-eventemitter-asyncresource.js | eventemitter | — |
| test-eventtarget-custom-inspect-does-not-throw.js | eventtarget | — |
| test-os.js | os.js | — |
| test-performance-global.js | performance | FAIL（行为差异(assert)） |
| test-performance-many-marks.js | performance | — |
| test-performance-measure-detail.js | performance | — |
| test-asyncresource-bind.js | asyncresource | — |
| test-promise-handled-rejection-no-warning.js | promise | — |
| test-promise-swallowed-event.js | promise | — |
| test-promise-unhandled-silent.js | promise | — |
| test-streams-highwatermark.js | streams | — |

（其余 222 个控制组可过的文件同理可用；第一波起用 60 个、第二波起用 40 个，见 MANIFEST.txt / 3.0 节。）

## 4. 控制组未通过 → 排除（不入 MANIFEST）

| 文件 | 控制组失败原因（node v24.19.0 / Windows） | 处置 |
|---|---|---|
| test-util-stripvtcontrolcharacters.js | node:test shim: 1 failing test(s): | 排除（原因如上：上游 fixtures/deps、需 `--expose-gc`、node:test shim 能力、或 mustCall 组合） |
| test-process-versions.js | Error: Cannot find module '../../deps/acorn/acorn/package.json' | 排除（原因如上：上游 fixtures/deps、需 `--expose-gc`、node:test shim 能力、或 mustCall 组合） |
| test-process-getactiveresources.js | AssertionError [ERR_ASSERTION]: Expected values to be strictly deep-equal: | 排除（原因如上：上游 fixtures/deps、需 `--expose-gc`、node:test shim 能力、或 mustCall 组合） |
| test-module-loading-deprecated.js | Error: Cannot find module '../fixtures/packages/missing-main' | 排除（原因如上：上游 fixtures/deps、需 `--expose-gc`、node:test shim 能力、或 mustCall 组合） |
| test-module-parent-setter-deprecation.js | Mismatched <anonymous> function calls. Expected exactly 1, actual 0. | 排除（原因如上：上游 fixtures/deps、需 `--expose-gc`、node:test shim 能力、或 mustCall 组合） |
| test-module-parent-deprecation.js | Mismatched <anonymous> function calls. Expected exactly 1, actual 0. | 排除（原因如上：上游 fixtures/deps、需 `--expose-gc`、node:test shim 能力、或 mustCall 组合） |
| test-require-cache.js | Error: Cannot find module '../fixtures/semicolon' | 排除（原因如上：上游 fixtures/deps、需 `--expose-gc`、node:test shim 能力、或 mustCall 组合） |
| test-diagnostics-channel-memory-leak.js | Error: Cannot find module '../common/gc' | 排除（原因如上：上游 fixtures/deps、需 `--expose-gc`、node:test shim 能力、或 mustCall 组合） |
| test-diagnostics-channel-gc-race-condition.js | TypeError: global.gc is not a function | 排除（原因如上：上游 fixtures/deps、需 `--expose-gc`、node:test shim 能力、或 mustCall 组合） |
| test-promise-unhandled-warn.js | Mismatched <anonymous> function calls. Expected exactly 6, actual 0. | 排除（原因如上：上游 fixtures/deps、需 `--expose-gc`、node:test shim 能力、或 mustCall 组合） |
| test-promise-unhandled-error.js | Error: Cannot find module '../common/countdown' | 排除（原因如上：上游 fixtures/deps、需 `--expose-gc`、node:test shim 能力、或 mustCall 组合） |
| test-crypto-classes.js | Error: Cannot find module '../common/crypto' | 排除（原因如上：上游 fixtures/deps、需 `--expose-gc`、node:test shim 能力、或 mustCall 组合） |
| test-crypto-pbkdf2.js | Error: Cannot find module '../common/crypto' | 排除（原因如上：上游 fixtures/deps、需 `--expose-gc`、node:test shim 能力、或 mustCall 组合） |
| test-crypto-ecb.js | Error: Cannot find module '../common/crypto' | 排除（原因如上：上游 fixtures/deps、需 `--expose-gc`、node:test shim 能力、或 mustCall 组合） |
| test-crypto-hkdf.js | Error: Cannot find module '../common/crypto' | 排除（原因如上：上游 fixtures/deps、需 `--expose-gc`、node:test shim 能力、或 mustCall 组合） |
| test-crypto-cipheriv-decipheriv.js | Error: Cannot find module '../common/crypto' | 排除（原因如上：上游 fixtures/deps、需 `--expose-gc`、node:test shim 能力、或 mustCall 组合） |

- 前两行是 harness 能力问题（node:test shim 缺 mock/plan 之类；`../deps/acorn` 不在 vendor 范围）。
- `test-crypto-*`（5 个）与 `test-diagnostics-channel-memory-leak` 需要额外 vendor `common/crypto.js`、`common/gc.js`，
  本波按“harness 最小”原则不做；下一波若要扩 crypto 覆盖可一并引入。
- `test-promise-unhandled-*` 与 `test-module-parent-*deprecation` 在 node 控制组就 mustCall 失配，属上游写法与
  harness 组合问题，不是 pir 缺陷。

## 5. 已知上限（写结论时要带上）

- 控制组 node 为 Windows(x64) `v24.19.0`；`common.skip()` 的文件按 exit 0 计为 pass，runner 会在子测试列显示
  `0/0/n`（例：`test-url-format.js` 两侧都只跑 skip 分支；pir 侧 `common.hasIntl` 为假会让 hasIntl-gated 用例整file skip）。
  读到 `pass` 但子测试列是 `0/0/n` 时不要当成真通过。
- harness 预期失败 2 个：`test-util-inspect.js`（`internalBinding('js_stream').JSStream` 的 C++ External 桩不可复现）、
  `test-util-format.js`（QuickJS `Function.prototype.toString` 从 AST 重印源码）。
- `test-path-resolve.js`、`test-events-once.js` 保持 EXCLUDED（原因见 runner.mjs）。
- 选材只覆盖“纯 JS 可测”面：不含 worker/cluster/napi/inspector/http/net/tls/vm/v8/webstreams 等；
  这份子集不是 Node 测试套件的代表样本，分母是“可测面”而非“全量”。

