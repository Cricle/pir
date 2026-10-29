"""Add the compiled-out-builtin diagnostic pin to tests/suites/plugins_units.rs."""

import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
REL = "tests/suites/plugins_units.rs"

TEST = """/// A `builtin:<name>` path that names a real builtin compiled out of this
/// build must say *why* instead of claiming the name is unknown; only a
/// genuinely unknown name keeps the upstream wording (the test above).
#[test]
fn compiled_out_builtins_name_their_cargo_feature() {
    // Every entry the diagnostic table knows is a registered builtin in a
    // full build (the default feature set this suite runs under).
    for (name, feature) in [
        ("subagent", "subagent"),
        ("tool-repair", "tool-repair"),
        ("tool-search", "tool-search"),
        ("mcp", "mcp"),
        ("python", "python"),
        ("codemode", "extensions"),
    ] {
        assert!(
            builtin_names().contains(&name),
            "{name} is not a registered builtin name in a full build"
        );
        assert_eq!(
            pir::plugins::builtin::missing_builtin_message(name),
            format!(
                "Built-in extension `builtin:{name}` is not compiled into this build (the `{feature}` feature is off)"
            ),
            "the diagnostic for a compiled-out builtin must name the feature"
        );
    }
    // Unknown names are untouched by the table.
    assert_eq!(
        pir::plugins::builtin::missing_builtin_message("definitely-not-a-builtin"),
        "Unknown built-in extension: builtin:definitely-not-a-builtin"
    );
}
"""


def main() -> int:
    path = os.path.join(ROOT, REL)
    with open(path, encoding="utf-8", newline="") as fh:
        text = fh.read()
    if "compiled_out_builtins_name_their_cargo_feature" in text:
        print("already added")
        return 0
    nl = "\r\n" if "\r\n" in text else "\n"
    anchor = (
        "    assert_eq!(" + nl
        + "        diagnostics[0].message," + nl
        + '        "Unknown built-in extension: builtin:nope"' + nl
        + "    );" + nl
        + "}" + nl
    )
    if anchor not in text:
        print("anchor missing", file=sys.stderr)
        return 1
    body = TEST if nl == "\n" else TEST.replace("\n", "\r\n")
    text = text.replace(anchor, anchor + nl + body + nl, 1)
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(text)
    print("test added")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
