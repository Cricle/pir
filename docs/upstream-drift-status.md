> Historical. Superseded by [STATUS.md](STATUS.md); kept as the audit record.

# Upstream drift status — `3daa20280..2ec55d808` (74 commits)

Read-only static audit, 2026-10-21. Method: `git log`/`git show` in the
upstream clone, then code evidence in this tree.

## Summary

| Status | Count |
|---|---:|
| Landed (code evidence verified) | 33 |
| Partial (core landed, integration gap recorded) | 2 |
| N/A by decision / scope / test-only / docs / Nix | 34 |
| In flight (telemetry, W12-A; since landed — see MAPPING `src/telemetry/`) | 2 |
| Superseded by a later commit in the same window | 2 |
| Open / out-of-scope contradiction (SQLite `prefix`) | 1 |
| **Total** | **74** |

## Non-landed rows

- `7ec1f648e` **Partial** — `HarnessOptions.settings` is a closure; `models`
  is not a function (model access stays on generation/tool seams).
- `d7f7e0dfe` **Partial** — `resolve_model_reference` serves CLI/SDK/credential
  paths; TUI `/model <term>` still uses `tui/slash.rs` resolution without the
  credential tie-break / thinking suffix.
- `f7e3cb543` **Open/out-of-scope** — namespaced SQLite `prefix`; MAPPING
  records it out of scope.
- `2ec55d808`, `48909d987` **WIP at audit time** — telemetry; landed
  afterwards (MAPPING `src/telemetry/`, `tests/suites/ai_telemetry_w11.rs`).
- `a3f265b1c`, `6a1f799b6` **Superseded** — final Kimi state is `b06c1f2cf`.
- Every other commit in the window is **Landed** (33) or **N/A** (34) per the
  audit; the per-commit prose ledger was removed in the 2026-10-29 cleanup.
  Landing evidence is frozen in the code and tests named by
  [`MAPPING.md`](../MAPPING.md).

## Residual items

1. **SQLite `prefix`** — decide scope: MAPPING says out of scope; no code
   dependency depends on it.
2. **`ctx.ui.custom`** — continues N/A while custom overlays are unsupported
   (`docs/hooks.md`); upstream's close-path fix needs no separate port.
3. **No gate evidence** — this audit is static only; it does not replace
   `cargo check --tests`, `g_*`, ConPTY, soak, release or A/B. Current
   evidence: [`STATUS.md`](STATUS.md).
