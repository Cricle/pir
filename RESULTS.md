# PIR 最终结果

- 状态快照（2026-11-17）：`master` 包含 #11630、TUI activity 与 native shortcut
  routing 改动。
- 代码提交 `70ab9c1` 的 CI：3/3 绿（Tests full suite / Linux lib + bins /
  Lint + SDK matrix）。
- 本文件是面向用户的结果文档；2026-11-15 之前的发版与旧 profiling 记录，以及
  已被取代的 2026-11-16/17 旧 fixtures A/B，已移到
  [`docs/archive/results-history-2026-11.md`](docs/archive/results-history-2026-11.md)。

## 2026-11-20 三模式 + pi A/B（N=1，debug）

同一任务（2026-11-18 mode bakeoff 的 AAA 单文件浏览器射击游戏）、同一网关/
模型（`new-provider-2`，`deepseek-v4.1-flash`，`openai-completions`），每臂独立
workspace 与 agent dir。`plan` 走 headless `print → submit_plan` → 等价的
`pir foreman approve` → detached job；`foreman` 走 detached run + `jobs wait`；
normal/pi 走 `print --mode json`。

| 臂 | tokens 合计 | 主会话 | 子 agent | 轮次 | 工具 | 耗时 | 产物 | 特征 | node |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| normal | 1,602,721 | 1,602,721 | 0 | 17 | 17 | 635s | 99 KiB | 25/25 | ok |
| plan | 2,236,905 | 2,236,905 | 0 | 28 | 37 | 515s | 110 KiB | 25/25 | ok |
| foreman | 5,290,066 | 278,180 | 5,011,886 | 113 | 145 | 1345s | 81 KiB | 25/25 | ok |
| pi | 4,177,643 | 4,177,643 | 0 | 67 | 70 | 1162s | 139 KiB | 25/25 | ok |

- 四臂都产出了可运行文件：25/25 结构特征、内联脚本 `node --check` 通过、
  0 外部资源。**没有做视觉/可玩性验收**。
- normal 最省 token；plan 最快且完整；foreman 主会话最轻（278k），但
  worker/reviewer 的 96 个嵌套轮次把总成本推到 5.29M；pi 单会话长跑，
  tokens/轮次/耗时都高于 pir normal，产物最大。
- 限制：N=1、单模型/单网关、debug 构建；墙钟只能在本轮四臂间比较。本轮
  网关与 2026-11-17 矩阵（tinygate）不同，不能直接相减。
- 本轮同时修复了 headless print 未绑定 `SessionService`、导致 plan approval
  无法落盘/批准的问题；另发现 `pir foreman run --foreground` 被忽略（仍走
  detached）——**已修复**：`--foreground` 现在前台运行子进程并透传退出码，
  暂停/未验证状态也返回非零并给出提示（`src/main_cli/foreman_mode.rs`）。
- 完整报告与原始记录：`../.play/mode-ab/RESULTS-20261120.md`、
  `../.play/mode-ab/runs/summary-modeab-n1-20261120-p2b.json`。
- 早期 tinygate 失败证据（402 余额不足 + stream 提前结束）：
  `../.play/mode-ab/runs/modeab-n1-20261120/`。

## 2026-11-20 P0/P1 实现状态与 tinygate 复测（N=1，debug）

本轮按计划落地了以下成本/质量改动：

- **read-before-edit 中间路线**：默认 `edit-partial`。`edit` 在 partial read
  后放行并告警；never-read/stale 目标仍阻断；`write` 仍要求完整、未变读取。
  `PIR_READ_BEFORE_EDIT=enforce` 可一键回到严格模式。
- **read/bash 输出预算**：balanced 保持 2000 行/50 KiB，efficient 默认
  1000 行/24 KiB；`PIR_TOOL_OUTPUT_MAX_LINES/BYTES` 可覆盖。
- **大文件写入**：`write` 新增可选 `append`，length-stop 错误提示拆分写入；
  系统提示要求超长内容先写骨架再追加。
- **plan 合约瘦身**：system prompt 只带 approval id、摘要与最多 2,000 字符
  摘录；完整 plan 仍在批准 follow-up 与 journal 中。
- **reviewer 契约**：`reviewer.md` 首行要求
  `{"verdict":"PASS"|"FAIL","visual":"verified"|"not_checked",...}`；
  `visual` 不参与 PASS/FAIL；不伪造视觉验收。
- **subagent 预算**：新增 `performance.subagentMaxTurns` /
  `subagentMaxTotalTokens`（efficient 默认 100 轮/100k tokens），
  `PIR_SUBAGENT_MAX_TOTAL_TOKENS` 可覆盖；inline 与 process executor 都生效。
- **截断即失败**：`stopReason: length` 现在按失败处理（`is_failed`、
  foreman worker gate、reviewer verdict）。截断的 worker/reviewer 不能再被
  当成完成或 PASS。
- **todo 批处理**：`todo` 新增 `batch`（一次调用原子执行多条
  create/update/delete/clear，最多 100 条）；`create` 可直接带
  `status: in_progress|completed`。工具说明改为鼓励 batch 和
  change → test → change 的小步循环，不再要求每个状态变化单独占一轮。

验证：`cargo fmt`/`cargo clippy --lib --tests -D warnings` 干净；
`zz_merged_2/4/6` 共 2,766 个测试，首轮 2 个与本次改动无关的时序用例失败
（RPC abort、MCP startup budget），单独重跑均通过。

muchenyun（`new-provider-2`）随后额度耗尽：返回
`HTTP 403 insufficient_balance`，用户提供的两个 key 也返回
`HTTP 401 invalid_api_key`。改用同模型 `tinygate`（`new-provider-3`，
`openai-responses`）完成 4 臂 N=1：

| 臂 | tokens | 主会话 | 子 agent | 轮次 | 工具 | 耗时 | 文件 | 特征 | 完整 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| normal | 472,076 | 472,076 | 0 | 19 | 21 | 511s | 39.5 KiB | 24/25 | yes |
| plan | 227,862 | 227,862 | 0 | 10 | 13 | 155s | 42.4 KiB | 23/25 | yes |
| foreman | 2,323,279 | 167,611 | 2,155,668 | 97 (12+85) | 109 | 564s | 55.8 KiB | 25/25 | yes |
| pi | 139,389 | 139,389 | 0 | 10 | 8 | 211s | 42.7 KiB | 24/25 | yes |

- 本轮 pi 最省（139k / 10 轮 / 8 工具）；normal 用了 3.4× tokens、
  1.9× 轮次，额外成本主要来自 10 轮自验证（todo churn、反复跑 node
  smoke），不是工具 schema 的固定开销。
- plan 最接近 pi：1.63× tokens、轮次持平、墙钟 0.73×，产物
  42.4 KiB / 938 行；plan 阶段 3 轮 / 40k，执行阶段 7 轮 / 187k。
- foreman 总成本 16.7× pi，但 25/25、55.8 KiB；reviewer 首轮 JSON
  `FAIL`，worker 修复后第二轮 `PASS`（`visual:"not_checked"`），
  P1 的 reviewer 契约与失败阻断确实生效。主会话只有 168k/12 轮，
  成本仍在 85 个嵌套 worker/reviewer 轮次。
- 四臂都 `artifact.complete`、`node --check` 通过、0 外部资源。结构特征
  是正则启发式：normal/pi 的 restart 用了 `startGame`/PLAY AGAIN，
  plan 的 muzzle 被写作 flash，不能当成真实功能缺失。
- 首屏视觉抽查（Edge headless，`shots-p0d/`）：normal 与 plan 层次最清楚，
  foreman 背景最丰富但正文对比度略低；pi 的渐变标题有明显重影，且开屏
  左上/右上残留 HUD 条，视觉完成度最低。这是静态首帧，不是可玩性验收。
- 与上一轮 `new-provider-2` 数字不可直接相减（不同网关/协议、N=1、
  单次方差）。P2 的 N=5 / release / 双 provider 矩阵仍未执行。

## 2026-11-20 Todo 批处理复测（N=1，debug）

与上一节 tinygate 历史样本相比，主会话纯 todo 回合为 Normal **6 → 2**、
Plan **3 → 2**、Foreman **6 → 3**；七次 `batch` 共提交 18 项操作，全部成功。
这是同任务、同模型和 API 的历史对照（旧 HTTP、新 HTTPS），不是仅切换 todo 的消融。

- Normal：273,035 tokens / 11 轮 / 358s，本次四臂中最少且最快；首次语法
  检查抓到错误，修复后仍跑了运行模拟。pi 为 410,095 / 18 / 770s，验证受
  Windows shell 和路径问题影响。单次结果不代表稳定领先，tokens 含缓存读取。
- Plan：484,315 / 21 / 1,441s（含 scout 3 轮）。纯 todo 回合减少，但多轮探索、
  网关断流重试和临时路径试错使总量上升。
- Foreman：724,832 / 47 / 838s，其中子会话 611,314 tokens / 39 轮。
  reviewer 给出 `PASS`，journal 为 `VERIFIED`，但独立运行检查发现菜单首帧访问
  未初始化的粒子数组，**不算成功交付**。不能把用量下降算作同质量节省。
- 四臂均通过完整性启发式和 `node --check`，工作区仅留下 HTML。旧四臂以及新
  Normal / Plan / pi 通过 Node VM 启动、暂停、继续和重开逻辑检查；新 Foreman
  首帧失败。未完成浏览器视觉验收。

完整分析、调用统计与独立运行检查的原始记录存放在 `.play/mode-ab/`
（工作材料，未入库；当前工作树上该目录已清理）。

## 2026-11-21 门禁修复后复测（N=1，debug）

同一任务/网关/模型（`new-provider-3`、`deepseek-v4.1-flash`、`openai-responses`），
二进制固定为 `ae4537ca…`（含混合调用、验证后修改、reviewer 工具收窄、
approval_id 授权面、审批消费持久化等门禁修复）。

| 臂 | tokens | 主会话 | 子 agent | 轮次 | 耗时 | 特征 | 首帧执行 |
|---|---:|---:|---:|---:|---:|---:|---|
| normal | 418,733 | 418,733 | 0 | 15 | 356s | 24/25 | 通过（379 次绘制） |
| plan | 718,755 | 718,755 | 0 | 23 | 615s | 24/25 | **失败**：`drawPlayer` 读 `p.invuln`，`p` 为 null |
| foreman | 1,166,133 | 105,811 | 1,060,322 | 43 | 387s | 24/25 | 通过（498 次绘制） |
| pi | 251,933 | 251,933 | 0 | 12 | 183s | 25/25 | 通过（731 次绘制） |

- 首帧检查用通用 Node VM 桩：加载内联脚本、派发 `DOMContentLoaded`/`load`、
  实际执行前两个 `requestAnimationFrame` 回调；上一轮已知坏掉的 foreman 产物
  作为对照稳定复现 `undefined.length`，说明检查有效。
- **Foreman reviewer 门禁生效**：reviewer 在证据中记录了临时 DOM/canvas 桩上
  执行启动、开火、暂停/恢复、DPR resize 与约 10 分钟波次到 boss HUD；产物首帧
  通过。上一轮“静态检查 25/25 但菜单帧崩溃”的类别本轮未再出现。
- **Plan 仍可能交付首帧崩溃的产物**：批准的 plan 只声明了静态验证
  （文件存在、JS 解析、外部引用 grep、特征 grep），门禁忠实放行。已在
  `PLAN_PROMPT`/`docs/tui.md` 增加“可运行交付物至少要有一条命令实际执行启动与
  首个执行周期，静态解析/grep 不算”的要求；这是提示契约，不是程序级强制。
- 限制：N=1，且同二进制上一轮 normal 273k、pi 410k，本轮 normal 419k、pi 252k，
  臂间波动大于门禁影响；token/耗时差值不作因果解读。N≥5 / release / 双 provider
  仍未执行。

### 复测：plan 运行时验证 + transcript 上限（pinned `98bae617…`）

第二轮用当前二进制（含子会话 transcript 上限、plan 运行时验证要求）复测，rep1：

| 臂 | tokens | 主会话 | 子 agent | 轮次 | 耗时 | 特征 | 首帧 |
|---|---:|---:|---:|---:|---:|---:|---|
| normal | 452,144 | 452,144 | 0 | 16 | 611s | 25/25 | 通过 |
| plan | 1,147,051 | 1,140,456 | 6,595 | 31 | 309s | 24/25 | 通过（696 绘制） |
| foreman | 776,419 | 105,171 | 671,248 | 37 | 316s | 24/25 | 通过（459 绘制） |
| pi | 242,571 | 242,571 | 0 | 14 | 240s | 24/25 | 通过（352 绘制） |

- **plan 不再交付首帧崩溃的产物**：批准的 plan 明确包含 headless Node smoke
  harness（stub DOM/canvas/audio、派发 Enter、执行 4 帧），并在交付后执行；
  上一轮同任务在菜单帧崩溃。代价是 plan 1.147M tokens / 31 轮（上一轮 719k / 23），
  单样本不能归因，但写/跑运行时检查本身有成本。
- 四臂首帧全部通过；静态特征只差 `muzzle`/`restart` 这类词边界漏计
  （如 `restartGame` 不匹配 `\brestart\b`），不能替代运行检查。
- rep2 因网关 `HTTP 402 insufficient_quota` 未执行（四臂均 0 tokens），N=2 未取得。

审计与证据、运行记录、复测记录存放在 `.play/audit/` 与 `.play/mode-ab/runs/`
（工作材料，未入库；当前工作树上已清理）。

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
- 旧 fixtures A/B（N=3、N=5）原始数据路径见
  [`docs/archive/results-history-2026-11.md`](docs/archive/results-history-2026-11.md)
- 历史发版/旧 profiling：[`docs/archive/results-history-2026-11.md`](docs/archive/results-history-2026-11.md)
- 测试映射：`docs/test-harness-map.md`
