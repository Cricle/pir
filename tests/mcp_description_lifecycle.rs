//! C2-01 regression: MCP tool names/descriptions are owned by the live
//! `McpToolBridge` and released when a registry rebuild drops the previous
//! bridge generation. The former global `Box::leak` intern pool retained every
//! distinct description for the process lifetime, so a server that changed its
//! `tools/list` text on each reload grew RSS without bound.
//!
//! The bridge constructor is the same one `McpPlugin::register` calls; a
//! rebuild is modelled as build a generation, observe the live-text gauge,
//! drop it, and repeat with a different description.

use std::path::Path;
use std::sync::{Arc, Mutex};

use pir::agent::tool_execution::AgentTool;
use pir::config::mcp::McpExposure;
use pir::mcp::manager::{DiscoveredTool, McpManager};
use pir::mcp::protocol::McpTool;
use pir::plugins::builtin::McpToolBridge;

/// The stats gauge is process-wide; keep this file's tests from overlapping.
static STATS_LOCK: Mutex<()> = Mutex::new(());

fn bridge(manager: Arc<McpManager>, description: String) -> McpToolBridge {
    let discovered = DiscoveredTool {
        server: "reload_server".to_owned(),
        tool: McpTool {
            name: "search".to_owned(),
            description: Some(description),
            ..Default::default()
        },
        exposure: McpExposure::Direct,
    };
    McpToolBridge::new(manager, discovered, "mcp__reload_server__search".to_owned())
}

#[test]
fn repeated_reloads_with_changed_descriptions_do_not_grow_live_text() {
    let _guard = STATS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let manager = McpManager::without_log(Path::new("."));
    let baseline = McpToolBridge::live_text_stats();
    assert_eq!(baseline, (0, 0), "test starts with no live bridges");

    let mut observed_bytes = Vec::new();
    for generation in 0..64 {
        // Same shape/length every generation, different text: the old pool
        // would add one ~8 KiB entry per reload.
        let description = format!("generation {generation:03} {}", "x".repeat(8 * 1024));
        let bridge = bridge(Arc::clone(&manager), description.clone());
        let (entries, bytes) = McpToolBridge::live_text_stats();
        assert_eq!(entries, 1, "one live bridge after rebuild {generation}");
        assert_eq!(bytes, bridge.name().len() + bridge.description().len());
        observed_bytes.push(bytes);

        // Dropping the old generation (what registry replacement does) must
        // release all of its text before the next generation is built.
        drop(bridge);
        assert_eq!(
            McpToolBridge::live_text_stats(),
            baseline,
            "rebuild {generation} must release the previous description"
        );
    }
    assert!(
        observed_bytes.windows(2).all(|pair| pair[0] == pair[1]),
        "live text must stay constant across description changes: {observed_bytes:?}"
    );
}

#[test]
fn oversized_server_description_is_capped_before_registration() {
    let _guard = STATS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let manager = McpManager::without_log(Path::new("."));
    let baseline = McpToolBridge::live_text_stats();

    // Two bytes per char: the cap must cut on a UTF-8 boundary, never panic.
    let oversized = "界".repeat(McpToolBridge::MAX_DESCRIPTION_BYTES);
    let bridge = bridge(manager, oversized.clone());
    assert!(bridge.description().len() <= McpToolBridge::MAX_DESCRIPTION_BYTES);
    assert!(oversized.starts_with(bridge.description()));
    assert_eq!(
        McpToolBridge::live_text_stats(),
        (
            baseline.0 + 1,
            baseline.1 + bridge.name().len() + bridge.description().len()
        )
    );

    drop(bridge);
    assert_eq!(McpToolBridge::live_text_stats(), baseline);
}
