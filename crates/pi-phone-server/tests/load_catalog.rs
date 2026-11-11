use std::time::Instant;

use pir_phone_server::protocol::SessionDescriptor;
use pir_phone_server::store::Store;

#[test]
fn ten_thousand_session_catalog_page_is_fast() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("pir-phone-load-{}", uuid::Uuid::new_v4()));
    let store = Store::open(&dir)?;
    let user = store.create_user("load", None)?;
    let device = store.create_device(&user.id, "load-device", None)?;

    let descriptors: Vec<SessionDescriptor> = (0..10_000)
        .map(|index| SessionDescriptor {
            id: format!("session-{index}"),
            session_id: None,
            has_pending_ui_request: Some(false),
            last_activity_at: None,
            label: Some(format!("Session {index}")),
            kind: Some("parent".into()),
            cwd: Some(format!("/workspace/{index}")),
            model: None,
            thinking_level: None,
            is_streaming: None,
            is_compacting: None,
            message_count: Some(index),
            last_error: None,
        })
        .collect();

    let started = Instant::now();
    store.upsert_sessions_batch(&user.id, &device.id, &descriptors, true)?;
    let insert_elapsed = started.elapsed();

    let started = Instant::now();
    let page = store.list_sessions(&user.id, 0, 50, None, None)?;
    let list_elapsed = started.elapsed();

    assert_eq!(page.len(), 50);
    assert!(
        list_elapsed.as_millis() < 200,
        "catalog page took {list_elapsed:?} (insert took {insert_elapsed:?})"
    );
    assert!(
        insert_elapsed.as_secs() < 30,
        "batch insert took {insert_elapsed:?}"
    );

    let second_page = store.list_sessions(&user.id, 50, 50, None, None)?;
    assert_eq!(second_page.len(), 50);
    assert_ne!(page[0].id, second_page[0].id);

    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}

/// Latency distribution for real catalog paging over a 10k-session store.
///
/// `list_sessions(offset, limit, None, None)` is exactly what
/// `ws.rs::build_catalog` calls for `sessions/list` and for the catalog
/// broadcast; the unfiltered path walks the updated-at index in reverse and
/// materializes only the requested page. 120 samples over rotating offsets
/// give p50/p95 without turning this into a benchmark suite.
#[test]
fn ten_thousand_session_catalog_pagination_percentiles() -> anyhow::Result<()> {
    use std::time::Duration;

    let dir = std::env::temp_dir().join(format!("pir-phone-page-{}", uuid::Uuid::new_v4()));
    let store = Store::open(&dir)?;
    let user = store.create_user("page", None)?;
    let device = store.create_device(&user.id, "page-device", None)?;

    let descriptors: Vec<SessionDescriptor> = (0..10_000)
        .map(|index| SessionDescriptor {
            id: format!("session-{index}"),
            session_id: None,
            has_pending_ui_request: Some(false),
            last_activity_at: None,
            label: Some(format!("Session {index}")),
            kind: Some("parent".into()),
            cwd: Some(format!("/workspace/{index}")),
            model: None,
            thinking_level: None,
            is_streaming: None,
            is_compacting: None,
            message_count: Some(index),
            last_error: None,
        })
        .collect();
    let started = Instant::now();
    store.upsert_sessions_batch(&user.id, &device.id, &descriptors, true)?;
    let insert_ms = started.elapsed().as_secs_f64() * 1000.0;

    // Warm the index/redb caches before sampling the steady state.
    for offset in (0..10).map(|round| (round * 137) % 9_950) {
        let page = store.list_sessions(&user.id, offset, 50, None, None)?;
        assert_eq!(page.len(), 50, "warmup offset {offset}");
    }

    let mut samples = Vec::with_capacity(120);
    for round in 0..120usize {
        let offset = (round * 83) % 9_950;
        let started = Instant::now();
        let page = store.list_sessions(&user.id, offset, 50, None, None)?;
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(page.len(), 50, "offset {offset}");
        samples.push(elapsed);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];

    eprintln!(
        "PHONE_10K_CATALOG insert_ms={insert_ms:.1} samples={} p50_ms={:.2} p95_ms={:.2} min_ms={:.2} max_ms={:.2} pid={}",
        samples.len(),
        p(0.50),
        p(0.95),
        samples[0],
        samples[samples.len() - 1],
        std::process::id()
    );

    // Keep the process alive briefly so an external sampler can record the
    // peak working set after the 10k insert (this test is run manually).
    std::thread::sleep(Duration::from_secs(3));

    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}
