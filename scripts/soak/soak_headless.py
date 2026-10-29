#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Long-run headless soak: N turns on one session without degradation.

What it drives
--------------
One ``pir --mode json --print ...`` process (or a small number of batches when
the platform command-line limit is reached) that executes N prompts against a
single persisted session. The turn schedule alternates:

* plain text turns;
* tool-call turns (``bash`` and ``read``, every ``--tool-every`` turns);
* context growth (the mock's text responses and tool results add real bytes
  to the transcript, so the request body grows every turn);
* periodic threshold compaction (the mock reports usage derived from the real
  request size; with the mock's context window the session crosses the
  threshold every ~N turns and ``compaction_start`` / ``compaction_end``
  frames appear in the JSON stream).

Every ``--interval`` completed turns the driver samples the WHOLE process
tree's working set and member count (the tool children included). A run fails
when any of these holds:

* a request produces no event for ``--request-timeout`` seconds (a hung
  provider or tool call) - the tree is killed and the run fails;
* RSS exceeds ``--rss-max-mb``, its least-squares slope exceeds
  ``--rss-slope-mb`` MB/turn, or the last-quarter median exceeds the
  first-quarter median by ``--rss-growth-mb``;
* fewer than ``--min-tree-samples`` process-tree samples (default 4) or
  ``--min-process-samples`` process-count readings (default 4) were collected
  (missing instrumentation fails instead of silently passing);
* the process tree exceeds ``--max-processes``, its least-squares slope over
  the sample window implies a rise above ``--process-growth``, or its
  quarterly-median growth exceeds ``--process-growth`` (orphaned tool
  children); surviving pids after a batch exits are reported as orphans;
* fewer than N turns complete, any turn exits non-zero, a tool reports an
  error, a compaction fails/aborts, or the expected compaction/tool-call
  counts are not reached (the mock compaction floor is derived from measured
  context growth, not a fixed number);
* the session file is unreadable, has a malformed COMPLETE line, does not
  grow, its header is not strict v3 with the expected id, or a normal run
  left it without a trailing newline.

The default provider is a local OpenAI-compatible mock (no API key, no
network); pass ``--model provider/model --no-mock`` to soak a real model.

Usage
-----
    python scripts/soak/soak_headless.py --turns 300 --interval 10
    python scripts/soak/soak_headless.py --turns 1000 --log logs/soak/headless-1k
    python scripts/soak/soak_headless.py --model openai/gpt-5 --no-mock

Exit codes: 0 = pass, 1 = soak verdict failed, 2 = setup/harness error.
"""

import argparse
import os
import sys
import time
import traceback
import uuid

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import soak_common as common


def build_prompts(turns, tool_every):
    """Short prompts; growth comes from responses/tool output, not argv."""
    prompts = []
    expected_tools = 0
    for index in range(1, turns + 1):
        if tool_every and index % tool_every == 0:
            expected_tools += 1
            ordinal = index // tool_every
            marker = common.MARKER_TOOL_BASH if ordinal % 2 else common.MARKER_TOOL_READ
            prompts.append("t%05d %s" % (index, marker))
        else:
            prompts.append("t%05d %s" % (index, common.MARKER_TEXT))
    return prompts, expected_tools


def apply_event(event, state):
    """Apply one parsed JSON event to ``state``; return True on agent_end.

    Shared by the normal read loop and the post-expected-turn drain so a
    trailing agent_end/unparsed/compaction frame cannot disappear.
    """
    kind = event.get("type")
    state["events"] += 1
    if kind == "agent_start":
        state["agent_starts"] += 1
    elif kind == "agent_end":
        state["agent_ends"] += 1
        state["completed"] += 1
        state["last_agent_end_at"] = time.monotonic()
        return True
    elif kind == "tool_execution_end":
        state["tool_calls"] += 1
        if event.get("isError"):
            state["tool_errors"] += 1
    elif kind == "compaction_start":
        state["compactions_started"] += 1
    elif kind == "compaction_end":
        if common.compaction_succeeded(event):
            state["compactions"] += 1
        else:
            state["compaction_failures"] += 1
            if event.get("aborted"):
                state["compaction_aborted"] += 1
    elif kind == "auto_retry_start":
        state["retries"] += 1
    elif kind == "error":
        state["error_events"] += 1
    elif kind == "message_end":
        message = event.get("message") or {}
        if message.get("role") == "assistant":
            state["assistant_messages"] += 1
            if message.get("stopReason") in ("error", "aborted"):
                state["bad_stops"] += 1
            text = ""
            content = message.get("content")
            if isinstance(content, list):
                text = "".join(
                    part.get("text", "") for part in content
                    if isinstance(part, dict) and part.get("type") == "text"
                )
            if common.MOCK_OK in text:
                state["mock_texts"] += 1
    elif kind == "message_update":
        update = event.get("assistantMessageEvent") or {}
        if update.get("type") == "error":
            state["error_events"] += 1
    return False


def _record_root_sample(state, args, samples, sample_path, batch_index, turn):
    record = {
        "x": turn,
        "turn": turn,
        "batch": batch_index,
        "rss_mb": None,
        "processes": None,
        "scope": "root",
        "elapsed_s": round(time.monotonic() - state["started"], 2),
    }
    root_rss = common.sample_root_rss_mb(state["pid"])
    if root_rss is None:
        return
    record["rss_mb"] = root_rss
    samples.append(record)
    common.append_jsonl(sample_path, record)
    if turn == 0:
        print("[soak] baseline root rss=%.1f MB" % record["rss_mb"])
    else:
        print("[soak] turn %d/%d root rss=%.1f MB elapsed=%.1fs"
              % (turn, args.turns, record["rss_mb"], record["elapsed_s"]))


def _record_post_agent_end_sample(state, runner, args, samples, sample_path,
                                  batch_index):
    """Anchor the tree curve after the final ``agent_end`` of a batch.

    The background sampler runs for the whole batch, but its last CIM query
    can start before ``agent_end`` and the child can exit immediately after
    the turn. One explicit tree reading taken while the child is still alive
    proves the post-turn memory was observed (the same rule the reopen driver
    applies). When the child has already exited the check is recorded as
    skipped rather than silently passing.
    """
    if runner.poll() is not None:
        state["post_agent_end_skipped"] += 1
        return
    tree_now = common.sample_process_tree(runner.proc.pid)
    if tree_now is None:
        # The root disappeared while the CIM query ran: the sampler's own
        # points still cover the turn; record the skip explicitly.
        state["post_agent_end_skipped"] += 1
        return
    record = {
        "x": state["completed"],
        "turn": state["completed"],
        "batch": batch_index,
        "rss_mb": tree_now["rss_mb"],
        "processes": tree_now["processes"],
        "scope": "tree",
        "post_agent_end": True,
        "elapsed_s": round(time.monotonic() - state["started"], 2),
    }
    samples.append(record)
    common.append_jsonl(sample_path, record)
    state["post_agent_end_samples"] += 1


def run_batch(runner, expected_turns, state, args, samples, sample_path,
              total_deadline, batch_index):
    """Consume one batch's events; returns ``(ok, failure_reason)``."""
    batch_completed = 0
    baseline_taken = False
    state["pid"] = runner.proc.pid
    while True:
        now = time.monotonic()
        if now > total_deadline:
            runner.kill()
            return False, "total timeout %.0fs exceeded" % args.timeout
        if not baseline_taken and state["completed"] == 0:
            # One fast baseline sample (x=0), extra to the --interval cadence:
            # the trend checks need an anchor before the first growth turn.
            # tasklist is ~50 ms, so this does not stall the event loop; the
            # full process-tree curve comes from the background TreeSampler.
            _record_root_sample(state, args, samples, sample_path,
                                batch_index, 0)
            baseline_taken = True
        wait = args.request_timeout
        remaining_total = total_deadline - time.monotonic()
        if remaining_total < wait:
            wait = remaining_total
        try:
            line = runner.next_line(max(0.01, wait))
        except common.StreamTimeout:
            runner.kill()
            if time.monotonic() >= total_deadline:
                return False, "total timeout %.0fs exceeded" % args.timeout
            return False, (
                "request hang: no event for %.0fs at turn %d/%d "
                "(batch %d)" % (args.request_timeout, state["completed"],
                                args.turns, batch_index)
            )
        except common.StreamClosed:
            break
        event = common.parse_event(line)
        if event is None:
            state["unparsed"] += 1
            continue
        if apply_event(event, state):
            batch_completed += 1
            if (state["completed"] % args.interval == 0
                    or state["completed"] == args.turns):
                # Fast root RSS at the exact K-turn boundary. The tree total
                # (children included) is sampled continuously in the
                # background; a blocking CIM query here would let a fast
                # child run far ahead and skip boundaries.
                _record_root_sample(state, args, samples, sample_path,
                                    batch_index, state["completed"])
            if batch_completed > expected_turns:
                return False, "batch %d produced more agent_end events than prompts" % batch_index
        if batch_completed >= expected_turns:
            # The final prompt of this batch has ended. Take an explicit
            # post-agent_end tree point before draining to EOF.
            _record_post_agent_end_sample(
                state, runner, args, samples, sample_path, batch_index)
            # All prompts in this batch completed; the child should exit.
            # Keep draining to EOF, but parse the tail: trailing agent_end,
            # unparsed lines, compaction_end frames and errors are part of the
            # verdict, not disposable output.
            extra_agent_ends = 0
            try:
                while True:
                    tail_line = runner.next_line(args.request_timeout)
                    tail_event = common.parse_event(tail_line)
                    if tail_event is None:
                        state["unparsed"] += 1
                        continue
                    if apply_event(tail_event, state):
                        extra_agent_ends += 1
            except common.StreamClosed:
                pass
            except common.StreamTimeout:
                runner.kill()
                return False, "batch %d did not exit after its last turn" % batch_index
            if extra_agent_ends:
                return False, (
                    "batch %d produced %d more agent_end event(s) than prompts"
                    % (batch_index, extra_agent_ends))
            break
    code = runner.wait(60)
    if code is None:
        runner.kill()
        return False, "batch %d did not exit after stdout closed" % batch_index
    if code != 0:
        return False, "batch %d exited %s" % (batch_index, code)
    if batch_completed != expected_turns:
        return False, ("batch %d completed %d/%d turns before exit"
                       % (batch_index, batch_completed, expected_turns))
    return True, None


def derive_min_compactions(turns, context_window, reserve_tokens,
                          keep_recent_tokens, growth_chars, tool_every,
                          tool_output_lines, measured_growth_tokens=None):
    """Expected threshold compactions from context window and per-turn growth.

    The trigger is ``context_tokens > context_window - reserve_tokens``; a
    successful compaction brings the resident context back to the model's
    ``keepRecentTokens`` target, so each later run needs another
    ``trigger - keep_recent`` tokens. Growth is estimated at four characters
    per token (responses plus the amortized tool output). Returns 0 when the
    run is too short to cross the first threshold; an explicit
    ``--min-compactions`` always wins over this estimate.

    ``measured_growth_tokens`` is the context growth actually appended to the
    session (see [`common.measured_growth_tokens`]). When present it replaces
    the per-turn estimate: the derived floor is then "at least one compaction
    per measured context-window worth of growth", i.e.
    ``max(1, floor(growth / trigger))`` once ``growth >= trigger`` — derived
    from what the run really grew, never a fixed small number.
    """
    trigger = max(1, context_window - reserve_tokens)
    if turns <= 0:
        return 0
    if measured_growth_tokens is not None:
        return common.compactions_for_growth(
            measured_growth_tokens, context_window, reserve_tokens)
    per_turn_chars = max(0, growth_chars)
    if tool_every and tool_every > 0:
        per_turn_chars += max(0, tool_output_lines) * 80.0 / tool_every
    per_turn_tokens = per_turn_chars / 4.0
    if per_turn_tokens <= 0:
        return 0
    first_at = trigger / per_turn_tokens
    if turns < first_at:
        return 0
    interval_tokens = max(1.0, trigger - max(0, keep_recent_tokens))
    return 1 + int((turns - first_at) // (interval_tokens / per_turn_tokens))


def main(argv=None):
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("--pir", default=None,
                    help="pir executable (default: $PIR_SOAK_EXE, then "
                         "target/release, target-alt/release, target/debug)")
    ap.add_argument("--turns", type=int, default=200,
                    help="number of user prompts to execute (default 200)")
    ap.add_argument("--interval", type=int, default=10,
                    help="record a root-RSS sample every K turns (default 10); "
                         "the full process tree is sampled continuously in the background")
    ap.add_argument("--sample-seconds", type=float, default=1.0,
                    help="background process-tree sampling interval (default 1.0; "
                         "one CIM query is the real throttle on Windows)")
    ap.add_argument("--model", default="mock/soak-model",
                    help="provider/model (default mock/soak-model; a non-mock "
                         "value selects a real provider unless --mock is set)")
    ap.add_argument("--agent-dir", default=None,
                    help="PIR_AGENT_DIR for the child (mock default: <log>/agent)")
    ap.add_argument("--log", default=os.path.join(common.REPO, "logs", "soak", "headless"),
                    help="artifact directory (default logs/soak/headless)")
    ap.add_argument("--thinking", default="off", help="thinking level (default off)")
    ap.add_argument("--request-timeout", type=float, default=180.0,
                    help="fail when no event arrives for this many seconds "
                         "(per-request hang detector; default 180)")
    ap.add_argument("--timeout", type=float, default=3600.0,
                    help="total wall-clock budget in seconds (default 3600)")
    ap.add_argument("--tool-every", type=int, default=4,
                    help="every K-th turn is a bash/read tool call (default 4; 0 disables)")
    ap.add_argument("--growth-chars", type=int, default=4000,
                    help="mock text response size per turn (context growth; default 4000)")
    ap.add_argument("--tool-output-lines", type=int, default=25,
                    help="bash output lines / read line limit per tool turn (default 25)")
    ap.add_argument("--context-window", type=int, default=65536,
                    help="mock model context window (default 65536)")
    ap.add_argument("--reserve-tokens", type=int, default=8192,
                    help="mock compaction reserveTokens (default 8192)")
    ap.add_argument("--keep-recent-tokens", type=int, default=12288,
                    help="mock compaction keepRecentTokens (default 12288)")
    ap.add_argument("--mock-delay-ms", type=int, default=0,
                    help="artificial per-request provider delay in the mock (default 0)")
    ap.add_argument("--warmup-turns", type=int, default=10,
                    help="exclude the first K turns from the RSS slope/growth "
                         "trend (startup ramp; default 10)")
    ap.add_argument("--min-tree-samples", type=int, default=4,
                    help="minimum process-tree RSS samples required for a PASS "
                         "(default 4); fewer samples fail the run instead of "
                         "silently passing (raise for long runs, lower only for "
                         "a deliberately short smoke run)")
    ap.add_argument("--min-process-samples", type=int, default=4,
                    help="minimum process-count readings required for a PASS "
                         "(default 4; the process-tree sampler is the only "
                         "orphan-leak coverage)")
    ap.add_argument("--rss-slope-mb", type=float, default=0.10,
                    help="maximum least-squares RSS slope in MB/turn (default 0.10)")
    ap.add_argument("--rss-growth-mb", type=float, default=64.0,
                    help="maximum last-quarter minus first-quarter median RSS in MB "
                         "(default 64)")
    ap.add_argument("--rss-max-mb", type=float, default=512.0,
                    help="absolute peak-RSS ceiling in MB (default 512)")
    ap.add_argument("--max-processes", type=int, default=64,
                    help="maximum process-tree member count (default 64)")
    ap.add_argument("--process-growth", type=int, default=8,
                    help="maximum process-count growth over the run (default 8)")
    ap.add_argument("--max-tool-errors", type=int, default=0,
                    help="allowed tool_execution_end isError count (default 0)")
    ap.add_argument("--min-tool-calls", type=int, default=None,
                    help="required tool calls (mock default: one per tool prompt)")
    ap.add_argument("--min-compactions", type=int, default=None,
                    help="required successful compactions (default: derived "
                         "from --context-window/--reserve-tokens/"
                         "--keep-recent-tokens and per-turn growth in mock "
                         "mode, else 0; pass an explicit value to override)")
    ap.add_argument("--session-id", default=None,
                    help="session id to create/append (default: fresh uuid4)")
    ap.add_argument("--session", default=None,
                    help="existing session file to append to (mutually exclusive "
                         "with --session-id)")
    ap.add_argument("--extra-arg", action="append", default=[],
                    help="extra pir CLI argument inserted before the session args "
                         "(repeatable)")
    ap.add_argument("--mock", dest="mock", action="store_true", default=None,
                    help="force the built-in mock provider")
    ap.add_argument("--no-mock", dest="mock", action="store_false",
                    help="never start the mock; use --model against a real provider")
    ap.add_argument("--keep-batches", action="store_true",
                    help="keep one stdout file per auto batch (default false)")
    args = ap.parse_args(argv)

    if args.turns < 1:
        print("ERROR: --turns must be >= 1", file=sys.stderr)
        return 2
    if args.interval < 1:
        print("ERROR: --interval must be >= 1", file=sys.stderr)
        return 2
    if args.session and args.session_id:
        print("ERROR: --session and --session-id are mutually exclusive", file=sys.stderr)
        return 2
    if args.tool_every < 0:
        print("ERROR: --tool-every must be >= 0", file=sys.stderr)
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
        print("ERROR: mock mode needs --model provider/model (e.g. mock/soak-model)",
              file=sys.stderr)
        return 2
    if mock_mode and args.model.split("/", 1)[1] and not args.model.startswith("mock/"):
        # A non-"mock" provider id is allowed only when the user explicitly
        # asked for the built-in server; provisioning rewrites that provider's
        # entry, so call it out in the log.
        print("[soak] note: --mock rewrites provider '%s' in the agent dir"
              % args.model.split("/", 1)[0])

    log_dir = common.resolve_path(args.log)
    common.ensure_dir(log_dir)
    work_dir = common.ensure_dir(os.path.join(log_dir, "work"))
    session_dir = common.ensure_dir(os.path.join(log_dir, "sessions"))
    common.write_fixture_file(work_dir, lines=max(50, args.tool_output_lines * 8))

    session_id = None
    expected_session_id = None
    session_path = common.resolve_path(args.session) if args.session else None
    if session_path:
        report = common.inspect_jsonl(session_path)
        if not report["exists"]:
            print("ERROR: --session file not found: %s" % session_path, file=sys.stderr)
            return 2
        if not report["header_ok"]:
            print("ERROR: --session file has no valid v3 session header: %s"
                  % session_path, file=sys.stderr)
            return 2
        expected_session_id = report["header"]["id"]
    else:
        session_id = args.session_id or str(uuid.uuid4())
        expected_session_id = session_id

    prompts, expected_tools = build_prompts(args.turns, args.tool_every)
    min_tool_calls = args.min_tool_calls
    if min_tool_calls is None:
        min_tool_calls = expected_tools if mock_mode else 0
    min_compactions_estimate = args.min_compactions
    min_compactions_explicit = min_compactions_estimate is not None
    min_compactions_derived = False
    if min_compactions_estimate is None:
        if mock_mode:
            # Pre-run estimate from the configured per-turn growth. The
            # post-run verdict replaces it with the MEASURED session growth
            # (see `measured_growth_tokens` below), so a run that grows more
            # than configured is required to compact more.
            min_compactions_estimate = derive_min_compactions(
                args.turns, args.context_window, args.reserve_tokens,
                args.keep_recent_tokens, args.growth_chars, args.tool_every,
                args.tool_output_lines)
            min_compactions_derived = True
        else:
            min_compactions_estimate = 0
    min_compactions = min_compactions_estimate
    session_bytes_before = os.path.getsize(session_path) if session_path else 0

    mock = None
    agent_dir = None
    if mock_mode:
        provider, model_id = args.model.split("/", 1)
        mock = common.MockProvider(
            model_id=model_id or "soak-model",
            provider=provider,
            response_chars=args.growth_chars,
            tool_lines=max(1, args.tool_output_lines),
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
    base_argv = common.build_pir_argv(
        pie, args.model, thinking=args.thinking, session_dir=session_dir,
        prompts=[], extra_args=args.extra_arg,
    )
    # Reserve room for --session-id/--session plus quoting.
    batches = common.auto_batch_prompts(base_argv + ["x" * 240], prompts)

    state = {
        "started": time.monotonic(),
        "completed": 0,
        "agent_starts": 0,
        "agent_ends": 0,
        "tool_calls": 0,
        "tool_errors": 0,
        "compactions": 0,
        "compactions_started": 0,
        "compaction_failures": 0,
        "compaction_aborted": 0,
        "retries": 0,
        "error_events": 0,
        "assistant_messages": 0,
        "bad_stops": 0,
        "mock_texts": 0,
        "unparsed": 0,
        "events": 0,
        "post_agent_end_samples": 0,
        "post_agent_end_skipped": 0,
        "last_agent_end_at": None,
        "stream_header_errors": [],
        "leftover_trees": [],
    }
    samples = []
    samples_path = os.path.join(log_dir, "samples.jsonl")
    try:
        os.remove(samples_path)
    except OSError:
        pass

    print("[soak] pir=%s" % pie)
    print("[soak] model=%s mock=%s turns=%d interval=%d" %
          (args.model, "yes" if mock_mode else "no", args.turns, args.interval))
    print("[soak] log=%s" % log_dir)
    print("[soak] batches=%d prompts/batch=%s"
          % (len(batches), [len(batch) for batch in batches][:8]))

    failure = None
    harness_error = False
    exit_codes = []
    stdout_first_paths = []
    total_deadline = state["started"] + args.timeout
    try:
        for batch_index, batch in enumerate(batches):
            if args.session and batch_index == 0:
                argv = common.build_pir_argv(
                    pie, args.model, thinking=args.thinking,
                    session_dir=session_dir, session_path=session_path,
                    prompts=batch, extra_args=args.extra_arg,
                )
            else:
                # Batch 0 creates the session by id; later batches reopen the
                # discovered file. This also exercises the reopen path when
                # the command line forces more than one batch.
                known_path = session_path
                if known_path is None and session_id:
                    known_path = common.find_session_file(session_dir, session_id)
                argv = common.build_pir_argv(
                    pie, args.model, thinking=args.thinking,
                    session_dir=session_dir,
                    session_id=session_id if known_path is None else None,
                    session_path=known_path,
                    prompts=batch, extra_args=args.extra_arg,
                )
            stdout_path = os.path.join(
                log_dir,
                "headless-batch%02d.stdout.jsonl" % batch_index
                if len(batches) > 1 or args.keep_batches
                else "headless.stdout.jsonl",
            )
            stderr_path = os.path.join(
                log_dir,
                "headless-batch%02d.stderr.txt" % batch_index
                if len(batches) > 1 or args.keep_batches
                else "headless.stderr.txt",
            )
            runner = common.LineProcess(argv, cwd=work_dir, env=env,
                                        stdout_path=stdout_path,
                                        stderr_path=stderr_path)
            stdout_first_paths.append(stdout_path)
            state["commands"] = state.get("commands", []) + [argv]
            print("[soak] batch %d: %d prompts -> %s"
                  % (batch_index, len(batch), os.path.basename(stdout_path)))
            sampler = common.TreeSampler(
                runner.proc.pid, interval_s=args.sample_seconds,
                x_fn=lambda s=state: s["completed"],
            )
            sampler.start()
            try:
                ok, reason = run_batch(runner, len(batch), state, args, samples,
                                       samples_path, total_deadline, batch_index)
            finally:
                tree_samples = sampler.stop()
                for sample in tree_samples:
                    sample["batch"] = batch_index
                    samples.append(sample)
                    common.append_jsonl(samples_path, sample)
                # A sampler point whose query COMPLETED at/after the last
                # agent_end (and while the child was still alive) is direct
                # post-turn evidence, exactly like the reopen driver counts.
                agent_end_at = state.get("last_agent_end_at")
                sampler_started = sampler.started_monotonic
                post_counted = 0
                if agent_end_at is not None and sampler_started is not None:
                    relative = max(0.0, agent_end_at - sampler_started)
                    for sample in tree_samples:
                        sample_at = float(sample.get(
                            "sampled_at_s", sample.get("at_s", 0)))
                        if sample_at >= relative:
                            sample["post_agent_end"] = True
                            post_counted += 1
                state["post_agent_end_samples"] += post_counted
                state["batch_post_agent_end"] = post_counted
                if tree_samples:
                    print("[soak] batch %d tree samples: %d (last rss=%.1f MB, "
                          "procs=%s)" % (batch_index, len(tree_samples),
                                         tree_samples[-1]["rss_mb"],
                                         tree_samples[-1]["processes"]))
                code = runner.wait(30)
                runner.close()
                # Zombie/orphan handling: every pid the sampler ever saw in
                # the tree (root included) must be gone after the root exits.
                # Once the root dies the parent links are gone, so the
                # sampler's pid set is the only way to see a surviving tool
                # child; such a child is a real soak finding.
                survivors = common.alive_pids(sampler.seen_pids)
                if survivors:
                    state["leftover_trees"].append({
                        "batch": batch_index,
                        "survivingPids": survivors[:16],
                        "survivingCount": len(survivors),
                    })
            exit_codes.append(code)
            if not ok:
                failure = reason
                break
            if session_path is None and session_id:
                session_path = common.find_session_file(session_dir, session_id)
                if session_path is None:
                    failure = "session file not found for id %s" % session_id
                    break
    except KeyboardInterrupt:
        print("[soak] interrupted", file=sys.stderr)
        return 130
    except Exception as exc:  # noqa: BLE001 - the harness must still summarize
        traceback.print_exc()
        failure = "harness error: %s: %s" % (type(exc).__name__, exc)
        harness_error = True
    finally:
        if mock is not None:
            mock.stop()

    if session_path is None and session_id:
        session_path = common.find_session_file(session_dir, session_id)
    session_report = common.inspect_jsonl(session_path) if session_path else None

    reasons = []
    if failure:
        reasons.append(failure)
    if state["completed"] != args.turns:
        reasons.append("completed %d/%d turns" % (state["completed"], args.turns))
    for index, code in enumerate(exit_codes):
        if code not in (0, None):
            reasons.append("batch %d exit code %s" % (index, code))
    if state["tool_errors"] > args.max_tool_errors:
        reasons.append("tool errors %d > --max-tool-errors %d"
                       % (state["tool_errors"], args.max_tool_errors))
    if state["tool_calls"] < min_tool_calls:
        reasons.append("tool calls %d < required %d"
                       % (state["tool_calls"], min_tool_calls))
    if state["compaction_failures"]:
        detail = ""
        if state["compaction_aborted"]:
            detail = " (%d aborted; an aborted run is never counted as a " \
                     "successful compaction)" % state["compaction_aborted"]
        reasons.append("compaction failures: %d%s"
                       % (state["compaction_failures"], detail))
    unfinished_compactions = (state["compactions_started"]
                              - state["compactions"] - state["compaction_failures"])
    if unfinished_compactions:
        reasons.append("compactions started but never ended: %d"
                       % unfinished_compactions)
    # Stream-level error frames are reported in `counts.error_events` but do
    # not fail on their own: a provider error that the retry policy recovers
    # from is not degradation. A turn that never ends is caught by the
    # completed-turns check, and an error stop is caught by `bad_stops`.
    if state["bad_stops"]:
        reasons.append("assistant messages stopped with error/aborted: %d"
                       % state["bad_stops"])
    if mock_mode and args.growth_chars > 0 and state["mock_texts"] == 0:
        reasons.append("no mock response text observed in assistant messages")
    if state["unparsed"]:
        reasons.append("unparsed stdout lines: %d" % state["unparsed"])

    if session_report is None or not session_report["exists"]:
        reasons.append("session file missing")
    else:
        header_reason = common.header_version_reason(session_report)
        if header_reason:
            reasons.append(header_reason)
        elif not session_report["header_ok"]:
            reasons.append("session header invalid")
        if expected_session_id and \
                (session_report["header"] or {}).get("id") != expected_session_id:
            reasons.append("session id mismatch (expected %s)" % expected_session_id)
        if session_report["malformed_complete"]:
            reasons.append("session has %d malformed complete line(s)"
                           % len(session_report["malformed_complete"]))
        if session_report["torn_tail"]:
            reasons.append("normal run left a torn final session line")
        if not session_report["ends_with_newline"]:
            reasons.append("session file does not end with a newline")

    # The persisted session must also have grown: a run that reported N turns
    # but appended nothing (a fresh ephemeral session, or a silent fallback)
    # is not the long-run scenario. Measured growth is the context growth the
    # compaction floor is derived from.
    session_bytes_after = (session_report or {}).get("bytes") or 0
    measured_growth = common.measured_growth_tokens(
        session_bytes_before, session_bytes_after)
    context_growth = {
        "sessionBytesBefore": session_bytes_before,
        "sessionBytesAfter": session_bytes_after,
        "appendedBytes": session_bytes_after - session_bytes_before,
        "measuredGrowthTokens": int(measured_growth),
        "contextWindow": args.context_window,
        "reserveTokens": args.reserve_tokens,
        "triggerTokens": max(1, args.context_window - args.reserve_tokens),
        "mockRequestGrowthBytes": ((mock.state.snapshot() or {})
                                   .get("requestGrowthBytes")
                                   if mock is not None else None),
    }
    if session_path and session_bytes_after <= session_bytes_before:
        reasons.append(
            "session file did not grow (before=%d after=%d); the run did not "
            "append to the persisted session"
            % (session_bytes_before, session_bytes_after))
    if mock_mode and args.growth_chars > 0 and session_path \
            and measured_growth <= 0:
        reasons.append(
            "no measured context growth in the session despite "
            "--growth-chars %d" % args.growth_chars)

    # Default compaction floor: >=1 successful compaction per measured
    # context-window worth of growth. An explicit --min-compactions always
    # wins; otherwise the measured floor replaces the pre-run estimate (the
    # estimate is kept in the summary for comparison).
    min_compactions_measured = None
    if not min_compactions_explicit and mock_mode:
        if measured_growth > 0:
            min_compactions_measured = common.compactions_for_growth(
                measured_growth, args.context_window, args.reserve_tokens)
            min_compactions = min_compactions_measured
            min_compactions_derived = True
        else:
            # No session growth: fall back to the configured-growth estimate
            # (0 for a flat run) so the check stays meaningful.
            min_compactions = min_compactions_estimate
    if state["compactions"] < min_compactions:
        reasons.append("successful compactions %d < required %d"
                       % (state["compactions"], min_compactions))

    rss_ok, rss_reasons, rss_metrics = common.rss_verdict(
        samples, args.rss_slope_mb, args.rss_growth_mb, args.rss_max_mb,
        warmup_x=args.warmup_turns, prefer_scope="tree",
        min_tree_samples=args.min_tree_samples)
    reasons.extend(rss_reasons)
    proc_ok, proc_reasons, proc_metrics = common.process_count_verdict(
        samples, args.max_processes, args.process_growth,
        min_samples=args.min_process_samples, warmup_x=args.warmup_turns)
    reasons.extend(proc_reasons)
    # A run that ends exactly at the last turn must still have proven the
    # post-agent_end sampler observed the child: without that, a leak that
    # only appears after the final agent_end would be invisible. The tree
    # sampler runs for the whole batch, so this is a diagnostic guard on the
    # tree-sample requirement (the >=4 tree samples above are the hard gate).
    tree_samples = [s for s in samples if s.get("scope") == "tree"]
    if not tree_samples:
        reasons.append("no process-tree samples were collected at all")
    if state["leftover_trees"]:
        reasons.append(
            "%d batch(es) left surviving processes after the root exited "
            "(orphaned tool children): %s"
            % (len(state["leftover_trees"]), state["leftover_trees"][:4]))
    if state["post_agent_end_samples"] < 1 and state["post_agent_end_skipped"] < 1:
        reasons.append("no post-agent_end tree sample and no recorded skip")
    # The JSON stdout stream must start with the persisted session header, and
    # that header must be the strict v3 header with the expected id. A run
    # whose first line is a different event shape (or a legacy header) is not
    # proven to have opened the session it claims.
    stream_header = None
    for stream_index, stream_path in enumerate(stdout_first_paths):
        header = common.stream_session_header(stream_path)
        if stream_index == 0:
            stream_header = header
        reason = None
        if header is None:
            reason = ("batch %d: first stdout line of the JSON stream is not "
                      "a session header" % stream_index)
        else:
            reason = common.header_version_reason(header)
            if reason is None and expected_session_id \
                    and header.get("id") != expected_session_id:
                reason = ("batch %d: JSON stream session id %r != expected %r"
                          % (stream_index, header.get("id"),
                             expected_session_id))
        if reason:
            reasons.append(reason)
            state["stream_header_errors"].append(reason)

    elapsed = time.monotonic() - state["started"]
    mock_stats = mock.state.snapshot() if mock is not None else None
    summary = {
        "script": "soak_headless",
        "verdict": "PASS" if not reasons else "FAIL",
        "reasons": reasons,
        "pir": pie,
        "model": args.model,
        "agentDir": agent_dir,
        "mock": mock_mode,
        "mockStats": mock_stats,
        "turns": args.turns,
        "completedTurns": state["completed"],
        "interval": args.interval,
        "batchCount": len(batches),
        "exitCodes": exit_codes,
        "elapsedS": round(elapsed, 2),
        "session": {
            "id": session_id or ((session_report or {}).get("header") or {}).get("id"),
            "path": session_path,
            "bytes": (session_report or {}).get("bytes"),
            "lines": (session_report or {}).get("lines"),
            "parsed": (session_report or {}).get("parsed"),
            "tornTail": (session_report or {}).get("torn_tail"),
            "headerVersion": (session_report or {}).get("header_version"),
            "headerVersionOk": (session_report or {}).get("header_version_ok"),
            "streamHeader": stream_header,
        },
        "counts": {key: state[key] for key in (
            "agent_starts", "agent_ends", "tool_calls", "tool_errors",
            "compactions", "compactions_started", "compaction_failures",
            "compaction_aborted", "retries", "error_events",
            "assistant_messages", "bad_stops", "mock_texts", "unparsed",
            "events", "post_agent_end_samples", "post_agent_end_skipped",
        )},
        "streamHeaderErrors": state["stream_header_errors"],
        "leftoverProcesses": state["leftover_trees"],
        "minCompactions": min_compactions,
        "minCompactionsEstimate": min_compactions_estimate,
        "minCompactionsMeasured": min_compactions_measured,
        "minCompactionsExplicit": min_compactions_explicit,
        "minCompactionsDerived": min_compactions_derived,
        "contextGrowth": context_growth,
        "rss": rss_metrics,
        "processes": proc_metrics,
        "thresholds": {
            "warmupTurns": args.warmup_turns,
            "sampleSeconds": args.sample_seconds,
            "rssSlopeMbPerTurn": args.rss_slope_mb,
            "rssGrowthMb": args.rss_growth_mb,
            "rssMaxMb": args.rss_max_mb,
            "maxProcesses": args.max_processes,
            "processGrowth": args.process_growth,
            "minTreeSamples": args.min_tree_samples,
            "minProcessSamples": args.min_process_samples,
            "requestTimeoutS": args.request_timeout,
            "minCompactions": min_compactions,
            "minCompactionsExplicit": min_compactions_explicit,
            "minCompactionsDerived": min_compactions_derived,
        },
        "samples": sorted(samples, key=lambda s: float(s.get("x", 0))),
        "commands": state.get("commands", []),
    }

    print("")
    print("=== soak_headless ===")
    print("  turns:        %d/%d in %.1fs" % (state["completed"], args.turns, elapsed))
    print("  events:       %d (agent_end=%d tool_calls=%d tool_errors=%d)"
          % (state["events"], state["agent_ends"], state["tool_calls"],
             state["tool_errors"]))
    print("  compaction:   %d ok / %d started / %d failed (required %d)"
          % (state["compactions"], state["compactions_started"],
             state["compaction_failures"], min_compactions))
    if mock_stats:
        print("  mock:         requests=%d by_kind=%s max_body=%.1fMB"
              % (mock_stats["requests"], mock_stats["byKind"],
                 mock_stats["maxRequestBytes"] / (1024.0 * 1024.0)))
    print("  rss:          peak=%s MB (tree) root=%s MB slope=%s MB/turn "
          "growth=%s MB samples=%s"
          % (rss_metrics.get("peakRssMb"), rss_metrics.get("rootPeakRssMb"),
             rss_metrics.get("slopeMbPerTurn"), rss_metrics.get("growthMb"),
             rss_metrics.get("verdictSamples")))
    print("  processes:    max=%s slope=%s/unit rise=%s growth=%s"
          % (proc_metrics.get("maxProcesses"),
             proc_metrics.get("slopeProcessesPerUnit"),
             proc_metrics.get("slopeRiseOverWindow"),
             proc_metrics.get("processGrowth")))
    print("  criteria:     rss=%s" % rss_metrics.get("criterion"))
    print("                procs=%s" % proc_metrics.get("criterion"))
    print("  growth:       tokenGrowth=%s appended=%s bytes requiredCompactions=%s"
          % (context_growth.get("measuredGrowthTokens"),
             context_growth.get("appendedBytes"), min_compactions))
    print("  post-end:     treeSamples=%s skipped=%s"
          % (state["post_agent_end_samples"], state["post_agent_end_skipped"]))
    if session_report:
        print("  session:      %s (%s bytes, %s lines, torn_tail=%s)"
              % (session_path, session_report.get("bytes"),
                 session_report.get("lines"), session_report.get("torn_tail")))
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
