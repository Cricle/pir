"""SDK reviewer findings — fixes 1, 3, 5, 6, 7.

1 (HIGH)  sdk::AgentBuilder must not silently ignore `extension_paths` in a
          build without the JS engine.
3 (MED)   config::keybindings validates the chord's `+` shape even without the
          TUI's authoritative parser (no empty parts, one final key token).
5 (LOW)   PluginHost::mcp_manager is compiled only with the `mcp` feature
          (the no-feature placeholder made it a permanently-None accessor).
6 (LOW)   drop the no-op `sqlite` feature (SQLite rides `extensions`).
7 (MED)   CI gains the minimal-configuration checks.
Docs updated: SDK-SCOPE.md, docs/sdk.md, README.md.
"""

import os
import re
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


def main() -> int:
    # --- 1. the builder must fail loudly --------------------------------
    patch(
        "src/sdk/builder.rs",
        [
            (
                """        let extensions = if self.extension_paths.is_empty() {
            None
        } else {
            crate::agent_session::load_extension_runtime(
                &self.extension_paths,
                crate::extensions::ExtensionConfig {
                    mode: "headless".to_owned(),
                    cwd: cwd.clone(),
                    ..Default::default()
                },
            )
            .await
        };
""",
                """        let extensions = if self.extension_paths.is_empty() {
            None
        } else {
            // A build without the JS engine cannot host extensions: fail the
            // build step rather than handing back an agent that silently
            // ignores the requested paths.
            #[cfg(not(feature = "extensions"))]
            {
                return Err(SdkError::Other(anyhow::anyhow!(
                    "extension_paths were requested, but this build has no JS extension \\
                     runtime (the `extensions` feature is off)"
                )));
            }
            #[cfg(feature = "extensions")]
            {
                crate::agent_session::load_extension_runtime(
                    &self.extension_paths,
                    crate::extensions::ExtensionConfig {
                        mode: "headless".to_owned(),
                        cwd: cwd.clone(),
                        ..Default::default()
                    },
                )
                .await
            }
        };
""",
            )
        ],
    )

    # --- 3. chord shape validation without the TUI -----------------------
    patch(
        "src/config/keybindings.rs",
        [
            (
                """    // The authoritative parser lives in the TUI (it owns the chord grammar).
    // Without the TUI only the shape is checked: one non-empty token.
    #[cfg(feature = "tui")]
    {
        crate::tui::keybindings::parse_key(chord).is_some()
    }
    #[cfg(not(feature = "tui"))]
    {
        !chord.contains(char::is_whitespace)
    }
""",
                """    // The authoritative parser lives in the TUI (it owns the key-name table).
    #[cfg(feature = "tui")]
    {
        return crate::tui::keybindings::parse_key(chord).is_some();
    }
    // Without it the chord's *shape* is still validated: at least one part and
    // no empty part around the `+` separators, so junk such as `"ctrl+"`
    // or `"a++b"` is reported instead of being accepted silently.
    #[cfg(not(feature = "tui"))]
    {
        let mut parts = chord.split('+');
        let first = parts.next().unwrap_or_default();
        !first.trim().is_empty() && parts.all(|part| !part.trim().is_empty())
    }
""",
            )
        ],
    )

    # --- 5. the mcp accessor is mcp-only --------------------------------
    patch(
        "src/plugins/host.rs",
        [
            (
                """    /// The `mcp` builtin's shared manager, when the plugin is loaded (host
    /// seam for status surfaces and tests).
    pub fn mcp_manager(&self) -> Option<&McpManagerHandle> {
""",
                """    /// The `mcp` builtin's shared manager, when the plugin is loaded (host
    /// seam for status surfaces and tests).
    #[cfg(feature = "mcp")]
    pub fn mcp_manager(&self) -> Option<&McpManagerHandle> {
""",
            )
        ],
    )

    # --- 6. drop the no-op `sqlite` feature ------------------------------
    patch(
        "Cargo.toml",
        [
            (
                'full = ["tui", "extensions", "python", "mcp", "sqlite", "env", "server"]',
                'full = ["tui", "extensions", "python", "mcp", "env", "server"]',
            ),
            (
                """# SQLite-backed extension shims. The node:sqlite / better-sqlite3 shims are
# host functions *of* the extension engine, so this feature rides `extensions`
# (which pulls rusqlite): SQLite cannot be trimmed out of the engine yet — the
# JS-side stub work that would allow it is a follow-up.
sqlite = ["extensions"]
""",
                """# (No `sqlite` feature: the node:sqlite / better-sqlite3 shims are host
# functions *of* the extension engine and ride `extensions`, which pulls
# rusqlite. Splitting SQLite back out needs JS-side stubs; see SDK-SCOPE.md §4.)
""",
            ),
        ],
    )

    # --- 7. CI: the minimal configuration must stay green -----------------
    patch(
        ".github/workflows/ci.yml",
        [
            (
                """      - name: Clippy
        run: cargo clippy --all-targets --all-features -- -D warnings
""",
                """      - name: Clippy
        run: cargo clippy --all-targets --all-features -- -D warnings
      - name: Minimal feature configurations
        # The SDK's trimming contract (SDK-SCOPE.md): the core library builds
        # with no features, and every feature is selectable on its own. Nothing
        # else in CI would catch a regression here.
        run: |
          cargo check --lib --no-default-features
          for f in tui extensions python mcp env server; do
            cargo check --lib --no-default-features --features "$f"
          done
""",
            )
        ],
    )

    # --- docs -------------------------------------------------------------
    patch(
        "SDK-SCOPE.md",
        [
            (
                'full = ["tui", "extensions", "python", "mcp", "sqlite", "env", "server"]',
                'full = ["tui", "extensions", "python", "mcp", "env", "server"]',
            ),
            (
                'python     = ["dep:rustpython-vm", "dep:rustpython-stdlib", "dep:rustpython-pylib"]',
                'python     = ["dep:rustpython-vm", "dep:rustpython-stdlib", "dep:rustpython-pylib"]',
            ),
            (
                'sqlite     = ["extensions"]                         # 与引擎同生命周期，见 §4\n',
                '',
            ),
            (
                '1. **`sqlite` 不能从引擎里单独摘掉**：node:sqlite / better-sqlite3 的宿主函数住在引擎的 host-fn 表里，',
                '1. **没有独立的 `sqlite` 特性**：node:sqlite / better-sqlite3 的宿主函数住在引擎的 host-fn 表里，',
            ),
        ],
    )
    patch(
        "docs/sdk.md",
        [
            (
                "| `sqlite` | the node:sqlite / better-sqlite3 host shims | implies `extensions` |\n",
                "",
            ),
            (
                "| `mcp` | MCP client + the `mcp` builtin (rmcp 3.5) | implies `extensions` (the exposure/output-schema model is built on the codemode engine) |",
                "| `mcp` | MCP client + the `mcp` builtin (rmcp 3.5) | implies `extensions` (the exposure/output-schema model is built on the codemode engine); SQLite's node:sqlite shims ride `extensions` too |",
            ),
            (
                "`SDK-SCOPE.md` records the exact boundary and the two known residuals\n(SQLite cannot be split out of the engine yet; MCP carries `extensions`).",
                "`SDK-SCOPE.md` records the exact boundary and the known residuals (no\nstandalone SQLite feature — its shims are engine host functions; MCP carries\n`extensions`).",
            ),
        ],
    )
    patch(
        "README.md",
        [
            (
                "(`tui`, `extensions`, `python`, `mcp`, `sqlite`, `env`, `server`; `full` is the default and",
                "(`tui`, `extensions`, `python`, `mcp`, `env`, `server`; `full` is the default and",
            ),
            (
                "`SDK-SCOPE.md` documents the boundary (which types stay available without a feature, what a\ntrimmed build reports instead of silently degrading) and the two known residuals.",
                "`SDK-SCOPE.md` documents the boundary (which types stay available without a feature, what a\ntrimmed build reports instead of silently degrading) and the known residuals.",
            ),
        ],
    )
    print("reviewer fixes applied")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
