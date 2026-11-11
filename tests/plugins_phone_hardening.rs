//! Phone pool restart-budget regression:
//! a session that crashed once, recovered for a long time and crashed again
//! must restart instead of being treated as a crash loop.

#![cfg(feature = "phone")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use pir::plugins::builtin::phone::session::{PoolConfig, SessionPool};

fn unique_log_path(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "pir-phone-{tag}-{}-{nanos}.log",
        std::process::id()
    ))
}

fn spawn_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().filter(|line| !line.is_empty()).count())
        .unwrap_or(0)
}

async fn wait_until(mut predicate: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if predicate() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Mock child: logs its PID, sleeps for the configured duration, then exits
/// abnormally.
#[test]
fn mock_phone_budget_child() {
    if std::env::var("PIR_PHONE_BUDGET_CHILD").as_deref() != Ok("1") {
        return;
    }
    if let Ok(path) = std::env::var("PIR_PHONE_BUDGET_LOG") {
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{}", std::process::id());
        }
    }
    let ms = std::env::var("PIR_PHONE_BUDGET_CRASH_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(300);
    std::thread::sleep(Duration::from_millis(ms));
    std::process::exit(7);
}

fn pool_config(log: &Path) -> PoolConfig {
    let mut config = PoolConfig::new(std::env::current_dir().expect("cwd"));
    config.executable = std::env::current_exe().expect("test executable");
    config.args = vec![
        "mock_phone_budget_child".to_owned(),
        "--nocapture".to_owned(),
    ];
    config
        .env
        .push(("PIR_PHONE_BUDGET_CHILD".to_owned(), "1".to_owned()));
    config
        .env
        .push(("PIR_PHONE_BUDGET_LOG".to_owned(), log.display().to_string()));
    config
        .env
        .push(("PIR_PHONE_BUDGET_CRASH_MS".to_owned(), "300".to_owned()));
    config.auto_restart = true;
    config.max_restarts = 1;
    config.restart_delay = Duration::from_millis(10);
    config.restart_backoff_max = Duration::from_millis(10);
    config.restart_budget_reset = Duration::from_millis(100);
    config.max_sessions = 1;
    config.request_timeout = Duration::from_secs(5);
    config.start_timeout = Duration::from_secs(5);
    config.kill_grace = Duration::from_secs(1);
    config
}

#[tokio::test]
async fn restart_budget_resets_after_a_long_lived_child() {
    let log = unique_log_path("restart-reset");
    let _ = std::fs::remove_file(&log);
    let (pool, _events) = SessionPool::new(pool_config(&log));
    let session_id = pool.spawn(None).expect("spawn");

    // Each child lives 300 ms, well past the 100 ms reset window, then
    // crashes. With the reset applied before the budget check the pool keeps
    // restarting; the old ordering stopped after the second spawn.
    assert!(
        wait_until(|| spawn_count(&log) >= 3, Duration::from_secs(10)).await,
        "a long-lived child's crash must not exhaust the old crash streak: saw {} spawn(s)",
        spawn_count(&log)
    );

    pool.stop(&session_id).await.expect("stop");
    pool.shutdown().await;
    let _ = std::fs::remove_file(&log);
}
