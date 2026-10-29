//! Markdown renderer parity tests for the 2026-10-10 feature-alignment fixes
//! (upstream `packages/tui/src/components/markdown.ts`):
//! - GFM task-list markers folded into the styled list marker
//!   (markdown.ts:782-786);
//! - multi-backtick code spans (marked's codespan rule);
//! - `<…>` autolinks (markdown.ts:699-718);
//! - CommonMark intraword emphasis suppression;
//! - inline style-prefix restoration after nested spans
//!   (markdown.ts:487-490, 569-575, 685-748);
//! - link titles excluded from the href fallback;
//! - the OSC 8 hyperlink enabled path (`set_global_hyperlinks(Some(true))`):
//!   the `hyperlink()` run shape, its targets for links/autolinks/titles, the
//!   unchanged visible width, and the click detector over the rendered line —
//!   with the process-global gate serialised on the binary-wide theme lock
//!   (see the section at the bottom);
//! - the OSC 8 wrap rule (`wrapTextWithAnsi`): a run that spans a line break
//!   is closed before the break and re-opened on the continuation line with
//!   the same URL/params/terminator (utils.ts:713-768), so the continuation
//!   cells stay clickable.

use std::sync::MutexGuard;

use pir::tui::altscreen_search::{osc8_link_at_column, splice_columns};
use pir::tui::ansi::{
    pad_to_width, strip_ansi, truncate_to_width, visible_width, wrap_text_with_ansi,
};
use pir::tui::core::Component;
use pir::tui::markdown::{Markdown, MarkdownTheme};

/// The `g_tui`-binary-wide theme lock (`super::tui_system_theme::lock()`, the
/// convention HANDOFF §5 pins): the OSC 8 gate is process-global, so every
/// render that pins it takes this lock too. An enabled-path render can then
/// never land inside a sibling suite's fallback render (or vice versa), and
/// the gate is always pinned under the lock by whichever test renders next.
fn lock() -> MutexGuard<'static, ()> {
    super::tui_system_theme::lock()
}

/// The render core: no padding, wide width, right-side width padding trimmed.
/// The caller pins the OSC 8 gate (under [`lock`]) before calling.
fn render_unpinned(text: &str, width: usize) -> Vec<String> {
    Markdown::new(text, 0, 0)
        .render(width)
        .into_iter()
        .map(|line| line.trim_end().to_string())
        .collect()
}

/// Render a single-line construct to its first content line (gate pinned by
/// the caller).
fn render_one_unpinned(text: &str) -> String {
    render_unpinned(text, 400)
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// Render with the OSC 8 gate pinned to `hyperlinks`; the gate is restored to
/// the `Some(false)` the sibling suites pin before the lock is released, so a
/// `Some(true)` window can never outlive the call.
fn render_pinned(text: &str, width: usize, hyperlinks: bool) -> Vec<String> {
    let _guard = lock();
    pir::tui::markdown::set_global_hyperlinks(Some(hyperlinks));
    let lines = render_unpinned(text, width);
    pir::tui::markdown::set_global_hyperlinks(Some(false));
    lines
}

/// Render with the gate `Some(false)`: the URL-parentheses fallback shape
/// every link assertion in this suite pins — upstream's own Links tests set
/// `hyperlinks: false` for exactly these cases (`markdown.test.ts:1620-1642`).
/// The enabled path (`Some(true)`) has its own section at the bottom of this
/// file; the lock keeps a sibling suite's enabled window from landing inside
/// this render.
fn render(text: &str, width: usize) -> Vec<String> {
    render_pinned(text, width, false)
}

/// Render a single-line construct to its first content line.
fn render_one(text: &str) -> String {
    render(text, 400).into_iter().next().unwrap_or_default()
}

fn theme() -> MarkdownTheme {
    MarkdownTheme::default()
}

/// A scoped `Some(true)` pin for the enabled-path tests: takes the
/// binary-wide lock, enables the gate, and restores the sibling suites'
/// `Some(false)` on drop (panic included). The lock stays held for the scope,
/// so the test's expected values — built from the live theme — observe the
/// same process state as its renders. [`Drop`] restores the gate unconditionally,
/// so no `Some(true)` can leak even when an assertion fails.
struct HyperlinksEnabled {
    _guard: MutexGuard<'static, ()>,
}

impl HyperlinksEnabled {
    fn enter() -> Self {
        let guard = lock();
        pir::tui::markdown::set_global_hyperlinks(Some(true));
        Self { _guard: guard }
    }
}

impl Drop for HyperlinksEnabled {
    fn drop(&mut self) {
        pir::tui::markdown::set_global_hyperlinks(Some(false));
    }
}

// ---------------------------------------------------------------------------
// GFM task lists
// ---------------------------------------------------------------------------

#[test]
fn task_list_markers_fold_into_the_styled_bullet() {
    let t = theme();
    let lines = render("- [x] done\n- [ ] todo", 100);
    assert_eq!(lines[0], format!("{}done", (t.list_bullet)("- [x] ")));
    assert_eq!(lines[1], format!("{}todo", (t.list_bullet)("- [ ] ")));
}

#[test]
fn task_marker_requires_trailing_whitespace() {
    let t = theme();
    // GFM: `[x]` without a following space is plain text.
    let lines = render("- [x]done", 100);
    assert_eq!(lines[0], format!("{}[x]done", (t.list_bullet)("- ")));
}

#[test]
fn task_list_continuation_prefix_matches_marker_width() {
    // The continuation prefix must be the full marker width (`- [x] ` = 6),
    // markdown.ts:785.
    let lines = render("- [x] aaaa bbbb cccc dddd", 20);
    assert_eq!(
        lines[0],
        format!("{}aaaa bbbb cccc", (theme().list_bullet)("- [x] "))
    );
    assert_eq!(lines[1], "      dddd");
}

#[test]
fn ordered_task_list_uses_the_ordered_marker() {
    let t = theme();
    let lines = render("1. [x] one\n2. [ ] two", 100);
    assert_eq!(lines[0], format!("{}one", (t.list_bullet)("1. [x] ")));
    assert_eq!(lines[1], format!("{}two", (t.list_bullet)("2. [ ] ")));
}

// ---------------------------------------------------------------------------
// Multi-backtick code spans
// ---------------------------------------------------------------------------

#[test]
fn multi_backtick_code_span_over_a_single_backtick() {
    let t = theme();
    assert_eq!(render_one("``a`b``"), (t.code)("a`b"));
}

#[test]
fn single_backtick_code_span_unchanged() {
    let t = theme();
    assert_eq!(render_one("`a`"), (t.code)("a"));
}

#[test]
fn code_span_strips_one_space_on_both_ends() {
    let t = theme();
    assert_eq!(render_one("` x `"), (t.code)("x"));
    // All-space content keeps its spaces.
    assert_eq!(render_one("`   `"), (t.code)("   "));
}

#[test]
fn unmatched_backtick_run_stays_literal() {
    assert_eq!(render_one("a ``b"), "a ``b");
}

// ---------------------------------------------------------------------------
// Autolinks
// ---------------------------------------------------------------------------

#[test]
fn uri_autolink_renders_as_a_styled_link() {
    let t = theme();
    assert_eq!(
        render_one("<https://example.com>"),
        (t.link)(&(t.underline)("https://example.com"))
    );
}

#[test]
fn email_autolink_renders_as_a_styled_link() {
    let t = theme();
    assert_eq!(
        render_one("<foo@bar.com>"),
        (t.link)(&(t.underline)("foo@bar.com"))
    );
}

#[test]
fn autolink_rejects_spaces_and_schemeless() {
    assert_eq!(render_one("<foo bar>"), "<foo bar>");
    assert_eq!(render_one("<foobar>"), "<foobar>");
    // A second `<` restarts the autolink scan, exactly like marked's inline
    // tokenizer: the literal `<a` prefix stays, the `<b@c.com>` autolinks.
    let t = theme();
    assert_eq!(
        render_one("<a<b@c.com>"),
        format!("<a{}", (t.link)(&(t.underline)("b@c.com")))
    );
}

// ---------------------------------------------------------------------------
// Intraword emphasis
// ---------------------------------------------------------------------------

#[test]
fn intraword_underscores_stay_literal() {
    assert_eq!(render_one("foo_bar_baz"), "foo_bar_baz");
    assert_eq!(render_one("foo_bar_"), "foo_bar_");
    assert_eq!(render_one("a_b"), "a_b");
}

#[test]
fn intraword_asterisks_still_emphasize() {
    let t = theme();
    assert_eq!(
        render_one("foo*bar*baz"),
        format!("foo{}baz", (t.italic)("bar"))
    );
}

#[test]
fn flanking_rules_keep_word_boundary_emphasis() {
    let t = theme();
    assert_eq!(render_one("_foo_"), (t.italic)("foo"));
    assert_eq!(
        render_one("do _em_ now"),
        format!("do {} now", (t.italic)("em"))
    );
    // A `_` closed against punctuation still works (literal here: the `_`
    // is intraword on both ends).
    assert_eq!(render_one("value = (foo_bar)"), "value = (foo_bar)");
}

// ---------------------------------------------------------------------------
// Style-prefix restoration
// ---------------------------------------------------------------------------

#[test]
fn codespan_inside_heading_restores_the_heading_style() {
    let t = theme();
    let (heading, bold, code) = (t.heading, t.bold, t.code);
    let apply = |s: &str| heading(&bold(s));
    // The heading context prefix (markdown.ts:487-490): everything the
    // wrapper chain emits before the sentinel text.
    let probe = apply("\u{0}");
    let prefix = &probe[..probe.find('\u{0}').expect("sentinel survives styling")];
    let inline = format!(
        "{}{}{}{}",
        apply("Head "),
        code("code"),
        prefix,
        apply(" tail")
    );
    assert_eq!(render_one("## Head `code` tail"), heading(&bold(&inline)));
}

#[test]
fn codespan_inside_blockquote_restores_the_quote_style() {
    let t = theme();
    let (quote, italic, code, quote_border) = (t.quote, t.italic, t.code, t.quote_border);
    // The quote style prefix (markdown.ts:567-575).
    let probe = quote(&italic("\u{0}"));
    let prefix = &probe[..probe.find('\u{0}').expect("sentinel survives styling")];
    let inner = format!("before {}{} after2", code("c"), prefix);
    assert_eq!(
        render_one("> before `c` after2"),
        format!("{}{}", quote_border("│ "), quote(&italic(&inner)))
    );
}

#[test]
fn heading_level_three_content_keeps_the_heading_style() {
    let t = theme();
    let (heading, bold) = (t.heading, t.bold);
    // Upstream renders h3+ content under the heading style context too
    // (markdown.ts:492-494).
    assert_eq!(
        render_one("### Tit"),
        format!("{}{}", heading(&bold("### ")), heading(&bold("Tit")))
    );
}

// ---------------------------------------------------------------------------
// Link titles
// ---------------------------------------------------------------------------

#[test]
fn link_title_is_excluded_from_the_url_fallback() {
    let t = theme();
    let expected = format!(
        "{} ({})",
        (t.link)(&(t.underline)("t")),
        (t.link_url)("https://x.io")
    );
    assert_eq!(render_one("[t](https://x.io \"Title\")"), expected);
}

#[test]
fn link_with_angle_destination_and_title() {
    let t = theme();
    let expected = format!(
        "{} ({})",
        (t.link)(&(t.underline)("t")),
        (t.link_url)("https://x.io")
    );
    assert_eq!(render_one("[t](<https://x.io> \"T\")"), expected);
}

#[test]
fn link_with_balanced_paren_destination_still_parses() {
    let t = theme();
    let expected = format!(
        "{} ({})",
        (t.link)(&(t.underline)("t")),
        (t.link_url)("/wiki/(a)")
    );
    assert_eq!(render_one("[t](/wiki/(a))"), expected);
}

#[test]
fn bare_link_without_title_unchanged() {
    let t = theme();
    let expected = format!(
        "{} ({})",
        (t.link)(&(t.underline)("t")),
        (t.link_url)("/foo")
    );
    assert_eq!(render_one("[t](/foo)"), expected);
}

// ---------------------------------------------------------------------------
// Regression: base emphasis shapes still render
// ---------------------------------------------------------------------------

#[test]
fn base_inline_shapes_unchanged() {
    let t = theme();
    assert_eq!(render_one("**bold**"), (t.bold)("bold"));
    assert_eq!(render_one("~~gone~~"), (t.strikethrough)("gone"));
    assert_eq!(render_one("***both***"), (t.italic)(&(t.bold)("both")));
}

// ---------------------------------------------------------------------------
// OSC 8 hyperlinks: the enabled path
// ---------------------------------------------------------------------------
// Upstream `case "link"` (markdown.ts:699-718) emits a terminal hyperlink
// whenever `getCapabilities().hyperlinks` is set: the styled link text is
// wrapped by `hyperlink()` (terminal-image.ts:732-734) —
// `ESC ] 8 ; ; <url> ESC \` + text + `ESC ] 8 ; ; ESC \` — with no params and
// the ST (`ESC \`) terminator, and the ` (url)` fallback disappears. pir pins
// the flag through `set_global_hyperlinks(Some(true))`, the port of upstream's
// capability override (`setCapabilityOverrides`, terminal-image.ts:187-197).
//
// Every test below runs under [`HyperlinksEnabled`], which holds the
// `g_tui`-wide lock for the whole test and restores `Some(false)` on drop; the
// render helpers above pin `Some(false)` under the same lock. The gate is
// therefore never observed un-pinned by any render in the binary, and a
// `Some(true)` window cannot outlive its test (panic included).

/// The exact run upstream `hyperlink()` builds (terminal-image.ts:732-734):
/// no parameters, `ESC \` (ST) terminator — never BEL.
fn osc8(text: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// The styled link text the run wraps: `theme.link(theme.underline(text))`
/// (upstream `styledLink`, markdown.ts:703).
fn styled_link(t: &MarkdownTheme, text: &str) -> String {
    (t.link)(&(t.underline)(text))
}

#[test]
fn osc8_enabled_link_is_one_run_around_the_styled_text() {
    let _enabled = HyperlinksEnabled::enter();
    let t = theme();
    let line = render_one_unpinned("[click here](https://example.com)");
    assert_eq!(
        line,
        osc8(&styled_link(&t, "click here"), "https://example.com")
    );
    // Exactly one run: the open and the close, both the bare `8;;` form.
    assert_eq!(line.matches("\x1b]8;;").count(), 2);
    assert!(!line.contains('\x07'), "ST terminator, never BEL");
    // The URL rides the escape run only; the visible text is the link text.
    assert_eq!(strip_ansi(&line), "click here");
    assert!(!line.contains("(https://example.com)"));
}

#[test]
fn osc8_enabled_autolink_targets_follow_marked() {
    let _enabled = HyperlinksEnabled::enter();
    let t = theme();
    let uri = styled_link(&t, "https://example.com");
    // CommonMark URI autolink: text == href (marked `autolink`).
    assert_eq!(
        render_one_unpinned("<https://example.com>"),
        osc8(&uri, "https://example.com")
    );
    // GFM extended autolinks (marked `url`): scheme URLs keep their href...
    assert_eq!(
        render_one_unpinned("Visit https://example.com now"),
        format!("Visit {} now", osc8(&uri, "https://example.com"))
    );
    // ...`www.` gains the `http://` prefix...
    assert_eq!(
        render_one_unpinned("at www.example.com today"),
        format!(
            "at {} today",
            osc8(
                &styled_link(&t, "www.example.com"),
                "http://www.example.com"
            )
        )
    );
    // ...and emails gain `mailto:` — for the extended form...
    assert_eq!(
        render_one_unpinned("mail foo@bar.com please"),
        format!(
            "mail {} please",
            osc8(&styled_link(&t, "foo@bar.com"), "mailto:foo@bar.com")
        )
    );
    // ...and for marked's `<email>` autolink branch as well.
    assert_eq!(
        render_one_unpinned("<foo@bar.com>"),
        osc8(&styled_link(&t, "foo@bar.com"), "mailto:foo@bar.com")
    );
    // An explicit `mailto:` destination is already the href.
    assert_eq!(
        render_one_unpinned("[Email me](mailto:test@example.com)"),
        osc8(&styled_link(&t, "Email me"), "mailto:test@example.com")
    );
}

#[test]
fn osc8_enabled_link_title_stays_out_of_the_run() {
    let _enabled = HyperlinksEnabled::enter();
    let t = theme();
    // `token.href` never carries the title (marked strips it into
    // `token.title`), so the run's URL is the destination alone and the title
    // is not visible text.
    let line = render_one_unpinned("[t](https://x.io \"Title\")");
    assert_eq!(line, osc8(&styled_link(&t, "t"), "https://x.io"));
    assert!(!line.contains("Title"));
}

#[test]
fn osc8_enabled_visible_width_is_unchanged() {
    // Every link in this document reads as its own href, so the disabled
    // shape prints no `(url)` either: the two renders must carry the same
    // visible text in the same cells, one wrapped in OSC 8 runs and one not.
    // The tail forces the paragraph to wrap, so the zero-width runs must leave
    // the wrap points, the visible text and the padding untouched.
    let doc = "see [https://example.com/a](https://example.com/a) and \
               <https://example.org/b> plus <foo@bar.com> for details, with a \
               long enough tail that the paragraph wraps\n";
    let enabled = {
        let _enabled = HyperlinksEnabled::enter();
        render_unpinned(doc, 40)
    };
    let disabled = render(doc, 40);
    assert!(enabled.iter().any(|line| line.contains("\x1b]8;;")));
    assert!(disabled.iter().all(|line| !line.contains("\x1b]8;;")));
    assert_eq!(enabled.len(), disabled.len());
    for (with, without) in enabled.iter().zip(&disabled) {
        assert_eq!(visible_width(with), visible_width(without), "line {with:?}");
        assert_eq!(strip_ansi(with), strip_ansi(without), "line {with:?}");
    }
    let enabled_bytes: usize = enabled.iter().map(String::len).sum();
    let disabled_bytes: usize = disabled.iter().map(String::len).sum();
    assert!(
        enabled_bytes > disabled_bytes,
        "the runs add bytes, not cells"
    );
}

#[test]
fn osc8_enabled_click_detector_covers_the_link_cells() {
    let _enabled = HyperlinksEnabled::enter();
    let t = theme();
    let line = render_one_unpinned("[click here](https://example.com) then plain");
    assert_eq!(
        line,
        format!(
            "{} then plain",
            osc8(&styled_link(&t, "click here"), "https://example.com")
        )
    );
    // Every cell of the visible link text resolves to the target...
    for column in 0.."click here".len() {
        assert_eq!(
            osc8_link_at_column(&line, column).as_deref(),
            Some("https://example.com"),
            "column {column}"
        );
    }
    // ...no cell outside it does, and past the line's cells there is nothing.
    for column in "click here".len()..visible_width(&line) {
        assert_eq!(osc8_link_at_column(&line, column), None, "column {column}");
    }
    assert_eq!(osc8_link_at_column(&line, 1000), None);

    // The frame line carries the component's left padding: the detector's
    // columns are screen columns, so the run starts at `padding_x`.
    let padded = Markdown::new("[t](https://u)", 2, 0)
        .render(40)
        .into_iter()
        .next()
        .map(|line| line.trim_end().to_string())
        .unwrap_or_default();
    assert_eq!(osc8_link_at_column(&padded, 0), None);
    assert_eq!(osc8_link_at_column(&padded, 1), None);
    assert_eq!(
        osc8_link_at_column(&padded, 2).as_deref(),
        Some("https://u")
    );
    assert_eq!(osc8_link_at_column(&padded, 3), None);
}

#[test]
fn osc8_enabled_two_links_resolve_independently() {
    let _enabled = HyperlinksEnabled::enter();
    let t = theme();
    let line = render_one_unpinned("[one](https://one.io) mid [two!!](https://two.io)");
    assert_eq!(
        line,
        format!(
            "{} mid {}",
            osc8(&styled_link(&t, "one"), "https://one.io"),
            osc8(&styled_link(&t, "two!!"), "https://two.io")
        )
    );
    // `one` = columns 0..3, ` mid ` = 3..8, `two!!` = 8..13.
    for column in 0..3 {
        assert_eq!(
            osc8_link_at_column(&line, column).as_deref(),
            Some("https://one.io"),
            "column {column}"
        );
    }
    for column in 3..8 {
        assert_eq!(osc8_link_at_column(&line, column), None, "column {column}");
    }
    for column in 8..13 {
        assert_eq!(
            osc8_link_at_column(&line, column).as_deref(),
            Some("https://two.io"),
            "column {column}"
        );
    }
    assert_eq!(osc8_link_at_column(&line, 13), None);
}

#[test]
fn osc8_enabled_styled_link_keeps_sgr_and_the_run() {
    let _enabled = HyperlinksEnabled::enter();
    let t = theme();
    let run = osc8(&styled_link(&t, "t"), "https://x.io");
    // Bold wraps the whole run (`case "strong"` renders its inline tokens,
    // then wraps the result), exactly like upstream.
    assert_eq!(render_one_unpinned("**[t](https://x.io)**"), (t.bold)(&run));
    // A level-2 heading keeps its wrapper chain around the run, and the link
    // label itself is rendered under the heading's style context — upstream
    // renders `theme.link(theme.underline(linkText))` with
    // `linkText` produced by the context's `applyText`, here
    // `heading(bold("t"))` (markdown.ts:703 via :487-490). So the run carries
    // the heading+bold SGR *inside* the OSC 8 run, and the block fold wraps
    // the whole run again (`codespan_inside_heading` pins that fold).
    let heading = render_one_unpinned("## [t](https://x.io)");
    let heading_run = osc8(
        &(t.link)(&(t.underline)(&(t.heading)(&(t.bold)("t")))),
        "https://x.io",
    );
    assert_eq!(
        heading,
        (t.heading)(&(t.bold)(&heading_run)),
        "the heading+bold SGR and the OSC 8 run both survive"
    );
    let probe = (t.heading)(&(t.bold)("\u{0}"));
    let open = &probe[..probe.find('\u{0}').expect("sentinel survives styling")];
    assert!(heading.contains(open), "the heading+bold SGR opens first");
    assert!(heading.contains("\x1b]8;;https://x.io\x1b\\"));
    assert!(heading.contains("\x1b]8;;\x1b\\"));
    assert_eq!(
        osc8_link_at_column(&heading, 0).as_deref(),
        Some("https://x.io")
    );
}

#[test]
fn osc8_disabled_gate_renders_the_fallback_without_runs() {
    let t = theme();
    let line = render_one("[click here](https://example.com)");
    assert_eq!(
        line,
        format!(
            "{} ({})",
            (t.link)(&(t.underline)("click here")),
            (t.link_url)("https://example.com")
        )
    );
    assert_eq!(strip_ansi(&line), "click here (https://example.com)");
    assert!(
        !line.contains("\x1b]8;"),
        "no OSC 8 sequence when the gate is off"
    );
}

#[test]
fn osc8_enabled_scope_restores_the_disabled_gate() {
    {
        let _enabled = HyperlinksEnabled::enter();
        assert!(pir::tui::markdown::global_hyperlinks());
    }
    // The scope dropped: the gate is back to the `Some(false)` every render
    // helper in the binary pins, under the lock the helpers take.
    let _guard = lock();
    assert!(!pir::tui::markdown::global_hyperlinks());
}

// ---------------------------------------------------------------------------
// OSC 8 hyperlinks: the wrap rule (`wrapTextWithAnsi`)
// ---------------------------------------------------------------------------
// Upstream closes an active run before every wrap-inserted line break and
// re-opens it at the start of the continuation line, with the run's own
// parameters and terminator (utils.ts:713-731 `getActiveCodes`,
// :759-768 `getLineEndReset`; pinned by `wrap-ansi.test.ts:203-245`). Without
// the rule a long link's visible text wraps but the OSC 8 sequences do not:
// only the first physical line is a link and `osc8_link_at_column` answers
// `None` on every continuation cell.

/// The open sequence of a markdown-emitted run (ST terminator, no params) —
/// the same bytes [`osc8`] emits.
fn osc8_open(url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\")
}

/// The bare close form, independent of the run's terminator.
fn osc8_close() -> &'static str {
    "\x1b]8;;\x1b\\"
}

#[test]
fn osc8_wrap_reopens_the_run_on_every_continuation_line() {
    let url = "https://example.com";
    // The upstream fixture (`wrap-ansi.test.ts:204-210`): a 10-cell run
    // wrapped at 6.
    let lines = wrap_text_with_ansi(&osc8("0123456789", url), 6);
    assert_eq!(
        lines,
        vec![
            format!("{}012345{}", osc8_open(url), osc8_close()),
            format!("{}6789{}", osc8_open(url), osc8_close()),
        ]
    );
    for line in &lines {
        assert_eq!(
            osc8_link_at_column(line, 0).as_deref(),
            Some(url),
            "every line starts inside a re-opened run: {line:?}"
        );
        assert_eq!(
            line.matches("\x1b]8;;").count(),
            2,
            "one open + one close: {line:?}"
        );
    }
    // Wrap parity: the runs add bytes, never cells — the same text without a
    // run breaks at the same cells.
    let plain = wrap_text_with_ansi("0123456789", 6);
    assert_eq!(plain.len(), lines.len());
    for (with, without) in lines.iter().zip(&plain) {
        assert_eq!(strip_ansi(with), *without);
    }
}

#[test]
fn osc8_wrap_a_link_ending_at_the_boundary_closes_once() {
    // The close sits exactly at the wrap point: the wrapped line must not
    // gain a second, wrap-inserted close, and the next line must not re-open
    // anything.
    let lines = wrap_text_with_ansi(&format!("{}efgh", osc8("abcd", "u")), 4);
    assert_eq!(lines, vec![osc8("abcd", "u"), "efgh".to_string()]);
    assert_eq!(lines[0].matches("\x1b]8;;").count(), 2, "{:?}", lines[0]);
    assert_eq!(osc8_link_at_column(&lines[1], 0), None);
}

#[test]
fn osc8_wrap_preserves_the_terminator_and_the_parameters() {
    // BEL is the login-dialog shape (`login-dialog.ts:125`): some terminals
    // only make BEL-terminated runs clickable, so a BEL run must be re-opened
    // with BEL (upstream `utils.ts:556-568`), never with ST.
    let bel = wrap_text_with_ansi("\x1b]8;;u\x07abcd\x1b]8;;\x07", 2);
    assert_eq!(
        bel,
        vec![
            "\x1b]8;;u\x07ab\x1b]8;;\x07".to_string(),
            "\x1b]8;;u\x07cd\x1b]8;;\x07".to_string(),
        ]
    );
    assert!(bel.iter().all(|line| !line.contains("\x1b\\")), "{bel:?}");
    // Parameters ride along on the reopen; the close stays the bare `8;;`
    // form (upstream `formatOsc8Close`).
    let params = wrap_text_with_ansi("\x1b]8;id=42;https://x\x1b\\ab\x1b]8;;\x1b\\", 1);
    assert_eq!(
        params,
        vec![
            "\x1b]8;id=42;https://x\x1b\\a\x1b]8;;\x1b\\".to_string(),
            "\x1b]8;id=42;https://x\x1b\\b\x1b]8;;\x1b\\".to_string(),
        ]
    );
}

#[test]
fn osc8_wrap_reopens_sgr_and_the_run_together() {
    // Underline + link: the continuation line re-opens the merged SGR first
    // and the run after it, and the line end closes the underline before the
    // run (upstream `getActiveCodes` / `getLineEndReset` ordering).
    let lines = wrap_text_with_ansi("\x1b[4m\x1b]8;;u\x1b\\abcdef\x1b]8;;\x1b\\\x1b[24m", 3);
    assert_eq!(
        lines,
        vec![
            "\x1b[4m\x1b]8;;u\x1b\\abc\x1b[24m\x1b]8;;\x1b\\".to_string(),
            "\x1b[4m\x1b]8;;u\x1b\\def\x1b]8;;\x1b\\\x1b[24m".to_string(),
        ]
    );
    // An SGR reset does not close the run: upstream `reset` leaves
    // `activeHyperlink` alone (`utils.ts:704`), so the wrap after a reset
    // still closes and re-opens.
    let after_reset = wrap_text_with_ansi("\x1b]8;;u\x1b\\\x1b[0mabcdef\x1b]8;;\x1b\\", 3);
    assert_eq!(
        after_reset,
        vec![
            "\x1b]8;;u\x1b\\\x1b[0mabc\x1b]8;;\x1b\\".to_string(),
            "\x1b]8;;u\x1b\\def\x1b]8;;\x1b\\".to_string(),
        ]
    );
}

#[test]
fn osc8_wrap_a_literal_newline_carries_the_still_open_run() {
    // The cross-input-line path: the physical line after a literal newline is
    // prefixed with the run still active at the end of the previous one
    // (upstream `wrapTextWithAnsi`'s `getActiveCodes()` prefix)...
    let lines = wrap_text_with_ansi("\x1b]8;;u\x1b\\ab\ncd\x1b]8;;\x1b\\", 80);
    assert_eq!(
        lines,
        vec![
            "\x1b]8;;u\x1b\\ab".to_string(),
            "\x1b]8;;u\x1b\\cd\x1b]8;;\x1b\\".to_string(),
        ]
    );
    for line in &lines {
        assert_eq!(osc8_link_at_column(line, 1).as_deref(), Some("u"));
    }
    // ...and a run that closed before the newline leaves no prefix behind.
    let closed = wrap_text_with_ansi(&format!("{}\ncd", osc8("ab", "u")), 80);
    assert_eq!(closed, vec![osc8("ab", "u"), "cd".to_string()]);
    assert_eq!(osc8_link_at_column(&closed[1], 0), None);
}

#[test]
fn truncate_mid_link_keeps_the_run_balanced() {
    // Cutting inside the run's text (budget 3 of 6 cells) keeps the open, the
    // clipped text and the close; the ellipsis lands outside the run.
    let out = truncate_to_width(&osc8("abcdef", "u"), 5, "..");
    assert_eq!(out, format!("{}abc{}..", osc8_open("u"), osc8_close()));
    assert_eq!(strip_ansi(&out), "abc..");
    assert_eq!(visible_width(&out), 5);
    for column in 0..3 {
        assert_eq!(
            osc8_link_at_column(&out, column).as_deref(),
            Some("u"),
            "column {column}"
        );
    }
    for column in 3..5 {
        assert_eq!(osc8_link_at_column(&out, column), None, "column {column}");
    }
}

#[test]
fn truncate_of_an_unclosed_link_closes_the_run_before_the_ellipsis() {
    // A line whose OSC 8 run is not closed in the input (a prefix of a link,
    // e.g. a streaming fragment): the clip leaves the run OPEN, and the
    // ellipsis must not become part of it. Upstream `finalizeTruncatedResult`
    // synthesizes `getActiveOsc8Close(prefix)` + reset before the ellipsis;
    // without it a click on the ellipsis (or on padding written after this
    // string) resolves to the link.
    let out = truncate_to_width(&format!("{}abcdef", osc8_open("u")), 5, "..");
    assert_eq!(
        out,
        format!("{}abc{}\x1b[0m..\x1b[0m", osc8_open("u"), osc8_close()),
        "the synthesized close must precede the ellipsis"
    );
    assert_eq!(strip_ansi(&out), "abc..");
    assert_eq!(visible_width(&out), 5);
    for column in 0..3 {
        assert_eq!(
            osc8_link_at_column(&out, column).as_deref(),
            Some("u"),
            "column {column}"
        );
    }
    for column in 3..5 {
        assert_eq!(osc8_link_at_column(&out, column), None, "column {column}");
    }

    // Boundary case: the input closes the run before the cut, so `clipped`
    // already carries the close and no second one is synthesized (the close
    // stays where the input put it, before the ellipsis).
    let closed = format!("{}ab{}{}", osc8_open("u"), osc8_close(), "cdef");
    let out = truncate_to_width(&closed, 4, "..");
    assert_eq!(out, format!("{}ab{}{}", osc8_open("u"), osc8_close(), ".."));
    for column in 0..2 {
        assert_eq!(osc8_link_at_column(&out, column).as_deref(), Some("u"));
    }
    for column in 2..4 {
        assert_eq!(osc8_link_at_column(&out, column), None, "column {column}");
    }
}

#[test]
fn osc8_wrap_width_helpers_count_only_the_visible_cells() {
    // The wrap-inserted runs are zero-width for the other column helpers too:
    // padding is the plain text's padding (with the run inserted), the
    // already-fits truncation path returns the run untouched, and a search
    // paint spliced over a continuation line leaves the run covering every
    // cell it covered before.
    let lines = wrap_text_with_ansi(&osc8("0123456789", "u"), 6);
    assert_eq!(pad_to_width(&lines[0], 8), format!("{}  ", lines[0]));
    assert_eq!(pad_to_width(&lines[1], 8), format!("{}    ", lines[1]));
    assert_eq!(truncate_to_width(&lines[1], 8, "..."), lines[1]);

    let painted = splice_columns(&lines[1], &[(0, 2, "\x1b[7m", "\x1b[27m")]);
    assert_eq!(strip_ansi(&painted), "6789");
    assert_eq!(painted.matches("\x1b]8;;").count(), 2, "{painted:?}");
    for column in 0..4 {
        assert_eq!(
            osc8_link_at_column(&painted, column).as_deref(),
            Some("u"),
            "column {column} of {painted:?}"
        );
    }
}

#[test]
fn osc8_enabled_link_wrapping_inside_its_text_stays_clickable() {
    let _enabled = HyperlinksEnabled::enter();
    let url = "https://example.com";
    // The link text itself wraps (a space inside the run), content width 14:
    // line 0 ends inside the run and is closed by the wrapper, line 1 is the
    // wrapper's re-opened continuation. Both lines are rendered markdown
    // lines, padding trimmed.
    let lines = render_unpinned("[aaaaaaaaaa bbbbbbbbbb](https://example.com)", 14);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(strip_ansi(&lines[0]), "aaaaaaaaaa");
    assert_eq!(strip_ansi(&lines[1]), "bbbbbbbbbb");
    for line in &lines {
        assert!(
            line.contains(&osc8_open(url)),
            "continuation line must re-open the run: {line:?}"
        );
        assert!(line.ends_with(osc8_close()), "{line:?}");
        for column in 0..10 {
            assert_eq!(
                osc8_link_at_column(line, column).as_deref(),
                Some(url),
                "column {column} of {line:?}"
            );
        }
        // Past the last visible cell (padding is trimmed) nothing is linked.
        assert_eq!(osc8_link_at_column(line, 10), None, "{line:?}");
    }

    // The unpadded frame row: the wrap-inserted close precedes the trailing
    // padding, so the padding cells are not part of the run either.
    let padded = Markdown::new("[aaaaaaaaaa bbbbbbbbbb](https://example.com)", 0, 0)
        .render(14)
        .into_iter()
        .nth(1)
        .unwrap_or_default();
    assert_eq!(visible_width(&padded), 14, "{padded:?}");
    assert!(padded.ends_with("    "), "{padded:?}");
    for column in 10..14 {
        assert_eq!(
            osc8_link_at_column(&padded, column),
            None,
            "padding column {column} of {padded:?}"
        );
    }
}

#[test]
fn osc8_enabled_wrapped_autolink_keeps_the_plain_wrap_points() {
    // A bare URL autolink: the visible text is the URL itself, so the
    // disabled render prints no `(url)` and both renders must carry the same
    // visible text in the same cells — the wrap points INSIDE the run only
    // the enabled render adds escapes to.
    let url = "https://example.com/some/very/long/path/that/keeps/going/and/going";
    let enabled = {
        let _enabled = HyperlinksEnabled::enter();
        render_unpinned(&format!("<{url}>"), 30)
    };
    let disabled = render(&format!("<{url}>"), 30);
    assert!(enabled.len() > 1, "{enabled:?}");
    assert_eq!(enabled.len(), disabled.len());
    for (with, without) in enabled.iter().zip(&disabled) {
        assert_eq!(strip_ansi(with), strip_ansi(without), "line {with:?}");
        assert_eq!(visible_width(with), visible_width(without), "line {with:?}");
        // Every cell of every rendered line is inside the re-opened run.
        for column in 0..visible_width(without) {
            assert_eq!(
                osc8_link_at_column(with, column).as_deref(),
                Some(url),
                "line {with:?} column {column}"
            );
        }
        assert_eq!(with.matches("\x1b]8;;").count(), 2, "{with:?}");
    }
}
