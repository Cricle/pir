// Integration tests (kept out of src/).

use pir_builtin_context_mode::*;

#[test]
fn markdown_splits_at_headings_and_keeps_line_ranges() {
    let content = "# One\n\na\n\n# Two\n\nb\n";
    let chunks = chunk_document("doc.md", "markdown", content);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].title, "One");
    assert_eq!(chunks[1].title, "Two");
    assert_eq!(chunks[0].line_start, 1);
    assert_eq!(chunks[1].line_start, 5);
    assert!(chunks[0].content.contains('a'));
}

#[test]
fn fenced_headings_do_not_start_sections() {
    let content = "# One\n\n```\n# not a heading\n```\n\ntail\n";
    let chunks = chunk_document("doc.md", "markdown", content);
    assert_eq!(chunks.len(), 1, "the fenced # is payload");
    assert!(chunks[0].content.contains("# not a heading"));
}

#[test]
fn plain_text_chunks_at_blank_lines_and_takes_a_title() {
    let content = "first line\n\nsecond paragraph\n";
    let chunks = chunk_document("notes.txt", "text", content);
    assert_eq!(chunks.len(), 2);
    assert_eq!(chunks[0].title, "first line");
    // A split section keeps the same title with a "(part N)" suffix.
    assert_eq!(chunks[1].title, "first line (part 2)");
}

#[test]
fn empty_content_yields_no_chunks() {
    assert!(chunk_document("x", "markdown", "   \n\n ").is_empty());
}

#[test]
fn oversized_sections_are_split_with_part_suffixes() {
    let mut content = String::from("# Big\n\n");
    for index in 0..1200 {
        content.push_str(&format!("line {index}\n"));
        if index % 20 == 0 {
            content.push('\n');
        }
    }
    let chunks = chunk_document("big.md", "markdown", &content);
    assert!(chunks.len() > 1);
    assert!(chunks[1].title.contains("(part 2)"));
}

#[test]
fn from_text_wraps_chunks() {
    let doc = IndexDoc::from_text("s".into(), "markdown".into(), "# H\n\nx\n");
    assert_eq!(doc.chunks.len(), 1);
    assert_eq!(doc.source, "s");
}
