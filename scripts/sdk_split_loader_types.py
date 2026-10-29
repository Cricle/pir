"""Move the two engine-free loader data types into `extensions::types`.

`LoadResult` (in `types.rs`) names `LoadedExtension`/`ExtensionLoadError`, so
those two plain-data structs must live in the always-compiled module too.
`loader.rs` re-exports them, keeping every existing path working.

Run from the repo root. Idempotent: refuses to run twice.
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TYPES = os.path.join(ROOT, "src", "extensions", "types.rs")
LOADER = os.path.join(ROOT, "src", "extensions", "loader.rs")

BLOCK = '''/// One discovered extension entry point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedExtension {
    /// Extension display path (the file that was loaded).
    pub path: PathBuf,
    /// Synthetic QuickJS module name for this entry point.
    pub module_name: String,
}

/// An error entry for a path that failed to load.
#[derive(Debug, Clone)]
pub struct ExtensionLoadError {
    pub path: PathBuf,
    pub error: String,
}

'''


def read(path: str) -> str:
    with open(path, encoding="utf-8", newline="") as fh:
        return fh.read()


def write(path: str, text: str) -> None:
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(text)


def main() -> int:
    types = read(TYPES)
    if "pub struct LoadedExtension" in types:
        print("refusing to run: types.rs already carries the loader data types", file=sys.stderr)
        return 1
    anchor = "/// Result of loading a set of extensions."
    if anchor not in types:
        print("anchor not found in types.rs", file=sys.stderr)
        return 1
    types = types.replace(anchor, BLOCK + anchor, 1)
    write(TYPES, types)

    loader = read(LOADER)
    pattern = re.compile(
        r"/// One discovered extension entry point\.\n"
        r"#\[derive\(Debug, Clone, PartialEq, Eq\)\]\n"
        r"pub struct LoadedExtension \{.*?\n\}\n\n"
        r"/// An error entry for a path that failed to load\.\n"
        r"#\[derive\(Debug, Clone\)\]\n"
        r"pub struct ExtensionLoadError \{.*?\n\}\n\n",
        re.S,
    )
    loader2, n = pattern.subn(
        "// Engine-free data types shared with the host: defined in\n"
        "// `crate::extensions::types` (always compiled) and re-exported here.\n"
        "pub use crate::extensions::types::{ExtensionLoadError, LoadedExtension};\n\n",
        loader,
        count=1,
    )
    if n != 1:
        print("failed to cut the struct definitions out of loader.rs", file=sys.stderr)
        return 1
    write(LOADER, loader2)
    print("moved LoadedExtension/ExtensionLoadError into types.rs")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
