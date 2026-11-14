# 系统提示词审计（pir vs upstream pi）

> 状态：**基线审计（仅审计，未修复）+ 2026-11-13 复核**。基线：pir `a07d4d2`（2026-11-04），上游 pi `38ff99177`（2026-10-24）。
> 复核只更新已确认修复的行（B14、M1/M4 的 RPC 部分，§4/§5 内已标注）；其余行仍是基线快照。
> 方法：只读检索 + 源码逐段对照。

## 1. 结论

pir 的核心段落构建器与上游 `system-prompt.ts` 基本同构：段落顺序、XML 包装、规则去重、
skills XML、`hiddenTools` 过滤、provider cache marker 大多一致。实质问题集中在：

1. `SYSTEM.md` / `APPEND_SYSTEM.md` 只在 print 生效，TUI/RPC/SDK 未接入。
2. RPC 的 prompt cwd 使用进程 cwd，不是 session cwd；`--skill` / `--no-skills` 对 RPC 无效。
3. token 估算漏掉 `Context.system_prompt` 和 `Context.tools`，影响 max_tokens、compaction、
   `context_usage`。
4. subagent 在裸工作区且 agent body 非空时只发送 body，丢失默认 preamble/tools/rules/cwd；
   body 位置在 `</cwd>` 之后，不是 `<addendum>`。
5. `before_agent_start` 修改 `selectedTools` 只改提示文本，不改实际工具声明。
6. 自定义 section 与内置 section 同名时生成重复 XML 标签。
7. MCP section 会列 direct server 的失败原因，可能泄漏 URL/token 片段；上游只列 indirect
   server 的 description/instructions，不输出状态错误。
8. 默认工具顺序：pir 按字母序，上游为 `read, bash, edit, write`。

## 2. 组装顺序（pir 与上游一致）

`build_system_prompt_sections`（`src/system_prompt.rs:474-554`）：

1. `preamble`（默认或 override，不加 XML）
2. `tools`（仅 `promptSnippet` 非空的工具）
3. `rules`（条件 shell 规则 + 工具 guidelines + 固定规则，trim 后去重）
4. `docs`（解析出的 package dir 含 `README.md` + `docs/` + `examples/` 时）
5. `addendum`（CLI/SDK append，以 `\n\n` 连接）
6. `project_context`（AGENTS.md/CLAUDE.md）
7. `skills`（`<available_skills>` XML）
8. `cwd`
9. 自定义 sections（按插入顺序）

默认 preamble：

```text
You are an expert coding assistant operating inside pir, a coding agent harness.
You help users by reading files, executing commands, editing code, and writing new files.
```

上游对应文本为 `... operating inside pi ...`（`packages/coding-agent/src/core/system-prompt.ts:155-156`）。

动态变量：cwd、工具列表、工具 snippet/guidelines、context files、skills、MCP 状态、
`PI_PACKAGE_DIR`。date/platform/model/permissions 两边都没有注入。

## 3. 已对齐项

- 段落顺序与 XML 包装（preamble 除外）。
- 工具列表只列 `promptSnippet`，空列表为 `(none)`。
- 规则构建、trim、`BTreeSet` 去重、固定两条规则。
- `project_context` 模板与 header。
- `addendum` 连接方式、`<cwd>` 反斜杠转 `/`。
- skills XML 形状、转义、`disableModelInvocation` 过滤、reader hint 三种文案。
- `hiddenTools` 过滤与 indirect reader 行为。
- `customPrompt` 只替换 preamble/tools/rules/docs，保留后续段落。
- 扩展 `systemPromptOptions` 字段形状与 `forceSystemPrompt`。
- context file 候选顺序、BOM 剥离、worktree shadow。
- provider 层 cache marker（Anthropic/OpenAI）。
- `before_agent_start` options 可修改并重渲染。

## 4. 偏差（B）与缺失（M）

### 高优先级

| # | 问题 | pir 证据 | 上游语义 | 影响 |
|---|---|---|---|---|
| B1 | `docs` 段落条件注入 | `src/system_prompt.rs:432-443,510-512`；`PI_PACKAGE_DIR` 常为空 | `system-prompt.ts:162-169` 无条件注入 | 默认 prompt 缺 docs/examples 路径，模型不知道文档位置 |
| B2 | 默认工具顺序字母序 | `src/agent/tool_execution.rs:1178-1190` 对 HashMap key `sort()` | `DEFAULT_TOOL_NAMES = read,bash,edit,write` | tools/rules 顺序与缓存前缀变化 |
| B3 | bash/powershell snippet 改写 | `bash.rs:1652-1656`、`powershell.rs:150-154` 引导 `PIR_BIN` | upstream 原文 `Execute bash commands (ls, grep, find, etc.)` | 改变工具选择行为，无法逐字 parity |
| B4 | MCP section 语义不同 | `system_prompt.rs:358-412` 列所有 enabled server + `failed: reason` | `mcp/index.ts:150-223` 只列 indirect，description/instructions，不输出错误 | 重复/冗余；错误文本可能泄漏 URL/token |
| B5 | 自定义 section 与内置同名 → 重复标签 | `system_prompt.rs:479-490,536-553`；`Custom("tools") != Tools` | `system-prompt.ts:184-191` 同名 key 直接覆盖 | 出现两个 `<tools>`，行为不可预测 |
| B6 | print 的 `APPEND_SYSTEM.md` 与 CLI append 叠加 | `print.rs:3343-3346` | `resource-loader.ts:694-706` CLI append 存在时不发现文件 | 重复注入；host 行为不一致 |
| B7 | SDK `.append_system_prompt` 追加在末尾 | `build.rs:951-996`、`builder.rs:1980-1990` | 进入 `addendum`，位于 project_context/skills/cwd 之前 | 段落顺序不一致、无法结构化消费 |
| B8 | SDK `.system_prompt` 整段替换 | `build.rs:356-359`、`builder.rs:1970-1995` | `customPrompt` 仅替换 preamble/tools/rules/docs，保留其余段落 | SDK 用户丢失 AGENTS.md/skills/cwd |
| B9 | `before_agent_start.selectedTools` 不应用到 loadout | `prompt_loop.rs:52-127,1196-1199` | `agent-session.ts:1926-1975` 应用到本次请求工具声明 | prompt 与请求声明不一致（高） |
| B10 | 无 JS runtime 时 options 的 context/skills 为空 | `session_ctx.rs:414-418,483-489` | loader 始终提供 agentsFiles/skills | native plugin 重渲染会丢 project_context/skills |
| B11 | `promptSnippet` 未单行化 | `system_prompt.rs:202-211` 只 trim | `agent-session.ts:1692-1700` 换行/空白折叠为单行 | 多行 snippet 破坏列表 |
| B12 | 顶层 `promptGuidelines` 缺少核心 builder 字段 | `SystemPromptInput` 无字段（`system_prompt.rs:152-181`） | `BuildSystemPromptOptions.promptGuidelines` 公开 | 嵌入方无法表达 top-level guidelines |
| B13 | 非法 section 名只 warn 跳过 | `system_prompt.rs:479-490` | `system-prompt.ts:144-148` 抛错 | 配置错误静默失效 |
| B14 | RPC prompt cwd 用进程 cwd | **已修复（2026-11-13 复核）**：`rpc.rs:3030-3031,3064` 传 session cwd；`build.rs:344-348` 的 `current_dir` 回退只在没有 session 时生效（基线为 `rpc.rs:2705` `cwd: None`） | `sdk.ts:192` 取 `options.cwd ?? sessionManager.getCwd()` | 基线时 `<cwd>`、AGENTS.md、skills 全部错位 |
| B15 | context file 严格 UTF-8、相对 cwd 不绝对化 | `files.rs:51-58`、`context_files.rs:133-153` | Node 替换非法字节；先 `resolvePath(cwd)` | 非 UTF-8 AGENTS.md 整体消失；相对 cwd 漏祖先 |
| B16 | `SystemMessage.sections` 用 `BTreeMap` | `messages.rs:233-247`；`transcript.rs:170-182` 按 values 拼接 | JS object insertion order | 恢复/导出 transcript 时段落重排 |
| B17 | `PIR_SYSTEM_PROMPT_FILE` 读取失败中止 print | `print.rs:3315-3317` | `resource-loader.ts:194-205` warning + fallback | 失效路径导致 headless run 失败 |
| B18 | `SystemPromptOptions::from_json` 解析失败静默 default | `system_prompt_options.rs:160-168` | 逐字段 normalize | 一个类型错误丢掉整份 options |
| B19 | 4 条 pir-only `<rules>` bullet | `src/system_prompt.rs:269-290` | `buildRules` 只有工具 guidelines + 2 条固定规则 | **刻意偏差（已决策）**：见 §4.1 |

### 4.1 已决策的刻意偏差（product decisions）

以下三项在 round-1 prompt A/B（`.play/evidence/ab-round1/lane-ab-prompt/REPORT.md`）
中被判为“未记录的分歧”。复核后确认它们对应 pir 真实存在的能力，因此**保留行为**，
只补文档 + 回归测试（不再当作 bug 反复上报）：

| 审计项 | 决策 | 依据 | 回归 |
|---|---|---|---|
| B1 `docs` 段落条件注入 | **已定案（刻意偏差，PIR-BIN-061 运行时探针）**：`package_docs_paths` 仅在解析出的 package dir 同时含 `README.md`、`docs/`、`examples/` 时输出 `<docs>`，否则整段省略（`PI_PACKAGE_DIR` 优先，未设置时探测可执行文件同目录）；上游始终输出其包路径，pir 的验收底线是 `<docs>` 永不点名不存在的路径 | `src/system_prompt.rs:453-505`、`src/config/env.rs:35-56`、上游 `system-prompt.ts:102-108,157-169`、`config.ts:393-405` | `tests/suites/prompt_loadout_w22.rs::docs_section_*`、`package_dir_*`；`.play/lanes/lane-fix-docs69/evidence/061-probes/result.json` |
| B2 默认工具顺序 | **已修复**：`PrintToolLoadout::definitions` / `print_active_definitions` 直接从 active-set 顺序（`declared_tools`）构建声明数组，与 `<tools>` 段落同序；`ToolRegistry::iter/names` 的确定性排序仅保留给无序路径 | `agent-session.ts:1502-1512,3707-3712`、`system-prompt.ts:151-160` | `prompt_loadout_w22::declared_tool_array_order_matches_the_tools_section_order` |
| B19 4 条 pir-only rules | **保留（刻意偏差）**：这 4 条对应 pir 真实的强制行为 — `PIR_READ_BEFORE_EDIT` 默认 `enforce` 的读前门（`tools/util.rs:25-60`）、字面路径校验（`tools/util.rs:365-370`）、`tools.broadSearchGuard`（默认 true，`settings/env.rs:97`）以及收尾自检；上游没有这些机制，删掉规则会让模型看不到实际会被拒绝的约束。代价约 +709 chars/request，仅出现在对应工具被声明时 | 本表 §4 B19 + `docs/environment-variables.md` | `prompt_loadout_w22::pir_only_rules_*`（逐字列表 + 工具门控） |

**默认工具集（22 = 9 核心 + 13 内置，上游 4）同样是刻意偏差**，2026-11-14 由
`.play/live-assist/x10-tool-loadout.md` 定案（Option B：保留装载，修正文档）；见
[`FEATURES.md`](FEATURES.md) § 3.1：`balanced` 扩展默认集（grep/find/ls/background_*
五个核心 + `ctx_*`/`python`/`todo`/`memory`/`subagent` 十三个默认启用的内置工具）是
产品能力，`efficient` profile 或 `--tools read,bash,edit,write` 都可切回上游四工具集，
`--no-extensions` 保留核心九工具，且注册表从不被选择过滤（F12）。之前的“9”只统计了
核心层；全量构建的实际声明是 22（工具定义 JSON 约 +30.7 KB/首次请求，其中内置层
约 26 KB）。PIR-BIN-062/063 以“已记录刻意偏差”关闭，061（`<docs>` 门控）维持原判。

### 缺失

| # | 缺失 | 说明 |
|---|---|---|
| M1 | TUI/RPC/SDK 不加载 `SYSTEM.md`/`APPEND_SYSTEM.md` | **RPC 已修复（2026-11-13 复核）**：print（`print.rs:3771`）与 RPC（`rpc.rs:3078-3092`）都调 `discover_system_md`；SDK 需显式 `.discover_system_prompt(true)`（`builder.rs:628`）；TUI 仍只处理 CLI override |
| M2 | SDK `ResourceLoader` 接口缺 prompt/context 能力 | `sdk/resources.rs:34-42` 只有 `reload/skills/prompt_templates`；上游有 `getAgentsFiles/getSystemPrompt/getAppendSystemPrompt` |
| M3 | `diff_sections` / `SystemMessage.sections` 增量机制未接线 | `system_prompt.rs:596-612` 无调用者；请求始终发送完整字符串；`normalize_context` 未被 provider 使用 |
| M4 | RPC/SDK 的 `--skill`/`--no-skills` 不进入 prompt 发现 | **RPC 已修复（2026-11-13 复核）**：`rpc.rs:3096-3106` 把 `--skill`/`--no-skills` 传进 `DiscoveredResources`；其余 host 仍硬编码空 `cli_skill_paths`/`no_skills=false`（`context.rs:944`）；SDK 仅在资源非空时安装 loader |
| M5 | subagent 裸工作区丢 base prompt | `subagent/child.rs:415-424` 在 `context_system_prompt_with_hidden_in` 返回 `None` 时只发 body；上游子进程始终有默认 prompt，body 作为 `appendSystemPrompt` |
| M6 | codemode 没有 `promptSnippet`/`promptGuidelines` | `plugins/codemode.rs:3763-3770` 未覆盖 trait 默认；上游 `codemode/tool.ts:108-113` 有 |
| M7 | 顶层 `promptGuidelines` 未进入 `SystemPromptInput` | 同 B12 |
| M8 | 双方都没有 date/platform/model/permissions | 不是 pir 相对上游的偏差，属于产品需求 |

## 5. Host 差异矩阵

| Host | context files | skills | SYSTEM/APPEND | CLI override | MCP | 主要问题 |
|---|---|---|---|---|---|---|
| TUI | 是 | 是（资源发现生效） | **否** | 是 | 是 | 无 SYSTEM/APPEND |
| print | 是 | 是 | **是** | 是 | 是 | APPEND 与 CLI append 叠加（B6） |
| RPC | 是（session cwd） | 默认发现；`--skill`/`--no-skills` 生效 | **是**（2026-11-13 复核） | 是 | 是 | B14/M4 已修复 |
| SDK | 构建时默认发现 | loader 仅在非空时安装 | **否** | `.system_prompt`/`.append_system_prompt` | 是 | append 位置（B7）、显式 prompt 丢 context（B8） |
| subagent | 子 cwd 发现 | 子 cwd 发现 | **否** | agent body 追加 | 子 session | 裸工作区丢 base prompt（M5） |

## 6. 测试现状

已有覆盖主要在 `tests/suites/system_prompt.rs`、`system_prompt_options_units.rs`、
`session_context_cli.rs`、`tests/subagent_builtin.rs`、`tests/suites/sdk.rs`、
`tests/suites/extensions_events.rs`、`tests/suites/print_mode.rs`、`tool_search_declaration.rs`。

关键未覆盖：

1. TUI/RPC/SDK 的 `SYSTEM.md`/`APPEND_SYSTEM.md` 集成。
2. RPC 的 cwd、`--skill`、`--no-skills` prompt 行为。
3. SDK `without_skills()` 且无 prompt templates 时是否真正清除 skills。
4. 顶层 `promptGuidelines` 渲染/位置/去重。
5. `promptSnippet` 多行 normalize。
6. 默认工具顺序 `read, bash, edit, write`。
7. 内置工具 contribution 逐字比对（bash/powershell/codemode）。
8. 自定义 section 与内置同名。
9. MCP section 只列 indirect、description/instructions、错误脱敏。
10. token 估算包含 system prompt/tools（clamp/compaction/context_usage）。
11. subagent 裸工作区 + 非空 body 的 base prompt，以及 body 的 addendum 位置。
12. `before_agent_start.selectedTools` 实际 loadout 同步。
13. `SystemPromptOptions::from_json` 畸形输入。
14. 非 UTF-8 AGENTS.md / 相对 cwd 祖先发现。
15. `PIR_SYSTEM_PROMPT_FILE` 读取失败容错。
16. `SystemMessage.sections` 的插入顺序。
17. 空/重复 AGENTS.md。
18. `docs` 段落在真实 host 的端到端断言。

## 7. 修复优先级

### P0（先修）

1. **统一 ResourceLoader**：所有 host 读取 `SYSTEM.md`、`APPEND_SYSTEM.md`、AGENTS.md、skills。
   目标：`src/coding_agent/resources.rs`、`src/sdk/resources.rs`、`src/tui/terminal.rs`、
   `src/coding_agent/rpc.rs`、`src/agent_session/build.rs`。
2. **token 估算计入 `system_prompt + tools`**：`src/ai/provider.rs:3569-3585`、
   `src/agent_session/compact.rs:783-788`、`src/agent_session/stats.rs:101-127`；同时复核
   `CHARS_PER_TOKEN=4.0` 与上游 3.5 的差异。
3. **subagent 始终构建无条件 base prompt，body 走 `<addendum>`**：
   `src/plugins/subagent/child.rs:415-424`、`src/coding_agent/context.rs:641-662`。
4. **`before_agent_start.selectedTools` 真正应用到 loadout**：
   `src/agent_session/prompt_loop.rs:52-127,1196-1199`。
5. **RPC 使用 session cwd 并传资源路径**：`src/coding_agent/rpc.rs:1044-1050,2661-2718`。

### P1（中优先级）

1. `docs` 段落始终输出或显式配置开关（`system_prompt.rs:432-443,510-512`）。
2. 默认工具顺序恢复 `DEFAULT_TOOL_NAMES`（`tool_execution.rs:1178-1190`）。
3. 补齐 codemode snippet/guidelines（`plugins/codemode.rs:3763-3770`）。
4. 自定义 section 与内置名冲突：拒绝或覆盖（`system_prompt.rs:479-490,536-553`）。
5. MCP section 向 upstream 靠拢（`system_prompt.rs:358-412`、`mcp/manager.rs:738-778`）。
6. SDK `.system_prompt` 改 `preamble_override` 语义，append 走 `<addendum>`（`build.rs:356-359`、`builder.rs:1970-2086`）。
7. `promptSnippet` 单行化（`system_prompt.rs:202-211`）。
8. RPC/SDK 接入 `--skill`/`--no-skills`，空资源也安装覆盖（`context.rs:758-781`、`builder.rs:2078-2086`）。
9. context file 超限 warning、非 UTF-8 容错、相对 cwd 绝对化（`context_files.rs:21-35,122-153`、`files.rs:51-58`）。

### P2（一致性/清理）

1. print 的 APPEND 优先级改为 CLI 覆盖 discovered（`print.rs:3343-3346`）。
2. 非法 section 名升级为可配置 fatal（`system_prompt.rs:479-490`）。
3. `SystemPromptOptions::from_json` 返回 `Result`（`system_prompt_options.rs:160-168`）。
4. `SystemMessage.sections` 改保持插入顺序的结构或固定 builder 顺序渲染（`messages.rs:233-247`、`transcript.rs:170-182`）。
5. `PIR_SYSTEM_PROMPT_FILE` 失败 warning + fallback（`print.rs:3315-3317`）。
6. `diff_sections` 死代码删除/标注，或接入 section 机制（`system_prompt.rs:596-612`）。
7. `SystemPromptInput` 增加 `prompt_guidelines` 字段（`system_prompt.rs:152-181`）。

## 8. 最终判断

段落构建器质量较高，模板文本、规则顺序、skills XML、hiddenTools 过滤和 provider cache
marker 与上游一致。真正需要先处理的是**资源加载统一、token 计量、subagent base prompt、
selectedTools 实际应用、RPC cwd** 五项运行时一致性问题；其余是输出文本/顺序/host 行为偏差。
