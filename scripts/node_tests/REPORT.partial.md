# pir × Node 官方测试套件（子集）兼容率报告

- 日期: 2026-10-14 · 引擎: `pir js @ C:\Users\XPENG_USER\Desktop\github\pir\target-alt\debug\pir.exe`
- 来源: nodejs/node `v24.19.0` test/parallel，vendored 原样保存（仅 require 改写 + 3 处 common 补丁）
- 文件级: 0/1 通过 (0.0%)；fail 1；harness 预期失败 0
- 子测试级（node:test 文件）: pass 16 / fail 2 / skip 0 → 通过率 88.9%
- 排除（不运行）: 2

## 逐文件结果

| 文件 | 结果 | 子测试 p/f/s | 失败归类 | 首个错误 |
|---|---|---|---|---|
| test-assert.js | fail | 16/2/0 | 行为差异(assert) | node:test shim: 2 failing test(s): |

## 失败归类统计

- 行为差异(assert): 1

## 排除清单（不运行）

- test-path-resolve.js: 基础设施: 需要 child_process 自举 process.execPath + fixtures/path-resolve.js（pir 下 execPath 再入 pir 无法按 node 语义执行）
- test-events-once.js: 内部符号: 直接读 AbortSignal 的 EventTarget 内部存储 kEvents（需 --expose-internals），pir 的监听器存储无法对齐到该符号

## 失败样本 stderr 尾部

### test-assert.js — 行为差异(assert)
```
node:test shim: 2 failing test(s):
--- Additional asserts
AssertionError [ERR_ASSERTION]: Expected values to be strictly deep-equal:
+ actual - expected

  Comparison {
    code: 'ERR_ASSERTION',
+   message: 'false == true'
--- Additional assert
AssertionError [ERR_ASSERTION]: Missing expected exception.
    at <anonymous> (<input>:1490:15)
    at runNode (<input>:101:16)
    at runSuite (<input>:49:19)
```

## 本轮改写/补丁清单

- ./common/index.js 改写
- ./shims/test.cjs 改写

