//! D2a: task sizing is a regression-tested pure function.
//!
//! The rows below are the `RESULTS.md` fixtures (`s1-duration`, `s2-slugify`,
//! `l1-ledger`, `c3-two-turn`) plus the boundary cases each threshold needs.
//! The failing case this table exists for is `c3-two-turn`: 24,954 tokens over
//! 9 turns, cheaper than a single-file fix by volume but a plan→implement task
//! by shape. A rule that ranks on tokens alone classifies it Small.

use pir_builtin_mode::size::{size_task, PromptShape, TaskInputs, TaskSize};

fn task(named: &[&str], modules: usize, shape: PromptShape) -> TaskInputs {
    TaskInputs {
        named_files: named.iter().map(|file| (*file).to_owned()).collect(),
        touched_modules: modules,
        prompt_shape: shape,
        failed_attempts: None,
    }
}

fn shape(deliverables: usize, sequential: bool) -> PromptShape {
    PromptShape {
        deliverables,
        sequential,
    }
}

#[test]
fn the_recorded_fixtures_land_in_the_expected_lane() {
    // s1-duration: one file, one sentence, 12,523 tokens, 5 turns.
    assert_eq!(
        size_task(&task(&["src/duration.rs"], 6, PromptShape::default())),
        TaskSize::Small,
        "s1-duration is a one-file fix"
    );
    // s2-slugify: same shape, 16,435 tokens — still one deliverable.
    assert_eq!(
        size_task(&task(&["src/slug.rs"], 6, PromptShape::default())),
        TaskSize::Small,
        "s2-slugify is a one-file fix"
    );
    // c3-two-turn: 24,954 tokens but two ordered steps. Token volume says
    // Small; the shape says Medium, and the shape is right.
    assert_eq!(
        size_task(&task(&[], 6, shape(2, true))),
        TaskSize::Medium,
        "c3-two-turn is cheap in tokens but multi-step by shape"
    );
    // l1-ledger: 33 tests over 9 modules — many deliverables.
    assert_eq!(
        size_task(&task(&[], 9, shape(6, true))),
        TaskSize::Large,
        "l1-ledger is a multi-module ledger repair"
    );
}

#[test]
fn named_file_count_drives_the_medium_boundary() {
    assert_eq!(
        size_task(&task(&["a.rs"], 3, PromptShape::default())),
        TaskSize::Small
    );
    assert_eq!(
        size_task(&task(&["a.rs", "b.rs"], 3, PromptShape::default())),
        TaskSize::Medium,
        "two named files is not a one-file change"
    );
}

#[test]
fn deliverable_count_drives_both_boundaries() {
    assert_eq!(size_task(&task(&[], 3, shape(1, false))), TaskSize::Small);
    assert_eq!(size_task(&task(&[], 3, shape(2, false))), TaskSize::Medium);
    assert_eq!(
        size_task(&task(&[], 3, shape(4, false))),
        TaskSize::Large,
        "four deliverables is large even with no stated sequence"
    );
}

#[test]
fn sequence_without_many_deliverables_is_still_medium() {
    assert_eq!(
        size_task(&task(&[], 3, shape(1, true))),
        TaskSize::Medium,
        "an ordered prompt is a multi-step task whatever it names"
    );
}

#[test]
fn module_count_is_a_large_signal_only() {
    use pir_builtin_mode::size::thresholds::MODULES_LARGE;
    assert_eq!(
        size_task(&task(&[], MODULES_LARGE, PromptShape::default())),
        TaskSize::Small,
        "the boundary itself is not yet large"
    );
    assert_eq!(
        size_task(&task(&[], MODULES_LARGE + 1, PromptShape::default())),
        TaskSize::Large
    );
}

#[test]
fn a_retry_escalates_without_consulting_shape() {
    let mut retry = task(&["a.rs"], 3, PromptShape::default());
    retry.failed_attempts = Some(1);
    assert_eq!(
        size_task(&retry),
        TaskSize::Large,
        "a task that already failed an attempt does not start small again"
    );
}

#[test]
fn every_lane_has_a_reason_for_the_ledger() {
    for lane in [TaskSize::Small, TaskSize::Medium, TaskSize::Large] {
        assert!(!lane.reason().is_empty(), "{lane:?}");
        assert!(!lane.label().is_empty(), "{lane:?}");
    }
    assert_eq!(TaskSize::Small.label(), "small");
    assert_eq!(TaskSize::Medium.label(), "medium");
    assert_eq!(TaskSize::Large.label(), "large");
}

#[test]
fn prompt_shape_reads_bullets_sequences_and_enumerations() {
    let bulleted = PromptShape::from_prompt("1. plan the change\n2. implement it");
    assert_eq!(bulleted.deliverables, 2, "{bulleted:?}");
    assert!(bulleted.sequential, "{bulleted:?}");

    let dashed = PromptShape::from_prompt("- fix the parser\n- add a test\n- update docs");
    assert_eq!(dashed.deliverables, 3, "{dashed:?}");
    assert!(dashed.sequential, "{dashed:?}");

    let prose = PromptShape::from_prompt("fix the duration parser");
    assert_eq!(prose.deliverables, 0, "{prose:?}");
    assert!(!prose.sequential, "{prose:?}");

    let sequenced = PromptShape::from_prompt("refactor it, then run the tests");
    assert!(sequenced.sequential, "{sequenced:?}");

    // A one-line enumeration is the same signal without the list formatting.
    let joined = PromptShape::from_prompt("fix the parser and update the docs");
    assert_eq!(joined.deliverables, 2, "{joined:?}");
}

#[test]
fn sizing_is_pure() {
    // Same inputs, same answer: the property that makes this a regression test
    // rather than a heuristic with hidden state.
    let input = task(&["a.rs", "b.rs"], 7, shape(2, true));
    let first = size_task(&input);
    for _ in 0..100 {
        assert_eq!(size_task(&input), first);
    }
}
