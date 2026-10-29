#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Crash/reopen soak: kill ``pir`` mid-turn, reopen the session, repeat.

For each iteration the driver:

1. starts ``pir --mode json --print --session <session>`` with a prompt;
2. waits until the turn has actually started (``turn_start`` /
   ``message_start`` in the JSON stream), sleeps a random interval in
   ``[--kill-min, --kill-max]`` and kills the WHOLE process tree
   (``taskkill /T /F`` on Windows, ``SIGKILL`` to the process group on POSIX);
3. inspects every JSONL file that belongs to the session (and any
   ``durable/`` journals under the agent dir): the header must still parse,
   every COMPLETE line must parse, and only the final, non-newline-terminated
   line may be torn by the kill;
4. reopens the same session with a normal turn and requires a clean exit, an
   ``agent_end`` and an assistant message;
5. re-inspects the file: the appended region must be byte-clean (every new
   line parses and ends with a newline) and the header id must be unchanged.

The default mock provider makes the crashed turn slow (``--slow-delay-ms``)
so the kill always lands while the process is waiting on the provider. A
recovery turn runs at full speed. ``--iterations`` is the crash-reopen
counter reported in ``summary.json``.

Usage
-----
    python scripts/soak/soak_crash_reopen.py --iterations 10
    python scripts/soak/soak_crash_reopen.py --iterations 30 --slow-delay-ms 5000
    python scripts/soak/soak_crash_reopen.py --session path/to/session.jsonl --iterations 5

Exit codes: 0 = pass, 1 = soak verdict failed, 2 = setup/harness error.
"""

import argparse
import os
import random
import shutil
import sys
import time
import traceback
import uuid

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import soak_common as common


def run_turn(argv, cwd, env, stdout_path, stderr_path, request_timeout,
             turn_timeout=600.0):
    """Run one normal turn and collect the assertions the crash driver needs."""
    started = time.monotonic()
    hard_deadline = started + turn_timeout
    runner = common.LineProcess(argv, cwd=cwd, env=env, stdout_path=stdout_path,
                                stderr_path=stderr_path)
    metrics = {
        "exit": None,
        "wallS": None,
        "agentEnds": 0,
        "assistantMessages": 0,
        "badStops": 0,
        "compactions": 0,
        "compactionFailures": 0,
        "compactionAborted": 0,
        "events": 0,
        "unparsed": 0,
        "hang": None,
        "openedSessionId": None,
        "streamHeader": None,
        "streamHeaderVersion": None,
        "lastText": "",
        "stderrTail": [],
    }
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
                line = runner.next_line(min(1.0, remaining))
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
            elif kind == "compaction_end":
                if common.compaction_succeeded(event):
                    metrics["compactions"] += 1
                else:
                    metrics["compactionFailures"] += 1
                    if event.get("aborted"):
                        metrics["compactionAborted"] += 1
    finally:
        code = runner.wait(60)
        runner.close()
        metrics["exit"] = code
        metrics["wallS"] = round(time.monotonic() - started, 2)
        header = common.stream_session_header(stdout_path)
        if header is not None:
            metrics["streamHeader"] = header
            metrics["streamHeaderVersion"] = header.get("version")
            if header.get("type") == "session":
                metrics["openedSessionId"] = header.get("id")
        metrics["stderrTail"] = common.log_tail(stderr_path, 4)
    return metrics




def main(argv=None):
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--pir", default=None)
    ap.add_argument("--iterations", type=int, default=10,
                    help="crash/reopen iterations (default 10)")
    ap.add_argument("--model", default="mock/soak-model")
    ap.add_argument("--agent-dir", default=None)
    ap.add_argument("--log", default=os.path.join(common.REPO, "logs", "soak", "crash-reopen"))
    ap.add_argument("--session", default=None,
                    help="existing session file (copied into the log dir unless "
                         "--session-in-place)")
    ap.add_argument("--session-in-place", action="store_true",
                    help="crash the given --session file directly (no copy)")
    ap.add_argument("--session-id", default=None,
                    help="session id for a fresh session (default: uuid4)")
    ap.add_argument("--bootstrap-turns", type=int, default=1,
                    help="normal turns before the first crash (default 1; 0 for "
                         "an existing --session)")
    ap.add_argument("--thinking", default="off")
    ap.add_argument("--turn-prompt", default="crash turn %s" % common.MARKER_TEXT)
    ap.add_argument("--bootstrap-prompt", default="bootstrap %s" % common.MARKER_TEXT)
    ap.add_argument("--recovery-prompt", default="recovery %s" % common.MARKER_TEXT)
    ap.add_argument("--kill-min", type=float, default=0.05,
                    help="minimum seconds between turn_start and the kill (default 0.05)")
    ap.add_argument("--kill-max", type=float, default=1.0,
                    help="maximum seconds between turn_start and the kill (default 1.0)")
    ap.add_argument("--startup-timeout", type=float, default=120.0,
                    help="seconds to wait for the turn to start before giving up "
                         "(default 120)")
    ap.add_argument("--slow-delay-ms", type=int, default=5000,
                    help="mock provider delay for the crashed turn, so the kill "
                         "lands mid-request (default 5000)")
    ap.add_argument("--request-timeout", type=float, default=180.0,
                    help="per-request hang timeout for normal/recovery turns "
                         "(default 180)")
    ap.add_argument("--turn-timeout", type=float, default=600.0,
                    help="hard wall-clock cap for one normal/recovery turn "
                         "(default 600)")
    ap.add_argument("--timeout", type=float, default=3600.0,
                    help="total wall-clock budget (default 3600)")
    ap.add_argument("--context-window", type=int, default=131072)
    ap.add_argument("--reserve-tokens", type=int, default=16384)
    ap.add_argument("--keep-recent-tokens", type=int, default=20000)
    ap.add_argument("--extra-arg", action="append", default=[])
    ap.add_argument("--seed", type=int, default=None,
                    help="seed the kill-delay RNG for reproducibility")
    ap.add_argument("--mock", dest="mock", action="store_true", default=None)
    ap.add_argument("--no-mock", dest="mock", action="store_false")
    args = ap.parse_args(argv)

    if args.iterations < 1:
        print("ERROR: --iterations must be >= 1", file=sys.stderr)
        return 2
    if args.kill_min < 0 or args.kill_max < args.kill_min:
        print("ERROR: need 0 <= --kill-min <= --kill-max", file=sys.stderr)
        return 2
    if args.session and args.session_id:
        print("ERROR: --session and --session-id are mutually exclusive", file=sys.stderr)
        return 2
    if args.session_id:
        try:
            args.session_id = str(uuid.UUID(args.session_id))
        except ValueError:
            print("ERROR: --session-id must be a UUID: %s" % args.session_id,
                  file=sys.stderr)
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
    if mock_mode and args.slow_delay_ms <= args.kill_max * 1000.0:
        print("[soak] note: --slow-delay-ms (%d) is not longer than --kill-max "
              "(%.0f ms); a kill can race the end of the turn. The iteration "
              "then fails loudly instead of pretending it killed a live turn — "
              "raise --slow-delay-ms or lower --kill-max."
              % (args.slow_delay_ms, args.kill_max * 1000.0))

    log_dir = common.resolve_path(args.log)
    common.ensure_dir(log_dir)
    work_dir = common.ensure_dir(os.path.join(log_dir, "work"))
    session_dir = common.ensure_dir(os.path.join(log_dir, "sessions"))

    mock = None
    agent_dir = None
    if mock_mode:
        provider, model_id = args.model.split("/", 1)
        mock = common.MockProvider(
            model_id=model_id or "soak-model",
            provider=provider,
            response_chars=500,
            delay_ms=0,
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
        # --no-mock without --agent-dir: resolve the same default the child
        # will use (env overrides, then ~/.pi/agent) so the durable journal
        # scan below never silently skips the real agent directory.
        agent_dir = common.resolve_agent_dir()
    env = common.build_child_env(agent_dir)
    random.seed(args.seed)

    session_path = None
    expected_id = None
    session_id = None
    iterations = []
    failure = None
    harness_error = False

    try:
        if args.session:
            source = common.resolve_path(args.session)
            if not os.path.exists(source):
                print("ERROR: --session not found: %s" % source, file=sys.stderr)
                return 2
            report = common.inspect_jsonl(source)
            if not report["header_ok"]:
                print("ERROR: --session has no valid v3 header: %s" % source,
                      file=sys.stderr)
                return 2
            expected_id = report["header"]["id"]
            if args.session_in_place:
                session_path = source
            else:
                session_path = os.path.join(work_dir, "soak-session.jsonl")
                shutil.copyfile(source, session_path)
                print("[soak] copied --session to %s" % session_path)
        else:
            session_id = args.session_id or str(uuid.uuid4())

        print("[soak] pir=%s" % pie)
        print("[soak] model=%s mock=%s iterations=%d kill=[%.2f,%.2f]s"
              % (args.model, "yes" if mock_mode else "no", args.iterations,
                 args.kill_min, args.kill_max))
        print("[soak] log=%s" % log_dir)

        # Bootstrap: a fresh session is created with --session-id, then all
        # later runs append with an explicit --session path (the same shape a
        # user's long-lived session has).
        for index in range(args.bootstrap_turns):
            if mock is not None:
                mock.set_delay(0)
            argv = common.build_pir_argv(
                pie, args.model, thinking=args.thinking, session_dir=session_dir,
                session_id=session_id if session_path is None else None,
                session_path=session_path,
                prompts=[args.bootstrap_prompt], extra_args=args.extra_arg,
            )
            stdout_path = os.path.join(log_dir, "bootstrap-%03d.stdout.jsonl" % index)
            stderr_path = os.path.join(log_dir, "bootstrap-%03d.stderr.txt" % index)
            print("[soak] bootstrap %d/%d ..." % (index + 1, args.bootstrap_turns))
            metrics = run_turn(argv, work_dir, env, stdout_path, stderr_path,
                               args.request_timeout, args.turn_timeout)
            if (metrics["exit"] != 0 or metrics["agentEnds"] < 1
                    or metrics["hang"] or metrics["unparsed"]):
                print("ERROR: bootstrap turn failed: exit=%s agent_end=%d "
                      "hang=%s unparsed=%d"
                      % (metrics["exit"], metrics["agentEnds"], metrics["hang"],
                         metrics["unparsed"]),
                      file=sys.stderr)
                for line in metrics["stderrTail"]:
                    print("  stderr: %s" % line, file=sys.stderr)
                return 2
            if mock_mode and common.MOCK_OK not in metrics["lastText"]:
                print("ERROR: bootstrap response did not come from the mock",
                      file=sys.stderr)
                for line in metrics["stderrTail"]:
                    print("  stderr: %s" % line, file=sys.stderr)
                return 2
            if session_path is None:
                session_path = common.find_session_file(session_dir, session_id)
                if session_path is None:
                    print("ERROR: session file not found for id %s" % session_id,
                          file=sys.stderr)
                    return 2
                report = common.inspect_jsonl(session_path)
                expected_id = (report["header"] or {}).get("id")
            report = common.inspect_jsonl(session_path)
            if not report["header_ok"] or report["malformed_complete"]:
                print("ERROR: session is not clean after bootstrap: %s" % session_path,
                      file=sys.stderr)
                return 2
            bootstrap_header_reason = common.header_version_reason(report)
            if bootstrap_header_reason:
                print("ERROR: %s" % bootstrap_header_reason, file=sys.stderr)
                return 2
            expected_id = report["header"]["id"]

        if session_path is None:
            print("ERROR: no session (set --bootstrap-turns >= 1 or pass --session)",
                  file=sys.stderr)
            return 2

        iterations_path = os.path.join(log_dir, "iterations.jsonl")
        try:
            os.remove(iterations_path)
        except OSError:
            pass
        total_deadline = time.monotonic() + args.timeout

        for index in range(1, args.iterations + 1):
            if time.monotonic() > total_deadline:
                failure = "total timeout %.0fs exceeded at iteration %d" % (
                    args.timeout, index)
                break
            if mock is not None:
                mock.set_delay(args.slow_delay_ms)
            before = common.inspect_jsonl(session_path)
            # Crash baseline captured BEFORE the crashed process starts: it is
            # the exact pre-crash size/line/parse state every post-crash check
            # compares against (shrinkage, added malformed lines, header id).
            crash_baseline = {
                "bytes": before["bytes"],
                "lines": before["lines"],
                "parsed": before["parsed"],
                "malformedComplete": len(before["malformed_complete"]),
                "headerVersion": before.get("header_version"),
                "headerId": (before["header"] or {}).get("id"),
                "tornTail": before["torn_tail"],
                "recordedAt": round(time.time(), 3),
            }
            if not before["exists"] or not before["header_ok"]:
                failure = ("iteration %d: crash baseline is not a valid v3 "
                           "session: %s" % (index, session_path))
                break
            argv = common.build_pir_argv(
                pie, args.model, thinking=args.thinking, session_path=session_path,
                prompts=[args.turn_prompt], extra_args=args.extra_arg,
            )
            stdout_path = os.path.join(log_dir, "crash-%03d.stdout.jsonl" % index)
            stderr_path = os.path.join(log_dir, "crash-%03d.stderr.txt" % index)
            runner = common.LineProcess(argv, cwd=work_dir, env=env,
                                        stdout_path=stdout_path,
                                        stderr_path=stderr_path)
            # Background tree sampler. It must NOT block the kill path: a
            # synchronous CIM query between the random delay and the kill can
            # outlast the turn (especially on Windows), turning the "crash"
            # into a clean completion. The sampler records member pids while
            # the root is alive; after the kill they are re-checked for
            # orphans (zombies excluded).
            sampler = common.TreeSampler(runner.proc.pid, interval_s=0.2)
            sampler.start()
            turn_started = False
            killed = False
            kill_leftovers = []
            startup_deadline = time.monotonic() + args.startup_timeout
            start_wall = time.monotonic()
            try:
                # Wait for proof the turn is actually running. turn_start is
                # emitted before the provider request; message_start is the
                # first frame of the stream.
                while time.monotonic() < startup_deadline:
                    if runner.poll() is not None:
                        break
                    try:
                        line = runner.next_line(min(1.0, max(
                            0.01, startup_deadline - time.monotonic())))
                    except common.StreamTimeout:
                        continue
                    except common.StreamClosed:
                        break
                    event = common.parse_event(line)
                    if event and event.get("type") in (
                            "turn_start", "message_start", "tool_execution_start"):
                        turn_started = True
                        break
                if not turn_started:
                    runner.kill()
                    runner.wait(30)
                    runner.close()
                    failure = ("iteration %d: turn never started (exit=%s)"
                               % (index, runner.proc.returncode))
                    break
                delay = random.uniform(args.kill_min, args.kill_max)
                time.sleep(delay)
                if runner.poll() is not None:
                    runner.close()
                    failure = ("iteration %d: turn completed before the kill "
                               "(delay %.2fs); raise --slow-delay-ms or lower "
                               "--kill-max" % (index, delay))
                    break
                kill_wall = time.monotonic()
                runner.kill()
                killed = True
                crash_exit = runner.wait(60)
                trailing = runner.drain(0.1)
                runner.close()
                # Zombie/orphan handling: every process that belonged to the
                # killed tree (the root included) must be gone after the
                # kill+reap. On POSIX, zombies are reported as dead by
                # alive_pids(); the root is reaped by runner.wait().
                kill_leftovers = common.alive_pids(sampler.seen_pids)
            except Exception:
                runner.close()
                raise
            finally:
                sampler.stop()
                if killed:
                    # The sampler may have observed a member between the kill
                    # and the stop; re-check the full seen set once more.
                    leftovers = common.alive_pids(sampler.seen_pids)
                    if leftovers:
                        kill_leftovers = leftovers

            if mock is not None:
                mock.set_delay(0)

            # --- durability check of the crashed file -------------------
            crashed = common.inspect_jsonl(session_path)
            before_malformed = crash_baseline["malformedComplete"]
            scan_roots = [
                (session_path, "session_header"),
                # Durable journals first so dedup keeps the strict mode for
                # them; the resolved agent dir itself is included even in
                # --no-mock (the real ~/.pi/agent or env override), never
                # silently skipped.
                (os.path.join(agent_dir, "durable"), "durable")
                if agent_dir else None,
                (agent_dir, "report") if agent_dir else None,
                # Sibling sessions from earlier runs may carry a repaired
                # torn line from their own crash: readability only.
                (session_dir, "report"),
            ]
            scans = common.scan_jsonl_roots(
                [root for root in scan_roots if root])
            reasons = common.scan_jsonl_failures(scans)
            if not crashed["header_ok"]:
                reasons.append("crashed session header invalid")
            crash_header_reason = common.header_version_reason(crashed)
            if crash_header_reason:
                reasons.append("crashed session: %s" % crash_header_reason)
            if expected_id and (crashed["header"] or {}).get("id") != expected_id:
                reasons.append("crashed session id changed")
            if crashed["bytes"] < crash_baseline["bytes"]:
                reasons.append("crashed session shrank (baseline=%d crashed=%d)"
                               % (crash_baseline["bytes"], crashed["bytes"]))
            if crashed["parsed"] < crash_baseline["parsed"]:
                reasons.append(
                    "crashed session lost parseable records (baseline=%d "
                    "crashed=%d)" % (crash_baseline["parsed"], crashed["parsed"]))
            if len(crashed["malformed_complete"]) > before_malformed:
                reasons.append(
                    "crash added %d malformed complete line(s)"
                    % (len(crashed["malformed_complete"]) - before_malformed))
            if crash_exit is None:
                reasons.append("crashed process did not exit after the kill")
            elif crash_exit == 0:
                reasons.append("crashed process exited 0; the kill raced the end "
                               "of the turn")
            if kill_leftovers:
                reasons.append(
                    "kill left %d process(es) alive from the crashed tree "
                    "(zombies excluded): %s"
                    % (len(kill_leftovers), kill_leftovers[:8]))

            # --- recovery turn -------------------------------------------
            pre_recovery_bytes = crashed["bytes"]
            pre_recovery_malformed = len(crashed["malformed_complete"])
            pre_recovery_torn_malformed = bool(
                crashed["torn_tail"] and not crashed["torn_tail_parsed"])
            recovery_argv = common.build_pir_argv(
                pie, args.model, thinking=args.thinking, session_path=session_path,
                prompts=[args.recovery_prompt], extra_args=args.extra_arg,
            )
            recovery_stdout = os.path.join(
                log_dir, "recovery-%03d.stdout.jsonl" % index)
            recovery_stderr = os.path.join(
                log_dir, "recovery-%03d.stderr.txt" % index)
            recovery = run_turn(recovery_argv, work_dir, env,
                                recovery_stdout, recovery_stderr,
                                args.request_timeout, args.turn_timeout)
            if recovery["hang"]:
                reasons.append("recovery hang: %s" % recovery["hang"])
            if recovery["exit"] != 0:
                reasons.append("recovery exit code %s" % recovery["exit"])
            if recovery["agentEnds"] < 1:
                reasons.append("recovery produced no agent_end")
            if recovery["assistantMessages"] < 1:
                reasons.append("recovery produced no assistant message")
            if recovery["badStops"]:
                reasons.append("recovery assistant stopReason error/aborted")
            if mock_mode and common.MOCK_OK not in recovery["lastText"]:
                reasons.append("recovery response did not come from the mock")
            if recovery["openedSessionId"] != expected_id:
                reasons.append("recovery opened session %r, expected %r"
                               % (recovery["openedSessionId"], expected_id))
            recovery_header_reason = common.header_version_reason(
                recovery.get("streamHeader"))
            if recovery_header_reason:
                reasons.append("recovery JSON stream: %s" % recovery_header_reason)
            if recovery["compactionFailures"]:
                reasons.append(
                    "recovery compaction failures: %d (%d aborted; aborted is "
                    "never counted as successful)"
                    % (recovery["compactionFailures"],
                       recovery["compactionAborted"]))
            if recovery["unparsed"]:
                reasons.append("recovery emitted %d unparsed stdout lines"
                               % recovery["unparsed"])

            appended = common.inspect_appended_region(session_path, pre_recovery_bytes)
            if not appended["clean"]:
                reasons.extend("appended region: %s" % error
                               for error in appended["errors"][:4])
            # Recovery success means the recovery turn was really persisted:
            # a non-empty byte append is not enough (a lone repair newline, or
            # an unparsable tail, would pass a bytes-only check).
            if appended["lines"] <= 0:
                reasons.append("recovery appended no session lines")
            if appended["parsed"] <= 0:
                reasons.append(
                    "recovery appended no parseable session record (bytes=%d, "
                    "lines=%d, leading_newline=%s)"
                    % (appended["bytes"], appended["lines"],
                       appended["leading_newline"]))
            if appended["parsed"] > 0 \
                    and not appended["last_line_parsed"]:
                reasons.append("the final appended session line did not parse")
            if appended["message_roles"].get("assistant", 0) < 1:
                reasons.append("recovery appended no assistant message")

            after = common.inspect_jsonl(session_path)
            after_header_id = (after["header"] or {}).get("id")
            allowed_malformed = pre_recovery_malformed + (
                1 if pre_recovery_torn_malformed else 0)
            if not after["header_ok"]:
                reasons.append("session header invalid after recovery")
            after_header_reason = common.header_version_reason(after)
            if after_header_reason:
                reasons.append("post-recovery session: %s" % after_header_reason)
            if after_header_id != expected_id:
                reasons.append("session header id %r changed (expected %r)"
                               % (after_header_id, expected_id))
            if after["bytes"] < pre_recovery_bytes:
                reasons.append("session shrank after recovery (pre=%d after=%d)"
                               % (pre_recovery_bytes, after["bytes"]))
            if len(after["malformed_complete"]) > allowed_malformed:
                reasons.append(
                    "recovery added %d malformed complete line(s)"
                    % (len(after["malformed_complete"]) - allowed_malformed))
            if not after["ends_with_newline"]:
                reasons.append("session does not end with a newline after recovery")
            # A successfully parsed tail: the recovery must have repaired the
            # torn line (the appended region then starts with the repair
            # newline) and left no unterminated final line.
            if after["torn_tail"]:
                reasons.append(
                    "session still has an unterminated/torn final line after "
                    "recovery (tail was not successfully parsed)")
            if not crashed["ends_with_newline"] and not appended["leading_newline"]:
                reasons.append(
                    "the crashed file lacked a trailing newline but the "
                    "recovery did not append the documented repair newline")

            record = {
                "iteration": index,
                "killed": True,
                "turnStarted": turn_started,
                "killDelayS": round(delay, 4),
                "crashExit": crash_exit,
                "crashWallS": round(kill_wall - start_wall, 2),
                "trailingEventLines": len(trailing),
                "crashBaseline": crash_baseline,
                "beforeBytes": before["bytes"],
                "crashedBytes": crashed["bytes"],
                "crashedHeaderVersion": crashed.get("header_version"),
                "tornTail": crashed["torn_tail"],
                "tornTailParsed": crashed["torn_tail_parsed"],
                "preKillTreePids": len(sampler.seen_pids),
                "killLeftovers": kill_leftovers[:16],
                "preRecoveryBytes": pre_recovery_bytes,
                "preRecoveryMalformed": pre_recovery_malformed,
                "recoveryExit": recovery["exit"],
                "recoveryWallS": recovery["wallS"],
                "recoveryAgentEnds": recovery["agentEnds"],
                "recoveryAssistantMessages": recovery["assistantMessages"],
                "recoveryOpenedSessionId": recovery["openedSessionId"],
                "recoveryStreamHeaderVersion": recovery.get(
                    "streamHeaderVersion"),
                "recoveryCompactions": recovery["compactions"],
                "recoveryCompactionFailures": recovery["compactionFailures"],
                "recoveryUnparsed": recovery["unparsed"],
                "appendedClean": appended["clean"],
                "appendedLines": appended["lines"],
                "appendedParsed": appended["parsed"],
                "appendedLeadingNewline": appended["leading_newline"],
                "appendedMessageRoles": appended["message_roles"],
                "afterBytes": after["bytes"],
                "afterBytesDelta": after["bytes"] - pre_recovery_bytes,
                "afterHeaderId": after_header_id,
                "afterHeaderIdMatches": after_header_id == expected_id,
                "afterHeaderVersion": after.get("header_version"),
                "afterTornTail": after["torn_tail"],
                "afterMalformedComplete": len(after["malformed_complete"]),
                "scanFiles": len(scans),
                "scanMalformed": sum(len(report["malformed_complete"])
                                     for report in scans),
                "scanTornTails": sum(1 for report in scans if report["torn_tail"]),
                "reasons": reasons,
            }
            iterations.append(record)
            common.append_jsonl(iterations_path, record)
            print("[crash] %d/%d killed after %.2fs exit=%s; pre=%d B "
                  "torn=%s; recovery exit=%s agent_end=%d appended_clean=%s"
                  % (index, args.iterations, delay, crash_exit, crashed["bytes"],
                     crashed["torn_tail"], recovery["exit"],
                     recovery["agentEnds"], appended["clean"]))
            for reason in reasons:
                print("    - %s" % reason)
            if reasons:
                failure = "iteration %d: %s" % (index, "; ".join(reasons))
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

    reasons = []
    if failure:
        reasons.append(failure)
    completed = len(iterations)
    if completed != args.iterations:
        reasons.append("completed %d/%d crash/reopen iterations"
                       % (completed, args.iterations))
    mid_turn = sum(1 for item in iterations if item["killed"] and item["turnStarted"])
    if mid_turn != completed:
        reasons.append("only %d/%d kills were confirmed mid-turn" % (mid_turn, completed))
    final_report = common.inspect_jsonl(session_path) if session_path else {}
    final_header_reason = common.header_version_reason(final_report) \
        if session_path else "no session path"
    if final_header_reason:
        reasons.append("final session: %s" % final_header_reason)
    if final_report.get("malformed_complete"):
        reasons.append("final session has %d malformed complete line(s)"
                       % len(final_report["malformed_complete"]))
    leftovers = sum(1 for item in iterations if item.get("killLeftovers"))
    if leftovers:
        reasons.append("%d iteration(s) left processes alive after the kill"
                       % leftovers)

    summary = {
        "script": "soak_crash_reopen",
        "verdict": "PASS" if not reasons else "FAIL",
        "reasons": reasons,
        "pir": pie,
        "model": args.model,
        "agentDir": agent_dir,
        "mock": mock_mode,
        "iterations": args.iterations,
        "crashReopens": completed,
        "midTurnKills": mid_turn,
        "killsWithLeftoverProcesses": leftovers,
        "session": {
            "id": expected_id,
            "path": session_path,
            "finalBytes": final_report.get("bytes"),
            "finalHeaderVersion": final_report.get("header_version"),
            "finalHeaderVersionOk": final_report.get("header_version_ok"),
            "finalMalformedComplete": len(
                final_report.get("malformed_complete") or []),
            "finalTornTail": final_report.get("torn_tail"),
        },
        "thresholds": {
            "killMinS": args.kill_min,
            "killMaxS": args.kill_max,
            "slowDelayMs": args.slow_delay_ms,
            "requestTimeoutS": args.request_timeout,
            "headerVersion": 3,
        },
        "iterationsDetail": iterations,
    }

    print("")
    print("=== soak_crash_reopen ===")
    print("  crash/reopens: %d/%d (mid-turn kills: %d)"
          % (completed, args.iterations, mid_turn))
    print("  session:       %s" % session_path)
    print("  verdict:       %s" % summary["verdict"])
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
