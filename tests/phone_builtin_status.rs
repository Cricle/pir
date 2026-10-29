//! The native `pi-phone` status entry flips across start/stop.
//!
//! Moved out of `src/plugins/builtin/phone/mod.rs` (all tests live under
//! `tests/`). Uses an ephemeral port, so the test is hermetic.

use pir::plugins::builtin::phone::server::STATUS_KEY;
use pir::plugins::builtin::phone::PhonePlugin;
use pir::plugins::AgentPlugin;

const ON: &str = "📱 ● phone on";
const OFF: &str = "📱 ○ phone off";

fn entry(text: &str) -> (String, String) {
    (STATUS_KEY.to_owned(), text.to_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn statuses_flip_across_start_and_stop() {
    let plugin = PhonePlugin::new();
    assert_eq!(plugin.statuses(), vec![entry(OFF)]);

    let port = plugin.start_for_test("0 -").await.expect("ephemeral start");
    assert!(port > 0);
    assert_eq!(
        plugin.statuses(),
        vec![entry(ON)],
        "a running listener must report the on entry"
    );

    assert!(plugin.stop_for_test().await);
    assert_eq!(
        plugin.statuses(),
        vec![entry(OFF)],
        "a stopped listener must report the off entry again"
    );
}
