"""Gate the QuickJS engine inside `extensions::runtime` + `extensions::handle`.

After this step `crate::extensions` compiles without the `extensions` cargo
feature: the engine-free surface (data types, `RuntimeState`, `ExtensionHandle`
and its mailbox/accessor methods, `EventBacklog`, `ext_stdio_route`,
`lock_state`) stays, and only the QuickJS-carrying items are gated
(`ExtensionRuntime` + its impl modules, host functions, the actor, the swc
transpile/loader chain).

Run from the repo root. Idempotent: refuses to run twice.
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
RUNTIME_MOD = os.path.join(ROOT, "src", "extensions", "runtime", "mod.rs")
RUNTIME_STATE = os.path.join(ROOT, "src", "extensions", "runtime", "state.rs")
HANDLE = os.path.join(ROOT, "src", "extensions", "handle.rs")
MAILBOX = os.path.join(ROOT, "src", "extensions", "handle", "mailbox.rs")

GATE = '#[cfg(feature = "extensions")]\n'


def read(path: str) -> str:
    with open(path, encoding="utf-8", newline="") as fh:
        return fh.read()


def write(path: str, text: str) -> None:
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(text)


def sub_once(text: str, old: str, new: str, what: str) -> str:
    if old not in text:
        print(f"MISSING anchor ({what}):\n{old[:200]}", file=sys.stderr)
        raise SystemExit(1)
    return text.replace(old, new, 1)


def gate_line(text: str, line: str, what: str) -> str:
    """Prefix one exact line with the cfg attribute."""
    return sub_once(text, line, GATE + line, what)


def main() -> int:
    mod = read(RUNTIME_MOD)
    if '#[cfg(feature = "extensions")]\nuse rquickjs' in mod:
        print("refusing to run: runtime/mod.rs already gated", file=sys.stderr)
        return 1

    # -- imports ------------------------------------------------------------
    mod = sub_once(
        mod,
        "use rquickjs::promise::MaybePromise;\nuse rquickjs::{AsyncContext, AsyncRuntime, Error as JsError};\nuse serde::{Deserialize, Serialize};\n",
        GATE
        + "use rquickjs::promise::MaybePromise;\n"
        + GATE
        + "use rquickjs::{AsyncContext, AsyncRuntime, Error as JsError};\n",
        "rquickjs + serde imports",
    )
    for line, what in [
        ("use self::host_functions::register_host_functions;\n", "register_host_functions"),
        ("use super::api::{HOST_PRELUDE, HOST_PRELUDE_MODULE};\n", "api prelude"),
        ("use super::loader::{\n", "loader import"),
        ("use super::{loader, transpile};\n", "loader/transpile alias"),
        ("mod exec;\n", "mod exec"),
        ("mod host_functions;\n", "mod host_functions"),
        ("mod dispatch;\n", "mod dispatch"),
        ("mod hooks;\n", "mod hooks"),
        ("mod ops;\n", "mod ops"),
        ("mod runtime;\n", "mod runtime"),
    ]:
        if line not in mod:
            # `mod exec;`/`mod host_functions;` sit on one line at the top.
            if line.rstrip("\n") in mod.splitlines():
                mod = re.sub(r"(?m)^" + re.escape(line.rstrip("\n")) + r"$", GATE + line.rstrip("\n"), mod, count=1)
                continue
            print(f"MISSING line ({what}): {line!r}", file=sys.stderr)
            return 1
        mod = gate_line(mod, line, what)
    # `mod exec;\nmod host_functions;` share one source line.
    mod = sub_once(mod, "mod exec;\nmod host_functions;\n", "", "top module pair") if "mod exec;\nmod host_functions;\n" in mod else mod

    mod = sub_once(
        mod,
        "pub(crate) use state::{lock_state, recover_interrupted};\n",
        "pub(crate) use state::lock_state;\n" + GATE + "pub(crate) use state::recover_interrupted;\n",
        "state re-exports",
    )
    mod = gate_line(mod, "pub use runtime::{package_entry_root, RejectionLog};\n", "runtime re-exports")
    write(RUNTIME_MOD, mod)

    # -- state.rs: the interrupt recovery touches QuickJS -------------------
    state = read(RUNTIME_STATE)
    state = gate_line(
        state,
        "pub(crate) async fn recover_interrupted(\n",
        "recover_interrupted",
    )
    write(RUNTIME_STATE, state)

    # -- handle.rs ----------------------------------------------------------
    handle = read(HANDLE)
    handle = sub_once(
        handle,
        "use crate::extensions::loader::LoadedExtension;\n",
        "use crate::extensions::types::LoadedExtension;\n",
        "LoadedExtension import",
    )
    handle = sub_once(
        handle,
        "use crate::extensions::runtime::{\n    CommandOutcome, EmitOutcome, ExtensionConfig, ExtensionRuntime, LoadResult, Notification,\n    RendererOutcome, RuntimeState, SessionContextSnapshot, ShortcutDef, ToolDef, ToolResult,\n};\n",
        "use crate::extensions::runtime::{\n    CommandOutcome, EmitOutcome, ExtensionConfig, LoadResult, Notification, RendererOutcome,\n    RuntimeState, SessionContextSnapshot, ShortcutDef, ToolDef, ToolResult,\n};\n"
        + GATE
        + "use crate::extensions::runtime::ExtensionRuntime;\n",
        "runtime import list",
    )
    handle = sub_once(
        handle,
        "mod accessors;\nmod actor;\nmod commands;\nmod mailbox;\n\nuse actor::run_actor;\npub(crate) use commands::Command;\n",
        "mod accessors;\n"
        + GATE
        + "mod actor;\nmod commands;\nmod mailbox;\n\n"
        + GATE
        + "use actor::run_actor;\npub(crate) use commands::Command;\n",
        "handle submodules",
    )
    write(HANDLE, handle)

    # -- handle/mailbox.rs: only `start` (spawns the actor) is engine-bound -
    mailbox = read(MAILBOX)
    mailbox = sub_once(
        mailbox,
        "impl ExtensionHandle {\n    /// Create a runtime on a dedicated actor thread and return its handle.\n",
        GATE
        + "impl ExtensionHandle {\n    /// Create a runtime on a dedicated actor thread and return its handle.\n",
        "mailbox start impl",
    )
    mailbox = sub_once(
        mailbox,
        "struct ActorLiveness(Arc<AtomicBool>, usize);\n",
        GATE + "struct ActorLiveness(Arc<AtomicBool>, usize);\n",
        "ActorLiveness",
    )
    write(MAILBOX, mailbox)
    print("gated runtime/mod.rs, runtime/state.rs, handle.rs, handle/mailbox.rs")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
