# Upstream v1.5.2 follow-up plan (updated 2026-11-17)

Scope: what remains from the 2026-11-15 v1.5.2 audit after the 2026-11-16
batches and the 2026-11-17 TUI work. Per-commit evidence lives in
[`upstream-drift-status.md`](upstream-drift-status.md) and `CHANGELOG.md`;
this page is only the remaining plan.

## Landed since the audit

The 2026-11-16 passes closed the compatibility/wire, tools/runtime, and TUI
batches from the v1.5.2 audit; the per-item record is in `CHANGELOG.md` and
[`upstream-drift-status.md`](upstream-drift-status.md). Two later landings are
the current baseline:

- **#11630 host-resolved keybinding hints**: the TUI pushes `keyText`/
  `keyHint` into tool/message/entry render contexts and re-pushes after
  `/reload`; headless hosts keep the empty-hint upstream behavior.
- **Activity TUI (2026-11-17)**: `/subagents` + `ctrl+shift+a` show live and
  recent subagent runs with keyboard selection and per-run abort; `/todos`
  opens a keyboard-selectable list (status advance, delete, details); native
  builtins can queue presentation notifications without a JS runtime;
  background tool completions append a compact transcript notice.

Gate evidence before this batch: full nextest **7570 passed / 0 failed /
46 skipped**, doctests **33 passed / 7 ignored**, fmt/clippy clean, CI green
on `b480b5f`. Re-run the full gate after the activity/TUI batch.

## A/B status

Old fixtures (q1–q3, c1–c3) were measured at N=5 and N=3; the 2026-11-17
N=3 run had pir 18/18 with 150 turns / 206 tools / 2.20M tokens / 554.5s vs
pi 18/18 with 176 / 266 / 2.54M / 642.3s.

The 2026-11-17 four-category matrix (short, long, QA, real upstream release
tooling) is the broader check: both arms 60/60; pi used fewer tokens, turns,
tools, and time in aggregate, while pir had a slightly lower QA citation
invalid rate. Results are task- and run-dependent (one long-task outlier and
one transport retry move category totals), so the matrix does **not** support
a general "pir is cheaper/faster" claim. Full numbers and caveats:
[`RESULTS.md`](../RESULTS.md). Single-model/single-gateway remains the main
limitation; a second model is still the cleanest next experiment.

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
