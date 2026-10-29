# TUI ConPTY acceptance harness (repo-managed copy)

ConPTY-based interactive acceptance harness for the pir TUI. It launches
`pir.exe interactive` inside a Windows pseudo-console, feeds scripted key
bytes, snapshots the raw output stream to `.log` files, then quits the TUI
and reports the exit code plus the terminal restore sequences found in the
final log. Scenario files are plain data (`NAME` + `OPS`) executed by the
driver; `acc3_render.py` replays a snapshot through `pyte` into a readable
`.screen.txt` for the visual assertions.

Provenance: all `.py` files were copied from `target/tui_shots/` on
2026-10-15 (recorded acceptance evidence stays there, untouched). Only the
paths listed in the inventory below were changed.

Everything this harness writes goes to `scripts/tui_conpty/out/` (created at
run time). It never writes to `target/`, `target-alt/` or the repo root.

## Inventory (14 copied files + battery additions)

| File | Role | Changes vs `target/tui_shots/<name>` |
|---|---|---|
| `run_battery.py` | Assertion battery (new, 2026-10-15): discover scenarios, mock-provider injection, per-scenario artifacts, pyte rendering, CHECKS, reports | new file |
| `mock_llm.py` | Hermetic OpenAI-compatible mock LLM (new, 2026-10-15): stdlib `http.server` on a random loopback port, `GET /v1/models` + streaming `POST /v1/chat/completions` (`mock reply`; `MockServer(script=[...])` / scenario `MOCK_SCRIPT` scripts text and `tool_calls` steps — see `s18_background_status.py`) | new file |
|---|---|---|
| `tui_drive.py` | ConPTY driver: `Pty` class (CreatePseudoConsole/CreateProcess), op interpreter (`send`/`wait`/`wait_quiet`/`snap`/`log`/`resize`/`clipset`/`clipdump`), `/quit` + restore-marker report | `PIR` defaults to `<repo>/target-alt/debug/pir.exe` and honors `PIR_TUI_EXE`; `SHOTS` = `out/`; `PIR_AGENT_DIR` and the child cwd = `out/tui_agent/`; `out/` + agent dir created in `run_scenario`. Driver logic unchanged except the quit path: esc → short wait → `/quit`, ONE retry if still alive, then the loud ctrl+c/terminate escalation (never loops; every step marked). |
| `acc3_run.py` | Runner for the acc3 conversation+tool scenario | `td.PIR` no longer hard-coded: keeps the driver default (target-alt) unless `PIR_TUI_EXE` is set. |
| `acc3_render.py` | Renders recorded `.log` snapshots to `.screen.txt` via `pyte` | `SHOTS` now `<dir>/out` (reads `out/<snap>.log`, writes `out/<snap>.screen.txt`). Render loop unchanged. |
| `acc3_s03.py` | Scenario: real model dialog, streaming render, bash tool call, ctrl+o expand/collapse, `/session`, `/copy` | Scenario data unchanged (header comment only). |
| `r2b_s1_help.py` | `/help` notice renders in the snapshot with no further key events | unchanged (header only) |
| `r2b_s2_shifttab.py` | `ESC[Z` (BackTab) cycles thinking; "Thinking level: ..." notice | unchanged (header only) |
| `r2b_s3_bug.py` | `/bug` completes exactly (not `/debug`); `/share`, `/import` popups; Enter executes `/bug` | unchanged (header only) |
| `r2b_s4_space.py` | typing `XY ` must NOT open the whole-cwd file listing | unchanged (header only) |
| `r2b_s5_compact.py` | `/compact` on a small session surfaces the real error, not false success | unchanged (header only) |
| `r2b_s6_settings.py` | `/settings` dialog; value change shows "Saved setting: \<path\>" | unchanged (header only) |
| `r2b_s7_paste.py` | bracketed paste renders once; quit for the `ESC[?2004l` count | unchanged (header only) |
| `r2b_s7b_paste_quit.py` | same, with the editor cleared before `/quit` so the final log carries the exit sequences | unchanged (header only) |
| `r2b_s8_session.py` | after one model round-trip, `/session` shows the Cost section | unchanged (header only) |
| `r2b_s8b_session_cost.py` | with model rates configured, `/session` shows "Cost" + "Total: $..." | unchanged (header only) |

## Prerequisites

- Windows with ConPTY (Windows 10 1809+), Python 3 with `pyte`
  (`pip install pyte`; verified importable with the repo's Python 3.12).
- PowerShell on `PATH` only for the clipboard ops (`clipset`/`clipdump`);
  none of the copied scenarios use them.
- `target-alt/debug/pir.exe` present (built outside this harness; this
  directory never builds pir). As of 2026-10-15 it exists.
- For the model round-trip scenarios, a provider config in the agent dir —
  see "Dependencies" below.

## How to run

Run from this directory (`scripts/tui_conpty`). `PIR_TUI_EXE` is optional:
unset it uses `<repo>/target-alt/debug/pir.exe`.

### acc3 conversation+tool scenario

```bash
cd scripts/tui_conpty
python acc3_run.py
```

With the explicit binary override (same command shape for every scenario):

```bash
PIR_TUI_EXE="C:\Users\XPENG_USER\Desktop\github\pir\target-alt\debug\pir.exe" python acc3_run.py
```

`python tui_drive.py acc3_s03.py` is equivalent to `acc3_run.py`.

Render the acc3 snapshots to readable screens (writes `out/*.screen.txt`):

```bash
python acc3_render.py
```

### r2b_s1..s8 batch

```bash
cd scripts/tui_conpty
export PIR_TUI_EXE="C:\Users\XPENG_USER\Desktop\github\pir\target-alt\debug\pir.exe"
for s in r2b_s1_help r2b_s2_shifttab r2b_s3_bug r2b_s4_space r2b_s5_compact \
         r2b_s6_settings r2b_s7_paste r2b_s7b_paste_quit r2b_s8_session r2b_s8b_session_cost; do
  python tui_drive.py "$s.py"
done
```

(`r2b_s7b` and `r2b_s8b` are the batch follow-ups recorded on 2026-10-10;
drop them for a strict s1..s8 run. Single scenario:
`python tui_drive.py r2b_s1_help.py`.)

## Outputs (all under `out/`)

- `out/<NAME>.log` — full raw byte log of the scenario after `/quit`
  (e.g. `out/r2b_s1_help.log`, `out/acc3_dialog_tool.log`).
- `out/<snap>.log` — raw log at each `snap` op
  (e.g. `out/r2b_s1_help_after.log`).
- `out/<snap>.screen.txt` — `pyte` render of a snapshot (acc3 only).
- `out/tui_agent/` — the TUI's agent dir (`PIR_AGENT_DIR`) and child cwd:
  sessions, `pir.log`, settings the TUI writes. Delete freely between runs.

## What PASS looks like

Each run prints a report and the python process exits 0 unless the driver
itself failed (traceback). `run_scenario` does not fail the process on a bad
marker — read the report:

```
=== scenario r2b_s1_help ===
  exit_code: 0
  alt_screen_exit (ESC[?1049l): False   <- expected: not observable via ConPTY
  mouse_disable (ESC[?1000l): False
  mouse_sgr_disable (ESC[?1006l): False
  cursor_show (ESC[?25h): True
  bracketed_paste_off (ESC[?2004l): True
  snap r2b_s1_help_after at ...s size=...
  log: C:\...\scripts\tui_conpty\out\r2b_s1_help.log (N bytes)
```

PASS = all of:

1. python exits 0 with no traceback;
2. `exit_code: 0` and no `WARN: still alive ... ctrl+c/terminating` mark
   (the TUI quit cleanly on the driver's `/quit`);
3. `cursor_show` and `bracketed_paste_off` are `True` — the TUI restored
   the terminal.

**Alt-screen (ESC[?1049h/l) is not observable through this harness**
(2026-10-29): Windows ConPTY consumes the child's alternate-screen switches
and never forwards them on the host-side output pipe. Verified with
`probe_alt_passthrough.py` (a child that writes only the two sequences
produces a capture containing neither) and with pir's own boot capture,
which carries the fullscreen-only mouse capture (`?1003;1006h`) while 1049h
and 1049l both stay at 0. `run_battery.restore_markers` therefore does not
gate on 1049 by default; `alt_screen_exit`/`alt_screen_exit_min` remain
opt-in overrides for a harness that preserves the raw child stream.

Recorded baseline (from the kept logs in `target/tui_shots`, recorded on an
older harness): final logs contain `ESC[?2004l` exactly once and `ESC[?25h`
at least once. The historical `ESC[?1049l` count is not reproducible under
the current ConPTY capture (see above). `mouse_disable`/`mouse_sgr_disable`
are expected `False`: that build never emits `ESC[?1000h`/`ESC[?1006h`, so
there is nothing to disable — treat only an *enable* sequence without its
matching disable as a regression. For `r2b_s7`/`r2b_s7b` the dedup criterion
is exactly one `ESC[?2004l` in the final log (recorded
`r2b_s7b_paste_quit.log`: `2004h`x1 / `2004l`x1).

Scenario content (help notice, thinking notice, popup contents, compact
error, "Saved setting", paste rendered once, cost section, tool-card
expand/collapse) is asserted by reading the snapshots — `.screen.txt` for
acc3, raw `.log` otherwise, same as the recorded runs.

## Dependencies not copied (read before running)

- **Provider config (`models.json`, `settings.json`)**: the recorded runs
  used `target/tui_agent/models.json` + `settings.json` as `PIR_AGENT_DIR`.
  They were deliberately **not copied** — they contain live API keys.
  - **Never put real credentials under `out/`.** `scripts/tui_conpty/out/`
    is gitignored, but it is still a plain directory: Q27 found 42
    `out/**/agent/models.json` copies carrying the user's `~/.pi/agent`
    provider keys and the `~/.claude` Anthropic token; 1048 credential-named
    `{models,auth,settings}.json` files had accumulated across runs. Those
    were deleted and the tree ignored.
  - Since 2026-10-23 the **battery never reads or copies a real config**:
    every seeded scenario gets an in-process **sanitized template**
    (`models.json`: provider `openai`, model `gpt-5`, dummy key
    `sk-mock-battery`, dead loopback baseUrl, 1M context; `settings.json`:
    theme/provider/model/thinking only, no `packages`) generated by
    `run_battery.py`. Nothing is read from `out/tui_agent/` or `~/.pi/agent`.
  - Hermetic runs (the default) then rewrite the per-scenario `models.json`
    baseUrl to the local mock LLM and the key to the dummy (mock_llm.py,
    stdlib loopback), and stop the mock afterwards.
  - `--real-provider` does **not** fall back to the user's config: it
    requires `--models-template <path>` whose credential-bearing values are
    **whole-value** `$ENV`/`${ENV}`/`!command` templates (or the exact dummy
    `sk-mock-battery`). The check is not limited to secret-named fields: every
    `apiKey`, `headers.*`, `customHeaders.*`, URL userinfo and key-like value
    is validated; literal values, mixed literal+template values
    (`sk-LITERAL-${ENV}`), literal `X-Api-Key`/`Cookie` headers and
    `scheme://user:literal@` URLs are refused with a path/shape-only
    diagnostic (the value is never echoed). `!command` is an explicit trust
    boundary: it is accepted only if the command text does not embed an
    obvious literal key shape (`sk-...`, `Bearer <token>`, `Cookie:`, a 20+
    alnum token); a runtime key lookup by the command itself cannot be judged
    statically. No scenario currently sets `NEEDS_REAL_PROVIDER`, so the gate
    batteries are all hermetic. Run `python run_battery.py --selftest` to
    exercise the validator (pure functions; no scenario/TUI/model call).
  - The legacy `tui_drive.py` runs (acc3/r2b) still use `out/tui_agent/` as
    the default agent dir. If you point them at a provider, create a
    sanitized env-templated `models.json` there — never copy `~/.pi/agent`.
- `pyte` Python package (renderer only), PowerShell (clipboard ops only),
  Windows ConPTY, and `target-alt/debug/pir.exe` itself.
- Not copied from `target/tui_shots/`: all `.log`/`.screen.txt`/`.dmp`
  evidence, and the one-off debug/probe drivers outside the recorded
  acc3/r2b path (`accept2_run.py`, `freeze_run*.py`, `retest_*.py`,
  `fix_*.py`, `ord_check.py`, `dbg*`, `peek*`, `keyprobe*`, `*_probe.py`,
  `tui_drive_dev.py`, `clipcheck.py`, `conpty_test.py`). Those remain in
  `target/tui_shots/` and point at the frozen `target/debug` binary.

---

# TUI battery (`run_battery.py`)

Assertion-based batch runner on top of `tui_drive.py`: it discovers every
scenario in `scenarios/`, runs each one in its own artifact directory, renders
**every** captured `*.log` through `pyte`, applies the scenario's `CHECKS`,
and writes `out/<run-id>/report.md` + `report.json`. The run is **hermetic by
default**: every seeded scenario (anything not `SEED_AGENT = False`) gets the
local mock LLM (`mock_llm.py`) injected as its provider — an in-process
OpenAI-compatible server on a random loopback port answering
`GET /v1/models` and streaming `POST /v1/chat/completions` (`mock reply`).
Scenarios tagged `NEEDS_REAL_PROVIDER` are listed but SKIPped unless
`--real-provider` is given (which then requires an env-templated
`--models-template`; see below). Exit code is **0 only if every executed
scenario passed**.

```bash
cd scripts/tui_conpty
python run_battery.py                          # all scenarios, hermetic mock
python run_battery.py --list                   # inventory, no runs (imports modules; see note)
python run_battery.py --selftest               # validate the credential validator (pure)
python run_battery.py --only s01*,s13*         # comma list of glob/substring filters
python run_battery.py --skip s14*
python run_battery.py --real-provider \
    --models-template C:\safe\models-template.json   # no mock; env-templated key
python run_battery.py --timeout 90             # per-scenario budget override (default 120s)
python run_battery.py --pir "C:\path\to\pir.exe"
PIR_TUI_EXE="C:\path\to\pir.exe" python run_battery.py
```

`--include-provider` is kept as a deprecated alias of `--real-provider`.
There is no `--filter`: use `--only` (glob/substring list; e.g. `s18*`, `s39*`,
`s50*,s51*`). `--only` substrings that match nothing make the run exit 2.
`--list` runs no scenario and makes no model call, but it is not
side-effect-free: scenario modules are imported/exec'd, and
`s37_terminal_input_replace.py` writes `out/terminal_input_ext.js` at import.
It never seeds an agent config and never reads a credential.

Binary resolution order: `--pir` > `PIR_TUI_EXE` > `<repo>/target-alt/debug/pir.exe`.

## Scenario inventory

Each scenario is a plain data module under `scenarios/`; the runner discovers them all.
Acceptance rule for every scenario: the app must exit on a SINGLE `/quit` (a driver
`RETRY:` mark fails the scenario; liveness is judged from the PROCESS HANDLE, since a
ConPTY keeps its output pipe open after the child exits), and scenarios tagged
`NEEDS_CLIPBOARD` are SKIPped when the system clipboard is unreachable (locked desktop).

- `s01_help_overlay` — py (recorded 2026-10-10, re-run 2026-10-15).
- `s02_thinking_shifttab` — py.
- `s03_bug_flow` — py.
- `s04_space_no_filelist` — py.
- `s05_compact_small` — py.
- `s06_settings_dialog` — py.
- `s07_paste_once` — py / r2b_s7b_paste_quit.py.
- `s07b_paste_twice` — New (battery starter; r2b_s7b sibling).
- `s08_session_info` — py.
- `s08b_session_cost` — py.
- `s09_type_renders` — .
- `s10_hotkeys_overlay` — .
- `s11_ctrl_o_toggle` — .
- `s12_esc_closes_popup` — .
- `s13_submit_clears_editor` — THE FREEZE REGRESSION TEST.
- `s14_resume_picker` — session picker without a model.
- `s15_backspace_deletes` — New (P0 input regression).
- `s16_no_sequence_garbage` — New 2026-10-15 - DA1-LEAK REGRESSION LOCK.
- `s17_paste_fold` — s17 — LARGE paste folds into the paste marker instead of inserting raw text.
- `s17b_paste_lines` — s17b — a multi-line BRACKETED paste must keep its line breaks (Windows).
- `s18_background_status` — s18 — background tool-call UI: the `bg: N` status marker lifecycle and the.
- `s21_quit_enter` — Promoted from the 2026-10-15 play session (p14_quit_enter.py) into the.
- `s22_quit_twice` — Promoted from the 2026-10-15 play session (p20_quit_twice.py) into the.
- `s23_ctrl_d_idle` — Promoted from the 2026-10-15 play session (p13_ctrl_d.py) into the.
- `s24_hostile_input` — Promoted from the 2026-10-15 play session (p09_hostile.py) into the.
- `s26_sessions_resume_fork` — Promoted from the 2026-10-15 play session (p12_sessions.py) into the.
- `s27_dialogs_and_selectors` — Promoted from the 2026-10-15 play session (p17_selectors.py) into the.
- `s28_stream_throughput` — s28 — streaming throughput (2026-10-15, promoted from the foreman's latency probe):.
- `s29_worker_error_no_spinner` — Promoted from the 2026-10-16 foreman verification (p24_f2_f4_assert.py): an.
- `s30_single_abort_error` — Promoted from the 2026-10-16 foreman verification (p25_f4_assert.py): an.
- `s38_console_clear` — console hygiene on enter: a `ESC[2J` clear-all precedes the first frame banner, and the boot stream clears at least twice (2026-10-15 user report). ConPTY consumes the `ESC[?1049h`/`ESC[?1049l` switches, so they are not asserted (see "What PASS looks like").
- `s39_parallel_tool_stream` — a fast parallel tool result renders while its slow sibling is still running (2026-10-15 user report).
- `s40_native_cli_commands` — inside the TUI, a bash call runs `$PIR_BIN grep --version` and `$PIR_BIN python -c "print(42)"`; asserts the grep banner and `42` output (2026-10-19 native CLI).
- `s41_brush_shell` — a bash call runs `echo $((1+1))` and `echo abc | tr a-z A-Z`; asserts `2` / `ABC` and no `command not found` (2026-10-19 `brush` feature).
- `s42_no_startup_update_notice` — pi 1.3.0 wave (2026-10-20): the startup update-version notice is gone. Runs online-gated (`OFFLINE=False`, no `PI_OFFLINE`) and asserts the boot/idle screens AND the raw final stream carry no `new version` / `update available` / `pir update` text, while `ctrl+o more` proves the boot frame rendered.
- `s43_read_range_label` — pi 1.3.0 wave (2026-10-20): one hermetic mock turn with two `read` calls asserts the normalized line-range labels (src/tui/messages.rs `format_read_line_range`): `read models.json:2-2` for `offset=2 limit=1`, and `read models.json:1-1` for a lone `limit=1` (the old formatter emitted no range for limit-only). The generated agent-dir `models.json` only supplies an existing file; the `read ` call-header assertions are independent of its key/line ordering.

## Scenario format (`scenarios/<name>.py`)

Plain data modules — the same `NAME`/`OPS` shape as the `r2b_*` files, plus
optional fields:

```python
NAME = "s01_help_overlay"     # unique; also the final-log base name
OPS = [                       # the tui_drive op list (send/wait/wait_quiet/snap/log/...)
    ("wait_quiet", 4),
    ("send", b"/help\r"),
    ("wait", 1.5),
    ("snap", "s01_after"),
]
CHECKS = [                    # assertions; all must pass for PASS
    ("restore_markers",),
    ("screen_contains", "s01_after", "Commands: /model /thinking /clear /compact"),
]
TIMEOUT = 90                  # optional per-scenario wall-clock budget (s)
NEEDS_REAL_PROVIDER = True    # optional: cannot run against the mock -> SKIP unless
                              #           --real-provider (0=offline gate is then dropped)
NEEDS_PROVIDER = True         # optional deprecated alias of NEEDS_REAL_PROVIDER
OFFLINE = False               # optional: child PI_OFFLINE (default: not NEEDS_*)
SEED_AGENT = False            # optional: skip seeding models.json/settings.json
                              #           AND the mock injection (stays provider-free)
SEED_SESSION = True           # optional: write one fixture session JSONL for the cwd
                              #           (so /resume has something to list)
ENV = {"KEY": "val"}          # optional extra child env (merged over the driver BASE_ENV)
ARGS = ["--flag"]             # optional extra `pir interactive` args
MOCK_SCRIPT = [               # optional: script the hermetic mock LLM, one step per
    {"text": "plain reply"},  #   POST /v1/chat/completions, last step repeats
    {"tool_call": {"name": "bash", "arguments": {"command": "sleep 5",
                                                 "background": True}}},
]                             #   (steps: {"text"} | {"tool_call"} | {"tool_calls": [...]};
                              #    without MOCK_SCRIPT the mock keeps `mock reply`)
```

`MOCK_SCRIPT` is what lets a scenario drive real tool calls (see
`s18_background_status.py`: a `background: true` bash call, then two
concurrent background calls, asserting the `bg: N` status marker, the
non-blocking turn, and the in-place block flip to the real output).

Snapshots (`("snap", name)` ops) are rendered to `<name>.screen.txt`; a check
may also reference `"@final"` (or the scenario `NAME`) for the final log
rendered after the driver's `/quit`.

## Assertion reference

| Check | Meaning |
|---|---|
| `("screen_contains", snap, text)` | `text` appears in the pyte render of `snap` |
| `("screen_not_contains", snap, text)` (alias `not_contains`) | `text` appears nowhere in `snap` |
| `("screen_regex", snap, pattern)` / `("screen_not_regex", ...)` | `re.search` over the rendered screen (multi-line) |
| `("screen_count", snap, text, n)` | occurrences of `text` equal `n` (int, or `"==1"`/`">=1"`/`"<2"`) — used for paste dedup |
| `("screen_row_changed", snapA, snapB, pattern)` | at least one row matches `pattern` in each screen and the matching rows DIFFER (state that is only observable as a change, e.g. the footer thinking level after Shift+Tab) |
| `("editor_contains", snap, text)` / `("editor_not_contains", ...)` | text is (not) in the **editor input-box region**, i.e. between the last adjacent pair of border rows (≤8-row gap). A border row may carry an inline label (`── ⠇ Working ──…`, observed 2026-10-15) and still counts; the cwd/status rows below the box contain no rule chars, so they never form a pair. This is how "Enter cleared the editor" is asserted even though a submitted prompt legitimately reappears in the transcript |
| `("restore_markers",)` | clean acceptance quit: `exit_code == 0`, no `WARN: still alive ...` driver mark, and the final log has `ESC[?2004l` x1 and `ESC[?25h` >=1. `ESC[?1049l` is NOT gated: ConPTY consumes the child's alt-screen switches (see "What PASS looks like"), so `alt_screen_exit`/`alt_screen_exit_min` are opt-in overrides. Any sub-criterion can be overridden, e.g. `("restore_markers", {"exit_code": 1})` |
| `("exit_code", n)` | final pir process exit code equals `n` |
| `("driver_contains", text)` | `text` (e.g. a `("log", ...)` MARK) is in the scenario's driver log |
| `("log_regex", snap, pattern)` / `("log_not_regex", ...)` | `re.search` over the RAW captured stream (ANSI escapes preserved; use for sequences pyte erases) |
| `("log_count", snap, text, n)` | occurrences of a plain substring in the raw stream (`n` forms as `screen_count`) |
| `("log_ordered", snap, [a, b, ...])` | all substrings present in the raw stream, in this order |

## Per-run artifacts

Every battery invocation creates `out/<run-id>/` (default run-id
`YYYYmmdd-HHMMSS`, `--run-id` to override); nothing is overwritten between
runs:

```
out/<run-id>/
  report.md / report.json          # console table, per-scenario checks, artifact paths
  <scenario>/
    agent/                         # child PIR_AGENT_DIR + cwd (models.json/settings.json
                                   # are the SANITIZED in-process template unless
                                   # SEED_AGENT=False; never copied from ~/.pi or
                                   # out/tui_agent; models.json is mock-injected
                                   # unless --real-provider + --models-template)
    <scenario>.log                 # final raw app log (post-/quit)
    <snap>.log                     # raw app log at each snap
    <snap>.screen.txt              # pyte render of each log (incl. the final one)
    <scenario>.driver.log          # DRIVER trace + `MARK` annotations — never app output
    <scenario>.result.json         # exit code, restore counts, marks, snaps, timeout flag
    <scenario>.stdout.txt          # driver stdout/stderr
    <scenario>.mock_llm.log        # mock LLM request log (JSONL; only when injected)
```

Harness fixes this battery relies on (all in `tui_drive.py`):
`("log", ...)` ops no longer write `<<<MARK ...>>>` into the captured app
stream (they go to `<scenario>.driver.log`), so rendered screens contain only
app output; every op is traced in the driver log; a per-scenario `--timeout`
is enforced between ops inside the driver so a frozen TUI can never hang the
battery (expiry runs the normal graceful-quit path and sets
`timed_out: true`); a hard `timeout+60s` subprocess cap plus
`taskkill /T /F` of the battery's **own** process tree is the last resort.
The graceful quit is esc → short wait → `/quit`, ONE retry if the app is still
alive, then the loud ctrl+c/terminate escalation — a `RETRY: ...` mark records
the retry, the `WARN: still alive ...` marks stay fatal for `restore_markers`.
`acc3_render.py` now renders every `*.log` in a directory (driver logs
excluded) instead of the old hard-coded 7-name list, and is importable.

## PASS semantics and the current build

A scenario is **PASS** only when every `CHECKS` entry passes. Infra failures
are FAIL with a clear reason: driver hard timeout, missing `result.json`
(driver crash/kill), or `timed_out` from the driver budget. `SKIP`
(NEEDS_REAL_PROVIDER without `--real-provider`) does not fail the run; exit
codes are `0` = all executed scenarios passed, `1` = at least one FAIL,
`2` = config error / nothing selected. Starter-library convention: content
checks come first and `restore_markers` last, so the reported "first failing
check" is the most informative one (the quit/restore gate is only meaningful
once the app actually reacted).

**Expected right now (2026-10-15, target-alt rebuilt after the Enter/DPR
fixes):** the former P0 freeze (Enter did not submit) is FIXED — `/help` and
friends execute, and `s13_submit_clears_editor` passes. The DA1-reply leak
(`[?61;…` typed into the prompt) is also fixed and is locked by
`s16_no_sequence_garbage`. The bracketed-paste gate keeps `ESC[200~…201~`
out of the editor (s07/s07b) and large pastes fold into a
`[paste #N +L lines]` marker (s17). Remaining FAILs are calibration or app
bugs respectively — read `report.md`: every scenario lists its checks, and
`<scenario>.driver.log` + the rendered screens are next to it.
`s13_submit_clears_editor` remains the dedicated submit regression test: it
types a unique token, presses Enter, and requires the editor region to no
longer contain it.
