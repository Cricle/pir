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
resource/package/project-trust keys.

Gate evidence for the latest commits: full nextest **7570 passed / 0 failed /
46 skipped**, doctests **33 passed / 7 ignored**, fmt/clippy clean, 43 catalog
files canonical, CI green on `ab28770` and rerun for the newer commits.

## A/B extended validation

Status: **blocked on providers**. ZCode `new-provider-2` answers
`401 Invalid token`; `new-provider-1`/`new-provider-4` report insufficient
quota; `new-provider-3` works but intermittently drops connections, which
made the three-way-concurrent N=5 campaign retry/fail (2–7 of 10 cells per
task). Partial provider-3 results are local under
`.play/ab/results/runs-ab-n5-qa-final2-q*.jsonl` and are **not** final
evidence. `run_ab_pi.py` now takes `AB_PROVIDER_ID`/`AB_MODEL` and rewrites
both arms' `models.json` without storing the key.

Plan once a working key/quota is available:

1. Sequential (no concurrency) `--reps 5` Q&A for q1/q2/q3, then `--reps 5`
   coding for c1/c2/c3, with both arms alternating per repetition.
2. If provider-2 is restored, use it for a second-model leg; otherwise record
   the single-model limitation.
3. Summarize success rate, keyword hits, valid citations, tokens and elapsed
   medians; keep raw data in `.play/` and land only the conclusions in
   `RESULTS.md`.

## Wave A — small, ready to implement

Each item is independent and should stay a separate commit with a regression
test that cites the upstream commit.

1. **MCP refresh-only guard (#11743, `bb266bf35`).** A failed refresh calls
   `authorize_mcp(..., FlowOptions::default())` and can re-enter the browser
   flow / overwrite a pending verifier. Add a refresh-only flow option and use
   the sign-in document's redirect URI; make the refresh lock wait abortable
   (`797e122cb`).
2. **Anthropic root `$defs` (#11959, `4a6a58eb5`).** `legacy_input_schema`
   drops `$defs`; keep the root `$defs` alongside
   `type`/`properties`/`required` for non-strict tools.
3. **Unicode tool-search segmentation (#11037).** `tokenize` is ASCII-only;
   normalize + segment non-ASCII words so non-English tool descriptions match
   (also check the durable copy).
4. **Extension keybinding hints (#11630).** The JS render/message context
   lacks `keyHint`/`keyText`; inject the host's resolved bindings instead of
   the static shim table.
5. **Selection page keys (#7629 residual).** Autocomplete and the settings
   submenu lack PageUp/PageDown while the other lists have them.
6. **SDK dispose settles (#9126).** `AgentSessionRuntime::dispose` shuts down
   extensions without awaiting `session.abort()`; request the abort and wait
   for the run/compaction claim to clear before shutdown.
7. **Untrusted URL/terminal text (#11849).** Validate Radius
   `authorizationEndpoint`/`verification_uri` with the same http/https helper
   as OAuth metadata, and strip control characters before displaying device
   codes/URLs.

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
