//! The theme file read is bounded: an oversized document is rejected instead
//! of being read whole into memory.
//!
//! Covers the watcher's reload path (both it and the startup registry loader
//! share `theme_json::read_theme_file`).

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use pir_tui::theme::reload_theme_file;

/// Every color token the schema requires (`Theme::key` outputs minus the five
/// optional ones). Kept as a literal so a new required token makes the fixture
/// visibly incomplete rather than silently wrong.
const REQUIRED_COLOR_KEYS: &[&str] = &[
    "accent",
    "border",
    "borderAccent",
    "borderMuted",
    "success",
    "error",
    "warning",
    "muted",
    "dim",
    "text",
    "thinkingText",
    "selectedBg",
    "userMessageBg",
    "userMessageText",
    "customMessageBg",
    "customMessageText",
    "customMessageLabel",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
    "toolTitle",
    "toolOutput",
    "mdHeading",
    "mdLink",
    "mdLinkUrl",
    "mdCode",
    "mdCodeBlock",
    "mdCodeBlockBorder",
    "mdQuote",
    "mdQuoteBorder",
    "mdHr",
    "mdListBullet",
    "toolDiffAdded",
    "toolDiffRemoved",
    "toolDiffContext",
    "syntaxComment",
    "syntaxKeyword",
    "syntaxFunction",
    "syntaxVariable",
    "syntaxString",
    "syntaxNumber",
    "syntaxType",
    "syntaxOperator",
    "syntaxPunctuation",
    "thinkingOff",
    "thinkingMinimal",
    "thinkingLow",
    "thinkingMedium",
    "thinkingHigh",
    "thinkingXhigh",
    "bashMode",
];

fn temp_dir(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let dir =
        std::env::temp_dir().join(format!("pir-theme-{tag}-{}-{}", std::process::id(), stamp));
    std::fs::create_dir_all(&dir).expect("fixture dir");
    dir
}

/// A complete, valid theme document. `pad` inflates the allowed `vars` block
/// (an unknown top-level field would be rejected by the schema instead, which
/// would defeat the purpose of the padding).
fn valid_theme(name: &str, pad: usize) -> String {
    let colors = REQUIRED_COLOR_KEYS
        .iter()
        .map(|key| format!("\"{key}\":\"#808080\""))
        .collect::<Vec<_>>()
        .join(",");
    let padding = " ".repeat(pad);
    format!(
        r#"{{"name":"{name}","appearance":"dark","colors":{{{colors}}},"vars":{{"pad":"{padding}"}}}}"#
    )
}

#[test]
fn an_oversized_theme_file_is_rejected_without_being_parsed() {
    let dir = temp_dir("oversized");
    let path = dir.join("big.json");

    // Comfortably past the documented 1 MiB ceiling but otherwise valid: if
    // the read were unbounded, this would parse fine and the test would fail
    // for the wrong reason.
    std::fs::write(&path, valid_theme("huge", 2 * 1024 * 1024)).expect("write oversized fixture");

    let error = reload_theme_file(&path).expect_err("an oversized theme must be rejected");
    assert!(
        error.contains("limit"),
        "the error must name the size limit, got: {error}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_normal_theme_file_still_reloads() {
    let dir = temp_dir("normal");
    let path = dir.join("ok.json");
    std::fs::write(&path, valid_theme("checked", 0)).expect("write fixture");

    // `reload_theme_file` swaps the global theme, so a success here also proves
    // the bounded read did not break the happy path.
    let name = reload_theme_file(&path).expect("a normal theme reloads");
    assert_eq!(name, "checked");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_theme_at_the_ceiling_is_still_accepted() {
    let dir = temp_dir("ceiling");
    let path = dir.join("edge.json");

    // Hedge: pad the document to just under the 1 MiB limit.
    let base = valid_theme("edge", 0).len();
    let pad = 1_048_576_usize.saturating_sub(base).saturating_sub(16);
    std::fs::write(&path, valid_theme("edge", pad)).expect("write ceiling fixture");
    let size = std::fs::metadata(&path).expect("stat").len();
    assert!(
        size <= 1_048_576,
        "fixture must sit at or under the ceiling: {size}"
    );

    let name = reload_theme_file(&path).expect("a file at the ceiling still reloads");
    assert_eq!(name, "edge");

    let _ = std::fs::remove_dir_all(&dir);
}
