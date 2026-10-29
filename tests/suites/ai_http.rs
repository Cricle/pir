use std::collections::BTreeMap;
use std::time::Duration;

use pir::ai::auth::{env_api_keys, ApiKeyAuth, MapAuthContext};
use pir::ai::transport::{ProviderRequestClient, ProviderRequestConfig};
use pir::ai::types::AbortSignal;

#[test]
fn common_api_key_environment_mapping_resolves() {
    let mut values = BTreeMap::new();
    values.insert("OPENAI_API_KEY".to_string(), "test-key".to_string());
    let context = MapAuthContext::new(values);
    let auth = ApiKeyAuth::new(
        "OpenAI",
        env_api_keys::OPENAI
            .iter()
            .map(|v| (*v).to_string())
            .collect(),
    );
    let result = auth
        .resolve(&context, None)
        .expect("auth resolution should succeed");
    assert_eq!(
        result.and_then(|value| value.auth.api_key),
        Some("test-key".to_string())
    );
}

#[test]
fn request_client_has_bounded_defaults_and_observes_abort() {
    let client = ProviderRequestClient::new(ProviderRequestConfig {
        timeout: Duration::from_millis(100),
        max_response_bytes: 1024,
        max_retries: 1,
        max_retry_delay: Duration::from_millis(100),
        proxy_url: None,
        http_idle_timeout: None,
    })
    .expect("client should build");
    assert_eq!(client.config.max_response_bytes, 1024);
    let signal = AbortSignal::new();
    signal.abort();
    assert!(signal.check().is_err());
}

/// `Retry-After` accepts an HTTP-date, and the date header is
/// provider-controlled. Parsing one must never panic on hostile values:
/// a pre-epoch date is a valid date whose delay is already negative (upstream
/// clamps `Date.parse(...) - Date.now()` at zero), and a month token whose
/// third byte lands inside a multi-byte character is simply an invalid month.
#[test]
fn http_date_retry_after_does_not_panic_on_pre_epoch_or_multibyte_fields() {
    use pir::ai::transport::parse_retry_after;

    // Pre-epoch dates: zero delay ("retry now"), never a day-arithmetic
    // underflow.
    assert_eq!(
        parse_retry_after("Wed, 31 Dec 1969 23:59:59 GMT"),
        Some(Duration::ZERO)
    );
    assert_eq!(
        parse_retry_after("Thu, 01 Jan 1970 00:00:00 GMT"),
        Some(Duration::ZERO)
    );
    // "éé" is 4 bytes; byte index 3 is not a character boundary, so the old
    // `month[..3]` panicked here.
    assert_eq!(
        parse_retry_after("Thu, 01 \u{e9}\u{e9} 1970 00:00:00 GMT"),
        None
    );
    // Out-of-range fields are invalid input, not arithmetic overflow.
    assert_eq!(parse_retry_after("Thu, 99 Foo 1970 99:99:99 GMT"), None);
    // A normal past date still resolves to an already-elapsed delay.
    assert_eq!(
        parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"),
        Some(Duration::ZERO)
    );
    // Seconds form is untouched.
    assert_eq!(parse_retry_after("60"), Some(Duration::from_secs(60)));
}

/// A finite but absurdly large `Retry-After` value (seconds or milliseconds)
/// is provider-controlled input. Converting it to a `Duration` must saturate
/// instead of panicking (`Duration::from_secs_f64` panics on overflow), and the
/// saturated delay reaches the retry cap so the request fails fast rather than
/// sleeping for ~u64::MAX seconds.
#[test]
fn huge_retry_after_values_saturate_instead_of_panicking() {
    use pir::ai::transport::{parse_retry_after, server_retry_delay};

    assert_eq!(parse_retry_after("1e300"), Some(Duration::MAX));
    assert_eq!(
        parse_retry_after("1.7976931348623157e308"),
        Some(Duration::MAX)
    );
    // Milliseconds form (the `retry-after-ms` header) has the same overflow
    // path: `ms / 1000.0` is still far beyond the duration range.
    assert_eq!(server_retry_delay(Some("1e300"), None), Some(Duration::MAX));
    assert_eq!(
        server_retry_delay(Some("1e308"), Some("60")),
        Some(Duration::MAX),
        "retry-after-ms wins and saturates"
    );
    // Non-finite/negative values still fall through to the exponential
    // backoff instead of becoming a delay.
    assert_eq!(parse_retry_after("inf"), None);
    assert_eq!(parse_retry_after("NaN"), None);
    assert_eq!(parse_retry_after("-1"), None);
}
