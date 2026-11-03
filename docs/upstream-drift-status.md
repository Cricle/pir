# Upstream drift status — pi `38ff99177` (v1.4.0+62)

Static code audit of the port against the upstream clone at `c:/wp/empty/pi`
`HEAD = 38ff99177` ("fix(durable): continue runs after tool handoffs",
2026-10-24, tag `v1.4.0` + 62 commits). Read-only: every row is **code
evidence read from this tree**, not a test run. Nothing here replaces
`cargo check`, the test suites, ConPTY or release gates; current gate evidence
is [`../RESULTS.md`](../RESULTS.md) and the port map is
[`../MAPPING.md`](../MAPPING.md).

## Counts

| Range | Commits | Diff |
|---|---:|---|
| `3daa20280..38ff99177` — everything since the recorded baseline | **184** | 597 files, +44,149 −13,594 |
| `3daa20280..2ec55d808` — the 2026-10-19 audited window | 74 | (previous record) |
| `2ec55d808..38ff99177` — drift after that window | **110** | — |
| `v1.4.0..38ff99177` — drift after the v1.4.0 tag | **62** | 266 files, +12,716 −3,052 |

The baseline in `.migration-info` (`commit=3daa20280`, 2026-10-13) is
unchanged: this document refreshes the *drift* record, not the port baseline.
`v1.4.0` is `4a080252f`; `38ff99177` is the last commit of the audited set.

## Classification — all 62 commits in `v1.4.0..HEAD`

| # | Commit | Change | Status | Evidence / what is missing |
|---:|---|---|---|---|
| 1 | `38ff99177` | continue runs after tool handoffs | **Open** | `harness/generation.rs:1946` still computes `ending = terminate \|\| handoff.is_some()` and applies the handoff reset only when ending; upstream appends the reset, then hands the run to `postTools` in the handoff's context. |
| 2 | `6c2fd73b0` | deferred storage of throttled commits | **Partial** | Partial throttle exists (`generation.rs:74-77`, inline awaited flush) but there is no `partial-changes` counterpart: every flush writes its payload into the commit instead of deferring throttled writes. |
| 3 | `d0aa218cb` | cheaper container copies in Delta's tracker / applyImmutable | N/A | The chord tracker is not ported (replaced by the full-draft diff, `durable/chord/mod.rs:1-20`); `durable/chord/apply.rs` takes the revision by value and has no container-copy path to tune. |
| 4 | `f321e5bdd` | docs(chord) delta README, appendText | N/A | Documentation for the unported tracker; see row 3. |
| 5 | `1de2d7ade` | Kimi K3.1 Pro treated as K3 family | **Landed** | `catalog/moonshotai.json:1` — `kimi-k3.1-pro`/`-highspeed` carry the K3 compat (`thinkingFormat:"openai"`, `supportsReasoningEffort:true`, `supportsMidConvoToolAdditions`) and costs 2/10 and 4/20; `catalog/kimi-coding.json:1` — `k3.1-pro` priced 2/10 with the same compat. |
| 6 | `bdb151774` | PendingOps fuzz-test headroom | N/A | Test-only, and `PendingOps` itself is not ported. |
| 7 | `d8d99950a` | PendingOps folding; Watch delivers once per busy period | N/A | chord `watch` is out of scope by decision (`MAPPING.md:23`; `durable/chord/mod.rs:1-20`). |
| 8 | `1e42501bb` | keep `pi.live` cached in a cost test | N/A | Test-only. |
| 9 | `da412b039` | `appendText`; durable partial commits append | **Partial** | No `append_text`/`appendText` anywhere in `src`; the `pi.live` draft is staged as a whole document each time (`live_to_object`, `harness/compaction.rs:580`, `harness/generation.rs` boundary staging), so a growing string is re-serialized rather than appended. The chord half is N/A (row 3). |
| 10 | `f715a15c1` | parse streamed tool-call arguments incrementally | **Landed** | `ai/api/anthropic.rs:2228` (`parse_streaming_tool_arguments` per `partial_json` delta), `ai/api/openai/sse.rs:121,442` (incremental JSON reader on `tool_call_delta`), `ai/api/bedrock.rs:1454`, `ai/api/mistral.rs:1131`. |
| 11 | `136b87e8a` | changelog policy docs | N/A | Docs/process only. |
| 12 | `acf83acfd` | restore changelog attributions | N/A | Docs only. |
| 13 | `e9c3ac9d9` | drop changelog author attribution | N/A | Docs only. |
| 14 | `a8fe8e0ad` | missing changelog entries | N/A | Docs only. |
| 15 | `1c8cb1781` | bound image line search by tallest image | N/A | pir emits image escape sequences inline and has no Kitty placement/scroll-crop layer (only encoding: `tui/terminal_image.rs:363`), so the placeholder-row search does not exist. |
| 16 | `4464442f9` | crop boxed images scrolled above the viewport | N/A | Same missing placement layer as row 15. |
| 17 | `a7b94a03f` | preserve literal data in Gemini tool schemas | **Landed** | `ai/api/google/shared.rs:717` (six name maps, incl. `dependencies`), `:728` (sixteen subschema keywords), `:753-782` (rewrite only in schema-bearing positions). |
| 18 | `7314819f0` | send `const` as `enum` in Gemini tool schemas | **Landed** | `ai/api/google/shared.rs:692` applies `rewrite_const_as_enum` to `parametersJsonSchema`; `:775-782` rewrites string/number `const` to a single-value `enum`. |
| 19 | `ed76cca69` | fork test asserts copied content | N/A | Test-only (behaviour covered by row 20). |
| 20 | `8ddc44ec8` | write a forked session through one fd | **Landed** | `session_manager/mod.rs:3567-3585` builds the whole fork body in memory and writes it once through a single handle (temp file, fsync, rename) — the per-entry reopen the commit removes cannot occur. |
| 21 | `f3fa8bd92` | do not count faux cache writes as input | N/A | pir has no `faux` provider in `src` (only JS shim names in `tests/ai_shims.rs`); the accounting path does not exist. |
| 22 | `606af8662` | show model names in model selectors | **Open** | Selector rows are `id [provider]` only (`tui/selector.rs:328-346`); the highlight block states pir "carries no display name in the selector state" (`selector.rs:680-683`). Missing: `getDistinctModelName`-style "name adds information" test and the ` (name)` suffix in both `/model` and `/scoped-models`. |
| 23 | `5e106ab1c` | `NativeWatchable` | N/A | No watch layer in the port (rows 7, 36, 40). |
| 24 | `3953e9ef4` | todo example returns undefined on validation errors | N/A | Upstream ships it as `packages/coding-agent/examples/extensions/todo.ts`; `examples/` here holds Rust SDK examples only. |
| 25 | `c55972086` | changelog entry | N/A | Docs only. |
| 26 | `c8ec752cf` | skip fd/rg downloads on unsupported architectures | N/A | pir searches in-process with the `ignore`/`grep-*` crates (`coding_agent/tools/grep.rs:1-6`) and never downloads `fd`/`rg` binaries. |
| 27 | `ff6896471` | iTerm2 word characters for double-click selection | **Landed** | `tui/altscreen_search.rs:121` — `WORD_SELECTION_JOINERS = ["/","-","+","\\","~","_","."]`, used by `word_selection_columns` (`:3354-3365`). |
| 28 | `bc73eb8b1` | remove stale Rex comment | N/A | Comment removal in the image module. |
| 29 | `ede2c9b46` | docs + `--help` text fixes | N/A | Documentation/`--help` copy for the Node CLI; pir documents its own `--export <file> [output]` (`coding_agent/cli.rs:654`). |
| 30 | `36be9c612` | decode `pi.exec` output across UTF-8 chunk boundaries | N/A | Node-only defect. pir captures raw bytes and decodes once at the end (`extensions/runtime/exec.rs:161-176`), so a split code point cannot be replaced by U+FFFD. |
| 31 | `8a2ebd385` | stop nested blockquote rendering growing exponentially | **Landed** | `tui/markdown/render.rs` — both quote renderers (`render_block_range` and `render_markdown_block_one`) now take the upstream `width < 3` branch: children render at the current width with no border and no re-wrap, so nesting cannot triple per level. Widths `>= 3` use `width - 2` directly (no `.max(1)` clamp). |
| 32 | `6dfb54678` | start when a configured package fails to install | **Landed** | `coding_agent/package_manager.rs:2992-2996` — an install error is recorded in `failures`, the package is skipped, startup continues; `:2507` warns instead of failing. |
| 33 | `9ffd27cfa` | accept only http/https OAuth URLs | **Landed** | `mcp/oauth.rs` — `safe_url` accepts only `http`/`https` (upstream `oauth/types.ts:126-134`), so `issuer`, `authorization_endpoint`, `token_endpoint` and `registration_endpoint` are all validated before the URL handler sees them; `authorization_servers` entries go through the same helper instead of a bare `Url::parse`. |
| 34 | `e3bc05e87` | fixed low top-level effort with Anthropic per-message effort | **Landed** | `ai/api/anthropic.rs:1173` — `ANTHROPIC_MANAGED_EFFORT = "low"` with the upstream rationale (organizations cap effort and reject higher values) on the constant; the trailing marker replays the same constant. `tests/suites/ai_adapters_w11.rs`, `tests/suites/ai_adapter_fixes.rs` assert `"low"`. |
| 35 | `3141dfdcf` | merge commit (watchable) | N/A | Merge only; both sides are classified in rows 36 and 37. |
| 36 | `a2fd25f56` | `Watchable` | N/A | chord watch out of scope (`MAPPING.md:23`). |
| 37 | `a1836b06c` | npm 12 `pack --json` in preview-packages | N/A | Release script (`scripts/preview-packages.mjs`); no counterpart here. |
| 38 | `14c811841` | `responseModel` on pi-messages terminal events | **Landed** | `ai/types/messages.rs:477-481` (`responseModel` field); propagated at `agent/agent_loop.rs:4499-4531`, `extensions/runtime/dispatch.rs:400-437`, `protocol/schemas/types.rs:201`. |
| 39 | `906e1302f` | Bun 1.4.4 for standalone binaries | N/A | CI workflow only. |
| 40 | `42cb822f1` | shared watch-handle implementation | N/A | chord watch out of scope (rows 7, 36). |
| 41 | `936e5f40a` | drop `autoevals` dependency | N/A | `packages/evals` is not ported. |
| 42 | `8c03fef4d` | rank contiguous fuzzy matches; search session fields separately | **Landed** | Session search: tiered fields plus contiguous-occurrence scoring (`session_manager/search.rs:1117-1118`, `:1296-1390`). Shared selector scorer: `tui/fuzzy.rs` is now `score_positions` + greedy alignment + "score every contiguous occurrence, keep the best" (upstream `fuzzy.ts:88-115`), so `/model`, autocomplete and settings search share the ranking. |
| 43 | `90d5cf784` | shorter device-code guidance | N/A | Docs only. |
| 44 | `7151099d0` | confirm device-code browser and clipboard actions | **Open** | No `OAuthDeviceCodeInfo`/`openBrowserOnConfirm`/`copyCodeOnConfirm` anywhere in `src`; device flows poll headlessly (`ai/auth/device_code.rs:54`) and the TUI login has no confirm row (`tui/terminal.rs:13561+`). |
| 45 | `c60de42e9` | preserve `pi.exec` spawn errors | **Landed** | `extensions/runtime/exec.rs:230-240` returns `stderr: "failed to spawn `<cmd>`: <err>"` with `code: 1` instead of an empty stderr. |
| 46 | `1c720d285` | HTML export deeper than the call stack | N/A | Node call-stack artifact in `template.js`; pir renders the export in Rust (`coding_agent/export_html.rs`) with no recursive JS tree walk. |
| 47 | `64627838b` | `messageId` + `timestamp` in markdown transformer context | **Open** | `plugins/services/mod.rs:1081-1084` — `transform(&self, markdown: &str)`; `agent_session/plugin_services.rs:3784` passes text only. Missing: the context argument (message id, timestamp) on the trait, the JS bridge and the call sites. |
| 48 | `9359adf0c` | render system messages in HTML export | **Partial** | System messages are rendered (`coding_agent/export_html.rs:557-562`) but as one `<pre>` block; the prompt/tool-list header and the "named section/tool update" structure the commit adds are absent. |
| 49 | `27cc6bfc0` | stop macOS shell groups before killing them | **Landed** | `coding_agent/tools/bash.rs` (unix tree-kill) — `#[cfg(target_os = "macos")] killpg(SIGSTOP)` before `killpg(SIGKILL)`, best effort, with the upstream race note (`shell.ts:239-251`); Linux keeps the direct kill (`tasklist_lock`). |
| 50 | `74f468640` | trust bot in PR gate | N/A | CI config. |
| 51 | `be14d9732` | open a Harness read-only | **Open** | `durable/harness/harness.rs:195-240` (`HarnessOptions`) has no read-only option and `Harness::open` (`:1384`) always starts the scheduler; no equivalent of upstream's read-only open/error. |
| 52 | `476a303c1` | drop `min-release-age` from managed `npm ci` | N/A | npm-invocation detail of the Node package manager. |
| 53 | `ac80dd88f` | approve contributors | N/A | Repository metadata. |
| 54 | `b9fb2b854` | renumber durable docs examples | N/A | Documentation/examples for `packages/durable`. |
| 55 | `92ab53602` | adopt callback inside the commit; watch types; explicit retirement | **Partial** | Adoption exists (`durable/session/assemble.rs:1100+`) but the module documents it as running *after* `Storage::commit` (`assemble.rs:16`), not inside it; chord `WatchHandle`/`WatchEnd` and explicit watch retirement are not ported. |
| 56 | `3bc6eebac` | hold Anthropic system updates past dropped assistant messages | **Landed** | `ai/api/anthropic.rs:1692-1694` — held updates flush only once the assistant message converted to blocks, so an empty/dropped assistant cannot leave an update before a user message. |
| 57 | `db774055e` | preview packages setup | N/A | CI/scripts only. |
| 58 | `2685de7a1` | dependency security updates | N/A | `package.json`/`package-lock` only. |
| 59 | `cae8d69e5` | OpenRouter-reported total cost | **Landed** | `ai/util.rs:445-484` (`apply_openrouter_reported_cost`, sets `totalFromProvider`), wired at `ai/api/openai/sse.rs:915-917` and `ai/api/anthropic.rs:2350-2351`; `ai/provider.rs:3605-3615` never overwrites a provider-billed total. |
| 60 | `5884ee681` | `/ir` imports a session from a `.jsonl` URL | N/A | The commit touches only `.pi/extensions/import-repro.ts`, a project-local user extension script; it is not part of the ported surface. |
| 61 | `7c22a93d0` | `totalFromProvider` flag on usage cost | **Landed** | `ai/types/stream.rs:27-53` (`totalFromProvider` serde field, `is_provider_billed`), set in `ai/util.rs:445-484`, `ai/classify.rs:254`, `ai/images.rs:443`; honoured at `ai/provider.rs:3605-3615`. |
| 62 | `58c0b02ce` | `[Unreleased]` changelog sections | N/A | Docs only. |

Totals: **17 Landed, 4 Partial, 5 Open, 36 N/A**.

## Open and Partial items

Open (behaviour absent, exact gap):

1. `38ff99177` — finish a tool round at `postTools` after a `handoff` reset
   (`generation.rs:1946`).
2. `606af8662` — model-name suffix in `/model` and `/scoped-models`
   (`selector.rs:328-346`, `:680-683`).
3. `7151099d0` — confirm-to-open-browser/copy-code on device login.
4. `64627838b` — message id + timestamp in the markdown-transformer context
   (`plugins/services/mod.rs:1081`).
5. `be14d9732` — read-only `Harness::open`
   (`durable/harness/harness.rs:195`).

Partial (core exists, one documented gap):

1. `6c2fd73b0` — throttled commits are stored inline, no deferred-storage path.
2. `da412b039` — no `appendText`; string drafts are rewritten whole.
3. `9359adf0c` — system messages exported as plain blocks, not as
   prompt/tool sections.
4. `92ab53602` — adoption runs after the commit, not inside it; watch
   retirement absent.

## Spot checks in `2ec55d808..v1.4.0` (110 commits)

Checked (23 of 110), all by the same method:

| Commit | Change | Verdict | Evidence |
|---|---|---|---|
| `7f225f25d` | image input for System One classifiers | Landed | `ai/classify/system_one.rs:166-230` (`SystemOneImageInput::{Assets,Images,StateImageUrls}`, per-service `add_images`, 4-image cap) |
| `02726086d` | recovery omits truncated call's tool results | not checked | — |
| `8c4e87389` | grep ignores ripgrep config | N/A | in-process engine, no spawned `rg` (`tools/grep.rs:1-6`) |
| `bd3c9b823` | activate extension tools registered after the loadout | Open | `agent_session/tools.rs:641-702` restores exactly the recorded names; `agent/tool_execution.rs:1163-1177` only activates at registration, so a later-registered extension tool stays inactive |
| `04a93bad8` / `adc5fa9f8` | client identity / attribution in pi-ai | Landed | `ai/provider.rs:3004-3024` (`default_attribution_headers`), `:1914-1917` |
| `adfc14461` | `/compact` instructions on split-turn prefix | Landed | `compaction/branch_summarization.rs:343-412` |
| `612a172e6` | escape regex metacharacters in fd queries | N/A | pir matches with its own scanner, no JS regex (`tui/altscreen_search.rs:625-628`) |
| `6e491cfa9` | retryable status codes as standalone numbers | Landed | `ai/util.rs:318-345` (ASCII lookaround equivalent) |
| `29057ece3` | retry Codex stream disconnects | Landed | `ai/util.rs:296-299` |
| `cedefc532` | ChatGPT Business workspace usage limit | Landed | `ai/util.rs:205-217` |
| `bdd9f0399` | `mcp.json` with a UTF-8 BOM | Landed | `config/mcp.rs:479` |
| `22dc6c26f` | expose `@earendil-works/pi-ai/models` to extensions | Landed | `extensions/api/shims.rs:187` |
| `885a564b3` | queue codemode scripts at the worker limit | Landed | `plugins/codemode_worker.rs:1249-1362` (semaphore admission with queueing) |
| `38847563f` | stack concurrent extension dialogs | Landed | `tui/terminal.rs:2163,2340-2366` (FIFO `dialog_queue` promotion) |
| `9df819de3` | restore terminal modes on exit without `stop()` | Landed | `tui/terminal.rs:20972` (`TerminalRestoreGuard`) |
| `2ed3118b0` | unclosed display math swallowing tables | Landed | `tui/markdown/latex_scan.rs:193-240` |
| `396ecd7b8` | Kitty images for Rex | Open | no `rex` branch in terminal detection (`tui/terminal_image.rs:120-168`) |
| `0ed1b0431` | detect Rex for OSC 8 hyperlinks | Open | same detection table; unknown terminals keep hyperlinks off (`terminal_image.rs:163-168`) |
| `26293e74f` | CJK emphasis in HTML export | N/A | upstream fix is in the `marked`-based template; pir converts markdown itself (`coding_agent/export_html.rs`) |
| `b2d0da6fe` | CJK emphasis next to punctuation | N/A | upstream adds `marked-cjk-friendly`'s `emStrong` tokenizer; pir's inline parser scans `**` delimiters directly (`tui/markdown/inline.rs`) |
| `fbc456d82` | xhigh thinking for Grok 4.3/4.5 | N/A | test expectation only |
| `73f8de80b` | recover from Copilot request-size overflow | Landed | `ai/util.rs:52,83` and `ai/api/mod.rs:530-541` (any HTTP 413 is overflow) |
| `d2e7d8b7c` | add MiniMax-M3.1-Flash | Landed | `catalog/minimax.json` (`MiniMax-M3.1-Flash` present) |
| `5c1736f82` | `checkpointWhen` required on document definitions | Partial | `durable/documents.rs:155,345` keep `checkpoint_when: Option<...>`, so a definition can still omit it silently |

Not checked (examples of what the window still hides): `16f5f0af4`
(IndexedDbStorage), `4759f09fb`/`89e309581`/`4c25119f5`/`cae9fd28a` (SQLite
WAL replication and its revert), `48c8f3228` (replicated connection wrapper),
`d9025a5f5` (SQLite telemetry store), `e5fe9777b` (retired models),
`fdce48eb4`, `530015d93`, `caa6967b6`, `61060ddf8`, `28bea006b`,
`ea25697e9`, `f8ab362cc`, `e28e70804`,
and the docs/changelog commits.

## Method

1. `git log`/`git show` in the upstream clone `c:/wp/empty/pi` enumerated the
   three ranges and, for each commit, its message **and** its file list — the
   list decides whether the change can have a counterpart here at all
   (`scripts/*.mjs`, `package-lock.json`, CI workflows, `packages/evals`,
   `packages/chord/watch.ts`, `.pi/extensions/*` are N/A by construction).
2. For every non-obvious row, the *behaviour*, not the commit title, was
   grepped in `src/`: `append_text`/`appendText`, `Watchable`/`NativeWatchable`,
   `PendingOps`, `total_from_provider`, `response_model`, `read_only`,
   `rewrite_const_as_enum`, `parse_streaming_tool_arguments`,
   `WORD_SELECTION_JOINERS`, `safe_url`, `ANTHROPIC_MANAGED_EFFORT`,
   `checkpoint_when`, `dialog_queue`, `MAX_CONCURRENT_LIVE_WORKERS`,
   `contains_retryable_status_code`, `default_attribution_headers`, and the
   respective catalog JSON entries.
3. Where the grep hit, the definition and at least one caller were read and
   cited by `file:line`; where it missed, the nearest related code was read to
   distinguish *Open* (the surface exists and the change is missing) from *N/A*
   (the surface does not exist in this architecture).
4. **Rows are static code evidence, not test runs.** No compiler, test suite,
   ConPTY, soak, A/B or release gate was executed for this audit; a `Landed`
   row says the code is present and reads like the upstream change, not that it
   passes any gate. Gate evidence lives in [`../RESULTS.md`](../RESULTS.md).
5. Deliberately not covered: `src/` and `tests/` were not modified by this
   audit, and no attempt was made to resolve the open items — they are recorded
   here so the next sync pass can be scoped from evidence.
