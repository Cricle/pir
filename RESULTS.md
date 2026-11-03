# PIR 最终结果

- 基线：`master` @ `b26a60fb23bd45306401190cacb5b6f2c1f5459c`
- 结果日期：2026-11-02
- 本文件是唯一面向用户的结果文档；过程材料仅在 `.audit/` 保留为原始证据。

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

