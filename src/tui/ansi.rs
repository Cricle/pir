//! ANSI text utilities — the terminal-side primitives behind every
//! component (`pi-tui`'s `utils.ts` subset used by the chat surface).
//!
//! All components render to plain `String` lines containing ANSI SGR
//! sequences; the core diff renderer compares and repaints those lines.
//! Width is measured in terminal cells over GRAPHEME CLUSTERS (UAX #29
//! extended clusters) — the unit `Intl.Segmenter` gives upstream — so a ZWJ
//! emoji, a flag pair, a keycap or a combining sequence counts and clips as
//! one character (upstream `graphemeWidth` / `visibleWidth`).

// Pre-existing baseline lints (tui is frozen for the parity batch).
#![allow(clippy::if_same_then_else)]

use unicode_normalization::char::is_combining_mark;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

/// Reset all SGR attributes.
pub const RESET: &str = "\x1b[0m";
/// Reset foreground only (theme `fg` pairs with this, like upstream).
pub const FG_RESET: &str = "\x1b[39m";
/// Reset background only (theme `bg` pairs with this).
pub const BG_RESET: &str = "\x1b[49m";

/// A `UAX #29 WB4`-style word-run joiner: a combining mark, or another
/// non-alphanumeric zero-width code point (variation selectors, ZWJ, the
/// enclosing keycap). Word segmentation keeps such code points inside the
/// word run that precedes them.
pub fn is_word_joiner(c: char) -> bool {
    is_combining_mark(c) || (UnicodeWidthChar::width(c) == Some(0) && !c.is_alphanumeric())
}

/// The UAX #29 extended grapheme clusters of `text`, in order — upstream
/// `Intl.Segmenter(undefined, { granularity: "grapheme" })`.
pub fn grapheme_clusters(text: &str) -> Vec<&str> {
    UnicodeSegmentation::graphemes(text, true).collect()
}

/// Grapheme clusters of `text` as `(char_index, char_len)` spans. Editor
/// cursor positions are char indices, so motion / deletion / wrapping need
/// the cluster lengths in chars, not bytes.
pub fn grapheme_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    for cluster in UnicodeSegmentation::graphemes(text, true) {
        let len = cluster.chars().count();
        spans.push((start, len));
        start += len;
    }
    spans
}

/// `General_Category=Mc` (Spacing_Mark) code-point ranges, from the Unicode
/// 15 Character Database — the `\p{Spacing_Mark}` class of upstream's
/// `terminalSpacingMarkRegex` (`utils.ts:46-47`).
#[rustfmt::skip]
const SPACING_MARK_RANGES: &[(u32, u32)] = &[
    (0x903, 0x903), (0x93B, 0x93B), (0x93E, 0x940), (0x949, 0x94C),
    (0x94E, 0x94F), (0x982, 0x983), (0x9BE, 0x9C0), (0x9C7, 0x9C8),
    (0x9CB, 0x9CC), (0x9D7, 0x9D7), (0xA03, 0xA03), (0xA3E, 0xA40),
    (0xA83, 0xA83), (0xABE, 0xAC0), (0xAC9, 0xAC9), (0xACB, 0xACC),
    (0xB02, 0xB03), (0xB3E, 0xB3E), (0xB40, 0xB40), (0xB47, 0xB48),
    (0xB4B, 0xB4C), (0xB57, 0xB57), (0xBBE, 0xBBF), (0xBC1, 0xBC2),
    (0xBC6, 0xBC8), (0xBCA, 0xBCC), (0xBD7, 0xBD7), (0xC01, 0xC03),
    (0xC41, 0xC44), (0xC82, 0xC83), (0xCBE, 0xCBE), (0xCC0, 0xCC4),
    (0xCC7, 0xCC8), (0xCCA, 0xCCB), (0xCD5, 0xCD6), (0xCF3, 0xCF3),
    (0xD02, 0xD03), (0xD3E, 0xD40), (0xD46, 0xD48), (0xD4A, 0xD4C),
    (0xD57, 0xD57), (0xD82, 0xD83), (0xDCF, 0xDD1), (0xDD8, 0xDDF),
    (0xDF2, 0xDF3), (0xF3E, 0xF3F), (0xF7F, 0xF7F), (0x102B, 0x102C),
    (0x1031, 0x1031), (0x1038, 0x1038), (0x103B, 0x103C), (0x1056, 0x1057),
    (0x1062, 0x1064), (0x1067, 0x106D), (0x1083, 0x1084), (0x1087, 0x108C),
    (0x108F, 0x108F), (0x109A, 0x109C), (0x1715, 0x1715), (0x17B6, 0x17B6),
    (0x17BE, 0x17C5), (0x17C7, 0x17C8), (0x1923, 0x1926), (0x1929, 0x192B),
    (0x1930, 0x1931), (0x1933, 0x1938), (0x1A19, 0x1A1A), (0x1A55, 0x1A55),
    (0x1A57, 0x1A57), (0x1A61, 0x1A61), (0x1A63, 0x1A64), (0x1A6D, 0x1A72),
    (0x1B04, 0x1B04), (0x1B35, 0x1B35), (0x1B3B, 0x1B3B), (0x1B3D, 0x1B41),
    (0x1B43, 0x1B44), (0x1B82, 0x1B82), (0x1BA1, 0x1BA1), (0x1BA6, 0x1BA7),
    (0x1BAA, 0x1BAA), (0x1BE7, 0x1BE7), (0x1BEA, 0x1BEC), (0x1BEE, 0x1BEE),
    (0x1BF2, 0x1BF3), (0x1C24, 0x1C2B), (0x1C34, 0x1C35), (0x1CE1, 0x1CE1),
    (0x1CF7, 0x1CF7), (0x302E, 0x302F), (0xA823, 0xA824), (0xA827, 0xA827),
    (0xA880, 0xA881), (0xA8B4, 0xA8C3), (0xA952, 0xA953), (0xA983, 0xA983),
    (0xA9B4, 0xA9B5), (0xA9BA, 0xA9BB), (0xA9BE, 0xA9C0), (0xAA2F, 0xAA30),
    (0xAA33, 0xAA34), (0xAA4D, 0xAA4D), (0xAA7B, 0xAA7B), (0xAA7D, 0xAA7D),
    (0xAAEB, 0xAAEB), (0xAAEE, 0xAAEF), (0xAAF5, 0xAAF5), (0xABE3, 0xABE4),
    (0xABE6, 0xABE7), (0xABE9, 0xABEA), (0xABEC, 0xABEC), (0x11000, 0x11000),
    (0x11002, 0x11002), (0x11082, 0x11082), (0x110B0, 0x110B2), (0x110B7, 0x110B8),
    (0x1112C, 0x1112C), (0x11145, 0x11146), (0x11182, 0x11182), (0x111B3, 0x111B5),
    (0x111BF, 0x111C0), (0x111CE, 0x111CE), (0x1122C, 0x1122E), (0x11232, 0x11233),
    (0x11235, 0x11235), (0x112E0, 0x112E2), (0x11302, 0x11303), (0x1133E, 0x1133F),
    (0x11341, 0x11344), (0x11347, 0x11348), (0x1134B, 0x1134D), (0x11357, 0x11357),
    (0x11362, 0x11363), (0x11435, 0x11437), (0x11440, 0x11441), (0x11445, 0x11445),
    (0x114B0, 0x114B2), (0x114B9, 0x114B9), (0x114BB, 0x114BE), (0x114C1, 0x114C1),
    (0x115AF, 0x115B1), (0x115B8, 0x115BB), (0x115BE, 0x115BE), (0x11630, 0x11632),
    (0x1163B, 0x1163C), (0x1163E, 0x1163E), (0x116AC, 0x116AC), (0x116AE, 0x116AF),
    (0x116B6, 0x116B6), (0x11720, 0x11721), (0x11726, 0x11726), (0x1182C, 0x1182E),
    (0x11838, 0x11838), (0x11930, 0x11935), (0x11937, 0x11938), (0x1193D, 0x1193D),
    (0x11940, 0x11940), (0x11942, 0x11942), (0x119D1, 0x119D3), (0x119DC, 0x119DF),
    (0x119E4, 0x119E4), (0x11A39, 0x11A39), (0x11A57, 0x11A58), (0x11A97, 0x11A97),
    (0x11C2F, 0x11C2F), (0x11C3E, 0x11C3E), (0x11CA9, 0x11CA9), (0x11CB1, 0x11CB1),
    (0x11CB4, 0x11CB4), (0x11D8A, 0x11D8E), (0x11D93, 0x11D94), (0x11D96, 0x11D96),
    (0x11EF5, 0x11EF6), (0x11F03, 0x11F03), (0x11F34, 0x11F35), (0x11F3E, 0x11F3F),
    (0x11F41, 0x11F41), (0x16F51, 0x16F87), (0x16FF0, 0x16FF1), (0x1D165, 0x1D166),
    (0x1D16D, 0x1D172),
];

fn is_spacing_mark(c: char) -> bool {
    let cp = c as u32;
    SPACING_MARK_RANGES
        .binary_search_by(|&(lo, hi)| {
            if cp < lo {
                std::cmp::Ordering::Greater
            } else if cp > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Marks that terminals allocate cells for when attached to a base character:
/// Unicode spacing marks minus the three conjoining exceptions, plus the
/// non-spacing exceptions from legacy wcwidth tables (upstream
/// `terminalSpacingMarkRegex`, `utils.ts:44-47`).
fn is_terminal_spacing_mark(c: char) -> bool {
    (is_spacing_mark(c) && !matches!(c, '\u{1734}' | '\u{302E}' | '\u{302F}'))
        || matches!(
            c,
            '\u{065F}'
                | '\u{0F7F}'
                | '\u{102B}'
                | '\u{102C}'
                | '\u{1031}'
                | '\u{1033}'..='\u{1035}'
                | '\u{1038}'
                | '\u{103A}'..='\u{103E}'
        )
}

/// `\p{Default_Ignorable_Code_Point} | \p{Control} | \p{Format} | \p{Mark}
/// | \p{Surrogate}` for a single character (upstream `nonPrintingCharRegex`).
/// Hangul jamo medial vowels / final consonants are zero-width in wcwidth
/// tables but are NOT Default_Ignorable, so upstream keeps them as visible
/// base code points; they are excluded here too.
fn is_non_printing(c: char) -> bool {
    c.is_control()
        || is_combining_mark(c)
        || (UnicodeWidthChar::width(c) == Some(0) && !is_conjoining_letter(c))
}

/// Hangul jamo medial vowels and final consonants (plus the extended blocks):
/// `Lo` letters with zero cell width.
fn is_conjoining_letter(c: char) -> bool {
    matches!(c as u32, 0x1160..=0x11FF | 0xA960..=0xA97F | 0xD7B0..=0xD7FF)
}

/// Zero-width-only cluster, i.e. every code point is non-printing
/// (upstream `zeroWidthRegex`).
fn is_zero_width_cluster(chars: &[char]) -> bool {
    !chars.is_empty() && chars.iter().all(|&c| is_non_printing(c))
}

fn is_regional_indicator(c: char) -> bool {
    matches!(c, '\u{1F1E6}'..='\u{1F1FF}')
}

/// `EAW ∈ {Wide, Fullwidth} → 2, else 1` — upstream `eastAsianWidth`
/// (`utils.ts:218`). `unicode-width`'s per-char table is the same
/// East Asian Width data, so its 2 matches; everything else (including
/// zero-width codepoints, which the callers exclude first) counts 1.
fn east_asian_width(c: char) -> usize {
    if UnicodeWidthChar::width(c) == Some(2) {
        2
    } else {
        1
    }
}

/// Fast pre-filter for the emoji test (upstream `couldBeEmoji`,
/// `utils.ts:27-37`) — deliberately broad blocks.
fn could_be_emoji(cluster: &str) -> bool {
    let Some(first) = cluster.chars().next() else {
        return false;
    };
    let cp = first as u32;
    (0x1F000..=0x1FBFF).contains(&cp)
        || (0x2300..=0x23FF).contains(&cp)
        || (0x2600..=0x27BF).contains(&cp)
        || (0x2B50..=0x2B55).contains(&cp)
        || cluster.contains('\u{FE0F}')
        || cluster.chars().count() > 2
}

/// Emoji-capable code point: upstream's `couldBeEmoji` blocks plus the
/// legacy emoji whose `East_Asian_Width` is not Wide (their `Emoji_Presentation`
/// form comes from a `U+FE0F` sequence).
fn is_emoji_codepoint(c: char) -> bool {
    let cp = c as u32;
    (0x1F000..=0x1FBFF).contains(&cp)
        || (0x2300..=0x23FF).contains(&cp)
        || (0x2600..=0x27BF).contains(&cp)
        || (0x2B50..=0x2B55).contains(&cp)
        || matches!(
            cp,
            0x00A9
                | 0x00AE
                | 0x203C
                | 0x2049
                | 0x2122
                | 0x2139
                | 0x2194..=0x2199
                | 0x21A9..=0x21AA
                | 0x24C2
                | 0x25AA..=0x25AB
                | 0x25B6
                | 0x25C0
                | 0x25FB..=0x25FE
                | 0x2934..=0x2935
                | 0x2B05..=0x2B07
                | 0x2B1B..=0x2B1C
                | 0x3030
                | 0x303D
                | 0x3297
                | 0x3299
        )
}

/// Emoji-sequence glue that does not add cells of its own: ZWJ, variation
/// selectors, skin-tone modifiers, tag characters and the enclosing keycap.
fn is_emoji_component(c: char) -> bool {
    matches!(
        c,
        '\u{200D}'
            | '\u{FE0E}'
            | '\u{FE0F}'
            | '\u{1F3FB}'..='\u{1F3FF}'
            | '\u{20E3}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

fn is_keycap_base(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '#' | '*')
}

/// `\p{RGI_Emoji}` test (upstream `rgiEmojiRegex`, `utils.ts:48,200`): does
/// this cluster render as one double-width emoji? Recognizes the RGI
/// sequence families that matter for width — keycaps, flag pairs, tag
/// sequences, modifier sequences, ZWJ sequences and emoji-presentation
/// (`U+FE0F`) sequences. Plain text-presentation emoji (`☺`, `A`) stay at
/// their East Asian Width, matching upstream.
fn is_rgi_emoji_like(chars: &[char]) -> bool {
    if chars.is_empty() {
        return false;
    }
    // Keycap sequence: [0-9#*] U+FE0F U+20E3.
    if chars.len() == 3
        && is_keycap_base(chars[0])
        && chars[1] == '\u{FE0F}'
        && chars[2] == '\u{20E3}'
    {
        return true;
    }
    // Flag sequence: exactly two regional indicators.
    if chars.len() == 2 && is_regional_indicator(chars[0]) && is_regional_indicator(chars[1]) {
        return true;
    }
    // `core` is the cluster without the emoji-sequence glue, falling back to
    // the whole cluster when every code point is glue. Walked as an iterator
    // (no `Vec<char>` / `to_vec` per emoji cluster): width measurement runs on
    // every rendered row, so the materialized copies were frame allocations.
    let first_core = chars.iter().copied().find(|&c| !is_emoji_component(c));
    let all_core_emoji = match first_core {
        Some(_) => chars
            .iter()
            .copied()
            .filter(|&c| !is_emoji_component(c))
            .all(is_emoji_codepoint),
        None => chars.iter().copied().all(is_emoji_codepoint),
    };
    if !all_core_emoji {
        return false;
    }
    let first = first_core.unwrap_or(chars[0]);
    let has_emoji_presentation_selector = chars.contains(&'\u{FE0F}');
    has_emoji_presentation_selector || UnicodeWidthChar::width(first) == Some(2)
}

/// Width of one grapheme cluster in terminal cells — port of upstream
/// `graphemeWidth` (`utils.ts:182-245`), including the emoji fast path, the
/// regional-indicator rule and the trailing spacing-mark accounting.
///
/// Allocation-free by design: `visible_width` runs this for every cluster of
/// every measured row of every frame (padding, wrapping, truncation, the
/// tool-box row painter), and the original `cluster.chars().collect::<Vec<_>>()`
/// was one 16 B heap allocation per cluster — the per-row churn the
/// frame-reuse allocation pins caught. The cluster's code points live in a
/// stack buffer; only a pathological cluster longer than
/// [`INLINE_CLUSTER_CHARS`] falls back to a `Vec`.
pub fn grapheme_width(cluster: &str) -> usize {
    // Single-byte fast path: one printable ASCII cell (the overwhelming
    // majority of measured clusters) or the 3-cell tab. Other ASCII bytes are
    // control characters and fall through to the non-printing rules.
    if cluster.len() == 1 {
        let byte = cluster.as_bytes()[0];
        if (0x20..=0x7E).contains(&byte) {
            return 1;
        }
        if byte == b'\t' {
            return 3;
        }
    }
    cluster_chars(cluster, |chars| grapheme_width_chars(cluster, chars))
}

/// The largest cluster whose code points [`grapheme_width`] keeps on the
/// stack; longer (pathological ZWJ) clusters fall back to a heap `Vec<char>`.
const INLINE_CLUSTER_CHARS: usize = 16;

/// Run `f` with the code points of `cluster`, borrowed from a stack buffer for
/// every realistic cluster (a flag pair, a keycap, a ZWJ emoji is 2-8 chars).
fn cluster_chars<R>(cluster: &str, f: impl FnOnce(&[char]) -> R) -> R {
    let mut inline = ['\0'; INLINE_CLUSTER_CHARS];
    let mut count = 0usize;
    for c in cluster.chars() {
        if count == INLINE_CLUSTER_CHARS {
            let chars: Vec<char> = cluster.chars().collect();
            return f(&chars);
        }
        inline[count] = c;
        count += 1;
    }
    f(&inline[..count])
}

/// [`grapheme_width`] over the cluster's code points (`cluster` is passed
/// separately for [`could_be_emoji`]'s string-level pre-filter).
fn grapheme_width_chars(cluster: &str, chars: &[char]) -> usize {
    if chars.len() == 1 {
        let c = chars[0];
        if ('\u{20}'..='\u{7E}').contains(&c) {
            return 1;
        }
        if c == '\t' {
            return 3;
        }
    }
    // Some marks occupy cells even without a base character.
    if !chars.is_empty() && chars.iter().all(|&c| is_terminal_spacing_mark(c)) {
        return chars.len();
    }
    // Zero-width clusters.
    if is_zero_width_cluster(chars) {
        return 0;
    }
    // Emoji check with the pre-filter.
    if could_be_emoji(cluster) && is_rgi_emoji_like(chars) {
        return 2;
    }
    // Base visible code point: the first code point that is not a non-printing
    // mark/control (walked in place instead of collecting a `Vec<char>`).
    let Some(base_start) = chars.iter().position(|&c| !is_non_printing(c)) else {
        return 0;
    };
    let base = &chars[base_start..];
    let first = base[0];
    // Regional indicators render full-width in terminals even when split by
    // the segmenter mid-stream (upstream `utils.ts:211-216`).
    if is_regional_indicator(first) {
        return 2;
    }
    let mut width = east_asian_width(first);
    // Count trailing visible code points terminals may allocate cells for:
    // Indic consonants after marks, halfwidth/fullwidth forms, Thai/Lao AM.
    let mut follows_mark = false;
    for &c in base.iter().skip(1) {
        if is_terminal_spacing_mark(c) {
            width += 1;
            follows_mark = false;
        } else if is_combining_mark(c) {
            follows_mark = true;
        } else if !is_non_printing(c) {
            if follows_mark || ('\u{FF00}'..='\u{FFEF}').contains(&c) {
                width += east_asian_width(c);
            } else if c == '\u{0E33}' || c == '\u{0EB3}' {
                width += 1;
            }
            follows_mark = false;
        }
    }
    width
}

/// Visible width of `text` in terminal cells: tabs count as 3 spaces,
/// ANSI escape sequences are skipped, and each grapheme cluster contributes
/// its own width (upstream `visibleWidth`).
///
/// The styled case is measured in one pass over the bytes with no
/// intermediate `String`: escape sequences are skipped exactly as
/// `strip_ansi` deletes them, and every visible run is segmented where it
/// lies. A cluster can only reach across an escape sequence when the two
/// characters flanking it join (a combining mark after its base, a ZWJ emoji
/// sequence, a flag pair, ...); that rare shape falls back to the
/// materialized path so the width stays identical either way.
pub fn visible_width(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    if text.is_ascii() && !text.contains('\x1b') && !text.contains('\t') {
        return text.chars().filter(|c| !c.is_ascii_control()).count();
    }
    if !text.contains('\x1b') {
        // `strip_ansi` only ever deletes escape sequences, so with no ESC to
        // open one it is the identity: the clusters are measured in place.
        return sum_cluster_widths(text);
    }
    match width_without_escapes(text) {
        Some(width) => width,
        None => sum_cluster_widths(&strip_ansi(text)),
    }
}

/// Sum of `grapheme_width` over the grapheme clusters of `text`.
fn sum_cluster_widths(text: &str) -> usize {
    let mut width = 0usize;
    // Iterate the clusters directly instead of materializing
    // `grapheme_clusters`' `Vec<&str>`: this runs for every measured line of
    // every frame (padding, wrapping, truncation), and the vec is discarded
    // right after the sum.
    for cluster in UnicodeSegmentation::graphemes(text, true) {
        width += grapheme_width(cluster);
    }
    width
}

/// Width of `text` accumulated over the visible runs between its escape
/// sequences — `None` when a grapheme cluster may span such a sequence, i.e.
/// when the characters flanking it can join (only then does the segmenter
/// have to see the runs as one contiguous string).
fn width_without_escapes(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut width = 0usize;
    // Last visible character measured, and the run it belongs to: the run
    // following an escape sequence starts a new cluster unless this character
    // joins it.
    let mut previous: Option<char> = None;
    let mut run_start = 0usize;
    let mut cursor = 0usize;
    loop {
        let run_end = match text[cursor..].find('\x1b') {
            Some(offset) => cursor + offset,
            None => bytes.len(),
        };
        if run_end > run_start {
            let run = &text[run_start..run_end];
            if let (Some(before), Some(after)) = (previous, run.chars().next()) {
                if !gap_is_cluster_boundary(before, after) {
                    return None;
                }
            }
            width += sum_cluster_widths(run);
            previous = run.chars().next_back();
        }
        if run_end >= bytes.len() {
            return Some(width);
        }
        cursor = escape_sequence_end(bytes, run_end);
        run_start = cursor;
    }
}

/// Index just past the bytes `strip_ansi` deletes for the escape sequence
/// starting at byte `escape` (always an ESC).
///
/// The scan is byte-wise, like `strip_ansi`'s is char-wise: escape-sequence
/// syntax is ASCII (`ESC`, `[`, `@..=~`, `]`, `_`, BEL, `\`), and UTF-8
/// continuation bytes are all `>= 0x80`, so no other code point can be
/// mistaken for any of it.
fn escape_sequence_end(bytes: &[u8], escape: usize) -> usize {
    let mut i = escape + 1;
    match bytes.get(i) {
        // CSI: `ESC [ parameters final`, dropped through the first byte in
        // `@..=~` (and through the end of the text while unterminated).
        Some(b'[') => {
            i += 1;
            while i < bytes.len() {
                let byte = bytes[i];
                i += 1;
                if (b'@'..=b'~').contains(&byte) {
                    break;
                }
            }
            i
        }
        // OSC / PM / APC: `ESC ] text BEL` or `ESC ] text ST`, likewise to the
        // end of the text while unterminated.
        Some(b']') | Some(b'_') => {
            i += 1;
            let mut prev = 0u8;
            while i < bytes.len() {
                let byte = bytes[i];
                i += 1;
                if byte == 0x07 || (prev == 0x1b && byte == b'\\') {
                    break;
                }
                prev = byte;
            }
            i
        }
        // A lone ESC (or one at the end of the text): only the ESC goes.
        _ => i,
    }
}

/// Are the visible characters flanking an escape sequence certainly separate
/// grapheme clusters? Rules GB3, GB6-GB8, GB9b, GB11 and GB12/GB13 join them
/// only when one of the two characters belongs to a narrow set, which the two
/// checks below refute; the remaining rules (GB9, GB9a, GB9c) need a mark,
/// joiner or conjunct consonant on the right and are probed against the
/// segmenter itself.
fn gap_is_cluster_boundary(left: char, right: char) -> bool {
    !left_may_join(left) && !right_may_join(right)
}

/// Could the cluster before the gap continue into it? Covers every left
/// character a joining rule can be gated on: GB3 (`CR x LF`), GB9b
/// (`Prepend x Any`), GB6-GB8 (Hangul), GB11 (`ZWJ x Extended_Pictographic`)
/// and GB12/GB13 (`RI x RI`).
fn left_may_join(left: char) -> bool {
    if left.is_ascii() {
        // The segmenter classes ASCII as `Any`, `CR`, `LF` or `Control`; GB3
        // is the only joining rule of those that an ASCII left character can
        // trigger (every other one needs a non-ASCII character).
        return left == '\r';
    }
    left == '\u{200D}'                  // ZWJ (GB11)
        || is_regional_indicator(left)  // regional indicator (GB12/GB13)
        || is_hangul_break(left)        // jamo / syllables (GB6-GB8)
        || probe_one_cluster_str(left, 'x') // Prepend (GB9b)
}

/// Could the cluster after the gap start before it? GB9 (`x Extend | ZWJ`),
/// GB9a (`x SpacingMark`) and GB9c (`Consonant [Extend Linker]* Linker
/// [Extend Linker]* x Consonant`) all attach such a character to the visible
/// character before it.
fn right_may_join(right: char) -> bool {
    if right.is_ascii() {
        // Same ASCII classes as above: none of them attaches to what precedes
        // it (GB9, GB9a and GB9c all need a non-ASCII character).
        return false;
    }
    // A mark after any plain base character (`a`), then an Indic consonant
    // after the consonant + linker context GB9c looks back for.
    probe_one_cluster_str('a', right) || probe_one_cluster(CONJUNCT_PROBE, right)
}

/// Hangul jamo and precomposed syllables — the characters GB6, GB7 and GB8
/// join. Whole blocks are matched, so the check only over-approximates (which
/// costs the fast path, never correctness).
fn is_hangul_break(c: char) -> bool {
    matches!(
        c as u32,
        0x1100..=0x11FF | 0xAC00..=0xD7A3 | 0xA960..=0xA97F | 0xD7B0..=0xD7FF
    )
}

/// `Consonant + Linker` (Devanagari ka + virama): the shortest left context
/// GB9c needs to join a following consonant.
const CONJUNCT_PROBE: &str = "\u{0915}\u{094D}";

/// Stack scratch for the probes below: `CONJUNCT_PROBE` plus one code point.
const PROBE_BUF_LEN: usize = 16;

/// `lead` immediately followed by `tail`, probed against the very crate
/// `visible_width` segments with: `true` when they form a single grapheme
/// cluster.
fn probe_one_cluster_str(lead: char, tail: char) -> bool {
    let mut lead_buf = [0u8; 4];
    let lead: &str = lead.encode_utf8(&mut lead_buf);
    probe_one_cluster(lead, tail)
}

/// Same probe for a `&str` lead; the pair is built in a stack buffer, so
/// nothing is allocated. A probe that cannot be built or segmented answers
/// `true`, i.e. "the characters may join".
fn probe_one_cluster(lead: &str, tail: char) -> bool {
    let mut buf = [0u8; PROBE_BUF_LEN];
    let mut tail_buf = [0u8; 4];
    let lead = lead.as_bytes();
    let tail = tail.encode_utf8(&mut tail_buf).as_bytes();
    let len = lead.len() + tail.len();
    if len > PROBE_BUF_LEN {
        return true;
    }
    buf[..lead.len()].copy_from_slice(lead);
    buf[lead.len()..len].copy_from_slice(tail);
    let Ok(probe) = std::str::from_utf8(&buf[..len]) else {
        return true;
    };
    let mut clusters = UnicodeSegmentation::grapheme_indices(probe, true);
    clusters.next().is_some() && clusters.next().is_none()
}

/// Truncate `text` to `max_width` visible cells, appending `ellipsis` when
/// clipped. ANSI codes are preserved and don't count toward width
/// (upstream `truncateToWidth`).
pub fn truncate_to_width(text: &str, max_width: usize, ellipsis: &str) -> String {
    if max_width == 0 {
        return String::new();
    }
    if text.is_empty() {
        return String::new();
    }
    let ellipsis_width = visible_width(ellipsis);
    if ellipsis_width >= max_width {
        if visible_width(text) <= max_width {
            return text.to_string();
        }
        // Ellipsis alone fills the budget: clip it.
        return clip_to_width(ellipsis, max_width);
    }
    let text_width = visible_width(text);
    if text_width <= max_width {
        return text.to_string();
    }
    let budget = max_width - ellipsis_width;
    let mut clipped = clip_to_width(text, budget);
    // Upstream `finalizeTruncatedResult` (`utils.ts:155-175`) emits
    // `${prefix}${getActiveOsc8Close(prefix)}\x1b[0m${ellipsis}\x1b[0m`: the
    // ellipsis lands OUTSIDE any hyperlink the clipped prefix leaves open.
    // `clip_to_width` copies every escape sequence of the input, so a close
    // that followed the cut is already in `clipped` and the tracker sees the
    // run closed; a run the input never closes (a prefix of a link) needs the
    // synthesized close here — otherwise the ellipsis, and any padding
    // written after this string, stays part of the link and is clickable.
    if let Some(close) = active_osc8_close(&clipped) {
        clipped.push_str(&close);
        clipped.push_str("\x1b[0m");
        if !ellipsis.is_empty() {
            clipped.push_str(ellipsis);
            clipped.push_str("\x1b[0m");
        }
    } else {
        clipped.push_str(ellipsis);
    }
    clipped
}

/// The OSC 8 close for the hyperlink still open at the end of `prefix`, or
/// `None` (upstream `getActiveOsc8Close`, `utils.ts:520-541`): the last
/// recognized OSC 8 code decides, and the close carries that run's own
/// terminator.
fn active_osc8_close(prefix: &str) -> Option<String> {
    let mut tracker = AnsiCodeTracker::default();
    process_tracker_from_text(prefix, &mut tracker);
    tracker
        .active_hyperlink
        .as_ref()
        .map(ActiveHyperlink::close)
}

/// Hard-clip to `width` visible cells without ellipsis, preserving ANSI
/// sequences that occur before the cut. Clusters are indivisible: a cluster
/// that does not fit is dropped whole (upstream `truncateFragmentToWidth`
/// segments at grapheme granularity, `utils.ts:90-99`).
fn clip_to_width(text: &str, width: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut used = 0usize;
    let mut chars = text.chars().peekable();
    let mut run = String::new();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Copy the escape sequence verbatim, flushing the plain run
            // before it so clusters never span a sequence boundary.
            let clipped = clip_run_to_width(&run, width.saturating_sub(used));
            used += visible_width(&clipped);
            out.push_str(&clipped);
            run.clear();
            out.push(c);
            if chars.peek() == Some(&'[') {
                out.push(chars.next().unwrap());
                for c in chars.by_ref() {
                    out.push(c);
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') || chars.peek() == Some(&'_') {
                out.push(chars.next().unwrap());
                let mut prev = '\0';
                for c in chars.by_ref() {
                    out.push(c);
                    if c == '\x07' || (prev == '\x1b' && c == '\\') {
                        break;
                    }
                    prev = c;
                }
            }
            continue;
        }
        run.push(c);
    }
    let clipped = clip_run_to_width(&run, width.saturating_sub(used));
    out.push_str(&clipped);
    out
}

/// Clip one ANSI-free run to `width` cells at cluster granularity.
fn clip_run_to_width(run: &str, width: usize) -> String {
    if run.is_empty() {
        return String::new();
    }
    if visible_width(run) <= width {
        return run.to_string();
    }
    let mut out = String::with_capacity(run.len());
    let mut used = 0usize;
    // Iterate the clusters directly instead of `grapheme_clusters`' throwaway
    // `Vec<&str>`: the prefix that fits is consumed once, in order, and the
    // cut-off cluster is never looked at again.
    for cluster in UnicodeSegmentation::graphemes(run, true) {
        let cw = grapheme_width(cluster);
        if used + cw > width {
            break;
        }
        used += cw;
        out.push_str(cluster);
    }
    out
}

/// Word-wrap `text` to `width` visible cells, preserving ANSI state across
/// lines (a style opened on line 1 continues on line 2). Long tokens break
/// at the cell boundary (upstream `wrapTextWithAnsi`).
pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut result: Vec<String> = Vec::new();
    // One tracker for the whole input: every wrapped line is prefixed with
    // the codes still active from the previous input lines, then the line's
    // own codes are applied on top of that state (upstream `wrapTextWithAnsi`
    // accumulates — a plain `\x1b[31mred\ntwo\nthree` keeps the color on all
    // three lines).
    let mut tracker = AnsiCodeTracker::default();
    for input_line in split_lines(text) {
        let mut line = if result.is_empty() {
            String::new()
        } else {
            tracker.get_active_codes()
        };
        line.push_str(input_line);
        result.extend(wrap_single_line(&line, width));
        // Feed the tracker from the original line; the prefix above is a
        // replay of state that is already tracked.
        process_tracker_from_text(input_line, &mut tracker);
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

/// Wrap a line that is pure printable ASCII (`0x20..=0x7e`) with `textwrap`.
///
/// This is the only slice of the wrapper where the crate's model is provably
/// the same as the hand-rolled loop below: every character is one cell and
/// one grapheme cluster, so `unicode-width`'s per-`char` measurement equals
/// [`visible_width`], and `AsciiSpace` splits exactly on the space runs the
/// tokenizer sees. `FirstFit` + `break_words` is the same greedy fit (break
/// before the word that no longer fits; break an over-wide word at the cell
/// boundary without a hyphen) and, like the caller, `textwrap` drops the
/// trailing whitespace of every produced line.
///
/// Everything else keeps the ANSI-aware machinery: `textwrap` measures per
/// `char`, not per UAX #29 cluster (it would split ZWJ emoji, flag pairs and
/// keycaps), treats `\t` as a zero-width word character instead of the 3 cells
/// upstream gives it, and its `UnicodeBreakProperties`/`HyphenSplitter`
/// introduce break opportunities (CJK, after `-`, other UAX #14 points) that
/// upstream's space-run tokenizer does not have. `NoHyphenation` removes the
/// hyphen opportunity for the ASCII path too.
fn wrap_plain_ascii_line(line: &str, width: usize) -> Option<Vec<String>> {
    if width == 0 || !line.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        return None;
    }
    let options = textwrap::Options::new(width)
        .word_separator(textwrap::WordSeparator::AsciiSpace)
        .word_splitter(textwrap::WordSplitter::NoHyphenation)
        .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit);
    Some(
        textwrap::wrap(line, options)
            .into_iter()
            .map(|line| line.into_owned())
            .collect(),
    )
}

/// Wrap one physical line (its replay prefix already glued on) to `width`
/// cells (upstream `wrapSingleLine`).
fn wrap_single_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    if visible_width(line) <= width {
        return vec![line.to_string()];
    }
    if let Some(wrapped) = wrap_plain_ascii_line(line, width) {
        return wrapped;
    }
    let mut wrapped: Vec<String> = Vec::new();
    let mut tracker = AnsiCodeTracker::default();
    let tokens = split_into_tokens_with_ansi(line);
    let mut current_line = String::new();
    let mut current_width = 0usize;

    for token in &tokens {
        let token_width = visible_width(token);
        let is_ws = !token.is_empty() && token.trim().is_empty();
        if token_width > width && !is_ws {
            // The token alone overflows: close the pending line, then break
            // the token cell by cell. The already-open codes stay attached
            // to the token and `break_long_word` re-opens them on every
            // piece, so no piece is left with a dangling opener.
            if !current_line.is_empty() {
                current_line.push_str(&tracker.get_line_end_reset());
                wrapped.push(std::mem::take(&mut current_line));
            }
            let mut broken = break_long_word(token, width, &mut tracker);
            let last = broken.pop().unwrap_or_default();
            wrapped.extend(broken);
            current_line = last;
            current_width = visible_width(&current_line);
            continue;
        }
        if current_width + token_width > width && current_width > 0 {
            // Trim the pending line in place: `trim_end` is always a prefix,
            // so `truncate` writes the bytes the old `trim_end().to_string()`
            // rebuilt (the buffer is handed to `wrapped` right after and
            // `current_line` is overwritten below, so nothing else observes
            // the untrimmed tail).
            let mut line_to_wrap = std::mem::take(&mut current_line);
            let trimmed_len = line_to_wrap.trim_end().len();
            if trimmed_len != line_to_wrap.len() {
                line_to_wrap.truncate(trimmed_len);
            }
            line_to_wrap.push_str(&tracker.get_line_end_reset());
            wrapped.push(line_to_wrap);
            current_line = tracker.get_active_codes();
            if is_ws {
                // Don't start a new line with whitespace.
                current_width = 0;
            } else {
                current_line.push_str(token);
                current_width = token_width;
            }
        } else {
            current_line.push_str(token);
            current_width += token_width;
        }
        process_tracker_from_text(token, &mut tracker);
    }
    if !current_line.is_empty() {
        // No reset on the final line — the caller owns the continuation.
        wrapped.push(current_line);
    }
    if wrapped.is_empty() {
        return vec![String::new()];
    }
    // Trim the trailing whitespace of every produced line in place: the old
    // `iter().map(|line| line.trim_end().to_string()).collect()` rebuilt a
    // fresh `String` (and a fresh `Vec`) for lines that are almost always
    // already trimmed. `trim_end` is a prefix, so `truncate` yields
    // byte-identical lines without touching the allocation.
    for line in wrapped.iter_mut() {
        let trimmed_len = line.trim_end().len();
        if trimmed_len != line.len() {
            line.truncate(trimmed_len);
        }
    }
    wrapped
}

/// Break a too-wide token (which may embed SGR codes) into pieces of at
/// most `width` cells, re-opening the still-active codes on each piece
/// (upstream `breakLongWord`). Clusters are indivisible.
fn break_long_word(word: &str, width: usize, tracker: &mut AnsiCodeTracker) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current_line = tracker.get_active_codes();
    let mut current_width = 0usize;
    let mut pending = String::new();
    let mut chars = word.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if let Some(seq) = read_escape_sequence(&mut chars, c) {
                flush_broken_run(
                    &mut pending,
                    &mut current_line,
                    &mut current_width,
                    width,
                    &mut lines,
                    tracker,
                );
                current_line.push_str(&seq);
                tracker.process(&seq);
                continue;
            }
            // Lone ESC: zero-width control char, part of the visible run.
        }
        pending.push(c);
    }
    flush_broken_run(
        &mut pending,
        &mut current_line,
        &mut current_width,
        width,
        &mut lines,
        tracker,
    );
    if !current_line.is_empty() {
        lines.push(current_line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Append the pending ANSI-free run to the current output line at cluster
/// granularity, breaking to a new line (re-opening the active codes) when a
/// whole cluster no longer fits.
fn flush_broken_run(
    pending: &mut String,
    current_line: &mut String,
    current_width: &mut usize,
    width: usize,
    lines: &mut Vec<String>,
    tracker: &AnsiCodeTracker,
) {
    // Same lazy iteration as `clip_run_to_width`: every cluster is appended
    // once, in order, so the `grapheme_clusters` vec was pure overhead.
    for cluster in UnicodeSegmentation::graphemes(pending.as_str(), true) {
        let cw = grapheme_width(cluster);
        if *current_width + cw > width {
            current_line.push_str(&tracker.get_line_end_reset());
            lines.push(std::mem::take(current_line));
            *current_line = tracker.get_active_codes();
            *current_width = 0;
        }
        current_line.push_str(cluster);
        *current_width += cw;
    }
    pending.clear();
}

fn split_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                lines.push(&text[start..i]);
                start = i + 1;
            }
            b'\r' => {
                lines.push(&text[start..i]);
                if bytes.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    lines.push(&text[start..]);
    lines
}

/// The exact OSC 8 terminator of an active run: BEL (`\x07`), which the login
/// dialogs emit (some terminals only make BEL-terminated links clickable),
/// or ST (`ESC \`), which markdown's `hyperlink()` emits
/// (upstream `utils.ts:556-568`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Osc8Terminator {
    Bel,
    St,
}

impl Osc8Terminator {
    fn as_str(self) -> &'static str {
        match self {
            Self::Bel => "\x07",
            Self::St => "\x1b\\",
        }
    }
}

/// The OSC 8 hyperlink active at the current point of a line — upstream
/// `ActiveHyperlink` (`utils.ts:486-490`): the run's parameters (`id=…`),
/// its URL, and the EXACT terminator the input carried, so a re-opened run
/// is byte-identical to the original instead of switching terminators.
struct ActiveHyperlink {
    params: String,
    url: String,
    terminator: Osc8Terminator,
}

impl ActiveHyperlink {
    /// The run's open sequence — upstream `formatOsc8Hyperlink`
    /// (`utils.ts:512-514`).
    fn open(&self) -> String {
        format!(
            "\x1b]8;{};{}{}",
            self.params,
            self.url,
            self.terminator.as_str()
        )
    }

    /// The run's close sequence — upstream `formatOsc8Close`
    /// (`utils.ts:516-518`).
    fn close(&self) -> String {
        format!("\x1b]8;;{}", self.terminator.as_str())
    }
}

/// What one escape sequence says about the active OSC 8 hyperlink — the three
/// answers of upstream `parseOsc8Hyperlink` (`utils.ts:492-510`).
enum Osc8Code {
    /// Not an OSC 8 code at all (upstream `undefined`): SGR handling proceeds.
    NotOsc8,
    /// An OSC 8 code with an empty URL (upstream `null`): closes the active
    /// run.
    Close,
    /// A run open.
    Open(ActiveHyperlink),
}

/// Parse one escape sequence as an OSC 8 hyperlink (upstream
/// `parseOsc8Hyperlink`): only a TERMINATED string is a code at all
/// (upstream's `extractAnsiCode` never yields an unterminated OSC, so such
/// text must leave the tracked state untouched), the terminator selects the
/// run's terminator, and the FIRST `;` after `8;` splits the parameters from
/// the URL — which may itself contain `;` (`utils.ts:497-509`). A code with no
/// second `;` is not a hyperlink code (state untouched); a code whose URL is
/// empty closes the run.
fn parse_osc8_code(ansi_code: &str) -> Osc8Code {
    let Some(rest) = ansi_code.strip_prefix("\x1b]8;") else {
        return Osc8Code::NotOsc8;
    };
    let (body, terminator) = if let Some(body) = rest.strip_suffix('\x07') {
        (body, Osc8Terminator::Bel)
    } else if let Some(body) = rest.strip_suffix("\x1b\\") {
        (body, Osc8Terminator::St)
    } else {
        return Osc8Code::NotOsc8;
    };
    let Some((params, url)) = body.split_once(';') else {
        return Osc8Code::NotOsc8;
    };
    if url.is_empty() {
        return Osc8Code::Close;
    }
    Osc8Code::Open(ActiveHyperlink {
        params: params.to_string(),
        url: url.to_string(),
        terminator,
    })
}

/// Tracked SGR attribute state — a port of upstream `AnsiCodeTracker`
/// (`utils.ts`). Colors are stored as their *full* parameter group (`31`,
/// `38;5;240`, `38;2;r;g;b`) and never split into single numbers; replay
/// emits a canonical merge (`\x1b[1;38;5;240m`). An active OSC 8 hyperlink is
/// tracked alongside the SGR state so a run that spans a wrap is closed before
/// the line break and re-opened at the start of the next line with the same
/// URL, parameters and terminator (upstream `activeHyperlink`,
/// `utils.ts:557`/`:727-728`/`:764-765`).
#[derive(Default)]
struct AnsiCodeTracker {
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    blink: bool,
    inverse: bool,
    hidden: bool,
    strikethrough: bool,
    /// Full code like `31` or `38;5;240` (upstream `fgColor`).
    fg_color: Option<String>,
    /// Full code like `41` or `48;2;r;g;b` (upstream `bgColor`).
    bg_color: Option<String>,
    /// The OSC 8 run active at this point (upstream `activeHyperlink`).
    active_hyperlink: Option<ActiveHyperlink>,
}

impl AnsiCodeTracker {
    /// Apply one escape sequence to the tracked state (upstream `process`).
    /// OSC 8 hyperlinks are tracked here too; only SGR (`\x1b[…m`) affects the
    /// SGR state and every other sequence is ignored.
    fn process(&mut self, ansi_code: &str) {
        match parse_osc8_code(ansi_code) {
            Osc8Code::Open(hyperlink) => {
                self.active_hyperlink = Some(hyperlink);
                return;
            }
            Osc8Code::Close => {
                self.active_hyperlink = None;
                return;
            }
            Osc8Code::NotOsc8 => {}
        }
        let Some(params) = ansi_code
            .strip_prefix("\x1b[")
            .and_then(|rest| rest.strip_suffix('m'))
        else {
            return;
        };
        if params.is_empty() || params == "0" {
            self.reset_sgr();
            return;
        }
        let parts: Vec<&str> = params.split(';').collect();
        let mut i = 0;
        while i < parts.len() {
            let Ok(code) = parts[i].parse::<i64>() else {
                i += 1;
                continue;
            };
            // 256-color / RGB groups consume several parameters; store them
            // whole so replay can never split `38;5;N` into stray codes.
            if code == 38 || code == 48 {
                if parts.get(i + 1) == Some(&"5") && parts.get(i + 2).is_some() {
                    let color = format!("{};5;{}", code, parts[i + 2]);
                    if code == 38 {
                        self.fg_color = Some(color);
                    } else {
                        self.bg_color = Some(color);
                    }
                    i += 3;
                    continue;
                } else if parts.get(i + 1) == Some(&"2") && parts.get(i + 4).is_some() {
                    let color = format!(
                        "{};2;{};{};{}",
                        code,
                        parts[i + 2],
                        parts[i + 3],
                        parts[i + 4]
                    );
                    if code == 38 {
                        self.fg_color = Some(color);
                    } else {
                        self.bg_color = Some(color);
                    }
                    i += 5;
                    continue;
                }
            }
            match code {
                0 => self.reset_sgr(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                5 => self.blink = true,
                7 => self.inverse = true,
                8 => self.hidden = true,
                9 => self.strikethrough = true,
                21 => self.bold = false, // bold-off on some terminals
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                25 => self.blink = false,
                27 => self.inverse = false,
                28 => self.hidden = false,
                29 => self.strikethrough = false,
                39 => self.fg_color = None, // default foreground
                49 => self.bg_color = None, // default background
                _ => {
                    // 30-37 / 90-97 foreground, 40-47 / 100-107 background.
                    if (30..=37).contains(&code) || (90..=97).contains(&code) {
                        self.fg_color = Some(code.to_string());
                    } else if (40..=47).contains(&code) || (100..=107).contains(&code) {
                        self.bg_color = Some(code.to_string());
                    }
                }
            }
            i += 1;
        }
    }

    /// Reset every SGR attribute (upstream `reset`). An SGR reset does NOT
    /// affect the active OSC 8 hyperlink (`utils.ts:693-705`).
    fn reset_sgr(&mut self) {
        self.bold = false;
        self.dim = false;
        self.italic = false;
        self.underline = false;
        self.blink = false;
        self.inverse = false;
        self.hidden = false;
        self.strikethrough = false;
        self.fg_color = None;
        self.bg_color = None;
    }

    /// The merged reopen sequence for everything still active, or `""`
    /// (upstream `getActiveCodes`). The SGR merge comes first and the active
    /// OSC 8 open — parameters, URL and original terminator — is appended
    /// after it (`utils.ts:713-731`).
    fn get_active_codes(&self) -> String {
        let mut codes: Vec<String> = Vec::new();
        if self.bold {
            codes.push("1".to_string());
        }
        if self.dim {
            codes.push("2".to_string());
        }
        if self.italic {
            codes.push("3".to_string());
        }
        if self.underline {
            codes.push("4".to_string());
        }
        if self.blink {
            codes.push("5".to_string());
        }
        if self.inverse {
            codes.push("7".to_string());
        }
        if self.hidden {
            codes.push("8".to_string());
        }
        if self.strikethrough {
            codes.push("9".to_string());
        }
        if let Some(fg) = &self.fg_color {
            codes.push(fg.clone());
        }
        if let Some(bg) = &self.bg_color {
            codes.push(bg.clone());
        }
        let mut result = if codes.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", codes.join(";"))
        };
        if let Some(hyperlink) = &self.active_hyperlink {
            result.push_str(&hyperlink.open());
        }
        result
    }

    /// Codes that must be closed at a line end so they cannot bleed into the
    /// padding: underline only — colors/backgrounds may continue and are
    /// re-opened by the next line (upstream `getLineEndReset`) — plus the OSC 8
    /// close of a still-open hyperlink, which the next line re-opens through
    /// [`Self::get_active_codes`]. The close carries the run's own terminator,
    /// so the signature is `String`, not `&'static str`
    /// (`utils.ts:759-768`).
    fn get_line_end_reset(&self) -> String {
        let mut result = String::new();
        if self.underline {
            result.push_str("\x1b[24m");
        }
        if let Some(hyperlink) = &self.active_hyperlink {
            result.push_str(&hyperlink.close());
        }
        result
    }
}

/// Split `text` into word / space runs with pending ANSI sequences attached
/// to the visible run that follows them, so a broken run keeps its style
/// prefix inside the same token (upstream `splitIntoTokensWithAnsi`). Space
/// runs cover ASCII space and tab (the codebase normalizes tabs upstream of
/// the wrapper).
fn split_into_tokens_with_ansi(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut pending_ansi = String::new();
    let mut current_kind: Option<bool> = None; // Some(true): space run, Some(false): word run.

    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if let Some(seq) = read_escape_sequence(&mut chars, c) {
                pending_ansi.push_str(&seq);
                continue;
            }
            // Lone ESC falls through as a zero-width word char.
        }
        let is_space = c == ' ' || c == '\t';
        if !current.is_empty() && current_kind != Some(is_space) {
            tokens.push(std::mem::take(&mut current));
        }
        if !pending_ansi.is_empty() {
            current.push_str(&pending_ansi);
            pending_ansi.clear();
        }
        current_kind = Some(is_space);
        current.push(c);
    }
    // Trailing codes with nothing to attach to: extend the last token, or
    // stand alone when the line is codes only.
    if !pending_ansi.is_empty() {
        if !current.is_empty() {
            current.push_str(&pending_ansi);
        } else if let Some(last) = tokens.last_mut() {
            last.push_str(&pending_ansi);
        } else {
            current = pending_ansi;
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Feed every SGR sequence found in `text` into `tracker` (upstream
/// `updateTrackerFromText`).
fn process_tracker_from_text(text: &str, tracker: &mut AnsiCodeTracker) {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if let Some(seq) = read_escape_sequence(&mut chars, c) {
                tracker.process(&seq);
            }
        }
    }
}

/// Consume one complete escape sequence starting at `lead` (the already-read
/// `ESC`) and return it verbatim; `None` for a lone `ESC`.
fn read_escape_sequence(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    lead: char,
) -> Option<String> {
    let mut seq = String::new();
    seq.push(lead);
    match chars.peek() {
        Some('[') => {
            seq.push(chars.next().unwrap());
            for c in chars.by_ref() {
                seq.push(c);
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
            Some(seq)
        }
        Some(']') | Some('_') => {
            seq.push(chars.next().unwrap());
            let mut prev = '\0';
            for c in chars.by_ref() {
                seq.push(c);
                if c == '\x07' || (prev == '\x1b' && c == '\\') {
                    break;
                }
                prev = c;
            }
            Some(seq)
        }
        _ => None,
    }
}

/// Replace visible-cell columns `[start, start + width)` of an ANSI line with
/// `replacement` (already cut/padded to `width` cells), preserving the SGR
/// state active on both sides. Used by the floating-overlay compositor: the
/// rows under the overlay keep their own styling outside the replaced band,
/// and the overlay's styling never bleeds past it.
///
/// Wide clusters are never split: a cluster straddling the left boundary stays
/// in the prefix (the replacement starts at the next whole cluster).
pub fn replace_columns(line: &str, start: usize, width: usize, replacement: &str) -> String {
    if width == 0 {
        return line.to_string();
    }
    let end = start.saturating_add(width);
    let mut out = String::with_capacity(line.len() + replacement.len() + 16);
    let mut col = 0usize;
    let mut inserted = false;
    let mut resumed = false;
    let mut active_codes: Vec<String> = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            let Some(seq) = read_escape_sequence(&mut chars, c) else {
                continue;
            };
            if seq.ends_with('m') && seq.len() > 2 {
                active_codes.push(seq.clone());
            }
            // Escape sequences before the band and after it stay in the
            // output; ones inside it only update the active SGR state so the
            // suffix can be re-opened after the replacement.
            if !inserted && col < start {
                out.push_str(&seq);
            } else if resumed {
                out.push_str(&seq);
            }
            continue;
        }
        let mut run = String::new();
        run.push(c);
        while let Some(&next) = chars.peek() {
            if next == '\x1b' {
                break;
            }
            run.push(next);
            chars.next();
        }
        for cluster in UnicodeSegmentation::graphemes(run.as_str(), true) {
            let cluster_width = if cluster == "\t" {
                3
            } else {
                grapheme_width(cluster)
            };
            if !inserted && col + cluster_width > start && col < start {
                // A wide cluster hits the band's left edge: keep it whole in
                // the prefix and replace from the next cluster.
                out.push_str(cluster);
                col += cluster_width;
                continue;
            }
            if !inserted && col >= start {
                let pad = start.saturating_sub(col);
                for _ in 0..pad {
                    out.push(' ');
                }
                out.push_str("\x1b[0m");
                out.push_str(replacement);
                out.push_str("\x1b[0m");
                for code in &active_codes {
                    out.push_str(code);
                }
                inserted = true;
            }
            if col >= end {
                if inserted && !resumed {
                    out.push_str("\x1b[0m");
                    for code in &active_codes {
                        out.push_str(code);
                    }
                    resumed = true;
                }
                out.push_str(cluster);
            } else if col < start {
                out.push_str(cluster);
            }
            // Inside the band: replaced, do not copy.
            col += cluster_width;
        }
    }
    if !inserted {
        // The line ended before the band: pad and place the replacement at a
        // padded tail so a short frame row still hosts its overlay.
        let pad = start.saturating_sub(col);
        for _ in 0..pad {
            out.push(' ');
        }
        out.push_str("\x1b[0m");
        out.push_str(replacement);
    }
    out
}

/// Pad `line` with spaces to exactly `width` visible cells, then apply
/// `bg` over the whole padded line (upstream `applyBackgroundToLine`).
pub fn apply_background_to_line(line: &str, width: usize, bg: &dyn Fn(&str) -> String) -> String {
    let visible = visible_width(line);
    let padding = width.saturating_sub(visible);
    // One allocation for the padded copy (`with_capacity`) plus the spaces
    // written in place: the old `line.to_string()` + `push_str` grew the
    // buffer twice and materialized the padding as a throwaway `" ".repeat`.
    let mut padded = String::with_capacity(line.len() + padding);
    padded.push_str(line);
    for _ in 0..padding {
        padded.push(' ');
    }
    bg(&padded)
}

/// Pad to `width` with spaces (visible-width aware).
pub fn pad_to_width(line: &str, width: usize) -> String {
    let pad = width.saturating_sub(visible_width(line));
    // Same treatment as `apply_background_to_line`: a single allocation of
    // the final length instead of a right-sized copy that reallocs on the
    // padding and a temporary `" ".repeat(pad)` buffer.
    let mut out = String::with_capacity(line.len() + pad);
    out.push_str(line);
    for _ in 0..pad {
        out.push(' ');
    }
    out
}

/// `prefix` + `text`, padded to `width` visible cells with spaces — exactly
/// what `pad_to_width(&format!("{prefix}{text}"), width)` produces. The
/// concatenation is built once into the output buffer instead of a throwaway
/// formatted `String` that `pad_to_width` copies a second time, and the width
/// is still measured on the identical `prefix + text` bytes, so the padding
/// count cannot differ.
pub fn pad_prefixed_to_width(prefix: &str, text: &str, width: usize) -> String {
    let mut out = String::with_capacity(prefix.len() + text.len() + width);
    out.push_str(prefix);
    out.push_str(text);
    let pad = width.saturating_sub(visible_width(&out));
    for _ in 0..pad {
        out.push(' ');
    }
    out
}

/// Strip ANSI escape sequences (used by tests and width calculations that
/// need plain text).
///
/// The implementation lives in [`crate::text`], which core surfaces without
/// the TUI (the HTML export) use too.
pub fn strip_ansi(text: &str) -> String {
    crate::text::strip_ansi(text)
}
