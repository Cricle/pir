// Integration tests (kept out of src/).

use pir_builtin_todo::TaskStatus::{Completed, Deleted, InProgress, Pending};
use pir_builtin_todo::{is_transition_valid, TaskStatus};

#[test]
fn transition_table_matches_upstream() {
    for (from, allowed) in [
        (Pending, vec![Pending, InProgress, Completed, Deleted]),
        (InProgress, vec![InProgress, Pending, Completed, Deleted]),
        (Completed, vec![Completed, Deleted]),
        (Deleted, vec![Deleted]),
    ] {
        for to in TaskStatus::ALL {
            assert_eq!(
                is_transition_valid(from, to),
                allowed.contains(&to),
                "{from} → {to}"
            );
        }
    }
}

#[test]
fn status_wire_round_trips() {
    for status in TaskStatus::ALL {
        assert_eq!(TaskStatus::parse(status.as_str()), Some(status));
    }
    assert_eq!(TaskStatus::parse("bogus"), None);
    assert_eq!(InProgress.as_str(), "in_progress");
    assert_eq!(InProgress.to_string(), "in_progress");
}
