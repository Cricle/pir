"""Split the engine-free extension API types out of `runtime/` (SDK trimming).

Moves `src/extensions/runtime/types.rs` to `src/extensions/types.rs` (always
compiled) and moves `impl ToolDef` (the `core()` constructor, host-side and
engine-free) along with it, leaving `runtime/mod.rs` re-exporting the same names
so every existing `crate::extensions::X` / `runtime::X` path keeps working.

Run from the repo root. Idempotent: refuses to run twice.
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OLD = os.path.join(ROOT, "src", "extensions", "runtime", "types.rs")
NEW = os.path.join(ROOT, "src", "extensions", "types.rs")
RUNTIME_MOD = os.path.join(ROOT, "src", "extensions", "runtime", "mod.rs")

HEADER = """\
//! Engine-free definition/data types of the extension system.
//!
//! These are the shapes the host hands across the Rust/JS boundary and that the
//! plugin framework, the session loop and the TUI read directly (`ToolDef`,
//! `CommandDef`, `Notification`, `ToolResult`, ...). They carry no
//! QuickJS/swc/tokio dependency, so they compile in every feature
//! configuration: a minimal SDK build (no `extensions` feature) keeps the whole
//! plugin/hook data surface, only the JS engine is trimmed.
//!
//! The engine side (`runtime`, `handle`, `loader`, `transpile`, `api`) lives in
//! the sibling modules behind `cfg(feature = "extensions")`.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::path::PathBuf;

"""


def read(path: str) -> str:
    with open(path, encoding="utf-8", newline="") as fh:
        return fh.read()


def write(path: str, text: str) -> None:
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(text)


def main() -> int:
    if os.path.exists(NEW):
        print("refusing to run: src/extensions/types.rs already exists", file=sys.stderr)
        return 1
    types = read(OLD)
    lines = types.splitlines(keepends=True)
    if lines[0].strip() != "use super::*;":
        print(f"unexpected first line: {lines[0]!r}", file=sys.stderr)
        return 1
    body = "".join(lines[1:]).lstrip("\n")
    write(NEW, HEADER + body)

    mod = read(RUNTIME_MOD)
    # 1. Pull `impl ToolDef { ... }` (the engine-free `core()` constructor) out
    #    of runtime/mod.rs and append it to the new types file.
    start = mod.index("impl ToolDef {")
    end = mod.index("\n}\n", start) + len("\n}\n")
    impl_block = mod[start:end]
    write(NEW, read(NEW) + "\n" + impl_block)
    mod = mod[:start] + mod[end:]
    # 2. `mod types;` -> the types module now lives one level up.
    mod = mod.replace("mod types;\n", "")
    # 3. Re-export the (now parent-level) types so `runtime::X` paths and the
    #    `use super::*` globs in this module's submodules keep resolving.
    mod = re.sub(
        r"pub use types::\{[^}]*\};",
        "// Definition/data types are engine-free and live in the always-compiled\n"
        "// `crate::extensions::types` (SDK-SCOPE.md): re-exported here so the\n"
        "// existing `runtime::<Type>` paths and the `use super::*` globs in this\n"
        "// module's submodules keep resolving.\n"
        "pub use crate::extensions::types::*;",
        mod,
        count=1,
    )
    write(RUNTIME_MOD, mod)

    os.remove(OLD)
    print("moved:", OLD, "->", NEW)
    print("patched:", RUNTIME_MOD)
    print("ToolDef impl bytes moved:", len(impl_block))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
