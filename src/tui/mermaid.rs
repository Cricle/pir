//! Mermaid → Unicode box-drawing renderer, backed by the [`mermaid-text`]
//! crate. This replaces a hand-written Rust port of the upstream
//! `grok-mermaid` npm subset (the chat surface's
//! `modes/interactive/components/mermaid.ts` calls `render(src)` from
//! `grok-mermaid`): the crate plays the same role — parse Mermaid source and
//! draw it as terminal box-drawing art — but with ecosystem coverage
//! (flowchart `graph`/`flowchart` in all four directions, the full
//! node-shape and edge-label grammar, subgraphs, plus sequence/pie/class/
//! state/ER and more, where the port only handled the flowchart subset and
//! bailed out on everything else).
//!
//! Engine results are mapped onto the upstream decision tiers:
//! - `Ok(rows)` → [`MermaidOutcome::Diagram`]; the markdown pipeline then
//!   applies the viewport-width cut and the off/final/streaming mode.
//! - [`mermaid_text::Error::ParseError`] (an unreadable diagram header) →
//!   [`MermaidOutcome::NotRendered`] carrying the diagnostic, which the
//!   markdown pipeline reports as `Mermaid diagram not rendered: …` under
//!   the raw fence for settled (`final`) renders.
//! - [`mermaid_text::Error::UnsupportedDiagram`] /
//!   [`mermaid_text::Error::EmptyInput`] → [`MermaidOutcome::NotRendered`]
//!   without a message: keep the raw code block, no advisory.
//!
//! Unlike the port, the engine never renders a partial prefix with
//! per-statement warnings (its flowchart parser is lenient — unreadable
//! statements degrade in place — and header failures fail whole), so there
//! is no `warnings` list on [`MermaidArt`] and the old `(+N more)` advisory
//! suffix has no producer.

use unicode_width::UnicodeWidthStr;

/// A rendered diagram: right-trimmed box-drawing rows and the display width
/// of the widest row, measured with `unicode-width` (the same measurement
/// the markdown pipeline uses for its viewport cut).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MermaidArt {
    /// The diagram rows, right-trimmed.
    pub plain: Vec<String>,
    /// Display width of the widest row (unicode-width measure).
    pub width: usize,
}

/// The outcome of a render attempt, mirroring the upstream decision tiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MermaidOutcome {
    /// A rendered diagram.
    Diagram(MermaidArt),
    /// No art to show; `Some(message)` is an advisory diagnostic for the raw
    /// fence (`None` for unsupported or blank input, which stay silent).
    NotRendered(Option<String>),
}

/// Render a Mermaid source as Unicode box-drawing art with the
/// `mermaid-text` engine. Unsupported diagram kinds and blank input yield
/// [`MermaidOutcome::NotRendered`] so the markdown pipeline keeps the raw
/// code block (upstream `render` → null).
pub fn render(src: &str) -> MermaidOutcome {
    let src = src.trim();
    match mermaid_text::render(src) {
        Ok(text) => {
            let plain: Vec<String> = text.lines().map(|row| row.trim_end().to_owned()).collect();
            let width = plain
                .iter()
                .map(|row| UnicodeWidthStr::width(row.as_str()))
                .max()
                .unwrap_or(0);
            MermaidOutcome::Diagram(MermaidArt { plain, width })
        }
        Err(err) => {
            // Only an engine parse failure carries a human-readable reason;
            // unsupported kinds and blank input keep the fence silently.
            let advisory = match &err {
                mermaid_text::Error::ParseError(_) => Some(err.to_string()),
                _ => None,
            };
            MermaidOutcome::NotRendered(advisory)
        }
    }
}
