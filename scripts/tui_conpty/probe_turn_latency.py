# -*- coding: utf-8 -*-
"""ConPTY turn-latency probe (2026-10-15, user report: "模型回答调用工具超级慢").

Measures, inside a real ConPTY at fixed 120x40:
  * process start -> first output byte, and start -> boot-quiet (the "ready" point),
  * prompt -> tool-call line -> tool result -> final text, each with wall-clock,
  * the OUTPUT GROWTH timeline (chunk arrivals every 25 ms) so "streaming" vs
    "one burst at the end" is a measured fact, not a guess,
  * child CPU time + working set sampled every 250 ms.

It measures the TUI turn pipeline with the battery's SANITIZED agent config
(scripts/tui_conpty/run_battery.py synthesizes it: dummy key, dead loopback
baseUrl) and, when --mode mock is used, the same in-process mock LLM. It never
reads `~/.pi/agent` or `~/.claude`: the old default (`--config real`) copied
`models.json` + `auth.json` (live credentials) into the probe's out dir, which
is how Q27 found credential copies under `scripts/tui_conpty/out/`.

A real provider is opt-in only: `--config template --models-template <path>`
where every credential-bearing value (apiKey, headers.*, customHeaders.*) is a
WHOLE `$ENV`/`${ENV}`/`!command` template (validated by
run_battery.load_models_template; literal, mixed literal+template and
URL-userinfo credentials are refused BEFORE out/ or the agent dir is created).
Extensions, if wanted, come from an explicit `--extensions-dir <path>`.

Usage:
  python probe_turn_latency.py --pir <pir.exe> [--mode mock|real] [--extensions no|yes]
      [--extensions-dir DIR] [--config sanitized|template --models-template FILE]
      [--prompt TEXT] [--out DIR] [--boot-seconds N] [--turn-seconds N]
"""
import argparse, ctypes, json, os, shutil, subprocess, sys, time
from ctypes import wintypes as wt

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

from tui_drive import Pty, BASE_ENV  # noqa: E402
import run_battery as rb  # noqa: E402
from mock_llm import MockServer  # noqa: E402

k32 = ctypes.WinDLL("kernel32", use_last_error=True)
k32.GetProcessId.argtypes = [wt.HANDLE]
k32.GetProcessId.restype = wt.DWORD


def decode(buf):
    return bytes(buf).decode("utf-8", errors="replace")


def build_agent_dir(root, use_extensions, config_source="sanitized",
                    models_template=None, extensions_dir=None):
    """Write a per-probe agent dir from the sanitized battery template.

    Credentials are never copied from the user's real agent dir; a real
    provider config is only used when explicitly passed (already validated).
    """
    agent = os.path.join(root, "agent")
    # seed_agent_dir validates models_template BEFORE creating `agent`, so a
    # refused template leaves no directory, let alone a models.json.
    if config_source == "template":
        if models_template is None:
            raise SystemExit("--config template requires --models-template <path>")
        rb.seed_agent_dir(agent, True, models_template)
    else:
        rb.seed_agent_dir(agent, True, None)
    if use_extensions:
        if not extensions_dir or not os.path.isdir(extensions_dir):
            raise SystemExit("--extensions yes requires --extensions-dir <dir> "
                            "(never defaulted to ~/.pi/agent)")
        shutil.copytree(extensions_dir, os.path.join(agent, "extensions"),
                        dirs_exist_ok=True)
    return agent


def start_sampler(pid, out_path):
    """One long-lived PowerShell sampling CPU (s) and working set (MB)."""
    script = (
        f"$p=Get-Process -Id {pid} -ErrorAction SilentlyContinue;"
        "while($p){"
        "$p.Refresh();"
        f"'{{0}} {{1}} {{2}}' -f (Get-Date -Format o), $p.CPU, $p.WorkingSet64;"
        "Start-Sleep -Milliseconds 250;"
        f"$p=Get-Process -Id {pid} -ErrorAction SilentlyContinue }}"
    )
    fh = open(out_path, "w", encoding="utf-8")
    proc = subprocess.Popen(["powershell", "-NoProfile", "-Command", script],
                            stdout=fh, stderr=subprocess.DEVNULL, text=True)
    return proc, fh


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--pir", required=True)
    ap.add_argument("--mode", choices=["mock", "real"], default="mock")
    ap.add_argument("--extensions", choices=["yes", "no"], default="no")
    ap.add_argument("--extensions-dir", default=None,
                    help="extension dir to copy when --extensions yes (required; "
                         "never defaults to ~/.pi/agent)")
    ap.add_argument("--config", choices=["sanitized", "template"],
                    default="sanitized",
                    help="sanitized (default, built-in dummy template) or template "
                         "(--models-template, env-templated secrets)")
    ap.add_argument("--models-template", default=None,
                    help="models.json for --config template; every credential-"
                         "bearing value must be a WHOLE $ENV/${ENV}/!command "
                         "template (literal header/URL-userinfo and mixed "
                         "literal+template values are refused)")
    ap.add_argument("--prompt", default=None)
    ap.add_argument("--model", default=None)
    ap.add_argument("--pir-args", default=None, help="extra CLI args, e.g. --session <path>")
    ap.add_argument("--out", default=None)
    ap.add_argument("--boot-seconds", type=float, default=20.0)
    ap.add_argument("--turn-seconds", type=float, default=90.0)
    args = ap.parse_args()

    stamp = time.strftime("%Y%m%d-%H%M%S")
    out = args.out or os.path.join(HERE, "out", f"probe_latency_{stamp}_{args.mode}_ext{args.extensions}")

    # Validate EVERYTHING that can refuse the invocation BEFORE creating out/,
    # work/ or the seeded agent config. A rejected --models-template must not
    # leave a models.json (or any artifact) behind.
    models_template = None
    if args.config == "template":
        if not args.models_template:
            raise SystemExit("--config template requires --models-template <path>")
        try:
            models_template = rb.load_models_template(args.models_template)
        except rb.ScenarioError as exc:
            raise SystemExit(str(exc))
    if args.extensions == "yes" and (
            not args.extensions_dir or not os.path.isdir(args.extensions_dir)):
        raise SystemExit("--extensions yes requires --extensions-dir <dir> "
                         "(never defaulted to ~/.pi/agent)")

    os.makedirs(out, exist_ok=True)
    work = os.path.join(out, "work")
    os.makedirs(work, exist_ok=True)

    agent = build_agent_dir(out, args.extensions == "yes", args.config,
                            models_template, args.extensions_dir)

    mock = None
    if args.mode == "mock":
        mock_log = os.path.join(out, "mock_llm.log")
        mock = MockServer(log_path=mock_log, script=[
            {"tool_call": {"name": "bash", "arguments": {"command": "for i in 7; do echo \"OUT-$((i*111))\"; done"}}},
            {"text": "PROBE-FINAL-OK"},
        ])
        rb.inject_mock_models(agent, mock.base_url)

    prompt = args.prompt
    if prompt is None:
        prompt = ("Run this bash command exactly as given, then answer with the probe's "
                  "final sentinel token and nothing else: for i in 7; do echo \"OUT-$((i*111))\"; done")
    markers = ["$ for i in 7", "OUT-777", "PROBE-FINAL-OK"]

    # The driver's exact env (BASE_ENV whitelist + the agent dir override):
    # mirroring it is what makes the battery's model resolution work here too.
    env = dict(BASE_ENV)
    env.update({
        "PIR_AGENT_DIR": agent,
        "PIR_NO_UPDATE_CHECK": "1",
        "RUST_LOG": "error",
        "PYTHONIOENCODING": "utf-8",
    })
    t0 = time.time()
    # The driver's exact launch shape: the `interactive` subcommand, full
    # console env, cwd = the working dir the user would run in.
    model_args = f' --model "{args.model}"' if args.model else ""
    extra = f" {args.pir_args}" if args.pir_args else ""
    pty = Pty(f'"{args.pir}" interactive{model_args}{extra}', work, env, cols=120, rows=40)
    pid = k32.GetProcessId(pty.hproc)
    sampler, sampler_fh = start_sampler(pid, os.path.join(out, "process_samples.txt"))

    samples = []            # (t, total_bytes)
    marker_times = {}
    prompt_sent_at = None
    boot_first_byte = None
    try:
        # Boot: wait for output to go quiet (the app is idle on the input box).
        # The quiet threshold mirrors the battery driver's `wait_quiet 4`: raw
        # mode must already be on when the prompt arrives, otherwise a cooked
        # line discipline holds the CR and the app never sees the turn.
        last_len, quiet_since = 0, time.time()
        while time.time() - t0 < args.boot_seconds:
            with pty.log_lock:
                n = len(pty.log)
            if boot_first_byte is None and n > 0:
                boot_first_byte = time.time() - t0
            if n != last_len:
                last_len, quiet_since = n, time.time()
            if n > 0 and time.time() - quiet_since >= 3.5:
                break
            time.sleep(0.025)
        boot_quiet = time.time() - t0
        time.sleep(0.7)

        # Turn.
        prompt_sent_at = time.time()
        prompt_sent_at_rel = prompt_sent_at - t0
        pty.write(prompt.encode("utf-8") + b"\r")
        deadline = time.time() + args.turn_seconds
        while time.time() < deadline:
            with pty.log_lock:
                buf = bytes(pty.log)
            got = len(buf)
            if not samples or samples[-1][1] != got:
                samples.append((round(time.time() - t0, 3), got))
            text = decode(buf)
            for m in markers:
                if m not in marker_times and m in text:
                    marker_times[m] = round(time.time() - t0, 3)
            if "OUT-777" in marker_times and "PROBE-FINAL-OK" in marker_times:
                # Let the final frame settle on screen.
                time.sleep(1.5)
                break
            time.sleep(0.025)

        with pty.log_lock:
            final = bytes(pty.log)
        text = decode(final)

        # Clean exit: esc, then /quit with one retry.
        for payload in (b"\x1b", b"/quit\r", b"/quit\r", b"\x03"):
            pty.write(payload)
            time.sleep(0.6)
            if not pty.alive:
                break
    finally:
        try:
            pty.terminate()
        except Exception:
            pass
        sampler.terminate()
        try:
            sampler.wait(timeout=10)
        except Exception:
            sampler.kill()
        sampler_fh.close()
        if mock:
            mock.stop()

    screens = {}
    try:
        import pyte
        screen = pyte.Screen(120, 40)
        stream = pyte.Stream(screen)
        stream.feed(decode(final))
        screens["final"] = "\n".join(screen.display)
    except Exception as exc:  # pyte missing -> raw tail only
        screens["final_error"] = str(exc)
    open(os.path.join(out, "final.screen.txt"), "w", encoding="utf-8").write(screens.get("final", ""))
    open(os.path.join(out, "final.raw.txt"), "wb").write(final)

    def rel(t):
        return None if t is None else round(t - prompt_sent_at_rel, 3)

    report = {
        "pir": args.pir,
        "mode": args.mode,
        "extensions": args.extensions,
        "agent_dir": agent,
        "prompt": prompt,
        "boot": {"first_byte_s": None if boot_first_byte is None else round(boot_first_byte, 3),
                 "quiet_s": round(boot_quiet, 3)},
        "turn": {
            "prompt_sent_at_s": round(prompt_sent_at - t0, 3),
            "tool_call_line_s": rel(marker_times.get("$ for i in 7")),
            "tool_result_s": rel(marker_times.get("OUT-777")),
            "final_text_s": rel(marker_times.get("PROBE-FINAL-OK")),
            "markers": marker_times,
        },
        "output_growth": samples[:400],
        "process_samples_file": os.path.join(out, "process_samples.txt"),
        "mock_log": os.path.join(out, "mock_llm.log") if mock else None,
        "screen_tail": screens.get("final", "")[-2000:],
    }
    with open(os.path.join(out, "report.json"), "w", encoding="utf-8") as fh:
        json.dump(report, fh, indent=2)

    ps = []
    with open(os.path.join(out, "process_samples.txt"), encoding="utf-8", errors="replace") as fh:
        for line in fh:
            parts = line.split()
            if len(parts) == 3:
                try:
                    ps.append((parts[0][11:23], float(parts[1]), float(parts[2]) / (1024 * 1024)))
                except ValueError:
                    pass
    print(f"[probe] mode={args.mode} extensions={args.extensions} out={out}")
    print(f"[probe] boot: first byte {report['boot']['first_byte_s']}s, ready {report['boot']['quiet_s']}s")
    print(f"[probe] turn: tool line {report['turn']['tool_call_line_s']}s, "
          f"tool result {report['turn']['tool_result_s']}s, final {report['turn']['final_text_s']}s")
    if ps:
        mid = ps[len(ps) // 2]
        print(f"[probe] proc: cpu {ps[-1][1]:.2f}s, ws {mid[2]:.0f}->{ps[-1][2]:.0f} MB ({len(ps)} samples)")
    chunks = [s for i, s in enumerate(samples) if i == 0 or s[1] != samples[i - 1][1]]
    print(f"[probe] output growth: {len(chunks)} size-changes; first 12: {chunks[:12]}")


if __name__ == "__main__":
    main()
