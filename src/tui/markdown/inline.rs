use super::*;

/// A markdown theme style function (`fn(&str) -> String`).
pub(super) type StyleFn = fn(&str) -> String;

/// Inline style context (upstream `InlineStyleContext`, markdown.ts:231-234
/// and :457-462): `wrap` is applied to every plain-text segment (the heading
/// style, for example) and `prefix` is re-applied after each nested span so
/// the surrounding style survives the span's own ANSI resets
/// (markdown.ts:685-727).
#[derive(Clone, Copy, Default)]
pub(super) struct InlineStyleContext<'a> {
    /// Wrapper chain, outermost first (applied in reverse per segment).
    wrap: &'a [StyleFn],
    /// ANSI prefix re-emitted after nested spans (upstream `stylePrefix`).
    prefix: &'a str,
}

impl<'a> InlineStyleContext<'a> {
    pub(super) fn new(wrap: &'a [StyleFn], prefix: &'a str) -> Self {
        Self { wrap, prefix }
    }

    /// Upstream `applyText` / `applyTextWithNewlines`: apply the wrapper
    /// chain to every newline-separated segment.
    fn apply(&self, text: &str) -> String {
        if self.wrap.is_empty() {
            return text.to_string();
        }
        text.split('\n')
            .map(|segment| {
                let mut styled = segment.to_string();
                for style in self.wrap.iter().rev() {
                    styled = style(&styled);
                }
                styled
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The ANSI prefix a wrapper chain emits before its text (upstream
/// `getStylePrefix`, markdown.ts:450-455): probe with a NUL sentinel and take
/// everything before it.
pub(super) fn style_prefix(styles: &[StyleFn]) -> String {
    const SENTINEL: char = '\u{0}';
    let mut styled = SENTINEL.to_string();
    for style in styles.iter().rev() {
        styled = style(&styled);
    }
    match styled.find(SENTINEL) {
        Some(index) => styled[..index].to_string(),
        None => String::new(),
    }
}

/// Render inline markdown (`**bold**`, `*em*`, `` `code` ``, `~~del~~`,
/// `[text](url)`, autolinks, `\` escapes, `$…$`/`\(…\)`/`\[…\]` math) into
/// one styled line under `ctx`. The default (unstyled) context is
/// `InlineStyleContext::default()`.
pub(super) fn render_inline_with_context(
    text: &str,
    theme: &MarkdownTheme,
    options: &MarkdownOptions,
    inside_link: bool,
    ctx: &InlineStyleContext<'_>,
) -> String {
    let mut out = String::with_capacity(text.len());
    // Accumulated plain-text run, flushed through the context style when a
    // styled span (or the end of input) is reached.
    let mut plain = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() => {
                // Inline math is attempted before the escape rule: marked
                // tries inline extensions first and `tokenizeInlineLatex`
                // claims `\(`/`\[` (markdown.ts:52-99, :655-663). A trailing
                // `\(`/`\[` is a pending token shown raw.
                if let Some(token) = tokenize_inline_latex(&chars, i) {
                    flush_plain(&mut out, &mut plain, ctx);
                    let rendered = emit_inline_latex(&chars, i, &token, options);
                    out.push_str(&ctx.apply(&rendered));
                    i = token.end;
                } else if options.preserve_backslash_escapes {
                    // Upstream `escape` + `preserveBackslashEscapes`
                    // (markdown.ts:665-667): keep the raw `\x`.
                    plain.push('\\');
                    plain.push(chars[i + 1]);
                    i += 2;
                } else {
                    // Backslash escape: emit the literal next char.
                    plain.push(chars[i + 1]);
                    i += 2;
                }
            }
            '$' => {
                // Inline math (`$…$`, `$$…$$`) per `tokenizeInlineLatex`
                // (markdown.ts:52-99); currency/shell-variable lookalikes are
                // rejected inside the tokenizer and stay literal.
                if let Some(token) = tokenize_inline_latex(&chars, i) {
                    flush_plain(&mut out, &mut plain, ctx);
                    let rendered = emit_inline_latex(&chars, i, &token, options);
                    out.push_str(&ctx.apply(&rendered));
                    i = token.end;
                } else {
                    plain.push('$');
                    i += 1;
                }
            }
            '`' => {
                // Codespan: a run of N backticks closes at the next run of
                // exactly N backticks (marked's codespan rule, CommonMark
                // code spans), so ``` ``a`b`` ``` spans over a single
                // backtick.
                let run = chars[i..].iter().take_while(|&&x| x == '`').count();
                if let Some(close) = find_code_span_close(&chars, i + run, run) {
                    let mut code: String = chars[i + run..close].iter().collect();
                    // marked's codespan: newlines become spaces; one leading
                    // and trailing space is stripped when both sides have
                    // one and the content is not all spaces.
                    code = code.replace('\n', " ");
                    if code.contains(|c: char| c != ' ')
                        && code.starts_with(' ')
                        && code.ends_with(' ')
                    {
                        code = code[1..code.len() - 1].to_string();
                    }
                    flush_plain(&mut out, &mut plain, ctx);
                    out.push_str(&(theme.code)(&code));
                    out.push_str(ctx.prefix);
                    i = close + run;
                } else {
                    plain.push('`');
                    i += 1;
                }
            }
            '*' if chars.get(i + 1) == Some(&'*') => {
                // `***a***` renders as `em > strong` (marked tokenizes the
                // triple into an em around a strong). Only an exact triple on
                // both sides is claimed — longer runs (`****a****`) fall
                // through to the `**` handling below, as before.
                let triple_end = if chars.get(i + 2) == Some(&'*') && chars.get(i + 3) != Some(&'*')
                {
                    find_delimiter(&chars, i + 3, "***")
                        .filter(|&end| chars.get(end + 3) != Some(&'*'))
                } else {
                    None
                };
                if let Some(end) = triple_end {
                    flush_plain(&mut out, &mut plain, ctx);
                    let inner: String = chars[i + 3..end].iter().collect();
                    let rendered =
                        render_inline_with_context(&inner, theme, options, inside_link, ctx);
                    out.push_str(&(theme.italic)(&(theme.bold)(&rendered)));
                    out.push_str(ctx.prefix);
                    i = end + 3;
                } else if let Some(end) = find_delimiter(&chars, i + 2, "**") {
                    flush_plain(&mut out, &mut plain, ctx);
                    let inner: String = chars[i + 2..end].iter().collect();
                    let rendered =
                        render_inline_with_context(&inner, theme, options, inside_link, ctx);
                    out.push_str(&(theme.bold)(&rendered));
                    out.push_str(ctx.prefix);
                    i = end + 2;
                } else {
                    plain.push('*');
                    i += 1;
                }
            }
            '*' | '_' => {
                // Emphasis with CommonMark flanking rules (marked applies
                // them too): `*` opens when left-flanking, `_` additionally
                // refuses intraword runs, so `foo_bar_baz` stays literal.
                if can_open_emphasis(&chars, i, c) {
                    if let Some(end) = find_emphasis_close(&chars, i, c) {
                        flush_plain(&mut out, &mut plain, ctx);
                        let inner: String = chars[i + 1..end].iter().collect();
                        let rendered =
                            render_inline_with_context(&inner, theme, options, inside_link, ctx);
                        out.push_str(&(theme.italic)(&rendered));
                        out.push_str(ctx.prefix);
                        i = end + 1;
                        continue;
                    }
                }
                // A `_` can also open an extended autolink (it is a legal
                // email local-part char), so the fallback goes through the
                // same extended-autolink attempt as any other plain char.
                if try_emit_extended_autolink(
                    &chars,
                    &mut i,
                    &mut out,
                    &mut plain,
                    inside_link,
                    theme,
                    ctx,
                ) {
                    continue;
                }
                plain.push(c);
                i += 1;
            }
            '~' if chars.get(i + 1) == Some(&'~') => {
                if let Some(end) = find_delimiter(&chars, i + 2, "~~") {
                    flush_plain(&mut out, &mut plain, ctx);
                    let inner: String = chars[i + 2..end].iter().collect();
                    let rendered =
                        render_inline_with_context(&inner, theme, options, inside_link, ctx);
                    out.push_str(&(theme.strikethrough)(&rendered));
                    out.push_str(ctx.prefix);
                    i = end + 2;
                } else {
                    plain.push_str("~~");
                    i += 2;
                }
            }
            '<' => {
                // Autolink (marked `autolink` rule, rendered by the link case
                // markdown.ts:699-718): `<https://…>` or `<a@b.c>` — the text
                // equals the href (modulo `mailto:`), so only the styled text
                // is emitted, no ` (url)` fallback.
                if let Some((content, end)) = parse_autolink(&chars, i) {
                    flush_plain(&mut out, &mut plain, ctx);
                    // marked's `autolink` tokenizer: the email branch carries
                    // `href = "mailto:" + text` (`t[2] === "@"` in the v18
                    // tokenizer), the URI branch `href = text`. Only the OSC 8
                    // URL shows the difference: the visible text is the same
                    // and the non-hyperlink fallback compares equal either way
                    // (the `mailto:` prefix is stripped before the compare).
                    let href = if is_email_autolink(&content) {
                        format!("mailto:{content}")
                    } else {
                        content.clone()
                    };
                    emit_styled_link(&mut out, &content, &content, &href, false, theme, ctx);
                    i = end;
                } else {
                    plain.push('<');
                    i += 1;
                }
            }
            '[' => {
                // Link: [text](href) — render text styled; URL in parens
                // when it differs (upstream non-hyperlink fallback,
                // markdown.ts:707-717).
                if let Some((link_text, href, end)) = parse_link(&chars, i) {
                    flush_plain(&mut out, &mut plain, ctx);
                    let rendered_text =
                        render_inline_with_context(&link_text, theme, options, true, ctx);
                    // Autolinked emails carry `text="foo@bar.com"` with
                    // `href="mailto:foo@bar.com"`: compare against the href
                    // with the `mailto:` prefix stripped.
                    emit_styled_link(
                        &mut out,
                        &rendered_text,
                        &link_text,
                        &href,
                        inside_link,
                        theme,
                        ctx,
                    );
                    i = end;
                } else {
                    plain.push('[');
                    i += 1;
                }
            }
            _ => {
                // GFM extended autolinks (marked v18 `url` tokenizer +
                // `rules.inline.url`, gated on `!state.inLink`): bare
                // `www.…` / `http(s|ftp)://…` URLs and bare emails,
                // linkified anywhere in prose — marked's `text` rule breaks
                // before every such position, so the tokenizer is reached at
                // each of them. Rendered like any other link token: styled
                // text, plus ` (url)` when the text differs from the href
                // (www gets `http://` prefixed, emails get `mailto:`).
                if try_emit_extended_autolink(
                    &chars,
                    &mut i,
                    &mut out,
                    &mut plain,
                    inside_link,
                    theme,
                    ctx,
                ) {
                    continue;
                }
                plain.push(c);
                i += 1;
            }
        }
    }
    flush_plain(&mut out, &mut plain, ctx);
    // Upstream trims style prefixes that would sit right before the outer
    // wrapper's own sequence (markdown.ts:746-748).
    while !ctx.prefix.is_empty() && out.ends_with(ctx.prefix) {
        out.truncate(out.len() - ctx.prefix.len());
    }
    out
}

/// Flush a pending plain-text run through the context style.
fn flush_plain(out: &mut String, plain: &mut String, ctx: &InlineStyleContext<'_>) {
    if !plain.is_empty() {
        out.push_str(&ctx.apply(plain));
        plain.clear();
    }
}

/// Attempt a GFM extended autolink at `*i` (see
/// [`parse_extended_autolink`]); on a match, flush the pending plain run,
/// emit the styled link and advance `*i` past it. Returns whether it fired.
/// Never fires inside link text (upstream gates the `url` tokenizer on
/// `!state.inLink`).
fn try_emit_extended_autolink(
    chars: &[char],
    i: &mut usize,
    out: &mut String,
    plain: &mut String,
    inside_link: bool,
    theme: &MarkdownTheme,
    ctx: &InlineStyleContext<'_>,
) -> bool {
    if inside_link {
        return false;
    }
    let Some((text, href)) = parse_extended_autolink(chars, *i) else {
        return false;
    };
    flush_plain(out, plain, ctx);
    emit_styled_link(out, &text, &text, &href, false, theme, ctx);
    *i += text.chars().count();
    true
}

/// Emit a link token's rendering (upstream `case "link"`, markdown.ts:699-718):
/// when the terminal supports OSC 8 hyperlinks the styled link text is wrapped
/// in a hyperlink run ([`osc8_hyperlink`], the URL never printed inline);
/// otherwise the styled link text, followed by ` (url)` in the fallback shape
/// when the visible text differs from the href. `rendered_text` is the
/// already-styled link text, `compare_text` the raw token text the href is
/// compared against (for mailto links the prefix is stripped first).
#[allow(clippy::too_many_arguments)]
fn emit_styled_link(
    out: &mut String,
    rendered_text: &str,
    compare_text: &str,
    href: &str,
    inside_link: bool,
    theme: &MarkdownTheme,
    ctx: &InlineStyleContext<'_>,
) {
    let styled = (theme.link)(&(theme.underline)(rendered_text));
    if global_hyperlinks() && !inside_link {
        // OSC 8 (upstream `hyperlink(styledLink, token.href)`,
        // markdown.ts:702-705): the URL rides the escape run, so only the
        // link text is ever visible. A link inside another link's text stays
        // plain styled text — upstream's lexer never produces a nested link
        // token (`state.inLink`), so the inner markup has no run of its own.
        out.push_str(&osc8_hyperlink(&styled, href));
        out.push_str(ctx.prefix);
        return;
    }
    // Fallback: the styled text alone when the text already reads as the
    // href, else the URL in parentheses. Autolinked emails carry
    // `text="foo@bar.com"` with `href="mailto:foo@bar.com"`: compare against
    // the href with the `mailto:` prefix stripped.
    let href_for_comparison = href.strip_prefix("mailto:").unwrap_or(href);
    if inside_link || compare_text == href || compare_text == href_for_comparison {
        out.push_str(&styled);
    } else {
        // Includes the empty-text link: `[](/foo)` shows ` (/foo)` after the
        // (empty) styled text.
        out.push_str(&format!("{styled} ({})", (theme.link_url)(href)));
    }
    out.push_str(ctx.prefix);
}

/// The OSC 8 hyperlink run — a port of upstream `hyperlink`
/// (`terminal-image.ts:732-734`): `ESC ] 8 ; ; <url> ST`, the text, then
/// `ESC ] 8 ; ; ST`. No params are ever emitted and ST (`ESC \`) is the
/// terminator, exactly like upstream (and the extension runtime's JS shim
/// `hyperlink()`). Both sequences are zero-width to the renderer
/// (`ansi::visible_width` skips a whole OSC string), so the visible text,
/// wrapping and padding are unchanged by the run.
fn osc8_hyperlink(text: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

fn find_delimiter(chars: &[char], from: usize, delimiter: &str) -> Option<usize> {
    let delim: Vec<char> = delimiter.chars().collect();
    chars[from..]
        .windows(delim.len())
        .position(|window| *window == delim[..])
        .map(|position| position + from)
}

/// The closing run of exactly `run` backticks at or after `from`; longer or
/// shorter runs stay inside the span content (marked's
/// ``(`+)([^`]|[^`][\s\S]*?[^`])\1(?!`)`` rule), and the content between the
/// delimiters must be non-empty.
fn find_code_span_close(chars: &[char], mut from: usize, run: usize) -> Option<usize> {
    let content_start = from;
    while from < chars.len() {
        if chars[from] != '`' {
            from += 1;
            continue;
        }
        let run_len = chars[from..].iter().take_while(|&&x| x == '`').count();
        if run_len == run && from > content_start {
            return Some(from);
        }
        from += run_len;
    }
    None
}

/// CommonMark emphasis flanking helpers. "Punctuation" is any char that is
/// neither alphanumeric nor whitespace (a superset of the spec's class that
/// agrees on the ASCII cases that matter).
fn is_punct_like(c: char) -> bool {
    !c.is_alphanumeric() && !c.is_whitespace()
}

fn preceded_by_ws_or_punct(chars: &[char], i: usize) -> bool {
    i == 0 || chars[i - 1].is_whitespace() || is_punct_like(chars[i - 1])
}

fn followed_by_ws_or_punct(chars: &[char], i: usize) -> bool {
    match chars.get(i + 1) {
        None => true,
        Some(&next) => next.is_whitespace() || is_punct_like(next),
    }
}

fn left_flanking(chars: &[char], i: usize) -> bool {
    let Some(&next) = chars.get(i + 1) else {
        return false;
    };
    !next.is_whitespace() && (!is_punct_like(next) || preceded_by_ws_or_punct(chars, i))
}

fn right_flanking(chars: &[char], i: usize) -> bool {
    if i == 0 {
        return false;
    }
    let prev = chars[i - 1];
    !prev.is_whitespace() && (!is_punct_like(prev) || followed_by_ws_or_punct(chars, i))
}

/// Whether the delimiter `c` at `i` may open emphasis (CommonMark
/// left-flanking rule; `_` additionally may not be both-flanking unless
/// preceded by punctuation).
fn can_open_emphasis(chars: &[char], i: usize, c: char) -> bool {
    if !left_flanking(chars, i) {
        return false;
    }
    if c == '_' && right_flanking(chars, i) && !preceded_by_ws_or_punct(chars, i) {
        return false;
    }
    true
}

/// Whether the delimiter `c` at `i` may close emphasis (CommonMark
/// right-flanking rule; `_` additionally may not be both-flanking unless
/// followed by punctuation).
fn can_close_emphasis(chars: &[char], i: usize, c: char) -> bool {
    if !right_flanking(chars, i) {
        return false;
    }
    if c == '_' && left_flanking(chars, i) && !followed_by_ws_or_punct(chars, i) {
        return false;
    }
    true
}

/// The first closing delimiter for the emphasis opened at `open`.
fn find_emphasis_close(chars: &[char], open: usize, marker: char) -> Option<usize> {
    let mut j = open + 1;
    while j < chars.len() {
        if chars[j] == '\\' {
            j += 2;
            continue;
        }
        if chars[j] == marker && can_close_emphasis(chars, j, marker) {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Parse `<scheme:…>` / `<a@b.c>` autolinks (CommonMark autolink rule).
/// Returns the link content and the index just past `>`.
fn parse_autolink(chars: &[char], open: usize) -> Option<(String, usize)> {
    let mut content = String::new();
    let mut i = open + 1;
    while i < chars.len() {
        let c = chars[i];
        if c == '>' {
            if content.is_empty() {
                return None;
            }
            if is_autolink_content(&content) {
                return Some((content, i + 1));
            }
            return None;
        }
        if c == '<' || c.is_whitespace() || (c as u32) < 0x20 {
            return None;
        }
        content.push(c);
        i += 1;
    }
    None
}

/// CommonMark autolink validity: an absolute URI (`scheme:` with a valid
/// scheme of at most 32 chars) or an email address.
fn is_autolink_content(content: &str) -> bool {
    if is_email_autolink(content) {
        return true;
    }
    let Some(colon) = content.find(':') else {
        return false;
    };
    if colon == 0 || colon > 32 {
        return false;
    }
    let scheme = &content[..colon];
    let mut scheme_chars = scheme.chars();
    if !scheme_chars.next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    scheme_chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
}

/// The CommonMark email autolink pattern, as a full match.
fn is_email_autolink(content: &str) -> bool {
    const LOCAL_EXTRA: &str = ".!#$%&'*+/=?^_`{|}~-";
    let Some(at) = content.find('@') else {
        return false;
    };
    let (local, domain) = content.split_at(at);
    let domain = &domain[1..];
    if local.is_empty()
        || domain.is_empty()
        || domain.contains('@')
        || !local
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || LOCAL_EXTRA.contains(c))
    {
        return false;
    }
    // Domain: dot-separated labels, each alphanumeric at both ends with
    // alphanumerics/hyphens inside (63 chars max per label).
    domain.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
            && label
                .chars()
                .last()
                .is_some_and(|c| c.is_ascii_alphanumeric())
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    })
}

/// Parse `[text](href "title")` starting at `open` (the `[`). Returns
/// `(text, href, end_index_after_paren)`. The optional title is consumed and
/// excluded from the href (marked's `link` rule).
fn parse_link(chars: &[char], open: usize) -> Option<(String, String, usize)> {
    let mut close = None;
    let mut i = open + 1;
    let mut depth = 1usize;
    while i < chars.len() {
        match chars[i] {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            '\\' => i += 1,
            _ => {}
        }
        i += 1;
    }
    let close = close?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let text: String = chars[open + 1..close].iter().collect();
    let mut i = close + 2;

    // Destination: `<…>` (angle-bracket form, may contain spaces) or a bare
    // run with balanced parentheses, ended by unbalanced `)` or whitespace.
    let href: String;
    if chars.get(i) == Some(&'<') {
        i += 1;
        let mut dest = String::new();
        loop {
            match chars.get(i) {
                None => return None,
                Some('>') => {
                    i += 1;
                    break;
                }
                Some('<') => return None,
                Some('\\') => {
                    dest.push(*chars.get(i + 1)?);
                    i += 2;
                }
                Some(&c) => {
                    dest.push(c);
                    i += 1;
                }
            }
        }
        href = dest;
        // Optional title, then the closing `)`.
        i = skip_title_and_close(chars, i)?;
    } else {
        let mut dest = String::new();
        let mut paren_depth = 0usize;
        while let Some(&c) = chars.get(i) {
            if c == '\\' {
                dest.push(*chars.get(i + 1)?);
                i += 2;
                continue;
            }
            if c == '(' {
                paren_depth += 1;
                dest.push(c);
                i += 1;
                continue;
            }
            if c == ')' {
                if paren_depth == 0 {
                    i += 1;
                    break;
                }
                paren_depth -= 1;
                dest.push(c);
                i += 1;
                continue;
            }
            if c.is_whitespace() {
                i += 1;
                break;
            }
            dest.push(c);
            i += 1;
        }
        if chars.get(i.wrapping_sub(1)) == Some(&')') {
            // The destination itself ended at the closing paren.
            return Some((text, dest, i));
        }
        href = dest;
        // Whitespace ended the destination: an optional quoted title and the
        // closing `)` must follow (upstream `link` rule); otherwise this is
        // not a link.
        i = skip_title_and_close(chars, i)?;
    }
    if chars.get(i.wrapping_sub(1)) != Some(&')') {
        return None;
    }
    Some((text, href, i))
}

/// Skip an optional link title (`"…"`, `'…'` or `(...)`) plus surrounding
/// whitespace, then the closing `)`. Returns the index just past `)`.
fn skip_title_and_close(chars: &[char], mut i: usize) -> Option<usize> {
    while matches!(chars.get(i), Some(&c) if c.is_whitespace()) {
        i += 1;
    }
    match chars.get(i) {
        Some(')') => Some(i + 1),
        Some(quote @ ('"' | '\'')) => {
            let quote = *quote;
            i += 1;
            loop {
                match chars.get(i) {
                    None => return None,
                    Some('\\') => i += 2,
                    Some(&c) if c == quote => {
                        i += 1;
                        break;
                    }
                    Some(_) => i += 1,
                }
            }
            skip_close_paren(chars, i)
        }
        Some('(') => {
            let mut depth = 1usize;
            i += 1;
            while depth > 0 {
                match chars.get(i)? {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
            skip_close_paren(chars, i)
        }
        _ => None,
    }
}

/// After a title: optional whitespace, then the closing `)`.
fn skip_close_paren(chars: &[char], mut i: usize) -> Option<usize> {
    while matches!(chars.get(i), Some(&c) if c.is_whitespace()) {
        i += 1;
    }
    if chars.get(i) == Some(&')') {
        Some(i + 1)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// GFM extended autolinks (marked v18)
// ---------------------------------------------------------------------------

/// The `www.` prefix of marked's `url` rule — case-sensitive (upstream
/// `|www\.` is not folded).
fn starts_with_www_dot(chars: &[char], pos: usize) -> bool {
    matches!(chars.get(pos..pos + 4), Some(['w', 'w', 'w', '.']))
}

/// `((?:[hH][tT][tT][pP][sS]?|[fF][tT][pP]))://` — the scheme of marked's
/// `url` rule (case-insensitive `http`/`https`/`ftp`), returning the index
/// just past `://`.
fn match_url_scheme(chars: &[char], pos: usize) -> Option<usize> {
    let low = |index: usize| chars.get(index).copied().map(|c| c.to_ascii_lowercase());
    if low(pos) == Some('h')
        && low(pos + 1) == Some('t')
        && low(pos + 2) == Some('t')
        && low(pos + 3) == Some('p')
    {
        let mut i = pos + 4;
        // `[sS]?`: with `s` present but no `://` after it, the backtrack
        // would demand `:` at the `s` position — impossible.
        if low(i) == Some('s') {
            i += 1;
        }
        if low(i) == Some(':') && low(i + 1) == Some('/') && low(i + 2) == Some('/') {
            return Some(i + 3);
        }
        return None;
    }
    if low(pos) == Some('f')
        && low(pos + 1) == Some('t')
        && low(pos + 2) == Some('p')
        && low(pos + 3) == Some(':')
        && low(pos + 4) == Some('/')
        && low(pos + 5) == Some('/')
    {
        return Some(pos + 6);
    }
    None
}

/// A GFM extended autolink at `pos` (marked v18 `rules.inline.url` +
/// the `url` tokenizer): a bare `scheme://…` / `www.…` URL or a bare email.
/// Returns `(link_text, href)`; the trailing-punctuation trim is marked's
/// `_backpedal` fixpoint.
fn parse_extended_autolink(chars: &[char], pos: usize) -> Option<(String, String)> {
    let is_url_host_start = |c: char| c.is_ascii_alphanumeric() || c == '-';
    let prefix_end: usize;
    let is_www: bool;
    if let Some(end) = match_url_scheme(chars, pos) {
        prefix_end = end;
        is_www = false;
    } else if starts_with_www_dot(chars, pos) {
        prefix_end = pos + 4;
        is_www = true;
    } else {
        return match_extended_email(chars, pos);
    }
    // `(?:[a-zA-Z0-9\-]+\.?)+` needs at least one host char (a leading `.` or
    // any other punctuation fails the rule); `[^\s<]*` then runs to the first
    // whitespace or `<`. Greedy backtracking cannot change the total end:
    // every char the host part accepts, `[^\s<]*` accepts too.
    if !chars
        .get(prefix_end)
        .copied()
        .is_some_and(is_url_host_start)
    {
        return None;
    }
    let mut end = prefix_end;
    while matches!(chars.get(end), Some(&c) if !c.is_whitespace() && c != '<') {
        end += 1;
    }
    let text = backpedal(&chars[pos..end]);
    let href = if is_www {
        format!("http://{text}")
    } else {
        text.clone()
    };
    Some((text, href))
}

/// marked's email branch: `[A-Za-z0-9._+-]+(@)[a-zA-Z0-9-_]+
/// (?:\.[a-zA-Z0-9-_]*[a-zA-Z0-9])+(?![-_])`, href = `mailto:` + text. No
/// `_backpedal` is applied (the domain grammar itself excludes the trailing
/// punctuation).
fn match_extended_email(chars: &[char], pos: usize) -> Option<(String, String)> {
    let is_local_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-');
    let mut local_end = pos;
    while matches!(chars.get(local_end), Some(&c) if is_local_char(c)) {
        local_end += 1;
    }
    if local_end == pos || chars.get(local_end) != Some(&'@') {
        return None;
    }
    // The local run stops at `@` (it is not a local char), so the greedy
    // `(@)` needs no backtracking.
    let domain_end = match_email_domain(chars, local_end + 1)?;
    let text: String = chars[pos..domain_end].iter().collect();
    let href = format!("mailto:{text}");
    Some((text, href))
}

/// `[a-zA-Z0-9-_]+(?:\.[a-zA-Z0-9-_]*[a-zA-Z0-9])+(?![-_])` with the regex's
/// greedy + backtracking order: the first label shortens from its longest
/// run, each dot-label tries one more group before the trailing lookahead.
fn match_email_domain(chars: &[char], pos: usize) -> Option<usize> {
    let is_domain_char = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let mut run_end = pos;
    while matches!(chars.get(run_end), Some(&c) if is_domain_char(c)) {
        run_end += 1;
    }
    for label_end in (pos + 1..=run_end).rev() {
        if let Some(end) = match_dot_labels(chars, label_end) {
            return Some(end);
        }
    }
    None
}

/// The `(?:\.Lk)+` tail, `Lk = [a-zA-Z0-9-_]*[a-zA-Z0-9]`: every label must
/// end on an alphanumeric (so `a@b..com` and `a@.com` fail), then the whole
/// match may not stop on `-` or `_` (so `user@example.co` wins over the
/// trailing `-` of `user@example.com-`).
fn match_dot_labels(chars: &[char], pos: usize) -> Option<usize> {
    if chars.get(pos) != Some(&'.') {
        return None;
    }
    let is_domain_char = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    let mut run_end = pos + 1;
    while matches!(chars.get(run_end), Some(&c) if is_domain_char(c)) {
        run_end += 1;
    }
    for label_end in (pos + 2..=run_end).rev() {
        if !chars[label_end - 1].is_ascii_alphanumeric() {
            continue;
        }
        // Greedy `+`: one more dot-label group first …
        if let Some(end) = match_dot_labels(chars, label_end) {
            return Some(end);
        }
        // … then the trailing `(?![-_])` lookahead.
        if !matches!(chars.get(label_end), Some('-') | Some('_')) {
            return Some(label_end);
        }
    }
    None
}

/// The `_backpedal` fixpoint (marked `url` tokenizer): apply
/// [`backpedal_once`] until the match stabilizes, trimming trailing
/// punctuation (each pass removes one trailing segment, so `…path).` loses
/// `.` then `)`).
fn backpedal(url: &[char]) -> String {
    let mut current: String = url.iter().collect();
    loop {
        let next = backpedal_once(&current);
        if next == current {
            return next;
        }
        current = next;
    }
}

/// One `_backpedal` pass: the longest prefix of `url` consumable by
/// `[^?!.,:;*_'"~()&]+ | \([^)]*\) | &(?![a-zA-Z0-9]+;$) | [?!.,:;*_'"~)]+(?!$)`
/// (marked `rules.inline._backpedal`; the match is anchored at the scheme).
fn backpedal_once(url: &str) -> String {
    // Group 1's exclusion set (every one of these stops the plain run).
    let wide_punct = |c: char| {
        matches!(
            c,
            '?' | '!' | '.' | ',' | ':' | ';' | '*' | '_' | '\'' | '"' | '~' | '(' | ')' | '&'
        )
    };
    // Group 4's set (group 1's set minus `(` and `&`, which have their own
    // alternatives).
    let narrow_punct = |c: char| {
        matches!(
            c,
            '?' | '!' | '.' | ',' | ':' | ';' | '*' | '_' | '\'' | '"' | '~' | ')'
        )
    };
    let chars: Vec<char> = url.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if !wide_punct(c) {
            while i < chars.len() && !wide_punct(chars[i]) {
                i += 1;
            }
        } else if c == '(' {
            // `\([^)]*\)`: a balanced (non-nested) paren group survives.
            match chars[i + 1..].iter().position(|&x| x == ')') {
                Some(close) => i += close + 2,
                None => break,
            }
        } else if c == '&' {
            // `&(?![a-zA-Z0-9]+;$)`: an entity reference ending the string
            // stops the match (its `;` would otherwise be trimmed).
            let mut j = i + 1;
            while matches!(chars.get(j), Some(&x) if x.is_ascii_alphanumeric()) {
                j += 1;
            }
            if j > i + 1 && chars.get(j) == Some(&';') && j + 1 == chars.len() {
                break;
            }
            i += 1;
        } else {
            // `[?!.,:;*_'"~)]+(?!$)`: the greedy run backtracks one char
            // when it reaches the end (internal punctuation survives).
            let mut j = i;
            while j < chars.len() && narrow_punct(chars[j]) {
                j += 1;
            }
            if j < chars.len() {
                i = j;
            } else if j - i >= 2 {
                i = j - 1;
            } else {
                break;
            }
        }
    }
    chars[..i].iter().collect()
}
