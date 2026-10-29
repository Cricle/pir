//! Incremental completeness-gate vectors for the embedded shell's stdin loop.
//!
//! The gate is private to `src/shell_embed`; the crate's hygiene rule keeps
//! inline test modules out of `src/`, so this suite drives the doc-hidden
//! test seam. The gate only decides whether a whole-buffer parse can be
//! skipped; the parser remains authoritative (the end-to-end stdin tests in
//! `brush_embed.rs` cover execution).

#![cfg(feature = "brush")]

use pir::shell_embed::{
    completeness_gate_is_incomplete_for_test as incomplete,
    keyword_terminator_for_test as terminator,
};

fn assert_incomplete(lines: &[&str]) {
    assert!(incomplete(lines), "expected incomplete: {lines:?}");
}

fn assert_complete(lines: &[&str]) {
    assert!(!incomplete(lines), "expected complete: {lines:?}");
}

#[test]
fn plain_commands_are_complete() {
    assert_complete(&["echo hi\n"]);
}

#[test]
fn open_quotes_and_escapes_are_incomplete() {
    assert_incomplete(&["echo 'unterminated\n"]);
    assert_complete(&["echo 'unterminated\n", "still here'\n"]);
    assert_incomplete(&["echo \"a)\n"]);
    assert_complete(&["echo \"a)\"\n"]);
    assert_incomplete(&["echo a \\\n"]);
    assert_complete(&["echo a \\\n", "b\n"]);
}

#[test]
fn trailing_operators_are_incomplete() {
    assert_incomplete(&["echo a |\n"]);
    assert_incomplete(&["echo a &&\n"]);
    assert_complete(&["echo a |\n", "cat\n"]);
}

#[test]
fn parens_braces_and_compound_blocks_are_tracked() {
    assert_incomplete(&["(\n", "echo hi\n"]);
    assert_complete(&["(\n", "echo hi\n", ") > out\n"]);
    assert_incomplete(&["if true; then\n", "echo hi\n"]);
    assert_complete(&["if true; then\n", "echo hi\n", "fi\n"]);
    assert_incomplete(&["for x in a b; do\n", "echo $x\n"]);
    assert_complete(&["for x in a b; do\n", "echo $x\n", "done\n"]);
    assert_incomplete(&["foo() {\n", "echo hi\n"]);
    assert_complete(&["foo() {\n", "echo hi\n", "}\n"]);
    assert_incomplete(&["function foo {\n", "echo hi\n"]);
    assert_complete(&["function foo {\n", "echo hi\n", "}\n"]);
}

#[test]
fn case_pattern_parens_do_not_close_the_unit() {
    let lines = [
        "case $x in\n",
        "  a) echo a ;;\n",
        "  (b|c) echo b ;;\n",
        "esac\n",
    ];
    assert_complete(&lines);
    assert_incomplete(&lines[..2]);
}

#[test]
fn heredoc_body_is_opaque_until_its_delimiter() {
    let lines = [
        "(\n",
        "cat <<'EOF'\n",
        ")\n", // a body line that would close the wrapper if scanned
        "EOF\n",
        ") > out\n",
    ];
    assert_incomplete(&lines[..3]);
    assert_incomplete(&lines[..4]);
    assert_complete(&lines);
}

#[test]
fn standalone_units_complete_incrementally() {
    assert_complete(&["echo ${HOME}/x\n"]);
    assert_complete(&["echo $((1 << 2))\n"]);
    assert_complete(&["[[ -n $x ]]\n"]);
    assert_complete(&["echo hi # ) comment\n"]);
}

#[test]
fn keyword_terminator_lines_trigger_a_parse() {
    assert!(terminator("fi\n"));
    assert!(terminator("} > /dev/null\n"));
    assert!(!terminator("echo done\n"));
}
