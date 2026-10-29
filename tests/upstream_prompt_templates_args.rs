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
