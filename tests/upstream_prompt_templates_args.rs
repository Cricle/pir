//! Regression tests for upstream drift commits whose fixes have no local test.
//!
//! `9fe3f3716` — empty quoted prompt-template arguments must be kept, so later
//! positional arguments do not shift and `${1:-default}` still applies when
//! the first argument is explicitly empty.

use std::path::PathBuf;

use pir::coding_agent::prompt_templates::{
    expand_prompt_template, parse_command_args, PromptTemplate,
};

fn review_template() -> PromptTemplate {
    PromptTemplate {
        name: "review".to_string(),
        description: "Review a change.".to_string(),
        argument_hint: Some("<focus> <path>".to_string()),
        content: "Review $2. Focus: ${1:-correctness}.".to_string(),
        file_path: PathBuf::from("/tmp/review.md"),
    }
}

#[test]
fn keeps_empty_double_quoted_argument() {
    assert_eq!(parse_command_args("\"\" \" \""), vec!["", " "]);
}

#[test]
fn keeps_empty_single_quoted_argument_between_values() {
    assert_eq!(parse_command_args("a '' c"), vec!["a", "", "c"]);
}

#[test]
fn keeps_trailing_empty_argument() {
    assert_eq!(parse_command_args("a \"\""), vec!["a", ""]);
}

/// `96667f342` #11873: a quote only opens a quoted argument at the start of an
/// argument, so apostrophes inside words stay literal (upstream
/// `prompt-templates.ts:44-47`).
#[test]
fn keeps_apostrophes_inside_words() {
    assert_eq!(
        parse_command_args("why doesn't the user's cache work"),
        vec!["why", "doesn't", "the", "user's", "cache", "work"]
    );
}

/// An unclosed quote is re-read as literal text from its opening position
/// (upstream `prompt-templates.ts:62-69`).
#[test]
fn unclosed_quote_is_literal_text() {
    assert_eq!(
        parse_command_args("'tis the season"),
        vec!["'tis", "the", "season"]
    );
}

/// Backslash before the open quote character yields a literal quote; other
/// backslashes stay as typed (upstream `prompt-templates.ts:38-42`).
#[test]
fn quoted_argument_supports_escaped_quotes_and_paths() {
    assert_eq!(
        parse_command_args("\"quoted \\\"text\\\"\""),
        vec!["quoted \"text\""]
    );
    assert_eq!(parse_command_args("'it\\'s here'"), vec!["it's here"]);
    assert_eq!(
        parse_command_args("\"C:\\dir\\file\""),
        vec!["C:\\dir\\file"]
    );
    assert_eq!(
        parse_command_args("'\\\\server\\share'"),
        vec!["\\\\server\\share"]
    );
    // A quoted argument ending in a backslash reads the final `\"` as an
    // escaped quote, so the quote never closes and the whole argument is kept
    // literally, quotes included (upstream changelog edge case).
    assert_eq!(parse_command_args("\"C:\\dir\\\""), vec!["\"C:\\dir\\\""]);
}

#[test]
fn empty_argument_does_not_shift_later_positional_arguments() {
    let template = review_template();

    assert_eq!(
        expand_prompt_template("/review \"\" src/index.ts", std::slice::from_ref(&template)),
        "Review src/index.ts. Focus: correctness."
    );
    assert_eq!(
        expand_prompt_template("/review '' src/index.ts", &[template]),
        "Review src/index.ts. Focus: correctness."
    );
}

/// PIR-BIN-068: upstream `extractFrontmatter` trims the body and normalizes
/// CRLF/CR to LF (frontmatter.ts:19-21), so a `/template` expansion carries no
/// leading/trailing whitespace and no carriage returns.
#[test]
fn template_body_is_trimmed_and_lf_normalized() {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("pir-template-body-{stamp}"));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("greet.md");
    std::fs::write(
        &path,
        "---\ndescription: Greet\nargument-hint: <name>\n---\n\nGREET-TEMPLATE-BODY $1 and $2\r\nSECOND-LINE\r\n",
    )
    .expect("write template");
    let template =
        pir::coding_agent::prompt_templates::load_template_file(&path).expect("load template");
    assert_eq!(
        template.content,
        "GREET-TEMPLATE-BODY $1 and $2\nSECOND-LINE"
    );
    std::fs::remove_dir_all(&dir).ok();
}
