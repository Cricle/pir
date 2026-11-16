# Upstream v1.5.2 follow-up plan (2026-11-16)

Scope: what remains from the 2026-11-15 v1.5.2 audit after the 2026-11-16
batches. Evidence for the audit rows lives in `.play/evidence/upstream-v152/`
(local only); landed items are recorded in
[`upstream-drift-status.md`](upstream-drift-status.md) and `CHANGELOG.md`.

## Landed in the 2026-11-16 passes

Compatibility/wire: LiteLLM/Zhipu overflow and cut-short retry patterns,
project-trust precedence, OpenRouter token margin, `requiresNonNullAssistantContent`,
`promptCacheKeyMode` (Azure retention-independent), `requiresToolsWithToolHistory`,
OpenAI Responses `session-id`, provider ids with `/`, new default-model map,
`CLAUDE_CODE_VERSION`, `qwen-token-plan-individual-cn`, OpenCode endpoints,
Copilot classifier, Codex display name, cache-warming `idle`.

Tools/runtime: active-tool set order, branch-summary thinking level and context
edits, live thinking record, grep lone CR plus engine-supplied context/limit
semantics, hidden-thinking empty label, smaller PNG/JPEG, npm `--no-audit
--no-fund`/`realpath`/unchanged-version warning, prompt-template quote rules,
tool-search name/exact/coverage/field ranking and local `$ref` following,
`pi.exec` `maxBuffer`/`truncated`, MCP `login`/`logout -l` with a name
suggestion and `authProvider` reporting, WSL helper cwd, external-editor
failure text, deleted-cwd startup diagnostic, canonical `auth.json` lock,
Dart/Nix/Groovy highlighting, Windows PowerShell 5.1 exit suffix, transport
cause chains, diff indentation, custom find `limit + 1`, Windows case-folded
resource/package/project-trust keys, the refresh-only MCP refresh guard and
sign-in's stored-client refresh, root `$defs` in non-strict Anthropic
schemas, Unicode tool-search segmentation in both tokenizers, web-URL-only
OAuth opening with control-character stripping in sign-in displays, SDK
runtime disposal that settles an aborted run, autocomplete/`/settings`
PageUp/PageDown, and host-resolved `keyText`/`keyHint` hints in extension
render contexts.

Gate evidence for the latest commits: full nextest **7570 passed / 0 failed /
46 skipped**, doctests **33 passed / 7 ignored**, fmt/clippy clean, 43 catalog
files canonical, CI green on `ab28770` and rerun for the newer commits.

## A/B extended validation

Status: **done for one model** (2026-11-16). The sequential N=5 campaign ran
with `new-provider-3` (`tinygate`, Anthropic messages,
`deepseek-v4.1-flash`): Q&A q1/q2/q3 and coding c1/c2/c3, both arms alternating
per repetition, no concurrency. Raw data:
`.play/ab/results/runs-ab-n5-{qa,coding}-final5.jsonl`; user-facing medians and
caveats: [`RESULTS.md`](../RESULTS.md). Three Q&A cells hit provider transport
failures in rep1 (recorded; retried up to three attempts); the other 27 cells
completed, so the campaign is final evidence for this one model only.

`run_ab_pi.py` takes `AB_PROVIDER_ID`/`AB_MODEL`/`AB_BASE_URL`/`AB_WIRE_API`
and rewrites both arms' `models.json` without storing the key. A second-model
leg remains possible if another provider is restored; record the single-model
limitation until then.

## Wave A — complete

1. **Extension keybinding hints (#11630).** Landed: the TUI resolves the
   settings/keybindings-file text through
   `Keybindings::keybinding_hints_json` and pushes it with
   `ExtensionHandle::update_keybinding_hints`; the prelude injects
   `keyText`/`keyHint` into `ToolRenderContext`, `MessageRenderOptions`, and
   `EntryRenderOptions`, and re-pushes after `/reload`. Headless hosts never
   push, so their renderers format empty hints like upstream RPC/print. Test:
   `tests/renderer_parity.rs::renderers_receive_host_resolved_keybinding_hints`.

## Wave B — medium, needs a small design pass

1. **Fork option for a missing session cwd (#11060, `4ea88379`).** Extend the
   TUI/startup prompt to Continue/Fork/Cancel, write the fork with the current
   cwd and parent session id, and fix `/import` copying before the cwd check.
2. **Prompt/tree navigation overlap (#9154).** Reserve a prompt-preflight
   count across input hooks/auth checks so `navigate_tree` refuses while a
   prompt is between submit and `_startRun`.
3. **`pi auth login/logout` flags (#11823).** Add `--no-browser`,
   `--api-key`, and the Radius MCP credential hook.
4. **`ctx.ui.custom()` blocked overlays (#11851).** Per-overlay program-status
   sources with reset on `/reload` and session switches.
5. **Package update version summary (#53/#54).** Record per-source
   before/after versions and only claim "up to date" after an actual check.
6. **Queued steer/follow-up image normalization (#11862).** Resize at delivery
   time, not only for the idle prompt path.
7. **Strict-tool rejection memory (#11860).** Remember per-session strict-mode
   rejections instead of retrying blindly.
8. **Bash timeout after shell exit (#11613).** Report the shell's exit code in
   the timeout message when pipes stay open; the Windows path needs a design
   decision because the current reader-grace loop already returns before the
   deadline.

## Wave C — deferred feature families

Anthropic message threads, Bedrock strict-tool fallback, the pi-schema
coercion/validation superset, extension-package changelog UI, MCP client-ID
metadata `auto`, Markdown-transformer message surfaces, temporary npm/git
source scopes, the edit-splice engine, and the views/settings/Chord feature
families. These need their own design and data refresh; they are not parity
bugs.

## Unverified (no portable surface established)

`b2cb6292` (settle credential writes before exit) and `80d4d3c8` (reset
extension overlays on `/reload`). Re-check only if the Rust architecture grows
the corresponding seam.
