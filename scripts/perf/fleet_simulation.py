#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Fleet-scale resource simulation for `pir` (Windows, stdlib only).

Why this exists
---------------
The target workload (100 sessions x 1,000 turns x ~100 tool calls/turn) is
10 million tool calls; it cannot be run end-to-end.  This harness therefore
does the two things that *can* be evidence:

1. **Measured unit costs.**  It drives the real `pir --mode json --print`
   binary against the offline OpenAI-compatible mock (the same shape as
   `scripts/soak/soak_common.py`) over a small matrix of real sessions /
   turns / tool calls, and samples each process tree's working set, CPU time
   (kernel+user), thread count, handle count, process-tree membership, and
   the session directory size.  CPU is always the *process CPU-time delta*
   (`GetProcessTimes` + a Job Object total), never wall clock; the machine's
   `Win32_Processor.LoadPercentage` is recorded alongside as background noise.
2. **Extrapolation.**  It fits per-turn / per-tool-call / per-KB-text /
   per-compaction unit costs from the measured legs, then extrapolates the
   target workload in optimistic / typical / pessimistic scenarios.  Every
   extrapolated number is labelled as such; measured and extrapolated values
   are never mixed in one table without the label.

The mock supports multi-tool-call turns: a tool-marked prompt receives one
tool call per request until the requested count is reached, then the final
text answer.  Tool results are real `read`/`bash` executions inside pir (not
synthetic numbers); `bash` spawns a real child shell, `read` stays in-process.

Usage
-----
    python scripts/perf/fleet_simulation.py --pir target/debug/pir.exe \
        --out logs/lane-fleet/run1
    python scripts/perf/fleet_simulation.py --sessions 100 --turns 1000 \
        --tool-calls 100 --text-kb 5 --compactions 100 --out ...   # target only
    python scripts/perf/fleet_simulation.py --from-results <fleet_results.json>
        # re-extrapolate with different target parameters, no runs

Outputs `<out>/fleet_results.json` (all raw leg records + units + scenarios)
and `<out>/FLEET.md` (measured vs extrapolated tables).

Exit codes: 0 = run completed (legs may individually be marked invalid);
2 = setup error / binary missing; 3 = a real run leg failed.

File scope: this script only writes under its `--out` directory.
"""

import argparse
import ctypes
import ctypes.wintypes as wt
import json
import math
import os
import re
import shutil
import statistics
import subprocess
import sys
import threading
import time
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, os.path.join(REPO, "scripts", "soak"))

import soak_common as common  # noqa: E402  (shared mock SSE shape / helpers)

CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)

# ---------------------------------------------------------------------------
# Win32 primitives (stdlib ctypes; no psutil)
# ---------------------------------------------------------------------------

_k32 = ctypes.WinDLL("kernel32", use_last_error=True)
_psapi = ctypes.WinDLL("psapi", use_last_error=True)

PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
TH32CS_SNAPPROCESS = 0x00000002
TH32CS_SNAPTHREAD = 0x00000004
INVALID_HANDLE_VALUE = ctypes.c_void_p(-1).value


class PROCESS_MEMORY_COUNTERS(ctypes.Structure):
    _fields_ = [
        ("cb", wt.DWORD), ("PageFaultCount", wt.DWORD),
        ("PeakWorkingSetSize", ctypes.c_size_t),
        ("WorkingSetSize", ctypes.c_size_t),
        ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
        ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
        ("PagefileUsage", ctypes.c_size_t),
        ("PeakPagefileUsage", ctypes.c_size_t),
    ]


class PROCESSENTRY32W(ctypes.Structure):
    _fields_ = [
        ("dwSize", wt.DWORD), ("cntUsage", wt.DWORD),
        ("th32ProcessID", wt.DWORD),
        ("th32DefaultHeapID", ctypes.POINTER(ctypes.c_ulong)),
        ("th32ModuleID", wt.DWORD), ("cntThreads", wt.DWORD),
        ("th32ParentProcessID", wt.DWORD),
        ("pcPriClassBase", ctypes.c_long), ("dwFlags", wt.DWORD),
        ("szExeFile", wt.WCHAR * 260),
    ]


class THREADENTRY32(ctypes.Structure):
    _fields_ = [
        ("dwSize", wt.DWORD), ("cntUsage", wt.DWORD),
        ("th32ThreadID", wt.DWORD), ("th32OwnerProcessID", wt.DWORD),
        ("tpBasePri", ctypes.c_long), ("tpDeltaPri", ctypes.c_long),
        ("dwFlags", wt.DWORD),
    ]


class JOBOBJECT_IO_COUNTERS(ctypes.Structure):
    _fields_ = [
        ("ReadOperationCount", ctypes.c_ulonglong),
        ("WriteOperationCount", ctypes.c_ulonglong),
        ("OtherOperationCount", ctypes.c_ulonglong),
        ("ReadTransferCount", ctypes.c_ulonglong),
        ("WriteTransferCount", ctypes.c_ulonglong),
        ("OtherTransferCount", ctypes.c_ulonglong),
    ]


class JOBOBJECT_BASIC_AND_IO_ACCOUNTING(ctypes.Structure):
    _fields_ = [
        ("TotalUserTime", ctypes.c_ulonglong),
        ("TotalKernelTime", ctypes.c_ulonglong),
        ("ThisPeriodTotalUserTime", ctypes.c_ulonglong),
        ("ThisPeriodTotalKernelTime", ctypes.c_ulonglong),
        ("TotalPageFaultCount", wt.DWORD),
        ("TotalProcesses", wt.DWORD),
        ("ActiveProcesses", wt.DWORD),
        ("TotalTerminatedProcesses", wt.DWORD),
        ("IoInfo", JOBOBJECT_IO_COUNTERS),
    ]


def _filetime_seconds(value):
    return (value.dwHighDateTime << 32 | value.dwLowDateTime) / 1e7


def open_process(pid):
    handle = _k32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, int(pid))
    return handle if handle else None


def close_handle(handle):
    if handle:
        try:
            _k32.CloseHandle(handle)
        except Exception:
            pass


def proc_rss_mb(handle):
    mem = PROCESS_MEMORY_COUNTERS()
    mem.cb = ctypes.sizeof(PROCESS_MEMORY_COUNTERS)
    if not _psapi.GetProcessMemoryInfo(handle, ctypes.byref(mem), mem.cb):
        return None
    return mem.WorkingSetSize / (1024.0 * 1024.0)


def proc_cpu_s(handle):
    created = wt.FILETIME()
    exited = wt.FILETIME()
    kernel = wt.FILETIME()
    user = wt.FILETIME()
    if not _k32.GetProcessTimes(handle, ctypes.byref(created), ctypes.byref(exited),
                                ctypes.byref(kernel), ctypes.byref(user)):
        return None
    return _filetime_seconds(kernel) + _filetime_seconds(user)


def proc_handles(handle):
    count = wt.DWORD(0)
    if not _k32.GetProcessHandleCount(handle, ctypes.byref(count)):
        return None
    return count.value


def process_table():
    """{pid: (ppid, thread_count)} from one Toolhelp32 snapshot."""
    snap = _k32.CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
    if not snap or snap == INVALID_HANDLE_VALUE:
        return {}
    rows = {}
    try:
        entry = PROCESSENTRY32W()
        entry.dwSize = ctypes.sizeof(PROCESSENTRY32W)
        ok = _k32.Process32FirstW(snap, ctypes.byref(entry))
        while ok:
            rows[int(entry.th32ProcessID)] = (int(entry.th32ParentProcessID),
                                              int(entry.cntThreads))
            ok = _k32.Process32NextW(snap, ctypes.byref(entry))
    finally:
        _k32.CloseHandle(snap)
    return rows


def tree_pids(root_pid, rows=None):
    rows = rows if rows is not None else process_table()
    children = {}
    for pid, (ppid, _threads) in rows.items():
        children.setdefault(ppid, []).append(pid)
    out, stack = [], [root_pid]
    seen = set()
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        out.append(pid)
        stack.extend(children.get(pid, ()))
    return [pid for pid in out if pid in rows]


# --- job object (aggregate CPU + IO across a whole leg, children included) ---

def job_create():
    _k32.CreateJobObjectW.restype = wt.HANDLE
    _k32.CreateJobObjectW.argtypes = [ctypes.c_void_p, wt.LPCWSTR]
    handle = _k32.CreateJobObjectW(None, None)
    return handle if handle else None


def job_assign(job, popen_handle):
    try:
        return bool(_k32.AssignProcessToJobObject(job, int(popen_handle)))
    except Exception:
        return False


def job_query(job):
    if not job:
        return None
    info = JOBOBJECT_BASIC_AND_IO_ACCOUNTING()
    returned = ctypes.c_ulong(0)
    ok = _k32.QueryInformationJobObject(
        job, 8, ctypes.byref(info), ctypes.sizeof(info), ctypes.byref(returned))
    if not ok:
        return None
    return {
        "cpuSeconds": (info.TotalUserTime + info.TotalKernelTime) / 1e7,
        "userSeconds": info.TotalUserTime / 1e7,
        "kernelSeconds": info.TotalKernelTime / 1e7,
        "totalProcesses": int(info.TotalProcesses),
        "activeProcesses": int(info.ActiveProcesses),
        "readOps": int(info.IoInfo.ReadOperationCount),
        "writeOps": int(info.IoInfo.WriteOperationCount),
        "otherOps": int(info.IoInfo.OtherOperationCount),
        "readBytes": int(info.IoInfo.ReadTransferCount),
        "writeBytes": int(info.IoInfo.WriteTransferCount),
    }


def job_close(job):
    if job:
        try:
            _k32.CloseHandle(job)
        except Exception:
            pass


# --- system load (background noise; the exact metric the task names) ---

_LOAD_SCRIPT = ("Get-CimInstance Win32_Processor | "
                "Measure-Object -Property LoadPercentage -Average | "
                "ForEach-Object { $_.Average }")


def system_load_percent():
    try:
        done = subprocess.run(
            ["powershell", "-NoProfile", "-NonInteractive", "-Command", _LOAD_SCRIPT],
            capture_output=True, text=True, errors="replace", timeout=15,
            creationflags=CREATE_NO_WINDOW,
        )
        text = (done.stdout or "").strip()
        return float(text) if text else None
    except (OSError, ValueError, subprocess.SubprocessError):
        return None


class LoadSampler(threading.Thread):
    """Background `Win32_Processor.LoadPercentage` sampler."""

    def __init__(self, interval=1.0):
        super().__init__(daemon=True)
        self.interval = interval
        self.samples = []
        self._stop_event = threading.Event()

    def run(self):
        while not self._stop_event.is_set():
            value = system_load_percent()
            if value is not None:
                self.samples.append(round(value, 1))
            self._stop_event.wait(self.interval)

    def stop(self):
        self._stop_event.set()
        self.join(timeout=10)
        return self.samples


# ---------------------------------------------------------------------------
# Process-tree sampler with per-process CPU/RSS/threads/handles
# ---------------------------------------------------------------------------

class LegSampler(threading.Thread):
    """Samples every root process of one leg, plus its whole process tree.

    Stable handles are opened by the caller before the run and kept open, so
    the *final* CPU time is still readable after the process exits (Windows
    keeps the process object alive while a handle exists).  Peak RSS / thread
    / handle values come from live samples; the last live RSS is the steady
    state.
    """

    def __init__(self, roots, interval=0.25):
        super().__init__(daemon=True)
        # roots: list of {"index", "pid", "handle"}
        self.roots = roots
        self.interval = interval
        self._stop_event = threading.Event()
        self.records = {r["index"]: {
            "rssPeakMb": 0.0, "rssLastMb": None, "threadsPeak": 0,
            "handlesPeak": 0, "handlesLast": None, "childPeak": 0,
            "treeRssPeakMb": 0.0, "samples": 0,
        } for r in roots}
        self.aggregate = []

    def sample_once(self):
        rows = process_table()
        total_rss = 0.0
        total_handles = 0
        total_threads = 0
        total_procs = 0
        for root in self.roots:
            index, pid, handle = root["index"], root["pid"], root["handle"]
            rec = self.records[index]
            if not handle:
                continue
            rss = proc_rss_mb(handle)
            # A terminated process (handle still valid for final CPU) reports
            # a 0 MB working set; that is "gone", not a live steady-state
            # reading. Keep the last positive value.
            if rss is None or rss <= 0.5:
                continue
            rec["samples"] += 1
            rec["rssLastMb"] = round(rss, 1)
            rec["rssPeakMb"] = max(rec["rssPeakMb"], rss)
            threads = rows.get(pid, (0, 0))[1] if rows else 0
            rec["threadsPeak"] = max(rec["threadsPeak"], threads)
            handles = proc_handles(handle)
            if handles is not None:
                rec["handlesPeak"] = max(rec["handlesPeak"], handles)
                rec["handlesLast"] = handles
            members = tree_pids(pid, rows) if rows else [pid]
            rec["childPeak"] = max(rec["childPeak"], max(0, len(members) - 1))
            tree_rss = 0.0
            for member in members:
                if member == pid:
                    tree_rss += rss
                    continue
                child = open_process(member)
                if child:
                    try:
                        child_rss = proc_rss_mb(child)
                        if child_rss:
                            tree_rss += child_rss
                    finally:
                        close_handle(child)
            rec["treeRssPeakMb"] = max(rec["treeRssPeakMb"], tree_rss)
            total_rss += rss
            total_handles += handles or 0
            total_threads += threads
            total_procs += len(members)
        self.aggregate.append({
            "atS": round(time.monotonic() - self.started, 2),
            "totalRssMb": round(total_rss, 1),
            "totalHandles": total_handles,
            "totalThreads": total_threads,
            "totalTreeProcesses": total_procs,
        })

    def run(self):
        self.started = time.monotonic()
        while not self._stop_event.is_set():
            self.sample_once()
            self._stop_event.wait(self.interval)

    def stop(self):
        self._stop_event.set()
        self.join(timeout=30)
        return self.records, self.aggregate


# ---------------------------------------------------------------------------
# Mock provider that emits N tool calls before the final text answer
# ---------------------------------------------------------------------------

FLEET_MARKER = "[[soak:tool:bash]]"


def _repeat(prefix, target_chars, unit):
    if target_chars <= len(prefix):
        return prefix
    remaining = target_chars - len(prefix)
    copies = (remaining // max(1, len(unit))) + 1
    return prefix + (unit * copies)[:remaining]


def _classify_fleet(body, tools_per_turn):
    """summary | tool | text, using the newest prompt marker and tool count."""
    if common.SUMMARY_MARKER in body:
        return "summary"
    marker_pos = body.rfind(common.MARKER_PREFIX.encode("ascii"))
    if marker_pos < 0:
        return "text"
    head = body[marker_pos:marker_pos + 64]
    if b"tool" not in head:
        return "text"
    tail = body[marker_pos:]
    count = tail.count(b'"role":"tool"') + tail.count(b'"role": "tool"')
    return "tool" if count < tools_per_turn else "text"


class _FleetHandler(common._MockHandler):  # reuse GET/SSE plumbing
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
        kind = _classify_fleet(bytes(body), server.fleet_tools_per_turn)
        server.fleet_state.record(len(body), kind)
        prompt_tokens = max(1, len(body) // 4)
        try:
            if kind == "tool":
                self._send_fleet_tool(server, prompt_tokens)
            elif kind == "summary":
                self._send_text(server.fleet_summary, prompt_tokens)
            else:
                self._send_text(server.fleet_response, prompt_tokens)
        except (BrokenPipeError, ConnectionResetError, OSError):
            return

    def _send_fleet_tool(self, server, prompt_tokens):
        server.fleet_tool_seq += 1
        use_bash = server.fleet_tool_kind == "bash" or (
            server.fleet_tool_kind == "mix" and server.fleet_tool_seq % 5 == 1)
        if use_bash:
            name = "bash"
            lines = ["fleet bash line %04d %s" % (
                i, "lorem ipsum dolor sit amet consectetur adipiscing elit")
                for i in range(server.fleet_tool_lines)]
            # Unique prefix so the agent's degenerate-loop guard does not
            # treat 100 identical calls in one turn as the same execution.
            command = ("echo fleet-call-%d; printf '%s\\n' "
                       % (server.fleet_tool_seq,
                          " ".join("'%s'" % line for line in lines)))
            arguments = json.dumps({"command": command})
        else:
            name = "read"
            # Vary the offset: identical calls trip a repeat streak and the
            # agent stops a prompt after 40 calls (REPEAT_STREAK_STOP, 20+20).
            offset = (server.fleet_tool_seq % 286) + 1
            arguments = json.dumps({
                "path": server.fleet_fixture_name,
                "limit": 13,
                "offset": offset,
            })
        call_id = "call_fleet_%d_%d" % (int(time.time() * 1000),
                                        server.fleet_tool_seq)
        split = len(arguments) // 2
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        self.wfile.write(common._chunk({"role": "assistant", "content": ""},
                                       model=server.soak_model))
        self.wfile.write(common._chunk({
            "tool_calls": [{
                "index": 0, "id": call_id, "type": "function",
                "function": {"name": name, "arguments": arguments[:split]},
            }],
        }, model=server.soak_model))
        if split < len(arguments):
            self.wfile.write(common._chunk({
                "tool_calls": [{
                    "index": 0,
                    "function": {"arguments": arguments[split:]},
                }],
            }, model=server.soak_model))
        completion = max(1, len(arguments) // 4)
        self.wfile.write(common._chunk(
            {}, finish="tool_calls",
            usage={"prompt_tokens": prompt_tokens,
                   "completion_tokens": completion,
                   "total_tokens": prompt_tokens + completion},
            model=server.soak_model))
        self.wfile.write(b"data: [DONE]\n\n")


class FleetMock(common.MockProvider):
    """`soak_common.MockProvider` with multi-tool turns and per-leg stats."""

    def __init__(self, tools_per_turn=1, tool_kind="read", tool_kb=1.0,
                 response_chars=5000, summary_chars=1200, **kwargs):
        super().__init__(response_chars=response_chars,
                         summary_chars=summary_chars, **kwargs)
        self.tools_per_turn = max(0, int(tools_per_turn))
        self.tool_kind = tool_kind
        self.tool_kb = tool_kb
        self.tool_lines = max(1, int(round(tool_kb * 1024 / 78.0)))

    def start(self):
        super().start()
        # Rebuild the server with the fleet handler; the base class bound the
        # soak handler, so swap it on a fresh server (same port allocation
        # contract, no state shared with the base instance).
        server = common._MockServer(("127.0.0.1", 0), _FleetHandler)
        server.soak_state = self.state
        server.soak_model = self.model_id
        server.soak_delay_ms = 0
        server.soak_chunk_delay_ms = 0
        server.soak_chunk_chars = 64 * 1024
        server.soak_response = self._response_text
        server.soak_summary = self._summary_text
        server.soak_tool_lines = self.tool_lines
        server.soak_fixture_name = self.fixture_name
        server.fleet_state = self.state
        server.fleet_tools_per_turn = self.tools_per_turn
        server.fleet_tool_kind = self.tool_kind
        server.fleet_tool_lines = self.tool_lines
        server.fleet_fixture_name = self.fixture_name
        server.fleet_response = self._response_text
        server.fleet_summary = self._summary_text
        server.fleet_tool_seq = 0
        old = self._server
        if old is not None:
            try:
                old.shutdown()
                old.server_close()
            except Exception:
                pass
        self._server = server
        self._thread = threading.Thread(target=server.serve_forever, daemon=True)
        self._thread.start()
        return self


# ---------------------------------------------------------------------------
# Session analysis (bytes per turn / tool call / compaction)
# ---------------------------------------------------------------------------

def analyze_session(path):
    rec = {
        "path": path, "bytes": 0, "lines": 0, "headerBytes": 0,
        "systemBytes": 0, "userBytes": 0, "assistantTextBytes": 0,
        "toolCallEntryBytes": 0, "toolResultBytes": 0, "compactionBytes": 0,
        "compactions": 0, "toolCalls": 0, "assistantMessages": 0,
        "turns": 0, "messageEntries": 0, "totalEntries": 0, "entryTypes": {},
    }
    try:
        with open(path, "rb") as fh:
            for raw in fh:
                rec["bytes"] += len(raw)
                rec["lines"] += 1
                try:
                    entry = json.loads(raw.decode("utf-8", errors="replace"))
                except ValueError:
                    continue
                etype = entry.get("type")
                rec["totalEntries"] += 1
                rec["entryTypes"][etype] = rec["entryTypes"].get(etype, 0) + 1
                if etype == "compaction":
                    rec["compactions"] += 1
                    rec["compactionBytes"] += len(raw)
                if etype != "message":
                    if etype == "session":
                        rec["headerBytes"] += len(raw)
                    continue
                message = entry.get("message") or {}
                rec["messageEntries"] += 1
                role = message.get("role")
                content = message.get("content")
                blocks = content if isinstance(content, list) else []
                if role == "system":
                    rec["systemBytes"] += len(raw)
                elif role == "user":
                    rec["userBytes"] += len(raw)
                    rec["turns"] += 1
                elif role == "assistant":
                    rec["assistantMessages"] += 1
                    has_tool = any(isinstance(b, dict) and b.get("type") == "toolCall"
                                   for b in blocks)
                    if has_tool:
                        rec["toolCallEntryBytes"] += len(raw)
                        rec["toolCalls"] += sum(
                            1 for b in blocks
                            if isinstance(b, dict) and b.get("type") == "toolCall")
                    else:
                        rec["assistantTextBytes"] += len(raw)
                elif role == "toolResult":
                    rec["toolResultBytes"] += len(raw)
    except OSError as exc:
        rec["error"] = str(exc)
    return rec


def dir_stats(path):
    total = 0
    files = 0
    by_ext = {}
    if not os.path.isdir(path):
        return {"bytes": 0, "files": 0, "byExt": {}}
    for root, _dirs, names in os.walk(path):
        for name in names:
            full = os.path.join(root, name)
            try:
                size = os.path.getsize(full)
            except OSError:
                continue
            total += size
            files += 1
            ext = os.path.splitext(name)[1] or "(none)"
            by_ext[ext] = by_ext.get(ext, 0) + size
    return {"bytes": total, "files": files, "byExt": by_ext}


def count_events(stdout_path):
    counts = {}
    compaction_ok = 0
    tool_calls = 0
    tool_errors = 0
    try:
        with open(stdout_path, "r", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                line = line.strip()
                if not line.startswith("{"):
                    continue
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                etype = event.get("type")
                counts[etype] = counts.get(etype, 0) + 1
                if etype == "compaction_end" and common.compaction_succeeded(event):
                    compaction_ok += 1
                elif etype == "tool_execution_end":
                    tool_calls += 1
                    if event.get("isError"):
                        tool_errors += 1
    except OSError:
        pass
    return {"byType": counts, "compactions": compaction_ok,
            "toolCalls": tool_calls, "toolErrors": tool_errors}


# ---------------------------------------------------------------------------
# Leg runner
# ---------------------------------------------------------------------------

def make_prompts(turns, tools_per_turn):
    prompts = []
    for i in range(1, turns + 1):
        marker = FLEET_MARKER if tools_per_turn > 0 else common.MARKER_TEXT
        prompts.append("f%05d %s" % (i, marker))
    return prompts


def run_leg(name, args, mock, agent_dir, sessions, turns, tools_per_turn,
            text_kb, tool_kind, context_window, reserve_tokens,
            keep_recent_tokens, out_root, timeout=None):
    leg_dir = os.path.join(out_root, "legs", name)
    # A rerun must not mix session files from an earlier attempt into the
    # per-turn byte accounting (measured: a reused directory reported 2x the
    # turns because the previous leg's session file was still there).
    shutil.rmtree(leg_dir, ignore_errors=True)
    sessions_dir = os.path.join(leg_dir, "sessions")
    stdout_dir = os.path.join(leg_dir, "stdout")
    stderr_dir = os.path.join(leg_dir, "stderr")
    work_dir = os.path.join(leg_dir, "work")
    for path in (sessions_dir, stdout_dir, stderr_dir, work_dir):
        os.makedirs(path, exist_ok=True)
    common.write_fixture_file(work_dir, lines=300)
    common.provision_mock_agent(agent_dir, mock, context_window,
                                reserve_tokens=reserve_tokens,
                                keep_recent_tokens=keep_recent_tokens)
    env = common.build_child_env(agent_dir)
    prompts = make_prompts(turns, tools_per_turn)
    disk_before = dir_stats(sessions_dir)
    job = job_create()
    roots = []
    procs = []
    stdout_paths = []
    started = time.monotonic()
    if timeout is None:
        timeout = max(180.0, sessions * turns * 2.0)
    load_sampler = LoadSampler(interval=1.2)
    load_sampler.start()
    try:
        for index in range(sessions):
            session_id = str(uuid.uuid4())
            argv = [args.pir, "--mode", "json", "--print",
                    "--model", "mock/soak-model", "--thinking", "off",
                    "--no-context-files",
                    "--session-dir", sessions_dir,
                    "--session-id", session_id] + prompts
            out_path = os.path.join(stdout_dir, "s%03d.jsonl" % index)
            err_path = os.path.join(stderr_dir, "s%03d.stderr.txt" % index)
            out_fh = open(out_path, "wb")
            err_fh = open(err_path, "wb")
            proc = subprocess.Popen(
                argv, cwd=work_dir, env=env, stdin=subprocess.DEVNULL,
                stdout=out_fh, stderr=err_fh, creationflags=CREATE_NO_WINDOW)
            proc._out_fh = out_fh  # closed in the wait loop
            proc._err_fh = err_fh
            if job:
                job_assign(job, proc._handle)
            handle = open_process(proc.pid)
            roots.append({"index": index, "pid": proc.pid, "handle": handle})
            procs.append(proc)
            stdout_paths.append(out_path)
        sampler = LegSampler(roots, interval=0.25)
        sampler.start()
        timed_out = False
        deadline = started + timeout
        while any(proc.poll() is None for proc in procs):
            if time.monotonic() > deadline:
                timed_out = True
                for proc in procs:
                    if proc.poll() is None:
                        common.kill_process_tree(proc.pid)
                break
            time.sleep(0.1)
        for proc in procs:
            try:
                proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                common.kill_process_tree(proc.pid)
                proc.wait(timeout=10)
        sampler.stop()
        wall = time.monotonic() - started
        # Final CPU from the retained handle (valid after exit).
        per_process = sampler.records
        for root in roots:
            if root["handle"]:
                cpu = proc_cpu_s(root["handle"])
                per_process[root["index"]]["cpuSeconds"] = round(cpu, 4) if cpu else None
                per_process[root["index"]]["handlesFinal"] = proc_handles(root["handle"])
        job_info = job_query(job)
        disk_after = dir_stats(sessions_dir)
        load_samples = load_sampler.stop()
        events = [count_events(p) for p in stdout_paths]
        session_files = sorted(
            os.path.join(sessions_dir, n) for n in os.listdir(sessions_dir)
            if n.endswith(".jsonl"))
        analyses = [analyze_session(p) for p in session_files]
        sampled = [r for r in per_process.values() if r.get("samples")]
        per_proc_cpu = [r.get("cpuSeconds") for r in sampled
                        if r.get("cpuSeconds") is not None]
        rss_last = [r.get("rssLastMb") for r in sampled
                    if r.get("rssLastMb") is not None]
        rss_peak = [r.get("rssPeakMb") for r in sampled]
        tree_peak = [r.get("treeRssPeakMb") for r in sampled]
        threads_peak = [r.get("threadsPeak") for r in sampled]
        handles_peak = [r.get("handlesPeak") for r in sampled]
        totals = {
            "sessions": sessions,
            "turnsPerSession": turns,
            "toolCallsPerTurn": tools_per_turn,
            "textKb": text_kb,
            "toolKind": tool_kind,
            "wallSeconds": round(wall, 2),
            "timedOut": timed_out,
            "exitCodes": [proc.returncode for proc in procs],
            "jobCpuSeconds": round(job_info["cpuSeconds"], 3) if job_info else None,
            "sumProcessCpuSeconds": round(sum(per_proc_cpu), 3) if per_proc_cpu else None,
            "cpuSecondsPerTurn": (round((job_info["cpuSeconds"] if job_info else
                                         sum(per_proc_cpu)) / max(1, sessions * turns), 4)
                                  if (job_info or per_proc_cpu) else None),
            "cpuPctOneCorePerProcess": (round(100.0 * statistics.mean(per_proc_cpu) /
                                             wall, 2)
                                        if per_proc_cpu and wall else None),
            "rssLastMedianMb": round(statistics.median(rss_last), 1) if rss_last else None,
            "rssPeakMedianMb": round(statistics.median(rss_peak), 1) if rss_peak else None,
            "treePeakMedianMb": round(statistics.median(tree_peak), 1) if tree_peak else None,
            "threadsPeakMedian": statistics.median(threads_peak) if threads_peak else None,
            "handlesPeakMedian": statistics.median(handles_peak) if handles_peak else None,
            "maxChildrenPerRoot": max((r.get("childPeak", 0)
                                       for r in per_process.values()), default=0),
            "aggregatePeakRssMb": max((s["totalRssMb"] for s in sampler.aggregate),
                                      default=None),
            "aggregatePeakHandles": max((s["totalHandles"] for s in sampler.aggregate),
                                        default=None),
            "aggregatePeakThreads": max((s["totalThreads"] for s in sampler.aggregate),
                                        default=None),
            "aggregatePeakTreeProcesses": max((s["totalTreeProcesses"]
                                               for s in sampler.aggregate), default=None),
            "aggregateSamples": sampler.aggregate,
            "diskBytesBefore": disk_before["bytes"],
            "diskBytesAfter": disk_after["bytes"],
            "diskBytesDelta": disk_after["bytes"] - disk_before["bytes"],
            "diskFiles": disk_after["files"],
            "diskByExt": disk_after["byExt"],
            "sessionBytesTotal": sum(a["bytes"] for a in analyses),
            "sessionBytesPerSession": round(sum(a["bytes"] for a in analyses) /
                                            max(1, len(analyses)), 1),
            "sessionAnalyzed": len(analyses),
            "turnsMeasured": sum(a["turns"] for a in analyses),
            "toolCallsMeasured": sum(a["toolCalls"] for a in analyses),
            "compactionsMeasured": sum(a["compactions"] for a in analyses),
            "systemBytes": sum(a["systemBytes"] for a in analyses),
            "headerBytes": sum(a["headerBytes"] for a in analyses),
            "assistantTextBytes": sum(a["assistantTextBytes"] for a in analyses),
            "toolCallEntryBytes": sum(a["toolCallEntryBytes"] for a in analyses),
            "toolResultBytes": sum(a["toolResultBytes"] for a in analyses),
            "userBytes": sum(a["userBytes"] for a in analyses),
            "compactionBytes": sum(a["compactionBytes"] for a in analyses),
            "messageEntries": sum(a["messageEntries"] for a in analyses),
            "totalEntries": sum(a["totalEntries"] for a in analyses),
            "events": {
                "agentEnd": sum(e["byType"].get("agent_end", 0) for e in events),
                "toolCalls": sum(e["toolCalls"] for e in events),
                "toolErrors": sum(e["toolErrors"] for e in events),
                "compactions": sum(e["compactions"] for e in events),
                "errorEvents": sum(e["byType"].get("error", 0) for e in events),
            },
            "jobIo": {k: job_info[k] for k in
                      ("readOps", "writeOps", "otherOps", "readBytes", "writeBytes")
                      } if job_info else None,
            "loadPercent": {
                "samples": len(load_samples),
                "start": load_samples[0] if load_samples else None,
                "median": round(statistics.median(load_samples), 1) if load_samples else None,
                "max": max(load_samples) if load_samples else None,
                "end": load_samples[-1] if load_samples else None,
            },
            "processes": [
                {"index": i, **{k: v for k, v in rec.items()}}
                for i, rec in sorted(per_process.items())
            ],
            "rawStdout": os.path.relpath(stdout_dir, REPO).replace("\\", "/"),
            "rawSessions": os.path.relpath(sessions_dir, REPO).replace("\\", "/"),
        }
        # Checks that make a leg invalid instead of silently misleading.
        problems = []
        if timed_out:
            problems.append("timed out")
        if any(code not in (0, None) for code in totals["exitCodes"]):
            problems.append("non-zero exit: %s" % totals["exitCodes"])
        if totals["turnsMeasured"] != sessions * turns:
            problems.append("turns measured %s != %s" %
                            (totals["turnsMeasured"], sessions * turns))
        if totals["toolCallsMeasured"] < sessions * turns * tools_per_turn:
            problems.append("tool calls measured %s < expected %s" %
                            (totals["toolCallsMeasured"],
                             sessions * turns * tools_per_turn))
        totals["problems"] = problems
        totals["valid"] = not problems
        record = {"name": name, "ok": totals["valid"], **totals,
                  "config": {"contextWindow": context_window,
                             "reserveTokens": reserve_tokens,
                             "keepRecentTokens": keep_recent_tokens}}
        with open(os.path.join(leg_dir, "leg.json"), "w", encoding="utf-8") as fh:
            json.dump(record, fh, indent=2)
        return record
    finally:
        try:
            load_sampler.stop()
        except Exception:
            pass
        for root in roots:
            close_handle(root.get("handle"))
        job_close(job)
        for proc in procs:
            for attr in ("_out_fh", "_err_fh"):
                fh = getattr(proc, attr, None)
                if fh:
                    try:
                        fh.close()
                    except Exception:
                        pass


# ---------------------------------------------------------------------------
# Unit model fitting
# ---------------------------------------------------------------------------

def _leg_by_name(legs, name):
    for leg in legs:
        if leg["name"] == name:
            return leg
    return None


def _cpu_per_session(leg):
    value = leg.get("jobCpuSeconds")
    if value is None:
        value = leg.get("sumProcessCpuSeconds")
    return value / max(1, leg["sessions"]) if value is not None else None


def _median_final_rss(leg):
    values = [p.get("rssLastMb") for p in leg.get("processes", [])
              if p.get("rssLastMb") is not None]
    return statistics.median(values) if values else None


def _median_tree_peak(leg):
    values = [p.get("treeRssPeakMb") for p in leg.get("processes", [])]
    return statistics.median(values) if values else None


def _bytes_per_turn(leg):
    """Session bytes that scale with turns (exclude header/system/compaction)."""
    turns = leg.get("turnsMeasured") or 0
    if not turns:
        return None
    base = (leg.get("sessionBytesTotal") or 0) - (leg.get("headerBytes") or 0) \
        - (leg.get("systemBytes") or 0) - (leg.get("compactionBytes") or 0)
    return max(0.0, base) / turns


def _bytes_per_tool_call(leg):
    calls = leg.get("toolCallsMeasured") or 0
    if not calls:
        return None
    return ((leg.get("toolCallEntryBytes") or 0)
            + (leg.get("toolResultBytes") or 0)) / calls


def _bytes_per_text_kb(leg):
    kb = (leg.get("turnsMeasured") or 0) * float(leg.get("textKb") or 0)
    if kb <= 0:
        return None
    return (leg.get("assistantTextBytes") or 0) / kb


def fit_units(legs):
    """Fit per-turn / per-tool / per-KB / per-compaction costs.

    The fit uses only valid legs from the `unit` family.  Missing comparison
    legs leave the corresponding unit at a documented fallback (0 or the
    direct measurement) instead of aborting the whole run.
    """
    valid = [l for l in legs if l.get("valid")]
    units = {"notes": []}

    # The 50-turn pair is the anchor: per-request CPU grows with the
    # serialized transcript, so 10-turn legs under-report the steady-state
    # per-turn cost. At the same context scale, 1 vs 5 tools isolates the
    # marginal tool cost; the bash leg isolates the shell premium.
    l50_5r = _leg_by_name(valid, "unit-1x50x5-read")
    l50_1r = _leg_by_name(valid, "unit-1x50x1-read")
    l50_5b = _leg_by_name(valid, "unit-1x50x5-bash")
    l50_t1 = _leg_by_name(valid, "unit-1x50x1-read-t1")
    l50_t20 = _leg_by_name(valid, "unit-1x50x1-read-t20")
    l10_5r = _leg_by_name(valid, "unit-1x10x5-read")
    l10_1r = _leg_by_name(valid, "unit-1x10x1-read")

    def cps(leg):
        return _cpu_per_session(leg) if leg else None

    c50_5r, c50_1r = cps(l50_5r), cps(l50_1r)
    tool_read = None
    if c50_5r is not None and c50_1r is not None:
        tool_read = max(0.0, (c50_5r - c50_1r) / (4.0 * 50.0))
        units["notes"].append("tool read from 50-turn 1 vs 5 tools")
    if tool_read is None:
        # Fall back to the 10-turn pair, knowing it under-reports.
        c10_5r, c10_1r = cps(l10_5r), cps(l10_1r)
        if c10_5r is not None and c10_1r is not None:
            tool_read = max(0.0, (c10_5r - c10_1r) / (4.0 * 10.0))
            units["notes"].append("tool read from 10-turn 1 vs 5 tools (fallback)")
        else:
            tool_read = 0.0
            units["notes"].append("tool read unavailable; set 0")

    tool_bash = tool_read
    c50_5b = cps(l50_5b)
    if c50_5b is not None and c50_5r is not None:
        premium = (c50_5b - c50_5r) / (5.0 * 50.0)
        tool_bash = max(tool_read, tool_read + premium)
        units["notes"].append("bash premium from 50-turn read vs bash legs")

    text_kb_cost = 0.0
    c50t1, c50t20 = cps(l50_t1), cps(l50_t20)
    if c50t1 is not None and c50t20 is not None:
        text_kb_cost = max(0.0, (c50t20 - c50t1) / (19.0 * 50.0))
        units["notes"].append("text cost from 50-turn 1 KB vs 20 KB legs")
    else:
        t1 = _leg_by_name(valid, "unit-1x10x1-read-t1")
        t20 = _leg_by_name(valid, "unit-1x10x1-read-t20")
        if cps(t1) is not None and cps(t20) is not None:
            text_kb_cost = max(0.0, (cps(t20) - cps(t1)) / (19.0 * 10.0))
            units["notes"].append("text cost from 10-turn legs (fallback)")
        else:
            units["notes"].append("text-cost legs unavailable; set 0")

    # Dense anchor: one session, 10 turns, 100 read calls/turn — the only leg
    # whose shape matches the target's tool density. Per-tool CPU there is
    # context-bound (~2.3 MB transcript by turn 10), which is what a target
    # turn grows between two compactions at 100 tools/turn.
    dense = _leg_by_name(valid, "unit-1x10x100-read")
    cpu_tool_read_dense = None
    if dense and cps(dense) is not None:
        per_turn_dense = cps(dense) / 10.0
        cpu_tool_read_dense = max(0.0, (per_turn_dense - 5.0 * text_kb_cost) / 100.0)
        units["notes"].append("dense 100-tool anchor: %.1f ms/tool at 10 turns"
                              % (cpu_tool_read_dense * 1000.0))
    cpu_tool_bash_dense = None
    if cpu_tool_read_dense is not None:
        premium = max(0.0, (tool_bash or 0.0) - (tool_read or 0.0))
        cpu_tool_bash_dense = cpu_tool_read_dense + premium
        if premium:
            units["notes"].append("bash dense = dense read + low-density bash premium")

    # Base turn cost at the anchor scale (startup/50 is inside it; at 1,000
    # turns the startup share is <0.1% and is added separately below).
    cpu_base_turn = 0.0
    if c50_1r is not None:
        cpu_base_turn = max(0.0, c50_1r / 50.0 - tool_read - 5.0 * text_kb_cost)
    else:
        units["notes"].append("base turn unavailable; set 0")

    # Startup CPU from the 10 vs 50-turn pair (same shape); clamp at 0 because
    # short-run noise can make the fitted intercept negative.
    startup = 0.0
    if l10_1r and l50_1r:
        a, b = cps(l10_1r), cps(l50_1r)
        if a is not None and b is not None:
            slope = (b - a) / 40.0
            startup = max(0.0, a - 10.0 * slope)

    # Compaction cost: compaction leg versus the same 40-turn shape without
    # compaction. If the delta is below noise (negative), report 0 and say so.
    comp_leg = next((l for l in valid if l["name"].startswith("compaction-1x")),
                    None)
    comp_base = next((l for l in valid if l["name"].startswith("compaction-baseline-1x")),
                     None)
    cpu_compaction = 0.0
    if comp_leg and cps(comp_leg) is not None:
        cdiff = None
        if comp_base and cps(comp_base) is not None:
            cdiff = cps(comp_leg) - cps(comp_base)
        if comp_leg.get("compactionsMeasured"):
            if cdiff is not None:
                cpu_compaction = max(0.0, cdiff / comp_leg["compactionsMeasured"])
                if cdiff <= 0:
                    units["notes"].append(
                        "compaction CPU delta below measurement noise; set 0")
            else:
                cpu_compaction = 0.0
            units["notes"].append("compaction leg produced %s events"
                                  % comp_leg["compactionsMeasured"])
        else:
            units["notes"].append("compaction leg produced no compaction; set 0")
    else:
        units["notes"].append("no compaction leg; set 0")

    # Disk units from the same 50-turn anchor.
    disk_tool_read = _bytes_per_tool_call(l50_1r or l10_1r or {})
    disk_tool_bash = (_bytes_per_tool_call(l50_5b)
                      if l50_5b else disk_tool_read)
    disk_text_kb = None
    if l50_t1 and l50_t20:
        b1 = l50_t1.get("assistantTextBytes") or 0
        b20 = l50_t20.get("assistantTextBytes") or 0
        turns = l50_t1.get("turnsMeasured") or 1
        disk_text_kb = max(0.0, (b20 - b1) / (19.0 * turns))
    if disk_text_kb is None:
        disk_text_kb = _bytes_per_text_kb(l50_1r or l10_1r or {}) or 0.0

    disk_turn_base = 0.0
    anchor = l50_1r or l10_1r
    if anchor:
        per_turn = _bytes_per_turn(anchor)
        tool_part = ((anchor.get("toolCallsMeasured") or 0)
                     / max(1, anchor.get("turnsMeasured") or 1))
        if per_turn is not None:
            disk_turn_base = max(0.0, per_turn
                                 - tool_part * (disk_tool_read or 0)
                                 - 5.0 * disk_text_kb)

    disk_compaction = None
    if comp_leg and comp_leg.get("compactionsMeasured"):
        disk_compaction = comp_leg.get("compactionBytes", 0) / comp_leg["compactionsMeasured"]
    if disk_compaction is None:
        disk_compaction = 30_000.0
        units["notes"].append("compaction bytes fallback 30 KB")

    # RSS units from the longest no-compaction single-session leg (50 turns
    # reaches a steadier state than 10; fall back to 10 when it is missing).
    rss_leg = l50_5r or l10_5r
    rss_candidates = [v for v in (
        _median_final_rss(rss_leg) if rss_leg else None,
        _median_final_rss(dense) if dense else None,
    ) if v is not None]
    rss_peak_candidates = [v for v in (
        _median_tree_peak(rss_leg) if rss_leg else None,
        _median_tree_peak(dense) if dense else None,
    ) if v is not None]
    rss_base = max(rss_candidates) if rss_candidates else None
    rss_peak = max(rss_peak_candidates) if rss_peak_candidates else None
    rss_slope = 0.0
    if l10_5r and l50_5r:
        a = _median_final_rss(l10_5r)
        b = _median_final_rss(l50_5r)
        if a is not None and b is not None:
            rss_slope = max(0.0, (b - a) / 40.0)
    units.update({
        "cpuStartupSeconds": round(startup, 4),
        "cpuBaseTurnSeconds": round(cpu_base_turn, 5),
        "cpuToolReadSeconds": round(tool_read or 0.0, 5),
        "cpuToolBashSeconds": round(tool_bash or 0.0, 5),
        "cpuToolReadDenseSeconds": round(cpu_tool_read_dense, 5)
        if cpu_tool_read_dense is not None else None,
        "cpuToolBashDenseSeconds": round(cpu_tool_bash_dense, 5)
        if cpu_tool_bash_dense is not None else None,
        "denseTurnWallSeconds": (round(dense["wallSeconds"] / max(1, dense["turnsPerSession"]), 3)
                                 if dense and dense.get("wallSeconds") else None),
        "cpuTextKbSeconds": round(text_kb_cost, 7),
        "cpuCompactionSeconds": round(cpu_compaction, 4),
        "diskTurnBaseBytes": round(disk_turn_base, 1),
        "diskToolReadBytes": round(disk_tool_read or 0.0, 1),
        "diskToolBashBytes": round(disk_tool_bash or 0.0, 1),
        "diskTextKbBytes": round(disk_text_kb, 1),
        "diskCompactionBytes": round(disk_compaction, 1),
        "rssProcessSteadyMb": round(rss_base, 1) if rss_base is not None else None,
        "rssProcessPeakTreeMb": round(rss_peak, 1) if rss_peak is not None else None,
        "rssGrowthPerTurnMb": round(rss_slope, 5),
        "threadsPerProcess": (round(statistics.median(
            [p.get("threadsPeak") for p in rss_leg.get("processes", [])
             if p.get("threadsPeak")]), 1)
            if rss_leg and rss_leg.get("processes") else None),
        "handlesPerProcess": (round(statistics.median(
            [p.get("handlesPeak") for p in rss_leg.get("processes", [])
             if p.get("handlesPeak")]), 1)
            if rss_leg and rss_leg.get("processes") else None),
        "childrenPerProcess": (rss_leg.get("maxChildrenPerRoot") if rss_leg else 0),
    })
    return units


# ---------------------------------------------------------------------------
# Extrapolation
# ---------------------------------------------------------------------------

def extrapolate(units, target, scenario):
    S = target["sessions"]
    T = target["turns"]
    C = target["toolCalls"]
    K = target["textKb"]
    Z = scenario.get("compactions", target["compactions"])
    bash_fraction = scenario.get("bashFraction", target.get("bashFraction", 0.2))
    # At the target's high tool density the dense 100-tool anchor is the
    # relevant per-call cost; at low density the 5-tool marginal is.
    dense_read = units.get("cpuToolReadDenseSeconds")
    if C >= 20 and dense_read is not None:
        read_cpu = dense_read
        bash_cpu = units.get("cpuToolBashDenseSeconds") or dense_read
    else:
        read_cpu = units.get("cpuToolReadSeconds") or 0
        bash_cpu = units.get("cpuToolBashSeconds") or read_cpu
    tool_cpu = bash_fraction * bash_cpu + (1.0 - bash_fraction) * read_cpu
    tool_cpu *= scenario.get("toolCpuScale", 1.0)
    turn_cpu = ((units.get("cpuBaseTurnSeconds") or 0)
                + C * tool_cpu
                + K * (units.get("cpuTextKbSeconds") or 0))
    cpu_total = S * ((units.get("cpuStartupSeconds") or 0) + T * turn_cpu
                     + Z * (units.get("cpuCompactionSeconds") or 0))
    cpu_per_turn_per_session = turn_cpu + Z * (units.get("cpuCompactionSeconds") or 0) / max(1, T)
    turn_latency = scenario.get("turnLatencySeconds", target.get("turnLatencySeconds", 2.0))
    cpu_pct_one_core = 100.0 * cpu_per_turn_per_session / turn_latency if turn_latency else None
    cores_needed = S * cpu_per_turn_per_session / turn_latency if turn_latency else None
    dense_wall = units.get("denseTurnWallSeconds")
    cores_at_dense_wall = (S * cpu_per_turn_per_session / dense_wall
                           if dense_wall else None)

    tool_bytes = (bash_fraction * (units.get("diskToolBashBytes") or 0)
                  + (1.0 - bash_fraction) * (units.get("diskToolReadBytes") or 0))
    tool_bytes *= scenario.get("toolBytesScale", 1.0)
    session_bytes = ((units.get("diskTurnBaseBytes") or 0)
                     + C * tool_bytes
                     + K * (units.get("diskTextKbBytes") or 0)) * T \
        + Z * (units.get("diskCompactionBytes") or 0)
    fixed_session = 28_000.0  # v3 header + persisted system prompt (measured)
    session_bytes += fixed_session
    disk_total = S * session_bytes

    rss_model = scenario.get("rssModel", "process")
    if rss_model == "process":
        per_session = (units.get("rssProcessSteadyMb") or 0) * scenario.get("rssScale", 1.0)
        rss_total = S * per_session
        rss_peak = S * ((units.get("rssProcessPeakTreeMb") or 0)
                        * scenario.get("rssPeakScale", 1.0))
    else:
        base = units.get("rssProcessSteadyMb") or 0
        per_session = scenario.get("serverSessionMb", 2.0)
        rss_total = base + S * per_session
        rss_peak = rss_total * 1.2
    threads = (units.get("threadsPerProcess") or 0) * (S if rss_model == "process" else 1) \
        * scenario.get("threadScale", 1.0)
    handles = (units.get("handlesPerProcess") or 0) * (S if rss_model == "process" else 1) \
        * scenario.get("handleScale", 1.0)
    processes = S * (1 + (units.get("childrenPerProcess") or 0)
                     * scenario.get("childScale", 1.0)) if rss_model == "process" else 1

    write_bytes = disk_total * scenario.get("writeAmplification", 1.0)
    wall = T * turn_latency
    # One append per persisted JSONL entry: user + assistant text + one
    # assistant toolCall + one toolResult per tool call, plus compaction
    # entries. This is the append-only session store's write-operation count.
    write_ops = S * (T * (2 + 2 * C) + Z)
    return {
        "scenario": scenario["name"],
        "assumptions": scenario,
        "target": target,
        "cpu": {
            "totalCoreSeconds": round(cpu_total, 1),
            "totalCoreHours": round(cpu_total / 3600.0, 2),
            "cpuPerTurnPerSessionSeconds": round(cpu_per_turn_per_session, 5),
            "cpuPctOneCorePerSession": round(cpu_pct_one_core, 1) if cpu_pct_one_core else None,
            "coresNeededAtTurnLatency": round(cores_needed, 2) if cores_needed else None,
            "coresNeededAtDenseWall": round(cores_at_dense_wall, 2)
            if cores_at_dense_wall else None,
            "denseTurnWallSeconds": dense_wall,
            "turnLatencySeconds": turn_latency,
            "toolCpuSeconds": round(tool_cpu, 5),
            "bashFraction": bash_fraction,
        },
        "rss": {
            "model": rss_model,
            "perSessionMb": round(per_session, 1),
            "totalMb": round(rss_total, 0),
            "totalGb": round(rss_total / 1024.0, 2),
            "peakGb": round(rss_peak / 1024.0, 2),
        },
        "disk": {
            "perSessionBytes": round(session_bytes, 0),
            "perSessionMb": round(session_bytes / (1024.0 * 1024.0), 1),
            "totalBytes": round(disk_total, 0),
            "totalGb": round(disk_total / (1024.0 ** 3), 2),
            "writeBytesTotalGb": round(write_bytes / (1024.0 ** 3), 2),
            "peakEqualsSteady": True,
            "tempFilesMeasured": 0,
            "wallSecondsAtLatency": round(wall, 0),
            "writeBytesPerSecond": round(write_bytes / wall, 0) if wall else None,
            "writeOpsTotal": write_ops,
            "writeOpsPerSecond": round(write_ops / wall, 1) if wall else None,
        },
        "threads": {"total": round(threads, 0)},
        "handles": {"total": round(handles, 0)},
        "processes": {"peak": round(processes, 0)},
    }


def default_scenarios(target, units):
    """Optimistic / typical / pessimistic, with explicit differing assumptions."""
    return [
        {
            "name": "optimistic",
            "label": "单进程 server 托管 100 会话；工具 CPU ×0.5（工具输出 0.5 KB、上下文更短）；每会话常驻 1.5 MB（未实测假设）",
            "rssModel": "server", "serverSessionMb": 1.5,
            "toolCpuScale": 0.5, "toolBytesScale": 0.5,
            "rssScale": 0.85, "rssPeakScale": 0.85,
            "bashFraction": 0.1, "turnLatencySeconds": 2.0,
            "childScale": 1.0, "threadScale": 1.0, "handleScale": 1.0,
            "writeAmplification": 1.0,
        },
        {
            "name": "typical",
            "label": "100 个 pir 进程；实测 100-tool 稠密锚点；工具输出 1 KB；20% bash；每会话实测 RSS 96 MB（50-turn 稳态）",
            "rssModel": "process",
            "toolCpuScale": 1.0, "toolBytesScale": 1.0,
            "rssScale": 1.0, "rssPeakScale": 1.0,
            "bashFraction": 0.2, "turnLatencySeconds": 2.0,
            "childScale": 1.0, "threadScale": 1.0, "handleScale": 1.0,
            "writeAmplification": 1.0,
        },
        {
            "name": "pessimistic",
            "label": "100 个 pir 进程；工具 CPU ×1.8（工具输出 2 KB、上下文约翻倍）；工具输出 2 KB；压缩 150 次；RSS 峰值 ×1.15",
            "rssModel": "process",
            "toolCpuScale": 1.8, "toolBytesScale": 2.0,
            "rssScale": 1.15, "rssPeakScale": 1.15,
            "bashFraction": 0.35, "turnLatencySeconds": 2.0,
            "childScale": 1.5, "threadScale": 1.1, "handleScale": 1.1,
            "writeAmplification": 1.0, "compactionsOverride": 150,
        },
    ]


# ---------------------------------------------------------------------------
# Report rendering
# ---------------------------------------------------------------------------

def _fmt(value, digits=1, unit=""):
    if value is None:
        return "N/A"
    if isinstance(value, float):
        return f"{value:,.{digits}f}{unit}"
    return f"{value:,}{unit}"


def render_markdown(results):
    lines = []
    add = lines.append
    meta = results["meta"]
    target = results["target"]
    units = results["units"]
    legs = results["legs"]
    add("# pir fleet resource simulation — measured units + extrapolation")
    add("")
    add(f"Generated `{meta['generated']}` by `scripts/perf/fleet_simulation.py`.")
    add("")
    add(f"- Binary: `{meta['pir']['path']}` ({_fmt(meta['pir']['size'])} B, "
        f"mtime `{meta['pir']['mtime']}`, version `{meta['pir']['version']}`)")
    add(f"- Host: {meta['host']['cpu']} — {meta['host']['cores']} cores / "
        f"{meta['host']['logical']} logical, {_fmt(meta['host']['ramGb'])} GB RAM, "
        f"load at start {meta['host']['loadStart']}% (background noise recorded per leg)")
    add(f"- Target: **{target['sessions']} sessions × {target['turns']} turns × "
        f"{target['toolCalls']} tool calls/turn × {target['textKb']} KB text/turn × "
        f"{target['compactions']} compactions/session** = "
        f"{target['sessions']*target['turns']:,} turns and "
        f"{target['sessions']*target['turns']*target['toolCalls']:,} tool calls.")
    add("")
    add("> **Measured vs extrapolated.** Every row under “Measured legs” was produced by "
        "a real `pir` process against the offline mock. Rows under “Extrapolated” are "
        "computed from the fitted unit costs and are labelled with their scenario; they "
        "are not measurements.")
    add("")
    add("## Measured experiment matrix")
    add("")
    add("| Leg | Sessions | Turns | Tools/turn | Tool kind | Text KB | Wall s | Job CPU s | "
        "CPU s/turn | CPU % 1 core/proc | RSS steady MB/proc | Tree peak MB/proc | "
        "Threads | Handles | Disk/session KB | Compactions | Load% median/max | Valid |")
    add("|---|---:|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|---|")
    for leg in legs:
        load = leg.get("loadPercent") or {}
        add(f"| {leg['name']} | {leg['sessions']} | {leg['turnsPerSession']} | "
            f"{leg['toolCallsPerTurn']} | {leg['toolKind']} | {leg['textKb']} | "
            f"{_fmt(leg['wallSeconds'],1)} | {_fmt(leg.get('jobCpuSeconds'),2)} | "
            f"{_fmt(leg.get('cpuSecondsPerTurn'),4)} | "
            f"{_fmt(leg.get('cpuPctOneCorePerProcess'),1)} | "
            f"{_fmt(leg.get('rssLastMedianMb'),1)} | {_fmt(leg.get('treePeakMedianMb'),1)} | "
            f"{_fmt(leg.get('threadsPeakMedian'),0)} | {_fmt(leg.get('handlesPeakMedian'),0)} | "
            f"{_fmt((leg.get('sessionBytesPerSession') or 0)/1024.0,1)} | "
            f"{leg.get('compactionsMeasured')} | {load.get('median')}/{load.get('max')} | "
            f"{'yes' if leg.get('valid') else 'NO: ' + '; '.join(leg.get('problems', []))} |")
    add("")
    add("Raw artifacts: each leg writes `legs/<name>/leg.json`, session JSONL under "
        "`legs/<name>/sessions/`, stdout event streams under `legs/<name>/stdout/`, and "
        "stderr under `legs/<name>/stderr/`.")
    add("")
    add("## Fitted unit costs")
    add("")
    add("| Unit | Value | Basis |")
    add("|---|---:|---|")
    add(f"| Process startup CPU | {_fmt(units.get('cpuStartupSeconds'),3)} s | 10 vs 50-turn pair |")
    add(f"| Per turn CPU (no tool/text) | {_fmt(units.get('cpuBaseTurnSeconds'),5)} s | fit |")
    add(f"| Per `read` tool call CPU | {_fmt(units.get('cpuToolReadSeconds'),5)} s | 1 vs 5 tools |")
    add(f"| Per `bash` tool call CPU | {_fmt(units.get('cpuToolBashSeconds'),5)} s | 5 bash tools |")
    add(f"| Per KB text CPU | {_fmt(units.get('cpuTextKbSeconds'),7)} s | 1 vs 20 KB |")
    add(f"| Per compaction CPU | {_fmt(units.get('cpuCompactionSeconds'),4)} s | compaction leg |")
    add(f"| Per turn session bytes | {_fmt(units.get('diskTurnBaseBytes'),0)} B | fit |")
    add(f"| Per `read` tool bytes | {_fmt(units.get('diskToolReadBytes'),0)} B | tool entries |")
    add(f"| Per `bash` tool bytes | {_fmt(units.get('diskToolBashBytes'),0)} B | tool entries |")
    add(f"| Per KB text bytes | {_fmt(units.get('diskTextKbBytes'),0)} B | text entries |")
    add(f"| Per compaction bytes | {_fmt(units.get('diskCompactionBytes'),0)} B | compaction entries |")
    add(f"| Per-process steady RSS | {_fmt(units.get('rssProcessSteadyMb'),1)} MB | 50-turn steady state |")
    add(f"| Per-process tree peak | {_fmt(units.get('rssProcessPeakTreeMb'),1)} MB | max(50-turn, 100-tool dense) |")
    add(f"| RSS growth/turn | {_fmt(units.get('rssGrowthPerTurnMb'),5)} MB | 10 vs 50 turns |")
    add(f"| Threads/process | {_fmt(units.get('threadsPerProcess'),0)} | median peak |")
    add(f"| Handles/process | {_fmt(units.get('handlesPerProcess'),0)} | median peak |")
    add("")
    if units.get("notes"):
        add("Fit notes: " + "; ".join(units["notes"]))
        add("")
    add("## Extrapolated target (100 × 1,000 × 100)")
    add("")
    add("| Scenario | CPU core-hours | CPU % one core/session @ latency | Cores @ 2 s/turn | Cores @ measured dense wall | RSS total GB | RSS peak GB | Disk GB | Disk/session MB | Threads | Handles | Peak processes |")
    add("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
    for scenario in results["scenarios"]:
        cpu, rss, disk = scenario["cpu"], scenario["rss"], scenario["disk"]
        add(f"| {scenario['scenario']} | {_fmt(cpu['totalCoreHours'],1)} | "
            f"{_fmt(cpu['cpuPctOneCorePerSession'],1)} | {_fmt(cpu['coresNeededAtTurnLatency'],2)} | "
            f"{_fmt(cpu.get('coresNeededAtDenseWall'),2)} | "
            f"{_fmt(rss['totalGb'],2)} | {_fmt(rss['peakGb'],2)} | {_fmt(disk['totalGb'],2)} | "
            f"{_fmt(disk['perSessionMb'],1)} | {_fmt(scenario['threads']['total'],0)} | "
            f"{_fmt(scenario['handles']['total'],0)} | {_fmt(scenario['processes']['peak'],0)} |")
    add("")
    add("Scenario assumptions:")
    add("")
    for scenario in results["scenarios"]:
        add(f"- **{scenario['scenario']}** — {scenario['assumptions'].get('label', '')}")
    add("")
    typical = next((s for s in results["scenarios"] if s["scenario"] == "typical"), None)
    if typical:
        per_turn = typical["cpu"]["cpuPerTurnPerSessionSeconds"]
        add("CPU demand depends on the wall time per turn (provider + tool latency). "
            "Same total core-hours, different core count:")
        add("")
        add("| Assumed turn wall | Cores for 100 sessions | CPU % of one core per session |")
        add("|---:|---:|---:|")
        for seconds in (2.0, 6.6, 20.0, 60.0):
            add(f"| {seconds:.1f} s | {per_turn * 100.0 / seconds:.1f} | "
                f"{100.0 * per_turn / seconds:.1f}% |")
        add("")
    add("## Bottleneck ranking (typical scenario)")
    add("")
    for row in results["bottlenecks"]:
        add(f"- **{row['resource']}**: {row['value']} — {row['verdict']}")
    add("")
    return "\n".join(lines) + "\n"


def bottleneck_rows(results, host):
    typical = next((s for s in results["scenarios"] if s["scenario"] == "typical"),
                   results["scenarios"][0])
    rows = []
    cores = typical["cpu"].get("coresNeededAtDenseWall")         or typical["cpu"]["coresNeededAtTurnLatency"]
    wall = typical["cpu"].get("denseTurnWallSeconds")
    rows.append({
        "resource": "CPU",
        "value": f"{typical['cpu']['totalCoreHours']} core-hours; "
                 f"{cores} cores at the measured {wall}s/turn dense wall",
        "verdict": ("first bottleneck: %0.1f× the %d logical cores"
                    % (cores / host["logical"], host["logical"]))
        if cores and cores > host["logical"] else "fits the host",
    })
    rss = typical["rss"]
    rows.append({
        "resource": "Memory",
        "value": f"{rss['totalGb']} GB total, {rss['peakGb']} GB peak",
        "verdict": ("exceeds" if rss["totalGb"] > host["ramGb"] else "fits")
                   + f" the {host['ramGb']} GB host",
    })
    disk = typical["disk"]
    rows.append({
        "resource": "Disk",
        "value": f"{disk['totalGb']} GB total, {disk['perSessionMb']} MB/session",
        "verdict": f"host has {host.get('diskFreeGb')} GB free",
    })
    rows.append({
        "resource": "Threads",
        "value": f"{typical['threads']['total']:,}",
        "verdict": "not a hard Windows limit",
    })
    rows.append({
        "resource": "Handles",
        "value": f"{typical['handles']['total']:,}",
        "verdict": "below the system handle-table limit",
    })
    return rows


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def parse_matrix(spec):
    legs = []
    for item in spec.split(","):
        item = item.strip()
        if not item:
            continue
        match = re.match(r"^(\d+)x(\d+)x(\d+)([rbm]?)(?:t(\d+(?:\.\d+)?))?$", item)
        if not match:
            raise SystemExit("bad --matrix item: %r (want SxTxC[rbm][tK])" % item)
        sessions, turns, tools = int(match.group(1)), int(match.group(2)), int(match.group(3))
        kind = {"r": "read", "b": "bash", "m": "mix", "": "read"}[match.group(4)]
        text_kb = float(match.group(5)) if match.group(5) else None
        legs.append({"sessions": sessions, "turns": turns, "tools": tools,
                     "kind": kind, "textKb": text_kb})
    return legs


def main(argv=None):
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--pir", default=os.path.join(REPO, "target", "debug", "pir.exe"))
    ap.add_argument("--out", default=os.path.join(REPO, "logs", "lane-fleet", "fleet-run"))
    ap.add_argument("--sessions", type=int, default=100)
    ap.add_argument("--turns", type=int, default=1000)
    ap.add_argument("--tool-calls", type=int, default=100)
    ap.add_argument("--text-kb", type=float, default=5.0)
    ap.add_argument("--compactions", type=int, default=100)
    ap.add_argument("--tool-kb", type=float, default=1.0,
                    help="assumed output size per tool call for the extrapolation")
    ap.add_argument("--bash-fraction", type=float, default=0.2)
    ap.add_argument("--turn-latency-ms", type=float, default=2000.0,
                    help="assumed wall time per turn (provider+tool latency) for core sizing")
    ap.add_argument("--matrix", default="1x10x1r,1x10x5r,1x10x5b,"
                                        "1x10x100r,"
                                        "1x50x1r,1x50x5r,1x50x5b,"
                                        "1x50x1t1,1x50x1t20,"
                                        "4x10x1r,4x10x5r,10x10x1r,10x10x5r")
    ap.add_argument("--concurrency", default="20,50",
                    help="comma list of process counts for the concurrency probe; empty to skip")
    ap.add_argument("--conc-turns", type=int, default=10)
    ap.add_argument("--conc-tools", type=int, default=1)
    ap.add_argument("--context-window", type=int, default=1_000_000,
                    help="unit legs use a large window so they do not compact")
    ap.add_argument("--reserve-tokens", type=int, default=16384)
    ap.add_argument("--keep-recent-tokens", type=int, default=20000)
    ap.add_argument("--compaction-window", type=int, default=16000)
    ap.add_argument("--compaction-reserve", type=int, default=4096)
    ap.add_argument("--compaction-keep", type=int, default=4096)
    ap.add_argument("--compaction-leg-turns", type=int, default=40)
    ap.add_argument("--timeout", type=float, default=None)
    ap.add_argument("--from-results", default=None,
                    help="re-extrapolate from an existing fleet_results.json (no runs)")
    ap.add_argument("--only-target", action="store_true",
                    help="skip real runs; require --from-results (target math only)")
    args = ap.parse_args(argv)

    if os.name != "nt":
        print("ERROR: fleet_simulation.py is Windows-only (Toolhelp32/Job objects)",
              file=sys.stderr)
        return 2
    pir = os.path.abspath(args.pir)
    target = {
        "sessions": args.sessions, "turns": args.turns,
        "toolCalls": args.tool_calls, "textKb": args.text_kb,
        "compactions": args.compactions, "toolKb": args.tool_kb,
        "bashFraction": args.bash_fraction,
        "turnLatencySeconds": args.turn_latency_ms / 1000.0,
    }

    out_dir = os.path.abspath(args.out)
    os.makedirs(out_dir, exist_ok=True)

    if args.from_results:
        with open(args.from_results, "r", encoding="utf-8") as fh:
            previous = json.load(fh)
        legs = previous.get("legs", [])
        units = previous.get("units") or fit_units(legs)
        meta = previous.get("meta", {})
        results = {
            "meta": meta, "target": target, "units": units, "legs": legs,
            "scenarios": [], "bottlenecks": [],
        }
        # Recompute scenarios with the new target.
        for scenario in default_scenarios(target, units):
            scenario["compactions"] = scenario.pop("compactionsOverride",
                                                   target["compactions"])
            results["scenarios"].append(extrapolate(units, target, scenario))
        results["bottlenecks"] = bottleneck_rows(results, meta.get("host", {}))
        with open(os.path.join(out_dir, "fleet_results.json"), "w", encoding="utf-8") as fh:
            json.dump(results, fh, indent=2)
        with open(os.path.join(out_dir, "FLEET.md"), "w", encoding="utf-8", newline="\n") as fh:
            fh.write(render_markdown(results))
        print("[fleet] re-extrapolated from %s -> %s" % (args.from_results, out_dir))
        return 0

    if not os.path.isfile(pir):
        print("ERROR: pir not found: %s" % pir, file=sys.stderr)
        return 2

    # Host facts.
    import platform
    host = {
        "cpu": platform.processor() or "unknown",
        "cores": os.cpu_count() or 1,
        "logical": os.cpu_count() or 1,
        "ramGb": None,
        "diskFreeGb": None,
        "loadStart": system_load_percent(),
    }
    try:
        out = subprocess.run(
            ["powershell", "-NoProfile", "-NonInteractive", "-Command",
             "Get-CimInstance Win32_Processor | Select-Object -First 1 "
             "-ExpandProperty NumberOfCores; "
             "Get-CimInstance Win32_ComputerSystem | Select-Object -ExpandProperty TotalPhysicalMemory"],
            capture_output=True, text=True, timeout=20).stdout.split()
        if len(out) >= 2:
            host["cores"] = int(out[0])
            host["ramGb"] = round(int(out[1]) / (1024.0 ** 3), 1)
    except Exception:
        pass
    try:
        out = subprocess.run(["powershell", "-NoProfile", "-NonInteractive", "-Command",
                              "(Get-PSDrive C).Free"], capture_output=True, text=True,
                             timeout=20).stdout.strip()
        if out:
            host["diskFreeGb"] = round(int(out) / (1024.0 ** 3), 1)
    except Exception:
        pass
    host["logical"] = os.cpu_count() or host["cores"]

    print("[fleet] pir=%s size=%s mtime=%s" % (
        pir, os.path.getsize(pir),
        time.strftime("%Y-%m-%dT%H:%M:%S", time.localtime(os.path.getmtime(pir)))))
    print("[fleet] host cores=%s logical=%s ram=%sGB free=%sGB load=%s%%" % (
        host["cores"], host["logical"], host["ramGb"], host["diskFreeGb"],
        host["loadStart"]))

    agent_dir = os.path.join(out_dir, "agent")
    os.makedirs(agent_dir, exist_ok=True)
    legs = []
    mock = None
    try:
        # Build the mock once per tool kind/text shape (the server state is
        # shared; per-leg counters are read from the difference).
        def run_with_mock(name, sessions, turns, tools, kind, text_kb,
                          context_window, reserve, keep, timeout=None):
            nonlocal mock
            if mock is not None:
                mock.stop()
            mock = FleetMock(tools_per_turn=max(1, tools), tool_kind=kind,
                             tool_kb=args.tool_kb,
                             response_chars=int(text_kb * 1024)).start()
            print("[fleet] leg %s: %dx%dx%d %s text=%.0fKB ..." %
                  (name, sessions, turns, tools, kind, text_kb), flush=True)
            leg = run_leg(name, args, mock, agent_dir, sessions, turns,
                          max(1, tools), text_kb, kind, context_window,
                          reserve, keep, out_dir, timeout=timeout)
            print("[fleet]   wall=%.1fs cpu=%ss rss=%.1fMB tree=%.1fMB "
                  "threads=%s handles=%s disk/session=%.1fKB valid=%s" % (
                      leg["wallSeconds"], leg.get("jobCpuSeconds"),
                      leg.get("rssLastMedianMb") or 0,
                      leg.get("treePeakMedianMb") or 0,
                      leg.get("threadsPeakMedian"), leg.get("handlesPeakMedian"),
                      (leg.get("sessionBytesPerSession") or 0) / 1024.0,
                      leg["valid"]), flush=True)
            legs.append(leg)
            return leg

        for item in parse_matrix(args.matrix):
            text_kb = item["textKb"] if item["textKb"] is not None else args.text_kb
            name = "unit-%dx%dx%d-%s" % (item["sessions"], item["turns"],
                                         item["tools"], item["kind"])
            if item["textKb"] is not None:
                name += "-t%g" % text_kb
            run_with_mock(
                name, item["sessions"], item["turns"], item["tools"], item["kind"],
                text_kb, args.context_window, args.reserve_tokens,
                args.keep_recent_tokens, timeout=args.timeout)

        # Compaction leg + its identical no-compaction baseline.
        comp_turns = args.compaction_leg_turns
        run_with_mock("compaction-baseline-1x%dx1" % comp_turns, 1, comp_turns, 1,
                      "read", args.text_kb, args.context_window,
                      args.reserve_tokens, args.keep_recent_tokens,
                      timeout=args.timeout)
        run_with_mock("compaction-1x%dx1" % comp_turns, 1, comp_turns, 1,
                      "read", args.text_kb, args.compaction_window,
                      args.compaction_reserve, args.compaction_keep,
                      timeout=args.timeout)

        for count in [int(x) for x in args.concurrency.split(",") if x.strip()]:
            run_with_mock("conc-%dx%dx%d" % (count, args.conc_turns, args.conc_tools),
                          count, args.conc_turns, args.conc_tools, "read",
                          args.text_kb, args.context_window, args.reserve_tokens,
                          args.keep_recent_tokens, timeout=args.timeout)
    finally:
        if mock is not None:
            mock.stop()

    units = fit_units(legs)
    results = {
        "meta": {
            "generated": time.strftime("%Y-%m-%d %H:%M:%S"),
            "pir": {
                "path": pir,
                "size": os.path.getsize(pir),
                "mtime": time.strftime("%Y-%m-%dT%H:%M:%S",
                                       time.localtime(os.path.getmtime(pir))),
                "version": subprocess.run(
                    [pir, "--version"], capture_output=True, text=True,
                    timeout=15, stdin=subprocess.DEVNULL).stdout.strip().splitlines()[0]
                if os.path.isfile(pir) else None,
            },
            "host": host,
            "args": vars(args),
        },
        "target": target,
        "units": units,
        "legs": legs,
        "scenarios": [],
        "bottlenecks": [],
    }
    for scenario in default_scenarios(target, units):
        scenario["compactions"] = scenario.pop("compactionsOverride",
                                               target["compactions"])
        results["scenarios"].append(extrapolate(units, target, scenario))
    results["bottlenecks"] = bottleneck_rows(results, host)

    with open(os.path.join(out_dir, "fleet_results.json"), "w", encoding="utf-8") as fh:
        json.dump(results, fh, indent=2)
    with open(os.path.join(out_dir, "FLEET.md"), "w", encoding="utf-8", newline="\n") as fh:
        fh.write(render_markdown(results))
    print("[fleet] wrote %s and %s" % (
        os.path.join(out_dir, "fleet_results.json"),
        os.path.join(out_dir, "FLEET.md")))
    if any(not leg["valid"] for leg in legs):
        return 3
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
