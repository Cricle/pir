"""Doc consolidation step 2: reference rewrites + deletions.

Absorbed docs (content now lives elsewhere):
  context.md            -> HANDOFF.md (§2 decisions, §5 protocol, §7 tiers)
  plan.md               -> HANDOFF.md §9 (open work) + WAVE-REPORT.md (history)
  DRIFT.md              -> UPDATING.md §2/§3 (judgments + closed ledger)
  PERF-SCOPE.md         -> HANDOFF.md §8 (harness list) + §9 (perf residuals)
  DURABLE-SCOPE.md      -> HANDOFF.md §9 item 4
Superseded artifacts removed: the diagnostics-channel wave reports under
scripts/node_tests (archived in logs/phase2/node-suite-20261015/), a stale TUI
battery smoke log, and the generated .pytest_cache.
"""

import os
import shutil
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def patch(rel: str, pairs) -> None:
    path = os.path.join(ROOT, rel)
    with open(path, encoding="utf-8", newline="") as fh:
        text = fh.read()
    nl = "\r\n" if "\r\n" in text else "\n"
    for old, new in pairs:
        old_n = old if nl == "\n" else old.replace("\n", "\r\n")
        new_n = new if nl == "\n" else new.replace("\n", "\r\n")
        if old_n not in text:
            print(f"MISSING in {rel}: {old[:90]}", file=sys.stderr)
            sys.exit(1)
        text = text.replace(old_n, new_n, 1)
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(text)


def main() -> int:
    refs = {
        "src/extensions/api/node_shims.rs": [
            ("permanent boundary per context.md §2.4", "permanent boundary per HANDOFF.md §2.3"),
        ],
        "src/extensions/api/js/process_global_stub.js.txt": [
            ("reach follow the three-tier discipline (context.md §2)", "reach follow the three-tier discipline (HANDOFF.md §2)"),
        ],
        "src/extensions/api/js/vm_shim.js.txt": [
            ("Route decision (context.md §2.4 / §11.3)", "Route decision (HANDOFF.md §2.3)"),
        ],
        "src/extensions/api/js/pi_env_shim.js.txt": [
            ("the scope decision recorded in DURABLE-SCOPE.md §3", "the scope decision recorded in HANDOFF.md §9"),
        ],
        "tests/differential_node.rs": [
            ("(context.md §2 tiering", "(HANDOFF.md §2 tiering"),
            ("non-goal (context.md §10)", "non-goal (HANDOFF.md §2.6)"),
        ],
        "tests/web_globals.rs": [
            ("The engine-global rule (context.md §3)", "The engine-global rule (HANDOFF.md §3)"),
        ],
        "src/plugins/codemode.rs": [
            ("(see `DRIFT.md` D23)", "(see the `UPDATING.md` ledger, D23)"),
        ],
        "src/plugins/builtin/mcp.rs": [
            ("see `DRIFT.md` D23 for the", "see the `UPDATING.md` ledger (D23) for the"),
        ],
        "tests/tui_drift_w1.rs": [
            ("(DRIFT.md W1, TUI half)", "(UPDATING.md ledger, W1 wave, TUI half)"),
        ],
        "tests/upstream_d3_strict_tool_fallback.rs": [
            ("(DRIFT.md W1)", "(UPDATING.md ledger, D3)"),
        ],
        "tests/upstream_w2_drift.rs": [
            ("(DRIFT.md): the breaking", "(UPDATING.md ledger): the breaking"),
        ],
        "tests/upstream_w1_drift.rs": [
            ("(DRIFT.md): one place to pin the", "(UPDATING.md ledger): one place to pin the"),
        ],
        "scripts/perf/p3_task_e2e.mjs": [
            ("P3 real-task E2E (plan.md 阶段 1)", "P3 real-task E2E (WAVE-REPORT.md §2 evidence)"),
            ('"# P3 real-task E2E (plan.md 阶段 1)"', '"# P3 real-task E2E (WAVE-REPORT.md §2)"'),
        ],
        "scripts/perf/ttft_curve.mjs": [
            ("(`plan.md` 阶段 1 evidence,", "(`WAVE-REPORT.md` §2 evidence,"),
        ],
        ".migration-info": [
            ("DRIFT.md W1/W2/W3, D1–D33", "UPDATING.md ledger D1–D33"),
            ("DRIFT.md §4)", "UPDATING.md §1)"),
            ("DRIFT.md §5 window", "UPDATING.md §1 window"),
        ],
    }
    for rel, pairs in refs.items():
        patch(rel, [(a, b) for a, b in pairs])

    for rel in [
        "plan.md",
        "context.md",
        "DRIFT.md",
        "PERF-SCOPE.md",
        "DURABLE-SCOPE.md",
        "scripts/tui_conpty/RUN-LOG-20261015-smoke.md",
        "scripts/node_tests/REPORT-dc-baseline.md",
        "scripts/node_tests/REPORT-dc-after2.md",
        "scripts/node_tests/REPORT-dc-control.md",
    ]:
        path = os.path.join(ROOT, rel)
        if os.path.exists(path):
            os.remove(path)
            print("deleted", rel)
    cache = os.path.join(ROOT, ".pytest_cache")
    if os.path.isdir(cache):
        shutil.rmtree(cache)
        print("deleted .pytest_cache/")
    print("reference rewrites + deletions done")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
