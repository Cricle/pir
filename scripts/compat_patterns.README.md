# compat_patterns.mjs — 模式级双跑对拍（pir × 真 node）

## 为什么单独一套

四套既有资产各管一段：`eco_census.mjs` 打 npm 包端到端、`node_tests/` 打 Node 官方
测试文件、`tests/differential_node.rs` 打 16 个模块级 fixture、`tests/node_surface.rs`
锁 1051 个导出名。**没有一套隔离"语言/惯用法反射层"**——own vs inherited 属性拷贝、
原型链与 `instanceof`、访问器语义、`Symbol.toStringTag`、迭代协议、错误对象形状、
模块身份。iconv-lite/safer-buffer 那类"包内部鸭子探测"回归正好落在这个缝里。

本脚本用 24 条模式 fixture 填这段：**同一条 fixture 先跑真 node、再跑 `pir js`，
把两边 stdout 的 `key: value` 行全量对拍**（不是断言硬编码期望值，真 node 即期望）。

## 判定

| 结果 | 含义 | 计分 |
|---|---|---|
| `match` | 两边 key/value 全等 | ✓ |
| `mismatch` | 有 key 缺失或值不同（报告给逐 key diff） | ✓ |
| `pir-error` | node 通过但 pir 抛错/超时 | ✓ |
| `node-error` | 系统 node 自己就不过 → **探针无效**，不进分母 | ✗ |

超时的 fixture 是 hard hang（如 `URLSearchParams` 迭代器），runner 用 20s 超时兜底：
它本身就是一条真实缺陷信号（消费者进程会假死）。

## 用法

```bash
node scripts/compat_patterns.mjs                        # 自动找 target-alt/debug（debug 优先）或 --pir
node scripts/compat_patterns.mjs --pir <pir.exe> --only stream-proto-chain,error-shape
node scripts/compat_patterns.mjs --verbose              # 打印分歧明细到 stdout
node scripts/compat_patterns.mjs --out report.md        # 默认写 %TEMP%/pir-compat-patterns/report.md
```

退出码恒 0（与 census 同约定）；报告与 `results.json` 是产物。

## 加一条模式

在 `PATTERNS` 里追加 `{ id, cat, why, js }`：

- `js` 只打印确定性的 `key: value` 行（必须先有 `": "` 才能被解析），结尾
  `console.log("PASS")`；不要依赖时间/路径/随机数。
- **先在系统 node 上跑通**（harness 会做 `node-error` 门禁，但别依赖它兜底）。
- 只断言 npm 生态真会依赖的行为；pir 分级不追的面（Intl 格式化、V8 内部）不要加。

## 历史基线（2026-10-13，pir target-alt/release；非当前口径——现行口径见 `scripts/node_tests/README.md` 的「当前结论」122/125）

24 条 → `match 16 / mismatch 7 / pir-error 1 / node-error 0`。7+1 条分歧全部登记在
`scripts/node_tests/REPORT.md` 底部「人工分析」的“引擎级发现”表：stream 原型链、
`Writable[Symbol.hasInstance]`、`URLSearchParams` 迭代器、`error.stack` 首行、
`util.types` 身份、`toStringTag`、定时器顺序、`globalThis.process` 描述符。

与 `eco_census.mjs` IDIOMS 的分工：IDIOMS 跑在**已安装 npm 包的目录里**、用真实依赖树
（safer-buffer/yazl/pump/js-yaml/archiver/pako/fast-glob/express）；本脚本的 fixture
**零依赖**，只测引擎与内置模块，适合在兼容层 PR 后分钟级回归。
