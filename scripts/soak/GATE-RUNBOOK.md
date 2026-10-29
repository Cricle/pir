# GATE-RUNBOOK — memory probes + soak (pir)

Copy-pasteable commands for the two behavioural gates that are **not**
covered by `cargo test`: the memory budget and the long-run stability soak.
Every command below is real (verified against each script's `--help` on
2026-10-21); nothing here is invented.

Binary discipline: `target-alt` is the build directory. `pir\target` belongs to
the eval harness — never write or kill processes from there. The release binary
in `target-alt/release` is **stale** until the next rebuild, and the soak/perf
scripts treat a stale binary as an error, not a pass.

---

## 0. Prerequisites

```powershell
$env:CARGO_TARGET_DIR = "C:\Users\XPENG_USER\Desktop\github\pir\target-alt"
cargo build                      # debug leg (memory probes below use it)
# release leg, after the soak/perf work is done:
cargo build --release
```

No API key and no network are needed: every driver here starts a local
OpenAI-compatible mock (`127.0.0.1`, random port) and writes a throwaway agent
dir unless `--no-mock` + `--model` are passed.

---

## 1. Memory probes (debug binary, after a rebuild)

The user's budget: a **fresh 100 MB session costs ≤ 20 MB increment** over the
same binary's 2 MB baseline, and the interactive leg must not exceed the 2 MB
baseline leg.

```powershell
# (a) headless: 100 MB session, one turn, 128k window, vs a 2 MB baseline
python scripts/perf/big_session_probe.py --size-mb 100 --baseline-size-mb 2 --only pir

# (b) interactive TUI leg (ConPTY): first frame, transcript on screen, peak RSS
python scripts/perf/tui_session_probe.py --size-mb 100 --baseline-size-mb 2

# (c) floor: one-delta run with the REAL agent dir (attribution, not a budget)
python scripts/perf/rss_probe.py --pir target-alt\debug\pir.exe --thinking off "say ok"
```

**Expected verdict strings:** (a) prints `INCREMENT … (WITHIN/OVER the 20 MB
budget)`; (b) reports first-paint (timed by the `ESC[?2026l` frame-end marker)
and the peak working set per size; (c) prints a single peak-RSS number.

**A failure means:** (a) OVER ⇒ a transcript copy or a whole-session
materialization regressed; (b) peak above the 2 MB baseline leg ⇒ windowing or
an eager consumer regressed; (c) a large jump ⇒ extension host / shim cost.

---

## 2. Soak (debug or release; release is authoritative)

All three drivers accept `--mock` (default) / `--no-mock`, sample the process
tree continuously, and fail loudly rather than printing a silent PASS.

```powershell
# (a) headless long run: N turns, alternating text/tool, growth + compaction
python scripts/soak/soak_headless.py --turns 200 --tool-every 5

# (b) reopen: one large session, one turn, close, repeat
python scripts/soak/soak_session_reopen.py --reopens 5 --size-mb 100

# (c) crash/reopen: kill the tree mid-turn, reopen, assert every JSONL and
#     durable journal still parses and the next turn succeeds
python scripts/soak/soak_crash_reopen.py --iterations 3
```

**Expected verdict:** each driver prints its own verdict line naming the
criteria it evaluated (RSS slope / growth / max, process count, tool errors,
compactions, reopen or crash counters). `--min-tree-samples` /
`--min-process-samples` default to 4: fewer samples is a **failure**, never a
pass.

**A failure means:** a real leak (flat-RSS criterion), an orphaned tool child
(process-count criterion), a lost/torn durable record (crash-reopen), or a turn
that stopped making progress (request/turn timeout).

Full-scale runs (thousands of turns) are the same commands with
`--turns 5000` / `--reopens 200` / `--iterations 50`; they are wall-clock
bound, so run them only on a release binary and only when the machine is idle.

---

## 3. Release re-run (authoritative for the deliverable)

```powershell
cargo build --release
python scripts/perf/big_session_probe.py --size-mb 100 --baseline-size-mb 2 --only pir --pir target-alt\release\pir.exe
python scripts/perf/tui_session_probe.py --size-mb 100 --baseline-size-mb 2 --pir target-alt\release\pir.exe
python scripts/soak/soak_headless.py --pir target-alt\release\pir.exe --turns 500
python scripts/soak/soak_session_reopen.py --pir target-alt\release\pir.exe --reopens 10 --size-mb 100
python scripts/soak/soak_crash_reopen.py --pir target-alt\release\pir.exe --iterations 5
```

Record every verdict line in `docs/STATUS.md` with the binary path, size and
build time; a number without the binary it came from is not evidence.

---

## 4. Rules

- Never kill a process you did not start. The drivers kill only their own
  process trees (`taskkill /T /F` on Windows, process group on POSIX).
- Never point `--agent-dir` at `~/.pi`. Every run uses a throwaway dir.
- A run whose binary predates the change under test is a **smoke test**, not a
  measurement; label it as such.
