//! Bedrock credential cache: expiration-aware TTL, single-flight resolution,
//! and the four expiring parsers capturing their source expiration.
//!
//! Moved out of `src/ai/api/bedrock/credentials.rs` — unit tests do not live in
//! `src/`. The cache is white-box (injectable clock, per-key cells), so the
//! crate exposes the seams through `credentials::test_api`, behind opaque
//! wrappers that keep the real types private.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pir_core::ai::api::bedrock::credentials::test_api::{
    cache, credential_process_expiration, ecs_expiration, imds_expiration, iso8601, key,
    sts_expiration,
};
use pir_core::ai::error::AiError;

fn fake_clock(
    start: SystemTime,
) -> (
    Arc<Mutex<SystemTime>>,
    Box<dyn Fn() -> SystemTime + Send + Sync>,
) {
    let shared = Arc::new(Mutex::new(start));
    let clock = Arc::clone(&shared);
    (
        shared,
        Box::new(move || *clock.lock().unwrap_or_else(|poison| poison.into_inner())),
    )
}

fn advance(clock: &Arc<Mutex<SystemTime>>, by: Duration) {
    let mut now = clock.lock().unwrap_or_else(|poison| poison.into_inner());
    *now = now.checked_add(by).expect("fake clock must not overflow");
}

fn now_of(clock: &Arc<Mutex<SystemTime>>) -> SystemTime {
    *clock.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[test]
fn temporary_credential_parsers_capture_the_source_expiration() {
    let expected = iso8601("2030-01-01T00:00:00Z").expect("the fixture timestamp parses");
    let ecs = ecs_expiration(
        r#"{"AccessKeyId":"A","SecretAccessKey":"B","Token":"T","Expiration":"2030-01-01T00:00:00Z"}"#,
    )
    .expect("ECS payload parses");
    assert_eq!(ecs, Some(expected));
    let process = credential_process_expiration(
        r#"{"Version":1,"AccessKeyId":"A","SecretAccessKey":"B","Expiration":"2030-01-01T00:00:00Z"}"#,
    )
    .expect("credential_process payload parses");
    assert_eq!(process, Some(expected));
    let sts = sts_expiration(
        "<AssumeRoleResponse><AssumeRoleResult><Credentials>\
         <AccessKeyId>A</AccessKeyId><SecretAccessKey>B</SecretAccessKey>\
         <SessionToken>T</SessionToken><Expiration>2030-01-01T00:00:00Z</Expiration>\
         </Credentials></AssumeRoleResult></AssumeRoleResponse>",
    )
    .expect("STS payload parses");
    assert_eq!(sts, Some(expected));
    let imds = imds_expiration(
        r#"{"AccessKeyId":"A","SecretAccessKey":"B","Token":"T","Expiration":"2030-01-01T00:00:00Z"}"#,
    )
    .expect("IMDS payload parses");
    assert_eq!(imds, Some(expected));
}

#[tokio::test]
async fn cache_hit_returns_the_entry_without_resolving_again() {
    let (_clock, now) = fake_clock(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    let cache = cache(now, Duration::from_secs(300), Duration::from_secs(60));
    let calls = Arc::new(AtomicUsize::new(0));
    let key = key("dev");

    for _ in 0..3 {
        let calls = Arc::clone(&calls);
        let resolved = cache
            .resolve(&key, move || {
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(("AKID-HIT".to_string(), None))
                }
            })
            .await
            .expect("resolution succeeds");
        assert_eq!(resolved.access_key_id, "AKID-HIT");
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "one resolution for three hits"
    );
}

#[tokio::test]
async fn reported_expiration_refreshes_the_entry_before_it_lapses() {
    let (clock, now) = fake_clock(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    let cache = cache(now, Duration::from_secs(300), Duration::from_secs(60));
    let calls = Arc::new(AtomicUsize::new(0));
    let key = key("expiring");

    // First resolution reports an expiration 10 minutes out; the cache
    // refreshes it one minute early (9 minutes from now).
    let calls_first = Arc::clone(&calls);
    let clock_first = Arc::clone(&clock);
    let first = cache
        .resolve(&key, move || {
            let calls = Arc::clone(&calls_first);
            let clock = Arc::clone(&clock_first);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok((
                    "AKID-1".to_string(),
                    now_of(&clock).checked_add(Duration::from_secs(600)),
                ))
            }
        })
        .await
        .expect("first resolution");
    assert_eq!(first.access_key_id, "AKID-1");

    // Inside the freshness window: cached.
    advance(&clock, Duration::from_secs(8 * 60));
    let calls_second = Arc::clone(&calls);
    let second = cache
        .resolve(&key, move || {
            let calls = Arc::clone(&calls_second);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(("UNEXPECTED".to_string(), None))
            }
        })
        .await
        .expect("cache hit");
    assert_eq!(second.access_key_id, "AKID-1");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "still cached at +8m");

    // Past expiration minus the refresh margin (9m): resolve again.
    advance(&clock, Duration::from_secs(2 * 60));
    let calls_third = Arc::clone(&calls);
    let clock_third = Arc::clone(&clock);
    let third = cache
        .resolve(&key, move || {
            let calls = Arc::clone(&calls_third);
            let clock = Arc::clone(&clock_third);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok((
                    "AKID-2".to_string(),
                    now_of(&clock).checked_add(Duration::from_secs(600)),
                ))
            }
        })
        .await
        .expect("refresh");
    assert_eq!(third.access_key_id, "AKID-2");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "expired entry refreshed");
}

#[tokio::test]
async fn credentials_without_expiration_use_the_default_ttl() {
    let (clock, now) = fake_clock(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    let cache = cache(now, Duration::from_secs(300), Duration::from_secs(60));
    let calls = Arc::new(AtomicUsize::new(0));
    let key = key("static");

    let resolve = |calls: Arc<AtomicUsize>| {
        move || {
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(("AKID-STATIC".to_string(), None))
            }
        }
    };

    cache
        .resolve(&key, resolve(Arc::clone(&calls)))
        .await
        .expect("first");
    advance(&clock, Duration::from_secs(4 * 60));
    cache
        .resolve(&key, resolve(Arc::clone(&calls)))
        .await
        .expect("hit inside TTL");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    advance(&clock, Duration::from_secs(60));
    cache
        .resolve(&key, resolve(Arc::clone(&calls)))
        .await
        .expect("refresh after TTL");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn errors_are_never_cached() {
    let (_clock, now) = fake_clock(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    let cache = cache(now, Duration::from_secs(300), Duration::from_secs(60));
    let calls = Arc::new(AtomicUsize::new(0));
    let key = key("failing");

    let calls_first = Arc::clone(&calls);
    let first = cache
        .resolve(&key, move || {
            let calls = Arc::clone(&calls_first);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(AiError::Authentication("no credentials".to_string()))
            }
        })
        .await;
    assert!(first.is_err(), "the first failure must surface");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let calls_second = Arc::clone(&calls);
    let second = cache
        .resolve(&key, move || {
            let calls = Arc::clone(&calls_second);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(("AKID-RECOVERED".to_string(), None))
            }
        })
        .await
        .expect("a failed resolution must not be cached");
    assert_eq!(second.access_key_id, "AKID-RECOVERED");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn expiration_inside_the_refresh_margin_is_not_cached() {
    let (clock, now) = fake_clock(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    let cache = cache(now, Duration::from_secs(300), Duration::from_secs(60));
    let calls = Arc::new(AtomicUsize::new(0));
    let key = key("almost-expired");

    for _ in 0..2 {
        let calls = Arc::clone(&calls);
        let clock = Arc::clone(&clock);
        cache
            .resolve(&key, move || {
                let calls = Arc::clone(&calls);
                let clock = Arc::clone(&clock);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok((
                        "AKID-SOON".to_string(),
                        now_of(&clock).checked_add(Duration::from_secs(30)),
                    ))
                }
            })
            .await
            .expect("resolution");
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "credentials already inside the refresh margin must not be cached"
    );
}

#[tokio::test]
async fn different_keys_resolve_in_parallel() {
    let (_clock, now) = fake_clock(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    let cache = Arc::new(cache(
        now,
        Duration::from_secs(300),
        Duration::from_secs(60),
    ));
    // Both resolvers must be inside `resolve` at the same time. A cache-wide
    // lock (the old design) holds the first resolver while the second waits
    // for the lock, so the barrier below never completes and the timeout
    // fails the test.
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let per_resolve = Duration::from_millis(150);

    let run =
        |profile: &'static str, access_key: &'static str, barrier: Arc<tokio::sync::Barrier>| {
            let cache = Arc::clone(&cache);
            async move {
                let key = key(profile);
                cache
                    .resolve(&key, move || async move {
                        barrier.wait().await;
                        tokio::time::sleep(per_resolve).await;
                        Ok((access_key.to_string(), None))
                    })
                    .await
            }
        };

    let started = std::time::Instant::now();
    let (first, second) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            run("parallel-a", "AKID-A", Arc::clone(&barrier)),
            run("parallel-b", "AKID-B", barrier)
        )
    })
    .await
    .expect("different keys must not serialize on the credential cache");
    assert_eq!(first.expect("first key").access_key_id, "AKID-A");
    assert_eq!(second.expect("second key").access_key_id, "AKID-B");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "two different keys must resolve concurrently: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn concurrent_misses_resolve_once() {
    let (_clock, now) = fake_clock(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
    let cache = Arc::new(cache(
        now,
        Duration::from_secs(300),
        Duration::from_secs(60),
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let key = key("concurrent");

    let run = |calls: Arc<AtomicUsize>| {
        let cache = Arc::clone(&cache);
        let key = key.clone();
        async move {
            cache
                .resolve(&key, move || {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok(("AKID-SHARED".to_string(), None))
                    }
                })
                .await
        }
    };
    let (first, second) = tokio::join!(run(Arc::clone(&calls)), run(Arc::clone(&calls)));
    assert_eq!(first.expect("first").access_key_id, "AKID-SHARED");
    assert_eq!(second.expect("second").access_key_id, "AKID-SHARED");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "single-flight: concurrent misses must resolve exactly once"
    );
}
