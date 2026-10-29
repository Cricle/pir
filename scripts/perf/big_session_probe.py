#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Does loading a HUGE session blow up? Synthesize a ~N MB v3 session JSONL,
resume it non-interactively (`--session <file>`) against a LOCAL mock provider,
and sample peak RSS for both CLIs (pir and upstream pi, same on-disk format).

Usage:
  python scripts/perf/big_session_probe.py --size-mb 100                 # both CLIs
  python scripts/perf/big_session_probe.py --size-mb 100 --only pir
  python scripts/perf/big_session_probe.py --size-mb 100 --context-window 100000000
  # the gate (PLAN success criterion 3): 100 MB increment over a 2 MB baseline
  python scripts/perf/big_session_probe.py --size-mb 100 --baseline-size-mb 2 --only pir

The mock writes the request body size it received, so a "load + one turn" run
also shows whether the whole transcript was serialized into the request.

Gate contract (see GATE-RUNBOOK §1 and MEMORY-PROBE-NOTES.md): the two legs of
one --only target are run against the SAME binary, the SAME agent dir (so the
same context window / compaction reserve) and the SAME prompt; only the session
file differs. The verdict is the peak-RSS INCREMENT the big session adds over
the baseline leg, and the process EXIT CODE carries it (`--budget-mb`, default
20): 0 within, 1 over, 3 unmeasurable leg (never a pass). Every leg records the
binary identity (path, size, mtime, sha256, --version) so no verdict can be
detached from the binary that produced it.
"""

import argparse
import hashlib
import http.server
import json
import os
import re
import shutil
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import uuid

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))

STATE = {"bodies": []}


def binary_identity(path):
    """Identity of the executable a verdict came from (runbook §3: "a number
    without the binary it came from is not evidence").

    Contract: `mtime` is the build stamp the foreman compares against the
    source tree; `sha256` distinguishes two builds that happen to share an
    mtime. Failure to hash is recorded as None and never aborts the leg.
    """
    ident = {"path": os.path.abspath(path), "size": None, "mtime": None,
             "sha256": None, "version": None}
    try:
        st = os.stat(path)
        ident["size"] = st.st_size
        ident["mtime"] = time.strftime("%Y-%m-%dT%H:%M:%S", time.localtime(st.st_mtime))
    except OSError:
        return ident
    try:
        digest = hashlib.sha256()
        with open(path, "rb") as fh:
            for chunk in iter(lambda: fh.read(1 << 20), b""):
                digest.update(chunk)
        ident["sha256"] = digest.hexdigest()
    except OSError:
        pass
    try:
        done = subprocess.run([os.path.abspath(path), "--version"],
                              capture_output=True, text=True, timeout=15,
                              stdin=subprocess.DEVNULL)
        if done.returncode == 0 and (done.stdout or "").strip():
            ident["version"] = done.stdout.strip().splitlines()[0]
    except Exception:
        pass
    return ident


def staleness_note(binary_path, repo=REPO):
    """Compare the executable's mtime with the newest Rust build input.

    Contract (GATE-RUNBOOK §0 and §4: "a run whose binary predates the change
    under test is a smoke test, not a measurement"): every probe prints this
    note next to its verdict so a stale run is labelled where the number is
    read. It is a WARNING, not an exit code: lanes editing `src/` during a wave
    legitimately leave files newer than the foreman's build, and failing on
    that would let a harmless parallel edit block a real gate. The foreman's
    obligation is the rebuild *before* the run, not a harness enforcement.
    Returns None when either side cannot be stat'ed.
    """
    newest_path, newest = None, None
    candidates = [os.path.join(repo, "Cargo.toml"), os.path.join(repo, "Cargo.lock")]
    src = os.path.join(repo, "src")
    if os.path.isdir(src):
        for dirpath, _dirnames, filenames in os.walk(src):
            for name in filenames:
                if name.endswith(".rs"):
                    candidates.append(os.path.join(dirpath, name))
    for path in candidates:
        try:
            mtime = os.path.getmtime(path)
        except OSError:
            continue
        if newest is None or mtime > newest:
            newest_path, newest = path, mtime
    try:
        built = os.path.getmtime(binary_path)
    except OSError:
        return None
    if newest is None:
        return None
    return {
        "stale": built < newest,
        "binaryMtime": time.strftime("%Y-%m-%dT%H:%M:%S", time.localtime(built)),
        "newestSource": os.path.relpath(newest_path, repo),
        "newestSourceMtime": time.strftime("%Y-%m-%dT%H:%M:%S", time.localtime(newest)),
    }


def _sse(payload):
    return ("data: " + json.dumps(payload) + "\n\n").encode("utf-8")


class _Mock(http.server.BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def do_GET(self):
        body = json.dumps({"object": "list", "data": [{"id": "mock-model", "object": "model"}]})
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body.encode("utf-8"))

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        # Read the body without parsing it (a 100 MB transcript would make the
        # python-side parse dominate the measurement); record the size only.
        remaining = length
        while remaining > 0:
            chunk = self.rfile.read(min(1 << 20, remaining))
            if not chunk:
                break
            remaining -= len(chunk)
        STATE["bodies"].append(length)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        created = int(time.time())
        try:
            for delta in ({"role": "assistant", "content": ""}, {"content": "OK"}):
                self.wfile.write(_sse({
                    "id": "mock-1", "object": "chat.completion.chunk", "created": created,
                    "model": "mock-model",
                    "choices": [{"index": 0, "delta": delta, "finish_reason": None}],
                }))
            self.wfile.write(_sse({
                "id": "mock-1", "object": "chat.completion.chunk", "created": created,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
            }))
            self.wfile.write(b"data: [DONE]\n\n")
        except (BrokenPipeError, ConnectionResetError):
            return


class Mock(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def make_session(path, size_mb, cwd):
    """Write a valid v3 JSONL session of roughly `size_mb` MB (user/assistant
    text pairs; each assistant message carries a text block)."""
    target = size_mb * 1024 * 1024
    written = 0
    session_id = str(uuid.uuid4())
    stamp = "2026-01-01T00:00:00.000Z"
    payload = ("lorem ipsum dolor sit amet consectetur adipiscing elit " * 20).strip()
    with open(path, "w", encoding="utf-8", newline="\n") as fh:
        header = {"type": "session", "version": 3, "id": session_id,
                  "timestamp": stamp, "cwd": cwd}
        line = json.dumps(header) + "\n"
        fh.write(line)
        written += len(line)
        # A real session starts with model/thinking bookkeeping entries.
        for extra in ({"type": "model_change", "id": "m0", "parentId": None, "timestamp": stamp,
                       "provider": "mock", "modelId": "mock-model"},
                      {"type": "thinking_level_change", "id": "t0", "parentId": "m0",
                       "timestamp": stamp, "thinkingLevel": "off"}):
            line = json.dumps(extra) + "\n"
            fh.write(line)
            written += len(line)
        index = 0
        parent = "t0"
        # Growing, realistic usage: the compaction estimate reads the last
        # assistant message's input tokens; all-zero usage (a synthetic file)
        # makes the estimator believe the context is empty.
        usage = {"input": 1_500_000 + index * 250, "output": 120, "cacheRead": 0,
                 "cacheWrite": 0, "totalTokens": 1_500_000 + index * 250 + 120,
                 "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}}
        while written < target:
            index += 1
            user_id = f"u{index:06d}"
            asst_id = f"a{index:06d}"
            user = {"type": "message", "id": user_id, "parentId": parent, "timestamp": stamp,
                    "message": {"role": "user", "timestamp": 0,
                                "content": [{"type": "text",
                                             "text": f"question {index}: {payload}"}]}}
            # Assistant messages carry the provider bookkeeping a real session
            # has (api/provider/model/stopReason/usage); the reader rejects
            # entries without it.
            asst = {"type": "message", "id": asst_id, "parentId": user_id, "timestamp": stamp,
                    "message": {"role": "assistant", "timestamp": 0,
                                "api": "openai-completions", "provider": "mock",
                                "model": "mock-model", "stopReason": "stop", "usage": usage,
                                "content": [{"type": "text",
                                             "text": f"answer {index}: {payload}"}]}}
            for entry in (user, asst):
                line = json.dumps(entry) + "\n"
                fh.write(line)
                written += len(line)
            parent = asst_id
    return written


def build_agent_dir(port, context_window):
    agent = tempfile.mkdtemp(prefix="pir-big-session-")
    models = {"providers": {"mock": {
        "name": "Mock",
        "baseUrl": f"http://127.0.0.1:{port}/v1",
        "api": "openai-completions",
        "apiKey": "sk-mock-big-session",
        "models": [{
            "id": "mock-model", "name": "Mock", "reasoning": False, "input": ["text"],
            "contextWindow": context_window, "maxTokens": 4096,
        }],
    }}}
    with open(os.path.join(agent, "models.json"), "w", encoding="utf-8") as fh:
        json.dump(models, fh, indent=2)
    settings = {"defaultProvider": "mock", "defaultModel": "mock-model",
                "defaultThinkingLevel": "off"}
    with open(os.path.join(agent, "settings.json"), "w", encoding="utf-8") as fh:
        json.dump(settings, fh, indent=2)
    return agent


def session_id_of(path):
    """The id in the session file's header (the identity a leg must open)."""
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            return json.loads(fh.readline()).get("id")
    except Exception:
        return None


def sample_root_rss_mb(pid):
    """Fast root-only working set (MB) via `tasklist` (~50 ms).

    Contract: the tree total is the measurement, but a CIM query can outlast a
    whole leg on a loaded machine (measured 3-18 s here against the 15 s
    timeout below), which used to leave the leg with ZERO samples and a
    fabricated `peakRssMb=0.0`. This cheap point keeps every leg measured and
    is reported with `scope: "root"` so it is never mistaken for the tree
    number (`rss_probe.py` uses the same instrument).
    """
    try:
        done = subprocess.run(
            ["tasklist", "/FI", f"PID eq {pid}", "/FO", "CSV", "/NH"],
            capture_output=True, text=True, errors="replace", timeout=15)
    except Exception:
        return None
    match = re.search(r'"([\d.,]+) K"', done.stdout or "")
    if not match:
        return None
    try:
        kb = float(match.group(1).replace(",", "").replace(".", ""))
    except ValueError:
        return None
    return kb / 1024.0 if kb > 0 else None


def sample_tree_rss_mb_fast(pid):
    """Process-tree working set (MB) via a ctypes Toolhelp32 snapshot.

    Why this exists (and why it is the DEFAULT tree instrument): the CIM
    fallback below costs 0.7-3 s per query on this host, so a 10 s interactive
    leg produced ONE tree sample and the "peak" was wherever that one query
    happened to land (measured: 51.9 MB baseline vs 52.0 MB target from single
    samples — a number with no peak evidence behind it). A Toolhelp32
    CreateToolhelp32Snapshot walk is a few milliseconds, so a 50 ms cadence
    really samples the rise, the peak and the settle.

    Contract: same quantity as the CIM path — root pid plus every descendant,
    summed `WorkingSetSize` — so the two instruments are interchangeable and
    `TreeSampler` may fall back from one to the other without changing what the
    number means. Stdlib only (ctypes), no new dependency.
    """
    try:
        import ctypes
        from ctypes import wintypes

        TH32CS_SNAPPROCESS = 0x00000002
        INVALID_HANDLE_VALUE = ctypes.c_void_p(-1).value

        class PROCESSENTRY32W(ctypes.Structure):
            _fields_ = [
                ("dwSize", wintypes.DWORD),
                ("cntUsage", wintypes.DWORD),
                ("th32ProcessID", wintypes.DWORD),
                ("th32DefaultHeapID", ctypes.POINTER(ctypes.c_ulong)),
                ("th32ModuleID", wintypes.DWORD),
                ("cntThreads", wintypes.DWORD),
                ("th32ParentProcessID", wintypes.DWORD),
                ("pcPriClassBase", ctypes.c_long),
                ("dwFlags", wintypes.DWORD),
                ("szExeFile", wintypes.WCHAR * 260),
            ]

        class PROCESS_MEMORY_COUNTERS(ctypes.Structure):
            _fields_ = [
                ("cb", wintypes.DWORD),
                ("PageFaultCount", wintypes.DWORD),
                ("PeakWorkingSetSize", ctypes.c_size_t),
                ("WorkingSetSize", ctypes.c_size_t),
                ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                ("PagefileUsage", ctypes.c_size_t),
                ("PeakPagefileUsage", ctypes.c_size_t),
            ]

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        psapi = ctypes.WinDLL("psapi", use_last_error=True)
        kernel32.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
        snap = kernel32.CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
        if not snap or snap == INVALID_HANDLE_VALUE:
            return None
        rows = []
        try:
            entry = PROCESSENTRY32W()
            entry.dwSize = ctypes.sizeof(PROCESSENTRY32W)
            ok = kernel32.Process32FirstW(snap, ctypes.byref(entry))
            while ok:
                rows.append((entry.th32ProcessID, entry.th32ParentProcessID))
                ok = kernel32.Process32NextW(snap, ctypes.byref(entry))
        finally:
            kernel32.CloseHandle(snap)
        if not rows:
            return None
        tree, stack = {pid}, [pid]
        children = {}
        for child_pid, parent_pid in rows:
            children.setdefault(parent_pid, []).append(child_pid)
        while stack:
            for kid in children.get(stack.pop(), []):
                if kid not in tree:
                    tree.add(kid)
                    stack.append(kid)
        total = 0
        for member in tree:
            # PROCESS_QUERY_LIMITED_INFORMATION (0x1000) works without
            # SeDebugPrivilege for our own children; the counter struct must
            # carry its own size (cb) or the call fails.
            handle = kernel32.OpenProcess(0x1000, False, member)
            if not handle:
                continue
            try:
                counters = PROCESS_MEMORY_COUNTERS()
                counters.cb = ctypes.sizeof(PROCESS_MEMORY_COUNTERS)
                if psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters),
                                              counters.cb):
                    total += counters.WorkingSetSize
            finally:
                kernel32.CloseHandle(handle)
        return total / (1024 * 1024) if total > 0 else None
    except Exception:
        return None


def sample_rss_mb(pid, timeout=15):
    """Working set of the WHOLE PROCESS TREE, in MB. `pi.cmd` is a cmd.exe shim,
    so sampling the spawned pid alone measures the shim (9.7 MB measured) instead
    of node.

    Contract: the fast Toolhelp32 walk is tried first (milliseconds, so it can
    actually catch a peak); the CIM/PowerShell query is the fallback for a host
    where the ctypes path fails. Both return the same tree sum.
    """
    fast = sample_tree_rss_mb_fast(pid)
    if fast is not None:
        return fast
    return sample_tree_rss_mb_cim(pid, timeout)


def sample_tree_rss_mb_cim(pid, timeout=15):
    """CIM fallback for the process-tree sum (see `sample_rss_mb`)."""
    script = ("Get-CimInstance Win32_Process | ForEach-Object { "
              '"$($_.ProcessId) $($_.ParentProcessId) $($_.WorkingSetSize)" }')
    try:
        out = subprocess.run(["powershell", "-NoProfile", "-Command", script],
                             capture_output=True, text=True, timeout=timeout).stdout
    except Exception:
        return None
    if not out:
        return None
    rows = []
    for line in out.strip().splitlines():
        parts = line.split()
        if len(parts) == 3 and all(p.isdigit() for p in parts):
            rows.append((int(parts[0]), int(parts[1]), int(parts[2])))
    if not rows:
        return None
    children = {}
    own = {}
    for child_pid, parent_pid, ws in rows:
        own[child_pid] = ws
        children.setdefault(parent_pid, []).append(child_pid)
    total = own.get(pid, 0)
    stack = [pid]
    while stack:
        cur = stack.pop()
        for kid in children.get(cur, []):
            total += own.get(kid, 0)
            stack.append(kid)
    return total / (1024 * 1024)


class TreeSampler:
    """Background process-tree working-set sampler shared by every perf probe.

    Measurement contract:
      * Each sample is `sample_rss_mb` (Toolhelp32 tree walk, CIM fallback) —
        milliseconds, so `interval` (default 50 ms) is a real cadence.
      * The FIRST query runs immediately, before the first sleep.
      * `peak_mb`/`mark()` are None-able by contract: `samples == 0` means the
        leg was never measured, and callers must report that as INVALID rather
        than as a 0 MB peak.
    """

    def __init__(self, pid, interval=0.05):
        self.pid = pid
        self.interval = interval
        self.peak = 0.0
        self.peak_at = 0.0
        self.samples = []
        self.started = time.time()
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._first = True

    def _run(self):
        while self._first or not self._stop.wait(self.interval):
            self._first = False
            rss = sample_rss_mb(self.pid)
            # Stamp AFTER the query returns, not before it is issued: the value
            # describes the tree when the query executed, so the completion time
            # is the honest label.
            elapsed = time.time() - self.started
            # Contract: `rss is not None` (not `if rss`). A 0.0 MB tree is a
            # real reading of an already-exited process; the old truthiness test
            # dropped those points and made "the process is gone" look like "the
            # sampler is not running". `samples == 0` alone means unmeasured,
            # and a post-exit 0.0 can never raise the peak.
            if rss is not None:
                self.samples.append([round(elapsed, 2), round(rss, 1)])
                if rss > self.peak:
                    self.peak = rss
                    self.peak_at = elapsed
            if self._stop.is_set():
                break

    def mark(self):
        """Peak-so-far, or None when no sample has landed yet.

        Contract: an unmeasured mark is `None`, never `0.0` — a reader cannot
        tell a real 0 MB working set from a sampler that had not queried yet.
        """
        return round(self.peak, 1) if self.samples else None

    def start(self):
        self._thread.start()

    def stop(self):
        self._stop.set()
        self._thread.join(timeout=max(2.0, 3.0))


def run_cli(cli, label, session_path, agent, timeout, out_dir, no_prompt=False):
    env = dict(os.environ)
    env["PIR_AGENT_DIR"] = agent
    env["PI_CODING_AGENT_DIR"] = agent
    is_cmd = os.name == "nt" and cli.lower().endswith((".cmd", ".bat"))
    program = (os.environ.get("ComSpec") or "cmd.exe") if is_cmd else cli
    args = ["--mode", "json", "--print", "--model", "mock/mock-model",
            "--session", session_path]
    if not no_prompt:
        args.append("Reply with OK")
    argv = (["/d", "/c", cli] + args) if is_cmd else args
    out_path = os.path.join(out_dir, f"{label}.stdout.txt")
    err_path = os.path.join(out_dir, f"{label}.stderr.txt")
    started = time.time()
    with open(out_path, "wb") as out_fh, open(err_path, "wb") as err_fh:
        proc = subprocess.Popen([program] + argv, cwd=os.path.dirname(session_path), env=env,
                                stdin=subprocess.DEVNULL, stdout=out_fh, stderr=err_fh)
    # Peak WORKING SET of the whole process tree, sampled in the background so
    # a slow CIM query cannot starve the cheap root sampler (contract: the tree
    # is the measurement; the root point keeps every leg measured and is
    # labelled `scope: root`, never merged into a tree peak).
    sampler = TreeSampler(proc.pid)
    sampler.start()
    root_peak = 0.0
    root_peak_at = 0.0
    root_peak_at_last = -1.0
    root_samples = []
    timed_out = False
    while proc.poll() is None:
        if time.time() - started > timeout:
            subprocess.run(["taskkill", "/PID", str(proc.pid), "/T", "/F"],
                           capture_output=True)
            timed_out = True
            break
        elapsed = time.time() - started
        # 0.2 s keeps the extra `tasklist` spawns from perturbing the leg
        # (each costs ~0.2 s here).
        if elapsed - root_peak_at_last >= 0.2 or root_peak_at_last < 0:
            root = sample_root_rss_mb(proc.pid)
            root_peak_at_last = elapsed
            if root:
                root_samples.append([round(elapsed, 2), round(root, 1)])
                if root > root_peak:
                    root_peak = root
                    root_peak_at = elapsed
        time.sleep(0.05)
    sampler.stop()
    proc.wait(timeout=30)
    stderr = open(err_path, "r", encoding="utf-8", errors="replace").read()
    kinds = {}
    opened_id = None
    try:
        for line in open(out_path, "r", encoding="utf-8", errors="replace"):
            line = line.strip()
            if line.startswith("{"):
                try:
                    entry = json.loads(line)
                except Exception:
                    continue
                kinds[entry.get("type")] = kinds.get(entry.get("type"), 0) + 1
                if opened_id is None and entry.get("type") == "session":
                    opened_id = entry.get("id")
    except OSError:
        pass
    wanted_id = session_id_of(session_path)
    return {"label": label, "exit": proc.returncode, "wallS": round(time.time() - started, 2),
            # A killed leg's peak is a truncated measurement, not a peak. The
            # caller must mark it INVALID (contract: an unmeasured leg is never
            # a pass), otherwise a leg that never finished its load would be
            # ranked against a leg that did.
            "timedOut": timed_out,
            "openedSessionId": opened_id, "wantedSessionId": wanted_id,
            "sessionMatches": (wanted_id is not None and opened_id == wanted_id),
            # `peakRssMb`/`peakScope` name the instrument that actually produced
            # the verdict number: "tree" is the full process tree (the real
            # measurement), "root" is the fast tasklist fallback used only when
            # every CIM query failed. Never relabel a root peak as tree.
            #
            # Both samplers measure a SUBSET of the same quantity (the root is
            # contained in the tree), so each observation is a lower bound on
            # the true tree peak. A CIM query costs ~1-2 s on a loaded host and
            # can miss a transient spike (measured: tree 68 MB while the dense
            # root series saw 94 MB), so the reported peak is the max over BOTH
            # lower bounds — never an overstatement, and no longer an
            # understatement caused by sparse CIM sampling. `peakSource` names
            # which series produced it.
            "peakRssMb": round(max([sampler.peak] * bool(sampler.samples)
                                   + [root_peak] * bool(root_peak)), 1),
            "treePeakRssMb": round(sampler.peak, 1) if sampler.samples else None,
            "rootPeakRssMb": round(root_peak, 1) if root_peak else None,
            "peakSource": ("tree" if sampler.samples and sampler.peak >= root_peak
                           else ("root" if root_peak else None)),
            "peakScope": "tree" if sampler.samples else ("root" if root_peak else None),
            "peakAtS": round(sampler.peak_at if (sampler.samples and sampler.peak >= root_peak)
                             else root_peak_at, 2),
            "samples": sampler.samples,
            "rootSamples": root_samples,
            "compaction": kinds.get("compaction_start", 0), "kinds": kinds,
            "stderrTail": stderr.strip().splitlines()[-2:]}


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--pir", default=os.path.join(REPO, "target-alt", "release", "pir.exe"))
    ap.add_argument("--pi-cli", default=os.path.join(os.environ.get("APPDATA", ""), "npm", "pi.cmd"))
    ap.add_argument("--size-mb", type=int, default=100)
    ap.add_argument("--context-window", type=int, default=128000,
                    help="mock model context window (raise it to skip compaction)")
    ap.add_argument("--timeout", type=float, default=300.0)
    ap.add_argument("--only", default="")
    ap.add_argument("--no-prompt", action="store_true",
                    help="startup-only floor: pir exits before opening a session when no "
                         "prompt is given, so this leg measures the process floor and NOT the "
                         "load path — use --baseline-size-mb for the load increment")
    ap.add_argument("--baseline-size-mb", type=int, default=0,
                    help="also run one leg on a session of this size (a few MB is enough) and "
                         "report the peak-RSS increment the big session adds over it: the "
                         "load-plus-turn budget is judged on that delta")
    ap.add_argument("--session-name", default="big.jsonl",
                    help="file name inside <out>/work, so baseline and target legs can share "
                         "one --out directory without clobbering each other")
    ap.add_argument("--budget-mb", type=float, default=20.0,
                    help="PLAN success criterion 3: a fresh 100 MB session may add at most "
                         "this much peak process-tree RSS over the 2 MB baseline leg")
    ap.add_argument("--out", default=os.path.join(REPO, "logs", "perf", "big-session"))
    args = ap.parse_args(argv)

    # ABSOLUTE paths throughout: run_cli() starts the child with the session's
    # directory as its cwd, so a relative --out/--session path is resolved a
    # second time against that cwd. Measured failure: the child opened a FRESH
    # session under <out>/work/logs/perf/<out>/work/ and the leg reported
    # numbers for a session it never loaded (same defect the tui probe names).
    out_dir = os.path.abspath(args.out)
    os.makedirs(out_dir, exist_ok=True)
    work = os.path.join(out_dir, "work")
    os.makedirs(work, exist_ok=True)
    session_path = os.path.join(work, args.session_name)
    if not os.path.exists(session_path) or os.path.getsize(session_path) < args.size_mb * 1024 * 1024 * 0.9:
        t0 = time.time()
        written = make_session(session_path, args.size_mb, work)
        print(f"[big] session written: {written / 1048576:.1f} MB in {time.time() - t0:.1f}s -> {session_path}")

    server = Mock(("127.0.0.1", 0), _Mock)
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()
    agent = build_agent_dir(port, args.context_window)

    targets = []
    if args.only != "pi":
        targets.append(("pir", args.pir))
    if args.only != "pir":
        targets.append(("pi", args.pi_cli))
    # run_cli() passes the executable to Popen with cwd=<session dir>, so a
    # relative --pir/--pi-cli is looked up from there and Popen raises
    # FileNotFoundError ([WinError 2]) before any measurement. Resolve both
    # against the invocation directory, which is how the user typed them.
    targets = [(label, os.path.abspath(cli)) for label, cli in targets]

    results = []
    invalid_legs = []
    over_budget = []
    identities = {}
    try:
        for label, cli in targets:
            if not os.path.exists(cli):
                print(f"ERROR: {label} not found: {cli}", file=sys.stderr)
                return 2
            # Contract (runbook §3): a number without the binary it came from
            # is not evidence. Printed once per target and stored in the
            # summary so a stale build cannot be mistaken for the wave's.
            ident = binary_identity(cli)
            identities[label] = ident
            print(f"[big] {label} binary: {ident['path']} size={ident['size']} "
                  f"mtime={ident['mtime']} version={ident['version']} sha256={ident['sha256']}")
            note = staleness_note(cli)
            if note and note["stale"]:
                print(f"[big] {label} STALE BINARY: built {note['binaryMtime']}, but "
                      f"{note['newestSource']} is newer ({note['newestSourceMtime']}) — "
                      f"SMOKE TEST, not a measurement; rebuild before recording the verdict")
            baseline = None
            baseline_scope = None
            if args.baseline_size_mb > 0:
                base_path = os.path.join(work, f"baseline-{args.baseline_size_mb}mb.jsonl")
                if (not os.path.exists(base_path)
                        or os.path.getsize(base_path) < args.baseline_size_mb * 1024 * 1024 * 0.9):
                    make_session(base_path, args.baseline_size_mb, work)
                STATE["bodies"].clear()
                base = run_cli(cli, f"{label}-baseline", base_path, agent, args.timeout,
                               out_dir, no_prompt=args.no_prompt)
                baseline = base["peakRssMb"]
                baseline_scope = base.get("peakScope")
                print(f"[big] {label} baseline ({args.baseline_size_mb} MB session): "
                      f"peakRSS={base['peakRssMb']} MB ({base.get('peakSource')}; at "
                      f"{base['peakAtS']}s)")
                # The increment is only meaningful when BOTH legs opened the
                # session they were asked to open: a baseline that fell back to
                # a fresh session reports the floor of the wrong workload.
                if not base["sessionMatches"]:
                    print(f"[big] {label} baseline INVALID: it did not open the "
                          f"requested session (opened {base['openedSessionId']!r}, "
                          f"wanted {base['wantedSessionId']!r})")
                    invalid_legs.append(f"{label}-baseline")
                if base.get("timedOut"):
                    print(f"[big] {label} baseline INVALID: the leg hit --timeout "
                          f"{args.timeout}s and was killed; its peak is truncated")
                    invalid_legs.append(f"{label}-baseline-timeout")
                results.append(dict(base, role="baseline", sizeMb=args.baseline_size_mb))
            STATE["bodies"].clear()
            r = run_cli(cli, label, session_path, agent, args.timeout,
                        out_dir, no_prompt=args.no_prompt)
            sent = sorted(STATE["bodies"], reverse=True)
            sizes_all = ", ".join(f"{b/1048576:.1f}MB" for b in STATE["bodies"])
            print(f"[big] {label}: exit={r['exit']} wall={r['wallS']}s peakRSS={r['peakRssMb']} MB "
                  f"({r.get('peakSource')}; at {r['peakAtS']}s) "
                  f"compaction_events={r['compaction']} "
                  f"requests={len(STATE['bodies'])} "
                  f"largest_request={sent[0] if sent else 0} B request_sizes=[{sizes_all}]")
            if not r["sessionMatches"]:
                print(f"[big] {label} INVALID: the leg did not open the requested session "
                      f"(opened {r['openedSessionId']!r}, wanted {r['wantedSessionId']!r}) — "
                      f"its memory/wall numbers describe a fresh session, not the file")
                invalid_legs.append(label)
            # A leg that produced NEITHER tree nor root samples reports
            # peakRssMb 0.0, and the increment below would then be the whole
            # target peak — a fabricated "OVER budget" verdict. An unmeasured
            # leg is INVALID, not a pass.
            if r.get("peakScope") is None:
                print(f"[big] {label} INVALID: the leg produced no RSS samples "
                      f"(exit={r['exit']}); its peak/wall numbers are placeholders")
                invalid_legs.append(label)
            elif r.get("timedOut"):
                print(f"[big] {label} INVALID: the leg hit --timeout {args.timeout}s "
                      f"and was killed; its peak is truncated, not a peak")
                invalid_legs.append(f"{label}-timeout")
            elif r["peakScope"] == "root":
                print(f"[big] {label} WARNING: no process-TREE sample landed "
                      f"(CIM queries exceeded their timeout); peakRssMb is the "
                      f"ROOT-only fallback, not the tree measurement")
            if baseline is not None and baseline <= 0:
                print(f"[big] {label} baseline INVALID: no RSS samples on the "
                      f"{args.baseline_size_mb} MB baseline leg; the increment is "
                      f"not measurable")
                invalid_legs.append(f"{label}-baseline")
            if baseline is not None and baseline > 0 and r.get("peakScope") is not None:
                increment = r["peakRssMb"] - baseline
                r["baselinePeakRssMb"] = baseline
                r["incrementRssMb"] = round(increment, 1)
                r["budgetMb"] = args.budget_mb
                # Contract: a budget verdict is only honest when both legs had
                # the same instruments available. `peakRssMb` is the max over
                # the tree and root lower bounds, so a baseline that only got
                # the root fallback (every CIM timeout) is compared against a
                # target that also only got root — otherwise one leg is a
                # lower bound from a sparser series and the delta is noise.
                baseline_inst = baseline_scope
                if baseline_inst != r["peakScope"]:
                    print(f"[big] {label} INCREMENT INVALID: baseline measured with "
                          f"{baseline_inst!r} scope, target with {r['peakScope']!r} — the "
                          f"two peaks are not comparable")
                    invalid_legs.append(f"{label}-scope")
                else:
                    verdict = "WITHIN" if increment <= args.budget_mb else "OVER"
                    r["budgetVerdict"] = verdict
                    print(f"[big] {label} INCREMENT over the {args.baseline_size_mb} MB baseline: "
                          f"{increment:.1f} MB "
                          f"({verdict} the {args.budget_mb:.0f} MB budget)")
                    # Contract: the gate is the exit code, not the sentence.
                    # An OVER increment must fail the shell that runs the gate
                    # (exit 1); printing OVER and returning 0 was a silent pass
                    # for every `&&`-chained or CI invocation.
                    if verdict == "OVER":
                        over_budget.append(label)
            if r["stderrTail"]:
                print(f"      stderr: {r['stderrTail']}")
            r["largestRequestBytes"] = sent[0] if sent else 0
            r["requests"] = len(STATE["bodies"])
            results.append(r)
    finally:
        server.shutdown()
        shutil.rmtree(agent, ignore_errors=True)
        with open(os.path.join(out_dir, "summary.json"), "w", encoding="utf-8") as fh:
            json.dump({"sizeMb": args.size_mb, "contextWindow": args.context_window,
                       "session": session_path, "budgetMb": args.budget_mb,
                       "binaries": identities, "results": results}, fh, indent=2)
        print(f"[big] summary: {os.path.join(out_dir, 'summary.json')}")
        if invalid_legs:
            print(f"[big] INVALID legs: {', '.join(invalid_legs)} "
                  f"(no session match / no RSS samples / mixed instruments / timeout; "
                  f"their numbers are not comparable)")
        if over_budget:
            print(f"[big] GATE FAILED: {', '.join(over_budget)} over the "
                  f"{args.budget_mb:.0f} MB increment budget")
        else:
            print(f"[big] GATE: no target leg exceeded the {args.budget_mb:.0f} MB increment budget"
                  + ("" if any(r.get("budgetVerdict") for r in results) else
                     " (NO BUDGET VERDICT: pass --baseline-size-mb to judge the gate)"))
    if invalid_legs:
        return 3
    if over_budget:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
