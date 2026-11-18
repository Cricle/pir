# -*- coding: utf-8 -*-
# Real-provider A/B probe: long-running task (build/test/CI style).
# The user asks the model to run a ~20s command and then report the result.
# We count how the model waits:
#   - efficient: it calls bash with "background": true and ends its turn,
#     then continues from the automatic <background-tool-result> message.
#   - wasteful: it runs the command foreground and/or burns wall-clock with
#     `sleep N` / repeated polling.
# The scenario itself does not teach either approach; it only observes.
NAME = "r08_long_task_wait"
NEEDS_REAL_PROVIDER = True
OFFLINE = False
TIMEOUT = 200
OPS = [
    ("wait_quiet", 4),
    ("log", "ask the real model to run a long build-style command"),
    ("send", b"Run a build-style task that takes about 20 seconds: use bash to print START, sleep 20, then print BUILD_DONE. Report to me what the command printed. Do not skip the wait.\r"),
    # Let the model start the task and (if it chooses) wait for the result.
    ("wait", 75.0),
    ("wait_quiet", 10),
    ("snap", "r08_result"),
    ("send", b"/quit\r"),
]
CHECKS = [
    ("restore_markers",),
]
