//! Regression tests for the core-layer round-2 review fixes:
//!
//! 1. **print transport hooks**: the print host's provider requests (loop
//!    stream, event stream, compaction/navigate summaries) must carry the
//!    per-request extension transport hooks
//!    (`before_provider_request` / `after_provider_response`) — upstream fires
//!    them from the session streamFn for EVERY provider request
//!    (sdk.ts:420-434); print-mode previously built bare `StreamOptions` and
//!    silently lost both extension events. Also: warm requests gain the same
//!    seam via `warm_once_with_hooks` (upstream replays warm requests through
//!    the same streamFn, cache-warmer.ts:332-339).
//! 2. **compaction usage scan**: `estimate_context_tokens` must use upstream's
//!    pure reverse scan (`getLastAssistantUsageInfo`, compaction.ts:184-190) —
//!    no prefix-timestamp guard. An out-of-order timestamp must NOT discard
//!    the genuinely-newest usage (the old guard over-counted the context via
//!    chars/4 and triggered compaction early). Stale usage after a
//!    compaction/context edit stays invalidated by the projection-level check
//!    (`estimateProjectedContextTokens`, compaction.ts:227-262).
//! 3. **RPC prompt claim**: the claim resets via an RAII guard even when the
//!    prompt task panics (the former post-await `store(false)` never ran, so
//!    every later prompt was rejected with "agent is streaming" forever).
//!
//! Fixtures follow tests/hook_timeout.rs (generated JS extensions in per-test
//! temp dirs) and tests/p2_fixes.rs (fake providers).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::json;

use pir::ai::auth::ApiKeyAuth;
use pir::ai::event_stream::AssistantMessageEventStream;
use pir::ai::provider::{Provider, ProviderRegistry};
use pir::ai::types::{
    AssistantMessage, ContentBlock, Context, Cost, Message, Model, ModelCost, ModelCostRates,
    ProviderStreamHooks, StopReason, TerminalReason, TextContent, Usage, UserMessage, UserRole,
};
use pir::cache_warmer::{warm_once, warm_once_with_hooks};
use pir::coding_agent::print::extension_transport_hooks;
use pir::coding_agent::rpc::PromptClaimGuard;
use pir::compaction::{estimate_context_tokens, estimate_projected_context_tokens, CutEntry};
use pir::extensions::{ExtensionConfig, ExtensionHandle};
use pir::session_manager::{CompactionEntry, CustomEntry, FileEntry};

// ---------------------------------------------------------------------------
// Extension fixture (tests/hook_timeout.rs pattern)
// ---------------------------------------------------------------------------

fn js_config(cwd: &Path) -> ExtensionConfig {
    ExtensionConfig {
        cwd: cwd.to_path_buf(),
        mode: "test".to_string(),
        has_ui: false,
        model: Some("test-model".to_string()),
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-core-round2-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One extension recording the observed transport-hook events on globalThis.
/// `before_provider_request` replaces the payload (its return value is the
/// replacement, prelude.rs:1568-1573); a probe-shaped payload makes it return
/// the recorded state instead, because informational events
/// (`after_provider_response`, unknown names) ignore handler return values
/// (prelude.rs default arm) and cannot carry state back to the host.
const TRANSPORT_HOOKS_EXTENSION_TS: &str = r#"
export default function (pi) {
  pi.on("before_provider_request", (event) => {
    if (event.payload && event.payload.__probe === true) {
      return {
        reqSeen: globalThis.__reqPayload === undefined ? null : globalThis.__reqPayload,
        respStatus: globalThis.__respStatus === undefined ? null : globalThis.__respStatus,
      };
    }
    globalThis.__reqPayload = event.payload;
    return { __replacedByExtension: true };
  });
  pi.on("after_provider_response", (event) => {
    globalThis.__respStatus = event.status;
    return undefined;
  });
}
"#;

async fn load_hooks_extension(label: &str) -> (PathBuf, ExtensionHandle) {
    let dir = temp_dir(label);
    let handle = ExtensionHandle::start(js_config(&dir)).unwrap();
    let path = dir.join("ext.ts");
    std::fs::write(&path, TRANSPORT_HOOKS_EXTENSION_TS).unwrap();
    let result = handle.load(std::slice::from_ref(&path)).await.unwrap();
    assert!(
        result.errors.is_empty(),
        "extension load errors: {:?}",
        result.errors
    );
    (dir, handle)
}

// ---------------------------------------------------------------------------
// Fake provider fixture (tests/p2_fixes.rs pattern)
// ---------------------------------------------------------------------------

fn model_with(provider: &str, id: &str) -> Model {
    Model {
        id: id.into(),
        name: format!("{provider}/{id}"),
        api: "fake-api".into(),
        provider: provider.into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".into()],
        cost: ModelCost {
            rates: ModelCostRates {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            tiers: Vec::new(),
        },
        context_window: 4096,
        max_tokens: 256,
        sampling_params: None,
        headers: None,
        thinking_token_budget_field: None,
        input_limits: None,
        prompt_cache: None,
        sampling_params_by_thinking_level: None,
        model_type: None,
        compat: None,
    }
}

fn user_message(text: &str) -> Message {
    Message::User(UserMessage {
        role: UserRole::User,
        content: vec![ContentBlock::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        tools_added: None,
        tools_removed: None,
        timestamp: 0,
        sections: None,
    })
}

fn assistant_with_usage(timestamp: u64, total_tokens: u64, stop_reason: StopReason) -> Message {
    Message::Assistant(AssistantMessage {
        api: "fake".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        content: Vec::new(),
        stop_reason,
        usage: Usage {
            total_tokens,
            cost: Cost::default(),
            ..Usage::default()
        },
        timestamp,
        ..Default::default()
    })
}

/// Records the per-request hooks each provider request carried.
type SeenHooks = Arc<Mutex<Option<ProviderStreamHooks>>>;

struct HookRecorder {
    seen: SeenHooks,
}

impl Provider for HookRecorder {
    fn id(&self) -> &str {
        "hookrec"
    }
    fn name(&self) -> &str {
        "hookrec"
    }
    fn get_models(&self) -> Vec<Model> {
        vec![model_with("hookrec", "rec-model")]
    }
    fn api_key_auth(&self) -> Vec<ApiKeyAuth> {
        vec![ApiKeyAuth::new("fake key", vec!["FAKE_TEST_KEY".into()])]
    }
    fn stream(
        &self,
        _model: &Model,
        _context: Context,
        options: pir::ai::types::StreamOptions,
    ) -> AssistantMessageEventStream {
        *self.seen.lock().unwrap() = Some(options.hooks);
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEventStream::done_event(
            TerminalReason::Stop,
            AssistantMessage {
                api: "fake".into(),
                provider: "hookrec".into(),
                model: "rec-model".into(),
                stop_reason: StopReason::Stop,
                ..Default::default()
            },
        ));
        stream
    }
}

// ---------------------------------------------------------------------------
// 1a. print transport hooks dispatch through the extension runtime
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn print_transport_hooks_dispatch_through_the_extension_runtime() {
    let (dir, handle) = load_hooks_extension("print-hooks").await;
    let hooks = extension_transport_hooks(Some(&handle));
    assert!(
        hooks.before_provider_request.is_some(),
        "print requests must carry the before_provider_request hook"
    );
    assert!(
        hooks.after_provider_response.is_some(),
        "print requests must carry the after_provider_response hook"
    );

    // The payload hook dispatches through the extension and the handler's
    // return value replaces the payload (prelude.rs:1568-1573).
    let replaced =
        hooks.before_provider_request.as_ref().unwrap()(json!({ "model": "probe-model" })).await;
    assert_eq!(
        replaced,
        json!({ "__replacedByExtension": true }),
        "the extension's replacement payload must flow back to the transport"
    );

    // The response observer dispatches (return value ignored by design).
    hooks.after_provider_response.as_ref().unwrap()(
        418,
        BTreeMap::from([("x-hdr".to_string(), "y".to_string())]),
    )
    .await;

    // Probe through the SAME hook channel: the extension recorded the
    // original request payload and the observed response status.
    let observed =
        hooks.before_provider_request.as_ref().unwrap()(json!({ "__probe": true })).await;
    assert_eq!(
        observed["reqSeen"]["model"],
        json!("probe-model"),
        "the extension must have seen the original request payload"
    );
    assert_eq!(
        observed["respStatus"],
        json!(418),
        "the extension must have observed the after_provider_response status"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "current_thread")]
async fn print_transport_hooks_absent_without_a_runtime_or_handlers() {
    // No runtime at all: hooks stay unset so extension-free runs never touch
    // the transport.
    let hooks = extension_transport_hooks(None);
    assert!(hooks.before_provider_request.is_none());
    assert!(hooks.after_provider_response.is_none());
    assert!(hooks.on_payload.is_none());
    assert!(hooks.on_response.is_none());

    // A runtime without registered handlers: same.
    let dir = temp_dir("print-hooks-empty");
    let handle = ExtensionHandle::start(js_config(&dir)).unwrap();
    let hooks = extension_transport_hooks(Some(&handle));
    assert!(hooks.before_provider_request.is_none());
    assert!(hooks.after_provider_response.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 1b. warm requests carry the per-request transport hooks
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn warm_once_with_hooks_carries_the_hooks_to_the_provider_request() {
    let seen: SeenHooks = Arc::new(Mutex::new(None));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(HookRecorder {
        seen: Arc::clone(&seen),
    }));
    let model = model_with("hookrec", "rec-model");
    let context = Context {
        system_prompt: None,
        messages: vec![user_message("hi")],
        tools: None,
    };
    let hooks = ProviderStreamHooks {
        on_payload: Some(Arc::new(|_| {})),
        ..Default::default()
    };
    let result = warm_once_with_hooks(&registry, &model, &context, None, None, hooks).await;
    assert!(result.is_some(), "the warm request must complete");
    let carried = seen
        .lock()
        .unwrap()
        .clone()
        .expect("provider saw a request");
    assert!(
        carried.on_payload.is_some(),
        "the warm request must carry the explicit per-request hooks"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn warm_once_default_carries_no_hooks() {
    let seen: SeenHooks = Arc::new(Mutex::new(None));
    let mut registry = ProviderRegistry::default();
    registry.set_provider(Arc::new(HookRecorder {
        seen: Arc::clone(&seen),
    }));
    let model = model_with("hookrec", "rec-model");
    let context = Context {
        system_prompt: None,
        messages: vec![user_message("hi")],
        tools: None,
    };
    let result = warm_once(&registry, &model, &context, None, None).await;
    assert!(result.is_some(), "the warm request must complete");
    let carried = seen
        .lock()
        .unwrap()
        .clone()
        .expect("provider saw a request");
    assert!(carried.on_payload.is_none());
    assert!(carried.before_provider_request.is_none());
    assert!(carried.after_provider_response.is_none());
}

// ---------------------------------------------------------------------------
// 2. compaction usage scan (upstream getLastAssistantUsageInfo parity)
// ---------------------------------------------------------------------------

#[test]
fn estimate_uses_the_newest_usage_even_with_out_of_order_timestamps() {
    // The old prefix-timestamp guard dropped the assistant usage at t=1000
    // (it predates the preceding user message at t=5000), fell back to a
    // whole-context chars/4 estimate, and triggered compaction early.
    let messages = vec![
        user_message("u1"),
        {
            let mut message = assistant_with_usage(1000, 1000, StopReason::Stop);
            if let Message::Assistant(assistant) = &mut message {
                assistant.timestamp = 1000;
            }
            message
        },
        {
            // A later user message with an out-of-order (higher) timestamp;
            // its 4 chars estimate to ceil(4/4) = 1 trailing token.
            let mut message = user_message("tail");
            if let Message::User(user) = &mut message {
                user.timestamp = 9000;
            }
            message
        },
    ];
    let estimate = estimate_context_tokens(&messages);
    assert_eq!(estimate.usage_tokens, 1000, "the newest valid usage wins");
    assert_eq!(estimate.last_usage_index, Some(1));
    assert_eq!(estimate.trailing_tokens, 1);
    assert_eq!(estimate.tokens, 1001);
}

#[test]
fn estimate_skips_invalid_assistant_usage_in_the_reverse_scan() {
    // Reverse scan (upstream getLastAssistantUsageInfo): an error assistant
    // with tokens is not valid usage; the scan continues to the older valid
    // one.
    let messages = vec![
        assistant_with_usage(1000, 500, StopReason::Stop),
        assistant_with_usage(2000, 9999, StopReason::Error),
    ];
    let estimate = estimate_context_tokens(&messages);
    assert_eq!(estimate.usage_tokens, 500);
    assert_eq!(estimate.last_usage_index, Some(0));
}

#[test]
fn estimate_without_usage_falls_back_to_char_estimation() {
    let messages = vec![user_message("abcd"), user_message("efgh")];
    let estimate = estimate_context_tokens(&messages);
    assert_eq!(estimate.usage_tokens, 0);
    assert_eq!(estimate.last_usage_index, None);
    assert_eq!(estimate.tokens, 2);
}

#[test]
fn projected_estimate_still_invalidates_usage_after_a_compaction() {
    // The stale-usage concern the old timestamp guard claimed to address is
    // handled where upstream handles it: the projection-level invalidating
    // entry check (compaction.ts:227-262). A compaction entry AFTER the
    // usage entry invalidates it; without one the usage stays valid.
    let assistant_message = assistant_with_usage(1000, 1000, StopReason::Stop);
    let projected = vec![
        CutEntry {
            id: Some("m1".into()),
            ..CutEntry::from_message(user_message("q"))
        },
        CutEntry {
            id: Some("m2".into()),
            ..CutEntry::from_message(assistant_message.clone())
        },
        CutEntry {
            id: Some("m3".into()),
            ..CutEntry::from_message(user_message("more"))
        },
    ];
    let custom = |id: &str| {
        FileEntry::Custom(CustomEntry {
            custom_type: "test".into(),
            data: None,
            id: id.into(),
            parent_id: None,
            timestamp: String::new(),
        })
    };

    // No invalidating entry: the usage estimate stands.
    let branch_valid: Vec<FileEntry> = vec![custom("m1"), custom("m2"), custom("m3")];
    let estimate = estimate_projected_context_tokens(&projected, &branch_valid);
    assert_eq!(estimate.usage_tokens, 1000);

    // A compaction entry after the usage invalidates it: full char estimate.
    let branch_stale: Vec<FileEntry> = vec![
        custom("m1"),
        custom("m2"),
        custom("m3"),
        FileEntry::Compaction(CompactionEntry {
            id: "c1".into(),
            parent_id: None,
            timestamp: String::new(),
            summary: "summary".into(),
            first_kept_entry_id: String::new(),
            first_kept_entry_index: None,
            tokens_before: 0,
            details: None,
            usage: None,
            from_hook: None,
            system_message: None,
        }),
    ];
    let estimate = estimate_projected_context_tokens(&projected, &branch_stale);
    assert_eq!(estimate.usage_tokens, 0, "stale usage must be invalidated");
    assert_eq!(estimate.last_usage_index, None);
}

// ---------------------------------------------------------------------------
// 3. RPC prompt claim RAII guard
// ---------------------------------------------------------------------------

#[test]
fn prompt_claim_guard_resets_the_claim_on_drop() {
    let flag = Arc::new(AtomicBool::new(true));
    drop(PromptClaimGuard(Arc::clone(&flag)));
    assert!(
        !flag.load(Ordering::SeqCst),
        "dropping the guard must release the streaming claim"
    );
}

#[test]
fn prompt_claim_guard_resets_the_claim_when_the_task_panics() {
    let flag = Arc::new(AtomicBool::new(true));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = PromptClaimGuard(Arc::clone(&flag));
        panic!("simulated prompt task panic");
    }));
    assert!(
        result.is_err(),
        "the panic must propagate through the guard"
    );
    assert!(
        !flag.load(Ordering::SeqCst),
        "an unwinding prompt task must release the claim (the former post-await store never ran, wedging every later prompt behind 'agent is streaming')"
    );
}
