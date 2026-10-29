//! TUI syntax highlighting — `highlightCode` / `getLanguageFromPath` parity.
//!
//! Covers the upstream extension table, the line-oriented lexer state
//! machines, the no-language/unknown-language fallbacks, and the renderer
//! wiring (read result, write call preview, codemode script, markdown fence).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use pir::tui::ansi::{strip_ansi, FG_RESET};
use pir::tui::core::Component;
use pir::tui::markdown::{Markdown, MarkdownOptions, MarkdownTheme};
use pir::tui::messages::{format_tool_call_line, ToolExecutionComponent};
use pir::tui::syntax::{
    highlight_code, highlight_lines_with_theme, language_from_path, normalize_language,
    supports_language,
};
use pir::tui::theme::{set_theme, with_theme, Theme, ThemeColor};

fn tools_expanded(expanded: bool) -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(expanded))
}

/// The opening escape of a syntax color under the active theme. Callers hold
/// the `g_tui` theme lock, so the palette cannot move under them.
fn ansi(color: ThemeColor) -> String {
    with_theme(|theme| theme.get_fg_ansi(color))
}

fn all_scopes() -> (String, String, String, String, String, String) {
    with_theme(|theme: &Theme| {
        (
            theme.get_fg_ansi(ThemeColor::SyntaxKeyword),
            theme.get_fg_ansi(ThemeColor::SyntaxString),
            theme.get_fg_ansi(ThemeColor::SyntaxVariable),
            theme.get_fg_ansi(ThemeColor::SyntaxNumber),
            theme.get_fg_ansi(ThemeColor::SyntaxComment),
            theme.get_fg_ansi(ThemeColor::SyntaxFunction),
        )
    })
}

// ---------------------------------------------------------------------------
// Upstream `getLanguageFromPath` table
// ---------------------------------------------------------------------------

#[test]
fn language_table_matches_upstream_extension_map() {
    let table: &[(&str, &str)] = &[
        ("a.ts", "typescript"),
        ("a.tsx", "typescript"),
        ("a.js", "javascript"),
        ("a.jsx", "javascript"),
        ("a.mjs", "javascript"),
        ("a.cjs", "javascript"),
        ("a.py", "python"),
        ("a.rb", "ruby"),
        ("a.rs", "rust"),
        ("a.go", "go"),
        ("a.java", "java"),
        ("a.kt", "kotlin"),
        ("a.swift", "swift"),
        ("a.c", "c"),
        ("a.h", "c"),
        ("a.cpp", "cpp"),
        ("a.cc", "cpp"),
        ("a.cxx", "cpp"),
        ("a.hpp", "cpp"),
        ("a.cs", "csharp"),
        ("a.php", "php"),
        ("a.sh", "bash"),
        ("a.bash", "bash"),
        ("a.zsh", "bash"),
        ("a.fish", "fish"),
        ("a.ps1", "powershell"),
        ("a.sql", "sql"),
        ("a.html", "html"),
        ("a.htm", "html"),
        ("a.css", "css"),
        ("a.scss", "scss"),
        ("a.sass", "sass"),
        ("a.less", "less"),
        ("a.json", "json"),
        ("a.yaml", "yaml"),
        ("a.yml", "yaml"),
        ("a.toml", "toml"),
        ("a.xml", "xml"),
        ("a.md", "markdown"),
        ("a.markdown", "markdown"),
        ("Dockerfile", "dockerfile"),
        ("Makefile", "makefile"),
        ("CMakeLists.cmake", "cmake"),
        ("a.lua", "lua"),
        ("a.perl", "perl"),
        ("a.r", "r"),
        ("a.scala", "scala"),
        ("a.clj", "clojure"),
        ("a.ex", "elixir"),
        ("a.exs", "elixir"),
        ("a.erl", "erlang"),
        ("a.hs", "haskell"),
        ("a.ml", "ocaml"),
        ("a.vim", "vim"),
        ("a.graphql", "graphql"),
        ("a.proto", "protobuf"),
        ("a.tf", "hcl"),
        ("a.hcl", "hcl"),
    ];
    for (path, expected) in table {
        assert_eq!(
            language_from_path(path),
            Some(*expected),
            "extension table entry {path}"
        );
    }
    // The last extension is lowercased (upstream `.toLowerCase()`).
    assert_eq!(language_from_path("src/MAIN.PY"), Some("python"));
    assert_eq!(language_from_path("C:\\repo\\lib\\MAIN.RS"), Some("rust"));
    // Unknown extensions/languages never auto-detect.
    assert_eq!(language_from_path("notes.xyz"), None);
    assert_eq!(language_from_path("README"), None);
    assert_eq!(language_from_path("archive.tar.gz"), None);
}

#[test]
fn language_table_handles_bare_dockerfile_makefile_and_aliases() {
    assert_eq!(language_from_path("Dockerfile"), Some("dockerfile"));
    assert_eq!(language_from_path("src/Dockerfile"), Some("dockerfile"));
    assert_eq!(language_from_path("Makefile"), Some("makefile"));
    assert_eq!(language_from_path("build/Makefile"), Some("makefile"));
    assert_eq!(language_from_path("unknown.zzz"), None);

    assert_eq!(normalize_language("ts"), Some("typescript"));
    assert_eq!(normalize_language("JS"), Some("javascript"));
    assert_eq!(normalize_language("python"), Some("python"));
    assert_eq!(normalize_language("rs"), Some("rust"));
    assert_eq!(normalize_language("c++"), Some("cpp"));
    assert_eq!(normalize_language("golang"), Some("go"));
    assert_eq!(normalize_language("yml"), Some("yaml"));
    assert_eq!(normalize_language("terraform"), Some("hcl"));
    // Fence info strings may carry extra tokens; the first token wins.
    assert_eq!(normalize_language("rust title=main.rs"), Some("rust"));
    assert_eq!(normalize_language(""), None);
    assert_eq!(normalize_language("brainfuck"), None);
    assert!(supports_language("typescript"));
    assert!(!supports_language("brainfuck"));
}

// ---------------------------------------------------------------------------
// ANSI round-trip (the highlighter must never alter displayed text)
// ---------------------------------------------------------------------------

#[test]
fn strip_ansi_roundtrip_over_unicode_and_tabs() {
    let corpus = concat!(
        "fn main() {\n",
        "\tlet s = \"héllo 世界 🚀\";\n",
        "\t// comment ✓ with émojis\n",
        "\tlet n = 0x1f + 42;\n",
        "}\n",
        "\t\n",
        "```\n",
        "plain tail\tkept\n",
    );
    for language in [
        "rust",
        "javascript",
        "python",
        "html",
        "json",
        "bash",
        "markdown",
        "dockerfile",
        "brainfuck",
    ] {
        let highlighted = highlight_code(corpus, Some(language)).join("\n");
        assert_eq!(
            strip_ansi(&highlighted),
            corpus,
            "language {language} must preserve every byte except ANSI"
        );
    }
    let no_language = highlight_code(corpus, None).join("\n");
    assert_eq!(strip_ansi(&no_language), corpus);
}

// ---------------------------------------------------------------------------
// Scope goldens
// ---------------------------------------------------------------------------

#[test]
fn python_triple_string_styles_lines_and_leaves_blank_line_untouched() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let (keyword, string_color, _, _, _, _) = all_scopes();
    let code = "def f():\n    \"\"\"doc\n\n    still doc\"\"\"\n    return 1";
    let lines = highlight_code(code, Some("python"));
    assert_eq!(lines.len(), 5);
    assert!(
        lines[0].contains(&keyword),
        "`def` is a keyword: {:?}",
        lines[0]
    );
    assert!(
        lines[1].contains(&string_color),
        "the opening triple-quote line is a string: {:?}",
        lines[1]
    );
    assert_eq!(lines[2], "", "blank lines inside triple strings stay blank");
    assert!(
        lines[3].contains(&string_color),
        "the continuation line is a string: {:?}",
        lines[3]
    );
    assert!(
        lines[4].contains(&keyword),
        "`return` after the string is a keyword: {:?}",
        lines[4]
    );
    assert_eq!(strip_ansi(&lines.join("\n")), code);
}

#[test]
fn javascript_regex_literal_is_a_string_but_division_is_not() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let (_, string_color, _, _, _, _) = all_scopes();

    let regex = &highlight_code("const re = /ab+c/gi;", Some("javascript"))[0];
    assert!(
        regex.contains(&format!("{string_color}/ab+c/gi{FG_RESET}")),
        "regex literal styled as a string: {regex:?}"
    );

    let division = &highlight_code("let x = a / b;", Some("javascript"))[0];
    assert!(
        !division.contains(&string_color),
        "division must not be mistaken for a regex: {division:?}"
    );
}

#[test]
fn html_tags_attributes_and_strings_are_scoped() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let (keyword, string_color, variable, _, _, _) = all_scopes();
    let line = &highlight_code("<div class=\"x\">hi</div>", Some("html"))[0];
    assert!(line.contains(&keyword), "tag name is a keyword: {line:?}");
    assert!(
        line.contains(&variable),
        "attribute name is a variable: {line:?}"
    );
    assert!(
        line.contains(&format!("{string_color}\"x\"{FG_RESET}")),
        "attribute value is a string: {line:?}"
    );
    assert_eq!(strip_ansi(line), "<div class=\"x\">hi</div>");
}

#[test]
fn json_keys_are_variables_values_are_strings_and_numbers() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let (_, string_color, variable, number, _, _) = all_scopes();
    let line = &highlight_code(
        "{\"key\": \"value\", \"n\": 42, \"ok\": true}",
        Some("json"),
    )[0];
    assert!(
        line.contains(&format!("{variable}\"key\"{FG_RESET}")),
        "object key is a variable: {line:?}"
    );
    assert!(
        line.contains(&format!("{string_color}\"value\"{FG_RESET}")),
        "string value keeps the string scope: {line:?}"
    );
    assert!(
        line.contains(&format!("{number}42{FG_RESET}")),
        "number value: {line:?}"
    );
    assert!(
        line.contains(&format!("{number}true{FG_RESET}")),
        "literal uses the number/literal scope: {line:?}"
    );
}

// ---------------------------------------------------------------------------
// State machines
// ---------------------------------------------------------------------------

#[test]
fn block_comments_carry_across_lines() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let (keyword, _, _, _, comment, _) = all_scopes();
    let code = "/* start\nstill comment */ let x = 1;";
    let lines = highlight_code(code, Some("rust"));
    assert!(
        lines[0].starts_with(&comment),
        "first line opens a comment: {:?}",
        lines[0]
    );
    assert!(
        lines[1].starts_with(&comment),
        "state carried into the second line: {:?}",
        lines[1]
    );
    assert!(
        lines[1].contains(&keyword),
        "code after the closing delimiter is scanned normally: {:?}",
        lines[1]
    );
    assert_eq!(strip_ansi(&lines.join("\n")), code);
}

#[test]
fn escaped_quotes_do_not_close_strings() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let (_, string_color, _, _, _, _) = all_scopes();
    let line = &highlight_code("let s = \"a\\\"b\";", Some("javascript"))[0];
    assert!(
        line.contains(&format!("{string_color}\"a\\\"b\"{FG_RESET}")),
        "the escaped quote stays inside one string run: {line:?}"
    );
    assert_eq!(strip_ansi(line), "let s = \"a\\\"b\";");
}

#[test]
fn template_literals_span_lines() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let (_, string_color, _, _, _, _) = all_scopes();
    let code = "const t = `hello\nworld`;";
    let lines = highlight_code(code, Some("javascript"));
    assert!(
        lines[0].contains(&string_color),
        "opening template literal: {:?}",
        lines[0]
    );
    assert!(
        lines[1].contains(&format!("{string_color}world`{FG_RESET}")),
        "template literal continues on the next line: {:?}",
        lines[1]
    );
}

// ---------------------------------------------------------------------------
// Fallbacks and caps
// ---------------------------------------------------------------------------

#[test]
fn unknown_and_missing_languages_use_the_fallback_color() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let expected_unknown = with_theme(|theme| theme.fg(ThemeColor::MdCodeBlock, "plain text"));
    assert_eq!(
        highlight_code("plain text", Some("brainfuck")),
        vec![expected_unknown]
    );
    assert_eq!(
        highlight_code("plain text", None),
        vec![with_theme(
            |theme| theme.fg(ThemeColor::MdCodeBlock, "plain text")
        )]
    );

    // Tool call sites pass `toolOutput` when the path is missing/unknown.
    let tool_output = with_theme(|theme| {
        highlight_lines_with_theme(theme, "plain text", None, ThemeColor::ToolOutput)
    });
    assert_eq!(
        tool_output,
        vec![with_theme(
            |theme| theme.fg(ThemeColor::ToolOutput, "plain text")
        )]
    );
}

#[test]
fn highlighting_stops_at_the_line_cap_and_leaves_the_tail_plain() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let keyword = ansi(ThemeColor::SyntaxKeyword);
    let code = std::iter::repeat_n("fn f() {}", pir::tui::syntax::MAX_HIGHLIGHT_LINES + 2)
        .collect::<Vec<_>>()
        .join("\n");
    let lines = highlight_code(&code, Some("rust"));
    assert_eq!(lines.len(), pir::tui::syntax::MAX_HIGHLIGHT_LINES + 2);
    assert!(
        lines[pir::tui::syntax::MAX_HIGHLIGHT_LINES - 1].contains(&keyword),
        "the last line inside the cap is still highlighted"
    );
    assert_eq!(
        lines[pir::tui::syntax::MAX_HIGHLIGHT_LINES],
        "fn f() {}",
        "the tail is emitted verbatim"
    );
}

#[test]
fn oversized_single_line_is_left_plain() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let huge = format!(
        "fn {}",
        "a".repeat(pir::tui::syntax::MAX_HIGHLIGHT_BYTES + 1)
    );
    let lines = highlight_code(&huge, Some("rust"));
    assert_eq!(lines, vec![huge]);
}

// ---------------------------------------------------------------------------
// Renderer integration (existing render helpers)
// ---------------------------------------------------------------------------

#[test]
fn expanded_read_result_highlights_the_file_content() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let keyword = ansi(ThemeColor::SyntaxKeyword);
    let component = ToolExecutionComponent::new(
        "read",
        Arc::new(serde_json::json!({ "file_path": "demo.py" })),
        tools_expanded(true),
    );
    component.update_result(Arc::from("def f():\n    return 1"), false, Some(10));
    let rendered = component.render(80).join("\n");
    assert!(
        rendered.contains(&keyword),
        "read success highlights Python keywords"
    );
    assert!(strip_ansi(&rendered).contains("def f():"));
}

#[test]
fn error_read_result_stays_plain() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let keyword = ansi(ThemeColor::SyntaxKeyword);
    let component = ToolExecutionComponent::new(
        "read",
        Arc::new(serde_json::json!({ "file_path": "demo.py" })),
        tools_expanded(true),
    );
    component.update_result(Arc::from("def f():"), true, Some(10));
    let rendered = component.render(80).join("\n");
    assert!(
        !rendered.contains(&keyword),
        "error output is never syntax highlighted"
    );
}

#[test]
fn write_call_preview_highlights_the_first_ten_lines() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let keyword = ansi(ThemeColor::SyntaxKeyword);
    let content = (0..12)
        .map(|index| format!("fn f{index}() {{}}"))
        .collect::<Vec<_>>()
        .join("\n");
    let component = ToolExecutionComponent::new(
        "write",
        Arc::new(serde_json::json!({
            "file_path": "src/main.rs",
            "content": content,
        })),
        tools_expanded(false),
    );
    let rendered = component.render(200).join("\n");
    assert!(
        rendered.contains(&keyword),
        "write preview highlights Rust keywords"
    );
    let plain = strip_ansi(&rendered);
    assert!(plain.contains("fn f0() {}"));
    assert!(
        !plain.contains("fn f10() {}"),
        "collapsed preview stops at 10 lines"
    );
    assert!(plain.contains("more lines"));
}

#[test]
fn write_call_with_unknown_extension_stays_plain() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let keyword = ansi(ThemeColor::SyntaxKeyword);
    let component = ToolExecutionComponent::new(
        "write",
        Arc::new(serde_json::json!({
            "file_path": "notes.unknownext",
            "content": "fn main() {}",
        })),
        tools_expanded(false),
    );
    let rendered = component.render(200).join("\n");
    assert!(
        !rendered.contains(&keyword),
        "an unknown path keeps toolOutput"
    );
}

#[test]
fn codemode_script_highlights_as_javascript_with_a_ten_line_preview() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let keyword = ansi(ThemeColor::SyntaxKeyword);
    let code = (0..12)
        .map(|index| format!("let v{index} = {index};"))
        .collect::<Vec<_>>()
        .join("\n");
    let text = format_tool_call_line("codemode", &serde_json::json!({ "code": code }), false);
    assert!(
        text.contains(&keyword),
        "the codemode script is highlighted as JavaScript"
    );
    let plain = strip_ansi(&text);
    let lines: Vec<&str> = plain.lines().collect();
    // Title + 10 script lines + the expand hint.
    assert_eq!(lines.len(), 12, "codemode preview: {plain:?}");
    assert!(lines[1].contains("let v0"));
    assert!(lines[10].contains("let v9"));
    assert!(lines[11].contains("more lines"));
}

// ---------------------------------------------------------------------------
// Markdown fenced blocks + theme swap
// ---------------------------------------------------------------------------

fn custom_highlight(_: &str, _: Option<&str>) -> Vec<String> {
    vec!["CUSTOM-CODE".to_string()]
}

#[test]
fn markdown_fences_use_the_theme_highlight_hook() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let keyword = ansi(ThemeColor::SyntaxKeyword);

    let markdown = Markdown::with_options(
        "```rust\nfn main() {}\n```",
        0,
        0,
        MarkdownTheme::default(),
        MarkdownOptions::default(),
    );
    let rendered = markdown.render(60).join("\n");
    assert!(
        rendered.contains(&keyword),
        "rust fence is highlighted: {rendered:?}"
    );
    assert!(strip_ansi(&rendered).contains("fn main() {}"));

    // Unknown/absent fence language keeps the mdCodeBlock color.
    let plain = Markdown::with_options(
        "```brainfuck\nplain text\n```",
        0,
        0,
        MarkdownTheme::default(),
        MarkdownOptions::default(),
    );
    let rendered = plain.render(60).join("\n");
    let md_code_block = with_theme(|theme| theme.get_fg_ansi(ThemeColor::MdCodeBlock));
    assert!(
        rendered.contains(&md_code_block),
        "unknown fences keep mdCodeBlock: {rendered:?}"
    );
    assert!(!rendered.contains(&keyword));
}

#[test]
fn markdown_theme_hook_is_honored() {
    let theme = MarkdownTheme {
        highlight_code: custom_highlight,
        ..MarkdownTheme::default()
    };
    let markdown = Markdown::with_options(
        "```rust\nignored\n```",
        0,
        0,
        theme,
        MarkdownOptions::default(),
    );
    let rendered = markdown.render(60).join("\n");
    assert!(
        rendered.contains("CUSTOM-CODE"),
        "the custom hook replaces the default highlighter: {rendered:?}"
    );
}

#[test]
fn theme_swap_changes_the_highlight_output() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let dark = highlight_code("fn main() {}", Some("rust"));
    set_theme("light");
    let light = highlight_code("fn main() {}", Some("rust"));
    // Restore before asserting so a failure cannot leak the light theme.
    set_theme("dark");
    assert_ne!(
        dark, light,
        "the same code under two themes must produce different ANSI"
    );
}
