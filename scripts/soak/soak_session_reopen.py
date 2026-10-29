#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Session-reopen soak: open a large session, run one turn, close, repeat M times.

The scenario the objective cares about is "a session that has been used for a
long time can still be opened, appended to and re-opened without the resident
set creeping up".  This driver:

1. writes (or reuses) a large valid v3 JSONL session fixture;
2. for each of ``--reopens`` iterations, launches a FRESH
   ``pir --mode json --print --session <fixture> ...`` process, executes one
   turn, waits for a clean exit, and records the process tree's peak working
   set (sampled in the background while the turn runs);
3. after every iteration re-validates the session file (header id unchanged,
   every complete line parses, the file ends with a newline, the file only
   grew), so a reopen that silently fell back to a fresh session or truncated
   history is a failure.

Pass criteria
-------------
* every reopen exits 0, emits ``agent_end``/an assistant message, reports
  the same session id in its JSON header line (strict version 3), and appends
  a parseable user/assistant pair;
* every session inspection is clean (strict v3 header, no malformed complete
  line, no lost trailing newline, monotonic size chained across reopens);
* peak RSS per reopen has no upward trend: least-squares slope <=
  ``--rss-slope-mb`` MB/reopen, last-quarter median <= first-quarter median +
  ``--rss-growth-mb``, and no tree-scope sample above ``--rss-max-mb``; at
  least four reopen peaks and ``--min-process-samples`` process readings are
  required (missing samples fail, never pass silently);
* the process tree never exceeds ``--max-processes``; the process verdict is
  a least-squares slope over the reopen window (fitted rise <=
  ``--process-growth``) plus quarterly-median growth, and reports its
  criterion and measured/limit pairs.

The fixture defaults to ``--context-window * 2`` usage tokens, so the first
open takes the tail-first (paged) path and performs one threshold compaction;
later opens see the compaction checkpoint in the tail window and stay paged.
Pass ``--fixture-usage-tokens`` lower than the window to exercise the full
(widening) load path instead.

Usage
-----
    python scripts/soak/soak_session_reopen.py --reopens 20 --size-mb 20
    python scripts/soak/soak_session_reopen.py --fixture logs/session.jsonl --reopens 10

Exit codes: 0 = pass, 1 = soak verdict failed, 2 = setup/harness error.
"""

import argparse
import json
import os
import shutil
import sys
import time
import traceback

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import soak_common as common


def last_assistant_usage(path):
    """Best-effort ``totalTokens`` of the newest assistant message."""
    total = None
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    entry = json.loads(line)
                except ValueError:
                    continue
                message = entry.get("message") if isinstance(entry, dict) else None
                if isinstance(message, dict) and message.get("role") == "assistant":
                    usage = message.get("usage")
                    if isinstance(usage, dict):
                        value = usage.get("totalTokens")
                        if isinstance(value, int):
                            total = value
    except OSError:
        pass
    return total


def ensure_fixture(args, log_dir, work_dir, context_window):
    """Return ``(path, generated, usage_tokens)``."""
    target_path = os.path.join(
        common.ensure_dir(os.path.join(log_dir, "fixture")), "soak-session.jsonl"
    )
    if args.fixture:
        source = common.resolve_path(args.fixture)
        if not os.path.exists(source):
            raise RuntimeError("--fixture not found: %s" % source)
        if not args.fixture_in_place:
            common.ensure_dir(os.path.dirname(target_path))
            if os.path.abspath(source) != os.path.abspath(target_path):
                shutil.copyfile(source, target_path)
            path = target_path
        else:
            path = source
        # Honor --fixture-usage-tokens for a supplied fixture by rewriting the
        # copy's assistant usage. Without this the flag would be accepted and
        # silently ignored, so the requested load path would not be exercised.
        if args.fixture_usage_tokens is not None:
            rewritten = common.rewrite_fixture_usage(
                path, args.fixture_usage_tokens)
            if rewritten <= 0:
                raise RuntimeError(
                    "--fixture-usage-tokens was given but %s contains no "
                    "assistant messages with usage" % path)
            print("[reopen] rewrote assistant usage to %d tokens in %d "
                  "message(s)%s" % (args.fixture_usage_tokens, rewritten,
                                    " (in place)" if args.fixture_in_place else ""))
        return path, False, last_assistant_usage(path)

    target_bytes = int(args.size_mb * 1024 * 1024)
    usage = args.fixture_usage_tokens
    if usage is None:
        usage = int(context_window * 2)
    meta_path = target_path + ".meta.json"
    if not args.regen_fixture and os.path.exists(target_path):
        try:
            with open(meta_path, "r", encoding="utf-8") as fh:
                meta = json.load(fh)
        except (OSError, ValueError):
            meta = {}
        if (meta.get("sizeMb") == args.size_mb and meta.get("usageTokens") == usage
                and os.path.getsize(target_path) >= target_bytes * 0.9):
            print("[reopen] reusing fixture %s (%.1f MB)"
                  % (target_path, os.path.getsize(target_path) / 1048576.0))
            return target_path, False, usage
    print("[reopen] writing %.0f MB fixture (usage=%d tokens) ..."
          % (args.size_mb, usage))
    started = time.time()
    written = common.make_session_fixture(
        target_path, args.size_mb, work_dir, usage_tokens=usage)
    common.write_json(meta_path, {"sizeMb": args.size_mb, "usageTokens": usage})
    print("[reopen] fixture written: %.1f MB in %.1fs"
          % (written / 1048576.0, time.time() - started))
    return target_path, True, usage


def run_one_reopen(argv, cwd, env, stdout_path, stderr_path, request_timeout,
                   sample_interval, turn_timeout=600.0):
    """Run one reopen; returns a metrics dict (never raises on soak failure)."""
    started = time.monotonic()
    hard_deadline = started + turn_timeout
    runner = common.LineProcess(argv, cwd=cwd, env=env, stdout_path=stdout_path,
                                stderr_path=stderr_path)
    metrics = {
        "exit": None,
        "wallS": None,
        "peakRssMb": None,
        "treePeakRssMb": None,
        "rootPeakRssMb": None,
        "maxProcesses": None,
        "treeSamples": 0,
        "rootSamples": 0,
        "postAgentEndTreeSamples": 0,
        "postAgentEndSkipped": False,
        "agentEndChildAlive": None,
        "agentEndAtS": None,
        "samplerStartAtS": None,
        "agentEnds": 0,
        "assistantMessages": 0,
        "badStops": 0,
        "toolCalls": 0,
        "compactions": 0,
        "events": 0,
        "unparsed": 0,
        "hang": None,
        "openedSessionId": None,
        "streamHeader": None,
        "streamHeaderVersion": None,
        "lastText": "",
        "samples": [],
    }
    # Start the background sampler before any blocking probe. A fast child can
    # otherwise finish while the first CIM query is still running, leaving the
    # run with zero tree samples even though it was a clean turn.
    sampler_started = time.monotonic()
    metrics["samplerStartAtS"] = round(sampler_started - started, 3)
    sampler = common.TreeSampler(runner.proc.pid, interval_s=sample_interval)
    sampler.start()

    # One fast root sample anchors the peak even for runs that exit before the
    # slower CIM tree query returns; the background sampler adds the children
    # and the rest of the curve without stalling the event reader. The scope
    # of every sample stays attached to it: the tree total is the RSS verdict
    # basis, the root value is reported separately.
    root_rss = common.sample_root_rss_mb(runner.proc.pid)
    if root_rss is not None:
        metrics["samples"].append({"at_s": 0.0, "rss_mb": root_rss,
                                   "processes": None, "scope": "root"})
    first = common.sample_process_tree(runner.proc.pid)
    if first is not None:
        metrics["samples"].append({"at_s": 0.01, "rss_mb": first["rss_mb"],
                                   "processes": first["processes"],
                                   "scope": "tree"})
    last_event = time.monotonic()
    try:
        while True:
            now = time.monotonic()
            remaining = request_timeout - (now - last_event)
            if remaining <= 0:
                runner.kill()
                metrics["hang"] = "no event for %.0fs" % request_timeout
                break
            if now > hard_deadline:
                runner.kill()
                metrics["hang"] = "turn exceeded %.0fs" % turn_timeout
                break
            try:
                line = runner.next_line(min(0.25, remaining))
            except common.StreamTimeout:
                continue
            except common.StreamClosed:
                break
            last_event = time.monotonic()
            event = common.parse_event(line)
            if event is None:
                metrics["unparsed"] += 1
                continue
            metrics["events"] += 1
            kind = event.get("type")
            if kind == "agent_end":
                metrics["agentEnds"] += 1
                if metrics["agentEndAtS"] is None:
                    metrics["agentEndAtS"] = round(time.monotonic() - started, 3)
                    # Anchor the peak with a tree reading taken after the turn,
                    # but never fail a clean run merely because the child exits
                    # first: sample_process_tree() is a full CIM query and the
                    # root can disappear while it runs. Retry while the child is
                    # still alive; if it exits, record the check as skipped.
                    alive_at_agent_end = runner.poll() is None
                    metrics["agentEndChildAlive"] = alive_at_agent_end
                    got_post_sample = False
                    if alive_at_agent_end:
                        for attempt in range(3):
                            if attempt:
                                time.sleep(0.1)
                            if runner.poll() is not None:
                                break
                            sample_started_at = time.monotonic() - sampler_started
                            tree_now = common.sample_process_tree(runner.proc.pid)
                            sample_done_at = time.monotonic() - sampler_started
                            if tree_now is not None:
                                metrics["samples"].append({
                                    "at_s": round(sample_started_at, 3),
                                    "sampled_at_s": round(sample_done_at, 3),
                                    "rss_mb": tree_now["rss_mb"],
                                    "processes": tree_now["processes"],
                                    "scope": "tree",
                                    "post_agent_end": True,
                                })
                                metrics["postAgentEndTreeSamples"] += 1
                                got_post_sample = True
                                break
                    if not got_post_sample and runner.poll() is not None:
                        metrics["postAgentEndSkipped"] = True
                    elif not got_post_sample and not alive_at_agent_end:
                        metrics["postAgentEndSkipped"] = True
            elif kind == "message_end":
                message = event.get("message") or {}
                if message.get("role") == "assistant":
                    metrics["assistantMessages"] += 1
                    if message.get("stopReason") in ("error", "aborted"):
                        metrics["badStops"] += 1
                    content = message.get("content")
                    if isinstance(content, list):
                        text = "".join(
                            part.get("text", "") for part in content
                            if isinstance(part, dict) and part.get("type") == "text"
                        )
                        if text:
                            metrics["lastText"] = text[:200]
            elif kind == "tool_execution_end":
                metrics["toolCalls"] += 1
            elif kind == "compaction_end" and common.compaction_succeeded(event):
                metrics["compactions"] += 1
            elif kind == "session":
                # Some hosts emit the header through the event stream; the
                # header is also the first stdout line (parsed below).
                metrics["openedSessionId"] = event.get("id")
    finally:
        # Reap the child before stopping the sampler. On POSIX a killed root
        # stays a zombie with rss=0 until waited on; sampling that as a real
        # point would dilute the RSS trend. TreeSampler also skips zero-RSS
        # roots, but waiting first removes the race entirely.
        code = runner.wait(60)
        runner.close()
        samples = sampler.stop()
        metrics["samples"].extend(samples)
        metrics["exit"] = code
        metrics["wallS"] = round(time.monotonic() - started, 2)
        # Keep each sample's true scope. Sampler points at/after the agent_end
        # timestamp count as post-agent_end evidence even when the explicit
        # live sample above could not land.
        agent_end_at_s = metrics.get("agentEndAtS")
        sampler_at_s = metrics.get("samplerStartAtS")
        if agent_end_at_s is not None and sampler_at_s is not None:
            relative = max(0.0, agent_end_at_s - sampler_at_s)
            for sample in samples:
                # Use the completion time: a CIM query that started before
                # agent_end and finished after it still observed post-turn
                # memory, and at_s alone would incorrectly discard it.
                sample_at = float(sample.get(
                    "sampled_at_s", sample.get("at_s", 0)))
                if sample.get("scope") == "tree" and sample_at >= relative:
                    sample["post_agent_end"] = True
        tree_samples = [sample for sample in metrics["samples"]
                        if sample.get("scope") == "tree"
                        and sample.get("rss_mb") is not None]
        root_samples = [sample for sample in metrics["samples"]
                        if sample.get("scope") == "root"
                        and sample.get("rss_mb") is not None]
        post_tree = [sample for sample in tree_samples
                     if sample.get("post_agent_end")]
        metrics["treeSamples"] = len(tree_samples)
        metrics["rootSamples"] = len(root_samples)
        metrics["postAgentEndTreeSamples"] = len(post_tree)
        tree_rss = [sample["rss_mb"] for sample in tree_samples]
        root_rss = [sample["rss_mb"] for sample in root_samples]
        metrics["treePeakRssMb"] = round(max(tree_rss), 2) if tree_rss else None
        metrics["rootPeakRssMb"] = round(max(root_rss), 2) if root_rss else None
        # Peak RSS is the tree total only; a root-only peak must never be
        # relabeled as tree.
        metrics["peakRssMb"] = metrics["treePeakRssMb"]
        tree_procs = [sample["processes"] for sample in tree_samples
                      if sample.get("processes") is not None]
        metrics["maxProcesses"] = max(tree_procs) if tree_procs else None
        # First stdout line is the persisted session header (print-mode
        # contract); keep the whole object so its version can be validated.
        stream_header = common.stream_session_header(stdout_path)
        if stream_header is not None:
            metrics["streamHeader"] = stream_header
            metrics["streamHeaderVersion"] = stream_header.get("version")
            if stream_header.get("type") == "session":
                metrics["openedSessionId"] = stream_header.get("id")
        metrics["stderrTail"] = common.log_tail(stderr_path, 4)
    return metrics


def main(argv=None):
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--pir", default=None)
    ap.add_argument("--reopens", type=int, default=20,
                    help="number of open/turn/close iterations (default 20)")
    ap.add_argument("--size-mb", type=float, default=20.0,
                    help="generated fixture size in MB (default 20)")
    ap.add_argument("--fixture", default=None,
                    help="use/copy an existing session file instead of generating one")
    ap.add_argument("--fixture-in-place", action="store_true",
                    help="append directly to --fixture instead of copying it")
    ap.add_argument("--fixture-usage-tokens", type=int, default=None,
                    help="assistant totalTokens baked into a GENERATED fixture "
                         "(default: 2 x --context-window, forcing the paged path). "
                         "With --fixture it rewrites the copied fixture's "
                         "assistant usage instead of being ignored (0 is honored "
                         "as the full-load path; --fixture-in-place rewrites the "
                         "given file)")
    ap.add_argument("--regen-fixture", action="store_true",
                    help="regenerate the fixture even when a matching one exists")
    ap.add_argument("--model", default="mock/soak-model")
    ap.add_argument("--agent-dir", default=None)
    ap.add_argument("--log", default=os.path.join(common.REPO, "logs", "soak", "session-reopen"))
    ap.add_argument("--thinking", default="off")
    ap.add_argument("--turn-prompt", default="reopen turn %s" % common.MARKER_TEXT,
                    help="prompt for each reopen (default: a plain text marker)")
    ap.add_argument("--context-window", type=int, default=131072)
    ap.add_argument("--reserve-tokens", type=int, default=16384)
    ap.add_argument("--keep-recent-tokens", type=int, default=20000)
    ap.add_argument("--response-chars", type=int, default=500,
                    help="mock response size per reopen (default 500; keep it "
                         "small so repeated turns do not mask a leak)")
    ap.add_argument("--mock-delay-ms", type=int, default=500,
                    help="mock per-request delay so RSS sampling can observe the "
                         "load (default 500)")
    ap.add_argument("--request-timeout", type=float, default=180.0)
    ap.add_argument("--turn-timeout", type=float, default=600.0,
                    help="hard wall-clock cap for one reopen (default 600)")
    ap.add_argument("--timeout", type=float, default=3600.0,
                    help="total wall-clock budget (default 3600)")
    ap.add_argument("--sample-interval", type=float, default=0.05,
                    help="background process-tree sampling interval (default 0.05; "
                         "one CIM query takes longer than this on Windows)")
    ap.add_argument("--warmup-reopens", type=int, default=1,
                    help="exclude the first K reopens from the RSS slope/growth "
                         "trend (the first may compact; default 1)")
    ap.add_argument("--rss-slope-mb", type=float, default=1.0,
                    help="maximum RSS slope in MB/reopen (default 1.0)")
    ap.add_argument("--rss-growth-mb", type=float, default=24.0,
                    help="maximum last-quarter minus first-quarter median peak "
                         "RSS in MB (default 24)")
    ap.add_argument("--rss-max-mb", type=float, default=1024.0,
                    help="absolute peak-RSS ceiling in MB (default 1024)")
    ap.add_argument("--max-processes", type=int, default=64)
    ap.add_argument("--process-growth", type=int, default=8)
    ap.add_argument("--min-tree-samples", type=int, default=1,
                    help="minimum process-tree RSS samples in ONE reopen for a "
                         "PASS (default 1: the peak must come from a real tree "
                         "sample, never from a root-only point). The run-level "
                         "RSS verdict separately requires >= 4 reopen peaks.")
    ap.add_argument("--min-process-samples", type=int, default=4,
                    help="minimum reopen peaks for the run-level process/RSS "
                         "trend verdict (default 4; raise for long runs)")
    ap.add_argument("--min-compactions", type=int, default=None,
                    help="required successful compactions across all reopens "
                         "(default: 1 for a freshly generated high-usage fixture, "
                         "else 0)")
    ap.add_argument("--extra-arg", action="append", default=[])
    ap.add_argument("--mock", dest="mock", action="store_true", default=None)
    ap.add_argument("--no-mock", dest="mock", action="store_false")
    args = ap.parse_args(argv)

    if args.reopens < 1:
        print("ERROR: --reopens must be >= 1", file=sys.stderr)
        return 2
    if args.size_mb <= 0 and not args.fixture:
        print("ERROR: --size-mb must be > 0", file=sys.stderr)
        return 2
    if args.fixture_usage_tokens is not None and args.fixture_usage_tokens < 0:
        print("ERROR: --fixture-usage-tokens must be >= 0", file=sys.stderr)
        return 2

    pie = common.resolve_pir(args.pir)
    if not os.path.exists(pie):
        print("ERROR: pir binary not found: %s\n"
              "Build one or pass --pir <path>." % pie, file=sys.stderr)
        return 2

    mock_mode = args.mock if args.mock is not None else args.model.startswith("mock/")
    if mock_mode and "/" not in args.model:
        print("ERROR: mock mode needs --model provider/model", file=sys.stderr)
        return 2

    log_dir = common.resolve_path(args.log)
    common.ensure_dir(log_dir)
    work_dir = common.ensure_dir(os.path.join(log_dir, "work"))

    mock = None
    agent_dir = None
    if mock_mode:
        provider, model_id = args.model.split("/", 1)
        mock = common.MockProvider(
            model_id=model_id or "soak-model",
            provider=provider,
            response_chars=args.response_chars,
            delay_ms=max(0, args.mock_delay_ms),
        ).start()
        agent_dir = common.resolve_path(args.agent_dir) if args.agent_dir else \
            common.ensure_dir(os.path.join(log_dir, "agent"))
        common.provision_mock_agent(
            agent_dir, mock, args.context_window,
            reserve_tokens=args.reserve_tokens,
            keep_recent_tokens=args.keep_recent_tokens,
        )
    elif args.agent_dir:
        agent_dir = common.resolve_path(args.agent_dir)
    else:
        # Real-provider mode: resolve the same directory the child will use
        # (env override, then ~/.pi/agent) instead of passing None.
        agent_dir = common.resolve_agent_dir()
    env = common.build_child_env(agent_dir)

    try:
        fixture, generated, usage = ensure_fixture(
            args, log_dir, work_dir, args.context_window)
    except (OSError, RuntimeError) as exc:
        if mock is not None:
            mock.stop()
        print("ERROR: %s" % exc, file=sys.stderr)
        return 2

    fixture_report = common.inspect_jsonl(fixture)
    if not fixture_report["exists"] or not fixture_report["header_ok"]:
        if mock is not None:
            mock.stop()
        print("ERROR: fixture is not a readable v3 session: %s" % fixture,
              file=sys.stderr)
        return 2
    if fixture_report["malformed_complete"]:
        if mock is not None:
            mock.stop()
        print("ERROR: fixture already has malformed complete lines: %s" % fixture,
              file=sys.stderr)
        return 2
    expected_id = fixture_report["header"]["id"]
    fixture_bytes = fixture_report["bytes"]

    min_compactions = args.min_compactions
    if min_compactions is None:
        # A high-usage fixture crosses the threshold on its first open. The
        # decision uses the MEASURED newest assistant usage (which honors
        # --fixture-usage-tokens for both generated and supplied fixtures),
        # not a fixed constant: one required compaction per crossing.
        trigger = args.context_window - args.reserve_tokens
        min_compactions = 1 if (mock_mode and usage is not None
                                and usage > trigger) else 0

    print("[soak] pir=%s" % pie)
    print("[soak] model=%s mock=%s reopens=%d fixture=%.1fMB"
          % (args.model, "yes" if mock_mode else "no", args.reopens,
             fixture_bytes / 1048576.0))
    print("[soak] fixture=%s id=%s" % (fixture, expected_id))
    print("[soak] log=%s" % log_dir)

    iterations = []
    samples = []
    iterations_path = os.path.join(log_dir, "iterations.jsonl")
    try:
        os.remove(iterations_path)
    except OSError:
        pass
    failure = None
    harness_error = False
    total_deadline = time.monotonic() + args.timeout
    expected_bytes = fixture_bytes
    try:
        for index in range(1, args.reopens + 1):
            if time.monotonic() > total_deadline:
                failure = "total timeout %.0fs exceeded at reopen %d" % (
                    args.timeout, index)
                break
            before_bytes = os.path.getsize(fixture) if os.path.exists(fixture) else 0
            bytes_chain_ok = before_bytes == expected_bytes
            argv = common.build_pir_argv(
                pie, args.model, thinking=args.thinking, session_path=fixture,
                prompts=[args.turn_prompt], extra_args=args.extra_arg,
            )
            stdout_path = os.path.join(log_dir, "reopen-%03d.stdout.jsonl" % index)
            stderr_path = os.path.join(log_dir, "reopen-%03d.stderr.txt" % index)
            metrics = run_one_reopen(
                argv, work_dir, env, stdout_path, stderr_path,
                args.request_timeout, args.sample_interval, args.turn_timeout,
            )
            inspection = common.inspect_jsonl(fixture)
            after_bytes = inspection["bytes"]
            session_header = inspection["header"] or {}
            appended = common.inspect_appended_region(fixture, before_bytes)
            record = {
                "reopen": index,
                "exit": metrics["exit"],
                "wallS": metrics["wallS"],
                "peakRssMb": metrics["peakRssMb"],
                "treePeakRssMb": metrics["treePeakRssMb"],
                "rootPeakRssMb": metrics["rootPeakRssMb"],
                "treeSamples": metrics["treeSamples"],
                "rootSamples": metrics["rootSamples"],
                "postAgentEndTreeSamples": metrics["postAgentEndTreeSamples"],
                "postAgentEndSkipped": metrics["postAgentEndSkipped"],
                "agentEndChildAlive": metrics["agentEndChildAlive"],
                "maxProcesses": metrics["maxProcesses"],
                "agentEnds": metrics["agentEnds"],
                "assistantMessages": metrics["assistantMessages"],
                "badStops": metrics["badStops"],
                "toolCalls": metrics["toolCalls"],
                "compactions": metrics["compactions"],
                "events": metrics["events"],
                "unparsed": metrics["unparsed"],
                "hang": metrics["hang"],
                "openedSessionId": metrics["openedSessionId"],
                "sessionIdMatches": metrics["openedSessionId"] == expected_id,
                "sessionHeaderId": session_header.get("id"),
                "sessionHeaderIdMatches": session_header.get("id") == expected_id,
                "sessionHeaderVersion": inspection.get("header_version"),
                "sessionBytes": after_bytes,
                "grewBytes": after_bytes - before_bytes,
                "bytesChainOk": bytes_chain_ok,
                "expectedBeforeBytes": expected_bytes,
                "endsWithNewline": inspection["ends_with_newline"],
                "malformedComplete": len(inspection["malformed_complete"]),
                "streamHeaderVersion": metrics.get("streamHeaderVersion"),
                "appendedClean": appended["clean"],
                "appendedLines": appended["lines"],
                "appendedParsed": appended["parsed"],
                "appendedEntryTypes": appended["entry_types"],
                "appendedMessageRoles": appended["message_roles"],
                "stderrTail": metrics["stderrTail"],
            }
            iterations.append(record)
            common.append_jsonl(iterations_path, record)
            if metrics["treePeakRssMb"] is not None:
                samples.append({
                    "x": index,
                    "turn": index,
                    "rss_mb": metrics["treePeakRssMb"],
                    "processes": metrics["maxProcesses"],
                    "scope": "tree",
                })
            if metrics["rootPeakRssMb"] is not None:
                samples.append({
                    "x": index,
                    "turn": index,
                    "rss_mb": metrics["rootPeakRssMb"],
                    "processes": None,
                    "scope": "root",
                })
            print("[reopen] %d/%d exit=%s wall=%.2fs rss=%s MB (root=%s) "
                  "procs=%s compactions=%d session=%.1fMB"
                  % (index, args.reopens, metrics["exit"], metrics["wallS"] or 0,
                     metrics["peakRssMb"], metrics["rootPeakRssMb"],
                     metrics["maxProcesses"], metrics["compactions"],
                     after_bytes / 1048576.0))

            reasons = []
            if metrics["hang"]:
                reasons.append("request hang: %s" % metrics["hang"])
            if metrics["exit"] != 0:
                reasons.append("exit code %s" % metrics["exit"])
            if metrics["agentEnds"] < 1:
                reasons.append("no agent_end (turn did not run)")
            if metrics["assistantMessages"] < 1:
                reasons.append("no assistant message")
            if metrics["badStops"]:
                reasons.append("assistant stopReason error/aborted")
            if mock_mode and common.MOCK_OK not in metrics["lastText"]:
                reasons.append("assistant response did not come from the mock")
            if metrics["unparsed"]:
                reasons.append("%d unparsed stdout lines" % metrics["unparsed"])
            if not record["sessionIdMatches"]:
                reasons.append("opened session id %r != fixture id %r"
                               % (metrics["openedSessionId"], expected_id))
            stream_header_reason = common.header_version_reason(
                metrics.get("streamHeader"))
            if stream_header_reason:
                reasons.append("JSON stream: %s" % stream_header_reason)
            if not record["sessionHeaderIdMatches"]:
                reasons.append("session file header id %r != fixture id %r"
                               % (session_header.get("id"), expected_id))
            if not inspection.get("header_version_ok"):
                reasons.append("session file: %s"
                               % common.header_version_reason(inspection))
            if metrics["treeSamples"] < args.min_tree_samples:
                reasons.append(
                    "insufficient tree RSS samples: %d (require >= %d)"
                    % (metrics["treeSamples"], args.min_tree_samples))
            if metrics["treePeakRssMb"] is None:
                reasons.append("no tree-scope RSS peak for this reopen")
            elif metrics["treePeakRssMb"] > args.rss_max_mb:
                reasons.append(
                    "tree-scope peak RSS %.1f MB exceeds --rss-max-mb %.1f"
                    % (metrics["treePeakRssMb"], args.rss_max_mb))
            if metrics["maxProcesses"] is not None \
                    and metrics["maxProcesses"] > args.max_processes:
                reasons.append(
                    "process tree reached %d members (limit %d)"
                    % (metrics["maxProcesses"], args.max_processes))
            if (metrics["postAgentEndTreeSamples"] < 1
                    and not metrics["postAgentEndSkipped"]):
                reasons.append("no post-agent_end tree RSS sample for the peak")
            # A skip is recorded (the child exited before a full CIM query could
            # finish); it is not fatal on its own, but the reopen still needs a
            # real tree peak from --min-tree-samples, so a run that never
            # sampled the tree cannot pass.
            if not inspection["header_ok"]:
                reasons.append("session header invalid after reopen")
            if inspection["malformed_complete"]:
                reasons.append("%d malformed complete session lines"
                               % len(inspection["malformed_complete"]))
            if not inspection["ends_with_newline"]:
                reasons.append("session file lost its trailing newline")
            if not bytes_chain_ok:
                reasons.append(
                    "session size chain broken: reopen %d started at %d bytes, "
                    "expected %d (a previous reopen's append is missing)"
                    % (index, before_bytes, expected_bytes))
            if after_bytes <= before_bytes:
                reasons.append("session file did not grow (before=%d after=%d)"
                               % (before_bytes, after_bytes))
            expected_bytes = after_bytes
            # The appended region must be a real persisted turn: at least one
            # parsed record, and both the user prompt and the assistant reply.
            # A byte-count increase alone could be a newline repair or a
            # bookkeeping entry, which is not proof the turn was written.
            if not appended["clean"]:
                reasons.extend("appended region: %s" % error
                               for error in appended["errors"][:4])
            if appended["parsed"] < 1:
                reasons.append(
                    "reopen appended no parseable session record (bytes=%d, "
                    "leading_newline=%s)"
                    % (appended["bytes"], appended["leading_newline"]))
            if appended["message_roles"].get("user", 0) < 1:
                reasons.append("reopen appended no user message for its prompt")
            if appended["message_roles"].get("assistant", 0) < 1:
                reasons.append("reopen appended no assistant message")
            if reasons:
                failure = "reopen %d: %s" % (index, "; ".join(reasons))
                break
    except KeyboardInterrupt:
        print("[soak] interrupted", file=sys.stderr)
        if mock is not None:
            mock.stop()
        return 130
    except Exception as exc:  # noqa: BLE001 - the harness must still summarize
        traceback.print_exc()
        failure = "harness error: %s: %s" % (type(exc).__name__, exc)
        harness_error = True
    finally:
        if mock is not None:
            mock.stop()

    total_compactions = sum(item["compactions"] for item in iterations)
    rss_ok, rss_reasons, rss_metrics = common.rss_verdict(
        samples, args.rss_slope_mb, args.rss_growth_mb, args.rss_max_mb,
        warmup_x=args.warmup_reopens, prefer_scope="tree")
    proc_ok, proc_reasons, proc_metrics = common.process_count_verdict(
        samples, args.max_processes, args.process_growth,
        min_samples=args.min_process_samples, warmup_x=args.warmup_reopens)
    reasons = []
    if failure:
        reasons.append(failure)
    if len(iterations) != args.reopens:
        reasons.append("completed %d/%d reopens" % (len(iterations), args.reopens))
    if total_compactions < min_compactions:
        reasons.append("successful compactions %d < required %d"
                       % (total_compactions, min_compactions))
    reasons.extend(rss_reasons)
    reasons.extend(proc_reasons)

    final_report = common.inspect_jsonl(fixture)
    final_header_reason = common.header_version_reason(final_report)
    if final_header_reason:
        reasons.append("final session: %s" % final_header_reason)
    if final_report.get("malformed_complete"):
        reasons.append("final session has %d malformed complete line(s)"
                       % len(final_report["malformed_complete"]))
    if (final_report.get("bytes") or 0) < fixture_bytes:
        reasons.append("final session (%s bytes) is smaller than the fixture "
                       "baseline (%d bytes)"
                       % (final_report.get("bytes"), fixture_bytes))
    # Document the peak scope explicitly: the verdict ceiling and the trend
    # are computed from tree-total samples only, never from the root-only
    # fast points.
    peak_scope = "tree"
    if rss_metrics.get("verdictScope") not in (None, "tree"):
        reasons.append(
            "RSS verdict did not use the documented tree scope (got %r)"
            % rss_metrics.get("verdictScope"))
    summary = {
        "script": "soak_session_reopen",
        "verdict": "PASS" if not reasons else "FAIL",
        "reasons": reasons,
        "pir": pie,
        "model": args.model,
        "agentDir": agent_dir,
        "mock": mock_mode,
        "mockStats": mock.state.snapshot() if mock is not None else None,
        "reopens": args.reopens,
        "completedReopens": len(iterations),
        "peakScope": peak_scope,
        "fixture": {
            "path": fixture,
            "id": expected_id,
            "generated": generated,
            "usageTokens": usage,
            "usageMeetsTrigger": usage is not None and usage > (
                args.context_window - args.reserve_tokens),
            "initialBytes": fixture_bytes,
            "finalBytes": final_report.get("bytes"),
            "finalBytesDelta": (final_report.get("bytes") or 0) - fixture_bytes,
            "finalHeaderVersion": final_report.get("header_version"),
            "finalHeaderVersionOk": final_report.get("header_version_ok"),
            "finalMalformedComplete": len(final_report.get("malformed_complete") or []),
        },
        "rss": rss_metrics,
        "processes": proc_metrics,
        "compactions": total_compactions,
        "thresholds": {
            "warmupReopens": args.warmup_reopens,
            "rssSlopeMbPerReopen": args.rss_slope_mb,
            "rssGrowthMb": args.rss_growth_mb,
            "rssMaxMb": args.rss_max_mb,
            "maxProcesses": args.max_processes,
            "processGrowth": args.process_growth,
            "minTreeSamplesPerReopen": args.min_tree_samples,
            "minProcessSamples": args.min_process_samples,
            "minCompactions": min_compactions,
            "requestTimeoutS": args.request_timeout,
        },
        "iterations": iterations,
    }

    print("")
    print("=== soak_session_reopen ===")
    print("  reopens:      %d/%d" % (len(iterations), args.reopens))
    print("  compactions:  %d (required %d)" % (total_compactions, min_compactions))
    print("  rss peaks:    %s MB" % [item["peakRssMb"] for item in iterations])
    print("  rss:          peak=%s MB slope=%s MB/reopen growth=%s MB"
          % (rss_metrics.get("peakRssMb"), rss_metrics.get("slopeMbPerTurn"),
             rss_metrics.get("growthMb")))
    print("  processes:    max=%s slope=%s/unit rise=%s growth=%s"
          % (proc_metrics.get("maxProcesses"),
             proc_metrics.get("slopeProcessesPerUnit"),
             proc_metrics.get("slopeRiseOverWindow"),
             proc_metrics.get("processGrowth")))
    print("  criteria:     rss=%s" % rss_metrics.get("criterion"))
    print("                procs=%s" % proc_metrics.get("criterion"))
    print("  verdict:      %s" % summary["verdict"])
    for reason in reasons:
        print("    - %s" % reason)
    print("")

    if reasons:
        code = 2 if harness_error else 1
    else:
        code = 0
    return common.finish_summary(log_dir, summary, code)


if __name__ == "__main__":
    raise SystemExit(main())
