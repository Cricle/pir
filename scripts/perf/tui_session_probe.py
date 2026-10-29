# -*- coding: utf-8 -*-
"""Interactive-path memory probe (the leg the user actually feels).

`big_session_probe.py` measures `--mode json --print`. This one opens the same
session in the REAL TUI inside a ConPTY, exercises the paths a human uses —
render, scroll to the end, page up, wheel, and optionally one turn — and
samples the process tree's peak working set throughout. The TUI materializes
message blocks and per-frame row caches that the headless path never touches,
so this is where a huge session's resident cost shows up for a user.

Usage (from anywhere):
  python scripts/perf/tui_session_probe.py --size-mb 100
  python scripts/perf/tui_session_probe.py --size-mb 100 --baseline-size-mb 2
  python scripts/perf/tui_session_probe.py --size-mb 100 --prompt "Reply with OK"

Gate contract (PLAN success criterion 3, GATE-RUNBOOK §1b,
MEMORY-PROBE-NOTES.md): the interactive 100 MB peak must not exceed the peak of
the "2 MB baseline" leg run by the SAME binary through the SAME ConPTY driver.
That is an ABSOLUTE peak comparison, not a 20 MB increment: the TUI's window
and row caches are expected to be session-size-independent, so any growth over
the tiny-session leg is a regression. The process exit code carries the
verdict: 0 WITHIN, 1 OVER the baseline leg, 3 unmeasurable/invalid leg.

The session generator, agent-dir fixture and RSS sampler are shared with
`big_session_probe.py`; the ConPTY driver is the battery's `tui_drive.py`.
"""

import argparse
import json
import os
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, os.path.join(REPO, "scripts", "tui_conpty"))

import big_session_probe as big  # noqa: E402
import tui_drive  # noqa: E402


def log(text):
    print(f"[tui-mem] {text}", flush=True)


# One sampler implementation for every perf probe (defined in
# big_session_probe.TreeSampler): the TUI leg and the headless legs must use
# the same instrument or their peaks are not comparable. It takes its first
# CIM sample immediately, records `peak_at_s`, and `mark()` returns None for
# "no sample yet" instead of 0.0 MB.
TreeSampler = big.TreeSampler


def stripped_line_count(data):
    import re
    text = data.decode("utf-8", errors="replace")
    text = re.sub(r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)", "", text)
    text = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", text)
    text = re.sub(r"\x1b[()][A-Za-z0-9]", "", text)
    return len([line for line in text.splitlines() if line.strip()])


FRAME_END = b"\x1b[?2026l"
# The synthetic session generator writes this token into every message (see
# `big_session_probe.make_session`); it is the only string that proves the
# TRANSCRIPT (not the banner, not a status line) reached the screen.
TRANSCRIPT_TOKEN = "lorem ipsum"


def transcript_on_screen(data):
    """True when a pyte render of `data` shows the synthetic transcript.

    Contract (GATE-RUNBOOK §1b: "transcript on screen ... verified by a pyte
    render rather than newline counts"): the check is a terminal emulation of
    the captured stream, not a byte search and not a line count. A byte search
    is a FALSE POSITIVE for the windowed renderer, which addresses rows
    absolutely and can carry a row's text in the log while the final screen
    never shows it (measured: `transcriptVisible` was computed on raw bytes in
    one probe and reported True on a screen whose last frame was the welcome
    banner). Returns None when pyte is unavailable (the caller then reports the
    leg as unverified rather than as verified).
    """
    try:
        import pyte  # type: ignore
    except Exception:
        return None
    try:
        screen = pyte.Screen(140, 45)
        stream = pyte.Stream(screen)
        stream.feed(data.decode("utf-8", "replace"))
        return TRANSCRIPT_TOKEN in "\n".join(screen.display)
    except Exception:
        return None


def run_tui(cli, session_path, agent, out_dir, label, timeout, prompt, observe_seconds=60.0):
    env = dict(tui_drive.BASE_ENV)
    env["PIR_AGENT_DIR"] = agent
    env["PI_CODING_AGENT_DIR"] = agent
    env["RUST_LOG"] = "error"
    cmdline = f'"{cli}" interactive --session "{session_path}"'
    if prompt:
        cmdline += f' "{prompt}"'
    pty = tui_drive.Pty(cmdline, os.path.dirname(session_path), env)
    sampler = TreeSampler(pty.pid)
    sampler.start()
    marks = []
    try:
        # Boot + first full render of the session. A huge transcript can take a
        # long time to paint, so the FIRST PAINT is timed by the frame-end
        # marker separately from "the output went quiet".
        time.sleep(1.0)
        marks.append(["boot", sampler.mark()])
        started = time.time()
        first_paint_s = None
        quiet_deadline = started + observe_seconds
        last_len = -1
        stable_since = time.time()
        settled_without_transcript = False
        while time.time() < quiet_deadline:
            length = len(pty.log)
            log = bytes(pty.log)
            # Contract (GATE-RUNBOOK §1b): a completed frame ends with the
            # synchronized-update reset ESC[?2026l, so the FIRST occurrence is
            # the first paint. Byte-count or line-count proxies are wrong for
            # the windowed renderer, which addresses rows absolutely (a "first
            # paint" defined as >=100 KB of output measured a 4 MB session at
            # 51 MB RSS before its first frame).
            if first_paint_s is None and FRAME_END in log:
                first_paint_s = round(time.time() - started, 2)
                marks.append(["first-paint", sampler.mark()])
            # The quiet rule must NOT treat the welcome-screen frame as "done":
            # a debug build can sit CPU-bound for several seconds between the
            # banner and the first transcript paint (measured 6-8 s on a 100 MB
            # session). Settle only once the transcript token is on the wire.
            transcript_output = TRANSCRIPT_TOKEN.encode() in log
            if length != last_len:
                last_len = length
                stable_since = time.time()
            elif (
                time.time() - stable_since > 2.0
                and first_paint_s is not None
                and transcript_output
            ):
                break
            elif (
                time.time() - stable_since > 2.0
                and first_paint_s is not None
                and time.time() - started - first_paint_s > 20.0
            ):
                # 20 s of silence after a first frame with no transcript token:
                # the leg did NOT prove the session was loaded. Record it and
                # let main() mark the leg INVALID — the old code broke out here
                # and reported a peak as if a transcript had painted.
                settled_without_transcript = True
                break
            time.sleep(0.2)
        marks.append(["rendered", sampler.mark()])
        boot_lines = stripped_line_count(pty.snapshot())

        # Scroll to the end, then page up through history, then a wheel notch —
        # the paths that materialize/cache rows.
        pty.write(b"\x1b[1;5F")          # ctrl+end: jump to the live end
        time.sleep(1.0)
        for _ in range(6):
            pty.write(b"\x1b[5~")        # page up
            time.sleep(0.35)
        marks.append(["scrolled", sampler.mark()])
        pty.write(b"[<64;40;20M")        # ESC-less wheel-up (the console shape)
        time.sleep(0.4)
        pty.write(b"\x1b[<64;40;20M")    # ESC-prefixed wheel-up
        time.sleep(1.0)
        marks.append(["wheeled", sampler.mark()])

        if prompt:
            # One turn on top of the loaded session (compaction may fire).
            turn_deadline = time.time() + timeout / 2
            while time.time() < turn_deadline:
                if len(pty.log) != last_len:
                    last_len = len(pty.log)
                    stable_since = time.time()
                elif time.time() - stable_since > 3.0:
                    break
                time.sleep(0.3)
            marks.append(["turn", sampler.mark()])

        snapshot = pty.snapshot()
        with open(os.path.join(out_dir, f"{label}.screen.log"), "wb") as fh:
            fh.write(snapshot)
        frames = snapshot.count(FRAME_END)
        # Per-leg artifacts never share a file name: two legs of different
        # sizes in one --out dir would otherwise clobber each other's evidence.
        final_lines = stripped_line_count(snapshot)
        # Screen-level sanity (contract above `transcript_on_screen`): a pyte
        # render of the FINAL screen must show the synthetic payload, which
        # proves the transcript and not just the banner reached the screen.
        transcript_visible = transcript_on_screen(snapshot)
        # The live-loop decision and the final render must agree; a mismatch
        # means the transcript never painted (or scrolled away), so the leg's
        # "loaded the session" claim is not proven.
        transcript_proven = bool(transcript_visible) and not settled_without_transcript

        # Graceful quit, then the loud escalation the battery uses.
        pty.write(b"/quit\r")
        time.sleep(1.5)
        if pty.alive:
            pty.write(b"\x03")
            time.sleep(0.5)
    finally:
        # The ConPTY child is pir.exe; a `--prompt` turn can leave tool
        # subprocesses (bash/node/dev servers) that terminating the direct
        # child would not reach. Kill the TREE while the root is verifiably
        # alive (pid-reuse safe), then fall back to the direct terminate.
        try:
            if pty.running():
                tui_drive.kill_process_tree(pty.pid)
        except Exception as exc:  # diagnostic only — cleanup must continue
            log(f"process-tree kill failed: {exc}")
        pty.terminate()
        pty.wait(3)
        pty.close()
        sampler.stop()
        with open(os.path.join(out_dir, f"{label}.pty.log"), "wb") as fh:
            fh.write(pty.snapshot())

    return {
        "label": label,
        "aliveAtEnd": bool(pty.alive),
        "firstPaintS": first_paint_s,
        # Contract: `peakRssMb` is None when no sample landed, so an unmeasured
        # leg can never be compared as a 0.0 MB peak (main() marks it INVALID).
        "peakRssMb": round(sampler.peak, 1) if sampler.samples else None,
        "peakScope": "tree" if sampler.samples else None,
        "peakAtS": round(sampler.peak_at, 2) if sampler.samples else None,
        "marks": marks,
        "samples": sampler.samples,
        "screenLines": final_lines,
        "frames": frames,
        "transcriptVisible": transcript_visible,
        "transcriptProven": transcript_proven,
        "settledWithoutTranscript": settled_without_transcript,
        "bootLines": boot_lines,
        "ptyBytes": len(pty.snapshot()),
    }


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--pir", default=os.path.join(REPO, "target-alt", "debug", "pir.exe"))
    ap.add_argument("--size-mb", type=int, default=100)
    ap.add_argument("--baseline-size-mb", type=int, default=0,
                    help="also open a session of this size; the gate (PLAN criterion 3) is "
                         "target peak <= this leg's peak, the same binary and driver")
    ap.add_argument("--budget-mb", type=float, default=0.0,
                    help="allowed peak growth over the baseline leg (PLAN: 0 MB — the "
                         "interactive 100 MB session must not exceed the baseline leg)")
    ap.add_argument("--noise-mb", type=float, default=1.0,
                    help="instrument resolution: an increment inside +/-this is reported "
                         "as WITHIN (at noise) instead of flipping the verdict on run-to-run "
                         "sampling jitter (measured spread: 0.1-3 MB on the same pair)")
    ap.add_argument("--context-window", type=int, default=128000)
    ap.add_argument("--session-name", default="tui-big.jsonl")
    ap.add_argument("--prompt", default="", help="also send one prompt (runs a turn)")
    ap.add_argument("--timeout", type=float, default=420.0)
    ap.add_argument("--observe-seconds", type=float, default=60.0,
                    help="how long to watch for the session's FIRST paint before the "
                         "interaction phase (a huge transcript may take a while)")
    ap.add_argument("--out", default=os.path.join(REPO, "logs", "perf", "tui-session"))
    args = ap.parse_args(argv)

    if not os.path.exists(args.pir):
        print(f"ERROR: pir not found: {args.pir}", file=sys.stderr)
        return 2
    # ABSOLUTE binary path: the ConPTY child is started with the session's work
    # dir as its cwd, so a relative --pir was resolved a second time against
    # that dir and CreateProcess failed with err=2 (measured on
    # `--pir target-alt/debug/pir.exe`) — the run died before any measurement,
    # which is at least loud, but it made the smallest documented command
    # unusable. Same rule the --out/--session paths already follow.
    pir = os.path.abspath(args.pir)
    ident = big.binary_identity(pir)
    log(f"binary: {ident['path']} size={ident['size']} mtime={ident['mtime']} "
        f"version={ident['version']} sha256={ident['sha256']}")
    note = big.staleness_note(pir)
    if note and note["stale"]:
        log(f"STALE BINARY: built {note['binaryMtime']}, but {note['newestSource']} is "
            f"newer ({note['newestSourceMtime']}) — SMOKE TEST, not a measurement; "
            f"rebuild before recording the verdict")
    # ABSOLUTE paths throughout: the pty child runs with the work dir as its
    # cwd, so a relative --session path would be resolved a second time against
    # that cwd (observed: a doubled path, the app opening nothing and painting
    # the welcome screen instead of the session).
    out_dir = os.path.abspath(args.out)
    os.makedirs(out_dir, exist_ok=True)
    work = os.path.join(out_dir, "work")
    os.makedirs(work, exist_ok=True)
    session_path = os.path.join(work, args.session_name)
    if (not os.path.exists(session_path)
            or os.path.getsize(session_path) < args.size_mb * 1024 * 1024 * 0.9):
        t0 = time.time()
        written = big.make_session(session_path, args.size_mb, work)
        log(f"session written: {written / 1048576:.1f} MB in {time.time() - t0:.1f}s -> {session_path}")

    server = big.Mock(("127.0.0.1", 0), big._Mock)
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()
    agent = big.build_agent_dir(port, args.context_window)

    results = []
    invalid_legs = []
    over_budget = []
    try:
        baseline_peak = None
        if args.baseline_size_mb > 0:
            base_path = os.path.join(work, f"baseline-{args.baseline_size_mb}mb.jsonl")
            if (not os.path.exists(base_path)
                    or os.path.getsize(base_path) < args.baseline_size_mb * 1024 * 1024 * 0.9):
                big.make_session(base_path, args.baseline_size_mb, work)
            base = run_tui(pir, base_path, agent, out_dir,
                           f"tui-baseline-{args.baseline_size_mb}mb", args.timeout,
                           args.prompt, args.observe_seconds)
            baseline_peak = base["peakRssMb"]
            log(f"baseline ({args.baseline_size_mb} MB session): peakRSS={base['peakRssMb']} MB "
                f"({base.get('peakScope')}; at {base['peakAtS']}s), "
                f"snapshot {base['screenLines']} non-empty lines, "
                f"transcript proven: {base['transcriptProven']}")
            if baseline_peak is None:
                log(f"baseline INVALID: no RSS samples landed; the peak comparison is "
                    f"not measurable")
                invalid_legs.append("baseline-unmeasured")
            results.append(base)

        r = run_tui(pir, session_path, agent, out_dir,
                            f"tui-target-{args.size_mb}mb", args.timeout, args.prompt,
                            args.observe_seconds)
        log(f"target ({args.size_mb} MB session): peakRSS={r['peakRssMb']} MB "
            f"({r.get('peakScope')}; at {r['peakAtS']}s), "
            f"first frame at {r['firstPaintS']}s, {r['frames']} frames, "
            f"transcript on screen (pyte): {r['transcriptVisible']}, "
            f"transcript proven: {r['transcriptProven']}, alive at end {r['aliveAtEnd']}, "
            f"pty log {r['ptyBytes']} B")
        log("marks: " + ", ".join(
            f"{name}={'unmeasured' if peak is None else f'{peak}MB'}"
            for name, peak in r["marks"]))
        # Contract: an unmeasured peak is not a number and a leg whose
        # transcript never painted did not exercise the load path. Both are
        # INVALID (exit 3), never a pass.
        if r["peakRssMb"] is None:
            log("target INVALID: no RSS samples landed; the peak is unmeasured")
            invalid_legs.append("target-unmeasured")
        if not r["transcriptProven"]:
            log("target INVALID: the transcript never reached the screen "
                "(pyte render / 20 s no-token fallback); the leg did not prove it "
                "loaded the session")
            invalid_legs.append("target-transcript")
        if baseline_peak is not None and r["peakRssMb"] is not None:
            increment = r["peakRssMb"] - baseline_peak
            r["baselinePeakRssMb"] = baseline_peak
            r["incrementRssMb"] = round(increment, 1)
            r["budgetMb"] = args.budget_mb
            r["noiseMb"] = args.noise_mb
            # Contract: PLAN criterion 3 judges the INTERACTIVE leg as "100 MB
            # session <= baseline leg" — an absolute peak comparison against the
            # same binary/driver on a 2 MB session, not a 20 MB allowance. A
            # 20 MB allowance here would have passed a session-proportional
            # TUI regression that PLAN explicitly forbids.
            #
            # `--noise-mb` (default 1.0) is the instrument's resolution, not a
            # budget: two runs of the same pair measured -0.1 MB and -3.1 MB,
            # and a strict `increment <= 0` gate flips a pass/fail on that
            # jitter alone. An increment inside ±noise is reported as
            # "WITHIN (at noise)", which is a pass the foreman can quote only
            # together with the noise band.
            if increment > args.noise_mb:
                verdict = "OVER"
            elif increment > args.budget_mb:
                verdict = "WITHIN (at noise)"
            else:
                verdict = "WITHIN"
            r["budgetVerdict"] = verdict
            log(f"INCREMENT over the {args.baseline_size_mb} MB baseline leg: "
                f"{increment:.1f} MB ({verdict} the {args.budget_mb:.1f} MB allowance "
                f"+/- {args.noise_mb:.1f} MB instrument noise; PLAN criterion 3: the "
                f"interactive leg must not exceed the baseline)")
            if verdict == "OVER":
                over_budget.append("target")
        results.append(r)
    finally:
        server.shutdown()
        import shutil
        shutil.rmtree(agent, ignore_errors=True)
        with open(os.path.join(out_dir, "summary.json"), "w", encoding="utf-8") as fh:
            json.dump({"sizeMb": args.size_mb, "prompt": args.prompt,
                       "budgetMb": args.budget_mb, "binary": ident,
                       "session": session_path, "results": results}, fh, indent=2)
        log(f"summary: {os.path.join(out_dir, 'summary.json')}")
        log(f"GATE (exit {'3 INVALID' if invalid_legs else '1 OVER' if over_budget else '0'}): "
            f"invalid={invalid_legs or 'none'} over_budget={over_budget or 'none'}")
    if invalid_legs:
        return 3
    if over_budget:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
