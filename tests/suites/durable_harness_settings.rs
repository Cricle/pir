//! Harness settings resolution.
//!
//! Covers upstream `harness/agent.ts::resolveSettings` and the settings types
//! of `harness/types.ts`: every field over its default, field-by-field merges,
//! the `"provider/modelId"` compaction override map, the three-state
//! `afterFailedRun`, both queue modes and both tool execution modes.
//!
//! Tests live here as an integration suite rather than inline in the module:
//! the crate forbids inline tests.

use pir::durable::harness::registry::Extension;
use pir::durable::harness::settings::{
    resolve_compaction_for, resolve_settings, AfterFailedRun, CompactionPolicy, CompactionSettings,
    ConversationRetryPolicy, ConversationRetrySettings, ConversationStreamOptions, DeferredWindow,
    HarnessSettings, PartialCompactionPolicy, ProgressPolicy, ProgressSettings, QueueMode,
    RestartLimit, StreamDeferred, StreamTransport, ToolExecutionMode,
    DEFAULT_COMPACTION_BACKGROUND_TOKENS, DEFAULT_COMPACTION_ENABLED,
    DEFAULT_COMPACTION_KEEP_RECENT_TOKENS, DEFAULT_COMPACTION_RESERVE_TOKENS,
    DEFAULT_CONTEXT_RETENTION_MS, DEFAULT_DOCUMENT_RETENTION_MS, DEFAULT_OUTPUT_INTERVAL_MS,
    DEFAULT_PARTIAL_INTERVAL_MS, DEFAULT_RESTART_LIMIT, DEFAULT_RETRY_BASE_DELAY_MS,
    DEFAULT_RETRY_ENABLED, DEFAULT_RETRY_MAX_AGENT_DELAY_MS, DEFAULT_RETRY_MAX_RETRIES,
};
use serde_json::json;

// ── defaults ───────────────────────────────────────────────────────────────

#[test]
fn empty_settings_resolve_to_every_default() {
    let settings = resolve_settings(&HarnessSettings::default());

    assert!(settings.extensions.is_none(), "no default extension list");
    assert_eq!(settings.stream, ConversationStreamOptions::default());

    assert_eq!(
        settings.retry,
        ConversationRetryPolicy {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2_000,
            max_agent_delay_ms: 60_000,
        }
    );
    assert_eq!(
        settings.compaction,
        CompactionPolicy {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
            background_tokens: 32_768,
            model_overrides: Default::default(),
        }
    );
    assert_eq!(
        settings.progress,
        ProgressPolicy {
            partial_interval_ms: 100,
            output_interval_ms: 100,
        }
    );
    assert_eq!(settings.tool_execution, ToolExecutionMode::Parallel);
    assert_eq!(settings.steering_mode, QueueMode::OneAtATime);
    assert_eq!(settings.follow_up_mode, QueueMode::OneAtATime);
    assert_eq!(settings.context_retention_ms, 600_000.0);
    assert_eq!(settings.document_retention_ms, 600_000.0);
    assert_eq!(settings.after_failed_run, AfterFailedRun::Keep);
    assert_eq!(settings.restart_limit, RestartLimit::Limit(5));
}

#[test]
fn default_constants_match_the_upstream_policy() {
    let settings = resolve_settings(&HarnessSettings::default());
    assert_eq!(settings.retry.enabled, DEFAULT_RETRY_ENABLED);
    assert_eq!(DEFAULT_RETRY_MAX_RETRIES, 3);
    assert_eq!(DEFAULT_RETRY_BASE_DELAY_MS, 2_000);
    assert_eq!(DEFAULT_RETRY_MAX_AGENT_DELAY_MS, 60_000);
    assert_eq!(settings.compaction.enabled, DEFAULT_COMPACTION_ENABLED);
    assert_eq!(DEFAULT_COMPACTION_RESERVE_TOKENS, 16_384);
    assert_eq!(DEFAULT_COMPACTION_KEEP_RECENT_TOKENS, 20_000);
    assert_eq!(DEFAULT_COMPACTION_BACKGROUND_TOKENS, 32_768);
    assert_eq!(DEFAULT_PARTIAL_INTERVAL_MS, 100);
    assert_eq!(DEFAULT_OUTPUT_INTERVAL_MS, 100);
    assert_eq!(DEFAULT_CONTEXT_RETENTION_MS, 600_000.0);
    assert_eq!(DEFAULT_DOCUMENT_RETENTION_MS, 600_000.0);
    assert_eq!(DEFAULT_RESTART_LIMIT, 5);
}

#[test]
fn settings_default_matches_resolved_defaults() {
    let default = pir::durable::harness::settings::Settings::default();
    let resolved = resolve_settings(&HarnessSettings::default());
    assert!(default.compaction.enabled);
    assert_eq!(default.retry, resolved.retry);
    assert_eq!(default.compaction, resolved.compaction);
    assert_eq!(default.progress, resolved.progress);
    assert_eq!(default.restart_limit, resolved.restart_limit);
}

// ── partial overrides ──────────────────────────────────────────────────────

#[test]
fn partial_retry_keeps_unset_members() {
    let settings = resolve_settings(&HarnessSettings {
        retry: Some(ConversationRetrySettings {
            enabled: Some(false),
            max_retries: Some(9),
            ..Default::default()
        }),
        ..Default::default()
    });

    assert!(!settings.retry.enabled);
    assert_eq!(settings.retry.max_retries, 9);
    // Unset members keep the defaults rather than becoming zero.
    assert_eq!(settings.retry.base_delay_ms, 2_000);
    assert_eq!(settings.retry.max_agent_delay_ms, 60_000);
}

#[test]
fn partial_progress_is_field_by_field() {
    let settings = resolve_settings(&HarnessSettings {
        progress: Some(ProgressSettings {
            partial_interval_ms: None,
            output_interval_ms: Some(250),
        }),
        ..Default::default()
    });

    assert_eq!(settings.progress.partial_interval_ms, 100);
    assert_eq!(settings.progress.output_interval_ms, 250);
}

#[test]
fn explicitly_unset_progress_interval_keeps_its_default() {
    // Upstream uses `??`, so a value equal to the default is honored as given
    // and an absent one falls back; both end at the same number, and a
    // zero pause stays zero rather than reverting to 100.
    let zero = resolve_settings(&HarnessSettings {
        progress: Some(ProgressSettings {
            partial_interval_ms: Some(0),
            output_interval_ms: Some(0),
        }),
        ..Default::default()
    });
    assert_eq!(zero.progress.partial_interval_ms, 0);
    assert_eq!(zero.progress.output_interval_ms, 0);
}

#[test]
fn partial_compaction_merges_over_defaults() {
    let settings = resolve_settings(&HarnessSettings {
        compaction: Some(CompactionSettings {
            enabled: None,
            reserve_tokens: Some(1_000),
            keep_recent_tokens: None,
            background_tokens: Some(0),
            model_overrides: None,
        }),
        ..Default::default()
    });

    assert!(settings.compaction.enabled);
    assert_eq!(settings.compaction.reserve_tokens, 1_000);
    assert_eq!(settings.compaction.keep_recent_tokens, 20_000);
    assert_eq!(settings.compaction.background_tokens, 0);
    assert!(settings.compaction.model_overrides.is_empty());
}

#[test]
fn stream_options_are_used_as_given() {
    let settings = resolve_settings(&HarnessSettings {
        stream: Some(ConversationStreamOptions {
            transport: Some(StreamTransport::WebsocketCached),
            timeout_ms: Some(30_000),
            max_retries: Some(2),
            max_retry_delay_ms: Some(1_500),
            headers: Some(
                [("x-test".to_owned(), "1".to_owned())]
                    .into_iter()
                    .collect(),
            ),
            metadata: Some(json!({ "trace": "abc" }).as_object().cloned().unwrap()),
            cache_retention: None,
            deferred: Some(StreamDeferred::Window {
                window: Some(DeferredWindow::OneHour),
            }),
        }),
        ..Default::default()
    });

    assert_eq!(
        settings.stream.transport,
        Some(StreamTransport::WebsocketCached)
    );
    assert_eq!(settings.stream.timeout_ms, Some(30_000));
    assert_eq!(settings.stream.max_retries, Some(2));
    assert_eq!(settings.stream.max_retry_delay_ms, Some(1_500));
    assert_eq!(
        settings
            .stream
            .headers
            .as_ref()
            .and_then(|h| h.get("x-test")),
        Some(&"1".to_owned())
    );
    assert!(settings.stream.metadata.is_some());
    assert_eq!(
        settings.stream.deferred,
        Some(StreamDeferred::Window {
            window: Some(DeferredWindow::OneHour),
        })
    );
}

#[test]
fn stream_transport_wire_names_match_pi_ai() {
    assert_eq!(json!("auto"), json!(StreamTransport::Auto));
    assert_eq!(json!("sse"), json!(StreamTransport::Sse));
    assert_eq!(json!("websocket"), json!(StreamTransport::Websocket));
    assert_eq!(
        json!("websocket-cached"),
        json!(StreamTransport::WebsocketCached)
    );
}

#[test]
fn extension_selection_is_carried_through() {
    let settings = resolve_settings(&HarnessSettings {
        extensions: Some(vec![Extension::new("coding"), Extension::new("skills")]),
        ..Default::default()
    });
    let names: Vec<&str> = settings
        .extensions
        .as_ref()
        .expect("carried")
        .iter()
        .map(|extension| extension.name.as_str())
        .collect();
    assert_eq!(names, ["coding", "skills"]);
}

// ── model overrides ────────────────────────────────────────────────────────

fn override_policy(
    enabled: Option<bool>,
    reserve: Option<u64>,
    keep: Option<u64>,
    background: Option<u64>,
) -> PartialCompactionPolicy {
    PartialCompactionPolicy {
        enabled,
        reserve_tokens: reserve,
        keep_recent_tokens: keep,
        background_tokens: background,
    }
}

#[test]
fn model_overrides_apply_field_by_field() {
    let settings = resolve_settings(&HarnessSettings {
        compaction: Some(CompactionSettings {
            enabled: Some(true),
            reserve_tokens: Some(1_000),
            keep_recent_tokens: Some(2_000),
            background_tokens: Some(3_000),
            model_overrides: Some(
                [(
                    "anthropic/claude-sonnet".to_owned(),
                    override_policy(None, Some(500), None, Some(0)),
                )]
                .into_iter()
                .collect(),
            ),
        }),
        ..Default::default()
    });

    let for_model = settings.compaction_for("anthropic/claude-sonnet");
    // Overridden fields win...
    assert_eq!(for_model.reserve_tokens, 500);
    assert_eq!(for_model.background_tokens, 0);
    // ...and the fields the override leaves unset stay harness-wide.
    assert!(for_model.enabled);
    assert_eq!(for_model.keep_recent_tokens, 2_000);
    // The resolved leaf no longer carries the map.
    assert!(for_model.model_overrides.is_empty());
}

#[test]
fn model_overrides_are_keyed_by_exact_provider_slash_model() {
    let settings = resolve_settings(&HarnessSettings {
        compaction: Some(CompactionSettings {
            reserve_tokens: Some(1_000),
            model_overrides: Some(
                [(
                    "openai/gpt-5".to_owned(),
                    override_policy(Some(false), Some(7), Some(11), Some(13)),
                )]
                .into_iter()
                .collect(),
            ),
            ..Default::default()
        }),
        ..Default::default()
    });

    // Another model, another provider with the same model id, and the bare
    // model id all miss the exact key.
    for key in ["openai/other", "other/gpt-5", "gpt-5", "openai/gpt-5 "] {
        let policy = settings.compaction_for(key);
        assert!(policy.enabled, "{key} must not match");
        assert_eq!(policy.reserve_tokens, 1_000, "{key} must not match");
        assert_eq!(policy.keep_recent_tokens, 20_000, "{key} must not match");
        assert_eq!(policy.background_tokens, 32_768, "{key} must not match");
    }

    let hit = settings.compaction_for("openai/gpt-5");
    assert!(!hit.enabled);
    assert_eq!(hit.reserve_tokens, 7);
    assert_eq!(hit.keep_recent_tokens, 11);
    assert_eq!(hit.background_tokens, 13);
}

#[test]
fn resolve_compaction_for_matches_the_method() {
    let policy = CompactionPolicy {
        enabled: true,
        reserve_tokens: 1_000,
        keep_recent_tokens: 2_000,
        background_tokens: 3_000,
        model_overrides: [(
            "p/m".to_owned(),
            override_policy(Some(false), Some(1), Some(2), Some(3)),
        )]
        .into_iter()
        .collect(),
    };

    assert_eq!(
        resolve_compaction_for(&policy, "p/m"),
        policy.for_model("p/m")
    );
    assert_eq!(
        resolve_compaction_for(&policy, "p/other").reserve_tokens,
        1_000
    );
}

#[test]
fn empty_model_overrides_leave_the_harness_policy_alone() {
    let policy = CompactionPolicy {
        enabled: false,
        reserve_tokens: 10,
        keep_recent_tokens: 20,
        background_tokens: 30,
        model_overrides: [("p/m".to_owned(), override_policy(None, None, None, None))]
            .into_iter()
            .collect(),
    };
    let resolved = resolve_compaction_for(&policy, "p/m");
    assert_eq!(
        resolved,
        CompactionPolicy {
            enabled: false,
            reserve_tokens: 10,
            keep_recent_tokens: 20,
            background_tokens: 30,
            model_overrides: Default::default(),
        }
    );
}

// ── restart limit ──────────────────────────────────────────────────────────

#[test]
fn restart_limit_defaults_to_five_when_unset() {
    let settings = resolve_settings(&HarnessSettings::default());
    assert_eq!(settings.restart_limit, RestartLimit::Limit(5));
}

#[test]
fn restart_limit_infinity_never_parks() {
    let settings = resolve_settings(&HarnessSettings {
        restart_limit: Some(RestartLimit::Infinite),
        ..Default::default()
    });
    assert_eq!(settings.restart_limit, RestartLimit::Infinite);
}

#[test]
fn restart_limit_below_one_falls_back_to_the_default() {
    let zero = resolve_settings(&HarnessSettings {
        restart_limit: Some(RestartLimit::Limit(0)),
        ..Default::default()
    });
    assert_eq!(zero.restart_limit, RestartLimit::Limit(5));
}

#[test]
fn restart_limit_above_one_is_kept() {
    let settings = resolve_settings(&HarnessSettings {
        restart_limit: Some(RestartLimit::Limit(1)),
        ..Default::default()
    });
    assert_eq!(settings.restart_limit, RestartLimit::Limit(1));

    let large = resolve_settings(&HarnessSettings {
        restart_limit: Some(RestartLimit::Limit(u32::MAX)),
        ..Default::default()
    });
    assert_eq!(large.restart_limit, RestartLimit::Limit(u32::MAX));
}

#[test]
fn restart_limit_infinity_round_trips_as_null() {
    // `JSON.stringify(Infinity)` writes `null`; the wire form matches so a log
    // or settings document written by either side reads back as "never park".
    assert_eq!(json!(null), json!(RestartLimit::Infinite));
    assert_eq!(json!(5), json!(RestartLimit::Limit(5)));
    let decoded: RestartLimit = serde_json::from_value(json!(null)).expect("null is infinite");
    assert_eq!(decoded, RestartLimit::Infinite);
    let decoded: RestartLimit = serde_json::from_value(json!(9)).expect("number is a limit");
    assert_eq!(decoded, RestartLimit::Limit(9));
}

// ── after_failed_run and modes ─────────────────────────────────────────────

#[test]
fn after_failed_run_defaults_to_keep() {
    let settings = resolve_settings(&HarnessSettings::default());
    assert_eq!(settings.after_failed_run, AfterFailedRun::Keep);
}

#[test]
fn after_failed_run_is_taken_as_given() {
    for (given, expected) in [
        (AfterFailedRun::Keep, AfterFailedRun::Keep),
        (AfterFailedRun::Continue, AfterFailedRun::Continue),
        (AfterFailedRun::Withdraw, AfterFailedRun::Withdraw),
    ] {
        let settings = resolve_settings(&HarnessSettings {
            after_failed_run: Some(given),
            ..Default::default()
        });
        assert_eq!(settings.after_failed_run, expected);
    }
}

#[test]
fn after_failed_run_wire_names_match_upstream() {
    assert_eq!(json!("keep"), json!(AfterFailedRun::Keep));
    assert_eq!(json!("continue"), json!(AfterFailedRun::Continue));
    assert_eq!(json!("withdraw"), json!(AfterFailedRun::Withdraw));
}

#[test]
fn queue_modes_default_to_one_at_a_time_and_are_independent() {
    let defaults = resolve_settings(&HarnessSettings::default());
    assert_eq!(defaults.steering_mode, QueueMode::OneAtATime);
    assert_eq!(defaults.follow_up_mode, QueueMode::OneAtATime);

    let settings = resolve_settings(&HarnessSettings {
        steering_mode: Some(QueueMode::All),
        ..Default::default()
    });
    assert_eq!(settings.steering_mode, QueueMode::All);
    assert_eq!(settings.follow_up_mode, QueueMode::OneAtATime);

    let settings = resolve_settings(&HarnessSettings {
        follow_up_mode: Some(QueueMode::All),
        ..Default::default()
    });
    assert_eq!(settings.steering_mode, QueueMode::OneAtATime);
    assert_eq!(settings.follow_up_mode, QueueMode::All);
}

#[test]
fn queue_mode_wire_names_are_kebab_case() {
    assert_eq!(json!("all"), json!(QueueMode::All));
    assert_eq!(json!("one-at-a-time"), json!(QueueMode::OneAtATime));
}

#[test]
fn tool_execution_defaults_to_parallel() {
    let defaults = resolve_settings(&HarnessSettings::default());
    assert_eq!(defaults.tool_execution, ToolExecutionMode::Parallel);

    let sequential = resolve_settings(&HarnessSettings {
        tool_execution: Some(ToolExecutionMode::Sequential),
        ..Default::default()
    });
    assert_eq!(sequential.tool_execution, ToolExecutionMode::Sequential);
    assert_eq!(json!("parallel"), json!(ToolExecutionMode::Parallel));
    assert_eq!(json!("sequential"), json!(ToolExecutionMode::Sequential));
}

// ── retention ──────────────────────────────────────────────────────────────

#[test]
fn retentions_take_zero_and_infinity() {
    let zero = resolve_settings(&HarnessSettings {
        context_retention_ms: Some(0.0),
        document_retention_ms: Some(0.0),
        ..Default::default()
    });
    assert_eq!(zero.context_retention_ms, 0.0);
    assert_eq!(zero.document_retention_ms, 0.0);

    let infinite = resolve_settings(&HarnessSettings {
        context_retention_ms: Some(f64::INFINITY),
        document_retention_ms: Some(f64::INFINITY),
        ..Default::default()
    });
    assert_eq!(infinite.context_retention_ms, f64::INFINITY);
    assert_eq!(infinite.document_retention_ms, f64::INFINITY);
}

#[test]
fn negative_document_retention_keeps_the_default() {
    let settings = resolve_settings(&HarnessSettings {
        document_retention_ms: Some(-1.0),
        ..Default::default()
    });
    assert_eq!(
        settings.document_retention_ms,
        DEFAULT_DOCUMENT_RETENTION_MS
    );
}

#[test]
fn custom_retentions_are_honored() {
    let settings = resolve_settings(&HarnessSettings {
        context_retention_ms: Some(1_234.5),
        document_retention_ms: Some(60_000.0),
        ..Default::default()
    });
    assert_eq!(settings.context_retention_ms, 1_234.5);
    assert_eq!(settings.document_retention_ms, 60_000.0);
}
