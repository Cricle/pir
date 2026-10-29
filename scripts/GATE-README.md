# GATE-README — the project's verification gates, in order

Every command here was run in this checkout, or is a direct reading of a script
whose behaviour was verified on 2026-10-21 (dry runs with stub `cargo` / stub
test binaries for the red paths). Numbers are cited with the log they came
from; a number without a source is marked *unmeasured*.

Two companion runbooks cover the gates that are **not** `cargo` invocations —
do not duplicate them, read them:

- `scripts/soak/GATE-RUNBOOK.md` — memory probes and the long-run soak.
- `scripts/perf/AB-RUNBOOK.md` — pir-vs-pi A/B campaign.

---

## 0. Environment (applies to every gate)

```powershell
$env:CARGO_TARGET_DIR = "C:\Users\XPENG_USER\Desktop\github\pir\target-alt"
```

- `target-alt` is the build directory. **`pir\target` belongs to the eval
  harness** — no gate writes there and no gate kills a process from there.
- Shell matters: the `.ps1` gates are written for **Windows PowerShell 5.1**
  (`powershell -NoProfile -File ...`), which is what CI/terminal invocations
  use. They run under PowerShell 7 too, but 5.1 is the tested contract.
- A run whose binary predates the change under test is a **smoke test**, not a
  measurement. Label it that way in the wave report.

---

## 1. The sequence

Run top to bottom. A red row stops the sequence: later gates cost minutes to
tens of minutes and prove nothing while an earlier one is red.

| # | Gate | Command | Proves | Measured wall time (source) |
|---|---|---|---|---|
| 1 | `check --tests` | `cargo check --tests` | the test tree type-checks (fastest whole-repo signal) | ~97 s warm (`logs/check_tests3.log`, `Finished ... in 1m 37s`) |
| 2 | fmt | `cargo fmt --all -- --check` | formatting is the committed one — no diff noise in review | 5–12 s (`logs/acc-final/*/summary.txt`) |
| 3 | clippy | `cargo clippy --all-targets --all-features -- -D warnings` | zero warnings across every target and feature combination | 4–105 s depending on cache (`logs/acc-final/*/summary.txt`) |
| 4 | build | `cargo build --all-targets --all-features` | every target links with every feature on | 206–344 s (`logs/acc-final/*/summary.txt`) |
| 5 | targeted suites | `powershell -NoProfile -File scripts/test_select.ps1` | the suites mapped to the changed files, plus the `pir`+`cli` smoke pair | ~1–2 min for a small subset (*lane-reported; not re-measured here*) |
| 6 | full suite | `powershell -NoProfile -File scripts/test_fast.ps1` | **every** test binary, run concurrently, with load-flaky suites separated from real failures | 621–706 s = build 303–333 s + parallel 283–298 s across 253 suites at run time (`logs/test_fast3.log`, `logs/test_fast2.log`); the current tree has **258 test targets** (9 merged `g_*` harnesses + 248 standalone targets + the lib unit target; `scripts/test_fast.ps1 -List`) |
| 7 | doctests | `cargo test --doc` | the doc examples in `src/**` still compile and run | 16.3 s, 27 passed / 6 ignored (`logs/doctests2.log`) |
| 8 | feature matrix | `bash scripts/feature_matrix.sh` | 72 documented configurations (51 lib + 11 bin + 2 SDK consumer + 8 smoke) each compile; the script's asserted header is the source of truth (see §2) | *unmeasured end-to-end*; individual rows from the lane sweep below |
| 9 | ConPTY battery | `python scripts/tui_conpty/run_battery.py` | the TUI works on a real pseudo-console: rendering, quit/restore, clipboard, streaming | 31 scenarios ≈ 543 s (`scripts/tui_conpty/out/sdk-battery-rerun/report.md`); 6 scenarios ≈ 105 s (`out/recon_before_subset/report.md`) |
| 10 | memory + soak | see `scripts/soak/GATE-RUNBOOK.md` | the 20 MB/100 MB-session budget and long-run stability | per runbook |
| 11 | release | `cargo build --release` | the shipped artifact builds (fat LTO, `codegen-units=1`) | 8 m 51 s (`logs/perf/20261016-firstbyte/release_build.log`) |
| 12 | A/B | see `scripts/perf/AB-RUNBOOK.md` | quality/latency vs `pi` under a pinned model | per runbook |

`scripts/acc_final.ps1` runs **gates 2–4** in one command into
`logs/acc-final/<stamp>/`; run it instead of the three separate commands:

```powershell
powershell -NoProfile -File scripts/acc_final.ps1     # -> exit 0 iff fmt+clippy+build are green
```

It deliberately does **not** run the full suite: that is gate 6, started
separately so the static results can be read before the long run begins.

### Order rationale

1→4 are static and cheap-but-broad; they catch the mistakes that would make a
test run meaningless (a tree that does not type-check). 5→7 are behavioural:
targeted first because it is minutes, then the full suite, then doctests
(doctests are slow to link and rarely the source of a regression). 8 is the
configuration axis `cargo test` cannot cover (each row is a different feature
set). 9 is the only gate that exercises a real terminal. 10→12 are the
measurement gates and need a quiescent machine and, for 12, a key.

---

## 2. What each script guarantees (and its failure semantics)

### `scripts/test_fast.ps1` — full suite, concurrent

- Builds once (`cargo test --no-run`), then runs every test binary
  concurrently; per-suite output is captured, per-suite wall seconds printed
  (slowest 10), and totals aggregated.
- **Exit 0** only when every suite passed. **Exit 1** on a real failure, on a
  failed build (a partial suite is never reported as the full one), on
  `-NoBuild` with a requested target missing, or on a run that executed **zero**
  tests.
- A suite that fails only under parallel load is re-run **alone**; if it passes
  there it is reported `[load-flaky]` and does **not** fail the run. A suite
  that fails alone is a real failure.
- A harness that runs 0 tests is counted as an **empty harness**, never as a
  pass ("0 passed" can never inflate the total).
- Writes `logs/test_fast/last-run.json` (UTF-8, no BOM) on every executed run:
  `passed`, `failed` (real failures only), `failedParallel`, `flaky`,
  `failedTargets`, `emptySuites`, `exitCode`, per-suite `secs`. `-RetryFailed`
  reads `failedTargets` from it.
- Flags: `-Tests <names>` (exact cargo targets, the lever for link time),
  `-Filter <sub/glob>` (run only matching suites from an existing build),
  `-NoBuild`, `-RetryFailed`, `-List` (dry run: prints the resolved plan; works
  before a build exists and exits 0), `-Concurrency`, `-Threads`.
- Not covered: doctests (gate 7) and an unbounded hang inside one binary (no
  per-suite timeout — Ctrl+C).

### `scripts/acc_final.ps1` — static gates into dated logs

```powershell
powershell -NoProfile -File scripts/acc_final.ps1 [-TargetDir <dir>]
```

- Runs fmt → clippy → build, **all three even after a failure** (one invocation
  collects all evidence), each gate's stdout to `<gate>.log` and stderr to
  `<gate>.log.err`, both UTF-8.
- `summary.txt` records `RC` and wall seconds per gate.
- **Exit code = the first failing gate's RC; 0 iff all three are 0.**
- Default target dir is `<repo>/target-alt` (not a hardcoded machine path).

### `scripts/dev_cycle.ps1` — local debug fast loop (not a gate)

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/dev_cycle.ps1
```

- Runs fmt → clippy (`--lib --bins`) → build (`--bin pir`) → affected tests
  (`test_select.ps1`), times every step, stops at the first failure, and prints
  a warning plus a concrete suggestion for any step over `-SlowSec` (60 s).
- `-Only fmt,clippy` / `-NoTest` / `-Tests <targets>` / `-Files <paths>` /
  `-Since <min>` select what runs; `-List` prints the plan without executing.
- Deliberately narrower than the gates above: clippy skips the ~280 test
  targets and the test step is the affected subset (plus the `cli` smoke).
  Use `scripts/test_fast.ps1` / `scripts/acc_final.ps1` before handoff.
- Measured on 2026-10-29 (`target-lane-d`, one-file edit): 48.5 s for
  `-Only test`, 50.6 s for the full four-step loop when the test binaries were
  already built, 6.1 s for a no-change repeat; see
  `logs/lane-test-speed/REPORT.md`.

### `scripts/feature_matrix.sh` — the configuration axis

```bash
bash scripts/feature_matrix.sh
```

- **72 rows** — `scripts/feature_matrix.sh` is the source of truth (its header
  comment and the asserted `EXPECTED_*` counts define the matrix): **51 library
  rows** (`minimal`, `minimal+cli`, one row per non-provider feature —
  including `session`, `skills`, `sdk`, `client`, `env-daemon`, `print`, `rpc`,
  `export-html`, `tools`, the seven `tool-*` leaves, `grep-find` — the
  documented `no-js` and `tui-slim` slim profiles, one row per `provider-*`
  family, the high-risk pairs/triples, `all-features`, and `full`), **11 binary
  rows** (`cli`, `cli,tool-read`, `cli,tools`, `cli,env-daemon`, `cli,client`,
  `cli,mcp`, `cli,mcp-sd`, `cli,brush`, `cli,provider-openai`,
  `cli,tui,provider-openai`, `full`), **2 `tests/sdk_consumer_*.rs` pins**
  checked against the bare crate plus `sdk` (`--no-default-features --features
  sdk`), and **8 smoke rows** (`cli`, `cli,tool-read`, `cli,tools`, `cli,mcp`,
  `cli,provider-openai`, `cli,tui,provider-openai`, `full`, plus the
  `rpc`-without-`export-html` off-arm).
- The lib/bin/smoke row counts are asserted in the script
  (`EXPECTED_LIB_ROWS=51`, `EXPECTED_BIN_ROWS=11`, `EXPECTED_SMOKE_ROWS=8`):
  adding a row without updating the header exits **2** before compiling
  anything. A missing `tests/sdk_consumer_*.rs` is a failed row, not a silent
  skip.
- **All rows run even after a failure** (the point of a matrix is the complete
  list of broken configurations). **Exit 1** if any row fails, naming every
  failed row; **exit 0** only when all 72 compile. **Exit 2** = wrong cwd,
  row-count mismatch, or target dir could not be created.
- Row output goes to `$CARGO_TARGET_DIR/feature-matrix.log` (last row only);
  each failed row prints its own tail.

### `scripts/gate_run_bins.ps1` / `gate_run_serial.ps1` / `gate_extract_failures.ps1`

Ad-hoc helpers for "run this explicit list of already-built test binaries":

| script | purpose | exit 0 | exit 1 | exit 2 |
|---|---|---|---|---|
| `gate_run_bins.ps1` | list concurrently (`-Concurrency`, default 8), per-bin timeout `-TimeoutSec` | all printed a summary and exited 0 | any failed / timed out / printed **no** `test result:` line | input list missing or names no existing file |
| `gate_run_serial.ps1` | re-run a failure list one at a time (`-TimeoutSec`, default 300) | all green | any failed / timed out / no summary | input list missing |
| `gate_extract_failures.ps1` | print panics from `*.serial.log` | scan completed (clean tree is success) | — | `test_logs/` does not exist |

Exit-code and `NO_RESULT` semantics were verified with stub binaries
(2026-10-21): a crashing binary with no summary reported `NO_RESULT` → exit 1;
a `FAILED.` summary with exit 101 → exit 1; a clean binary → exit 0; a missing
input list → exit 2.

Input lists default to `<repo>/test_bins_gate.txt` and
`<repo>/test_gate_failed.txt` (not committed; pass `-ListFile`). Logs go to
`<repo>/test_logs/`.

### `scripts/tui_conpty/run_battery.py` — ConPTY battery

Windows-only; needs a real console session. `--list` is a free dry run.

```powershell
python scripts/tui_conpty/run_battery.py --list
python scripts/tui_conpty/run_battery.py                    # all scenarios, hermetic mock LLM
python scripts/tui_conpty/run_battery.py --only s07*,s17*   # subset
python scripts/tui_conpty/run_battery.py --pir target-alt\debug\pir.exe
```

Invariants (all verified in source, 2026-10-21):

- **Budget precedence**: `--timeout` (overrides every scenario) → scenario
  `TIMEOUT` → default 120 s. The budget is enforced **inside**
  `tui_drive.py` between ops, so a frozen TUI cannot hang the battery; expiry
  runs the normal graceful quit and sets `timed_out` (which makes the scenario a
  FAIL, not a timeout-shaped pass). A `timeout + 60 s` hard subprocess cap
  (`taskkill /PID <driver> /T /F` of the battery's **own** tree) is the
  backstop.
- **Quit retry fails the scenario**: if the driver had to escalate past the
  first esc + `/quit` (driver log contains
  `RETRY: still alive after first esc+/quit`), the synthetic
  `single_quit_suffices` check fails. One `/quit` must terminate the process.
  Any `WARN: still alive` mark also fails `restore_markers`.
- **Clipboard**: scenarios that drive the real system clipboard set
  `NEEDS_CLIPBOARD = True` (currently `s17_paste_fold`); when the clipboard is
  inaccessible (locked desktop / stuck owner) they are **SKIP**ped with the
  reason recorded, never failed.
- **Exit codes**: `0` = every executed scenario passed (SKIPs allowed and
  reported); `1` = at least one FAIL; `2` = config error, nothing selected, or
  **only** SKIPs (no scenario executed).
- Artifacts: `scripts/tui_conpty/out/<run-id>/{report.md,report.json}` plus a
  per-scenario dir with the driver log, result JSON, rendered screens and the
  mock-LLM request log.

---

## 3. Failure semantics in one paragraph

A gate is green only when its process exits 0 **and** its own report has no
failed row. The scripts are written so that "nothing ran" is never green:

- zero tests executed → `test_fast.ps1` exit 1;
- zero scenarios executed (all SKIP) → battery exit 2;
- a harness with no `test result:` line → failure in all three `gate_*.ps1`;
- a build that failed partway → `test_fast.ps1` refuses to run the partial set;
- a matrix row that did not compile → `feature_matrix.sh` exit 1 with the row
  named;
- a `cargo` that cannot be started → `acc_final.ps1` records RC 127.

Load-flaky is the one deliberate exemption, and it is explicit: a suite that
fails under concurrency but passes **alone** is reported `[load-flaky]` and
keeps the run green; `last-run.json` records it under `flaky` so it is visible,
not hidden.

---

## 4. Evidence index

| Fact | Source |
|---|---|
| full suite 621 s / 706 s (253 suites at run time; current tree: 258 test targets) | `logs/test_fast3.log`, `logs/test_fast2.log` |
| fmt/clippy/build RCs and seconds | `logs/acc-final/*/summary.txt` |
| doctests 27 passed / 16.3 s | `logs/doctests2.log` |
| `check --tests` 1 m 37 s | `logs/check_tests3.log` |
| release build 8 m 51 s | `logs/perf/20261016-firstbyte/release_build.log` |
| battery 31 scenarios / 543 s | `scripts/tui_conpty/out/sdk-battery-rerun/report.md` |
| feature-matrix per-row results | `logs/feature-matrix-report.md` |

History note: `logs/acc-final/2026*/**.log` written before 2026-10-21 are
UTF-16LE (PowerShell 5.1 `*>`), so `grep`/`tail` see them as binary. They are
left as-is; new runs are UTF-8 with stderr in a separate `.err`.
