use super::*;

/// F16: the TUI trace log (`pir.log`) is capped at startup — past this size
/// the file rotates to a timestamped generation so the new run starts on a
/// fresh file.
pub(super) const TUI_TRACE_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// F16: rotate the TUI trace log when it exceeds `max_bytes`: rename the
/// oversized `pir.log` to `pir.log.<unix-seconds>[-N]` and KEEP every
/// generation (user decision 2026-10-21: logs are not deleted).
pub(super) fn rotate_tui_trace_log(path: &std::path::Path, max_bytes: u64) {
    let over_limit = std::fs::metadata(path)
        .map(|meta| meta.len() > max_bytes)
        .unwrap_or(false);
    if !over_limit {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    for attempt in 0..1000u32 {
        let name = if attempt == 0 {
            format!("pir.log.{stamp}")
        } else {
            format!("pir.log.{stamp}.{attempt}")
        };
        let target = path.with_file_name(name);
        if target.exists() {
            continue;
        }
        let _ = std::fs::rename(path, &target);
        return;
    }
}

/// Validate `--tui-mode` (upstream errors on values other than
/// regular/fullscreen).
pub(super) fn validate_tui_mode(cli: &Cli) -> Result<()> {
    match cli.tui_mode.as_deref() {
        Some("regular") | Some("fullscreen") | None => Ok(()),
        Some(other) => Err(anyhow::anyhow!(
            "Invalid TUI mode \"{other}\". Valid values: regular, fullscreen"
        )),
    }
}
