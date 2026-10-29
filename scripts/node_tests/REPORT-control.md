# pir × Node 官方测试套件（子集）兼容率报告

- 日期: 2026-10-13 · 引擎: `node v24.19.0（控制组）`
- 来源: nodejs/node `v24.19.0` test/parallel，vendored 原样保存（仅 require 改写 + 3 处 common 补丁）
- 文件级: 125/125 通过 (100.0%)；fail 0；harness 预期失败 1
- 子测试级（node:test 文件）: pass 236 / fail 0 / skip 1 → 通过率 100.0%
- 排除（不运行）: 2

## 逐文件结果

| 文件 | 结果 | 子测试 p/f/s | 失败归类 | 首个错误 |
|---|---|---|---|---|
| test-buffer-alloc.js | pass | — |  |  |
| test-buffer-bytelength.js | pass | — |  |  |
| test-buffer-concat.js | pass | — |  |  |
| test-buffer-from.js | pass | — |  |  |
| test-buffer-includes.js | pass | — |  |  |
| test-buffer-indexof.js | pass | — |  |  |
| test-buffer-isencoding.js | pass | — |  |  |
| test-buffer-iterator.js | pass | — |  |  |
| test-buffer-slice.js | pass | — |  |  |
| test-buffer-tostring.js | pass | — |  |  |
| test-buffer-write.js | pass | — |  |  |
| test-path.js | pass | — |  |  |
| test-path-basename.js | pass | — |  |  |
| test-path-dirname.js | pass | — |  |  |
| test-path-extname.js | pass | — |  |  |
| test-path-join.js | pass | — |  |  |
| test-path-parse-format.js | pass | — |  |  |
| test-punycode.js | pass | — |  |  |
| test-querystring.js | pass | — |  |  |
| test-querystring-escape.js | pass | — |  |  |
| test-string-decoder.js | pass | — |  |  |
| test-url-format.js | pass | 1/0/0 |  |  |
| test-util-format.js | pass | — |  |  |
| test-util-inherits.js | pass | — |  |  |
| test-util-inspect.js | harness-fail | — | harness预期失败 | harness: internalBinding('js_stream').JSStream 的 _externalStream 是 C++ External 指针，纯 JS stub 无法复现（控制组 node 同样失败） |
| test-util-isDeepStrictEqual.js | pass | 2/0/0 |  |  |
| test-assert.js | pass | 18/0/0 |  |  |
| test-assert-async.js | pass | — |  |  |
| test-assert-deep.js | pass | 53/0/0 |  |  |
| test-assert-partial-deep-equal.js | pass | 140/0/0 |  |  |
| test-assert-fail.js | pass | 4/0/0 |  |  |
| test-assert-if-error.js | pass | 4/0/0 |  |  |
| test-buffer-equals.js | pass | — |  |  |
| test-buffer-compare.js | pass | — |  |  |
| test-buffer-read.js | pass | — |  |  |
| test-path-isabsolute.js | pass | — |  |  |
| test-path-relative.js | pass | — |  |  |
| test-path-normalize.js | pass | — |  |  |
| test-path-glob.js | pass | — |  |  |
| test-util-deprecate-invalid-code.js | pass | — |  |  |
| test-util-getcallsites-preparestacktrace.js | pass | — |  |  |
| test-util-inspect-getters-accessing-this.js | pass | — |  |  |
| test-url-urltooptions.js | pass | — |  |  |
| test-url-pathtofileurl.js | pass | — |  |  |
| test-string-decoder-end.js | pass | — |  |  |
| test-events-list.js | pass | — |  |  |
| test-event-emitter-method-names.js | pass | — |  |  |
| test-event-emitter-subclass.js | pass | — |  |  |
| test-timers-clearImmediate.js | pass | — |  |  |
| test-timers-args.js | pass | — |  |  |
| test-timers-zero-timeout.js | pass | — |  |  |
| test-process-env-delete.js | pass | — |  |  |
| test-process-env-symbols.js | pass | — |  |  |
| test-process-uptime.js | pass | — |  |  |
| test-process-warning.js | pass | — |  |  |
| test-os-eol.js | pass | — |  |  |
| test-module-builtin.js | pass | — |  |  |
| test-module-isBuiltin.js | pass | — |  |  |
| test-module-cache.js | pass | — |  |  |
| test-module-relative-lookup.js | pass | — |  |  |
| test-console-methods.js | pass | — |  |  |
| test-console-instance.js | pass | — |  |  |
| test-console-count.js | pass | — |  |  |
| test-console-group.js | pass | — |  |  |
| test-console-diagnostics-channels.js | pass | — |  |  |
| test-performance-measure.js | pass | — |  |  |
| test-perf-hooks-histogram.js | pass | — |  |  |
| test-diagnostics-channel-pub-sub.js | pass | — |  |  |
| test-async-hooks-constructor.js | pass | — |  |  |
| test-async-local-storage-bind.js | pass | — |  |  |
| test-zlib-from-string.js | pass | — |  |  |
| test-zlib-crc32.js | pass | — |  |  |
| test-crypto-update-encoding.js | pass | — |  |  |
| test-crypto-hmac.js | pass | — |  |  |
| test-stream-passthrough-drain.js | pass | — |  |  |
| test-stream-write-drain.js | pass | — |  |  |
| test-stream-readable-data.js | pass | — |  |  |
| test-stream-base-typechecking.js | pass | — |  |  |
| test-fs-non-number-arguments-throw.js | pass | — |  |  |
| test-fs-readfile-flags.js | pass | — |  |  |
| test-fs-mkdir-rmdir.js | pass | — |  |  |
| test-fs-chmod.js | pass | — |  |  |
| test-fs-opendir.js | pass | — |  |  |
| test-fs-mkdtemp.js | pass | — |  |  |
| test-require-unicode.js | pass | — |  |  |
| test-require-enoent-dir.js | pass | — |  |  |
| test-async-hooks-asyncresource-constructor.js | pass | — |  |  |
| test-async-hooks-enable-disable.js | pass | — |  |  |
| test-async-hooks-execution-async-resource.js | pass | — |  |  |
| test-console-assign-undefined.js | pass | — |  |  |
| test-console-issue-43095.js | pass | — |  |  |
| test-console-log-throw-primitive.js | pass | — |  |  |
| test-console-no-swallow-stack-overflow.js | pass | — |  |  |
| test-crypto-encoding-validation-error.js | pass | — |  |  |
| test-crypto-hash-stream-pipe.js | pass | — |  |  |
| test-diagnostics-channel-bind-store.js | pass | — |  |  |
| test-diagnostics-channel-has-subscribers.js | pass | — |  |  |
| test-diagnostics-channel-object-channel-pub-sub.js | pass | — |  |  |
| test-diagnostics-channel-safe-subscriber-errors.js | pass | — |  |  |
| test-diagnostics-channel-symbol-named.js | pass | — |  |  |
| test-event-emitter-emit-context.js | pass | — |  |  |
| test-event-emitter-errors.js | pass | — |  |  |
| test-eventemitter-asyncresource.js | pass | — |  |  |
| test-fs-constants.js | pass | — |  |  |
| test-module-globalpaths-nodepath.js | pass | — |  |  |
| test-module-nodemodulepaths.js | pass | — |  |  |
| test-module-setsourcemapssupport.js | pass | — |  |  |
| test-perf-hooks-timerify-basic.js | pass | — |  |  |
| test-perf-hooks-timerify-return-value.js | pass | — |  |  |
| test-perf-hooks-usertiming.js | pass | — |  |  |
| test-performance-many-marks.js | pass | — |  |  |
| test-performance-measure-detail.js | pass | — |  |  |
| test-process-beforeexit.js | pass | — |  |  |
| test-process-cpuUsage.js | pass | — |  |  |
| test-stream-construct.js | pass | — |  |  |
| test-stream-decoder-objectmode.js | pass | — |  |  |
| test-stream-destroy-event-order.js | pass | — |  |  |
| test-stream-duplex.js | pass | — |  |  |
| test-timers-clear-null-does-not-throw-error.js | pass | — |  |  |
| test-url-domain-ascii-unicode.js | pass | 1/0/0 |  |  |
| test-url-fileurltopath.js | pass | 6/0/1 |  |  |
| test-url-format-invalid-input.js | pass | 1/0/0 |  |  |
| test-url-format-whatwg.js | pass | 6/0/0 |  |  |
| test-zlib-const.js | pass | — |  |  |
| test-zlib-convenience-methods.js | pass | — |  |  |
| test-zlib-deflate-constructors.js | pass | — |  |  |

## 失败归类统计

- （无失败）

## 排除清单（不运行）

- test-path-resolve.js: 基础设施: 需要 child_process 自举 process.execPath + fixtures/path-resolve.js（pir 下 execPath 再入 pir 无法按 node 语义执行）
- test-events-once.js: 内部符号: 直接读 AbortSignal 的 EventTarget 内部存储 kEvents（需 --expose-internals），pir 的监听器存储无法对齐到该符号

## harness 预期失败（不计入分母）

- test-util-inspect.js: harness: internalBinding('js_stream').JSStream 的 _externalStream 是 C++ External 指针，纯 JS stub 无法复现（控制组 node 同样失败）

## 本轮改写/补丁清单

- ./common/index.js 改写
- ./shims/test.cjs 改写
- ./shims/internal_test_binding.cjs 改写
- ./common/*.js 改写

