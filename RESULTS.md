# PIR 最终结果

- 当前状态：2026-11-17，`master` 包含 #11630、TUI activity 与 native shortcut
  routing 改动。
- 代码提交 `70ab9c1` 的 CI：3/3 绿（Tests full suite / Linux lib + bins /
  Lint + SDK matrix）。
- 本文件是面向用户的结果文档；2026-11-15 之前的发版与旧 profiling 记录已移到
  [`docs/archive/results-history-2026-11.md`](docs/archive/results-history-2026-11.md)。

## 2026-11-17 四类任务矩阵（N=3）

同一网关/模型（`tinygate`、Anthropic messages、`deepseek-v4.1-flash`），两臂
都使用默认系统提示词，不传任务定制；同样的 flags 与工具集，隔离 agent dir，
按 rep 交替臂顺序。任务覆盖：

- **短任务**：单文件修 bug（`s1-duration`）、单文件补实现（`s2-slugify`）。
- **长任务**：33 个测试、9 个模块的 ledger 多回归修复（`l1-ledger`），以及
  两轮 plan→implement session（`c3-two-turn`）。
- **QA**：在真实 pir 快照上解释调度器、context-mode、memory builtin，要求
  `file:line` 引用（`q1`–`q3`）。
- **真实项目**：上游 pi 仓库真实 release 工具的维护任务，运行其自带
  `node --test`（`r1` 版本比较、`r2` sync-versions、`r3` 参数校验）。

### 分类合计

| 类别 | pir 成功/轮次/工具/tokens/耗时 | pi 成功/轮次/工具/tokens/耗时 |
|---|---:|---:|
| 短任务 | 6/6，33，35，87,200，77.7s | 6/6，27，26，62,625，59.8s |
| 长任务 | 6/6，72，93，880,827，573.4s | 6/6，65，89，1,214,427，446.3s |
| QA | 9/9，118，175，2,696,004，553.2s | 9/9，104，162，1,741,543，475.0s |
| 真实项目 | 9/9，66，71，528,869，206.0s | 9/9，57，68，393,543，191.3s |
| **合计** | **30/30，289，374，4,192,900，1,410.3s** | **30/30，253，345，3,412,138，1,172.4s** |

- 成功率：两臂 **60/60**；所有编码 fixture 均 `verify` EXIT 0，`tests/`
  修改数为 0。
- QA 引用：pir 无效 37/349（10.6%），pi 51/391（13.0%）；pir 引用总量更少，
  无效比例略低。
- 本矩阵中 pi 在 tokens/轮次/工具/耗时上整体更低；**不能**据此说 pir
  “更省更快”。这与 2026-11-17 上一轮旧 fixtures 的 N=3 结果方向相反，说明
  任务构成与单次 run 方差主导了总量。

### 任务中位数

| 任务 | pir tokens/轮次/工具/耗时 | pi tokens/轮次/工具/耗时 |
|---|---:|---:|
| s1-duration | 12,523 / 5 / 5 / 11.9s | 11,526 / 5 / 5 / 10.0s |
| s2-slugify | 16,435 / 6 / 5 / 14.1s | 9,177 / 4 / 3 / 8.7s |
| l1-ledger | 219,700 / 14 / 13 / 115.1s | 104,225 / 9 / 18 / 55.7s |
| c3-two-turn | 24,954 / 9 / 8 / 22.7s | 15,043 / 6 / 5 / 16.5s |
| q1-scheduler | 302,996 / 11 / 20 / 54.6s | 207,495 / 11 / 17 / 47.2s |
| q2-context-mode | 390,853 / 17 / 20 / 69.2s | 215,145 / 11 / 18 / 65.1s |
| q3-memory-builtin | 239,840 / 10 / 12 / 53.7s | 73,005 / 8 / 12 / 31.3s |
| r1-release-version | 53,153 / 6 / 6 / 15.1s | 23,474 / 6 / 6 / 16.7s |
| r2-sync-versions | 61,563 / 8 / 10 / 27.3s | 38,600 / 6 / 8 / 21.6s |
| r3-arg-validation | 44,140 / 7 / 8 / 20.5s | 42,645 / 6 / 7 / 21.4s |

**方差与例外**

- `l1-ledger` pi rep2 单次 975,275 tokens/303.7s，pir rep3 因网关
  `os error 10054` 重试到 318.1s；分类合计与中位数方向因此不同（pir 合计
  tokens 更低，但中位数更高、耗时更长）。
- QA q2 pir rep3 480,965 tokens、q3 pir rep1 323,824；短任务 pir rep1
  也明显高于其余两 rep。N=3 的单次跑飞足以改变类别结论。
- pir 的 stderr 在长任务中出现过“file has not been read in this session”
  的 read-tool 约束错误，增加了额外轮次；这是行为差异，不是网关错误。
- tokens = input + output + cache_read；重试失败尝试的 token 不计入最终记录。
- 限制：单模型/单网关、N=3；任务级方向不稳定；不能外推为普遍性能结论。

原始数据：`.play/ab/results/runs-matrix-n3-20261117.jsonl` 与
`summary-matrix-n3-20261117.json`。

## 2026-11-17 真机 A/B（N=3，旧 fixtures）

旧 fixtures（q1–q3、c1–c3）的默认配置对照：pir 18/18，150 轮、206 工具、
2,204,273 tokens、554.5s；pi 18/18，176 轮、266 工具、2,542,088 tokens、
642.3s。pir 相对 pi：轮次 −14.8%、工具 −22.6%、tokens −13.3%、耗时 −13.7%；
QA 引用无效率 10.9% vs 14.6%。限制：单模型、N=3，q1 token 方向与上一轮相反。
原始数据：`.play/ab/results/runs-real-n3-20261117.jsonl`。

## 2026-11-16 A/B vs pi v1.5.2（N=5，旧 fixtures）

同一网关/模型，Q&A 以关键词与 `file:line` 计分，编码以 fixture 测试通过且
未改测试为成功。汇总：q1 两臂中位耗时接近（75.5 vs 75.7s），pir tokens 更低；
q2 pir 4/5（一次网关 `error sending request`），tokens 更低；q3 pi tokens
更低；c1–c3 pir 15/15，pi 14/15（c1 rep4 改了 `tests/`）。原始数据：
`.play/ab/results/runs-ab-n5-qa-final5.jsonl`、`runs-ab-n5-coding-final5.jsonl`。
详细逐 run 解释在 git 历史与本文件归档前的版本中；结论是旧 fixtures 上
pir 的少轮次/少工具收益存在，但不能直接推广到新任务矩阵。

## 当前交付状态

- 测试：上一轮完整 nextest **7570 passed / 0 failed / 46 skipped**；
  doctests **33 passed / 7 ignored**；fmt/clippy 干净（CI `b480b5f`）。
- 本轮 TUI 改动（`/subagents`、`/todos` overlay、原生通知、background
  完成提示）已在本地通过 fmt、clippy 和定向 nextest；提交后跑 CI。
- 新 TUI 面：
  - `/subagents` 或 `ctrl+shift+a`：列出 live/recent subagent runs，
    ↑/↓ 选择、enter 展开任务/模型/usage/输出、`x` 中止选中 run、esc 关闭。
  - `/todos` 在 UI session 打开可键盘操作的列表：↑/↓ 选择、enter 展开
    描述/owner/依赖、space 推进状态、`d` tombstone、esc 关闭；原有
    `ctrl+shift+t` 折叠/展开 dock widget 保留。
  - 子 agent 完成时通过 `UiService::notify` 发一条完成/失败通知；native
    plugin 在没有 JS runtime 时也有 bounded 通知队列。
  - background tool 完成时追加一行 compact notice；完整输出仍替换原 tool
    block；`bg: N` 状态标记不变。

## 性能天花板（本机探针）

- **context-mode**：冷启动 18ms，warm 0.03ms；2M 次循环 380ms；索引
  38–72MB/s；64MB 文本生成 493MB DB（7.3×）；选择性检索 p50
  0.06–0.11ms；36.5k chunks 的 common-term 检索 120ms。非协作正则可持有
  进程级 `PY_VM_EXEC_LOCK` 超过 timeout。
- **turn/session**：live 5000 轮 221s（44ms/轮、18MB 常驻）；full 2000 轮
  144s、122MB，每轮成本 34→112ms；100MB session paged open 0.67ms、
  full 80ms、fork 282ms、listing 229ms、append 54µs；64MB 单行 paged
  93ms（慢于 full 48ms）；100MB recovery 1.6s/143MB。
- **plugin/notification**：native 服务纳秒级；JS extension 启动 45ms、
  +10–25MB；command/event/tool/renderer 单次跨边界 27–57µs；所有 JS 工作
  串行在一个 QuickJS actor。通知 push 0.4–0.8µs，1024 上限；500 个 widget
  入队 1.5ms、renderer refresh 13.5ms。
- 证据：`.play/evidence/context-mode-ceiling/`、
  `.play/evidence/turn-session-ceiling/`、
  `.play/evidence/plugin-notification-ceiling/`。

## 剩余边界

- context-mode 的 RustPython 不协作代码无法被 timeout 抢占；WAL/FTS5
  外部内容索引仍是下一步结构性优化。
- session listing 仍是全目录扫描 + 每文件窗口读；超大单行 paged 打开比
  full 更慢；没有 sidecar 索引/分段 session。
- 带 JS 扩展的 TUI 峰值约 97.5MB（JS runtime floor ~25MB）；native-only
  场景 QuickJS 零消耗。
- subagent 是 in-process 子会话（上下文隔离，不是 OS sandbox）；嵌套深度
  上限 3；子 runtime 有全局上限。
- persistent `pir jobs` 仍是 CLI 面；TUI 目前显示 background tool 的
  `bg: N` 与完成 notice，没有 job picker。
- A/B 只覆盖单模型/单网关；新任务矩阵 N=3，任务级结论不稳定。

## 复现

```bash
export PATH=/c/wp/empty/tools/mingw/mingw64/bin:$PATH
export RUSTUP_TOOLCHAIN=stable-x86_64-pc-windows-gnu
export CC=gcc AR=ar CARGO_TARGET_DIR=/c/wp/empty/pir/target-shared
cargo fmt --all -- --check
cargo clippy --lib --tests --locked -- -D warnings
cargo nextest run --lib --tests --locked
```

A/B：

```bash
cd /c/wp/empty
AB_BASE_URL=http://api.tinygate.top AB_WIRE_API=anthropic-messages \
  tools/python312/python.exe -u .play/ab/run_ab_pi.py \
  --reps 3 --categories short,long,qa,real --no-prompt --skip-perf \
  --tag matrix-n3-<date>
```

## 证据

- 四类矩阵：`.play/ab/results/runs-matrix-n3-20261117.jsonl`、
  `summary-matrix-n3-20261117.json`
- 旧 fixtures N=3：`.play/ab/results/runs-real-n3-20261117.jsonl`
- N=5：`.play/ab/results/runs-ab-n5-*.jsonl`
- 历史发版/旧 profiling：[`docs/archive/results-history-2026-11.md`](docs/archive/results-history-2026-11.md)
- 测试映射：`docs/test-harness-map.md`
