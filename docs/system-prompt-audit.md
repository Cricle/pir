# System prompt: pir vs upstream pi

Current state of the system-prompt builder against upstream
`packages/coding-agent/src/core/system-prompt.ts`. The unreduced per-row audit
(baseline pir `a07d4d2`, 2026-11-04, upstream `38ff99177`) is archived at
[`archive/system-prompt-audit-baseline-2026-11-04.md`](archive/system-prompt-audit-baseline-2026-11-04.md).

Builder: `crates/pir-core/src/system_prompt.rs` (`build_system_prompt_sections`,
the `render_*_section` helpers). The facade still exposes the historical
`pir::system_prompt` path.

## 1. Section order

`build_system_prompt_sections_with_rules` emits, in order:

1. `preamble` — default text, or an override (an override drops 2–4)
2. `tools` — only tools with a non-empty `promptSnippet`; `(none)` when empty
3. `rules` — conditional shell rules + tool guidelines + fixed rules, trimmed
   and deduplicated
4. `docs` — only when the resolved package dir has `README.md` + `docs/` +
   `examples/`
5. `addendum` — CLI/SDK append, joined with `\n\n`
6. `project_context` — AGENTS.md / CLAUDE.md
7. `skills` — `<available_skills>` XML
8. `cwd`
9. custom sections, in insertion order

This matches upstream. Default preamble:

```text
You are an expert coding assistant operating inside pir, a coding agent harness.
You help users by reading files, executing commands, editing code, and writing new files.
```

Upstream says `… operating inside pi …`. Neither side injects
date/platform/model/permissions.

## 2. Aligned with upstream

- XML wrapping and section order (preamble aside).
- Tool list rendering: `promptSnippet` only, `(none)` for an empty set.
- Rule construction, trim, `BTreeSet` dedup, the two fixed rules.
- `project_context` template and header.
- `addendum` joining, `<cwd>` backslash → `/`.
- skills XML shape, escaping, `disableModelInvocation` filtering, the three
  reader hints.
- `hiddenTools` filtering and indirect-reader behavior.
- `customPrompt` replacing only preamble/tools/rules/docs.
- Extension `systemPromptOptions` shape and `forceSystemPrompt`.
- Context-file candidate order, BOM stripping, worktree shadow.
- Provider cache markers (Anthropic/OpenAI).
- `before_agent_start` options mutation and re-render.

## 3. Host matrix

| Host | context files | skills | SYSTEM/APPEND | CLI override | MCP |
|---|---|---|---|---|---|
| TUI | yes | yes | yes | yes | yes |
| print | yes | yes | yes | yes | yes |
| RPC | yes (session cwd) | yes, `--skill`/`--no-skills` honored | yes | yes | yes |
| SDK | at build time | loader installed only when resources are non-empty | no — needs `.discover_system_prompt(true)` | `.system_prompt` / `.append_system_prompt` | yes |
| subagent | child cwd | child cwd | no | agent body appended | child session |

## 4. Known deviations

### Documented product decisions (keep, do not re-report)

| # | Deviation | Why it stays | Regression |
|---|---|---|---|
| B1 | `<docs>` section is omitted unless the package dir really has `README.md` + `docs/` + `examples/` (PIR-BIN-061 runtime probe; `PI_PACKAGE_DIR` wins, else the executable's directory is probed) | Upstream always prints its package path. pir's contract is stronger: `<docs>` never names a path this install lacks — a release archive ships only `README.md`, `cargo install` ships no assets. | `prompt_loadout_w22::docs_section_*`, `package_dir_*` |
| B19 | Three pir-only `<rules>` bullets | Each encodes an enforced gate upstream lacks: literal-path validation, `tools.broadSearchGuard` (default true), and the finish self-check. Removing them hides constraints the model will actually hit. **`efficient` profile exception**: it drops only the advisory finish self-check; the two gated rules stay in both profiles (`PromptRuleOptions { self_check: false }`). A fourth bullet (`read it completely, no offset/limit, before overwriting with write`) was **removed**: upstream has no such bullet, and a measured bug hunt obeyed it literally, pulling whole 48 KB / 35 KB files into context and driving repeated compaction. The constraint it described is enforced in code (`PIR_READ_BEFORE_EDIT`, default `edit-partial`: `write` to a never-read file is refused, a partial read warns). | `prompt_loadout_w22::pir_only_rules_*`, `no_read_before_write_bullet_is_rendered`, `efficient_profile_omits_only_the_advisory_finish_rule` |
| — | Default tool set is a superset of upstream's four | Product capability, decided 2026-11-14 (Option B: keep the loadout, fix the docs). Canonical list and cost measurement: [`FEATURES.md`](FEATURES.md) §3.1. `efficient` or `--tools read,bash,edit,write` selects the upstream four; `--no-extensions` leaves the core nine; the registry is never filtered by the selection. | `FEATURES.md` §3.1 |

### Open deviations

| # | Deviation | pir location | Upstream behavior | Effect |
|---|---|---|---|---|
| B2 | **Resolved**: the default tool order now leads with upstream's core four | `crates/pir-core/src/agent/tool_execution.rs` (`active_names_after`); `definitions`/`print_active_definitions` build the declared array from the active set | `DEFAULT_TOOL_NAMES = read, bash, edit, write` | The no-`--tools` path used to be a plain `sort()`, which put `read` at position 21 of 23 once the `ctx_*`/`bg_*` tools were declared. It now sorts by `(index in DEFAULT_TOOL_NAMES, name)`, so the first four lines match upstream and everything else stays alphabetical. An explicit `--tools` keeps its own order either way. Pinned by `declared_tool_array_order_matches_the_tools_section_order`; the order cue is the reason `read` is used at all — see `docs/system-prompt-audit.md` §4 measurement notes |
| B3 | bash/powershell snippets point at `PIR_BIN` | `coding_agent/tools/bash.rs`, `powershell.rs` | `Execute bash commands (ls, grep, find, etc.)` | Changes tool-selection behavior; no byte parity possible |
| B4 | MCP section lists every enabled server plus `failed: <reason>` | `system_prompt.rs` (`render_mcp_section`) | Only indirect servers, description/instructions, no status errors | Redundant lines; error text can echo URL/token fragments |
| B5 | A custom section sharing a built-in name emits a duplicate tag (`Custom("tools") != Tools`) | `system_prompt.rs` (section insert + custom loop) | Same-named key overwrites | Two `<tools>` blocks, unpredictable for the model |
| B6 | print stacks a discovered `APPEND_SYSTEM.md` on top of the CLI append | `coding_agent/print.rs` | CLI append suppresses file discovery | Double injection; hosts disagree |
| B7 | SDK `.append_system_prompt` appends at the very end | `agent_session/build.rs`, `sdk/builder.rs` | Goes into `addendum`, before project_context/skills/cwd | Section order differs; cannot be consumed structurally |
| B8 | SDK `.system_prompt` replaces the whole prompt | `agent_session/build.rs`, `sdk/builder.rs` | `customPrompt` replaces only preamble/tools/rules/docs | SDK users silently lose AGENTS.md/skills/cwd |
| B9 | `before_agent_start.selectedTools` changes prompt text but not the request's tool declarations | `agent_session/prompt_loop.rs` | Applied to the outgoing request | Prompt and request disagree |
| B10 | Without a JS runtime the options carry empty context/skills | `agent_session/session_ctx.rs` | Loader always supplies agentsFiles/skills | A native-plugin re-render drops project_context/skills |
| B11 | `promptSnippet` is trimmed but not flattened to one line | `system_prompt.rs` | Newlines/whitespace collapsed | A multi-line snippet breaks the list |
| B12 | `SystemPromptInput` has no top-level `promptGuidelines` | `system_prompt.rs` | Public `BuildSystemPromptOptions.promptGuidelines` | Embedders cannot express top-level guidelines |
| B13 | Invalid section names warn and are skipped | `system_prompt.rs` | Upstream throws | Config errors fail silently |
| B15 | Context files must be strict UTF-8; a relative cwd is not absolutized | `coding_agent/files.rs`, `context_files.rs` | Node replaces invalid bytes; `resolvePath(cwd)` first | A non-UTF-8 AGENTS.md disappears; relative cwd misses ancestors |
| B16 | `SystemMessage.sections` is a `BTreeMap` | `agent_session/messages.rs`, `transcript.rs` | JS object insertion order | Restored/exported transcripts reorder sections |
| B17 | A failed `PIR_SYSTEM_PROMPT_FILE` read aborts print | `coding_agent/print.rs` | Warns and falls back | A stale path kills a headless run |
| B18 | `SystemPromptOptions::from_json` failure silently falls back to defaults | `system_prompt_options.rs` | Per-field normalize | One type error discards the whole options object |
| M2 | SDK `ResourceLoader` lacks prompt/context accessors | `sdk/resources.rs` | `getAgentsFiles` / `getSystemPrompt` / `getAppendSystemPrompt` | SDK cannot reload prompt inputs |
| M3 | `diff_sections` is never called; requests always send the full string | `system_prompt.rs` (`diff_sections`) | Incremental section updates | Dead code + no incremental path |
| M4 | Hosts other than RPC hardcode empty skill args | `coding_agent/context.rs` | Resources drive discovery | `--skill`/`--no-skills` ignored outside RPC |
| M6 | codemode does not override `promptSnippet`/`promptGuidelines` | `plugins/codemode.rs` | codemode supplies both | Tool under-described in the prompt |
| M8 | Neither side injects date/platform/model/permissions | — | — | Not a pir-vs-upstream gap; a product question |

Fixed since the baseline audit: B14 (RPC prompt cwd), M1 (print/RPC/TUI
`SYSTEM.md`/`APPEND_SYSTEM.md`), M4-RPC (`--skill`/`--no-skills`), and M5
(subagent base prompt). SDK still needs an explicit
`.discover_system_prompt(true)`.

### Measurement notes: what decided B2

B2 is the only deviation here that was changed on measured evidence rather than
by design preference, so the evidence is recorded. The task was a single
reproducible bug hunt (`packages/durable/test/harness-inbox.test.ts`, 6 failing
of 37), run through both CLIs with `--mode json`, the same provider/model, 240 s
ceiling, clean session dirs, and a fixture in which the bug is the committed
state (no `git diff`, no reachable fix commit, no build artifact holding the
answer — `git diff`/`git show` had been handing the model the patch outright
before that).

Hypotheses that measurement falsified:

- *"bash's snippet advertises ls/grep/find, so the model never calls `read`."*
  Upstream pi's first six calls on this task were **all** bash (`cat`, `grep`,
  `sed`, `ls`), exactly like pir's. The snippet wording does not decide this.
- *"Narrowing the bash snippet to non-inspection work steers the model to
  `read`."* Two runs with the narrowed snippet came out **worse** than the
  widened one (read 8→3 and 2→1, tool output 224 KB / 184 KB → 393 KB / 389 KB).
  Reverted.
- *"Listing the 12 `ctx_*` tools in `<tools>` makes the model use them."* Six
  runs across four configurations produced **zero** `ctx_*` calls, with or
  without their `prompt_snippet`. Those snippets are still present only because
  they cost nothing measurable and the loadout is unchanged either way.

What the runs did show:

- `read` and `bash` order matters. Upstream lists `read` first and `bash`
  second; pir's `sort()` put `bash` first and `read` **21st of 23** once the
  `ctx_*`/`bg_*` tools were declared. Every run that ever called `read` at all
  was one where the `<tools>` section had grown; the two runs on the plain
  alphabetical 11-tool section called it zero times.
- Leading with the core four is the only change that moved the FIRST tool call.
  Opening six calls per run (most recent config last):
  `base [bash×6]`, `base [bash×9]`, `ctx [bash×5,read,…]`,
  `narrow [bash×6,read,…]`, then with the core four first
  `order [read,ls,bash,bash,bash,read,…]` and `order [bash,read,bash,bash,bash,read,…]`.
  Both ordering runs call `read` within the first two calls; no earlier
  configuration ever did. Total counts moved the same way (13/4 `read` against
  0/0 on the alphabetical baseline, and `bash` fell to 11 in one run).
- The effect is noisy, not deterministic. Upstream pi itself, unchanged across
  two runs of the same task, produced 8 `read` calls and 1 — and pir's ordering
  runs gave 13 and 4. Direction is consistent; magnitude is not. This section
  therefore reports counts rather than claiming a fixed improvement, and B2 is
  the only deviation here changed on this evidence.

## 5. Remaining work

Runtime consistency (highest value):

1. **Token estimation ignores `Context.system_prompt` and `Context.tools`** —
   affects `max_tokens`, compaction, and `context_usage`. Sites:
   `crates/pir-core/src/ai/provider.rs`, `agent_session/compact.rs`,
   `agent_session/stats.rs`. Also re-check `CHARS_PER_TOKEN=4.0` vs upstream's
   3.5.
2. **`before_agent_start.selectedTools` must reach the loadout** (B9) —
   `agent_session/prompt_loop.rs`.

Text and ordering: B2 default tool order, B11 snippet flattening, B19-adjacent
profile gating already decided.

Host behavior: B6 print append precedence, B7/B8 SDK prompt semantics, M4
non-RPC skill args, B17 fallback.

Cleanup: B5 duplicate tags, B13 fatal invalid names, B18 `Result` return, B16
ordered sections, M3 delete or wire `diff_sections`, B12 add
`prompt_guidelines`.

## 6. Tests

Existing coverage: `tests/suites/system_prompt.rs`,
`system_prompt_options_units.rs`, `session_context_cli.rs`,
`tests/subagent_builtin.rs`, `tests/suites/sdk.rs`,
`tests/suites/extensions_events.rs`, `tests/suites/print_mode.rs`,
`tool_search_declaration.rs`, `tests/suites/prompt_loadout_w22.rs`.

Gaps worth closing, in the order the deviations above are listed: SDK
`SYSTEM.md` wiring, non-RPC `--skill` handling, `promptSnippet` normalization,
default tool order, builtin snippet parity, duplicate custom section names, MCP
error redaction, token estimation including prompt+tools, `selectedTools`
loadout sync, malformed `SystemPromptOptions`, non-UTF-8 AGENTS.md, relative cwd
ancestors, `PIR_SYSTEM_PROMPT_FILE` failure tolerance, section insertion order,
empty/duplicate AGENTS.md, and an end-to-end `<docs>` assertion in a real host.
