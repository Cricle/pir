"""Docs: add the delta-throughput harness + its baseline to the wave report."""

import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def patch(rel: str, pairs) -> None:
    path = os.path.join(ROOT, rel)
    with open(path, encoding="utf-8", newline="") as fh:
        text = fh.read()
    nl = "\r\n" if "\r\n" in text else "\n"
    for old, new in pairs:
        old_n = old if nl == "\n" else old.replace("\n", "\r\n")
        new_n = new if nl == "\n" else new.replace("\n", "\r\n")
        if new_n in text:
            continue
        if old_n not in text:
            print(f"MISSING in {rel}:\n{old[:180]}", file=sys.stderr)
            sys.exit(1)
        text = text.replace(old_n, new_n, 1)
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(text)


BASELINE = """**可复跑回归工装（新增）**：`scripts/perf/ext_delta_throughput.py` —— 本地 mock SSE 流
（默认 8 000 reasoning + 2 000 content delta，无 pacing）+ 隔离 agent dir + 三种 leg
（`none` 无扩展 / `noop` 一个空 `message_update` handler / `stringify` 再加 JSON.stringify），
量 wall clock 与峰值工作集（`tasklist` 每 250 ms 采样；`EXT_DELTA_NO_RSS=1` 可关）。
修复前（release 08:13 二进制）实测：**none 2.00 s / 31.6 MB；noop 33.54 s / 804.6 MB；stringify 43.05 s / 808.2 MB**
—— 与 TUI 里量到的 1.1 s/40 MB → 23.6 s/442 MB 同形，且无头 `print` 更糟。
（工装自身的坑：`pir print` 会把回复流式写 stdout，父进程必须重定向到文件而不是留着不读的管道，否则 child 卡死——
第一版就踩了这个，误判成 pir 的 cliff。）

"""


def main() -> int:
    patch(
        "WAVE-REPORT.md",
        [
            (
                "**未修（在别的车道/后续）**：per-delta `message_update` 载荷（上表第 2/3 行，已派专门车道）；",
                BASELINE
                + "**未修（在别的车道/后续）**：per-delta `message_update` 载荷（上表第 2/3 行，已派专门车道）；",
            )
        ],
    )
    patch(
        "HANDOFF.md",
        [
            (
                "- Perf harnesses: `scripts/perf/{cli_cold_start,p3_task_e2e,ttft_curve}.mjs`; criterion",
                "- Per-delta extension cost: `python scripts/perf/ext_delta_throughput.py --pir <exe>`\n"
                "  (no-extension vs one `message_update` handler; wall clock + peak RSS).\n"
                "- Perf harnesses: `scripts/perf/{cli_cold_start,p3_task_e2e,ttft_curve}.mjs`; criterion",
            )
        ],
    )
    print("docs updated: harness + baseline recorded")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
