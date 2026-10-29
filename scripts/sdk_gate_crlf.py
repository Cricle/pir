"""CRLF-file gates (lib.rs, plugins/mod.rs, coding_agent/mod.rs).

Split out of scripts/sdk_gate_rest.py because those three files are stored
with CRLF: the anchors here are written with explicit \\r\\n.
"""

import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def patch(rel: str, pairs) -> None:
    path = os.path.join(ROOT, rel)
    with open(path, encoding="utf-8", newline="") as fh:
        text = fh.read()
    for old, new in pairs:
        if old in text:
            text = text.replace(old, new, 1)
            continue
        if new in text:
            continue  # already applied
        print(f"MISSING anchor in {rel}:\n{old[:200]}", file=sys.stderr)
        raise SystemExit(1)
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(text)


def main() -> int:
    patch(
        "src/lib.rs",
        [
            (
                '#[cfg(feature = "env")]\r\npub mod env_client;\r\n',
                "// The remote-env *client* is dependency-free and is called from the\r\n"
                "// core tool implementations; only the daemon (`env_daemon`, whose\r\n"
                "// watch operation uses `notify`) needs the `env` feature.\r\n"
                "pub mod env_client;\r\n",
            ),
            (
                "pub mod telemetry;\r\n",
                "pub mod telemetry;\r\n\r\n// Terminal-text helpers shared by the core and the TUI.\r\npub mod text;\r\n",
            ),
        ],
    )
    patch(
        "src/plugins/mod.rs",
        [
            (
                "pub mod codemode;\r\n",
                '#[cfg(feature = "extensions")]\r\npub mod codemode;\r\n',
            ),
            (
                "pub use codemode::{CodemodeConfig, CodemodePlugin, CodemodeTool, DEFAULT_INLINE_BUDGET};\r\n",
                '#[cfg(feature = "extensions")]\r\npub use codemode::{CodemodeConfig, CodemodePlugin, CodemodeTool, DEFAULT_INLINE_BUDGET};\r\n',
            ),
        ],
    )
    patch(
        "src/coding_agent/mod.rs",
        [
            (
                "pub mod session_pick;\r\n",
                '#[cfg(feature = "tui")]\r\npub mod session_pick;\r\n',
            )
        ],
    )
    print("CRLF gates applied: lib.rs, plugins/mod.rs, coding_agent/mod.rs")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
