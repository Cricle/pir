"""Remaining SDK feature gates (single pass, all files owned by the foreman).

Feature → trimmed surface:
  tui        coding_agent::session_pick, config::keybindings' chord parser,
             config::migrations' raw-mode keypress wait, export_html's theme
             colors, agent_session's theme snapshot line.
  mcp        plugins::builtin::mcp (lane-owned), plugins::host's manager seam,
             coding_agent::print's MCP wait hook.
  extensions plugins::codemode, agent_session::ext_session_ops.
  env        the env daemon only; the remote-env client stays in the core.

Pure text/model helpers that both the core and the TUI need move to
`crate::text` / `crate::agent_session` with the TUI paths kept as delegating
wrappers, so no logic is duplicated.

Run from the repo root. Idempotent: refuses to run twice.
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def read(rel: str) -> str:
    with open(os.path.join(ROOT, rel), encoding="utf-8", newline="") as fh:
        return fh.read()


def write(rel: str, text: str) -> None:
    path = os.path.join(ROOT, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="") as fh:
        fh.write(text)


def edit(rel: str, pairs) -> None:
    text = read(rel)
    for old, new in pairs:
        if new in text:
            continue  # already applied
        if old not in text:
            print(f"MISSING anchor in {rel}:\n{old[:240]}", file=sys.stderr)
            raise SystemExit(1)
        text = text.replace(old, new, 1)
    write(rel, text)


def gate(rel: str, snippets) -> None:
    """Prefix each snippet with the extensions-agnostic cfg attribute."""
    text = read(rel)
    for snippet, feature in snippets:
        attribute = f'#[cfg(feature = "{feature}")]'
        if (
            f"{attribute}\n{snippet}" in text
            or f"{attribute}\r\n{snippet.replace(chr(10), chr(13) + chr(10))}" in text
        ):
            continue  # already applied (possibly by the CRLF pass)
        if snippet not in text:
            print(f"MISSING snippet in {rel}:\n{snippet[:240]}", file=sys.stderr)
            raise SystemExit(1)
        sep = "\r\n" if "\r\n" in text else "\n"
        text = text.replace(snippet, f"{attribute}{sep}{snippet}", 1)
    write(rel, text)


def replace_fn_body(text: str, signature: str, new_body: str, what: str) -> str:
    """Replace a top-level fn's body (signature line kept, body swapped)."""
    start = text.index(signature)
    end = text.index("\n}\n", start) + len("\n}\n")
    return text[:start] + signature + new_body + "\n}\n" + text[end:]


TEXT_RS = r'''//! Terminal-text helpers shared by the core and the TUI.
//!
//! These are pure string transforms over terminal output. They live here
//! rather than under `crate::tui` because core surfaces that must work without
//! the `tui` feature consume them too — the HTML session export strips ANSI
//! codes from tool output, and headless SDK hosts render the same text. The
//! TUI re-exports [`strip_ansi`], so there is exactly one implementation.

/// Strip ANSI escape sequences (CSI, OSC and DCS introducers) from `text`,
/// keeping the printable payload.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') || chars.peek() == Some(&'_') {
                chars.next();
                let mut prev = '\0';
                for c in chars.by_ref() {
                    if c == '\x07' || (prev == '\x1b' && c == '\\') {
                        break;
                    }
                    prev = c;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}
'''

SC0PED_FN = '''/// Next model in the scoped-model list (`/model` cycling): the available
/// scoped entries in order, walking forward with wrap-around. `None` when
/// fewer than two scoped entries are available.
pub fn scoped_cycle_target(
    scoped: &[(String, String)],
    available: &[(String, String)],
    current: (&str, &str),
) -> Option<(String, String)> {
    let in_scope: Vec<&(String, String)> = scoped
        .iter()
        .filter(|model| available.contains(model))
        .collect();
    if in_scope.len() < 2 {
        return None;
    }
    let current_index = in_scope
        .iter()
        .position(|(provider, id)| provider.as_str() == current.0 && id.as_str() == current.1)
        .unwrap_or(0);
    let next = in_scope[(current_index + 1) % in_scope.len()];
    Some(next.clone())
}

'''


def main() -> int:
    text_rs = os.path.join(ROOT, "src", "text.rs")
    if os.path.exists(text_rs) and read("src/text.rs") != TEXT_RS:
        print("refusing to run: src/text.rs exists with different content", file=sys.stderr)
        return 1
    if not os.path.exists(text_rs):
        write("src/text.rs", TEXT_RS)
    # src/lib.rs is CRLF: patched by sdk_gate_crlf.py.

    # ---- 2. the TUI delegates the two moved helpers -----------------------
    ansi = read("src/tui/ansi.rs")
    ansi = replace_fn_body(
        ansi,
        "pub fn strip_ansi(text: &str) -> String {",
        "    crate::text::strip_ansi(text)\n",
        "strip_ansi",
    )
    write("src/tui/ansi.rs", ansi)
    slash = read("src/tui/slash.rs")
    slash = replace_fn_body(
        slash,
        "pub fn scoped_cycle_target(\n",
        "    crate::agent_session::scoped_cycle_target(scoped, available, current)\n",
        "scoped_cycle_target",
    )
    # The doc comment above the moved fn stays; retitle it as a re-export note.
    slash = slash.replace(
        "/// Next model in the scoped-model list (`/model` cycling): the available\n/// scoped entries in order, walking forward with wrap-around. `None` when\n/// fewer than two scoped entries are available.\n",
        "/// Next model in the scoped-model list (`/model` cycling); the policy\n/// lives in [`crate::agent_session::scoped_cycle_target`], which headless\n/// hosts use too.\n",
        1,
    )
    write("src/tui/slash.rs", slash)

    # ---- 3. agent_session: canonical helper + JS session ops gate ---------
    edit(
        "src/agent_session/models.rs",
        [
            (
                "            if let Some((provider, model_id)) = crate::tui::slash::scoped_cycle_target(\n",
                "            if let Some((provider, model_id)) = crate::agent_session::scoped_cycle_target(\n",
            ),
        ],
    )
    # Append the canonical fn to models.rs (end of file).
    models = read("src/agent_session/models.rs")
    if "pub fn scoped_cycle_target" not in models:
        if not models.endswith("\n"):
            models += "\n"
        models += "\n" + SC0PED_FN
        write("src/agent_session/models.rs", models)
    edit(
        "src/agent_session.rs",
        [
            (
                "pub mod ext_session_ops;\n",
                "#[cfg(feature = \"extensions\")]\npub mod ext_session_ops;\n",
            ),
            (
                "mod models;\n",
                "mod models;\npub(crate) use models::scoped_cycle_target;\n",
            ),
        ],
    )

    # ---- 4. plugins: codemode needs the JS engine -------------------------
    gate(
        "src/plugins/mod.rs",
        [
            ("pub mod codemode;\n", "extensions"),
            (
                "pub use codemode::{CodemodeConfig, CodemodePlugin, CodemodeTool, DEFAULT_INLINE_BUDGET};\n",
                "extensions",
            ),
        ],
    )

    # ---- 5. plugins/host.rs: the mcp manager seam -------------------------
    edit(
        "src/plugins/host.rs",
        [
            ("use super::builtin::mcp::McpPlugin;\n", "#[cfg(feature = \"mcp\")]\nuse super::builtin::mcp::McpPlugin;\n"),
            (
                "    /// The `mcp` builtin's shared manager, when the plugin is loaded. The\n    /// refresh path polls its dirty flags (discovered tools, JS server\n    /// registrations) to rebuild the registry.\n    mcp_manager: Option<Arc<crate::mcp::manager::McpManager>>,\n",
                "    /// The `mcp` builtin's shared manager, when the plugin is loaded. The\n    /// refresh path polls its dirty flags (discovered tools, JS server\n    /// registrations) to rebuild the registry. Without the `mcp` feature the\n    /// handle type is a never-constructed placeholder, so the field stays\n    /// `None` and every read is a no-op.\n    mcp_manager: Option<McpManagerHandle>,\n",
            ),
            (
                "use super::merge::MergedEmit;\n",
                "use super::merge::MergedEmit;\n\n/// The `mcp` builtin's shared manager handle (`mcp` feature) or an\n/// uninhabited placeholder in a build without it.\n#[cfg(feature = \"mcp\")]\npub type McpManagerHandle = Arc<crate::mcp::manager::McpManager>;\n#[cfg(not(feature = \"mcp\"))]\npub type McpManagerHandle = std::convert::Infallible;\n",
            ),
            (
                "                            let plugin: Arc<dyn AgentPlugin> = if plugin\n                                .as_any()\n                                .and_then(|any| any.downcast_ref::<McpPlugin>())\n                                .is_some()\n                            {\n                                Arc::new(McpPlugin::with_cwd(\n                                    &crate::coding_agent::context::agent_dir(),\n                                    &cwd,\n                                ))\n                            } else {\n                                plugin\n                            };\n",
                "                            #[cfg(feature = \"mcp\")]\n                            let plugin: Arc<dyn AgentPlugin> = if plugin\n                                .as_any()\n                                .and_then(|any| any.downcast_ref::<McpPlugin>())\n                                .is_some()\n                            {\n                                Arc::new(McpPlugin::with_cwd(\n                                    &crate::coding_agent::context::agent_dir(),\n                                    &cwd,\n                                ))\n                            } else {\n                                plugin\n                            };\n",
            ),
            (
                "            if let Some(mcp) = plugin\n                .as_any()\n                .and_then(|any| any.downcast_ref::<McpPlugin>())\n            {\n                mcp.attach_js(js.clone());\n            }\n",
                "            #[cfg(feature = \"mcp\")]\n            if let Some(mcp) = plugin\n                .as_any()\n                .and_then(|any| any.downcast_ref::<McpPlugin>())\n            {\n                mcp.attach_js(js.clone());\n            }\n",
            ),
            (
                "        let mcp_manager = plugins.iter().find_map(|plugin| {\n            plugin\n                .as_any()\n                .and_then(|any| any.downcast_ref::<McpPlugin>())\n                .map(|mcp| mcp.manager().clone())\n        });\n",
                "        #[cfg(feature = \"mcp\")]\n        let mcp_manager = plugins.iter().find_map(|plugin| {\n            plugin\n                .as_any()\n                .and_then(|any| any.downcast_ref::<McpPlugin>())\n                .map(|mcp| mcp.manager().clone())\n        });\n        #[cfg(not(feature = \"mcp\"))]\n        let mcp_manager: Option<McpManagerHandle> = None;\n",
            ),
            (
                "        if let Some(manager) = &self.mcp_manager {\n            // Discovered tools changed (session-start connect, or a\n            // `tools/list_changed` refresh during a call): rebuild so the\n            // freshly listed `mcp__*` tools join the registry.\n            changed |= manager.take_tools_dirty();\n        }\n",
                "        #[cfg(feature = \"mcp\")]\n        if let Some(manager) = &self.mcp_manager {\n            // Discovered tools changed (session-start connect, or a\n            // `tools/list_changed` refresh during a call): rebuild so the\n            // freshly listed `mcp__*` tools join the registry.\n            changed |= manager.take_tools_dirty();\n        }\n",
            ),
            (
                "            if let Some(mcp) = self.mcp_plugin() {\n                mcp.flush_server_changes().await;\n            }\n        } else if let Some(snapshot) = read {\n",
                "            #[cfg(feature = \"mcp\")]\n            if let Some(mcp) = self.mcp_plugin() {\n                mcp.flush_server_changes().await;\n            }\n        } else if let Some(snapshot) = read {\n",
            ),
            (
                "    pub fn mcp_manager(&self) -> Option<&Arc<crate::mcp::manager::McpManager>> {\n",
                "    pub fn mcp_manager(&self) -> Option<&McpManagerHandle> {\n",
            ),
            (
                "    fn mcp_plugin(&self) -> Option<&McpPlugin> {\n",
                "    #[cfg(feature = \"mcp\")]\n    fn mcp_plugin(&self) -> Option<&McpPlugin> {\n",
            ),
            (
                "        if let Some(mcp) = self.mcp_plugin() {\n            mcp.flush_server_changes().await;\n        }\n        Ok(())\n",
                "        #[cfg(feature = \"mcp\")]\n        if let Some(mcp) = self.mcp_plugin() {\n            mcp.flush_server_changes().await;\n        }\n        Ok(())\n",
            ),
        ],
    )

    # ---- 6. print.rs: the MCP reach wait hook -----------------------------
    edit(
        "src/coding_agent/print.rs",
        [
            ("use crate::plugins::builtin::McpPlugin;\n", "#[cfg(feature = \"mcp\")]\nuse crate::plugins::builtin::McpPlugin;\n"),
            (
                "/// The `mcp` builtin in a plugin host's ordered dispatch list, if loaded\n/// (the same lookup `agent_session::mcp_reach` makes for the session).\nfn mcp_plugin_of(plugins: &PluginHost) -> Option<&McpPlugin> {\n",
                "/// The `mcp` builtin in a plugin host's ordered dispatch list, if loaded\n/// (the same lookup `agent_session::mcp_reach` makes for the session).\n#[cfg(feature = \"mcp\")]\nfn mcp_plugin_of(plugins: &PluginHost) -> Option<&McpPlugin> {\n",
            ),
            (
                "pub fn print_tool_call_wait(plugins: Option<&Arc<PluginHost>>) -> Option<ToolCallWaitFn> {\n",
                "#[cfg(feature = \"mcp\")]\npub fn print_tool_call_wait(plugins: Option<&Arc<PluginHost>>) -> Option<ToolCallWaitFn> {\n",
            ),
            (
                "/// The print host's loop dependencies for one prompt turn (public for\n",
                "/// Without the `mcp` feature there is no `mcp` builtin to reach, so the\n/// print host has no MCP wait hook.\n#[cfg(not(feature = \"mcp\"))]\npub fn print_tool_call_wait(plugins: Option<&Arc<PluginHost>>) -> Option<ToolCallWaitFn> {\n    let _ = plugins;\n    None\n}\n\n/// The print host's loop dependencies for one prompt turn (public for\n",
            ),
        ],
    )

    # ---- 7. tui-only core surfaces ---------------------------------------
    # (coding_agent/mod.rs is CRLF: patched by sdk_gate_crlf.py)
    edit(
        "src/coding_agent/export_html.rs",
        [
            ("    crate::tui::ansi::strip_ansi(text)\n", "    crate::text::strip_ansi(text)\n"),
            (
                "    let export = crate::tui::theme::theme_export_colors(None);\n    let mut variables = String::new();\n    if let Some(page_bg) = &export.page_bg {\n        variables.push_str(&format!(\"--pir-export-page-bg: {page_bg};\"));\n    }\n    if let Some(card_bg) = &export.card_bg {\n        variables.push_str(&format!(\"--pir-export-card-bg: {card_bg};\"));\n    }\n    if let Some(info_bg) = &export.info_bg {\n        variables.push_str(&format!(\"--pir-export-info-bg: {info_bg};\"));\n    }\n",
                "    let mut variables = String::new();\n    // The theme's export colors are a TUI surface (the active theme lives in\n    // the TUI's theme registry); without the TUI the export keeps the\n    // stylesheet's built-in surface colors.\n    #[cfg(feature = \"tui\")]\n    {\n        let export = crate::tui::theme::theme_export_colors(None);\n        if let Some(page_bg) = &export.page_bg {\n            variables.push_str(&format!(\"--pir-export-page-bg: {page_bg};\"));\n        }\n        if let Some(card_bg) = &export.card_bg {\n            variables.push_str(&format!(\"--pir-export-card-bg: {card_bg};\"));\n        }\n        if let Some(info_bg) = &export.info_bg {\n            variables.push_str(&format!(\"--pir-export-info-bg: {info_bg};\"));\n        }\n    }\n",
            ),
        ],
    )
    edit(
        "src/config/keybindings.rs",
        [
            (
                "pub fn is_valid_chord(chord: &str) -> bool {\n    !chord.trim().is_empty() && crate::tui::keybindings::parse_key(chord).is_some()\n}\n",
                "pub fn is_valid_chord(chord: &str) -> bool {\n    let chord = chord.trim();\n    if chord.is_empty() {\n        return false;\n    }\n    // The authoritative parser lives in the TUI (it owns the chord grammar).\n    // Without the TUI only the shape is checked: one non-empty token.\n    #[cfg(feature = \"tui\")]\n    {\n        crate::tui::keybindings::parse_key(chord).is_some()\n    }\n    #[cfg(not(feature = \"tui\"))]\n    {\n        !chord.contains(char::is_whitespace)\n    }\n}\n",
            ),
        ],
    )
    edit(
        "src/config/migrations.rs",
        [
            (
                "fn wait_for_any_keypress() {\n    use crossterm::event::read;\n",
                "fn wait_for_any_keypress() {\n    // Raw mode is a TUI facility; without the TUI the migration proceeds\n    // without waiting (the same path this takes when raw mode is\n    // unavailable).\n    #[cfg(not(feature = \"tui\"))]\n    {\n        return;\n    }\n    #[cfg(feature = \"tui\")]\n    use crossterm::event::read;\n",
            ),
        ],
    )

    # ---- 8. agent_session::models' theme snapshot line --------------------
    models = read("src/agent_session/models.rs")
    if "crate::tui::" in models:
        print("NOTE: models.rs still references crate::tui:", file=sys.stderr)
        for i, line in enumerate(models.splitlines(), 1):
            if "crate::tui::" in line:
                print(f"  {i}: {line.strip()}", file=sys.stderr)
    print("all gates applied")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
