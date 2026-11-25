// Integration tests (kept out of src/).

use pir_builtin_mode::*;

#[test]
fn labels_round_trip() {
    for mode in Mode::ALL {
        assert_eq!(Mode::from_label(mode.label()), Some(mode));
    }
    for phase in [
        Phase::Idle,
        Phase::Planning,
        Phase::Delegating,
        Phase::Review,
        Phase::Rework,
        Phase::Verified,
        Phase::Unverified,
    ] {
        assert_eq!(Phase::from_label(phase.label()), Some(phase));
    }
}

#[test]
fn u8_round_trip() {
    assert_eq!(Mode::from_u8(0), Mode::Normal);
    assert_eq!(Mode::from_u8(1), Mode::Foreman);
    assert_eq!(Mode::from_u8(2), Mode::Plan);
    assert_eq!(Mode::from_u8(9), Mode::Normal);
    assert_eq!(Phase::from_u8(0), Phase::Idle);
    assert_eq!(Phase::from_u8(5), Phase::Verified);
    assert_eq!(Phase::from_u8(6), Phase::Unverified);
    assert_eq!(Phase::from_u8(9), Phase::Idle);
}

#[test]
fn rejects_unknown_labels() {
    assert_eq!(Mode::from_label("FOREMAN"), None);
    assert_eq!(Phase::from_label("idle"), None);
}
