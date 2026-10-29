//! Multi-tool-call batch types shared by the scheduler and the agent loop.
//!
//! These mirror the upstream `packages/agent/src/types.ts` tool-call surface:
//! calls keep their source index, preflight happens in source order, tool
//! execution may run concurrently, and final results are always reassembled
//! in source order so session transcripts stay deterministic.

/// Global tool execution mode for a batch.
///
/// - `Sequential`: each tool call is prepared, executed, and finalized before
///   the next one starts (completion and source order coincide).
/// - `Parallel`: tool calls are prepared sequentially, then executable tools
///   run concurrently. `tool_execution_end` events are emitted in completion
///   order while results are returned in source order.
///
/// A tool whose own `execution_mode` is `Sequential` forces the whole batch to
/// degrade to sequential execution, matching upstream behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolExecutionMode {
    Sequential,
    #[default]
    Parallel,
}

/// A single tool call extracted from an assistant message.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentToolCall {
    /// Position of this call inside the assistant message content, in source order.
    pub index: usize,
    /// Provider tool call id; preserved verbatim in results.
    pub call_id: String,
    /// Tool name as requested by the model.
    pub tool_name: String,
    /// Raw tool arguments produced by the model.
    pub arguments: serde_json::Value,
    /// Final-parse failure reported by the provider (upstream
    /// `ToolCall.argumentsError`). When set, the scheduler rejects the call
    /// with a model-visible error instead of schema-validating `{}`.
    pub arguments_error: Option<String>,
}

impl AgentToolCall {
    /// Create a new tool call.
    pub fn new(
        index: usize,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        arguments: serde_json::Value,
    ) -> Self {
        Self {
            index,
            call_id: call_id.into(),
            tool_name: tool_name.into(),
            arguments,
            arguments_error: None,
        }
    }
}

/// A tool call that passed lookup and argument validation.
#[derive(Debug, Clone)]
pub struct PreparedToolCall {
    /// Source-order position of the original call.
    pub index: usize,
    /// Provider tool call id.
    pub call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// Validated arguments passed to the tool.
    pub validated_args: serde_json::Value,
    /// Canonical mutation paths this tool will touch (empty = no mutation).
    pub mutation_paths: Vec<crate::agent::tool_dependencies::ResourceKey>,
    /// Tool-level execution mode; `Sequential` degrades the whole batch.
    pub execution_mode: ToolExecutionMode,
}

/// Executed tool output.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    /// Text content of the result.
    pub content: String,
    /// Image content blocks appended after the text (upstream tool results
    /// carry `{type: "image", data, mimeType}` blocks; the `read` tool emits
    /// one for image files). Not sent to non-vision models — the adapter
    /// transform downgrades them to placeholders.
    pub images: Vec<crate::ai::types::ImageContent>,
    /// Optional structured details payload.
    pub details: Option<serde_json::Value>,
    /// Machine-readable result matching the tool's declared output schema, for
    /// programmatic callers (upstream `AgentToolResult.structuredContent`).
    /// Not sent to the model; `content` remains the model-facing result.
    pub structured_content: Option<serde_json::Value>,
    /// Whether this execution is treated as an error.
    pub is_error: bool,
    /// Hint that the agent should stop after this batch; only stops when
    /// every finalized result in the batch sets this to true.
    pub terminate: bool,
}

impl ToolResult {
    /// A successful result with text content.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            images: Vec::new(),
            details: None,
            structured_content: None,
            is_error: false,
            terminate: false,
        }
    }

    /// An error result with a message and optional structured content.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: message.into(),
            images: Vec::new(),
            details: None,
            structured_content: None,
            is_error: true,
            terminate: false,
        }
    }

    /// Error result that also requests batch termination.
    pub fn error_terminate(message: impl Into<String>) -> Self {
        Self {
            content: message.into(),
            images: Vec::new(),
            details: None,
            structured_content: None,
            is_error: true,
            terminate: true,
        }
    }
}

/// A finalized tool execution: result plus source-order bookkeeping.
#[derive(Debug, Clone, PartialEq)]
pub struct FinalizedToolCall {
    /// Source-order index.
    pub index: usize,
    /// Provider tool call id.
    pub call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// Executed (or synthesized) result.
    pub result: ToolResult,
    /// Usage reported by the `afterToolCall` hook (upstream
    /// `AgentToolResult.usage`). Not part of [`ToolResult`] in pir, so the
    /// finalization carries it to the tool-result message.
    pub usage: Option<crate::ai::types::Usage>,
    /// Milliseconds `execute()` took, measured with a monotonic clock
    /// (upstream `AgentToolCallOutcome.durationMs`); `None` when the tool did
    /// not run (blocked, aborted, or skipped).
    pub duration_ms: Option<u64>,
}

impl FinalizedToolCall {
    /// Whether this finalized call carries an error result.
    pub fn is_error(&self) -> bool {
        self.result.is_error
    }

    /// Whether this finalized call requests batch termination.
    pub fn terminate(&self) -> bool {
        self.result.terminate
    }
}

/// A tool call a host adopted for background execution: it finishes after the
/// turn, and its real result is delivered out of band.
///
/// The future is exactly the execution a foreground call would await
/// (per-file mutation locks and the after-hook included), already `'static`:
/// everything it needs was moved into it. `abort` is the call's OWN signal,
/// never the run's — aborting the run must not cancel a background call, and
/// aborting a background call must not touch the run.
pub struct DetachedToolCall {
    /// Source-order position of the original call.
    pub index: usize,
    /// Provider tool call id; preserved verbatim for the real result.
    pub call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// Validated arguments with the `background` key already stripped.
    pub args: serde_json::Value,
    /// The requesting call id when the call was made by another tool
    /// (`parentToolCallId`); `None` for model-issued calls. Nested calls can
    /// detach too (`ctx.executeTool(name, args, { background: true })`), and
    /// the batch context's nested executor supplies their id here; the
    /// placeholder envelope still carries their own call id, and the
    /// completion's `tool_result` event correlates by `toolCallId` and
    /// additionally reports this id as `parentToolCallId` (when `Some`).
    pub parent_call_id: Option<String>,
    /// The detached call's own abort signal (see the type docs).
    pub abort: crate::ai::types::AbortSignal,
    /// The execution future. `Some` while the call awaits adoption; the sink
    /// TAKES it when it adopts the call (see [`Self::take_future`]), which
    /// leaves the call in the batch's `detached` report usable for
    /// bookkeeping — ids, arguments, abort signal — without a second future.
    pub future: Option<crate::agent::tool_execution::BoxFuture<'static, ToolResult>>,
}

impl DetachedToolCall {
    /// Take the execution future out of the call (the adopter's move): the
    /// returned future runs exactly once, and the call keeps only its
    /// bookkeeping fields.
    pub fn take_future(
        &mut self,
    ) -> Option<crate::agent::tool_execution::BoxFuture<'static, ToolResult>> {
        self.future.take()
    }

    /// Whether an adopter already took the future.
    pub fn is_adopted(&self) -> bool {
        self.future.is_none()
    }
}

impl std::fmt::Debug for DetachedToolCall {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The future is not `Debug`; report its presence only.
        formatter
            .debug_struct("DetachedToolCall")
            .field("index", &self.index)
            .field("call_id", &self.call_id)
            .field("tool_name", &self.tool_name)
            .field("args", &self.args)
            .field("parent_call_id", &self.parent_call_id)
            .field("abort", &self.abort)
            .finish_non_exhaustive()
    }
}

/// Tool execution lifecycle events.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolExecutionEvent {
    /// A tool call started execution (`tool_execution_start`).
    Start {
        index: usize,
        call_id: String,
        tool_name: String,
        /// Set when the call was made by another tool
        /// (upstream `parentToolCallId`); `None` for model-issued calls.
        parent_call_id: Option<String>,
    },
    /// A tool call finished execution (`tool_execution_end`).
    End {
        index: usize,
        call_id: String,
        tool_name: String,
        is_error: bool,
        /// Monotonic execution duration when the tool ran.
        duration_ms: Option<u64>,
        /// Set when the call was made by another tool
        /// (upstream `parentToolCallId`); `None` for model-issued calls.
        parent_call_id: Option<String>,
    },
}

/// Outcome of executing one assistant-message tool batch.
///
/// Not `Clone`: a detached call owns a one-shot execution future that must run
/// exactly once, so the outcome carrying it cannot be copied.
#[derive(Debug, Default)]
pub struct ToolBatchOutcome {
    /// Start events in assistant source order.
    pub execution_start: Vec<ToolExecutionEvent>,
    /// End events in actual completion order (parallel mode only).
    pub execution_end: Vec<ToolExecutionEvent>,
    /// Finalized results in assistant source order; always 1:1 with calls.
    pub results: Vec<FinalizedToolCall>,
    /// Calls a host adopted for background execution. Their finalized result
    /// above is the placeholder; the real result is delivered by whoever
    /// adopted the call.
    pub detached: Vec<DetachedToolCall>,
}

impl ToolBatchOutcome {
    /// True when the batch is non-empty and every finalized result requests
    /// termination. Matches the upstream "should terminate" rule: a mixed
    /// batch must continue.
    pub fn should_terminate(&self) -> bool {
        !self.results.is_empty() && self.results.iter().all(|result| result.terminate())
    }

    /// Total number of calls this batch was asked to execute.
    pub fn len(&self) -> usize {
        self.results.len()
    }

    /// Whether the batch contains no calls.
    pub fn is_empty(&self) -> bool {
        self.results.is_empty()
    }
}
