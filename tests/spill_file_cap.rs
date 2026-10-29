//! P1 regression: one spill file has a hard per-file byte cap.
//!
//! The sweep's total-bytes/count caps bound the directory over time, but a
//! single command can emit an unbounded stream; `SpillFile::write_all`
//! refuses to grow one file past its cap so the file never becomes an
//! unbounded disk sink. The tiny explicit cap keeps the test cheap.

use pir::coding_agent::tools::spill::create_spill_file_with_limit;

#[test]
fn spill_file_write_stops_at_the_per_file_cap() {
    let mut spill = create_spill_file_with_limit(8).expect("create spill file");
    let path = spill.path().to_path_buf();
    spill.write_all(b"12345678").expect("write exactly the cap");
    assert_eq!(spill.written(), 8);
    assert!(
        spill.write_all(b"x").is_err(),
        "one byte past the cap must be refused"
    );
    assert_eq!(spill.written(), 8);
    drop(spill);
    assert_eq!(std::fs::read(&path).expect("read spill"), b"12345678");
    let _ = std::fs::remove_file(path);
}

#[test]
fn spill_file_cap_is_checked_before_any_partial_write() {
    let mut spill = create_spill_file_with_limit(4).expect("create spill file");
    let path = spill.path().to_path_buf();
    assert!(spill.write_all(b"12345").is_err());
    assert_eq!(spill.written(), 0);
    drop(spill);
    assert!(std::fs::read(&path).expect("read spill").is_empty());
    let _ = std::fs::remove_file(path);
}
