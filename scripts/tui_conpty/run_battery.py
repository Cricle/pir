#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""ConPTY TUI battery: run every scenario in scenarios/, render every snap with
pyte, apply the scenario's CHECKS, and emit out/<run-id>/{report.md,report.json}.

Usage (from scripts/tui_conpty/):
  python run_battery.py                      # all scenarios, hermetic (mock LLM)
  python run_battery.py --only s01*,s13*     # glob/substring filter
  python run_battery.py --skip s07*          # exclude
  python run_battery.py --real-provider --models-template <path>
                                             # no mock; env-templated secrets
  python run_battery.py --list               # inventory only, no runs
  python run_battery.py --selftest           # validator selftest (pure)
  python run_battery.py --pir <pir.exe>      # binary override

Hermetic model round-trips: unless --real-provider is given, every seeded
scenario's agent dir gets a SANITIZED models.json/settings.json synthesized
in-process (dummy api key, mock cost rates) whose models.json is then
rewritten to point at an in-process mock LLM (mock_llm.py, 127.0.0.1, random
port). The harness NEVER reads or copies the user's real config
(`~/.pi/agent/**`, `~/.claude/**`) nor the legacy seed dir
(`scripts/tui_conpty/out/tui_agent/`); Q27 found real credentials copied into
`out/**/agent/models.json` that way. For a real-provider run, pass
`--models-template <path>`: every credential-bearing value (apiKey,
headers.*, customHeaders.*, URL userinfo, any key-like string) must be a
WHOLE `$ENV`/`${ENV}`/`!command` template or the exact dummy key. Literal
values, mixed literal+template values and literal header/URL-userinfo
credentials are refused with a path/shape-only diagnostic (never echoed).
`!command` is a trust boundary: accepted only when it does not embed an
obvious literal key shape; a runtime key lookup by that command is executed
as written.
Scenarios tagged NEEDS_REAL_PROVIDER are SKIPped without --real-provider
(their provider cannot be mocked). --include-provider is a deprecated alias
of --real-provider.

Binary resolution: --pir > PIR_TUI_EXE > <repo>/target-alt/debug/pir.exe.
Per scenario: fresh <run>/<name>/{agent/, logs, screens, driver log, result.json};
a per-scenario wall-clock budget is enforced INSIDE tui_drive.py between ops,
so a frozen TUI can never hang the battery. The budget is `--timeout` when
given (it overrides every scenario), else the scenario's own TIMEOUT, else
120 s; a +60 s hard subprocess cap + taskkill /T /F of our OWN process tree is
the backstop. Exit code 0 only when every executed scenario passed (skips are
allowed and reported; none executed = 2).

Assertion format — a scenario module defines:
  CHECKS = [ ("screen_contains", snap, "substring"),
             ("screen_not_contains", snap, "substring"),   # alias "not_contains"
             ("screen_regex", snap, r"pattern"),
             ("screen_not_regex", snap, r"pattern"),
             ("screen_count", snap, "substring", 1),        # int or "==1"/">=1"/"<2"
             ("screen_row_changed", snapA, snapB, r"row-regex"),  # matching rows must differ
             ("editor_contains", snap, "substring"),        # editor input-box region
             ("editor_not_contains", snap, "substring"),
             ("restore_markers",),                          # exit 0 + clean quit + 2004l x1, 25h >=1
                                                            # (no 1049l: ConPTY consumes alt-screen
                                                            # switches; see `restore_markers`)
             ("exit_code", 0),
             ("driver_contains", "op#3 snap") ]
Snapshots are referenced by the name passed to ("snap", name) in OPS; "@final"
(or the scenario NAME) refers to the final `<NAME>.log`. `restore_markers`
accepts an optional dict overriding any sub-criterion, e.g.
("restore_markers", {"exit_code": 1}).

Scenario module fields (all optional except NAME/OPS):
  NEEDS_REAL_PROVIDER = True   # SKIP unless --real-provider; never mock-injected
  NEEDS_PROVIDER = True        # deprecated alias of NEEDS_REAL_PROVIDER
  SEED_AGENT = False           # no models.json/settings.json seed AND no mock injection
  SEED_SESSION = True          # write one fixture session JSONL for the cwd
                               # (so /resume has something to list)
  MOCK_SCRIPT = [ {"text": "..."} | {"tool_call": {"name", "arguments"}}
                  | {"tool_calls": [ {...}, ... ]}, ... ]
                               # optional scripted conversation for the injected
                               # mock LLM (mock_llm.py): one step per
                               # /v1/chat/completions request, the last step
                               # repeats. Absent -> the fixed "mock reply".
  OFFLINE = False              # child PI_OFFLINE (default: True unless a provider tag)
  TIMEOUT, ENV, ARGS as before.
"""
import argparse
import fnmatch
import glob
import json
import os
import re
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.dirname(os.path.dirname(HERE))
DEFAULT_PIR = os.path.join(REPO_ROOT, "target-alt", "debug", "pir.exe")
DRIVER = os.path.join(HERE, "tui_drive.py")
DEFAULT_SCENARIOS_DIR = os.path.join(HERE, "scenarios")
DEFAULT_OUT_ROOT = os.path.join(HERE, "out")
DEFAULT_TIMEOUT = 120.0
HARD_TIMEOUT_SLACK = 60.0

# Mock LLM injection (hermetic provider). Dummy key: never a live credential.
MOCK_API_KEY = "sk-mock-battery"
# USD per 1M tokens (units of pir's calculate_cost, src/ai/types/model/cost.rs
# uses rates/1_000_000). Added only when a model has no `cost` block so that
# s08b's /session Cost section (rendered only when cost > 0) is testable
# hermetically. Values are arbitrary but non-zero.
MOCK_COST_RATES = {"input": 3.0, "output": 15.0,
                   "cacheRead": 0.3, "cacheWrite": 3.75}

sys.path.insert(0, HERE)
import acc3_render  # noqa: E402
from mock_llm import MockServer  # noqa: E402

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")

KNOWN_OPS = {"send", "wait", "wait_quiet", "snap", "log", "resize", "clipset", "clipdump"}
CHECK_KINDS = (
    "screen_contains", "screen_not_contains", "not_contains",
    "screen_regex", "screen_not_regex", "screen_count",
    "screen_row_changed",
    "editor_contains", "editor_not_contains",
    "restore_markers", "exit_code", "driver_contains",
    "log_regex", "log_not_regex", "log_count", "log_ordered",
)


class ScenarioError(Exception):
    pass


class Scenario:
    def __init__(self, path, name, ops, checks, timeout, needs_provider, offline,
                 seed_agent, ns, needs_real_provider=False, seed_session=False,
                 mock_script=None, needs_clipboard=False):
        self.path = path
        self.name = name
        self.ops = ops
        self.checks = checks
        self.timeout = timeout
        self.needs_provider = needs_provider
        self.needs_real_provider = needs_real_provider
        self.offline = offline
        self.seed_agent = seed_agent
        self.seed_session = seed_session
        self.ns = ns
        # Optional scripted conversation for the injected mock LLM
        # (mock_llm.py `MockServer(script=...)`); None -> fixed "mock reply".
        self.mock_script = mock_script
        # NEEDS_CLIPBOARD: the scenario drives the real system clipboard (e.g.
        # `clipset` + alt+v). Windows refuses clipboard access to a session
        # whose desktop is locked or whose clipboard owner is stuck, so such a
        # scenario is SKIPped with the reason recorded instead of failing.
        self.needs_clipboard = needs_clipboard


def load_scenario(path):
    ns = {"__file__": path, "__name__": os.path.splitext(os.path.basename(path))[0]}
    with open(path, encoding="utf-8") as f:
        exec(compile(f.read(), path, "exec"), ns)
    name = ns.get("NAME")
    if not isinstance(name, str) or not name:
        raise ScenarioError(f"{path}: missing/empty NAME")
    ops = ns.get("OPS")
    if not isinstance(ops, list) or not ops:
        raise ScenarioError(f"{path}: missing/empty OPS")
    for i, op in enumerate(ops):
        if not isinstance(op, (list, tuple)) or not op or op[0] not in KNOWN_OPS:
            raise ScenarioError(f"{path}: OPS[{i}] is not a known op: {op!r}")
    checks = ns.get("CHECKS", [])
    if not isinstance(checks, list):
        raise ScenarioError(f"{path}: CHECKS must be a list")
    for i, chk in enumerate(checks):
        if not isinstance(chk, (list, tuple)) or not chk or chk[0] not in CHECK_KINDS:
            raise ScenarioError(f"{path}: CHECKS[{i}] unknown: {chk!r}")
    # NEEDS_REAL_PROVIDER: cannot be served by the mock -> SKIP unless
    # --real-provider. NEEDS_PROVIDER is the deprecated alias (a scenario that
    # merely "needs a provider" is treated as real-provider-only, the safe
    # default; scenarios the mock can serve drop the tag entirely).
    needs_provider = bool(ns.get("NEEDS_PROVIDER", False))
    needs_real_provider = bool(ns.get("NEEDS_REAL_PROVIDER", needs_provider))
    # MOCK_SCRIPT: the scenario's scripted mock conversation (s18 lane). A
    # non-list is a scenario-file bug and is rejected here like a bad OPS
    # entry; the individual steps are validated by mock_llm._normalize_script
    # when the server starts, so a malformed step fails only that scenario.
    mock_script = ns.get("MOCK_SCRIPT")
    if mock_script is not None and not isinstance(mock_script, list):
        raise ScenarioError(f"{path}: MOCK_SCRIPT must be a list of steps")
    timeout = ns.get("TIMEOUT")
    return Scenario(
        path=path, name=name, ops=ops, checks=checks,
        timeout=float(timeout) if timeout else None,
        needs_provider=needs_provider,
        needs_real_provider=needs_real_provider,
        offline=bool(ns.get("OFFLINE", not (needs_provider or needs_real_provider))),
        seed_agent=bool(ns.get("SEED_AGENT", True)),
        seed_session=bool(ns.get("SEED_SESSION", False)),
        ns=ns,
        mock_script=mock_script,
        needs_clipboard=bool(ns.get("NEEDS_CLIPBOARD", False)),
    )


def discover(scenarios_dir):
    scenarios, seen = [], {}
    for path in sorted(glob.glob(os.path.join(scenarios_dir, "*.py"))):
        if os.path.basename(path).startswith("_"):
            continue
        scen = load_scenario(path)
        if scen.name in seen:
            raise ScenarioError(
                f"duplicate NAME {scen.name!r}: {path} and {seen[scen.name]}")
        seen[scen.name] = path
        scenarios.append(scen)
    return scenarios


def matches_any(name, patterns):
    for pat in patterns:
        if fnmatch.fnmatchcase(name, pat) or pat in name:
            return True
    return False


# ---------------------------------------------------------------------------
# checks
# ---------------------------------------------------------------------------

RULE_CHARS = {"\u2500"}  # ─ : horizontal box-drawing rule
# Editor-region heuristic (calibrated 2026-10-15 on the fixed build):
#   * The editor/input box is bounded by two horizontal rule rows. The TOP
#     border may carry an inline status label while a run is in flight, e.g.
#     `── ⠇ Working ─────…` (observed in s13_after_enter), so a border row is a
#     row whose stripped text STARTS and ENDS with ─ and is mostly ─ (>=60%);
#     a plain full-width rule matches too.
#   * Rows BELOW the last border pair are the cwd + status rows
#     (`~\\… (master)`, `?/1.0M … (openai) gpt-5 • max`) — they contain
#     no rule chars, so they can never form a pair and never shift the region.
#   * The last adjacent pair (gap <= 8 rows) of such rows wins, so transcript
#     rules above the editor are ignored.
RULE_FRACTION = 0.6
MAX_BOX_GAP = 8


def is_rule_row(line):
    stripped = line.strip()
    if not stripped or stripped[0] not in RULE_CHARS or stripped[-1] not in RULE_CHARS:
        return False
    return stripped.count("\u2500") / len(stripped) >= RULE_FRACTION


def editor_region(lines):
    """Locate the editor/input-box content rows: the content strictly between
    the LAST plausibly-adjacent pair of border rows (see the constants above;
    gap capped at MAX_BOX_GAP so settings dialogs etc. are not mistaken for
    the editor). Returns (rows, lo, hi) or (None, None, None)."""
    idx = [i for i, l in enumerate(lines) if is_rule_row(l)]
    pairs = [(a, b) for a, b in zip(idx, idx[1:]) if 1 <= b - a <= MAX_BOX_GAP]
    if not pairs:
        return None, None, None
    a, b = pairs[-1]
    return lines[a + 1: b], a, b


class ScreenMissing(ScenarioError):
    pass


class Ctx:
    """Everything a check can look at for one scenario run."""

    def __init__(self, scen, scen_dir, result, screens, driver_log_text, stdout_text):
        self.scen = scen
        self.scen_dir = scen_dir
        self.result = result or {}
        self.screens = screens            # {log-basename: [rows]}
        self.driver_log_text = driver_log_text or ""
        self.stdout_text = stdout_text or ""

    def rows(self, snap):
        key = self.scen.name if snap in ("@final", self.scen.name) else snap
        if key not in self.screens:
            raise ScreenMissing(
                f"snap '{snap}' has no rendered screen (expected "
                f"{os.path.join(self.scen_dir, key + '.log')})")
        return self.screens[key]

    def screen_text(self, snap):
        return "\n".join(self.rows(snap))

    def exists(self, snap):
        key = self.scen.name if snap in ("@final", self.scen.name) else snap
        return key in self.screens

    def raw(self, snap):
        """The raw captured byte stream for a snap/final log.

        ANSI escapes are preserved as their control characters (decoded
        latin-1), so log_* checks can assert terminal sequences the pyte
        render erases (e.g. the console-clear `ESC[2J`)."""
        key = self.scen.name if snap in ("@final", self.scen.name) else snap
        path = os.path.join(self.scen_dir, key + ".log")
        if not os.path.exists(path):
            raise ScreenMissing(f"snap '{snap}' has no raw log ({path})")
        with open(path, "rb") as handle:
            return handle.read().decode("latin-1")


def _hint(rows):
    tail = [l for l in rows if l.strip()]
    return repr(tail[-1][:100]) if tail else "<empty screen>"


def _parse_count_expectation(exp):
    if isinstance(exp, int):
        return f"== {exp}", lambda n: n == exp
    m = re.fullmatch(r"\s*(==|=|>=|<=|>|<)\s*(\d+)\s*", str(exp))
    if not m:
        raise ScenarioError(f"bad screen_count expectation: {exp!r}")
    op, num = m.group(1), int(m.group(2))
    ops = {"==": lambda n: n == num, "=": lambda n: n == num,
           ">=": lambda n: n >= num, "<=": lambda n: n <= num,
           ">": lambda n: n > num, "<": lambda n: n < num}
    return f"{op} {num}", ops[op]


def run_check(check, ctx):
    kind = check[0]
    if kind == "not_contains":
        kind = "screen_not_contains"
    spec = [str(a) for a in check[1:]]

    if kind in ("screen_contains", "screen_not_contains"):
        snap, text = check[1], check[2]
        rows = ctx.rows(snap)
        found = [(i, l) for i, l in enumerate(rows) if text in l]
        if kind == "screen_contains":
            if found:
                return True, f"'{text}' on row {found[0][0]}"
            return False, (f"'{text}' not found in screen '{snap}' "
                           f"(last non-empty row: {_hint(rows)})")
        if not found:
            return True, f"'{text}' absent from screen '{snap}'"
        return False, f"'{text}' unexpectedly on row {found[0][0]}: {found[0][1].strip()!r}"

    if kind in ("screen_regex", "screen_not_regex"):
        snap, pattern = check[1], check[2]
        rows = ctx.rows(snap)
        m = re.search(pattern, "\n".join(rows))
        if kind == "screen_regex":
            if m:
                return True, f"/{pattern}/ matched {m.group(0)[:60]!r}"
            return False, (f"/{pattern}/ did not match screen '{snap}' "
                           f"(last non-empty row: {_hint(rows)})")
        if not m:
            return True, f"/{pattern}/ absent from screen '{snap}'"
        return False, f"/{pattern}/ unexpectedly matched {m.group(0)[:60]!r}"

    if kind == "screen_count":
        snap, text, expected = check[1], check[2], check[3]
        rows = ctx.rows(snap)
        n = sum(l.count(text) for l in rows)
        desc, pred = _parse_count_expectation(expected)
        if pred(n):
            return True, f"'{text}' x{n} ({desc})"
        return False, f"'{text}' count={n}, want {desc} in screen '{snap}'"

    if kind == "screen_row_changed":
        snap_a, snap_b, pattern = check[1], check[2], check[3]
        rows_a = [l for l in ctx.rows(snap_a) if re.search(pattern, l)]
        rows_b = [l for l in ctx.rows(snap_b) if re.search(pattern, l)]
        if not rows_a:
            return False, f"no row matches /{pattern}/ in screen '{snap_a}' " \
                          f"(last non-empty row: {_hint(ctx.rows(snap_a))})"
        if not rows_b:
            return False, f"no row matches /{pattern}/ in screen '{snap_b}' " \
                          f"(last non-empty row: {_hint(ctx.rows(snap_b))})"
        if rows_a == rows_b:
            return False, (f"row matching /{pattern}/ is unchanged between "
                           f"'{snap_a}' and '{snap_b}': {rows_a[0].strip()!r}")
        return True, (f"row /{pattern}/ changed: {rows_a[0].strip()!r} -> "
                      f"{rows_b[0].strip()!r}")

    if kind in ("editor_contains", "editor_not_contains"):
        snap, text = check[1], check[2]
        rows = ctx.rows(snap)
        region, lo, hi = editor_region(rows)
        if region is None:
            return False, (f"editor region not found in screen '{snap}' "
                           f"(no adjacent box-rule pair; layout changed?)")
        found = [(i, l) for i, l in enumerate(region) if text in l]
        where = f"rows {lo+1}..{hi-1}"
        if kind == "editor_contains":
            if found:
                return True, f"'{text}' in editor region ({where})"
            return False, (f"'{text}' not in editor region ({where}); "
                           f"region: {[l.strip() for l in region]!r}")
        if not found:
            return True, f"'{text}' absent from editor region ({where})"
        return False, (f"'{text}' still in editor region ({where}): "
                       f"{found[0][1].strip()!r}")

    if kind in ("log_regex", "log_not_regex"):
        snap, pattern = check[1], check[2]
        raw = ctx.raw(snap)
        m = re.search(pattern, raw)
        if kind == "log_regex":
            if m:
                return True, f"/{pattern}/ matched {m.group(0)[:60]!r}"
            return False, f"/{pattern}/ did not match raw log '{snap}'"
        if not m:
            return True, f"/{pattern}/ absent from raw log '{snap}'"
        return False, f"/{pattern}/ unexpectedly matched {m.group(0)[:60]!r}"

    if kind == "log_count":
        snap, text, expected = check[1], check[2], check[3]
        n = ctx.raw(snap).count(text)
        desc, pred = _parse_count_expectation(expected)
        if pred(n):
            return True, f"'{text}' x{n} ({desc})"
        return False, f"'{text}' count={n}, want {desc} in raw log '{snap}'"

    if kind == "log_ordered":
        # ("log_ordered", snap, [seq_a, seq_b, ...]): every sequence is
        # present, in this order (plain substrings, not regexes).
        snap, needles = check[1], check[2]
        if not isinstance(needles, (list, tuple)) or not needles:
            raise ScenarioError(f"bad log_ordered needles: {needles!r}")
        raw = ctx.raw(snap)
        pos = 0
        for needle in needles:
            found = raw.find(needle, pos)
            if found < 0:
                return False, (f"'{needle}' not found after offset {pos} in "
                               f"raw log '{snap}'")
            pos = found + len(needle)
        return True, f"{len(needles)} sequences in order"

    if kind == "restore_markers":
        overrides = check[1] if len(check) > 1 and isinstance(check[1], dict) else {}
        res = ctx.result
        if not res:
            return False, "no driver result json (driver crashed or was hard-killed)"
        counts = res.get("counts") or {}
        reasons = []
        if res.get("timed_out"):
            reasons.append("driver hit the scenario timeout")
        want_exit = overrides.get("exit_code", 0)
        if res.get("exit_code") != want_exit:
            reasons.append(f"exit_code={res.get('exit_code')} (want {want_exit})")
        alive = [m for m in res.get("marks", []) if m.startswith("WARN: still alive")]
        if alive:
            reasons.append(alive[0])
        # Alt-screen switches are NOT observable through this harness. ConPTY
        # consumes the child's ESC[?1049h/ESC[?1049l and never forwards them
        # on the host-side output pipe: a child that writes only those two
        # sequences produces a capture with neither (probe_alt_passthrough.py,
        # 2026-10-29), and pir's own boot capture shows the fullscreen-only
        # mouse capture `?1003;1006h` while 1049h/1049l stay at 0. So the
        # default gate checks only what ConPTY preserves (exit code, clean
        # quit, bracketed paste off, cursor show); a scenario may opt into an
        # exact 1049 expectation only on a harness that keeps the raw child
        # stream.
        want_1049l = overrides.get("alt_screen_exit")
        if want_1049l is not None and counts.get("1049l") != want_1049l:
            reasons.append(f"ESC[?1049l x{counts.get('1049l', 0)} (want {want_1049l})")
        # `_min` mirrors `cursor_show_min`: for scenarios whose pickers own
        # their own alt screen the exact cycle count is timing-dependent,
        # so only the presence (a clean restore) is meaningful.
        min_1049l = overrides.get("alt_screen_exit_min")
        if min_1049l is not None and counts.get("1049l", 0) < min_1049l:
            reasons.append(f"ESC[?1049l x{counts.get('1049l', 0)} (want >= {min_1049l})")
        want_2004l = overrides.get("bracketed_paste_off", 1)
        if counts.get("2004l") != want_2004l:
            reasons.append(f"ESC[?2004l x{counts.get('2004l', 0)} (want {want_2004l})")
        min_25h = overrides.get("cursor_show_min", 1)
        if counts.get("25h", 0) < min_25h:
            reasons.append(f"ESC[?25h x{counts.get('25h', 0)} (want >= {min_25h})")
        if reasons:
            return False, "; ".join(reasons)
        return True, (f"exit_code=0, clean quit; 1049l x{counts.get('1049l')} "
                      f"(unobservable under ConPTY), 2004l x{counts.get('2004l')}, "
                      f"25h x{counts.get('25h')}")

    if kind == "exit_code":
        res = ctx.result
        if not res:
            return False, "no driver result json"
        want = check[1] if len(check) > 1 else 0
        got = res.get("exit_code")
        if got == want:
            return True, f"exit_code={got}"
        return False, f"exit_code={got} (want {want})"

    if kind == "driver_contains":
        text = check[1]
        if text in ctx.driver_log_text:
            return True, f"'{text}' in driver log"
        return False, f"'{text}' not in driver log"

    return False, f"unhandled check kind {kind!r}"


# ---------------------------------------------------------------------------
# run one scenario
# ---------------------------------------------------------------------------

def clipboard_available():
    """Whether the REAL system clipboard can be written and read back.

    A scenario that drives the clipboard (``clipset`` + alt+v) cannot run when
    Windows denies clipboard access — a locked desktop session or a stuck
    clipboard owner makes every ``OpenClipboard`` fail (PowerShell's
    ``Set-Clipboard`` errors with "access to the clipboard failed"). That is an
    environment property, not a product result, so such scenarios SKIP with the
    reason recorded. Probe with the same mechanism the driver uses.
    """
    probe = "pir-battery-clipboard-probe-7f3a"
    script = ("Set-Clipboard -Value '" + probe + "'; "
              "if ((Get-Clipboard -Raw) -notmatch '" + probe + "') { exit 3 }")
    try:
        done = subprocess.run(["powershell", "-NoProfile", "-Command", script],
                              capture_output=True, timeout=30)
        return done.returncode == 0
    except (OSError, subprocess.SubprocessError):
        return False


def new_run_id(out_root):
    stamp = time.strftime("%Y%m%d-%H%M%S")
    run_id, i = stamp, 2
    while os.path.exists(os.path.join(out_root, run_id)):
        run_id = f"{stamp}-{i}"
        i += 1
    return run_id


# ---------------------------------------------------------------------------
# sanitized agent-dir template (credential hygiene)
#
# The battery must never read or copy a live provider config:
#   * `out/tui_agent/models.json` was historically a manual copy of the
#     user's real config (README "place your own models.json"), and
#     seed_agent_dir copied it into EVERY scenario agent dir.  Q27 found 42
#     `out/**/agent/models.json` files carrying literal copies of the user's
#     `~/.pi/agent/models.json` provider keys and the `~/.claude` Anthropic
#     token; 1048 such files accumulated across runs before the tree was
#     cleaned and `scripts/tui_conpty/out/` gitignored.
#   * `probe_turn_latency.py` copied `~/.pi/agent/{models,auth}.json`
#     directly (its old `--config real` default).
#
# The fix is to synthesize the per-run config from constants defined here.
# Provider/model ids keep the ids the recorded runs render in the footer
# (`(openai) gpt-5`), so scenario assertions (e.g. s02) stay valid,
# while the key is the battery's dummy and every baseUrl is a dead loopback
# address until inject_mock_models rewrites it.
# ---------------------------------------------------------------------------

SANITIZED_PROVIDER = "openai"
SANITIZED_MODEL = "gpt-5"
SANITIZED_BASE_URL = "http://127.0.0.1:9/v1"  # dead port; rewritten per mock run


def sanitized_models_config():
    """Self-contained models.json: dummy key, 1M context, mock cost rates."""
    return {"providers": {SANITIZED_PROVIDER: {
        "name": "OpenAI (sanitized battery template)",
        "baseUrl": SANITIZED_BASE_URL,
        "api": "openai-completions",
        "apiKey": MOCK_API_KEY,
        "models": [{
            "id": SANITIZED_MODEL,
            "name": SANITIZED_MODEL,
            "reasoning": True,
            "input": ["text"],
            "contextWindow": 1_000_000,
            "maxTokens": 64_000,
            "cost": dict(MOCK_COST_RATES),
        }],
    }}}


def sanitized_settings_config(provider=SANITIZED_PROVIDER,
                              model=SANITIZED_MODEL):
    """Self-contained settings.json: the non-secret fields the scenarios use.

    Deliberately NO `packages` list (loading the user's extensions is not
    needed by any scenario and would execute third-party code in the run).
    """
    return {
        "lastChangelogVersion": "1.1.0",
        "theme": "dark",
        "defaultProvider": provider,
        "defaultModel": model,
        "defaultThinkingLevel": "max",
        "hideThinkingBlock": False,
        "showCacheMissNotices": True,
        "enableInstallTelemetry": False,
    }


# ---------------------------------------------------------------------------
# Credential-template validation (strict, whole-value)
# ---------------------------------------------------------------------------
# Upstream resolves `apiKey`, provider/model `headers` and `customHeaders`
# values like a credential (`schemas/models.schema.json`: "Values resolve like
# apiKey"), and a URL may carry credentials in its userinfo. Under
# --real-provider the template is copied verbatim into
# out/<run>/<scenario>/agent/models.json, so EVERY credential-capable value is
# checked below -- not only fields whose name looks secret.
#
# Accepted, whole value only:
#   ${ENV} / $ENV   ^\$(?:\{[A-Za-z_][A-Za-z0-9_]*\}|[A-Za-z_][A-Za-z0-9_]*)$
#   !command        a whole command (see the trust boundary below)
#   sk-mock-battery the battery's dummy key
#
# Refused:
#   * any other literal in a credential-capable field (apiKey, headers.*,
#     customHeaders.*) or in a URL's userinfo ("scheme://user:literal@");
#   * any value that mixes a template marker with literal text
#     ("sk-LITERAL-${ENV}", "${ENV}/v1", "https://user:${PASS}@host");
#     this whole-value rule applies to non-command values -- a `!command` is
#     literal command text by nature and gets the command checks below;
#   * key-like literals anywhere in the template (`sk-...`, `Bearer <token>`,
#     `Cookie: ...`, JWTs, 32+ hex, 40+ base64, known AWS/GitHub/Slack/Google
#     prefixes).
#
# `!command` is an explicit operator opt-in. The command text is copied into
# models.json and then executed by the TUI, so it is accepted only when it
# does not embed an obvious literal key shape (`sk-...`, `Bearer <token>`,
# `Cookie:`, a 20+ alnum token, URL userinfo). Residual trust: a command that
# reads a key at runtime from a file, helper script or secret manager cannot
# be judged by a static validator; never point --models-template at a command
# you do not trust.
#
# Diagnostics are path/shape-only. The literal value itself is NEVER included
# in the diagnostic, so a refused template cannot leak through stderr/logs.
_SECRET_FIELD_NAMES = {
    "apikey", "authorization", "token", "accesstoken", "secret",
    "clientsecret", "password", "passwd", "bearer", "refreshtoken",
    "privatekey", "secretkey", "accesskey", "credential", "credentials",
}
# Names that END in one of these are credential fields too (`X-Api-Key`,
# `clientSecret`, ...). Checked on a lowercase, '-'/'_'-stripped name.
_CREDENTIAL_SUFFIXES = (
    "apikey", "authorization", "token", "secret", "password", "passwd",
    "privatekey", "secretkey", "accesskey",
)
# Header maps: every value resolves like apiKey, whatever the header is named.
_HEADER_MAP_NAMES = {
    "headers", "customheaders", "extraheaders", "httpheaders",
}
# Upstream template syntax, WHOLE value only: `$ENV` or `${ENV}`.
_ENV_TEMPLATE_FULL_RE = re.compile(
    r"\$(?:\{[A-Za-z_][A-Za-z0-9_]*\}|[A-Za-z_][A-Za-z0-9_]*)\Z")
# Any template marker (including malformed/defaulted forms such as
# `${ENV:-x}`): a value containing one must be a full template.
_TEMPLATE_MARKER_RE = re.compile(r"\$(?:[A-Za-z_][A-Za-z0-9_]*|\{)")


def _norm_field_name(key):
    """Case-, '-' and '_'-insensitive field name ("X-Api-Key" -> "xapikey")."""
    return re.sub(r"[-_]", "", str(key).strip().lower())


def _is_credential_field_name(key):
    norm = _norm_field_name(key)
    return norm in _SECRET_FIELD_NAMES or norm.endswith(_CREDENTIAL_SUFFIXES)


# Literal key shapes refused anywhere in a template value. The tag is used in
# diagnostics; the matched text is never printed.
_LITERAL_KEY_PATTERNS = (
    ("sk-key", re.compile(r"(?i)(?<![a-z0-9])sk-[a-z0-9_-]{4,}")),
    ("bearer-token", re.compile(r"(?i)\bbearer\s+(?!\$)[a-z0-9._~+/=-]{4,}")),
    ("cookie-header", re.compile(r"(?i)\bcookie\s*:\s*(?!\$)\S")),
    ("aws-key", re.compile(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b")),
    ("github-token", re.compile(
        r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})")),
    ("slack-token", re.compile(r"\bxox[baprs]-[A-Za-z0-9-]{10,}")),
    ("google-key", re.compile(r"\bAIza[0-9A-Za-z_-]{30,}")),
    ("jwt", re.compile(
        r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}")),
    ("hex-token", re.compile(
        r"(?<![0-9A-Fa-f])[0-9A-Fa-f]{32,}(?![0-9A-Fa-f])")),
    ("base64-token", re.compile(
        r"(?<![A-Za-z0-9+/_-])[A-Za-z0-9+/_-]{40,}={0,2}"
        r"(?![A-Za-z0-9+/=_-])")),
    ("key-assignment", re.compile(
        r"(?i)\b(?:api[_-]?key|access[_-]?token|token|secret|password)"
        r"\s*[:=]\s*(?![\"']?\$)[^\s\"']{4,}")),
    ("key-flag", re.compile(
        r"(?i)--?(?:api[-_]?key|access[-_]?token|token|secret|password)"
        r"\s+(?![\"']?\$)[^\s\"']{4,}")),
)
# `!command` additionally refuses a bare 20+ alnum token: it is key-like and
# can be wrapped in an env template if a long path/id is really needed.
_COMMAND_LITERAL_PATTERNS = _LITERAL_KEY_PATTERNS + (
    ("long-token", re.compile(
        r"(?<![A-Za-z0-9])[A-Za-z0-9]{20,}(?![A-Za-z0-9])")),
)
_URL_USERINFO_RE = re.compile(r"(?i)\b[a-z][a-z0-9+.-]*://[^/?#\s]*@")


def _template_value_reasons(value, credential_field=False):
    """Reasons a string is not an acceptable credential template (empty = OK).

    Non-command values must be a whole `$ENV`/`${ENV}` template (or the exact
    dummy); a template marker plus any other text is refused. A whole
    `!command` is the documented trust boundary and is refused only when its
    command text embeds an obvious literal key shape. The value itself is
    never returned, so callers can print the reasons without echoing a
    credential.
    """
    if not isinstance(value, str):
        return []  # bools/numbers/nulls cannot carry a credential literal
    if value == MOCK_API_KEY:  # EXACT battery dummy only
        return []
    if value.startswith("!"):
        # The whole value is a command: the documented trust boundary.
        if not value[1:].strip():
            return ["empty-command"]
        reasons = [f"command-embeds-{name}"
                   for name, rx in _COMMAND_LITERAL_PATTERNS if rx.search(value)]
        if _URL_USERINFO_RE.search(value):
            reasons.append("url-userinfo")
        return reasons
    if _ENV_TEMPLATE_FULL_RE.fullmatch(value):
        return []
    reasons = []
    if credential_field:
        reasons.append("literal-credential-value")
    if _TEMPLATE_MARKER_RE.search(value):
        # Template marker plus any other text is a mixed value: refused.
        reasons.append("mixed-template-and-literal")
    reasons.extend(f"literal-key-shape-{name}"
                   for name, rx in _LITERAL_KEY_PATTERNS if rx.search(value))
    if _URL_USERINFO_RE.search(value):
        reasons.append("url-userinfo")
    return reasons


def template_secret_violations(config):
    """Paths (+ shape tags) of values that are not whole credential templates.

    Paths and pattern tags only; values are never included, so the diagnostic
    can be printed without leaking the credential the template was supposed
    to hide.
    """
    bad = []

    def check_string(value, path, credential_field):
        reasons = _template_value_reasons(value, credential_field)
        if reasons:
            bad.append(f"{path} [{', '.join(dict.fromkeys(reasons))}]")

    def walk(node, path, credential_field=False):
        if isinstance(node, dict):
            for key, value in node.items():
                here = f"{path}.{key}" if path else str(key)
                norm = _norm_field_name(key)
                if norm in _HEADER_MAP_NAMES:
                    # Every header value resolves like apiKey.
                    if isinstance(value, dict):
                        for hk, hv in value.items():
                            if isinstance(hv, str):
                                check_string(hv, f"{here}.{hk}", True)
                            else:
                                walk(hv, f"{here}.{hk}", True)
                    else:
                        walk(value, here, True)  # malformed map: still strict
                    continue
                if _is_credential_field_name(key):
                    walk(value, here, True)
                    continue
                walk(value, here, credential_field)
        elif isinstance(node, list):
            for i, value in enumerate(node):
                walk(value, f"{path}[{i}]", credential_field)
        elif isinstance(node, str):
            check_string(node, path, credential_field)

    walk(config, "")
    return bad


def _credential_error(label, violations):
    return ScenarioError(
        f"{label} has non-template credential values at "
        + ", ".join(violations)
        + "; each credential-bearing value must be a WHOLE $ENV/${ENV}/"
          "!command template or the battery dummy. Literal header and URL-"
          "userinfo values and mixed literal+template values are refused. "
          "Never copy ~/.pi/agent or ~/.claude config.")


def load_models_template(path):
    """Load and validate a user-supplied --models-template path.

    Raises ScenarioError (never returns) when the file is unreadable, is not
    a models.json shape, or contains a non-template credential. Validation
    reads the file; it only ever COPIES it when every credential-capable
    value is a whole env/command template or the exact dummy key.
    """
    try:
        with open(path, encoding="utf-8") as f:
            config = json.load(f)
    except (OSError, ValueError) as exc:
        raise ScenarioError(f"cannot read --models-template {path}: {exc}")
    if not isinstance(config, dict) or not isinstance(config.get("providers"), dict) \
            or not config["providers"]:
        raise ScenarioError("--models-template must be a models.json object "
                            "with a non-empty 'providers' object")
    violations = template_secret_violations(config)
    if violations:
        raise _credential_error("--models-template", violations)
    return config


def template_selection(config):
    """(provider id, model id) of the first provider in a models template."""
    providers = config["providers"]
    provider = next(iter(providers))
    models = providers[provider].get("models") or []
    model = None
    if models and isinstance(models[0], dict):
        model = models[0].get("id")
    return provider, model or "mock-model"


def seed_agent_dir(agent_dir, do_seed, models_template=None):
    """Write the per-scenario agent config from the SANITIZED template.

    Nothing is read from `out/tui_agent/` or the user's home: those trees may
    contain live credentials. `models_template` is an already-validated dict
    from load_models_template() (the --models-template opt-in); it is
    re-validated here as defense in depth BEFORE any directory or file is
    created, so an unvalidated template can never seed a config.
    """
    if models_template is not None:
        violations = template_secret_violations(models_template)
        if violations:
            raise _credential_error("models template", violations)
    os.makedirs(agent_dir, exist_ok=True)
    if not do_seed:
        return []
    if models_template is None:
        models = sanitized_models_config()
        provider, model = SANITIZED_PROVIDER, SANITIZED_MODEL
    else:
        models = models_template
        provider, model = template_selection(models)
    settings = sanitized_settings_config(provider, model)
    with open(os.path.join(agent_dir, "models.json"), "w", encoding="utf-8") as f:
        json.dump(models, f, indent=2)
    with open(os.path.join(agent_dir, "settings.json"), "w", encoding="utf-8") as f:
        json.dump(settings, f, indent=2)
    return ["models.json", "settings.json"]


def run_selftest():
    """Strict credential-template validator selftest.

    Pure functions only: no scenario module is exec'd, no out/ dir is created,
    no TUI/pir process is started and no model call is made. Every rejected
    value embeds SENTINEL; the diagnostic must list the field path but never
    the sentinel (the path/shape-only guarantee).
    """
    sentinel = "Q48LEAKSENTINELABCDEFGH"  # 24 alnum: also trips long-token
    results = []

    def record(name, ok, detail=""):
        results.append((name, bool(ok), detail))

    def expect_reject(name, config, path):
        violations = template_secret_violations(config)
        joined = "; ".join(violations)
        if not violations:
            record(name, False, "no violation reported")
        elif sentinel in joined:
            record(name, False, "VALUE ECHOED in diagnostic")
        elif path not in joined:
            record(name, False, f"missing path {path}")
        else:
            record(name, True)

    def expect_accept(name, config):
        violations = template_secret_violations(config)
        record(name, not violations,
               "" if not violations else "unexpected: " + ", ".join(violations))

    def pcfg(field, value):
        return {"providers": {"p": {field: value}}}

    # ---- Q47's failing cases: all must be refused, path-only diagnostics.
    expect_reject("apiKey literal sk-key",
                  pcfg("apiKey", f"sk-{sentinel}"), "providers.p.apiKey")
    expect_reject("headers.X-Api-Key literal",
                  pcfg("headers", {"X-Api-Key": sentinel}),
                  "providers.p.headers.X-Api-Key")
    expect_reject("headers.Cookie literal",
                  pcfg("headers", {"Cookie": f"session={sentinel}"}),
                  "providers.p.headers.Cookie")
    expect_reject("customHeaders.X-Api-Key literal",
                  {"providers": {"p": {"customHeaders": {"X-Api-Key": sentinel}}}},
                  "providers.p.customHeaders.X-Api-Key")
    expect_reject("apiKey sk-literal + ${ENV}",
                  pcfg("apiKey", f"sk-{sentinel}-${{ENV_NAME}}"),
                  "providers.p.apiKey")
    expect_reject("!command embeds sk-literal",
                  pcfg("apiKey", f"!echo sk-{sentinel}"),
                  "providers.p.apiKey")
    expect_reject("baseUrl URL userinfo literal",
                  pcfg("baseUrl", f"https://user:{sentinel}@example.invalid/v1"),
                  "providers.p.baseUrl")
    expect_reject("model baseUrl URL userinfo literal",
                  {"providers": {"p": {"models": [
                      {"id": "m", "baseUrl":
                       f"https://user:{sentinel}@example.invalid/v1"}]}}},
                  "providers.p.models[0].baseUrl")
    expect_reject("header mixed ${ENV}/literal",
                  pcfg("headers", {"X-Token": f"${{ENV_OK}}/{sentinel}"}),
                  "providers.p.headers.X-Token")
    expect_reject("plain literal header value (Content-Type)",
                  pcfg("headers", {"Content-Type": "application/json"}),
                  "providers.p.headers.Content-Type")
    expect_reject("literal Bearer token in name",
                  pcfg("name", f"Bearer {sentinel}"), "providers.p.name")
    expect_reject("Cookie:-style literal in name",
                  pcfg("name", f"Cookie: session={sentinel}"), "providers.p.name")
    expect_reject("32-hex literal in model id",
                  {"providers": {"p": {"models": [{"id": "deadbeef" * 4}]}}},
                  "providers.p.models[0].id")
    expect_reject("40-char base64 literal in model id",
                  {"providers": {"p": {"models": [{"id": "QUJD" * 10}]}}},
                  "providers.p.models[0].id")
    expect_reject("trailing space after ${ENV}",
                  pcfg("apiKey", "${ENV_OK} "), "providers.p.apiKey")
    expect_reject("defaulted ${ENV:-literal}",
                  pcfg("apiKey", "${ENV_OK:-fallback}"), "providers.p.apiKey")
    expect_reject("nested clientSecret literal",
                  pcfg("auth", {"clientSecret": sentinel}),
                  "providers.p.auth.clientSecret")
    expect_reject("!command embeds 20+ alnum token",
                  pcfg("apiKey", "!echo " + sentinel), "providers.p.apiKey")

    # ---- Previously accepted forms: must stay accepted.
    expect_accept("apiKey ${ENV}", pcfg("apiKey", "${PIR_BATTERY_API_KEY}"))
    expect_accept("apiKey $ENV", pcfg("apiKey", "$PIR_BATTERY_API_KEY"))
    expect_accept("apiKey exact dummy", pcfg("apiKey", MOCK_API_KEY))
    expect_accept("apiKey !command", pcfg("apiKey", "!op read op://vault/item"))
    expect_accept("apiKey !command with $VAR",
                  pcfg("apiKey", '!printf %s "$PIR_BATTERY_API_KEY"'))
    expect_accept("apiKey !command helper", pcfg("apiKey", "!runtime-helper"))
    expect_accept("headers ${ENV}",
                  pcfg("headers", {"X-Api-Key": "${PIR_BATTERY_API_KEY}"}))
    expect_accept("headers $ENV", pcfg("headers", {"X-Custom": "$CUSTOM_HEADER"}))
    expect_accept("customHeaders !command",
                  {"providers": {"p": {"customHeaders":
                                       {"X-Token": "!op read op://vault/token"}}}})
    expect_accept("baseUrl dead loopback", pcfg("baseUrl", SANITIZED_BASE_URL))
    expect_accept("baseUrl whole env template", pcfg("baseUrl", "$PIR_BASE_URL"))
    expect_accept("plain ids/names",
                  {"providers": {"p": {
                      "name": "OpenAI (sanitized battery template)",
                      "api": "openai-completions",
                      "models": [{"id": "gpt-5"}]}}})
    expect_accept("!command with harmless URL",
                  pcfg("apiKey", "!curl https://example.invalid/v1"))

    # ---- The real gates: load_models_template and seed_agent_dir use the
    # same validator and must refuse BEFORE writing anything.
    good = {"providers": {"p": {
        "baseUrl": SANITIZED_BASE_URL,
        "apiKey": "${PIR_BATTERY_API_KEY}",
        "models": [{"id": "m"}]}}}
    bad = {"providers": {"p": {
        "baseUrl": SANITIZED_BASE_URL,
        "headers": {"X-Api-Key": sentinel},
        "models": [{"id": "m"}]}}}
    with tempfile.TemporaryDirectory(prefix="q48-selftest-") as tmp:
        good_path = os.path.join(tmp, "good.json")
        bad_path = os.path.join(tmp, "bad.json")
        with open(good_path, "w", encoding="utf-8") as f:
            json.dump(good, f)
        with open(bad_path, "w", encoding="utf-8") as f:
            json.dump(bad, f)
        try:
            record("load_models_template accepts whole ${ENV}",
                   load_models_template(good_path) == good)
        except ScenarioError:
            record("load_models_template accepts whole ${ENV}", False,
                   "unexpected ScenarioError")
        try:
            load_models_template(bad_path)
            record("load_models_template refuses header literal", False,
                   "no ScenarioError")
        except ScenarioError as exc:
            msg = str(exc)
            record("load_models_template refuses header literal",
                   "providers.p.headers.X-Api-Key" in msg and sentinel not in msg,
                   "" if sentinel not in msg else "VALUE ECHOED in diagnostic")
        agent_dir = os.path.join(tmp, "agent")
        try:
            seed_agent_dir(agent_dir, True, bad)
            record("seed_agent_dir refuses before writing", False,
                   "no ScenarioError")
        except ScenarioError as exc:
            msg = str(exc)
            record("seed_agent_dir refuses before writing",
                   not os.path.exists(agent_dir) and sentinel not in msg,
                   "" if sentinel not in msg else "VALUE ECHOED in diagnostic")
        seed_agent_dir(agent_dir, True, good)
        with open(os.path.join(agent_dir, "models.json"), encoding="utf-8") as f:
            seeded = json.load(f)
        record("seed_agent_dir writes only a validated template", seeded == good)

    failed = [name for name, ok, _ in results if not ok]
    print("Q48 credential-template selftest (pure functions; no scenarios, "
          "TUI or model calls)")
    width = max(len(name) for name, _, _ in results)
    for name, ok, detail in results:
        line = f"  {'PASS' if ok else 'FAIL'}  {name:<{width}}"
        if detail:
            line += f"  ({detail})"
        print(line)
    print(f"selftest: {len(results) - len(failed)}/{len(results)} PASS")
    if failed:
        print("selftest FAILED: " + ", ".join(failed))
        return 1
    return 0


def encode_cwd_dir_name(cwd):
    """Mirror pir's `encode_cwd_dir_name` (src/session_manager/mod.rs:179):
    strip ONE leading separator, map / \\ : to '-', wrap in '--'."""
    stripped = cwd[1:] if cwd[:1] in ("/", "\\") else cwd
    safe = re.sub(r"[/\\:]", "-", stripped)
    return f"--{safe}--"


def _win_long_path(path):
    """Verbatim Win32 path for >MAX_PATH filesystem ops (Python's open()
    otherwise fails with FileNotFoundError around 260 chars). The FILE itself
    stays at the normal location -- only the API call uses the \\\\?\\ form;
    pir (Rust) reads the real path with its own long-path handling."""
    if os.name != "nt":
        return path
    absolute = os.path.abspath(path)
    if absolute.startswith("\\\\"):
        return path
    return "\\\\?\\" + absolute


def seed_session_fixture(agent_dir, cwd):
    """Write one minimal valid v3 session JSONL for `cwd` under
    <agent>/sessions/<encoded-cwd>/ so /resume has something to list.

    Needed because pir defers the CURRENT session's file to the first
    message: a fresh per-scenario agent dir has zero session files, and
    /resume then warns "No sessions found for this directory" instead of
    opening the picker. Header shape observed 2026-10-15 in
    out/tui_agent/sessions/*.jsonl ({"type":"session","version":3,...}).
    The encoded-cwd dir name plus artifact paths can exceed MAX_PATH (the
    s14 path is ~288 chars), hence _win_long_path: this machine has
    LongPathsEnabled=0, and when the prefix covered only the makedirs the
    write still raised FileNotFoundError even though the directory existed
    (the calib-1 battery error). Verified 2026-10-15 on the s14 runs in
    out/20261015-162749 and out/20261015-163424: with the prefix on both the
    fixture lands and the picker lists it
    ("seeded battery fixture session  1 now")."""
    session_id = "00000000-0000-4000-8000-000000000b16"
    stamp = "2026-01-01T00:00:00.000Z"
    header = {"type": "session", "version": 3, "id": session_id,
              "timestamp": stamp, "cwd": cwd}
    message = {"type": "message", "id": "fixture01", "parentId": None,
               "timestamp": stamp,
               "message": {"role": "user",
                           "content": [{"type": "text",
                                        "text": "seeded battery fixture session"}],
                           "timestamp": 0}}
    session_dir = os.path.join(agent_dir, "sessions", encode_cwd_dir_name(cwd))
    os.makedirs(_win_long_path(session_dir), exist_ok=True)
    path = os.path.join(session_dir, "2026-01-01T00-00-00-000Z_" + session_id + ".jsonl")
    with open(_win_long_path(path), "w", encoding="utf-8") as f:
        f.write(json.dumps(header) + "\n")
        f.write(json.dumps(message) + "\n")
    return path


def _fallback_mock_config(base_url):
    """Minimal self-contained provider config (used if the sanitized
    models.json was removed by a scenario/hook; keeps round-trips hermetic)."""
    return {"providers": {"mock": {
        "name": "Mock",
        "baseUrl": base_url,
        "api": "openai-completions",
        "apiKey": MOCK_API_KEY,
        "models": [{
            "id": "mock-model", "name": "Mock", "reasoning": False,
            "input": ["text"], "contextWindow": 100000, "maxTokens": 4096,
            "cost": dict(MOCK_COST_RATES),
        }],
    }}}


def inject_mock_models(agent_dir, mock_base_url):
    """Hermetic provider injection: rewrite <agent>/models.json so every
    provider/model id from the sanitized (or --models-template) config points
    at the local mock.

    - baseUrl -> http://127.0.0.1:<port>/v1 (task-mandated override)
    - apiKey  -> dummy constant (no literal credential can survive the copy)
    - per-model `cost` rates added only when absent (s08b's /session Cost
      block renders only when cost > 0)
    - settings.json: left untouched when its defaultProvider/defaultModel
      still resolve; otherwise retargeted (or created) to the first
      provider/model, so the app always has a model to boot with.
    Returns a record dict for the report.
    """
    models_path = os.path.join(agent_dir, "models.json")
    seed = None
    if os.path.isfile(models_path):
        try:
            with open(models_path, encoding="utf-8") as f:
                seed = json.load(f)
        except (OSError, ValueError):
            seed = None
    if not isinstance(seed, dict) or not isinstance(seed.get("providers"), dict) \
            or not seed["providers"]:
        config = _fallback_mock_config(mock_base_url)
    else:
        config = seed
    providers = config["providers"]
    for provider in providers.values():
        if not isinstance(provider, dict):
            continue
        provider["baseUrl"] = mock_base_url
        provider["apiKey"] = MOCK_API_KEY
        for model in provider.get("models", []) or []:
            if isinstance(model, dict) and not isinstance(model.get("cost"), dict):
                model["cost"] = dict(MOCK_COST_RATES)
    with open(models_path, "w", encoding="utf-8") as f:
        json.dump(config, f, indent=2)

    provider_ids = list(providers.keys())
    first_provider = provider_ids[0]
    first_models = providers[first_provider].get("models") or []
    first_model = (first_models[0].get("id") if first_models else None) or "mock-model"

    settings_path = os.path.join(agent_dir, "settings.json")
    settings = None
    if os.path.isfile(settings_path):
        try:
            with open(settings_path, encoding="utf-8") as f:
                settings = json.load(f)
        except (OSError, ValueError):
            settings = None
    selected = None
    if isinstance(settings, dict):
        selected = (settings.get("defaultProvider"), settings.get("defaultModel"))
    valid = (isinstance(selected, tuple) and selected[0] in providers
             and any(isinstance(m, dict) and m.get("id") == selected[1]
                     for m in providers[selected[0]].get("models", []) or []))
    action = "kept"
    if not isinstance(settings, dict):
        settings = {}
        action = "created"
    if not valid:
        action = "retargeted" if os.path.isfile(settings_path) else "created"
        settings["defaultProvider"] = first_provider
        settings["defaultModel"] = first_model
        with open(settings_path, "w", encoding="utf-8") as f:
            json.dump(settings, f, indent=2)
    return {
        "models_json": models_path,
        "provider_ids": provider_ids,
        "selected": f"{settings.get('defaultProvider')}/{settings.get('defaultModel')}",
        "settings_action": action,
    }


def pir_info(pir):
    try:
        st = os.stat(pir)
        return {"path": pir, "size": st.st_size,
                "mtime": time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(st.st_mtime))}
    except OSError:
        return {"path": pir, "size": None, "mtime": None}


def execute(scen, args, run_dir, pir, use_mock=True):
    scen_dir = os.path.join(run_dir, scen.name)
    agent_dir = os.path.join(scen_dir, "agent")
    os.makedirs(scen_dir, exist_ok=True)
    seeded = seed_agent_dir(agent_dir, scen.seed_agent,
                            getattr(args, "models_template_config", None))
    if scen.seed_session:
        seed_session_fixture(agent_dir, os.path.abspath(agent_dir))
    result_json = os.path.join(scen_dir, scen.name + ".result.json")
    driver_log = os.path.join(scen_dir, scen.name + ".driver.log")
    timeout = args.timeout if args.timeout is not None else (scen.timeout or DEFAULT_TIMEOUT)
    hard = timeout + HARD_TIMEOUT_SLACK

    # Hermetic provider: every SEEDED scenario (models.json present, i.e. the
    # scenario did not opt out via SEED_AGENT=False) gets the mock injected.
    # SEED_AGENT=False scenarios stay provider-free by design (s13/s15).
    mock = None
    mock_rec = None
    if use_mock and scen.seed_agent:
        mock_log = os.path.join(scen_dir, scen.name + ".mock_llm.log")
        # MOCK_SCRIPT: the scenario's scripted steps (if any) drive the mock;
        # absent keeps the fixed "mock reply".
        mock = MockServer(log_path=mock_log, script=scen.mock_script)
        injected = inject_mock_models(agent_dir, mock.base_url)
        mock_rec = {"base_url": mock.base_url, "log": mock_log,
                    "models_json": injected["models_json"],
                    "provider_ids": injected["provider_ids"],
                    "selected": injected["selected"],
                    "settings_action": injected["settings_action"],
                    "script_steps": len(scen.mock_script or ()),
                    "requests": None}
        if "models.json" in seeded:
            seeded[seeded.index("models.json")] = "models.json (mock-injected)"
        else:
            seeded.append("models.json (mock-injected)")

    argv = [sys.executable, DRIVER, scen.path,
            "--shots-dir", scen_dir,
            "--driver-log", driver_log,
            "--result-json", result_json,
            "--agent-dir", agent_dir,
            "--timeout", str(timeout)]
    if scen.offline:
        argv.append("--offline")
    env = dict(os.environ)
    env["PIR_TUI_EXE"] = pir
    env["PYTHONIOENCODING"] = "utf-8"

    rec = {"name": scen.name, "file": scen.path, "dir": scen_dir,
           "timeout_s": timeout, "hard_timeout_s": hard,
           "seeded_agent_files": seeded, "offline": scen.offline,
           "needs_provider": scen.needs_provider,
           "needs_real_provider": scen.needs_real_provider,
           "mock": mock_rec,
           "status": "RUNNING", "duration_s": None, "checks": [],
           "hard_timeout": False, "result": None}
    t0 = time.time()
    try:
        proc = subprocess.Popen(argv, cwd=HERE, env=env,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                text=True, encoding="utf-8", errors="replace")
        out = err = ""
        try:
            out, err = proc.communicate(timeout=hard)
        except subprocess.TimeoutExpired:
            rec["hard_timeout"] = True
            # Kill OUR OWN process tree only (the driver + the pir.exe it spawned).
            subprocess.run(["taskkill", "/PID", str(proc.pid), "/T", "/F"],
                           capture_output=True)
            try:
                out, err = proc.communicate(timeout=15)
            except subprocess.TimeoutExpired:
                proc.kill()
                out, err = "", ""
        rec["driver_returncode"] = proc.returncode
    finally:
        if mock is not None:
            # Stop the mock only after the driver (and the pir child it owns)
            # has exited, so no request can arrive on a closed socket.
            mock_rec["requests"] = mock.request_count
            mock.stop()
    rec["duration_s"] = round(time.time() - t0, 2)

    stdout_path = os.path.join(scen_dir, scen.name + ".stdout.txt")
    with open(stdout_path, "w", encoding="utf-8") as f:
        f.write(out or "")
        if err:
            f.write("\n----- stderr -----\n" + err)
    rec["stdout"] = stdout_path
    rec["driver_log"] = driver_log
    rec["result_json"] = result_json

    result = None
    if os.path.isfile(result_json):
        try:
            with open(result_json, encoding="utf-8") as f:
                result = json.load(f)
        except (OSError, ValueError):
            result = None
    rec["result"] = {k: result.get(k) for k in
                     ("exit_code", "timed_out", "counts", "marks", "snaps")} if result else None
    if result:
        rec["exit_code"] = result.get("exit_code")
        rec["timed_out"] = result.get("timed_out")
        rec["marks"] = result.get("marks", [])
        rec["counts"] = result.get("counts", {})
    driver_log_text = ""
    if os.path.isfile(driver_log):
        with open(driver_log, encoding="utf-8", errors="replace") as f:
            driver_log_text = f.read()

    # Render EVERY app log in the scenario dir (snaps + final) to .screen.txt.
    screens = {}
    rendered = []
    for log_path, screen_path, lines in acc3_render.render_dir(scen_dir):
        key = os.path.basename(log_path)[: -len(".log")]
        screens[key] = lines
        rendered.append({"log": log_path, "screen": screen_path})
    rec["rendered"] = rendered

    # Infra failures short-circuit the content checks with a clear reason.
    infra = None
    if rec["hard_timeout"]:
        infra = (f"hard timeout: driver exceeded {hard:.0f}s "
                 f"(scenario budget {timeout:.0f}s); process tree killed")
    elif result is None:
        tail = (out or "").strip().splitlines()[-3:]
        infra = ("driver produced no result json (crash / killed); "
                 f"stdout tail: {' | '.join(tail) if tail else '<empty>'}")
    elif result.get("timed_out"):
        infra = (f"scenario exceeded its {timeout:.0f}s budget; "
                 f"quit path was forced (see marks)")

    ctx = Ctx(scen, scen_dir, result, screens, driver_log_text, out)
    checks = []
    for chk in scen.checks:
        try:
            ok, detail = run_check(list(chk), ctx)
        except ScenarioError as exc:
            ok, detail = False, str(exc)
        except Exception as exc:  # noqa: BLE001 - report, never crash the battery
            ok, detail = False, f"check raised {type(exc).__name__}: {exc}"
        checks.append({"check": chk[0], "args": [str(a) for a in chk[1:]],
                       "ok": ok, "detail": detail})
    rec["checks"] = checks

    # Acceptance rule (foreman, 2026-10-15): the driver's quit path has an
    # escalation (esc → `/quit` → RETRY → force-kill) that made every scenario
    # "pass" while the app itself never exited on a single `/quit` (play-lane
    # F1). A scenario is only accepted when the FIRST quit attempt sufficed.
    QUIT_RETRY_MARK = "RETRY: still alive after first esc+/quit"
    if QUIT_RETRY_MARK in driver_log_text:
        checks.append({
            "check": "single_quit_suffices",
            "args": [],
            "ok": False,
            "detail": ("the app needed the driver's quit retry: one `/quit` must "
                       "terminate the process (play-lane F1 regression)"),
        })

    if infra:
        rec["status"] = "FAIL"
        rec["reason"] = infra
    elif all(c["ok"] for c in checks):
        rec["status"] = "PASS"
    else:
        rec["status"] = "FAIL"
    first_bad = next((c for c in checks if not c["ok"]), None)
    if rec["status"] == "FAIL":
        if first_bad:
            rec["first_failure"] = f"{first_bad['check']}({', '.join(first_bad['args'])}) — {first_bad['detail']}"
            if infra:
                rec["first_failure"] = f"{infra}; first check failure: {rec['first_failure']}"
        else:
            rec["first_failure"] = infra or "unknown failure"
    return rec


# ---------------------------------------------------------------------------
# reports
# ---------------------------------------------------------------------------

def fmt_check(chk):
    args = ", ".join(chk["args"])
    return f"{chk['check']}({args})" if args else f"{chk['check']}()"


def write_reports(run_dir, meta, records):
    report_json = os.path.join(run_dir, "report.json")
    report_md = os.path.join(run_dir, "report.md")
    summary = {
        "total": len(records),
        "passed": sum(1 for r in records if r["status"] == "PASS"),
        "failed": sum(1 for r in records if r["status"] == "FAIL"),
        "skipped": sum(1 for r in records if r["status"] == "SKIP"),
    }
    with open(report_json, "w", encoding="utf-8") as f:
        json.dump({"run": meta, "summary": summary, "scenarios": records}, f, indent=2)

    lines = []
    lines.append(f"# TUI battery report — {meta['run_id']}")
    lines.append("")
    lines.append(f"- pir: `{meta['pir']['path']}` "
                 f"({meta['pir']['size']} B, mtime {meta['pir']['mtime']})")
    lines.append(f"- provider mode: {meta.get('mock', 'n/a')}")
    lines.append(f"- started: {meta['started']} · finished: {meta['finished']} "
                 f"· wall: {meta['duration_s']}s")
    lines.append(f"- python: {meta['python']}")
    lines.append(f"- scenarios: {summary['total']} — "
                 f"{summary['passed']} PASS, {summary['failed']} FAIL, "
                 f"{summary['skipped']} SKIP")
    lines.append("")
    lines.append("| # | status | scenario | time | first failure |")
    lines.append("|---|---|---|---|---|")
    for i, r in enumerate(records, 1):
        first = (r.get("first_failure") or r.get("reason") or "").replace("|", "\\|")
        lines.append(f"| {i} | {r['status']} | {r['name']} | "
                     f"{r['duration_s'] if r['duration_s'] is not None else '-'}s | {first} |")
    lines.append("")
    for r in records:
        lines.append(f"## {r['status']} — {r['name']}")
        lines.append("")
        lines.append(f"- scenario file: `{r['file']}`")
        lines.append(f"- artifacts: `{r['dir']}`")
        if r["status"] == "SKIP":
            lines.append(f"- skipped: {r.get('reason', 'n/a')}")
            lines.append("")
            continue
        lines.append(f"- budget: {r['timeout_s']}s (hard cap {r['hard_timeout_s']}s) "
                     f"· offline: {r['offline']} · seeded agent files: {r['seeded_agent_files']}")
        mock = r.get("mock")
        if mock:
            script_note = (f" · script steps: {mock['script_steps']}"
                           if mock.get("script_steps") else "")
            lines.append(f"- mock LLM: `{mock['base_url']}` · providers "
                         f"{mock['provider_ids']} · selected {mock['selected']} "
                         f"(settings {mock['settings_action']}){script_note} "
                         f"· requests: {mock['requests']} "
                         f"· log: `{mock['log']}`")
        if r.get("result"):
            res = r["result"]
            lines.append(f"- exit_code: {res.get('exit_code')} · timed_out: {res.get('timed_out')} "
                         f"· counts: {res.get('counts')}")
        if r.get("first_failure"):
            lines.append(f"- **first failure**: {r['first_failure']}")
        lines.append("")
        if r["checks"]:
            lines.append("| check | result | detail |")
            lines.append("|---|---|---|")
            for c in r["checks"]:
                detail = c["detail"].replace("|", "\\|")
                lines.append(f"| {fmt_check(c)} | {'PASS' if c['ok'] else 'FAIL'} | "
                             f"{detail} |")
            lines.append("")
        if r.get("stdout"):
            lines.append(f"- driver stdout: `{r['stdout']}`")
        if r.get("driver_log"):
            lines.append(f"- driver log: `{r['driver_log']}`")
    with open(report_md, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")
    return report_md, report_json


# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    ap = argparse.ArgumentParser(description="ConPTY TUI battery (assertion-based)")
    ap.add_argument("--pir", help="pir.exe path (overrides PIR_TUI_EXE)")
    ap.add_argument("--scenarios-dir", default=DEFAULT_SCENARIOS_DIR)
    ap.add_argument("--out-root", default=DEFAULT_OUT_ROOT)
    ap.add_argument("--run-id", default=None, help="artifact dir name under out/ (default timestamp)")
    ap.add_argument("--only", default="", help="comma list of glob/substring filters")
    ap.add_argument("--skip", default="", help="comma list of glob/substring filters")
    ap.add_argument("--timeout", type=float, default=None,
                    help=f"--timeout OVERRIDES every scenario's TIMEOUT (default "
                         f"{DEFAULT_TIMEOUT:.0f}s for scenarios that set neither)")
    ap.add_argument("--include-provider", action="store_true",
                    help="deprecated alias of --real-provider")
    ap.add_argument("--real-provider", action="store_true",
                    help="do NOT inject the mock LLM: run seeded scenarios against "
                         "the provider config in --models-template (required) and allow "
                         "NEEDS_REAL_PROVIDER scenarios")
    ap.add_argument("--models-template", default=None,
                    help="models.json used for --real-provider runs. Every "
                         "credential-bearing value (apiKey, headers.*, "
                         "customHeaders.*) must be a WHOLE $ENV/${ENV}/!command "
                         "template or the battery dummy; literal and mixed "
                         "literal+template values are refused. The harness "
                         "never reads ~/.pi/agent or out/tui_agent")
    ap.add_argument("--selftest", action="store_true",
                    help="run the credential-template validator selftest and "
                         "exit (pure functions; no scenarios, TUI or model calls)")
    ap.add_argument("--fail-fast", action="store_true", help="stop after the first FAIL")
    ap.add_argument("--list", action="store_true",
                    help="list scenarios and exit (inventory only: no TUI, no "
                         "model call; scenario modules are imported, which may "
                         "write a scenario-import artifact such as "
                         "out/terminal_input_ext.js)")
    args = ap.parse_args(argv)
    if args.selftest:
        return run_selftest()
    real_provider = args.real_provider or args.include_provider

    try:
        scenarios = discover(args.scenarios_dir)
    except ScenarioError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 2

    only = [p.strip() for p in args.only.split(",") if p.strip()]
    skip = [p.strip() for p in args.skip.split(",") if p.strip()]
    selected, skipped = [], []
    for s in scenarios:
        if only and not matches_any(s.name, only):
            continue
        if skip and matches_any(s.name, skip):
            continue
        selected.append(s)

    if args.list:
        print(f"scenarios in {args.scenarios_dir}:")
        for s in scenarios:
            tags = []
            if s.needs_real_provider:
                tags.append("needs-real-provider")
            if s.seed_session:
                tags.append("seed-session")
            if s.mock_script:
                tags.append(f"mock-script:{len(s.mock_script)}")
            if not s.seed_agent:
                tags.append("no-agent-seed")
            if s.timeout:
                tags.append(f"TIMEOUT={s.timeout:g}")
            print(f"  {s.name:<28} checks={len(s.checks):<2} ops={len(s.ops):<2} "
                  f"{' '.join(tags):<24} {os.path.basename(s.path)}")
        return 0

    # Sanitized-template gate: the harness never sources a provider config
    # from the environment's real agent dir. A real-provider run must name an
    # explicit, validated template; hermetic runs always use the built-in one.
    args.models_template_config = None
    if real_provider:
        if not args.models_template:
            print("ERROR: --real-provider requires --models-template <path> "
                  "(a models.json whose credential-bearing values are whole "
                  "$ENV/${ENV}/!command templates). The harness never reads "
                  "~/.pi/agent or out/tui_agent for credentials.",
                  file=sys.stderr)
            return 2
        try:
            args.models_template_config = load_models_template(args.models_template)
        except ScenarioError as exc:
            print(f"ERROR: {exc}", file=sys.stderr)
            return 2
    elif args.models_template:
        print("ERROR: --models-template is only valid with --real-provider "
              "(hermetic runs synthesize a sanitized template).", file=sys.stderr)
        return 2

    pir = args.pir or os.environ.get("PIR_TUI_EXE") or DEFAULT_PIR
    # Absolute for the same reason as `run_dir` below: the driver spawns the
    # binary with the fixture cwd, so a relative `--pir` would be looked up
    # there and the driver would die before writing its result json.
    pir = os.path.abspath(pir)
    if not os.path.isfile(pir):
        print(f"ERROR: pir.exe not found: {pir}\n"
              f"       (pass --pir or set PIR_TUI_EXE)", file=sys.stderr)
        return 2

    run_id = args.run_id or new_run_id(args.out_root)
    # Absolute: the run dir becomes the child's PIR_AGENT_DIR, and a relative
    # path would be resolved against the child's own cwd (the fixtures), so the
    # seeded models.json/settings.json would silently never be found.
    run_dir = os.path.abspath(os.path.join(args.out_root, run_id))
    os.makedirs(run_dir, exist_ok=True)
    info = pir_info(pir)
    meta = {"run_id": run_id, "pir": info, "scenarios_dir": args.scenarios_dir,
            "started": time.strftime("%Y-%m-%d %H:%M:%S"),
            "python": sys.version.split()[0], "duration_s": 0,
            "real_provider": real_provider,
            "mock": "disabled (--real-provider)" if real_provider else "injected per seeded scenario"}

    print(f"=== TUI battery {run_id} ===")
    print(f"pir: {info['path']} ({info['size']} B, mtime {info['mtime']})")
    print(f"provider: {'REAL (--real-provider; no mock injection)' if real_provider else 'hermetic mock LLM injection'}")
    print(f"run dir: {run_dir}")

    records, t0 = [], time.time()
    for i, s in enumerate(selected, 1):
        if s.needs_clipboard and not clipboard_available():
            rec = {"name": s.name, "file": s.path, "dir": os.path.join(run_dir, s.name),
                   "status": "SKIP", "duration_s": None, "checks": [], "timeout_s": None,
                   "hard_timeout_s": None, "offline": False, "needs_provider": False,
                   "needs_real_provider": False, "mock": None,
                   "seeded_agent_files": [],
                   "reason": "NEEDS_CLIPBOARD scenario: the system clipboard is not "
                             "accessible (locked desktop / stuck clipboard owner)"}
            records.append(rec)
            print(f"[{i}/{len(selected)}] {s.name:<28} SKIP  (clipboard unavailable)")
            continue
        if s.needs_real_provider and not real_provider:
            rec = {"name": s.name, "file": s.path, "dir": os.path.join(run_dir, s.name),
                   "status": "SKIP", "duration_s": None, "checks": [], "timeout_s": None,
                   "hard_timeout_s": None, "offline": False, "needs_provider": True,
                   "needs_real_provider": True, "mock": None,
                   "seeded_agent_files": [],
                   "reason": "NEEDS_REAL_PROVIDER scenario: pass --real-provider to run"}
            records.append(rec)
            print(f"[{i}/{len(selected)}] {s.name:<28} SKIP  (needs real provider)")
            continue
        print(f"[{i}/{len(selected)}] {s.name:<28} ...", end="", flush=True)
        try:
            rec = execute(s, args, run_dir, pir, use_mock=not real_provider)
        except Exception as exc:  # noqa: BLE001 - one bad scenario must not kill the run
            rec = {"name": s.name, "file": s.path, "dir": os.path.join(run_dir, s.name),
                   "status": "FAIL", "duration_s": None, "checks": [],
                   "reason": f"battery error: {type(exc).__name__}: {exc}",
                   "first_failure": f"battery error: {type(exc).__name__}: {exc}",
                   "timeout_s": None, "hard_timeout_s": None, "offline": s.offline,
                   "needs_provider": s.needs_provider,
                   "needs_real_provider": s.needs_real_provider, "mock": None,
                   "seeded_agent_files": []}
        records.append(rec)
        status = rec["status"]
        dur = rec["duration_s"] if rec["duration_s"] is not None else "-"
        print(f" {status:<4}  ({dur}s)")
        if status == "FAIL" and rec.get("first_failure"):
            print(f"        first failing check: {rec['first_failure']}")
        if args.fail_fast and status == "FAIL":
            print("--fail-fast: stopping after first FAIL")
            break

    meta["duration_s"] = round(time.time() - t0, 2)
    meta["finished"] = time.strftime("%Y-%m-%d %H:%M:%S")
    report_md, report_json = write_reports(run_dir, meta, records)

    passed = sum(1 for r in records if r["status"] == "PASS")
    failed = sum(1 for r in records if r["status"] == "FAIL")
    skipt = sum(1 for r in records if r["status"] == "SKIP")
    print("")
    print(f"SUMMARY: {len(records)} scenarios — {passed} PASS, {failed} FAIL, "
          f"{skipt} SKIP (wall {meta['duration_s']}s)")
    for r in records:
        if r["status"] == "FAIL" and r.get("first_failure"):
            print(f"  FAIL {r['name']}: {r['first_failure']}")
    print(f"report: {report_md}")
    print(f"json:   {report_json}")
    if not records:
        print("ERROR: no scenarios selected", file=sys.stderr)
        return 2
    if failed:
        return 1
    ran = sum(1 for r in records if r["status"] != "SKIP")
    if not ran:
        print("ERROR: no scenarios were executed (only SKIPs — provider scenarios "
              "need --include-provider; filters may have excluded everything)",
              file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
