//! Allocator / GC tuning spike (single-engine acceleration).
//!
//! Two halves:
//!
//! 1. Pinned, always-run unit tests for the GC knobs: `PIR_GC_PROFILE`
//!    parsing + profile → threshold mapping, the resolution priority
//!    (explicit injection > `PI_EXT_GC_THRESHOLD_MB` > settings >
//!    `PIR_GC_PROFILE` > built-in default), and the `*_MB` value boundaries
//!    (`0`, oversized, invalid).
//!
//! 2. One OPT-IN measurement probe (`PIR_GC_MEASURE=1`) that runs the same
//!    allocation-heavy script under each profile and prints QuickJS's
//!    `JS_ComputeMemoryUsage` counters (`AsyncRuntime::memory_usage`) plus the
//!    wall time. It asserts NOTHING about performance — the foreman / an A/B
//!    run decides. Without the env var every run is a no-op test.
//!
//! Deliberately standalone (`[[test]]` in Cargo.toml, `required-features =
//! ["extensions"]`) instead of being merged into the `zz_merged_*`
//! harnesses: it is measurement scaffolding, and keeping it out leaves the
//! gate harnesses byte-identical.

use serde_json::json;

use pir::config::settings::Settings;
use pir::extensions::limits::{
    gc_profile_from_vars, ExtensionLimits, GcProfile, ENV_GC_PROFILE, ENV_GC_THRESHOLD_MB,
    ENV_NAMES, GC_PROFILE_BALANCED_BYTES, GC_PROFILE_THROUGHPUT_BYTES,
};

const MB: usize = 1024 * 1024;

fn settings_from(value: serde_json::Value) -> Settings {
    serde_json::from_value(value).expect("settings must deserialize")
}

// ---------------------------------------------------------------------------
// Profile: mapping and parsing
// ---------------------------------------------------------------------------

#[test]
fn profiles_map_to_the_documented_thresholds() {
    assert_eq!(
        GC_PROFILE_BALANCED_BYTES,
        4 * MB,
        "balanced = 4 MiB (documented candidate for the extension runtime)"
    );
    assert_eq!(
        GC_PROFILE_THROUGHPUT_BYTES,
        16 * MB,
        "throughput = 16 MiB (documented bulk-workload candidate)"
    );

    // `low` is QuickJS's own pacing: no threshold configured, exactly like
    // the historical `gc_threshold_bytes: None` default.
    assert_eq!(GcProfile::Low.gc_threshold_bytes(), None);
    assert_eq!(
        GcProfile::Balanced.gc_threshold_bytes(),
        Some(GC_PROFILE_BALANCED_BYTES)
    );
    assert_eq!(
        GcProfile::Throughput.gc_threshold_bytes(),
        Some(GC_PROFILE_THROUGHPUT_BYTES)
    );

    // Every profile round-trips through its canonical name, including the
    // case/whitespace tolerant spelling an operator will type.
    for name in GcProfile::NAMES {
        let parsed = GcProfile::parse(name).expect("canonical name parses");
        assert_eq!(parsed.as_str(), name);
        assert_eq!(GcProfile::parse(&format!("  {}  ", name.to_uppercase())), Some(parsed));
    }

    // Unknown / empty values are rejected rather than silently defaulted.
    assert_eq!(GcProfile::parse(""), None);
    assert_eq!(GcProfile::parse("   "), None);
    assert_eq!(GcProfile::parse("fast"), None);
    assert_eq!(GcProfile::parse("balanced "), Some(GcProfile::Balanced));
}

#[test]
fn profile_env_parsing_reports_absent_ok_and_invalid_values() {
    // Absent: no profile, no diagnostic (default behavior untouched).
    let (profile, diagnostics) = gc_profile_from_vars(&[]);
    assert_eq!(profile, None);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");

    // Present and known.
    let (profile, diagnostics) = gc_profile_from_vars(&[(ENV_GC_PROFILE, "throughput")]);
    assert_eq!(profile, Some(GcProfile::Throughput));
    assert!(diagnostics.is_empty(), "{diagnostics:?}");

    // Present but invalid: ignored (base default stands) + diagnostic.
    for bad in ["", "  ", "fast", "LOW2", "1"] {
        let (profile, diagnostics) = gc_profile_from_vars(&[(ENV_GC_PROFILE, bad)]);
        assert_eq!(profile, None, "{bad:?} must not select a profile");
        assert_eq!(diagnostics.len(), 1, "{bad:?}: {diagnostics:?}");
        assert!(diagnostics[0].contains(ENV_GC_PROFILE), "{diagnostics:?}");
        assert!(diagnostics[0].contains("ignored"), "{diagnostics:?}");
        assert!(
            diagnostics[0].contains("low|balanced|throughput"),
            "the diagnostic must name the accepted values: {diagnostics:?}"
        );
    }
}

#[test]
fn profile_is_only_the_base_default_settings_and_env_win() {
    // The profile is applied to the base, not forced.
    let balanced = ExtensionLimits::default().with_gc_profile(Some(GcProfile::Balanced));
    assert_eq!(balanced.gc_threshold_bytes, Some(4 * MB));

    // Settings override the profile.
    let (limits, diagnostics) = ExtensionLimits::from_settings_with_base(
        balanced.clone(),
        &settings_from(json!({ "extensionLimits": { "gcThresholdMb": 8 } })),
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(limits.gc_threshold_bytes, Some(8 * MB));

    // A silent settings object keeps the profile default.
    let (limits, diagnostics) =
        ExtensionLimits::from_settings_with_base(balanced.clone(), &Settings::default());
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(limits.gc_threshold_bytes, Some(4 * MB));

    // PI_EXT_GC_THRESHOLD_MB beats the profile.
    let (limits, diagnostics) = balanced.clone().apply_env(&[(ENV_GC_THRESHOLD_MB, "12")]);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(limits.gc_threshold_bytes, Some(12 * MB));

    // ... including the documented `0` = "engine default pacing" escape.
    let (limits, diagnostics) = balanced.clone().apply_env(&[(ENV_GC_THRESHOLD_MB, "0")]);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(limits.gc_threshold_bytes, None);

    // Selecting `low` explicitly resets a previously configured threshold:
    // the profile is the whole base value, not a floor.
    let configured = ExtensionLimits {
        gc_threshold_bytes: Some(2 * MB),
        ..ExtensionLimits::default()
    };
    assert_eq!(
        configured
            .with_gc_profile(Some(GcProfile::Low))
            .gc_threshold_bytes,
        None
    );

    // An explicit injection wins verbatim: `resolve(Some(..))` never reads
    // env or settings (this is the SDK/embedder/test seam).
    let explicit = ExtensionLimits {
        gc_threshold_bytes: Some(1 * MB),
        ..ExtensionLimits::default()
    };
    assert_eq!(
        ExtensionLimits::resolve(Some(explicit.clone())),
        explicit,
        "explicit limits are adopted verbatim"
    );
}

#[test]
fn profile_is_not_a_pi_ext_override() {
    // `PIR_GC_PROFILE` is a base-default selector resolved by `load()`, not
    // a `PI_EXT_*` field: `apply_process_env` must not see it, otherwise an
    // override would be silently applied twice with the wrong priority.
    assert!(!ENV_NAMES.contains(&ENV_GC_PROFILE));
    let (limits, diagnostics) =
        ExtensionLimits::default().apply_env(&[(ENV_GC_PROFILE, "throughput")]);
    assert_eq!(limits.gc_threshold_bytes, None);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

// ---------------------------------------------------------------------------
// `PI_EXT_GC_THRESHOLD_MB` boundaries
// ---------------------------------------------------------------------------

#[test]
fn gc_threshold_env_boundaries_are_pinned() {
    // Positive MiB converts to bytes.
    let (limits, diagnostics) = ExtensionLimits::default().apply_env(&[("PI_EXT_GC_THRESHOLD_MB", "4")]);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(limits.gc_threshold_bytes, Some(4 * MB));

    // `0` = no threshold (QuickJS default pacing), no diagnostic.
    let (limits, diagnostics) = ExtensionLimits::default().apply_env(&[("PI_EXT_GC_THRESHOLD_MB", "0")]);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(limits.gc_threshold_bytes, None);

    // Whitespace around a valid number is tolerated.
    let (limits, diagnostics) = ExtensionLimits::default().apply_env(&[("PI_EXT_GC_THRESHOLD_MB", " 7 ")]);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(limits.gc_threshold_bytes, Some(7 * MB));

    // Invalid values are ignored: the previous value stands and a diagnostic
    // is reported. This covers non-numeric, negative, fractional, blank, and
    // values too large for `u64`.
    let base = ExtensionLimits {
        gc_threshold_bytes: Some(2 * MB),
        ..ExtensionLimits::default()
    };
    for bad in ["lots", "-1", "4.5", " ", "999999999999999999999999999999"] {
        let (limits, diagnostics) = base.clone().apply_env(&[("PI_EXT_GC_THRESHOLD_MB", bad)]);
        assert_eq!(
            limits.gc_threshold_bytes,
            Some(2 * MB),
            "{bad:?} must leave the previous threshold"
        );
        assert_eq!(diagnostics.len(), 1, "{bad:?}: {diagnostics:?}");
        assert!(diagnostics[0].contains("PI_EXT_GC_THRESHOLD_MB"));
        assert!(diagnostics[0].contains("ignored"));
    }

    // Oversized but representable: saturates at `usize::MAX` instead of
    // panicking or wrapping (QuickJS treats that as effectively unreachable).
    let huge = u64::MAX.to_string();
    let (limits, diagnostics) =
        ExtensionLimits::default().apply_env(&[("PI_EXT_GC_THRESHOLD_MB", huge.as_str())]);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert_eq!(limits.gc_threshold_bytes, Some(usize::MAX));
    assert!(
        limits.summary().contains("gc_threshold="),
        "summary renders the threshold: {}",
        limits.summary()
    );
}

// ---------------------------------------------------------------------------
// Opt-in measurement probe (no performance assertions)
// ---------------------------------------------------------------------------

/// The exact script the probe runs under each profile: object churn with a
/// bounded live set, so the counters reflect both allocation rate and the
/// retained set instead of a single monotonic growth.
#[cfg(feature = "extensions")]
const CHURN_SCRIPT: &str = r#"
    (() => {
        const keep = [];
        for (let i = 0; i < 400000; i++) {
            keep.push({ i, label: "item-" + i, values: [i, i * 2, i * 3] });
            if (keep.length > 50000) keep.splice(0, 10000);
        }
        return keep.length;
    })()
"#;

#[cfg(feature = "extensions")]
mod measure {
    use std::time::{Duration, Instant};

    use super::{GcProfile, CHURN_SCRIPT};
    use pir::extensions::limits::{DEFAULT_MEMORY_BYTES, DEFAULT_STACK_BYTES};

    /// The measurement is strictly opt-in: `PIR_GC_MEASURE=1`.
    ///
    /// The foreman A/Bs the profiles (and the allocator feature) with, e.g.:
    /// `PIR_GC_MEASURE=1 PIR_GC_PROFILE=balanced cargo test --test
    /// alloc_gc_tuning --features extensions measure_gc_profiles --
    /// --nocapture --exact` — but note the probe itself exercises all three
    /// profiles in one run, so the env var only switches it on.
    #[tokio::test]
    async fn measure_gc_profiles() {
        if std::env::var("PIR_GC_MEASURE").ok().as_deref() != Some("1") {
            eprintln!(
                "alloc_gc_tuning: PIR_GC_MEASURE != 1; measurement skipped \
                 (set PIR_GC_MEASURE=1 to run)"
            );
            return;
        }

        println!(
            "profile,threshold_bytes,work_ms,total_bytes,used_bytes,malloc_count,obj_count,\
             after_gc_total_bytes,after_gc_used_bytes"
        );
        for profile in [GcProfile::Low, GcProfile::Balanced, GcProfile::Throughput] {
            match probe(profile.gc_threshold_bytes()).await {
                Ok(sample) => println!("{}", sample.line(profile)),
                Err(error) => println!("{},error,{error}", profile.as_str()),
            }
        }
    }

    struct Sample {
        work: Duration,
        total_bytes: i64,
        used_bytes: i64,
        malloc_count: i64,
        obj_count: i64,
        after_gc_total_bytes: i64,
        after_gc_used_bytes: i64,
    }

    impl Sample {
        fn line(&self, profile: GcProfile) -> String {
            format!(
                "{},{},{},{},{},{},{},{},{}",
                profile.as_str(),
                profile
                    .gc_threshold_bytes()
                    .map(|bytes| bytes.to_string())
                    .unwrap_or_else(|| "engine-default".to_owned()),
                self.work.as_millis(),
                self.total_bytes,
                self.used_bytes,
                self.malloc_count,
                self.obj_count,
                self.after_gc_total_bytes,
                self.after_gc_used_bytes,
            )
        }
    }

    async fn probe(threshold: Option<usize>) -> rquickjs::Result<Sample> {
        let rt = rquickjs::AsyncRuntime::new()?;
        // Same safety net the extension runtime applies, so the measurement
        // runs under production limits.
        rt.set_memory_limit(DEFAULT_MEMORY_BYTES).await;
        rt.set_max_stack_size(DEFAULT_STACK_BYTES).await;
        if let Some(bytes) = threshold {
            rt.set_gc_threshold(bytes).await;
        }
        let ctx = rquickjs::AsyncContext::full(&rt).await?;

        let started = Instant::now();
        let kept = ctx
            .async_with(async |ctx| ctx.eval::<i32, _>(CHURN_SCRIPT.to_string()))
            .await?;
        let work = started.elapsed();
        let usage = rt.memory_usage().await;

        // An explicit full collection shows how much of the total was
        // garbage that the configured threshold had not yet reclaimed.
        rt.run_gc().await;
        let after = rt.memory_usage().await;

        eprintln!(
            "alloc_gc_tuning: profile={} kept={kept} work={:?} malloc_size={} \
             memory_used_size={} malloc_count={} obj_count={}",
            threshold
                .map(|bytes| format!("{bytes}B"))
                .unwrap_or_else(|| "engine-default".to_owned()),
            work,
            usage.malloc_size,
            usage.memory_used_size,
            usage.malloc_count,
            usage.obj_count,
        );

        Ok(Sample {
            work,
            total_bytes: usage.malloc_size,
            used_bytes: usage.memory_used_size,
            malloc_count: usage.malloc_count,
            obj_count: usage.obj_count,
            after_gc_total_bytes: after.malloc_size,
            after_gc_used_bytes: after.memory_used_size,
        })
    }
}
