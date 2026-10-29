"""Honest diagnosis for a `builtin:<name>` path whose feature is off.

Today a trimmed build reports `Unknown built-in extension: builtin:<name>` for
a builtin that simply was not compiled in, which is the wrong diagnosis. Add a
name→feature table and route both diagnostics through one helper. In a full
build the helper is only reached for genuinely unknown names, so the default
behavior (and the pinned upstream wording) is unchanged.
"""

import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def read(rel: str) -> str:
    with open(os.path.join(ROOT, rel), encoding="utf-8", newline="") as fh:
        return fh.read()


def write(rel: str, text: str) -> None:
    with open(os.path.join(ROOT, rel), "w", encoding="utf-8", newline="") as fh:
        fh.write(text)


def patch(rel: str, pairs) -> None:
    text = read(rel)
    nl = "\r\n" if "\r\n" in text else "\n"
    for old, new in pairs:
        old_n = old if nl == "\n" else old.replace("\n", "\r\n")
        new_n = new if nl == "\n" else new.replace("\n", "\r\n")
        if new_n in text:
            continue
        if old_n not in text:
            print(f"MISSING anchor in {rel}:\n{old[:200]}", file=sys.stderr)
            sys.exit(1)
        text = text.replace(old_n, new_n, 1)
    write(rel, text)


HELPER = '''/// Every builtin name together with the cargo feature that compiles it in.
/// Used only for the diagnostics of a `builtin:<name>` path that did not
/// resolve: in a full build every name here is registered, so the entry is
/// reachable only from a trimmed build.
const BUILTIN_FEATURES: &[(&str, &str)] = &[
    ("subagent", "subagent"),
    ("tool-repair", "tool-repair"),
    ("tool-search", "tool-search"),
    ("mcp", "mcp"),
    ("python", "python"),
    ("codemode", "extensions"),
];

/// Diagnostic for a `builtin:<name>` pseudo-path that did not resolve.
///
/// A name that is a real builtin in a full build but is absent here names the
/// missing feature, so a trimmed build says *why* instead of claiming the
/// name is unknown; a genuinely unknown name keeps the upstream wording
/// (`Unknown built-in extension: builtin:<name>`).
pub fn missing_builtin_message(name: &str) -> String {
    match BUILTIN_FEATURES.iter().find(|(builtin, _)| *builtin == name) {
        Some((_, feature)) => format!(
            "Built-in extension `{BUILTIN_PATH_PREFIX}{name}` is not compiled into this build \\
             (the `{feature}` feature is off)"
        ),
        None => format!("Unknown built-in extension: {BUILTIN_PATH_PREFIX}{name}"),
    }
}

'''


def main() -> int:
    # 1. the table + helper, next to the factory list it describes.
    patch(
        "src/plugins/builtin.rs",
        [
            (
                "/// Instantiate the builtin plugin for `builtin:<name>`; `None` for an\n",
                HELPER + "/// Instantiate the builtin plugin for `builtin:<name>`; `None` for an\n",
            )
        ],
    )

    # 2. both diagnostics sites route through the helper.
    patch(
        "src/plugins/host.rs",
        [
            (
                """                        None => {
                            let path = text.into_owned();
                            diagnostics.push(PluginDiagnostic {
                                kind: super::registry::DiagnosticKind::Error,
                                path: path.clone(),
                                message: format!("Unknown built-in extension: {path}"),
                            });
                        }
""",
                """                        None => {
                            let path = text.into_owned();
                            diagnostics.push(PluginDiagnostic {
                                kind: super::registry::DiagnosticKind::Error,
                                path: path.clone(),
                                message: super::builtin::missing_builtin_message(name),
                            });
                        }
""",
            )
        ],
    )
    patch(
        "src/plugins/loader.rs",
        [
            (
                """            diagnostics.push(super::registry::PluginDiagnostic {
                kind: super::registry::DiagnosticKind::Error,
                path: format!("{BUILTIN_PATH_PREFIX}{name}"),
                message: format!("Unknown built-in extension: {BUILTIN_PATH_PREFIX}{name}"),
            });
""",
                """            diagnostics.push(super::registry::PluginDiagnostic {
                kind: super::registry::DiagnosticKind::Error,
                path: format!("{BUILTIN_PATH_PREFIX}{name}"),
                message: super::builtin::missing_builtin_message(name),
            });
""",
            )
        ],
    )

    # 3. the doc comment above `unknown_builtin_errors` stays accurate; the
    #    function name is unchanged (callers untouched).
    text = read("src/plugins/loader.rs")
    text = text.replace(
        """/// Errors for `builtin:<name>` paths that name no registered builtin
/// (upstream: `result.errors.push({ path, error: "Unknown built-in
/// extension: " + path })`).""",
        """/// Errors for `builtin:<name>` paths that name no registered builtin
/// (upstream: `result.errors.push({ path, error: "Unknown built-in
/// extension: " + path })`). A name that is a builtin only in a full build
/// reports the missing feature instead ([`super::builtin::missing_builtin_message`]).""",
        1,
    )
    write("src/plugins/loader.rs", text)
    print("builtin-missing diagnostics rewired")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
