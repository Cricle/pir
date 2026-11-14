# PIR 最终结果

- 基线：`master` @ `b26a60fb23bd45306401190cacb5b6f2c1f5459c`
- 结果日期：2026-11-02
- 最新实测：2026-11-12（与上游 pi 1.4.0 的 A/B 轮，见第一节；其下依次为 0.3.0 发版准备与 2026-11-02 基线历史）
- 本文件是唯一面向用户的结果文档；过程材料仅在 `.audit/` 保留为原始证据。

## A/B campaign vs pi 1.4.0 (2026-11-12)

pir 与上游 `pi` 1.4.0 的本轮 A/B（封闭环境、loopback mock、隔离 agent dir）。
每条结论都有可复现脚本与原始日志在 `.play/evidence/`。缺陷均已修复并附前后对比。

**已修复的缺陷（本轮发现→修复→验证）**

| issue | 问题 | 修复效果（实测） |
|---|---|---|
| PIR-TUI-027 | TUI 主屏跨 512 条目窗口滑动时 `3J` 清空终端 scrollback，用户"历史被重置" | 第 4/5 轮 scrollback 从 2116/3162 行且不可达 → **16736/33587 行且第 1 轮内容可达**；跨窗口后 `3J` 停止增加 |
| PIR-TUI-028 | （修复 027 时自引入）窗口无法扩展时自驱重绘死循环 | 97–99% CPU 持续 + 19 MB 写日志 → **14%→0%→收敛**，剩余输出经证实是用户请求的历史物化（387 个不同条目） |
| PIR-BIN-030 | 打印模式下位置参数若等于子命令名，会执行子命令而非当作 prompt | `--print --mode json "version"` 由输出纯文本改为发出模型请求并产出 JSONL |
| PIR-BIN-031 | `@file` 规则被放宽后，缺失文件不再报错 | 见 `.play/issues/PIR-BIN-031.md` |
| PIR-BIN-037 | 无 provider 时 `--mode json` 退出码 0（pi 为 1） | 见 `.play/issues/PIR-BIN-037.md` |
| PIR-BIN-057 | provider 重试设置从未接线（`maxRetries`/延迟上限/`timeoutMs` 无效） | `maxRetries:0` 3 请求→**1**；`maxRetries:3` 3→**4**；延迟上限 60s→**5s**；`timeoutMs` 6.1s 成功→**1.13s 超时** |
| PIR-BIN-058 | close-delimited 流被截断/零字节时静默返回"成功" | 1 请求 `stop` 半截文本 → **4 请求 `error`: Stream ended without finish_reason**；合法 `[DONE]` EOF 前后都 1 请求成功 |
| PIR-BIN-070 | 压缩阈值 floor 把工具声明算两次，导致压缩触发过早、次数翻倍 | 26 轮脚本压缩 **24 → 8 次（与 pi 相同）**；触发时真实请求 **2242 → 3017 tokens**（阈值 3000）；摘要 prompt **21882 → 13466 tokens**（pi 13429） |
| PIR-BIN-071 | 一个写坏的 `settings.json` 让所有非交互命令 `exit 1`（pi 只警告一次） | 畸形 settings：`exit 1` 无输出 → **`exit 0` + 正常回答**，仅一行警告 |
| PIR-BIN-053 | 恢复会话时系统提示发两遍 | preamble/`<cwd>` 各 2 → **各 1**，请求体积 2219 → 1731 字符 |
| PIR-TUI-025 | 陈旧 session 锁导致恢复被拒 | touch 竞态 400/400 → **0/400**；真实 ConPTY 循环 100/100 → **0/100** |
| Windows 上下文文件 | 大小写不敏感卷上 `AGENTS.md`/`AGENTS.MD` 是同一文件，产生重复诊断 | 同一文件被两条诊断 → **一条**；大小写敏感目录仍按上游优先级（用 `fsutil` 真机验证） |
| PIR-TUI-032 | footer 显示 `thinking off`，实际请求发出 `high`（用户看到的计费档位是假的） | 见 `.play/issues/PIR-TUI-032.md` |
| PIR-BIN-074 | 启动 thinking 等级未按模型 `thinkingLevelMap` 钳制，`--thinking max` 原样上线 | 见 `.play/issues/PIR-BIN-074.md` |
| PIR-BIN-075 | `--no-session` 会静默丢弃最老历史（1024 条/100k tokens），无事件、无警告、无文档 | 见 `.play/issues/PIR-BIN-075.md` |
| PIR-BIN-073 | abort 后 14-230 ms 内 prompt 被拒，steer 被延后到下一个 prompt 之后（顺序错乱，无后续 prompt 则丢失） | `abort` 改为等待 idle 再响应：请求序列从 `[first]→[first,second]→[first,second,Q]` 变为 **`[first]→[first,Q]→[first,Q,second]`**，与 pi 完全一致 |
| PIR-BIN-076 | （high）`bash` 把 Windows 形式路径用 `;` 拼到 POSIX PATH 上，MSYS 解析成乱码，原生命令全部 ENOENT | `PathDialect` 按消费方拼写组合：`execFileSync('node')` 从 **`spawnSync node ENOENT` → `NESTED-OK`** |
| PIR-BIN-077 | write/edit 错误泄露中文本地化串 | `Cannot write 'x': 拒绝访问。 (os error 5)` → **`Cannot write 'x': permission denied (Access is denied; os error 5)`** |
| PIR-BIN-079 | 文件工具拒绝工作区内任何含 `..` 的路径 | `read src/../src/main.rs` 从被拒 → **成功读到文件**；`../outside` 仍被拒 |
| PIR-BIN-078 | `--thinking` 无 `--model` 时整条丢弃 | 无 `--model` 时 `high`/`max` 从 **`medium`（丢弃）→ `high`（生效并钳制）** |
| PIR-BIN-074 | 启动 thinking 等级未按模型钳制 | `--thinking max` 从上线 `max` → **`high`**（模型不支持 max） |
| PIR-TUI-032 | footer 显示 `thinking off` 而实际发 `high` | footer 改为读解析后的等级（已有 10 个自动化测试） |

**长会话耐久（lane-ab-endurance，100–5000 轮实测）**

- 会话文件增长双方线性一致：pir 1152.75 B/轮 vs pi 1154.09 B/轮；`lines = 2N+2` 精确，不重写不裁剪。
- 重开会话（spawn→首个模型请求，3 次中位数）：pir 152/152/199/**345** ms vs pi 910/910/899/**925** ms（26 KB/118 KB/578 KB/2.31 MB），**pir 快 2.7–6.0 倍**；打开窗口峰值 RSS pir 57-63 MB vs pi 121-140 MB。
- **PIR-BIN-070 的修复在 1000 轮规模上验证**：修复前 998 次压缩 vs pi 332；修复后 **332 vs 332**，触发点 3138 ≥ 阈值 3000，最后压缩在第 998 轮，0 个悬挂 `firstKeptEntryId`、0 个空摘要。摘要是**替换**而非累积。1000 轮后转录完整（0 解析错误，1000/1000 用户轮各出现一次）。
- 每轮延迟漂移：pir +0.0395 ms/轮至 2000 轮（×5.0）、5000 轮 ×11.2；pi ×5.3 / ×14.1 —— 形状一致，pir 绝对延迟约 3 倍但 CPU 更低（1.88 s vs 2.28 s/1000 轮），代价在序列化不在计算。有界上下文（4K 窗口）则完全无漂移（33 ms 恒定）。
- 句柄数无泄漏：pir 24→173 后稳定（5000 轮最大 176），pi 24→234 后稳定。

**流式体验与侧通道（lane-ab-streamux 实测）**

- 首字可见时间（含 0/200/1500 ms 人为延迟）：JSON 模式 pir 133.5/310.6/1626.3 ms vs pi 925.1/1128.5/2383.0 ms；
  TUI 首字 pir 184-191/374-406/1669-1710 ms vs pi 1148-1323/1315-1522/2625-2811 ms。**扣除人为延迟后的自身开销 pir 110-210 ms，pi 880-1320 ms（约 6-8× 轻）**。
- 增量粒度（4000 字/200 个 delta）：双方都不缓冲整段；pir 最大间隔 11-12 ms，pi 出现过一次 104-115 ms 调度停顿。
- 思考过程：双方默认都实时显示，`hideThinkingBlock:true` 都折叠为 `Thinking...`，`ctrl+t` 都能切换。
- 工具调用交织：双方可见顺序与线上顺序一致，无重排/丢失/重复。
- 中断：ESC 双方都保留部分回复并在 session 里记 `stopReason:"aborted"`；**双 ctrl+c 时 pi 会丢掉 session 文件里的部分回复，pir 保留**。

**已确认的上游差距但非缺陷（parity / pir 更优）**

- 大工具输出：双方都在 50 KB 截断并内联，之后每轮照常重发；行为一致。
- 压缩后的请求形状 `[system, summary, tail]` 与磁盘保留条目数双方一致。
- 损坏的 session JSONL：双方都跳过坏行并保留原字节继续；**pir 额外打一条 WARN**，pi 完全静默。
- 工具目标权限被拒：双方都把错误放在工具结果里、回合继续；pir 额外记一条 WARN。
- 关闭的 stdout 管道（EPIPE）：**pir 安静 exit 0**，pi exit 1 并抛 Node 堆栈。
- 每轮上下文增长：双方量级一致（pir +255 字符/轮，pi +255），pir 每轮少 25 B。

**保留的已知差距（未在本轮修改）**

- PIR-BIN-052：fork 的编辑不落盘，退出即丢。
- PIR-BIN-061/062/063：默认系统提示比上游多 4 条规则、少一个 `<docs>` 段、默认加载 9 个工具而 pi 加载 4 个，基线请求成本 **+4464 字符（≈+1116 tokens）**。是否为有意设计需要产品决策，不是纯缺陷。
- 设置文件写坏时同一错误会打印三行（CLI + codemod + memory 各一条），内容真实但格式重复。

## 0.3.1-rc.1 release (2026-11-14)

0.3.1-rc.1 是 0.3.1 之后的预发布候选：读路径性能整备（R1–R8 全部落地，R5/R6/R7
本批完成）、会话替换语义与上游对齐（PIR-BIN-049 残留全部闭合 / 110）、对抗复核揪出
并修复 8 个真实缺陷（PIR-BIN-101/104/105/106/107/108/110 及临时会话面），Kitty 裁剪
parity 闭合（drift 15/16 → Landed），TUI resume 首帧延迟从 1 s 降到一次 drain。
无破坏性变更（durable `StorageWrite::Task` 的 Rust API 改为 `Arc<TaskRecord>`，序列化
字节不变）。

**本轮修复/性能（均有前后实测）**

| 类别 | issue | 修复效果（实测） |
|---|---|---|
| 性能 | R3 park token | 运行结束的 park 校验从两次全转录本遍历降为 O(delta)，保留全量校验回退 |
| 性能 | R5/R6 会话读路径 | `context_usage()` 1 622 → 21 allocs/调用（N=800）；RPC `get_entries` 不再深拷贝整个会话 |
| 性能 | R7 快照分支文本增量 | 16.7 ms / 5 283 allocs → 1.06 ms / 35（N=800，字节恒等） |
| 性能 | PIR-BIN-099 事件行预规格化 | 200 KB 工具结果轮 full 配置 −2.3 MB 分配；session face −12.6%（3 042 → 2 658 KB），字节恒等 |
| 性能 | PIR-BIN-023 durable 任务提交 | alloc events 47 255 → 43 414（−8.1%），bytes 49 510 002 → 42 863 298（−13.4%），digest 与全终态一致 |
| 正确性 | PIR-BIN-101/102/103 窗口化投影桥接 | 被弃分支不再进入模型上下文；两个投影 walker 对同一窗口一致；30/30 钉住 |
| 正确性 | PIR-BIN-104 park 切片身份 | token 记录 storage generation；widen→re-trim 恢复布尔值也不再接受陈旧证明（回归钉钉住 fast=Some(true)/full=false/guard=None） |
| 正确性 | PIR-BIN-105/106/108 R7 缓存陈旧 | 进程级 storage generation + swap 清缓存；空 aggregate 回退全量渲染；x5 三轮对抗复核 verified |
| 正确性 | PIR-BIN-049/110 替换语义 | 替换统一按操作给 reason（new/resume，含父链子会话）；fork 缝由调用方显式选择；clone 缝完整走上游 fork 生命周期（`session_before_fork{entryId,position:"at"}` 可取消 + shutdown/start fork），任何替换都不发 `session_tree`/drain 期 `queue_update` |
| 正确性 | PIR-BIN-052 残留 / PIR-BIN-107 临时会话 | `--mode json` 认领会话控制动作；ephemeral `ctx.newSession()` 保持内存态；无存储 fork 与上游同文案拒绝 |
| TUI | Kitty 裁剪 / TUI-042 / TUI-043 | 字节钉 + 启动期 status/widget 行 + 启动排队 turn 恰好一次 |
| TUI | PIR-TUI-024 resume 首帧 | ConPTY 实测 ready→恢复文本 gap 1.00 s → 0.12 s（debug；唤醒标志改为 drain 前清零，不再被首帧绘制期入队事件丢失） |
| 文档 | PIR-BIN-062/063 工具装载 | 定案为已记录刻意偏差（Option B）：全量构建声明 22 = 9 core + 13 内置，给出 efficient/--tools/--no-extensions 切换与成本实测 |
| 测试 | QE-001 / QE-002 / 线语料 | 计时 flake 以更强契约根治；harness 地图逐行复验；776 事件随机对抗语料钉字节恒等 |

**门禁（本机 GNU，2026-11-14 实测）**

- `cargo fmt --all -- --check` EXIT 0。
- `cargo clippy --all-targets --all-features --locked -- -D warnings` EXIT 0（0 条诊断）。
- 全量 nextest（`--profile ci`，`RUST_MIN_STACK=8388608`）：**7500 tests，0 failed，46 skipped**；
  分项 = lib/bins 29 + zz_merged_1..8 的 772/791/795/776/790/1094/880/791 + standalone 20 + profiling 762。
- doctests 33 passed / 0 failed / 7 ignored；`cargo check --no-default-features --features sdk` EXIT 0。
- 版本三处一致：`[workspace.package]`、`.migration-info`、二进制 `--version` 均为 0.3.1-rc.1，
  `Cargo.lock` 两个 workspace 包同步。证据：`.play/evidence/release-candidate-032/`。

## 0.3.1 release (2026-11-13)

0.3.1 收入 2026-11-12 A/B 战役的全部修复，并补完发版前发现的会话持久性 / Windows
可移植性缺陷。上表三条“保留的已知差距”里，PIR-BIN-052 与“设置三连警告”已在本版修复。

**本轮新增修复（A/B 缺陷表之外，均已实测验证）**

| issue | 问题 | 修复效果（实测） |
|---|---|---|
| PIR-BIN-052 | `fork {entryId}` 只在原 JSONL 内移动 leaf，退出即丢 fork（`sessionId` 不变、无 `parentSession`） | fork 改为换入新会话文件：header `parentSession` = 源文件、行 = root→目标前缀、`sessionId/sessionFile` 改变、源文件不再追加；重启后仍是 fork。19 个 fork 用例全过（含 RPC/TUI/SDK 同一 seam） |
| PIR-BIN-080 | Windows 下 cwd 大小写不同（`c:\proj` vs `C:\Proj`）时 `--continue`/`--resume`/`--session` 找不到会话，静默开新会话 | 默认会话目录跳过 header-cwd 过滤（与上游 `filterCwd` 一致）+ Windows 大小写不敏感比较；三种入口都能续上原会话，真正跨项目仍走 CrossProject 提示 |
| PIR-BIN-081 | `write`/`edit` 的 temp+rename 把磁盘上的 `MixedCase.txt` 静默改名为调用者拼写 | 磁盘拼写与请求不一致时改走 in-place 重写：保留原拼写/inode/ACL/硬链接；新文件与拼写一致路径仍原子替换；符号链接仍拒绝 |
| 设置三连警告 | 一个坏 `settings.json` 打 3 行 stderr（CLI + codemod + memory 各一条） | 子系统重复通道降级为 log-only（`DiagnosticLevel::Debug`）；用户可见 1 行、退出 0；`--verbose`/`RUST_LOG=debug` 仍可看细节，新增回归测试钉住“恰好一行” |
| PIR-BIN-069 | docs/prompt-templates.md 写错设置键/目录/调用形式 | 改为代码真实契约（`prompts` 键、`<agentDir>/prompts`、`<cwd>/.pi/prompts`、`/<stem>` 调用）；全仓库无 `promptTemplates`/`/template:` 残留 |
| PIR-BIN-061 | `<docs>` 段在真实安装下从不输出（`PI_PACKAGE_DIR` 未设） | 维持“只指向真实存在的资产树”（README.md + docs/ + examples/）并记录与 pi 的偏差；<docs> 绝不指向不存在的路径 |

**门禁（本机 GNU，2026-11-13 实测）**

- `cargo fmt --all -- --check` EXIT 0。
- `cargo clippy --all-targets --all-features --locked -- -D warnings` EXIT 0（0 条诊断）。
- 全量 nextest（`--profile local`，`RUST_MIN_STACK=8388608`）：**8140 tests，0 failed，46 skipped**；
  分项 = lib/bins 29 + zz_merged_1..8 的 919/882/869/904/878/1066/921/890 + standalone 20 + profiling 762。
- doctests 33 passed / 0 failed / 7 ignored；`cargo check --no-default-features --features sdk` EXIT 0。
- 版本三处一致：`[workspace.package]`、`.migration-info`、二进制 `--version` 均为 0.3.1，
  `Cargo.lock` 两个 workspace 包同步。证据：`.play/evidence/release-v031/`。

## 0.3.0 release preparation (2026-11-11)

本节为 0.3.0 发版准备轮的实测记录；下方各节是 2026-11-02 基线，原样保留为历史。

**版本与产物（全部实测）**

- 版本 `0.3.0` 由 `[workspace.package]` 单点定义、两个 crate 继承：`Cargo.toml`、`crates/pi-phone-server/Cargo.toml`、
  `.migration-info`、`Cargo.lock` 以及两个二进制各自的 `--version`。
- release（GNU）`pir.exe` **76,635,136 B**，sha256
  `4c890e3b2e4905438ba4e1089c2161edbf0c08e8904f5c6ea7bc9d2d087936f9`；
  `pir-phone-server.exe` **4,553,216 B**。两者只导入系统 DLL，GNU 链接在零售
  Windows 上独立可用。证据：`.play/evidence/release-prep/build/REPORT.md` §2。
- 可发布 crate `pir-sdk-0.3.0.crate` **6,795,853 B**（< 10,000,000 B 上限；
  784 文件、6.5 MiB 压缩），`tests/`、`target*/`、`.play/` include 泄漏均为
  **0**，137 个 `include_str!/include_bytes!` 目标全部存在。证据：
  `build/REPORT.md` §5、`.play/evidence/release-prep/readiness/REPORT.md` §2。
- release 运行冒烟 **19/19 PASS**；release 模式 `pir python -c "print(42)"`
  **10/10 EXIT 0**（修复前基线为 EXIT 3221226505），`PIR_ALLOC_TRACE=0/1` 变体
  各 3/3 通过。证据：`build/REPORT.md` §4、`build/smoke-results.json`、
  `build/python-10x-final.log`。

**门禁（本机 GNU，2026-11-11 实测）**

- `cargo fmt --all -- --check` EXIT 0。
- `cargo clippy --all-targets --all-features -D warnings` EXIT 0、0 条诊断
  （8 条硬错误 + 13 条 `src/` 残留全部修复，未新增 `#[allow]`）。证据：
  `.play/evidence/release-prep/closer/clippy-final.log`、`fixlib/REPORT.md` §1-§2。
- SDK 4 形态无默认特性矩阵（`sdk`、`sdk,session`、`sdk,print`、
  `sdk,session,print`）各自 EXIT 0。
- doctest **33 passed / 0 failed / 7 ignored**；`cargo check --examples
  --no-default-features --features sdk` EXIT 0。
- 源码卫生：`PREEXISTING_INLINE_TEST_ALLOWLIST` 由 18 收到 **8**（原始豁免集）；
  20 个 `src/` 文件的内联测试模块迁至 `tests/suites/*_units.rs`，通过
  `#[doc(hidden)] pub` seam 访问，**37+ 条测试逐字保留**。证据：
  `.play/evidence/release-prep/closer/REPORT.md`、`closer/per-file-counts.md`。

**本轮修复**

- **release python 崩溃（PIR-BIN-022）**：tracing 全局分配器对零尺寸 layout 在
  `alloc`/`alloc_zeroed` 返回 dangling 指针、`dealloc`/`realloc` 不再转发，绕开
  core 泛型 `GlobalAlloc` 的 `assert_unchecked(size != 0)`；release 下同样生效
  （10/10 EXIT 0）。
- **`LoopOptions::prepare_next_turn_needs_completed` 默认值**：与其文档契约一致，
  手写 `Default` 改为 `true`；读 `message`/`tool_results` 的 `prepare_next_turn`
  hook 不再拿到空快照。`g_agent::agent_loop_parity` **28/28**。证据：
  `fixlib/REPORT.md` §3。
- **TUI 搜索框突发粘贴丢字（PIR-TUI-023）**：控制台 coalescer 把 ≥8 个可打印键
  重写成一个 paste 后被吞掉；现在活跃搜索框按字符消费该 chunk。单元
  `tui_paste_burst_p5` 28/28、`altscreen_parity` 64/64。证据：
  `.play/issues/PIR-TUI-023.md`。
- **测试契约对齐**：`tool_search` 激活对该 run 的下一次调用可见（PIR-BIN-004，
  与上游 `setActiveTools` 时点重建一致）；分页 `transcript()` 明确会 widen
  （PIR-SDK-025），测试改为断言 2288 总数 / 1400 唯一 / 888 重复-by-id。证据：
  `.play/evidence/release-prep/fixtests/REPORT.md`（§1、§2）。

**性能（均已实测，非外推）**

- PIR-SDK-027 投影 fragment cache：8k 条目下每次 push 的 snapshot wall
  **519 → 333 ms（−36%）**，allocs −3.1%。证据：
  `.play/evidence/lane-sdk/round56-027-cache/REPORT.md`。
- `live_session_tail(64 KiB, 128)` 1000 轮：**34.05 MiB 基本持平，斜率
  −0.0014 MiB/round**；默认全量驻留 **101.78 MiB，+0.0634 MiB/round**
  （斜率降 60.5%）。证据：`docs/sdk.md` R56 段（34.05/101.78/±斜率）。
- durable search tokenize 单趟流式化：同一基准 alloc bytes
  **1,255,057,460 → 11,353,200（−99.1%）**。证据：
  `.play/lanes/lane-builtin/STATE.md` cycle68。
- scheduler `TaskRecord` Arc 化：240-commit 基准 alloc events
  **81,051 → 50,815（−37.3%）**、bytes −41.8%。证据：同上 cycle69。
- SDK optionality 矩阵 **15/15 组合构建 EXIT 0**；`sdk`-only release 二进制
  **38,072,320 B**、idle working set **约 6.2 MiB**。证据：
  `.play/evidence/lane-sdk-dev/round28-sdk-matrix/REPORT.md`、
  `round29-engine-driven/REPORT.md`。

**仍未关闭**

- **PIR-BIN-023（low，deferred）**：`StorageWrite::Task` 改 `Arc<TaskRecord>`
  的 A/B 已测（alloc bytes −13.42%），但落地要改公共 wire-shaped enum 并同步
  外部 app，本轮不改，无稳定性影响。
- **`Cargo.lock` 钉住已被 yank 的 `clipboard-win 5.4.1`**：目前仅是 warning，
  `cargo package` 仍 EXIT 0；需要一次联网 `cargo update` 刷新（离线环境不可做）。
- **`deny.toml` 不是 CI 门禁**：只有本地 `scripts/gate_resources.ps1` 调用它；
  两个 workflow 都不跑 `cargo deny`；`rust-toolchain.toml` 已改为不再声称存在
  `msrv` CI job（MSRV 由 `rust-version = "1.97"` 强制）。
- **本地只有 GNU 验证**：CI 的 Windows 产物是 MSVC
  （`x86_64-pc-windows-msvc`），本地从未运行；Linux musl / macOS universal 同理。
- **全量测试**：权威 8 组在 2026-11-11T10:34:23Z 跑完，
  `ALL GROUPS DONE status=0`，**8 组全部 EXIT 0、0 failed**。
  聚合总数仍标记 pending re-measure，由全量 lane 写入
  `.play/evidence/release-prep/final/REPORT.md`；本文件不写聚合总数。

## 1. 交付状态

- `test_fast` final7（RSS/hook 多线合并前）：**6705 passed / 0 failed /
  0 real failures / 306 suites**
  （8 个空 harness 不计入通过；1,049.0 s wall = build 185.8 s + run 862.9 s）
- `cargo fmt --all -- --check`：EXIT 0
- `cargo check --tests --locked`：EXIT 0
- `cargo clippy --all-targets --all-features --locked -- -D warnings`：EXIT 0
- `cargo test --doc --locked`：33 passed / 7 ignored / 0 failed
- 关键定向（RSS/hook/allow 多线合并后全量复跑）：`g_agent` **924/0**、
  `g_extensions` **217/0**（1 ignored；`codemode_builtin::abort_…` 由固定
  300 ms sleep 改为「脚本首个嵌套 `echo` 返回后再 abort」的确定性信号后
  稳定通过）、`g_durable` **744/0**、`lazy_history` **11/0**、
  `lazy_history_memory` **1/0**、`tui_paged_load_more` 5/0、
  `tui_search_whole_history` 10/0。
  RSS/hook 轮新增：`paged_prompt_context` 3/0、`tui_session_event_frames`
  9/0、`hook_coverage` 22/0、`session_listing_single_read` 8/0。

## 2. 性能

### Release（criterion，2026-11-01）

| bench | baseline median | current median | Δ |
|---|---:|---:|---:|
| `durable_context_read_100mb` | 442.84 ms | **142.27 ms** | −67.9% |
| `session_open_long_line_64mb` | 518.05 ms | **62.30 ms** | −88.0% |
| `tool_bash_spawn_fixed_run` | 40.59 ms | 47.35 ms | +16.7% BLOCK |
| `tool_edit_file_fixed_run` | 2.416 ms | **2.063 ms** | −14.6% |
| `tui_dirty_frame_512` | 980.07 µs | **516.74 µs** | −47.3% |
| `tui_steady_frame_assembly` | 7.37 µs | **4.00 µs** | −45.7% |

新基线：B3 `jsonl_recovery_open_100mb` **1.903 s**、B8 `session_search_keystroke_1k`
**5.62 ms**、T2-1 `durable_document_commit_no_observer` **9.34 ms**。
唯一严格 BLOCK 是 `tool_bash_spawn`，疑同机 Defender/环境漂移，待同机 baseline A/B。

### Debug（dev profile，2026-11-02，10 samples）

| bench | mean | median |
|---|---:|---:|
| `durable_context_read_100mb` | **59.62 ms** | **59.19 ms** |
| `session_open_long_line_64mb` | 298.10 ms | 299.02 ms |
| `tool_bash_spawn_fixed_run` | 42.71 ms | 42.71 ms |
| `tool_edit_file_fixed_run` | 3.111 ms | 3.161 ms |
| `tui_dirty_frame_512` | 2.594 ms | 2.558 ms |
| `tui_steady_frame_assembly` | 9.7 µs | 9.7 µs |
| `jsonl_recovery_open_100mb` (B3) | **3.009 s** | **2.977 s** |
| `session_search_keystroke_1k` (B8) | 52.95 ms | 52.60 ms |
| `durable_document_commit_no_observer` (T2-1) | 67.57 ms | 67.65 ms |

### Debug 内存

| 指标 | before | after | Δ |
|---|---:|---:|---:|
| `session_open_long_line_64mb` peak | 213.6 MB | **78.4 MB** | −63.3% |
| `session_open_long_line_64mb` max allocation | 134.2 MB | **67.1 MB** | −50.0% |
| B1 `durable_context_read_100mb` peak | 113.2 MB | **0.96 MB** | −99.15% |
| B1 total allocation | 456.9 MB | **4.19 MB** | −99.08% |
| B3 `jsonl_recovery_open_100mb` peak | 153.8 MB | **142.1 MB** | −7.64% |
| B3 total allocation | 510.3 MB | **394.2 MB** | −22.75% |
| B3 10 MB control peak | 38.3 MB | **27.9 MB** | −27.32% |

heap-only 探针；B1 的 0.96 MB 是调用线程峰值，line thread 仍保留一份解析记录。

### Lazy session 打开（tail-first mmap + 上翻分页）

| 场景 | 打开峰值增量 | 打开后 live | 打开耗时 | resident entries |
|---|---:|---:|---:|---:|
| paged 10 MB | **1.24 MB** | 1.24 MB | **14.78 ms** | 245 |
| paged 100 MB | **1.24 MB** | 1.24 MB | **14.46 ms** | 245 |
| full 10 MB | 25.87 MB | 15.08 MB | 78.58 ms | — |
| full 100 MB | 160.29 MB | 145.28 MB | 790.09 ms | — |

100 MB 与 10 MB 的 paged 峰值相同（约 1.24 MB）；上翻由 `read_older_page`
按页加载，TUI 在距顶部 5 个视口时后台预取到 4 页 / 8 MiB 有界 LRU。

### Debug RSS（2026-11-02，process-tree working set）

| 场景 | before | after | Δ |
|---|---:|---:|---:|
| 100 个 session + 256 MiB 单行 outlier listing peak | 799.4 MB | **2.12 MB** | **−99.7%** |
| 同上 total allocation | 1.067 GB | **3.40 MB** | **−99.7%** |
| 同上 最大单次分配 | 532.9 MB | **1.05 MB** | **−99.8%** |
| 同上 criterion median | 636.77 ms | **407.60 ms** | **−36.0%** |
| 1,000 个 session + outlier listing peak | 799.6 MB | **2.28 MB** | **−99.7%** |
| print 100 MB session（关闭压缩 / widen）peak RSS | 973.6 MB | **206.8–215.7 MB** | **−78%** |
| print 100 MB session（32k resident compaction）peak RSS | 563.2 MB | **426.3 MB** | **−24.3%** |
| TUI 2 MB session peak RSS | 81.9 MB | **66.0 MB**（J2 线后；早期 72.1–72.6 MB） | **−19%** |

listing 只用 1 MiB 行窗口扫描超大单行 session（`message_count` 仍精确），
reader 按文件大小选 8 KiB/64 KiB/1 MiB 缓冲，换行搜索走 `memchr`。print
不再在进 agent loop 前克隆整份 projection；widen 请求体 ≥8 MiB 序列化到
临时文件流式发送（wire 不变）；assistant replay 用 `LazyMessage` 借用同模型
消息（16 B/元素）；`before_provider_request` 只在存在活跃 handler 时才走
Value-body 路径，默认 print 不再为每个请求建 +116.5 MB 的 JSON 树。

无 JS 插件的 builtin-only plan 现在不会创建 `JsExtensionsPlugin`/JS adapter，
`js_handle()` 也不再把 plan-only handle 暴露为 live JS runtime；
`js_runtime_build_count` 仍为 0。普通 TUI 启动峰值约 **72.1–72.6 MB**
（2 MB session，debug；no-session floor 56.9 MB）。shell 持久池改为第一次
真正 loadout 才预热，启动期不再拉起 bash 成员 + 独立 runtime。

66 MB 左右的普通 debug 启动构成：约 20 MB 进程/CLI floor；约 24 MB 核心
AgentSession/plugin-host；约 13 MB TUI 基础设施；2 MB session 的
projection/render/request 约 9.1 MB（J2 线后；原 14.9 MB）。启用任一 builtin
约再加 1.9 MB，
mcp 约 +0.6 MB。100 MB **paged** TUI session 峰值不随 session 大小增长
（分页窗口固定）。700 MB 级别只出现在关闭压缩时被迫 widen 的一次性 turn，
且已从 973.6 MB 降到 ~207 MB。

**JS 内核成本（同二进制 A/B + dhat 归因，实测）**：同一 fixture 下，加载一个最小
JS 扩展（1 tool + 1 command + 1 hook）使 TUI 峰值 **72.0–72.5 →
97.3–97.5 MB（约 +25 MB；boot mark +14.5 MB）**。dhat 显示其中**堆只有
+2.31 MB live / 12.4 MB 总分配**（prelude 字符串 ~2 MB、miniz 流瞬态
3.2 MB）；plain `.js` 已走 verbatim 快路径不进 SWC，**其余 ~23 MB 是
QuickJS 解释器/运行时代码页进 working set + actor 线程栈，debug 下不改
架构就压不掉**。已顺手修掉 prelude 缓冲的 ~0.9 MB 常驻 overshoot
（`Shim::inflate` 改精确大小）。无 JS 场景 QuickJS 零消耗（不创建
runtime）。数据：`logs/js-ab/{nojs,withjs}/summary.json`。

### SDK 基准（debug，`benches/sdk_benchmark.rs`）

| bench | median |
|---|---:|
| `sdk_agent_build_in_memory` | 13.13 ms |
| `sdk_prompt_roundtrip_small` | 13.40 ms |
| `sdk_prompt_stream_small` | 13.26 ms |
| `sdk_transcript_read_medium` | 419.29 µs |
| `sdk_transcript_json_medium` | 3.514 ms |
| `sdk_session_attach_medium` | 13.78 ms |
| `sdk_event_bus_emit_8` | 269.90 ns |
| `sdk_event_projection_entry_appended` | 3.695 µs |

SDK 优化：非流式 `Agent::prompt` 不再为不存在的 stream 投影每个 delta/turn
事件；session-event bridge 改为第一次 `prompt_stream*` 时才订阅；`attach_manager`
和 `ThinkingLevel`/`session_path` 读取改用 leaf-path/轻量 accessor，避免为读一个
字段构建整份 projection。J2 线（`sdk_heap_profile`，200-turn fixture，16 ops）：
`transcript_json` 1,191,647 → **830,624 B/op**（−30.3%，blocks −15.3%），
SDK attach 4,002,637 → **2,853,475 B/op**（−28.7%）；实现上 transcript_json
不再构建整份 `SessionProjection`，attach 的 1 MiB BufReader 改为 8K/64K/1M
按文件大小选择。基准为离线 in-process provider，session fixture 走真实
`SessionManager` append 路径。

### SDK examples（10 个，全部可跑）

`examples/` 已整理为 10 个可直接运行的示例 + 共享 `common/mod.rs` + README
特性矩阵：quickstart / streaming / tools / sessions / plugin / control /
builtin / extension / proxy / real_endpoint。每个示例只声明自己需要的
`required-features`，离线示例零网络零凭据；`scripts/run_examples.sh` 一次
跑完全部离线示例（网络示例只编译检查）。所有 `main` 统一走
`common::run_big_stack`（64 MiB 线程）——GNU/Windows 主线程默认 2 MiB，
debug 下 tool 往返的异步 future 更深（`plugin.rs` 修复前会栈溢出），
这样 `cargo run --example …` 开箱即用。`--no-default-features --features sdk`
的 lib 构建也已修复（`tail_page` 的 `session` feature 门控 + print-only
辅助函数门控），所有 feature 组合 0 警告。

## 3. 事件 / Hook / 插件

- `WIRED_NATIVE_EVENTS` **49 个**，`UNSUPPORTED_NATIVE_EVENTS` 为空；
  JS `pi.on` 与 native `AgentPlugin::on_event` 均已注册并被 `hook_coverage` 校验。
- `agent_settled` / `entry_appended` exactly-once 回归通过；print SessionEvent sink
  覆盖 compaction/retry/background/summarization；`before_provider_headers`
  覆盖每个 provider 请求且在 auth 头之前执行。
- P2-01 已关闭：`PluginHost::emit_with_runtime` 统一 agent-loop/tool 的有序
  walk，JS adapter 只在 path-plan 槽位执行一次，typed result 链式写回。
- P2-02（native per-call deadline）、P2-03（ToolService scheduler/gate）、
  P2-04/06/07、P2-EVENT3-02 均已关闭。
- `hook_coverage` 新增静态 emit-site gate：49 个 wired event 每个都必须能在
  非测试源码中找到 dispatch marker 或 SessionEvent bridge 映射；不再只校验
  doc↔registry 常量。
- `/reload` 的 `resources_discover` 只投递一次（`ExtensionHandle::reload`
  不再自行 emit，由 session 统一派发）；`on_init`/`on_reload`/`on_shutdown`
  现在与 `on_event` 一样有 `catch_unwind` 隔离。
- `pi.on("terminal_input")` 现在注册到真正的 terminal-input 列表，
  `ctx.ui.onTerminalInput` 与 `pi.on` 共用同一 dispatch；native tool 注册
  补齐 label/promptSnippet/promptGuidelines/prepareLoadout。
- TUI 改为一个 session-event bridge：自动/手动 compaction、user
  `EntryAppended`、`SessionInfoChanged` 现在都进入 UI（手动 `/compact`
  不再重复发一对 compaction frame）。新增 `tui_session_event_frames` 9 tests。
- 第二轮对齐：`emit_native` 遵守 `cancel`/`block`/`handled` 短路并补齐
  `input` transform 回退；`before_provider_request` native 结果链式写回
  （相等 fast-path）；`terminal_input` 原生 fan-out 与 raw UI 回调有 panic
  隔离 + per-handler deadline，首 chunk 前补跑 one-shot `on_init`；
  read-only `session_tree` 入口也先跑 `on_init`；事件名校验统一走
  `is_known_event`（接受 `entry_appended` 等合成可达名）；`session_before_*`
  的 `Generic` 结果改为字段 overlay 并尊重 `cancel:true`；JS dispatch 关闭时
  hosted walk 跳过 JS adapter。新增回归：registry union/无重复、49 个
  wired 名逐一通过真实 `pi.on`、init/reload/shutdown exactly-once。

## 4. 剩余边界

- `message_update` 保留 JS/native 两条独立有界队列（per-delta 性能路径），
  不参与 result-contract 的统一 walk；两者都在 `message_end` 前 drain。
- server/client 不加载 extension runtime（产品边界）。
- print 的 main loop provider 已改为 print host 的 session-scoped
  `ProviderSessionScope`（与 branch/compaction summary 相同）；`--no-session`
  仍使用 run-scoped key，不伪造 session id/file。
- 子代理 fallback 仍共享父的 JS 定义/tool execution（仅事件投递已隔离）。
- widen 底板约 205 MB（140 MB `Vec<Message>` transcript + ~65 MB 进程/allocator
  floor）。消息槽已从 592 B 收到 **408 B**（`AssistantMessage` 400 B、
  `ToolResultMessage` 400 B；`deferred`/`nested_calls` 改为 boxed optional，
  100 MB 会话预计再省 ~14 MB，待复测）；要稳定进 100 MB 以内仍需把
  `Context.messages` 改成流式 representation。
- 带 JS 扩展的 TUI 峰值 ~97.5 MB（JS runtime floor ~25 MB），JS 内核瘦身
  是下一个结构性空间（N 线进行中）。
- `#[allow(clippy::too_many_arguments)]`：**70 → 0**（`src/` 与 `tests/`
  全部清零；`construction::model` 改 `ModelSpec`，compaction/背景任务/
  session 快照改命名参数结构体）。`src/` 外层 `#[allow]` 剩 36 个（主要是
  `large_enum_variant` 与平台条件代码）；tests 侧保留测试专用的
  `await_holding_lock` 等豁免。
- 插件合并契约修复：mutation-only handler 返回 `None` 时，先前 handler 的
  replacement 不再覆盖它刚写入 `payload` 的修改（`before_provider_request`
  等 result-shaped 事件）；新增跨 handler 回归测试。
- 已关闭（L 线端到端验证）：`PluginHost::reload`/`reload_native_plugins`
  现按 `session_shutdown{reload} → on_reload → JS reload →
  session_start{reload}（exactly-once）` 的顺序统一发生命周期（AgentSession
  不再二次发）；`on_context`/`on_session_context`/`statuses` 同步回调有
  `catch_unwind` panic 隔离，后续插件不受影响。回归：
  `reload_native_w14` 10/10、`plugin_events_w16` 19/19、`hook_coverage`
  26/26、`plugins_unified`(模块) 21/21。

## 5. 复现

```bash
export PATH=/c/wp/empty/tools/mingw/mingw64/bin:$PATH
export RUSTUP_TOOLCHAIN=stable-x86_64-pc-windows-gnu
export CARGO_TARGET_DIR=target
cargo fmt --check
cargo check --tests --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --doc --locked
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/test_fast.ps1 -Concurrency 8 -Threads 4
```

## 6. 证据

- 全量测试：`.audit/verify/test-fast-final7.log`
- Release 性能：`.audit/perf/perf-run-2026-11-01.md`
- Debug 性能：`.audit/perf/debug-perf-2026-11-02.md`
- 内存优化：`.audit/perf/memory-opt-2026-11-02.md`
- RSS 优化：`.audit/perf/rss-opt-2026-11-02.md`
- widen 流式 / hook 门控 / 启动堆：`.audit/perf/lane-efg-2026-11-02.md`
- A/B 汇总：`.audit/perf/ab-2026-11-02.md`
- SDK bench：`benches/sdk_benchmark.rs`（运行 `cargo bench --bench sdk_benchmark --profile dev`）
- SDK 性能/内存审计：`.audit/perf/sdk-2026-11-03.md`（8 阶段 dhat 归因、64 KiB 小栈回归、AgentInit）
- Lazy history：`.audit/perf/lazy-history-2026-11-02.md`
- 事件/hook：`.audit/special/events-hooks.md`
- 插件 P2：`.audit/special/plugin-p2-backlog.md`、`.audit/fix/lane-p2-final.md`

## 7. 2026-11-03 收尾实测（本机，debug profile，--quick）

本轮关闭/暴露的项：TUI `/model <ref>:<level>`（thinking 后缀）、原生插件 SessionService
的 new_session/fork/navigate_tree/switch_session（改为记录 SessionCommandAction）、
http_poll_pacing 的连接上限回归、phone 父会话图片断言、mcp 阻塞派发用例、
上游漂移重审计（`docs/upstream-drift-status.md`：184 / 110 / 62，其中 31/33/34/42/49 本轮落地）。

| bench（debug） | median |
|---|---:|
| `durable_context_read_100mb` | 52.9 ms |
| `jsonl_recovery_open_100mb` | 2.87 s |
| `jsonl_recovery_open_10mb` | 153 ms |
| `print_paged_prompt_context_100mb` | 598 ms |
| `session_search_keystroke_1k` / cold | 52.8 ms / 126 ms |
| `session_search_listing_100` / 1k+outlier | 423.5 ms / 497 ms |
| `durable_document_commit_no_observer` | 69.0 ms |
| `tui_dirty_frame_512` | 1.48 ms |

峰值：`print_widen_projection_100mb` **206.6 MB**；`print_paged_prompt_context_100mb`
**104.7 MB**（max allocation 1 MiB）；`jsonl_recovery_open_100mb` 143.0 MB；
`session_search_listing_1k_outlier` 2.28 MB。

| SDK bench（debug） | median |
|---|---:|
| `sdk_agent_build_in_memory` | 11.4 ms |
| `sdk_prompt_roundtrip_small` | 12.6 ms |
| `sdk_prompt_stream_small` | 12.0 ms |
| `sdk_transcript_read_medium` | 389 µs |
| `sdk_transcript_json_medium` | 3.53 ms |
| `sdk_session_attach_medium` | 12.6 ms |
| `sdk_event_bus_emit_8` | 301 ns |
| `sdk_event_projection_entry_appended` | 3.42 µs |

门禁（本轮实跑）：`cargo fmt --all -- --check` EXIT 0；
`cargo clippy --all-targets --all-features --locked -- -D warnings` EXIT 0；
定向测试 `tui::slash::tests` 4/4、`plugin_session_actions` 4/4、`http_poll_pacing` 6/6、
`phone_child_inline` 11/11、`mcp_integration` 70/70（含此前 ignored 的阻塞派发用例）。

**源码卫生门禁**：`src/ai/transport.rs`、`src/coding_agent/tools/shell_pool.rs`、
`src/tui/slash.rs` 的内联 `#[cfg(test)]` 模块已迁到独立测试二进制
（`tests/lane_e_streaming_file_body.rs`、`tests/shell_pool_prewarm.rs`、
`tests/tui_slash_model_argument.rs`），`source_hygiene` 两项恢复绿。

**遗留项已清零**：`plugin_registers_lazily_and_exposes_tools` 由
`3022965`（`/reload` 不再经自身 `session_start` 重新发现）修复；
`request_deadline_bounds_a_blocked_dispatch` 已解除 ignored —— 截止时间在派发**之前**
上弦、响应循环的 `notifications/cancelled` 改为 detach 发送，定时器不再被阻塞的传输写挡住。
`cargo test --test mcp_integration`：**70 passed / 0 failed / 0 ignored**。


## 8. 2026-11-03 上游漂移收口（代码核实）

`docs/upstream-drift-status.md` 的 9 项 Open/Partial 已全部关闭并按代码证据
修正：handoff 后的 postTools 续跑、deferred 节流提交与 partial append、
`getDistinctModelName`、device-code 确认、markdown transformer context、
HTML 导出 system prompt/update、只读 Harness、adoption 顺序（单写者等价）
与 watch retirement（原文档两项过期结论已改正）。Rex 检测与
`checkpointWhen` 必填同时落地。

门禁：`cargo test --test g_durable` **747/747**；定向
`tui_model_names` 2/2、`tui_drift_w1` 19/19、`tui_commands_parity` 22/22、
`plugin_renderer_completion` 8/8、`d32_resume_active_tools` 11/11、
`export_html` 15/15、`source_hygiene` 3/3；`cargo test --doc` 33/0；
`cargo clippy --all-targets --all-features --locked -- -D warnings` EXIT 0；
`cargo fmt --all -- --check` EXIT 0。

全量 `test_fast.ps1`（318 suites，2026-11-04 复跑）：**6774 passed / 0 real
failures**（并行阶段 2 个 load-flaky，串行重试全部通过：`g_agent` 940/940、
`lane_event3_json_host` 3/3；13 个空 harness 不计入）。本轮修复了
`unsafe_gate` 的 allowlist、`tui_render_parity` 的 scratch-pool helper、
`tool_repair` 的 mutation-only result 同步、native-only
`extension_ui_request` 镜像，以及 `session_lock_scopes`/`sdk_units`/
`lane_event3_json_host` 的测试隔离问题。

**2026-11-04 续作**：`bd3c9b823` 的 `/tree` 分支 loadout 已由
`AgentSession::restore_branch_active_tools` 补全（`d32` 11/11，含
`tree_navigation_restores_the_target_branch_loadout`）；110-commit 窗口
16 个未检查提交已全部给出结论（详见 drift 文档“Checked”表）。同时补上
Windows PowerShell 安装目录回退（`61060ddf8`）、Unix 剪贴板 helper 独立
进程组（`f8ab362cc`）和 GitHub Copilot 默认模型 `gpt-6.1-sol`
（`e5fe9777b`）。回归：`g_ai` 763/763、`g_tui` 777/777、`g_durable` 747/747。

`28bea006b` 的 Windows Store shell alias 校验也改用
`GetFileAttributesW`（`shell_path_exists`）；两个窗口已无未处理项。
