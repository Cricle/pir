//! The bug-report upload body is bounded before it is assembled.
//!
//! The body is one in-memory multipart buffer, and `session.jsonl` is the whole
//! transcript, so an unbounded bundle meant an unbounded allocation. These
//! tests pin the limit and the pre-flight measurement that enforces it.

use pir_tui::bug_report::{
    bug_report_body_len, bug_report_files, BugReportBundle, BUG_REPORT_MAX_UPLOAD_BYTES,
};

fn bundle_with_transcript(bytes: usize) -> BugReportBundle {
    BugReportBundle {
        metadata: serde_json::json!({ "version": "test" }),
        diagnostics: serde_json::json!({ "failures": [] }),
        session_jsonl: Some("x".repeat(bytes)),
        hint: None,
        summary: None,
    }
}

#[test]
fn a_small_bundle_is_well_under_the_upload_limit() {
    let bundle = bundle_with_transcript(1024);
    let len = bug_report_body_len(&bundle);
    assert!(
        len < BUG_REPORT_MAX_UPLOAD_BYTES,
        "a 1 KiB transcript must fit: {len}"
    );
    // The measurement must include the transcript verbatim plus framing, so it
    // is at least the transcript length.
    assert!(len >= 1024, "the transcript must be counted: {len}");
}

#[test]
fn the_measurement_tracks_the_transcript_length() {
    let small = bug_report_body_len(&bundle_with_transcript(1_000));
    let large = bug_report_body_len(&bundle_with_transcript(100_000));
    assert!(
        large > small,
        "the estimate must grow with the transcript: {small} -> {large}"
    );
}

#[test]
fn an_oversized_transcript_exceeds_the_limit() {
    // One byte past the limit for the transcript alone: framing only makes it
    // larger, so this must be refused.
    let bundle = bundle_with_transcript(BUG_REPORT_MAX_UPLOAD_BYTES + 1);
    assert!(
        bug_report_body_len(&bundle) > BUG_REPORT_MAX_UPLOAD_BYTES,
        "an oversized transcript must measure over the limit"
    );
}

#[test]
fn excluding_the_transcript_stays_under_the_limit() {
    // The error message tells the user to retry without the transcript; that
    // variant must actually fit, or the advice is useless.
    let mut bundle = bundle_with_transcript(0);
    bundle.session_jsonl = None;
    let len = bug_report_body_len(&bundle);
    assert!(
        len < BUG_REPORT_MAX_UPLOAD_BYTES,
        "a transcript-free bundle must fit: {len}"
    );
}

#[test]
fn every_file_contributes_to_the_measurement() {
    let mut bundle = bundle_with_transcript(0);
    bundle.session_jsonl = None;
    let files = bug_report_files(&bundle);
    let data_total: usize = files.iter().map(|file| file.data.len()).sum();
    let measured = bug_report_body_len(&bundle);
    assert!(
        measured > data_total,
        "framing overhead must be counted: measured {measured}, data {data_total}"
    );
    // With no transcript the bundle is exactly report.json + diagnostics.json.
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].name, "report.json");
    assert_eq!(files[1].name, "diagnostics.json");

    // Attaching the transcript adds exactly one more part.
    let with_transcript = bundle_with_transcript(0);
    assert_eq!(bug_report_files(&with_transcript).len(), 3);
}
