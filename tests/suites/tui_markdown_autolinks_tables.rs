//! Round-2 parity-gap repairs (upstream `packages/tui/src/components/markdown.ts`
//! + marked v18 `packages/coding-agent/src/core/export-html/vendor/marked.min.js`):
//! - GFM extended autolinks (marked `url` tokenizer + `_backpedal`): bare
//!   `www.…` / `scheme://…` / bare emails linkified in prose;
//! - table-cell wrap style restoration (upstream `wrapCellText`,
//!   markdown.ts:839-846): every non-final wrapped fragment ends with the
//!   `22;23;24;25;27;28;29;39m` reset and the surrounding style prefix.

use pir::tui::core::Component;
use pir::tui::markdown::{Markdown, MarkdownTheme};

/// Render with no padding at a wide width; trailing width padding trimmed.
///
/// The OSC 8 gate is process-global and this suite pins the URL-parentheses
/// fallback shape these tests assert (upstream's Links tests pin
/// `hyperlinks: false` the same way, `markdown.test.ts:1620-1642`); the
/// enabled path is covered by the OSC 8 section of `tui_markdown_parity`.
/// The `g_tui`-wide theme lock serialises the pin (and the render that reads
/// it) against every other suite's pin — in particular the enabled-path tests
/// — so a foreign `Some(true)` can never land inside this render.
fn render(text: &str, width: usize) -> Vec<String> {
    let _guard = super::tui_system_theme::lock();
    pir::tui::markdown::set_global_hyperlinks(Some(false));
    Markdown::new(text, 0, 0)
        .render(width)
        .into_iter()
        .map(|line| line.trim_end().to_string())
        .collect()
}

/// Render a single-line construct to its first content line.
fn render_one(text: &str) -> String {
    render(text, 400).into_iter().next().unwrap_or_default()
}

fn theme() -> MarkdownTheme {
    MarkdownTheme::default()
}

/// The styled link shape pir renders when text == href (angle autolinks,
/// scheme URLs, mailto emails).
fn styled_link(t: &MarkdownTheme, text: &str) -> String {
    (t.link)(&(t.underline)(text))
}

/// The non-OSC-8 fallback shape: styled text plus ` (url)` (markdown.ts:707-717).
fn fallback_link(t: &MarkdownTheme, text: &str, href: &str) -> String {
    format!("{} ({})", styled_link(t, text), (t.link_url)(href))
}

// ---------------------------------------------------------------------------
// Extended autolinks: bare URLs
// ---------------------------------------------------------------------------

#[test]
fn bare_www_url_links_with_http_href_fallback() {
    let t = theme();
    assert_eq!(
        render_one("visit www.example.com now"),
        format!(
            "visit {} now",
            fallback_link(&t, "www.example.com", "http://www.example.com")
        )
    );
}

#[test]
fn bare_scheme_url_links_without_fallback() {
    let t = theme();
    assert_eq!(
        render_one("go to https://example.com/a?b=1&c=2 now"),
        format!(
            "go to {} now",
            styled_link(&t, "https://example.com/a?b=1&c=2")
        )
    );
    assert_eq!(
        render_one("ftp://files.example.com/pub"),
        styled_link(&t, "ftp://files.example.com/pub")
    );
}

#[test]
fn scheme_match_is_case_insensitive_but_www_is_not() {
    let t = theme();
    assert_eq!(
        render_one("HTTPS://A.COM"),
        styled_link(&t, "HTTPS://A.COM")
    );
    assert_eq!(
        render_one("Ftp://Files.Example.Com"),
        styled_link(&t, "Ftp://Files.Example.Com")
    );
    // `WWW.` is a literal in the regex — no link.
    assert_eq!(render_one("WWW.X.COM"), "WWW.X.COM");
}

#[test]
fn url_trailing_punctuation_is_trimmed() {
    let t = theme();
    // `).` — the `_backpedal` fixpoint peels one trailing segment per pass;
    // the trimmed punctuation stays as plain text after the link.
    assert_eq!(
        render_one("see https://example.com/path)."),
        format!("see {}).", styled_link(&t, "https://example.com/path"))
    );
    assert_eq!(
        render_one("trailing colon https://example.com:"),
        format!("trailing colon {}:", styled_link(&t, "https://example.com"))
    );
    assert_eq!(
        render_one("trailing semi https://example.com;"),
        format!("trailing semi {};", styled_link(&t, "https://example.com"))
    );
    // Quotes are not in the trim set — the trailing `"` stays outside.
    assert_eq!(
        render_one("quote \"https://x.com\" end"),
        format!("quote \"{}\" end", styled_link(&t, "https://x.com"))
    );
}

#[test]
fn url_balanced_parens_survive_unbalanced_trimmed() {
    let t = theme();
    assert_eq!(
        render_one("wiki https://en.wikipedia.org/wiki/Foo_(bar) here"),
        format!(
            "wiki {} here",
            styled_link(&t, "https://en.wikipedia.org/wiki/Foo_(bar)")
        )
    );
    assert_eq!(
        render_one("paren https://example.com/a(b) end"),
        format!("paren {} end", styled_link(&t, "https://example.com/a(b)"))
    );
    // Unbalanced trailing `)` is trimmed (and stays as plain text); an
    // unclosed `(` ends the match.
    assert_eq!(
        render_one("close https://x.com/a) end"),
        format!("close {}) end", styled_link(&t, "https://x.com/a"))
    );
    assert_eq!(
        render_one("open https://x.com/a(b end"),
        format!("open {}(b end", styled_link(&t, "https://x.com/a"))
    );
}

#[test]
fn url_internal_punctuation_and_entities() {
    let t = theme();
    assert_eq!(
        render_one("amp https://x.com/?a&b end"),
        format!("amp {} end", styled_link(&t, "https://x.com/?a&b"))
    );
    // `&b;` at the end looks like an entity reference: trimmed from `&`
    // (the trimmed run stays as plain text).
    assert_eq!(
        render_one("entity https://x.com/?a&b; end"),
        format!("entity {}&b; end", styled_link(&t, "https://x.com/?a"))
    );
}

#[test]
fn url_needs_a_host_char_and_stops_at_whitespace_or_angle() {
    assert_eq!(render_one("http:// foo"), "http:// foo");
    assert_eq!(render_one("http://.a"), "http://.a");
    // Inside the URL a `<` stops the match (marked's `[^\s<]*`).
    assert_eq!(
        render_one("x https://x.com/a<b end"),
        format!("x {}<b end", styled_link(&t_default(), "https://x.com/a"))
    );
}

fn t_default() -> MarkdownTheme {
    theme()
}

#[test]
fn extended_autolinks_fire_midword_like_marked() {
    let t = theme();
    // marked's `text` rule breaks before `scheme://` / `www.` anywhere; the
    // preceding word stays plain text.
    assert_eq!(
        render_one("foowww.x.com"),
        format!("foo{}", fallback_link(&t, "www.x.com", "http://www.x.com"))
    );
    assert_eq!(
        render_one("xhttps://a.com"),
        format!("x{}", styled_link(&t, "https://a.com"))
    );
}

#[test]
fn extended_autolinks_inside_emphasis_and_after_spans() {
    let t = theme();
    assert_eq!(
        render_one("**go www.in-bold.com**"),
        (t.bold)(&format!(
            "go {}",
            fallback_link(&t, "www.in-bold.com", "http://www.in-bold.com")
        ))
        .to_string()
    );
    // After a codespan and after an angle autolink the tokenizer fires again.
    assert_eq!(
        render_one("`code` www.after.com"),
        format!(
            "{} {}",
            (t.code)("code"),
            fallback_link(&t, "www.after.com", "http://www.after.com")
        )
    );
    assert_eq!(
        render_one("<a@b.com> www.y.com"),
        format!(
            "{} {}",
            styled_link(&t, "a@b.com"),
            fallback_link(&t, "www.y.com", "http://www.y.com")
        )
    );
}

// ---------------------------------------------------------------------------
// Extended autolinks: bare emails
// ---------------------------------------------------------------------------

#[test]
fn bare_email_links_as_mailto_without_fallback() {
    let t = theme();
    // text == href modulo `mailto:` → styled text only, trailing `.` stays.
    assert_eq!(
        render_one("mail user@example.com."),
        format!("mail {}.", styled_link(&t, "user@example.com"))
    );
}

#[test]
fn email_local_part_allows_dots_underscores_and_multilabel_domains() {
    let t = theme();
    assert_eq!(
        render_one("user_name@example.co.uk"),
        styled_link(&t, "user_name@example.co.uk")
    );
    assert_eq!(
        render_one("mid.dots@example.com"),
        styled_link(&t, "mid.dots@example.com")
    );
    assert_eq!(render_one("-a@b.com"), styled_link(&t, "-a@b.com"));
}

#[test]
fn email_domain_backtracking_matches_the_regex() {
    let t = theme();
    // `(?![-_])` forces the last label short by one char (the regex then
    // ends on the earlier alphanumeric).
    assert_eq!(
        render_one("x@example.com-"),
        format!("{}m-", styled_link(&t, "x@example.co"))
    );
    assert_eq!(
        render_one("x@example.com_"),
        format!("{}m_", styled_link(&t, "x@example.co"))
    );
    // A hyphen inside the final label is consumable, so the full match wins.
    assert_eq!(
        render_one("x@sub.example.com-x"),
        styled_link(&t, "x@sub.example.com-x")
    );
}

#[test]
fn email_needs_dotted_domain() {
    assert_eq!(render_one("no a@b link"), "no a@b link");
    assert_eq!(render_one("a@b..com"), "a@b..com");
    assert_eq!(render_one("a@.com"), "a@.com");
    assert_eq!(render_one("httpx or foo@bar"), "httpx or foo@bar");
}

#[test]
fn leading_underscore_email_keeps_the_underscore() {
    let t = theme();
    // `_` opens no emphasis here; it is part of the email local part.
    assert_eq!(
        render_one("_user@example.com"),
        styled_link(&t, "_user@example.com")
    );
}

// ---------------------------------------------------------------------------
// No extended autolinks inside other link text
// ---------------------------------------------------------------------------

#[test]
fn no_extended_autolink_inside_link_text() {
    let t = theme();
    // `!inLink` gate: the outer link renders with its literal text and the
    // href fallback; no nested link tokens.
    assert_eq!(
        render_one("[www.x.com](https://y.com)"),
        fallback_link(&t, "www.x.com", "https://y.com")
    );
    assert_eq!(
        render_one("[mail me](x)"),
        fallback_link(&t, "mail me", "x")
    );
}

// ---------------------------------------------------------------------------
// Table-cell wrap style restoration (upstream wrapCellText, markdown.ts:839-846)
// ---------------------------------------------------------------------------

/// The reset sequence `wrapCellText` appends to every non-final fragment.
const CELL_STYLE_RESET: &str = "\x1b[22;23;24;25;27;28;29;39m";

#[test]
fn wrapped_plain_cell_fragments_end_with_the_style_reset() {
    // Column ends up 8 wide (`alpha beta` is 10), so the cell wraps into
    // `alpha` / `beta` fragments. A plain cell has no active style, so the
    // only addition is the reset on the non-final fragment.
    let lines = render("| col |\n| --- |\n| alpha beta |", 12);
    let data0 = &lines[3];
    let data1 = &lines[4];
    assert_eq!(
        *data0,
        format!("│ alpha{CELL_STYLE_RESET}{} │", " ".repeat(3))
    );
    // Final fragment: no reset appended.
    assert_eq!(*data1, format!("│ beta{} │", " ".repeat(4)));
}

#[test]
fn wrapped_styled_span_in_cell_keeps_the_style_on_continuation() {
    let t = theme();
    // `**alpha beta**` wrapped at column width 8: the bold closer lands on
    // the final fragment, so the non-final fragment ends with the dangling
    // opener — which the `wrapCellText` reset closes.
    let lines = render("| col |\n| --- |\n| **alpha beta** |", 12);
    assert_eq!(
        lines[3],
        format!("│ \x1b[1malpha{CELL_STYLE_RESET}{} │", " ".repeat(3))
    );
    assert_eq!(
        lines[4],
        format!("│ {}{} │", (t.bold)("beta"), " ".repeat(4))
    );
}

#[test]
fn wrapped_cell_in_blockquote_restores_the_quote_prefix() {
    let t = theme();
    // The blockquote's inline style prefix (upstream `stylePrefix`) is
    // re-appended after every non-final wrapped fragment. The prefix is
    // probed with a sentinel through the quote > italic chain, like
    // upstream `getStylePrefix` (markdown.ts:567-575).
    let sentinel = '\u{0}';
    let probe = (t.quote)(&(t.italic)(&sentinel.to_string()));
    let prefix: String = probe.chars().take_while(|&c| c != sentinel).collect();
    let lines = render("> | col |\n> | --- |\n> | **alpha beta** |", 14);
    let fragment = format!("\x1b[1malpha{CELL_STYLE_RESET}{prefix}");
    assert!(
        lines.iter().any(|line| line.contains(&fragment)),
        "expected a wrapped fragment ending with reset + quote prefix; got {lines:?}"
    );
}
