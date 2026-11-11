//! `pir::sdk::event` unit tests moved out of the source module's inline
//! `#[cfg(test)] mod tool_update_dedupe_tests`: the exactly-once delivery of
//! `ToolExecutionUpdate` under run-channel backpressure and the bounded
//! retained-memory accounting of [`ToolUpdateDedupe`] (PROTOCOL v5).
//!
//! The dedupe type, the live sink state and the two deliver paths are
//! `#[doc(hidden)] pub` seams on `pir::sdk`; `SdkEvent` itself is the public
//! SDK surface.

use std::time::Duration;

use pir::sdk::{
    deliver_loop_tool_update, push_tool_update_from_bridge, tool_update_fingerprint, SdkEvent,
    SharedSink, ToolUpdateDedupe,
};

fn update(seq: i64) -> SdkEvent {
    SdkEvent::ToolExecutionUpdate {
        tool_call_id: "call-flood".to_owned(),
        tool_name: Some("flood".to_owned()),
        parent_call_id: None,
        args: serde_json::json!({}),
        partial: serde_json::json!({ "seq": seq }),
    }
}

/// A loop update dropped by a full run channel must NOT be recorded: the
/// bridge copy is then still forwarded (exactly-once under backpressure).
/// Recording it before the send would suppress the only remaining copy.
#[tokio::test]
async fn dropped_loop_update_stays_unrecorded_and_bridge_delivers_it() {
    let sink = SharedSink::default();
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    sink.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .sender = Some(tx.clone());
    // Fill the channel so the loop copy hits `try_send` backpressure.
    tx.try_send(SdkEvent::MessageStart)
        .expect("channel is empty");

    assert!(
        !deliver_loop_tool_update(&sink, update(7)),
        "a full channel drops the loop copy"
    );
    assert!(
        !sink
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .loop_tool_updates
            .contains(&tool_update_fingerprint(&update(7)).expect("update fingerprint")),
        "a dropped loop copy must not be recorded"
    );

    // The consumer drains, then the bridge copy of the same update owns
    // the delivery.
    let _ = rx.recv().await;
    push_tool_update_from_bridge(&sink, || Some(update(7)));
    match rx.recv().await {
        Some(SdkEvent::ToolExecutionUpdate { partial, .. }) => {
            assert_eq!(partial.get("seq").and_then(|v| v.as_i64()), Some(7));
        }
        other => panic!("bridge copy must be delivered exactly once, got {other:?}"),
    }
}

#[test]
fn dedupe_is_per_call_identity_and_exactly_once_per_fingerprint() {
    let mut dedupe = ToolUpdateDedupe::default();
    let call_a = "call-a\u{1f}{\"content\":[{\"type\":\"text\",\"text\":\"a\"}]}";
    let call_b = "call-b\u{1f}{\"content\":[{\"type\":\"text\",\"text\":\"a\"}]}";

    dedupe.record(call_a.to_owned());
    dedupe.record(call_a.to_owned()); // bridge copy of the same update
    assert!(
        dedupe.contains(call_a),
        "the recorded update must dedupe its bridge copy"
    );

    // Same partial under another call id is a DIFFERENT update.
    assert!(!dedupe.contains(call_b));
    dedupe.record(call_b.to_owned());
    assert!(dedupe.contains(call_b));

    dedupe.clear();
    assert!(!dedupe.contains(call_a));
}

/// Manual probe (R51, PROTOCOL v5): retained dedupe memory must scale
/// with the number of live keys, not with payload bytes.
#[test]
#[ignore = "manual memory probe"]
fn dedupe_retained_memory_probe() {
    fn rss_kib() -> u64 {
        #[cfg(windows)]
        {
            let pid = std::process::id();
            let output = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
                .output()
                .expect("tasklist");
            let text = String::from_utf8_lossy(&output.stdout);
            let line = text
                .lines()
                .find(|line| line.contains(&pid.to_string()))
                .expect("pid row");
            let memory = line.rsplit("\",\"").next().unwrap_or("").trim_matches('"');
            memory
                .chars()
                .filter(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        }
        #[cfg(not(windows))]
        {
            0
        }
    }
    let payload = "x".repeat(20 * 1024);
    let before = rss_kib();
    let started = std::time::Instant::now();
    let mut dedupe = ToolUpdateDedupe::default();
    for index in 0..1000 {
        dedupe.record(format!("call-{index}\u{1f}{payload}"));
    }
    let elapsed = started.elapsed();
    let after = rss_kib();
    println!(
        "DEDUPE_PROBE,before_kib={before},after_kib={after},delta_kib={},elapsed_ms={},entries={}",
        after.saturating_sub(before),
        elapsed.as_millis(),
        dedupe.seen.len()
    );
    assert_eq!(dedupe.seen.len(), 1000, "all live keys retained");
}

#[test]
fn expired_fingerprints_are_pruned_and_memory_stays_bounded() {
    // 20ms is a test-only stand-in for the 15s production retention.
    let mut dedupe = ToolUpdateDedupe::with_retention(Duration::from_millis(20));
    dedupe.record("old-update".to_owned());
    assert!(dedupe.contains("old-update"));

    std::thread::sleep(Duration::from_millis(30));

    // Recording past the prune interval drops entries whose dispatch
    // window has closed. A bridge copy past RETENTION cannot exist, so
    // dropping them cannot resurrect a duplicate.
    for index in 0..ToolUpdateDedupe::PRUNE_EVERY {
        dedupe.record(format!("new-update-{index}"));
    }
    assert!(
        !dedupe.contains("old-update"),
        "fingerprint older than the dispatch window must be pruned"
    );
    assert_eq!(
        dedupe.seen.len(),
        ToolUpdateDedupe::PRUNE_EVERY,
        "only live-window fingerprints are retained"
    );
    for index in 0..ToolUpdateDedupe::PRUNE_EVERY {
        assert!(dedupe.contains(&format!("new-update-{index}")));
    }
}
