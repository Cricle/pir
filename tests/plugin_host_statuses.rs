//! Native status aggregation and JS/native merge semantics.
//!
//! Moved out of `src/plugins/host.rs` (all tests live under `tests/`).

use std::sync::Arc;

use pir::plugins::host::merge_statuses;
use pir::plugins::{AgentPlugin, PluginHost, PluginInfo};

/// Minimal native plugin publishing a fixed status set.
struct FakeStatusPlugin {
    info: PluginInfo,
    entries: Vec<(String, String)>,
}

impl AgentPlugin for FakeStatusPlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }

    fn statuses(&self) -> Vec<(String, String)> {
        self.entries.clone()
    }
}

fn entry(key: &str, text: &str) -> (String, String) {
    (key.to_owned(), text.to_owned())
}

/// Two native plugins contribute three statuses (the first returns two,
/// deliberately unsorted); the aggregate is plugin order then key, and
/// merging it into a JS snapshot keeps distinct JS keys while the native
/// entry wins on the shared key — no duplicate line.
#[test]
fn native_statuses_merge_into_the_status_snapshot() {
    let first = Arc::new(FakeStatusPlugin {
        info: PluginInfo::inline("first"),
        entries: vec![entry("b", "b native"), entry("a", "a native")],
    });
    let second = Arc::new(FakeStatusPlugin {
        info: PluginInfo::inline("second"),
        entries: vec![entry("c", "c native")],
    });
    let host = PluginHost::build(None, vec![first, second]);

    let native = host.statuses();
    assert_eq!(
        native,
        vec![
            entry("a", "a native"),
            entry("b", "b native"),
            entry("c", "c native"),
        ],
        "plugin registration order, then key within each plugin"
    );

    let js = vec![entry("js", "js status"), entry("a", "stale js")];
    let merged = merge_statuses(&js, &native);
    assert_eq!(
        merged,
        vec![
            entry("js", "js status"),
            entry("a", "a native"),
            entry("b", "b native"),
            entry("c", "c native"),
        ],
        "distinct keys survive; the native entry replaces the same-key JS one"
    );
}
