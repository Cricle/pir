//! The durable agent harness: settings, the extension registry, and agent
//! resolution.
//!
//! Ports the engine-free first slice of upstream `@earendil-works/pi-durable`'s
//! `src/harness/`: what a harness is configured with, how extension code is
//! registered and published, and how a conversation's stored `pi.agent`
//! resolves to the model, tools and prompt sections a request uses.
//!
//! | module | upstream | contents |
//! |---|---|---|
//! | [`settings`] | `harness/types.ts`, `harness/agent.ts` | [`HarnessSettings`], [`Settings`], policies |
//! | [`registry`] | `harness/registry.ts`, `harness/define.ts` | [`Extension`], [`Registry`], snapshots |
//! | [`agent`] | `harness/agent.ts`, `harness/types.ts` | [`AgentState`], [`AgentChange`], [`resolve_agent`] |
//!
//! # What is deliberately not here yet
//!
//! The rest of `harness/` (`scheduler`, `generation`, `tool`, `compaction`,
//! `prompt`, `live`, `events`, `view`) needs the pi-ai message model and the
//! task runtime; it lands in later lanes beside these modules. Everything here
//! is engine-free on purpose: tools are opaque handles, sections are
//! callbacks, and hooks are opaque handler values matched by task name. The
//! join points are documented on each type in [`registry`].
//!
//! # Persistence
//!
//! Only [`AgentState`] is durable — it is the `pi.agent` conversation document
//! (`version: 1`, `history: rewindable`, `fork: asOf`). Settings and the
//! registry are host configuration and are never persisted; a registry is
//! process-local and may outlive a Harness.

pub mod agent;
pub mod compaction;
pub mod context;
pub mod documents;
pub mod events;
pub mod generation;
pub mod inbox;
pub mod json;
pub mod live;
pub mod prompt;
pub mod registry;
pub mod settings;
pub mod submissions;
pub mod usage;

pub use agent::{
    configure, hooks_for_task, resolve_agent, resolve_agent_with, section_extension,
    tool_extension, Agent, AgentChange, AgentState, Change, ExtensionEdit, ExtensionSelection,
    ModelRef, ModelToolSelection, ModelToolsEdit, ToolEdit, ToolSelection, INSTRUCTIONS_KEY,
};
pub use registry::{
    builtin_tasks, create_registry, BoxedHook, BoxedTool, Extension, HookRegistration, PromptInput,
    PromptSection, Registry, RegistryError, RegistryListener, RegistryReader, RegistrySnapshot,
    ReplayPolicy, SectionRenderer, SectionWrapper, Subscription, ToolCaller, ToolRegistration,
    ToolWrapper, Wrap,
};
pub use settings::{
    resolve_compaction_for, resolve_settings, AfterFailedRun, CompactionPolicy, CompactionSettings,
    ConversationRetryPolicy, ConversationRetrySettings, ConversationStreamOptions, DeferredWindow,
    HarnessSettings, PartialCompactionPolicy, ProgressPolicy, ProgressSettings, QueueMode,
    RestartLimit, Settings, StreamDeferred, StreamTransport, ToolExecutionMode,
    DEFAULT_COMPACTION_BACKGROUND_TOKENS, DEFAULT_COMPACTION_ENABLED,
    DEFAULT_COMPACTION_KEEP_RECENT_TOKENS, DEFAULT_COMPACTION_RESERVE_TOKENS,
    DEFAULT_CONTEXT_RETENTION_MS, DEFAULT_DOCUMENT_RETENTION_MS, DEFAULT_OUTPUT_INTERVAL_MS,
    DEFAULT_PARTIAL_INTERVAL_MS, DEFAULT_RESTART_LIMIT, DEFAULT_RETRY_BASE_DELAY_MS,
    DEFAULT_RETRY_ENABLED, DEFAULT_RETRY_MAX_AGENT_DELAY_MS, DEFAULT_RETRY_MAX_RETRIES,
};

pub use compaction::{
    active_range_start, add_compaction_status, calculate_context_tokens, compaction_ownership,
    compaction_policy, compaction_request_id, compaction_status, compaction_status_mut,
    compaction_status_of, compaction_task, estimate_context, estimate_message_tokens,
    estimate_text_tokens, fail_compaction_status, order_tool_results, overflow_compaction,
    remove_compaction_status, retry_until, select_cut, serialize_conversation, summarized_messages,
    summary_entry, summary_entry_draft, summary_failure, summary_is_stale, summary_max_tokens,
    summary_prompt, summary_request_options, summary_text, threshold_compaction, truncate,
    wrap_summary, CompactionAgent, CompactionCheckpoint, CompactionCompletion, CompactionDecision,
    CompactionDraft, CompactionHooks, CompactionHost, CompactionInput, CompactionModel,
    CompactionReason, CompactionResult, CompactionTaskError, CompactionTrigger, NoCompactionHooks,
    PlacedSummary, PlacementAction, SummaryPlacement, SummaryRequest, COMPACTION_TASK_NAME,
    COMPACTION_TASK_VERSION, ESTIMATED_IMAGE_CHARS, SUMMARIZATION_PROMPT,
    SUMMARIZATION_SYSTEM_PROMPT, SUMMARY_PREFIX, SUMMARY_SUFFIX, TOOL_RESULT_MAX_CHARS,
};
pub use context::{derive_context, ContextView, MISSING_RESULT_TEXT};
pub use documents::{
    agent_definition, agent_seed, builtin_conversation_documents, conversation_definitions,
    provider_definition, ProviderState, AGENT_KIND, AGENT_VERSION, PROVIDER_KIND, PROVIDER_VERSION,
};
pub use events::{
    held_generations, message_changes, snapshot_of, tool_update, translate, watch_events,
    watch_events_with_held, AgentEvent, EventsStream, MessageChange, NestedCallState,
    NestedToolExecutionResult, QueuedItem, QueuedMode, RunInputs, SnapshotEvent, ToolEventCall,
    ToolOutputChange, ToolUpdate, Tri, ViewPort, GENERATION_TASK_KIND, MAX_PENDING_EVENT_BATCHES,
    NESTED_CALL_KIND,
};
pub use inbox::{
    boundary, check_input_entry, has_queued_reset, is_display_only, is_stale, is_stale_entry,
    remove_selected, withdraw_inputs, Boundary, InboxItem, InboxState, InputEntry, InputMode,
    InvalidInputEntry, Selected, SelectedKind, INBOX_DEFINITION, INBOX_KIND, INBOX_VERSION,
    REASON_ABORTED, REASON_STALE,
};
pub use json::{assign_json, assign_json_at, assign_then_diff};
pub use live::{
    apply_output_chunk, find_nested_slot, find_tool_slot, set_slot_done, set_slot_removed,
    set_slot_running, CompactionFailure, CompactionStatus, DeferredStatus, GenerationState,
    LiveState, NestedToolSlot, NestedToolSummary, OutputSkip, RetryStatus, RunState, SlotLocation,
    SlotProgress, SlotStatus, ToolDiagnostic, ToolSlot, LIVE_DEFINITION, LIVE_KIND, LIVE_VERSION,
};
pub use prompt::{plan_system_entries, render_sections, replay_sections};
pub use submissions::{
    admission_status, admit, admit_in_inbox, existing_type_name, find_request_submission,
    queued_item, resolve_request_id, settle_run_inputs, Admission, AdmissionStatus,
    RequestIdConflict, RequestIdResolution, RunOutcome, SubmissionDraft, WhenBusy, REASON_FAULTED,
    REASON_RUN_FAILED,
};
pub use usage::{
    add_usage, add_usage_state, usage_add, UsageScope, UsageState, USAGE_DEFINITION, USAGE_KIND,
    USAGE_VERSION,
};

// The executable task runtime and scheduler (upstream `harness/{tasks,types,scheduler}.ts`).
pub mod scheduler;
pub mod task;
pub mod tool;

// Structural mounts and their shared helpers (upstream `harness/{view,task-graph,util}.ts`).
#[path = "task-graph.rs"]
pub mod task_graph;
pub mod util;
pub mod view;

pub use task_graph::{
    blocked_reason_of, BlockedListener, BlockedSubject, NoBlocking, OutcomeStatus, TaskGraph,
    TaskGraphBlocking, TaskGraphFrame, TaskGraphNode, TaskGraphState, TaskGraphView,
    TaskGraphWatch, LIVE_STATUSES,
};
pub use util::{closed_error, contained, optional_extension, scan_all, HasFailure, Report};
pub use view::{
    AttachedView, ConversationView, ConversationViews, Detach, MountedDocument, ViewObserver,
};

// The `Harness` facade, its provider identity and its telemetry vocabulary
// (upstream `harness/{harness,provider,telemetry}.ts`).
#[allow(clippy::module_inception)] // `harness::harness` mirrors upstream's layout.
pub mod harness;
pub mod provider;
pub mod telemetry;

pub use harness::{
    agent_address, usage_address, Conversation, ConversationFrame, ConversationWatch,
    DocumentWatch, Harness, HarnessError, HarnessInspection, HarnessOptions, ReportHandler,
    Submission, SubmissionAbortResult, SCAN_PAGE_SIZE,
};
pub use provider::{ensure_provider_session_id, mint_session_id, provider_address, provider_doc};
pub use telemetry::{
    AttributeDescriptor, AttributeType, Cardinality, DurableTelemetrySchema, EventDescriptor,
    MetricDescriptor, MetricKind, NativeTelemetrySink, SpanDescriptor, CHECKPOINT_EVENT,
    DURABLE_TELEMETRY_SCHEMA, ENTRY_EVENT, MAX_TRACE_CONTEXTS, PHASE_DURATION_METRIC, PHASE_SPAN,
    REPLAY_EVENT, REPORTS_METRIC, REPORT_ATTRIBUTES, REPORT_EVENT, REPORT_REASONS, REPORT_TYPES,
    SUBMISSION_EVENT, SUBMIT_SPAN, TASKS_ACTIVE_METRIC,
};
