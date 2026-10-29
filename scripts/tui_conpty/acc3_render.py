# -*- coding: utf-8 -*-
# Adapted from target/tui_shots/acc3_render.py (copied 2026-10-15) into
# scripts/tui_conpty/. Battery change (run_battery.py): the hard-coded 7-name
# list is gone — the module now exposes `render` / `render_to_file` and
# `render_dir`, which renders EVERY `*.log` in a directory (driver logs
# `<name>.driver.log` are excluded: they are harness traces, not app output).
# Importable without side effects; `python acc3_render.py [dir ...]` renders
# the given dirs (default: out/) and prints the non-empty lines for eyeballing.
import glob
import os
import sys

sys_dir = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, sys_dir)
import pyte  # noqa: E402

COLS, ROWS = 120, 40
SHOTS = os.path.join(sys_dir, "out")

DRIVER_LOG_SUFFIX = ".driver.log"


def is_app_log(path):
    """True for captured app-output logs (snap + final), False for driver traces."""
    return path.lower().endswith(".log") and not path.lower().endswith(DRIVER_LOG_SUFFIX)


def render(data):
    """Replay raw ConPTY bytes through pyte; returns the 40x120 screen rows."""
    screen = pyte.Screen(COLS, ROWS)
    pyte.ByteStream(screen).feed(data)
    return [l.rstrip() for l in screen.display]


def render_to_file(data, path):
    lines = render(data)
    with open(path, "w", encoding="utf-8") as f:
        for i, l in enumerate(lines):
            f.write(f"{i:2d}|{l}\n")
    return lines


def screen_path(log_path):
    return log_path[: -len(".log")] + ".screen.txt"


def render_log(log_path):
    """Render `<log>` -> `<log>.screen.txt`; returns the rendered lines."""
    data = open(log_path, "rb").read()
    return render_to_file(data, screen_path(log_path))


def render_dir(directory, print_screens=False):
    """Render every app `*.log` in `directory` (non-recursive) to a sibling
    `.screen.txt`. Returns a list of (log_path, screen_path, lines)."""
    rendered = []
    for log_path in sorted(glob.glob(os.path.join(directory, "*.log"))):
        if not is_app_log(log_path):
            continue
        lines = render_log(log_path)
        rendered.append((log_path, screen_path(log_path), lines))
        if print_screens:
            print(f"=== {os.path.basename(log_path)} ===")
            for i, l in enumerate(lines):
                if l.strip():
                    print(f"{i:2d}|{l}")
            print()
    return rendered


def main(argv):
    dirs = argv[1:] or [SHOTS]
    total = 0
    for d in dirs:
        if not os.path.isdir(d):
            print(f"skip (not a dir): {d}", file=sys.stderr)
            continue
        total += len(render_dir(d, print_screens=True))
    print(f"rendered {total} log(s)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
