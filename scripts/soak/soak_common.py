#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Shared helpers for the long-run (`months/years`) soak harness.

The three soak drivers in this directory all need the same four primitives:

1. a deterministic, offline OpenAI-compatible provider (so a soak never spends
   API credits and every turn's shape is controlled by the harness);
2. process-tree RSS / child-count sampling (``tasklist`` alone measures only
   the spawned pid, and on Windows the shell tools spawn MSYS children);
3. JSONL session inspection with an explicit torn-tail rule (a ``SIGKILL``
   mid-append is allowed to leave one partial final line; a partial line
   anywhere else is corruption);
4. a line-oriented child runner that can report "no event for N seconds"
   (the per-request hang detector) and kill the whole tree on timeout.

Neither this module nor the drivers import anything outside the Python 3
standard library.  The sampling and kill code is adapted from the existing
drivers:

* ``scripts/perf/big_session_probe.py`` - CIM process table -> process tree
  working set, and the mock ``openai-completions`` SSE shape;
* ``scripts/perf/rss_probe.py`` - stdout/stderr go to files, stdin is
  ``DEVNULL`` (an unread pipe wedges the child);
* ``scripts/tui_conpty/tui_drive.py`` - ``taskkill /T /F`` for the whole tree
  while the root is still verifiably alive, and per-step bounded waits.

The TUI ConPTY path is deliberately not used: the soak drives
``pir --mode json --print`` so the event stream is machine-readable and does
not depend on a terminal renderer.
"""

import http.server
import json
import math
import os
import queue
import re
import signal
import socketserver
import subprocess
import threading
import time
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))

CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)

# Prompt markers the mock classifies requests by. They are plain text, so a
# real provider simply sees them as (harmless) prompt content.
MARKER_PREFIX = "[[soak:"
MARKER_TEXT = "[[soak:text]]"
MARKER_TOOL_BASH = "[[soak:tool:bash]]"
MARKER_TOOL_READ = "[[soak:tool:read]]"

# The compaction summarizer's fixed system prompt
# (src/compaction/prompts.rs, SUMMARIZATION_SYSTEM_PROMPT). A raw substring
# search over the request body is enough to tell a summary call apart from a
# conversational one, and it works even when the body is many megabytes.
SUMMARY_MARKER = b"You are a context summarization assistant"

# The mock response starts with this token, so drivers can prove the final
# assistant message came from the mock and not from a stale session entry.
MOCK_OK = "soak-ok"

# A session append is killed mid-write often enough that a partial LAST line
# is expected after a crash. The reader (session_manager) parses it if it
# happens to be valid JSON and repairs the missing newline otherwise; a bad
# line anywhere before the tail is real corruption. The helpers below make
# that rule explicit rather than treating every unparsable line alike.


# ---------------------------------------------------------------------------
# Paths / binaries
# ---------------------------------------------------------------------------

def candidate_pir_paths():
    """The same defaulting order the repo's probes use, plus a release-first
    preference: an explicit path always wins, then ``PIR_SOAK_EXE``, then the
    conventional build directories."""
    return [
        os.path.join(REPO, "target", "release", "pir.exe"),
        os.path.join(REPO, "target-alt", "release", "pir.exe"),
        os.path.join(REPO, "target", "release", "pir"),
        os.path.join(REPO, "target-alt", "release", "pir"),
        os.path.join(REPO, "target", "debug", "pir.exe"),
        os.path.join(REPO, "target-alt", "debug", "pir.exe"),
        os.path.join(REPO, "target", "debug", "pir"),
        os.path.join(REPO, "target-alt", "debug", "pir"),
    ]


def resolve_pir(explicit=None):
    if explicit:
        return os.path.abspath(explicit)
    env = os.environ.get("PIR_SOAK_EXE")
    if env:
        return os.path.abspath(env)
    for path in candidate_pir_paths():
        if os.path.exists(path):
            return path
    return candidate_pir_paths()[0]


def ensure_dir(path):
    os.makedirs(path, exist_ok=True)
    return path


def resolve_path(path):
    return os.path.abspath(os.path.expanduser(path))


def write_json(path, value):
    ensure_dir(os.path.dirname(os.path.abspath(path)))
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8", newline="\n") as fh:
        json.dump(value, fh, indent=2, sort_keys=False)
        fh.write("\n")
    os.replace(tmp, path)


def append_jsonl(path, value):
    with open(path, "a", encoding="utf-8", newline="\n") as fh:
        fh.write(json.dumps(value, sort_keys=False) + "\n")


# ---------------------------------------------------------------------------
# Process tree sampling
# ---------------------------------------------------------------------------

def _win_process_table():
    """{pid: (ppid, working_set_bytes)} for every process, via one CIM query.

    ``tasklist`` cannot report parent links, and a soak is specifically
    interested in the tool children a hung turn leaves behind, so the tree is
    built from the parent links (the same instrument as
    ``scripts/perf/big_session_probe.py``).
    """
    script = (
        "Get-CimInstance Win32_Process | ForEach-Object { "
        '"$($_.ProcessId) $($_.ParentProcessId) $($_.WorkingSetSize)" }'
    )
    try:
        done = subprocess.run(
            ["powershell", "-NoProfile", "-NonInteractive", "-Command", script],
            capture_output=True, text=True, errors="replace", timeout=30,
            creationflags=CREATE_NO_WINDOW,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    rows = {}
    for line in done.stdout.splitlines():
        parts = line.split()
        if len(parts) == 3 and all(part.isdigit() for part in parts):
            pid, ppid, ws = (int(part) for part in parts)
            rows[pid] = (ppid, ws)
    return rows or None


def _posix_process_table():
    """{pid: (ppid, rss_bytes)} from /proc (Linux). Returns None elsewhere."""
    if not os.path.isdir("/proc"):
        return None
    page = os.sysconf("SC_PAGE_SIZE") if hasattr(os, "sysconf") else 4096
    rows = {}
    try:
        entries = os.listdir("/proc")
    except OSError:
        return None
    for entry in entries:
        if not entry.isdigit():
            continue
        pid = int(entry)
        try:
            with open("/proc/%d/stat" % pid, "r", encoding="utf-8", errors="replace") as fh:
                stat = fh.read()
        except OSError:
            continue
        close = stat.rfind(")")
        if close < 0:
            continue
        fields = stat[close + 2:].split()
        # fields[0] is state (stat field 3), fields[1] is ppid (field 4),
        # fields[21] is rss in pages (field 24).
        if len(fields) < 22:
            continue
        # A killed child can remain a zombie until the runner reaps it. Its
        # resident set is 0; recording that as a sample would dilute the RSS
        # trend (and could hide growth), so zombies are not tree members.
        if fields[0] == "Z":
            continue
        try:
            ppid = int(fields[1])
            rss = int(fields[21]) * page
        except ValueError:
            continue
        if rss <= 0:
            continue
        rows[pid] = (ppid, rss)
    return rows or None


def sample_root_rss_mb(pid):
    """Fast root-process working set (MB), without a CIM query.

    ``tasklist`` is ~50 ms versus ~0.5-1.5 s for CIM, so this is the one
    sample that can still land on a run that exits almost immediately (a
    small session with a fast provider). It measures the root only; the tree
    sampler adds the children.
    """
    if os.name == "nt":
        try:
            done = subprocess.run(
                ["tasklist", "/FI", "PID eq %d" % pid, "/FO", "CSV", "/NH"],
                capture_output=True, text=True, errors="replace", timeout=15,
                creationflags=CREATE_NO_WINDOW,
            )
        except (OSError, subprocess.SubprocessError):
            return None
        match = None
        for line in done.stdout.splitlines():
            match = re.search(r'"([\d.,]+) K"', line)
            if match:
                break
        if not match:
            return None
        try:
            rss_kb = float(match.group(1).replace(",", "").replace(".", ""))
        except ValueError:
            return None
        if rss_kb <= 0:
            return None
        return round(rss_kb / 1024.0, 2)
    try:
        with open("/proc/%d/status" % pid, "r", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                if line.startswith("VmRSS:"):
                    rss_kb = int(line.split()[1])
                    if rss_kb <= 0:
                        return None
                    return round(rss_kb / 1024.0, 2)
    except (OSError, ValueError):
        return None
    return None


def sample_process_tree(root_pid):
    """Total working set (MB) and member count of ``root_pid``'s tree.

    Returns ``None`` when the table cannot be read or the root is already gone;
    callers treat that as "process exited", not as a memory verdict. On POSIX
    a zombie root/member is not a live tree member (its RSS is 0 and counting
    it would dilute the trend); Windows has no zombie state.
    """
    if os.name == "nt":
        rows = _win_process_table()
    else:
        rows = _posix_process_table()
    if rows is None:
        return None
    root_entry = rows.get(root_pid)
    if root_entry is None or root_entry[1] <= 0:
        # Gone, or a POSIX zombie awaiting reap: not a usable memory sample.
        return None
    children = {}
    for pid, (ppid, _rss) in rows.items():
        children.setdefault(ppid, []).append(pid)
    total = 0
    count = 0
    seen = set()
    stack = [root_pid]
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        entry = rows.get(pid)
        if entry is None:
            continue
        total += entry[1]
        count += 1
        stack.extend(children.get(pid, ()))
    return {"rss_mb": round(total / (1024.0 * 1024.0), 2),
            "processes": count, "pids": sorted(seen)}


def alive_pids(pids):
    """Which of ``pids`` are still live processes (zombies count as dead).

    Used after a child exits to catch orphaned tool children: the parent
    links are gone once the root exits, so the driver snapshots the tree's
    member pids while the root is alive and re-checks this set afterwards.
    """
    wanted = {int(pid) for pid in (pids or []) if pid}
    if not wanted:
        return []
    if os.name == "nt":
        rows = _win_process_table()
    else:
        rows = _posix_process_table()
    if rows is None:
        return []
    return sorted(pid for pid in wanted
                  if pid in rows and rows[pid][1] > 0)


def kill_process_tree(pid):
    """Kill ``pid`` and every descendant (best effort), while the root is alive.

    Adapted from ``scripts/tui_conpty/tui_drive.py``: a plain
    ``TerminateProcess`` leaves tool subprocesses behind, and on Windows the
    shell tools spawn MSYS children. ``taskkill /T`` follows the parent links;
    the tree is only meaningful while the root still exists.
    """
    if not pid:
        return False
    if os.name == "nt":
        try:
            done = subprocess.run(
                ["taskkill", "/T", "/F", "/PID", str(pid)],
                capture_output=True, timeout=20, creationflags=CREATE_NO_WINDOW,
            )
            return done.returncode == 0
        except (OSError, subprocess.SubprocessError):
            return False
    try:
        os.killpg(os.getpgid(pid), signal.SIGKILL)
        return True
    except (OSError, ProcessLookupError):
        try:
            os.kill(pid, signal.SIGKILL)
            return True
        except OSError:
            return False


# ---------------------------------------------------------------------------
# JSON / JSONL inspection
# ---------------------------------------------------------------------------

def _parse_json_line(raw, path, line_no):
    text = raw.decode("utf-8", errors="replace").strip()
    if not text:
        return None
    try:
        value = json.loads(text)
    except ValueError:
        return {"__malformed__": True, "line": line_no, "path": path,
                "raw_prefix": text[:160]}
    return value if isinstance(value, dict) else {"__malformed__": True,
                                                  "line": line_no,
                                                  "path": path,
                                                  "raw_prefix": text[:160]}


def inspect_jsonl(path, allow_legacy_header=False):
    """Readability/structure report for one session (or durable) JSONL file.

    Rules:
    * the file must be openable and non-empty;
    * every newline-terminated line must parse as a JSON object;
    * a final line without a newline may be torn by a kill; it is reported as
      ``torn_tail`` and only counts as corruption if the caller forbids it;
    * the first parseable object must be the v3 session header. Callers that
      intentionally inspect an older on-disk format must opt in with
      ``allow_legacy_header=True`` (a missing version or version 1/2); the
      default is strict version 3.
    """
    report = {
        "path": path,
        "exists": os.path.exists(path),
        "bytes": 0,
        "lines": 0,
        "parsed": 0,
        "malformed_complete": [],
        "torn_tail": False,
        "torn_tail_parsed": False,
        "ends_with_newline": False,
        "header": None,
        "header_ok": False,
        "header_version": None,
        "header_version_ok": False,
        "header_version_strict": 3,
    }
    if not report["exists"]:
        return report
    try:
        report["bytes"] = os.path.getsize(path)
        with open(path, "rb") as fh:
            previous = None
            line_no = 0
            while True:
                raw = fh.readline()
                if not raw:
                    break
                line_no += 1
                if previous is not None:
                    report["lines"] += 1
                    value = _parse_json_line(previous, path, line_no - 1)
                    if value is not None and value.get("__malformed__"):
                        report["malformed_complete"].append(value)
                    elif value is not None:
                        report["parsed"] += 1
                        if report["header"] is None:
                            report["header"] = value
                previous = raw
            if previous is not None:
                report["lines"] += 1
                complete = previous.endswith(b"\n")
                report["ends_with_newline"] = complete
                value = _parse_json_line(previous, path, line_no)
                if value is not None and value.get("__malformed__"):
                    if complete:
                        report["malformed_complete"].append(value)
                    else:
                        report["torn_tail"] = True
                elif value is not None:
                    report["parsed"] += 1
                    if not complete:
                        report["torn_tail"] = True
                        report["torn_tail_parsed"] = True
                    if report["header"] is None:
                        report["header"] = value
    except OSError as exc:
        report["read_error"] = str(exc)
        return report
    header = report["header"] or {}
    version = header.get("version")
    report["header_version"] = version
    version_ok = version == 3 or (
        allow_legacy_header and version in (None, 1, 2))
    report["header_version_ok"] = version_ok
    report["header_ok"] = (
        header.get("type") == "session"
        and isinstance(header.get("id"), str)
        and bool(header.get("id"))
        and version_ok
    )
    return report


def header_version_reason(report_or_header, strict=3):
    """Human-readable reason when a session header is not the strict version.

    ``report_or_header`` may be an [`inspect_jsonl`] report or a bare header
    dict (the ``session`` frame emitted on the JSON stdout stream). A missing
    version is reported as ``missing`` rather than silently treated as v3, and
    an unknown/legacy value is named explicitly.
    """
    if isinstance(report_or_header, dict) and "header_version" in report_or_header \
            and "header_ok" in report_or_header:
        header = report_or_header.get("header") or {}
        version = report_or_header.get("header_version")
    else:
        header = report_or_header if isinstance(report_or_header, dict) else {}
        version = header.get("version")
    if header.get("type") != "session":
        return "session header is missing (first parseable line is not type=session)"
    # Strict by definition: legacy opt-in is an inspect_jsonl concern, not a
    # reason this checker should ever accept.
    if version == strict:
        return None
    if version is None:
        return "session header version is missing (require version %d)" % strict
    return "session header version %r is unknown (require version %d)" % (
        version, strict)


def stream_session_header(path):
    """The ``session`` header line a run emits first on its JSON stdout.

    Returns ``None`` when the first stdout line is not a parsed JSON object.
    Callers validate its ``type``/``version``/``id`` (the print-mode contract
    is that the persisted session header is the first line).
    """
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            first_line = fh.readline().strip()
        header = json.loads(first_line) if first_line else None
        return header if isinstance(header, dict) else None
    except (OSError, ValueError):
        return None


def inspect_appended_region(path, start_offset):
    """Every complete line appended at/after ``start_offset`` must parse.

    Opening a session whose last write was killed appends exactly one newline
    (the documented repair). The first byte of the appended region may
    therefore be that newline; it is skipped (and reported as
    ``leading_newline`` so a caller can prove the repair happened). Everything
    after it is freshly written by the recovery run and must be clean.

    Returns ``{"clean", "lines", "parsed", "errors", "bytes",
    "leading_newline", "entry_types", "message_roles", "last_line_parsed"}``.
    ``lines`` counts non-empty lines; ``parsed`` counts those that are JSON
    objects. A caller that needs a real recovery record must require
    ``parsed >= 1`` (a non-empty append of unparsable bytes is not recovery).
    """
    result = {
        "clean": True, "lines": 0, "parsed": 0, "errors": [], "bytes": 0,
        "leading_newline": False, "entry_types": {}, "message_roles": {},
        "last_line_parsed": False,
    }
    try:
        with open(path, "rb") as fh:
            fh.seek(start_offset)
            data = fh.read()
    except OSError as exc:
        result["clean"] = False
        result["errors"].append(str(exc))
        return result
    result["bytes"] = len(data)
    if data.startswith(b"\n"):
        result["leading_newline"] = True
        data = data[1:]
    if data and not data.endswith(b"\n"):
        result["clean"] = False
        result["errors"].append("appended region does not end with a newline")
    for index, raw in enumerate(data.split(b"\n"), 1):
        if not raw.strip():
            continue
        result["lines"] += 1
        result["last_line_parsed"] = False
        try:
            value = json.loads(raw.decode("utf-8", errors="replace"))
        except ValueError:
            result["clean"] = False
            result["errors"].append("line %d is not JSON: %r" % (index, raw[:120]))
            continue
        if not isinstance(value, dict):
            result["clean"] = False
            result["errors"].append("line %d is not a JSON object" % index)
            continue
        result["parsed"] += 1
        result["last_line_parsed"] = True
        entry_type = value.get("type")
        if isinstance(entry_type, str):
            result["entry_types"][entry_type] = \
                result["entry_types"].get(entry_type, 0) + 1
        message = value.get("message")
        if isinstance(message, dict) and isinstance(message.get("role"), str):
            role = message["role"]
            result["message_roles"][role] = \
                result["message_roles"].get(role, 0) + 1
    return result


def scan_jsonl_roots(roots, limit=200):
    """Readability reports for JSONL files under several roots.

    ``roots`` is a list of ``(path, mode)`` where ``mode`` is:

    * ``"session"`` - must carry a valid v3 header and have no malformed
      COMPLETE line (a torn final line is allowed);
    * ``"session_header"`` - header required, malformed-line counting left to
      the caller (the crash driver tracks its own baseline across iterations,
      because each repaired torn line is a known post-crash artifact);
    * ``"session_legacy"`` - like ``"session"`` but the caller explicitly
      accepts a missing/1/2 header version (never the default);
    * ``"durable"`` - no header requirement, but a malformed COMPLETE line is
      corruption (durable journals are rewritten by their own recovery, so a
      torn tail is still tolerated);
    * ``"report"`` - only readability is required (used for sibling session
      files from earlier runs in the same log directory: their repaired torn
      lines are known artifacts, not this run's corruption).

    ``path`` may be a file or a directory (walked, bounded by ``limit``).
    """
    reports = []
    seen = set()
    for root, mode in roots:
        if not root or not os.path.exists(root):
            continue
        candidates = []
        if os.path.isdir(root):
            for current, _dirs, files in os.walk(root):
                for name in files:
                    if name.endswith(".jsonl"):
                        candidates.append(os.path.join(current, name))
        else:
            candidates.append(root)
        for path in sorted(candidates):
            real = os.path.abspath(path)
            if real in seen:
                continue
            seen.add(real)
            report = inspect_jsonl(
                path, allow_legacy_header=(mode == "session_legacy"))
            report["mode"] = mode
            reports.append(report)
            if len(reports) >= limit:
                return reports
    return reports


def scan_jsonl_failures(reports):
    """Reasons from [`scan_jsonl_roots`] reports, per mode."""
    reasons = []
    for report in reports:
        mode = report.get("mode", "session")
        if not report["exists"]:
            reasons.append("missing JSONL file: %s" % report["path"])
            continue
        if "read_error" in report:
            reasons.append("%s: %s" % (report["path"], report["read_error"]))
            continue
        if mode in ("session", "session_header", "session_legacy") \
                and not report["header_ok"]:
            reasons.append("%s: invalid session header" % report["path"])
        if mode in ("session", "session_legacy", "durable") \
                and report["malformed_complete"]:
            reasons.append("%s: %d malformed complete line(s)"
                           % (report["path"], len(report["malformed_complete"])))
    return reasons


# ---------------------------------------------------------------------------
# Statistics
# ---------------------------------------------------------------------------

def linear_slope(xs, ys):
    """Least-squares slope (y units per x unit); 0.0 for <2 points/zero span."""
    if len(xs) < 2 or len(xs) != len(ys):
        return 0.0
    n = float(len(xs))
    mean_x = sum(xs) / n
    mean_y = sum(ys) / n
    denom = sum((x - mean_x) ** 2 for x in xs)
    if denom == 0:
        return 0.0
    return sum((x - mean_x) * (y - mean_y) for x, y in zip(xs, ys)) / denom


def median(values):
    if not values:
        return None
    ordered = sorted(values)
    mid = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[mid]
    return (ordered[mid - 1] + ordered[mid]) / 2.0


# The mock (and the real provider's usage report) track context in tokens; the
# session JSONL stores the same textual content plus bookkeeping. Four
# characters per token is the same approximation the mock uses
# (``prompt_tokens = body_bytes // 4``), so a measured byte delta can be
# compared directly with the configured context window.
BYTES_PER_TOKEN = 4.0


def measured_growth_tokens(bytes_before, bytes_after, bytes_per_token=BYTES_PER_TOKEN):
    """Context tokens appended to the session, measured from file growth."""
    before = int(bytes_before or 0)
    after = int(bytes_after or 0)
    return max(0.0, (after - before) / float(bytes_per_token))


def compactions_for_growth(growth_tokens, context_window, reserve_tokens):
    """Compactions implied by measured context growth (>=1 per window)."""
    trigger = max(1, int(context_window) - int(reserve_tokens))
    if growth_tokens < trigger:
        return 0
    return int(growth_tokens // trigger)


def _rss_trend_metrics(samples, warmup_x, slope_limit_mb_per_turn,
                       growth_limit_mb, max_mb, first_fraction,
                       min_trend_samples=3):
    """Trend/peak metrics for one already-selected sample scope.

    ``warmup_x`` is an exclusive lower bound: samples with x <= warmup_x are
    excluded. With a 0-based sample x (headless) that drops the baseline and
    the first ``warmup_x`` completed turns; with a 1-based x (reopen index) it
    drops the first ``warmup_x`` reopens.

    ``min_trend_samples`` (default 3) is the minimum number of post-warmup
    points a least-squares slope may be computed from: two points are just
    "first vs last", which is exactly the fragile criterion this replaces.
    Fewer points are reported as ``insufficientTrendSamples`` and fail.
    """
    if not samples:
        return {}
    # The first/last-quarter medians assume chronological order. Root and
    # tree samples are appended from different threads/batches, so sort by x.
    ordered = sorted(samples, key=lambda s: float(s.get("x", 0)))
    ys = [float(s["rss_mb"]) for s in ordered]
    peak = max(ys)
    metrics = {
        "samples": len(ordered),
        "peakRssMb": round(peak, 2),
        "peakExceedsMax": peak > max_mb,
    }
    trend = [s for s in ordered if float(s.get("x", 0)) > warmup_x]
    metrics["trendSamples"] = len(trend)
    metrics["minTrendSamples"] = min_trend_samples
    metrics["insufficientTrendSamples"] = len(trend) < min_trend_samples
    if len(trend) < min_trend_samples:
        # Do not silently fall back to the full series: that reintroduces the
        # startup ramp the caller asked to exclude and hides a short run.
        metrics["slopeMbPerTurn"] = 0.0
        metrics["slopeExceedsLimit"] = False
        metrics["growthMb"] = None
        metrics["growthExceedsLimit"] = False
        return metrics
    xs = [float(s["x"]) for s in trend]
    trend_ys = [float(s["rss_mb"]) for s in trend]
    slope = linear_slope(xs, trend_ys)
    metrics["slopeMbPerTurn"] = round(slope, 4)
    metrics["slopeExceedsLimit"] = slope > slope_limit_mb_per_turn
    if len(trend_ys) >= 4:
        count = max(1, int(math.ceil(len(trend_ys) * first_fraction)))
        first = median(trend_ys[:count])
        last = median(trend_ys[-count:])
        growth = last - first
        metrics["firstMedianMb"] = round(first, 2)
        metrics["lastMedianMb"] = round(last, 2)
        metrics["growthMb"] = round(growth, 2)
        metrics["growthExceedsLimit"] = growth > growth_limit_mb
    else:
        metrics["growthMb"] = None
        metrics["growthExceedsLimit"] = False
    return metrics


def rss_verdict(samples, slope_limit_mb_per_turn, growth_limit_mb, max_mb,
                first_fraction=0.25, warmup_x=0, prefer_scope="tree",
                min_tree_samples=4):
    """Apply the standard memory criteria to ``[{"x":n,"rss_mb":m}, ...]``.

    Returns ``(ok, reasons, metrics)``. The criteria are deliberately separate:

    * absolute ceiling: the verdict scope's peak may not exceed ``max_mb``;
    * slope: least-squares MB per x unit over the sample window (post warm-up)
      must stay at/below the limit;
    * steady-state growth: median of the last quarter minus the median of the
      first quarter must stay within ``growth_limit_mb`` (catches a leak whose
      slope is diluted by early/late noise; compaction should reset it).

    ``warmup_x`` excludes the startup ramp from the slope/growth checks: a
    first sample taken while the binary is still loading is 4-20x below its
    steady state, and a linear fit through it would report a huge slope for a
    short run that is actually flat.

    Scopes are never silently discarded. Tree samples are the default verdict
    basis (they include tool children); a tree verdict requires at least
    ``min_tree_samples`` (default 4) such samples and at least one process-count
    reading, and fails loudly when either is absent. Root-only samples (the
    fast ``tasklist`` points) are always reported separately under
    ``scopeMetrics.root`` and are additionally merged with the other scopes
    under ``scopeMetrics.merged``; if tree sampling produced nothing, the
    merged trend is still computed for diagnosis, but the verdict fails for
    the missing tree basis. ``metrics["criterion"]`` records the exact rule
    used so a summary reader never has to infer it.
    """
    reasons = []
    usable = [s for s in samples if s.get("rss_mb") is not None]
    if not usable:
        return False, ["no RSS samples collected"], {
            "samples": 0, "treeSamples": 0, "rootSamples": 0,
            "processSamples": 0, "criterion": "no samples",
        }

    def scoped(scope):
        return [s for s in usable if (s.get("scope") or "unknown") == scope]

    tree = scoped("tree")
    root = scoped("root")
    unknown = scoped("unknown")
    process_samples = sum(1 for s in usable if s.get("processes") is not None)
    tree_process_samples = sum(
        1 for s in tree if s.get("processes") is not None)
    metrics = {
        "scopes": sorted({(s.get("scope") or "unknown") for s in usable}),
        "samples": len(usable),
        "treeSamples": len(tree),
        "rootSamples": len(root),
        "unknownSamples": len(unknown),
        "processSamples": process_samples,
        "treeProcessSamples": tree_process_samples,
        "minTreeSamples": min_tree_samples,
    }

    tree_too_few = False
    if prefer_scope == "tree":
        if len(tree) < min_tree_samples:
            tree_too_few = True
            reasons.append(
                "insufficient tree RSS samples: %d (require >= %d; root=%d, "
                "unknown=%d)" % (len(tree), min_tree_samples, len(root), len(unknown)))
        primary = tree if tree else usable
        metrics["verdictScope"] = "tree" if tree else "merged-fallback"
        # A tree verdict is the only scope that needs process-tree readings:
        # the tree sampler is the sole orphan-leak coverage. Root/merged
        # callers may legitimately have no tree samples at all.
        if tree_process_samples == 0:
            reasons.append(
                "no process samples collected (process-tree sampling failed)")
        metrics["treeSamplesWithoutProcess"] = len(tree) - tree_process_samples
    elif prefer_scope:
        preferred = scoped(prefer_scope)
        if not preferred:
            reasons.append("no %s RSS samples collected" % prefer_scope)
        primary = preferred if preferred else usable
        metrics["verdictScope"] = prefer_scope if preferred else "merged-fallback"
    else:
        primary = usable
        metrics["verdictScope"] = "merged"

    primary_metrics = _rss_trend_metrics(
        primary, warmup_x, slope_limit_mb_per_turn, growth_limit_mb,
        max_mb, first_fraction)
    metrics["verdictSamples"] = primary_metrics.get("samples", 0)
    metrics.update(primary_metrics)
    metrics["samples"] = len(usable)
    metrics["totalSamples"] = len(usable)
    metrics["warmupX"] = warmup_x
    if (metrics.get("insufficientTrendSamples")
            and not tree_too_few):
        reasons.append(
            "insufficient post-warmup RSS samples: %d (require >= %d; the "
            "least-squares slope needs a real window, not two points)"
            % (metrics.get("trendSamples", 0),
               metrics.get("minTrendSamples", 3)))

    # Report every scope (and the merged trend) instead of dropping root
    # points when a tree sample exists.
    scope_metrics = {}
    for name, subset in (("tree", tree), ("root", root), ("unknown", unknown)):
        if subset:
            scope_metrics[name] = _rss_trend_metrics(
                subset, warmup_x, slope_limit_mb_per_turn, growth_limit_mb,
                max_mb, first_fraction)
    scope_metrics["merged"] = _rss_trend_metrics(
        usable, warmup_x, slope_limit_mb_per_turn, growth_limit_mb,
        max_mb, first_fraction)
    metrics["scopeMetrics"] = scope_metrics
    if tree:
        metrics["treePeakRssMb"] = scope_metrics["tree"]["peakRssMb"]
    if root:
        metrics["rootPeakRssMb"] = scope_metrics["root"]["peakRssMb"]

    if metrics.get("peakExceedsMax"):
        reasons.append("peak RSS %.1f MB (%s) exceeds --rss-max-mb %.1f"
                       % (metrics["peakRssMb"], metrics["verdictScope"], max_mb))
    # Root points are not the verdict basis, but an out-of-ceiling root peak
    # must still be visible instead of being silently ignored. Skip it when
    # the primary trend is already the merged fallback (it includes root).
    if metrics["verdictScope"] not in ("merged", "merged-fallback"):
        for name in ("root", "unknown"):
            if name == prefer_scope:
                # Already checked as the primary scope.
                continue
            scoped_metrics = scope_metrics.get(name)
            if scoped_metrics and scoped_metrics.get("peakExceedsMax"):
                reasons.append(
                    "peak RSS %.1f MB (%s samples) exceeds --rss-max-mb %.1f"
                    % (scoped_metrics["peakRssMb"], name, max_mb))
    if metrics.get("slopeExceedsLimit"):
        reasons.append(
            "RSS slope %.4f MB/turn exceeds --rss-slope-mb %.4f"
            % (metrics["slopeMbPerTurn"], slope_limit_mb_per_turn)
        )
    if metrics.get("growthExceedsLimit"):
        reasons.append(
            "RSS grew %.1f MB from the first to the last quarter "
            "(limit %.1f)" % (metrics["growthMb"], growth_limit_mb)
        )
    metrics["criterion"] = (
        "basis=%s samples; absolute peak <= %.1f MB; least-squares slope "
        "over post-warmup(x>%s) samples <= %.4f MB/unit; last-quarter minus "
        "first-quarter median <= %.1f MB; require >=%d tree samples with "
        "process counts"
        % (metrics.get("verdictScope"), max_mb, warmup_x,
           slope_limit_mb_per_turn, growth_limit_mb, min_tree_samples))
    metrics["limits"] = {
        "rssMaxMb": max_mb,
        "rssSlopeMbPerUnit": slope_limit_mb_per_turn,
        "rssGrowthMb": growth_limit_mb,
        "warmupX": warmup_x,
        "minTreeSamples": min_tree_samples,
    }
    metrics["verdict"] = "PASS" if not reasons else "FAIL"
    return (not reasons), reasons, metrics


def process_count_verdict(samples, max_processes, growth_limit,
                          slope_limit=None, min_samples=4,
                          first_fraction=0.25, warmup_x=0):
    """Verdict for ``[{"x":n,"processes":c}, ...]`` process-tree sizes.

    Criteria (all reported individually under ``metrics["criteria"]``):

    * absolute: the maximum member count (over EVERY sample) may not exceed
      ``--max-processes``;
    * slope: the least-squares slope over the post-warmup sample window times
      the window span (the fitted rise) may not exceed ``--process-growth``.
      This is the criterion that matters for orphan leaks: it uses every
      post-warmup sample, not the first and last point (a single startup spike
      or a late dip cannot hide a monotonic climb);
    * steady state: last-quarter median minus first-quarter median may not
      exceed ``--process-growth``.

    ``warmup_x`` is an exclusive lower bound applied to the slope and
    quarterly-median criteria (the startup ramp is not a leak); the absolute
    ceiling still covers the warm-up. At least ``min_samples`` (default 4)
    post-warmup readings are required; missing process samples fail instead of
    silently passing (that sampler is the only orphan-leak coverage).
    """
    usable = [s for s in samples if s.get("processes") is not None]
    reasons = []
    metrics = {
        "samples": len(usable),
        "minSamples": min_samples,
        "warmupX": warmup_x,
        "criterion": ("absolute max over all samples <= limit; least-squares "
                      "slope (fitted rise over the post-warmup window) <= "
                      "growth limit; quarterly-median growth <= growth limit"),
    }
    if len(usable) < min_samples:
        reasons.append(
            "insufficient process samples: %d (require >= %d)"
            % (len(usable), min_samples))
        metrics["verdict"] = "FAIL"
        metrics["limits"] = {"maxProcesses": max_processes,
                             "processGrowth": growth_limit,
                             "processSlope": slope_limit,
                             "minSamples": min_samples,
                             "warmupX": warmup_x}
        return False, reasons, metrics
    usable.sort(key=lambda s: float(s.get("x", 0)))
    all_counts = [int(s["processes"]) for s in usable]
    metrics["maxProcesses"] = max(all_counts)
    metrics["minProcesses"] = min(all_counts)
    criteria = {}
    if max(all_counts) > max_processes:
        reasons.append("process count %d exceeds --max-processes %d"
                       % (max(all_counts), max_processes))
    criteria["absoluteMax"] = {
        "measured": max(all_counts), "limit": max_processes,
        "ok": max(all_counts) <= max_processes,
    }
    trend = [s for s in usable if float(s.get("x", 0)) > warmup_x]
    metrics["trendSamples"] = len(trend)
    if len(trend) < min_samples:
        reasons.append(
            "insufficient post-warmup process samples: %d (require >= %d)"
            % (len(trend), min_samples))
        criteria["trend"] = {"measured": len(trend), "limit": min_samples,
                             "ok": False}
        metrics["criteria"] = criteria
        metrics["limits"] = {"maxProcesses": max_processes,
                             "processGrowth": growth_limit,
                             "processSlope": slope_limit,
                             "minSamples": min_samples,
                             "warmupX": warmup_x}
        metrics["verdict"] = "FAIL"
        return False, reasons, metrics
    xs = [float(s.get("x", 0)) for s in trend]
    counts = [int(s["processes"]) for s in trend]
    metrics["firstProcesses"] = all_counts[0]
    metrics["lastProcesses"] = all_counts[-1]
    # Diagnostics only: max - first is not a verdict basis (a startup spike
    # or a late sample can hide a climb), but it is useful in the summary.
    metrics["processGrowth"] = max(all_counts) - all_counts[0]
    span = xs[-1] - xs[0]
    slope = linear_slope(xs, counts)
    fitted_rise = slope * span
    metrics["windowSpan"] = round(span, 3)
    metrics["slopeProcessesPerUnit"] = round(slope, 4)
    metrics["slopeRiseOverWindow"] = round(fitted_rise, 2)
    slope_ok = fitted_rise <= growth_limit
    criteria["slopeRise"] = {
        "measured": round(fitted_rise, 2), "limit": growth_limit,
        "slopePerUnit": round(slope, 4), "span": round(span, 3),
        "ok": slope_ok,
    }
    if not slope_ok:
        reasons.append(
            "process count slope %.4f/unit (fitted rise %.1f over the window) "
            "exceeds --process-growth %d"
            % (slope, fitted_rise, growth_limit))
    if slope_limit is not None and slope > slope_limit:
        criteria["slopePerUnit"] = {
            "measured": round(slope, 4), "limit": slope_limit,
            "ok": slope <= slope_limit,
        }
        reasons.append("process count slope %.4f/unit exceeds --process-slope %.4f"
                       % (slope, slope_limit))
    if len(counts) >= 4:
        count = max(1, int(math.ceil(len(counts) * first_fraction)))
        first = median(counts[:count])
        last = median(counts[-count:])
        quarter_growth = last - first
        metrics["firstQuarterMedian"] = first
        metrics["lastQuarterMedian"] = last
        metrics["quarterGrowth"] = quarter_growth
        quarter_ok = quarter_growth <= growth_limit
        criteria["quarterGrowth"] = {
            "measured": quarter_growth, "limit": growth_limit,
            "ok": quarter_ok,
        }
        if not quarter_ok:
            reasons.append(
                "process count grew %.1f from the first to the last quarter "
                "(limit %d), possible orphan leak" % (quarter_growth, growth_limit))
    else:
        metrics["quarterGrowth"] = None
    metrics["criteria"] = criteria
    metrics["limits"] = {"maxProcesses": max_processes,
                         "processGrowth": growth_limit,
                         "processSlope": slope_limit,
                         "minSamples": min_samples,
                         "warmupX": warmup_x}
    metrics["verdict"] = "PASS" if not reasons else "FAIL"
    return (not reasons), reasons, metrics


# ---------------------------------------------------------------------------
# Child runner: JSON line stream + hang detection
# ---------------------------------------------------------------------------

class StreamTimeout(Exception):
    """No event arrived within the per-request timeout."""


class StreamClosed(Exception):
    """The child closed stdout (process exited or is exiting)."""


class LineProcess:
    """``pir --mode json`` child with a line queue and a per-request timeout.

    stdout is tee'd to ``stdout_path`` (so a killed run still leaves the
    events it produced); stderr goes straight to a file for the same reason.
    stdin is ``DEVNULL``: an unread pipe wedges the child (rss_probe's note).
    """

    def __init__(self, cmd, cwd, env, stdout_path, stderr_path,
                 start_new_session=True):
        self.cmd = list(cmd)
        self.stdout_path = stdout_path
        self.stderr_path = stderr_path
        ensure_dir(os.path.dirname(os.path.abspath(stdout_path)))
        ensure_dir(os.path.dirname(os.path.abspath(stderr_path)))
        self.stdout_fh = open(stdout_path, "wb")
        self.stderr_fh = open(stderr_path, "wb")
        self.queue = queue.Queue()
        self._closed = threading.Event()
        kwargs = {}
        if os.name != "nt" and start_new_session:
            kwargs["start_new_session"] = True
        try:
            self.proc = subprocess.Popen(
                self.cmd,
                cwd=cwd,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=self.stderr_fh,
                **kwargs
            )
        except OSError:
            self.stdout_fh.close()
            self.stderr_fh.close()
            raise
        self.reader = threading.Thread(target=self._read_loop, daemon=True)
        self.reader.start()

    def _read_loop(self):
        try:
            while True:
                raw = self.proc.stdout.readline()
                if not raw:
                    break
                self.stdout_fh.write(raw)
                self.stdout_fh.flush()
                text = raw.decode("utf-8", errors="replace").rstrip("\r\n")
                if text:
                    self.queue.put(text)
        except (OSError, ValueError):
            pass
        finally:
            self._closed.set()
            self.queue.put(None)

    def next_line(self, timeout):
        """Wait for one non-empty stdout line; raises StreamTimeout/Closed."""
        try:
            item = self.queue.get(timeout=timeout)
        except queue.Empty:
            raise StreamTimeout()
        if item is None:
            raise StreamClosed()
        return item

    def drain(self, timeout=0.2):
        """Collect already-buffered lines (used after a kill)."""
        lines = []
        while True:
            try:
                item = self.queue.get(timeout=timeout)
            except queue.Empty:
                return lines
            if item is None:
                return lines
            lines.append(item)

    def poll(self):
        return self.proc.poll()

    def wait(self, timeout=60):
        try:
            return self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            return None

    def kill(self):
        pid = self.proc.pid
        kill_process_tree(pid)
        try:
            self.proc.kill()
        except OSError:
            pass

    def close(self):
        try:
            if self.proc.poll() is None:
                self.kill()
        finally:
            try:
                self.proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                pass
            try:
                if self.proc.stdout:
                    self.proc.stdout.close()
            except (OSError, ValueError):
                pass
            self.stdout_fh.close()
            self.stderr_fh.close()

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        self.close()


def parse_event(line):
    try:
        value = json.loads(line)
        return value if isinstance(value, dict) else None
    except ValueError:
        return None


def compaction_succeeded(event):
    """Whether a ``compaction_end`` frame is a real successful compaction.

    Upstream resolves the result with ``undefined`` for every non-success
    (abort, error, retry); a frame may therefore carry no ``errorMessage``
    yet still be a cancellation. Success requires all of:
    ``result`` is a JSON object (the wire ``CompactionResult``), ``aborted`` is
    false, ``willRetry`` is false, and ``errorMessage`` is absent/falsy.
    """
    result = event.get("result")
    return (
        isinstance(result, dict)
        and not event.get("aborted")
        and not event.get("willRetry")
        and not event.get("errorMessage")
    )


class TreeSampler(threading.Thread):
    """Background process-tree sampler for short-lived runs.

    One CIM query on Windows costs a few hundred milliseconds, so a
    foreground 50 ms sampler would sample once and then distort the run.
    A daemon thread does the expensive query while the driver keeps consuming
    stdout; ``stop()`` joins it. ``samples`` is a list of
    ``{"at_s": ..., "rss_mb": ..., "processes": ...}``.
    """

    def __init__(self, pid, interval_s=0.05, x_fn=None):
        super().__init__(daemon=True)
        self.pid = pid
        self.interval_s = max(0.0, interval_s)
        # Optional callable returning the current turn index. It is sampled at
        # the same time as the memory reading, so a long CIM query that
        # finishes while the child raced ahead still labels its point with
        # the turn the child was actually on.
        self.x_fn = x_fn
        self.samples = []
        # NOT `self._stop`: threading.Thread uses `_stop()` internally and
        # join() would call the Event. Keep the private name distinct.
        self._stop_event = threading.Event()
        self._started_at = None
        # Monotonic clock at thread start, so a driver can compare an event
        # timestamp (e.g. agent_end) with the sampler's relative `at_s`.
        self.started_monotonic = None
        # Every pid ever observed as a tree member. Once the root exits the
        # parent links are gone, so this set is how a driver checks for
        # orphaned tool children.
        self.seen_pids = set()

    def run(self):
        self._started_at = time.monotonic()
        self.started_monotonic = self._started_at
        while not self._stop_event.is_set():
            # Read x BEFORE the memory query: the query can take a second
            # while the child advances, and a point must never claim a later
            # turn than the memory it measured (that would hide growth).
            x = None
            if self.x_fn is not None:
                try:
                    x = int(self.x_fn())
                except (TypeError, ValueError):
                    x = None
            sample_started_at = time.monotonic() - self._started_at
            sample = sample_process_tree(self.pid)
            if sample is not None:
                record = {
                    # at_s is when the query started; sampled_at_s is when the
                    # reading completed. Post-agent_end evidence must use the
                    # latter, or a query that spans agent_end is under-counted.
                    "at_s": round(sample_started_at, 3),
                    "sampled_at_s": round(
                        time.monotonic() - self._started_at, 3),
                    "rss_mb": sample["rss_mb"],
                    "processes": sample["processes"],
                    "scope": "tree",
                }
                if x is not None:
                    record["x"] = x
                    record["turn"] = x
                self.samples.append(record)
                for pid in sample.get("pids") or ():
                    self.seen_pids.add(pid)
            # The query itself is the throttle on Windows; the sleep only
            # matters on /proc, where a query is cheap.
            if self.interval_s:
                self._stop_event.wait(self.interval_s)

    def stop(self):
        self._stop_event.set()
        self.join(timeout=40)
        return self.samples

    @property
    def peak_rss_mb(self):
        return max((s["rss_mb"] for s in self.samples), default=None)

    @property
    def max_processes(self):
        return max((s["processes"] for s in self.samples), default=None)


# ---------------------------------------------------------------------------
# Mock provider (OpenAI-compatible SSE)
# ---------------------------------------------------------------------------

class MockState:
    """Thread-safe counters shared by the mock's request handlers."""

    def __init__(self):
        self.lock = threading.Lock()
        self.requests = 0
        self.by_kind = {}
        self.bytes_total = 0
        self.max_bytes = 0
        self.first_bytes = None
        self.last_request_at = None
        self.first_request_at = None

    def record(self, size, kind):
        with self.lock:
            self.requests += 1
            self.by_kind[kind] = self.by_kind.get(kind, 0) + 1
            self.bytes_total += size
            self.max_bytes = max(self.max_bytes, size)
            if self.first_bytes is None:
                self.first_bytes = size
            now = time.time()
            if self.first_request_at is None:
                self.first_request_at = now
            self.last_request_at = now

    def snapshot(self):
        with self.lock:
            return {
                "requests": self.requests,
                "byKind": dict(self.by_kind),
                "bytesTotal": self.bytes_total,
                "maxRequestBytes": self.max_bytes,
                "firstRequestBytes": self.first_bytes,
                "requestGrowthBytes": (
                    self.max_bytes - self.first_bytes
                    if self.first_bytes is not None else None),
            }

def _sse(payload):
    return ("data: " + json.dumps(payload, separators=(",", ":")) + "\n\n").encode("utf-8")


def _chunk(delta, finish=None, usage=None, model="soak-model"):
    payload = {
        "id": "chatcmpl-soak",
        "object": "chat.completion.chunk",
        "created": int(time.time()),
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    }
    if usage is not None:
        payload["usage"] = usage
    return _sse(payload)


def _classify_request(body):
    """Return ``summary|followup|tool_bash|tool_read|text`` from the raw body.

    Ordering matters: a summary request replays the conversation (including
    old markers), so it is recognized first. A tool follow-up has the
    tool-result message AFTER the newest user prompt marker (``rfind`` picks
    the newest occurrence in the transcript).
    """
    if SUMMARY_MARKER in body:
        return "summary"
    marker_pos = body.rfind(MARKER_PREFIX.encode("ascii"))
    tool_variants = (b'"role":"tool"', b'"role": "tool"')
    tool_pos = max(body.rfind(variant) for variant in tool_variants)
    if tool_pos > marker_pos:
        return "followup"
    if marker_pos >= 0:
        head = body[marker_pos:marker_pos + 64]
        if b"tool:read" in head:
            return "tool_read"
        if b"tool:bash" in head:
            return "tool_bash"
    return "text"


class _MockHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def log_message(self, _fmt, *_args):
        pass

    def do_GET(self):
        body = json.dumps({
            "object": "list",
            "data": [{"id": self.server.soak_model, "object": "model"}],
        })
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body.encode("utf-8"))

    def do_POST(self):
        server = self.server
        length = int(self.headers.get("Content-Length") or 0)
        body = bytearray()
        remaining = length
        try:
            while remaining > 0:
                chunk = self.rfile.read(min(1 << 20, remaining))
                if not chunk:
                    break
                body.extend(chunk)
                remaining -= len(chunk)
        except (ConnectionResetError, OSError):
            return
        kind = _classify_request(bytes(body))
        server.soak_state.record(len(body), kind)
        if server.soak_delay_ms:
            time.sleep(server.soak_delay_ms / 1000.0)
        # Usage approximates the context the provider saw. pir's threshold
        # compaction uses the last assistant usage's total tokens, so this
        # makes context growth (and compaction) emerge from the real request
        # size instead of a synthetic counter.
        prompt_tokens = max(1, len(body) // 4)
        try:
            if kind in ("tool_bash", "tool_read"):
                self._send_tool_call(kind, prompt_tokens)
            elif kind == "summary":
                self._send_text(server.soak_summary, prompt_tokens)
            else:
                # followup and plain text turns both end in a text answer.
                self._send_text(server.soak_response, prompt_tokens)
        except (BrokenPipeError, ConnectionResetError, OSError):
            return

    def _send_text(self, text, prompt_tokens):
        server = self.server
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        self.wfile.write(_chunk({"role": "assistant", "content": ""},
                                model=server.soak_model))
        for start in range(0, len(text), server.soak_chunk_chars):
            if server.soak_chunk_delay_ms:
                time.sleep(server.soak_chunk_delay_ms / 1000.0)
            self.wfile.write(_chunk(
                {"content": text[start:start + server.soak_chunk_chars]},
                model=server.soak_model,
            ))
        completion = max(1, len(text) // 4)
        self.wfile.write(_chunk(
            {},
            finish="stop",
            usage={
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion,
                "total_tokens": prompt_tokens + completion,
            },
            model=server.soak_model,
        ))
        self.wfile.write(b"data: [DONE]\n\n")

    def _send_tool_call(self, kind, prompt_tokens):
        server = self.server
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        if kind == "tool_read":
            name = "read"
            arguments = json.dumps({
                "path": server.soak_fixture_name,
                "limit": server.soak_tool_lines,
            })
        else:
            name = "bash"
            lines = []
            for index in range(server.soak_tool_lines):
                lines.append(
                    "soak bash line %04d %s"
                    % (index, "lorem ipsum dolor sit amet consectetur adipiscing elit")
                )
            command = "printf '%s\\n' " + " ".join("'%s'" % line for line in lines)
            arguments = json.dumps({"command": command})
        call_id = "call_soak_%d" % int(time.time() * 1000)
        # Same frame shape as scripts/tui_conpty/mock_llm.py: one role-only
        # chunk, then the id/name chunk with the first argument fragment, then
        # a fragment-only chunk (the adapter appends fragments by `index`).
        split = len(arguments) // 2
        self.wfile.write(_chunk({"role": "assistant", "content": ""},
                                model=server.soak_model))
        self.wfile.write(_chunk({
            "tool_calls": [{
                "index": 0,
                "id": call_id,
                "type": "function",
                "function": {"name": name, "arguments": arguments[:split]},
            }],
        }, model=server.soak_model))
        if split < len(arguments):
            self.wfile.write(_chunk({
                "tool_calls": [{
                    "index": 0,
                    "function": {"arguments": arguments[split:]},
                }],
            }, model=server.soak_model))
        completion = max(1, len(arguments) // 4)
        self.wfile.write(_chunk(
            {},
            finish="tool_calls",
            usage={
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion,
                "total_tokens": prompt_tokens + completion,
            },
            model=server.soak_model,
        ))
        self.wfile.write(b"data: [DONE]\n\n")


class _MockServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def _repeat_filler(prefix, target_chars, unit):
    if target_chars <= len(prefix):
        return prefix
    remaining = target_chars - len(prefix)
    copies = (remaining // max(1, len(unit))) + 1
    return prefix + (unit * copies)[:remaining]


class MockProvider:
    """Local OpenAI-compatible provider used by every soak driver by default.

    The response shape is the one ``src/ai/api/openai/sse.rs`` parses:
    ``chat.completion.chunk`` frames, a final ``finish_reason``, a usage chunk
    and ``data: [DONE]``. The same shape was validated in
    ``scripts/perf/big_session_probe.py`` against a real ``pir`` binary.
    """

    def __init__(self, model_id="soak-model", provider="mock",
                 response_chars=4000, summary_chars=800,
                 tool_lines=25, fixture_name="soak_fixture.txt",
                 delay_ms=0, chunk_delay_ms=0, chunk_chars=2048):
        self.provider = provider
        self.model_id = model_id
        self.response_chars = response_chars
        self.summary_chars = summary_chars
        self.tool_lines = tool_lines
        self.fixture_name = fixture_name
        self.delay_ms = delay_ms
        self.chunk_delay_ms = chunk_delay_ms
        self.chunk_chars = max(64, chunk_chars)
        self.state = MockState()
        self._server = None
        self._thread = None

    def start(self):
        unit = "lorem ipsum dolor sit amet consectetur adipiscing elit. "
        self._response_text = _repeat_filler(
            "%s response: " % MOCK_OK, self.response_chars, unit
        )
        summary_heads = (
            "## Goal\n%s summary: keep the soak running.\n"
            "## Progress\n- completed turns recorded\n"
            "## Key Decisions\n- use the offline mock provider\n"
            "## Next Steps\n- continue the soak\n" % MOCK_OK
        )
        self._summary_text = _repeat_filler(summary_heads, self.summary_chars, unit)
        server = _MockServer(("127.0.0.1", 0), _MockHandler)
        server.soak_state = self.state
        server.soak_model = self.model_id
        server.soak_delay_ms = self.delay_ms
        server.soak_chunk_delay_ms = self.chunk_delay_ms
        server.soak_chunk_chars = self.chunk_chars
        server.soak_response = self._response_text
        server.soak_summary = self._summary_text
        server.soak_tool_lines = self.tool_lines
        server.soak_fixture_name = self.fixture_name
        self._server = server
        self._thread = threading.Thread(target=server.serve_forever, daemon=True)
        self._thread.start()
        return self

    @property
    def port(self):
        return self._server.server_address[1]

    def set_delay(self, delay_ms):
        """Change the per-request delay for subsequent requests.

        The crash driver uses this to make only the crashed turn slow: the
        recovery turn must run at full speed.
        """
        if self._server is not None:
            self._server.soak_delay_ms = max(0, int(delay_ms))

    def stop(self):
        if self._server is not None:
            try:
                self._server.shutdown()
            except Exception:
                pass
            try:
                self._server.server_close()
            except Exception:
                pass
            self._server = None

    def __enter__(self):
        return self.start()

    def __exit__(self, *_exc):
        self.stop()


def provision_mock_agent(agent_dir, mock, context_window,
                         reserve_tokens=8192, keep_recent_tokens=12288,
                         extra_settings=None):
    """Write/merge ``models.json`` + ``settings.json`` for the mock provider.

    Existing files are merged (only the ``mock`` provider and the soak's
    settings keys are replaced), so pointing ``--agent-dir`` at a real agent
    directory does not discard unrelated providers. Mock mode still owns those
    two files; use a real model + ``--no-mock`` for an untouched directory.
    """
    ensure_dir(agent_dir)
    models_path = os.path.join(agent_dir, "models.json")
    settings_path = os.path.join(agent_dir, "settings.json")

    models = {}
    if os.path.exists(models_path):
        try:
            with open(models_path, "r", encoding="utf-8") as fh:
                models = json.load(fh)
        except (OSError, ValueError):
            models = {}
    if not isinstance(models, dict):
        models = {}
    providers = models.setdefault("providers", {})
    if not isinstance(providers, dict):
        providers = {}
        models["providers"] = providers
    providers[mock.provider] = {
        "name": "Soak Mock",
        "baseUrl": "http://127.0.0.1:%d/v1" % mock.port,
        "api": "openai-completions",
        "apiKey": "sk-soak-mock",
        "models": [{
            "id": mock.model_id,
            "name": "Soak Mock",
            "reasoning": False,
            "input": ["text"],
            "contextWindow": int(context_window),
            "maxTokens": 4096,
        }],
    }
    write_json(models_path, models)

    settings = {}
    if os.path.exists(settings_path):
        try:
            with open(settings_path, "r", encoding="utf-8") as fh:
                settings = json.load(fh)
        except (OSError, ValueError):
            settings = {}
    if not isinstance(settings, dict):
        settings = {}
    settings.update({
        "defaultProvider": mock.provider,
        "defaultModel": mock.model_id,
        "defaultThinkingLevel": "off",
        "compaction": {
            "enabled": True,
            "reserveTokens": int(reserve_tokens),
            "keepRecentTokens": int(keep_recent_tokens),
        },
        "quietStartup": True,
    })
    if extra_settings:
        settings.update(extra_settings)
    write_json(settings_path, settings)
    return agent_dir


def resolve_agent_dir(explicit=None):
    """Resolve the agent directory the child will really use.

    Mirrors ``src/coding_agent/context.rs``: an explicit ``--agent-dir`` wins,
    then ``PIR_AGENT_DIR`` / ``PIR_CODING_AGENT_DIR`` / ``PI_CODING_AGENT_DIR``,
    then the platform default ``~/.pi/agent``. Callers must not create this
    directory (real-provider mode reads the user's existing state).
    """
    if explicit:
        return resolve_path(explicit)
    for key in ("PIR_AGENT_DIR", "PIR_CODING_AGENT_DIR", "PI_CODING_AGENT_DIR"):
        value = os.environ.get(key)
        if value:
            return resolve_path(value)
    home = os.environ.get("HOME") or os.environ.get("USERPROFILE")
    if not home:
        home = os.path.expanduser("~")
    return os.path.abspath(os.path.join(home, ".pi", "agent"))


def build_child_env(agent_dir, extra=None):
    env = dict(os.environ)
    if agent_dir:
        env["PIR_AGENT_DIR"] = agent_dir
        env["PIR_CODING_AGENT_DIR"] = agent_dir
        env["PI_CODING_AGENT_DIR"] = agent_dir
    else:
        # Real-provider mode without --agent-dir: inherit the user's default
        # agent directory instead of silently pointing at a soak temp dir.
        for key in ("PIR_AGENT_DIR", "PIR_CODING_AGENT_DIR", "PI_CODING_AGENT_DIR"):
            env.pop(key, None)
    env["PIR_NO_UPDATE_CHECK"] = "1"
    env["RUST_LOG"] = env.get("RUST_LOG", "error")
    # Never route localhost mock traffic through a corporate proxy.
    existing_no_proxy = env.get("NO_PROXY") or env.get("no_proxy") or ""
    merged = ",".join(part for part in (
        existing_no_proxy, "127.0.0.1", "localhost") if part)
    env["NO_PROXY"] = merged
    env["no_proxy"] = merged
    if extra:
        env.update(extra)
    return env


def build_pir_argv(pir, model, thinking="off", session_dir=None, session_id=None,
                   session_path=None, prompts=None, extra_args=None):
    """The argv shape both ``ab_bird.mjs`` and ``big_session_probe.py`` use.

    ``--print`` is followed by another option (never by the first prompt), so
    clap's optional value stays unset and the prompts are positionals.
    """
    argv = [pir, "--mode", "json", "--print", "--model", model]
    if thinking:
        argv += ["--thinking", thinking]
    argv += ["--no-context-files"]
    if extra_args:
        argv += list(extra_args)
    if session_dir:
        argv += ["--session-dir", session_dir]
    if session_id:
        argv += ["--session-id", session_id]
    elif session_path:
        argv += ["--session", session_path]
    if prompts:
        argv += list(prompts)
    return argv


def make_session_fixture(path, size_mb, cwd, usage_tokens=None):
    """Write a valid v3 JSONL session of roughly ``size_mb`` MB.

    Adapted from ``scripts/perf/big_session_probe.py``. Assistant messages
    carry the provider bookkeeping a real session has (api/provider/model/
    stopReason/usage); the reader rejects entries without it. ``usage_tokens``
    sets the last (and every) assistant message's ``totalTokens``: a value
    above the model context window makes the next open take the tail-first +
    compaction path, a low value makes it widen to the full transcript.

    ``usage_tokens is None`` means "grow a low, realistic value per entry"; an
    explicit ``0`` is honored as 0 (the full/widening load path) instead of
    being treated as "unset" — an override of 0 must not silently fall back.
    The override sets ``totalTokens`` exactly (the metric the loader reads).
    """
    target = max(1, int(size_mb * 1024 * 1024))
    payload = ("lorem ipsum dolor sit amet consectetur adipiscing elit " * 20).strip()
    session_id = str(uuid.uuid4())
    stamp = "2026-01-01T00:00:00.000Z"
    ensure_dir(os.path.dirname(os.path.abspath(path)))
    written = 0
    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        def emit(value):
            nonlocal written
            line = json.dumps(value, separators=(",", ":")) + "\n"
            fh.write(line)
            written += len(line)

        emit({"type": "session", "version": 3, "id": session_id,
              "timestamp": stamp, "cwd": cwd})
        emit({"type": "model_change", "id": "m0", "parentId": None,
              "timestamp": stamp, "provider": "mock", "modelId": "soak-model"})
        emit({"type": "thinking_level_change", "id": "t0", "parentId": "m0",
              "timestamp": stamp, "thinkingLevel": "off"})
        index = 0
        parent = "t0"
        fixed_tokens = None if usage_tokens is None else max(0, int(usage_tokens))
        while written < target:
            index += 1
            if fixed_tokens is None:
                tokens = 1000 + index
            else:
                # Honor the override exactly, including 0 (full-load path).
                tokens = fixed_tokens
            user_id = "u%06d" % index
            asst_id = "a%06d" % index
            emit({"type": "message", "id": user_id, "parentId": parent,
                  "timestamp": stamp,
                  "message": {"role": "user", "timestamp": 0,
                              "content": [{"type": "text",
                                           "text": "question %d: %s" % (index, payload)}]}})
            emit({"type": "message", "id": asst_id, "parentId": user_id,
                  "timestamp": stamp,
                  "message": {"role": "assistant", "timestamp": 0,
                              "api": "openai-completions", "provider": "mock",
                              "model": "soak-model", "stopReason": "stop",
                              "usage": {
                                  "input": max(0, tokens - 120), "output": 120,
                                  "cacheRead": 0, "cacheWrite": 0,
                                  "totalTokens": tokens,
                                  "cost": {"input": 0, "output": 0, "cacheRead": 0,
                                           "cacheWrite": 0, "total": 0},
                              },
                              "content": [{"type": "text",
                                           "text": "answer %d: %s" % (index, payload)}]}})
            parent = asst_id
    return written


def rewrite_fixture_usage(path, usage_tokens):
    """Rewrite every assistant ``usage.totalTokens`` in a session fixture.

    Used to honor ``--fixture-usage-tokens`` for a supplied ``--fixture``: the
    load path depends on the newest assistant usage, so without this the
    override would be accepted and silently ignored. The file is rewritten
    atomically; non-message entries keep their bytes.
    """
    tokens = max(0, int(usage_tokens))
    tmp = path + ".usage.tmp"
    rewritten = 0
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as src, \
                open(tmp, "w", encoding="utf-8", newline="\n") as dst:
            for raw in src:
                stripped = raw.rstrip("\n")
                if not stripped:
                    continue
                try:
                    entry = json.loads(stripped)
                except ValueError:
                    dst.write(stripped + "\n")
                    continue
                message = entry.get("message") if isinstance(entry, dict) else None
                usage = message.get("usage") if isinstance(message, dict) else None
                if isinstance(message, dict) and message.get("role") == "assistant" \
                        and isinstance(usage, dict):
                    # The flag names the LAST-assistant metric the loader reads
                    # (totalTokens), so set it exactly: input is the remainder
                    # after the recorded output.
                    output = max(0, int(usage.get("output") or 0))
                    usage["totalTokens"] = tokens
                    usage["input"] = max(0, tokens - output)
                    dst.write(json.dumps(entry, separators=(",", ":")) + "\n")
                    rewritten += 1
                else:
                    dst.write(stripped + "\n")
        os.replace(tmp, path)
    except OSError:
        try:
            os.remove(tmp)
        except OSError:
            pass
        raise
    return rewritten


def find_session_file(session_dir, session_id):
    """Locate ``<timestamp>_<session_id>.jsonl`` under ``session_dir``."""
    if not os.path.isdir(session_dir):
        return None
    best = None
    best_mtime = -1.0
    for name in os.listdir(session_dir):
        if not name.endswith(".jsonl"):
            continue
        path = os.path.join(session_dir, name)
        # The id is the file-name suffix; match it exactly to avoid a partial
        # uuid followed by more characters.
        if not name.endswith("_%s.jsonl" % session_id):
            continue
        try:
            mtime = os.path.getmtime(path)
        except OSError:
            continue
        if mtime > best_mtime:
            best = path
            best_mtime = mtime
    return best


def write_fixture_file(work_dir, name="soak_fixture.txt", lines=400):
    path = os.path.join(work_dir, name)
    ensure_dir(work_dir)
    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        for index in range(lines):
            fh.write("soak fixture line %04d lorem ipsum dolor sit amet "
                     "consectetur adipiscing elit sed do eiusmod tempor\n" % index)
    return path


def auto_batch_prompts(base_argv, prompts, limit=None):
    """Partition prompts so each argv stays under the platform command limit.

    Returns ``[batch_prompts, ...]``. A prompt longer than the limit itself is
    still put in its own batch (the process will fail with the OS error, which
    the driver reports); the generated soak prompts are tiny, so this only
    matters when a user passes custom ones.
    """
    if limit is None:
        limit = 30000 if os.name == "nt" else 2000000
    base_len = sum(len(part) + 3 for part in base_argv)
    batches = []
    current = []
    current_len = base_len
    for prompt in prompts:
        cost = len(prompt) + 3
        if current and current_len + cost > limit:
            batches.append(current)
            current = []
            current_len = base_len
        current.append(prompt)
        current_len += cost
    if current:
        batches.append(current)
    return batches


def log_tail(path, max_lines=8):
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            lines = fh.read().splitlines()
        return lines[-max_lines:]
    except OSError:
        return []


def print_artifact_hint(log_dir):
    print("[soak] artifacts: %s" % log_dir)
    print("[soak]   summary.json   machine-readable verdict")
    print("[soak]   *.stdout.jsonl raw --mode json event stream")
    print("[soak]   *.stderr.txt   child stderr")
    print("[soak]   samples.jsonl  RSS / process-count samples")


def finish_summary(log_dir, payload, code):
    payload["exitCode"] = code
    write_json(os.path.join(log_dir, "summary.json"), payload)
    print_artifact_hint(log_dir)
    return code
