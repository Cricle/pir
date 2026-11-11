//! In-process counters and OS memory readings used by `/api/metrics`.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Metrics {
    pub agent_frames: AtomicU64,
    pub client_frames: AtomicU64,
    pub event_frames: AtomicU64,
    pub broadcast_frames: AtomicU64,
    pub broadcast_deliveries: AtomicU64,
    pub commands: AtomicU64,
    pub catalog_broadcasts: AtomicU64,
    pub ws_connections: AtomicU64,
}

impl Metrics {
    pub fn incr(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            agent_frames: self.agent_frames.load(Ordering::Relaxed),
            client_frames: self.client_frames.load(Ordering::Relaxed),
            event_frames: self.event_frames.load(Ordering::Relaxed),
            broadcast_frames: self.broadcast_frames.load(Ordering::Relaxed),
            broadcast_deliveries: self.broadcast_deliveries.load(Ordering::Relaxed),
            commands: self.commands.load(Ordering::Relaxed),
            catalog_broadcasts: self.catalog_broadcasts.load(Ordering::Relaxed),
            ws_connections: self.ws_connections.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsSnapshot {
    pub agent_frames: u64,
    pub client_frames: u64,
    pub event_frames: u64,
    pub broadcast_frames: u64,
    pub broadcast_deliveries: u64,
    pub commands: u64,
    pub catalog_broadcasts: u64,
    pub ws_connections: u64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySnapshot {
    pub rss_bytes: u64,
    pub peak_rss_bytes: u64,
}

/// Read VmRSS/VmHWM from `/proc/self/status` on Linux. Other platforms return
/// zeros; deployment and load tests run on Linux where these numbers matter.
pub fn memory_snapshot() -> MemorySnapshot {
    #[cfg(target_os = "linux")]
    {
        let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
            return MemorySnapshot::default();
        };
        let mut snapshot = MemorySnapshot::default();
        for line in status.lines() {
            if let Some(value) = line.strip_prefix("VmRSS:") {
                snapshot.rss_bytes = parse_kib(value);
            } else if let Some(value) = line.strip_prefix("VmHWM:") {
                snapshot.peak_rss_bytes = parse_kib(value);
            }
        }
        snapshot
    }
    #[cfg(not(target_os = "linux"))]
    {
        MemorySnapshot::default()
    }
}

#[cfg(target_os = "linux")]
fn parse_kib(value: &str) -> u64 {
    value
        .trim()
        .trim_end_matches(" kB")
        .parse::<u64>()
        .unwrap_or(0)
        .saturating_mul(1024)
}
