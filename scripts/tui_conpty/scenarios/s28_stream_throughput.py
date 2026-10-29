# -*- coding: utf-8 -*-
# s28 — streaming throughput (2026-10-15, promoted from the foreman's latency probe):
# the hermetic mock paces 200 chunks at 5 ms (≈200 tokens/s) then answers a bash
# tool call, so the scenario pins that the TUI consumes a REAL-shaped token
# stream promptly (it passed in ≈1.8-2.1 s of app time) and that the tool round
# trip still completes. Timings are read from the driver log's op stamps.
NAME = "s28_stream_throughput"

_LINES = " ".join(f"stream-word-{i:04d}" for i in range(1, 121))

MOCK_SCRIPT = [
    {"text": _LINES, "chunks": 200, "chunk_delay_ms": 5},
    {"tool_call": {"name": "bash",
                   "arguments": {"command": 'for i in 7; do echo "OUT-$((i*111))"; done'}}},
    {"text": "TOOL-DONE"},
]

OPS = [
    ("wait_quiet", 5.0),
    ("send", b"stream please\r"),
    ("wait_quiet", 1.2),
    ("snap", "after_stream"),
    ("send", b"tool please\r"),
    ("wait_quiet", 1.2),
    ("snap", "after_tool"),
]

CHECKS = [
    ("screen_contains", "after_stream", "stream-word-0120"),
    ("screen_contains", "after_tool", "OUT-777"),
    ("screen_contains", "after_tool", "TOOL-DONE"),
]
TIMEOUT = 150
