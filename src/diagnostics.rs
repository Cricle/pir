//! Run diagnostics: a per-turn breakdown of where a run's wall time went,
//! assembled ONLY from events the agent loop already emits.
//!
//! The collector is fed from the host layer that consumes the loop's event
//! stream — print mode's event handler (`src/coding_agent/print.rs`) and the
//! session's per-run event wrapper (`src/agent_session/prompt_loop.rs`) — so
//! `src/agent/**` and `src/ai/**` stay untouched. Everything is derived from
//! the receipt times of `StreamStart` / `StreamEvent` / `StreamEnd`, the
//! finalized assistant message's usage, and the `ToolExecutionEnd` duration
//! the scheduler already measures:
//!
//! * `provider_ms` — request → first token: the gap from `StreamStart` to the
//!   first delta event (`text_delta` / `thinking_delta` / `toolcall_delta`).
//!   The provider adapters push their `Start` event immediately before the
//!   request is dispatched, so this gap is the first-token wait (transport
//!   retry waits included). `0` when the stream produced no delta (e.g. an
//!   immediate provider error — that wait then shows up in `total_ms`).
//! * `stream_ms` — the provider stream span: `StreamStart` → `StreamEnd`.
//! * `total_ms` — the whole turn: `TurnStart` → `TurnEnd` (provider wait +
//!   streaming + tool batches + loop bookkeeping).
//! * `deltas` — the number of delta events in the turn (text, thinking, and
//!   tool-call JSON chunks).
//! * `tool_calls` — one entry per `ToolExecutionEnd`: tool name, the
//!   scheduler's monotonic `duration_ms` (which INCLUDES any per-file queue
//!   wait inside the call — the scheduler starts its clock before acquiring
//!   the mutation queues), whether the result was an error, and whether the
//!   call was nested (issued by another tool).
//! * `usage` — summed from the `StreamEnd` (wire `message_end`) assistant
//!   message's usage.
//!
//! All timestamps are the collector's own receipt times (`Instant`): the loop
//! event payloads carry no per-event clock. [`RunDiagnosticsCollector::record`]
//! reads the clock itself; [`RunDiagnosticsCollector::record_at`] takes an
//! explicit `Instant` and is the deterministic test seam (no sleeps).
//!
//! Enablement: the collector is inert unless constructed enabled —
//! [`RunDiagnosticsCollector::record`] returns on the first `bool` check, so a
//! disabled collector costs one branch per event and allocates nothing. Print
//! mode enables it with the existing `PI_TIMING=1` gate
//! ([`crate::config::timings::timing_enabled`]); `AgentSession` always
//! collects (counter-only work) so hosts can render
//! [`AgentSession::last_run_diagnostics`](crate::agent_session::AgentSession::last_run_diagnostics).
//!
//! Wire shape (print mode's `--mode json` final frame, `"type":
//! "run_diagnostics"`; see [`RunDiagnostics::to_wire_json`]):
//!
//! ```json
//! {
//!   "type": "run_diagnostics",
//!   "wallMs": 12345,
//!   "turns": [
//!     {
//!       "turn": 1,
//!       "providerMs": 1240, "streamMs": 8320, "totalMs": 9910,
//!       "deltas": 142,
//!       "toolCalls": [
//!         { "name": "bash", "ms": 4210, "isError": false, "nested": false }
//!       ],
//!       "usage": {
//!         "input": 100, "output": 50,
//!         "cacheRead": 20, "cacheWrite": 5, "totalTokens": 175
//!       }
//!     }
//!   ],
//!   "total": {
//!     "turns": 1, "providerMs": 1240, "streamMs": 8320, "totalMs": 9910,
//!     "deltas": 142, "toolCalls": 1,
//!     "usage": { "input": 100, "output": 50, "cacheRead": 20,
//!                "cacheWrite": 5, "totalTokens": 175 }
//!   },
//!   "slowestTurn": { "turn": 1, "totalMs": 9910 },
//!   "slowestTool": { "name": "bash", "ms": 4210, "turn": 1 }
//! }
//! ```
//!
//! `slowestTurn` / `slowestTool` serialize as `null` when the run had no
//! timed turn / tool call. Turn numbers are 1-based over the collector's whole
//! event stream: a print invocation with several prompts numbers its agent
//! runs consecutively (the loop's own per-run `turn` numbering restarts at 1).

use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::agent::agent_loop::AgentEvent;
use crate::agent::tool_batch::ToolExecutionEvent;
use crate::ai::types::{AssistantMessageEvent, Usage};

/// One completed run's diagnostics: the per-turn breakdown plus the run
/// totals and the two convenience extremes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunDiagnostics {
    /// Wall time from the first `agent_start` to the last observed event.
    pub wall_ms: u64,
    /// One entry per turn, in event order (1-based `turn` numbers).
    pub turns: Vec<TurnDiagnostics>,
    /// Run totals (sums over `turns`).
    pub total: RunTotals,
    /// The turn with the largest `total_ms`, when the run had any turn.
    pub slowest_turn: Option<SlowestTurn>,
    /// The single tool call with the largest `ms`, when any call reported a
    /// duration.
    pub slowest_tool: Option<SlowestTool>,
}

impl RunDiagnostics {
    /// The JSON wire frame: `{"type": "run_diagnostics", ...summary fields}`
    /// (the `type` key first, matching the stream's other frames).
    pub fn to_wire_json(&self) -> serde_json::Value {
        let mut value = serde_json::json!({ "type": "run_diagnostics" });
        let fields = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let (Some(object), Some(fields)) = (value.as_object_mut(), fields.as_object()) {
            object.extend(fields.clone());
        }
        value
    }

    /// The compact aligned human-readable block (no color):
    ///
    /// ```text
    /// --- Run Diagnostics ---
    ///  turn  provider  stream   total  deltas  tools
    ///     1     200ms   900ms  1100ms       3  2 calls: bash 4000ms, read 12ms
    ///     2     150ms   250ms   400ms       1  -
    /// TOTAL     350ms  1150ms  1500ms       4  2 calls
    /// usage: input=110 output=52 cache_read=20 cache_write=5 total=187
    /// slowest_turn: 1 (1100ms)
    /// slowest_tool: bash (4000ms, turn 1)
    /// ```
    ///
    /// A tool whose result carried no duration renders as `name -`; an error
    /// result is marked with a trailing `!`. Empty tool cells render `-`.
    pub fn render(&self) -> String {
        // Columns: turn / provider / stream / total / deltas / tools. The
        // TOTAL row's cells join each numeric column's cell list (as its last
        // element) so every column is wide enough for the totals too.
        let mut turn_cells: Vec<String> = self.turns.iter().map(|t| t.turn.to_string()).collect();
        let mut provider_cells: Vec<String> = self
            .turns
            .iter()
            .map(|t| format!("{}ms", t.provider_ms))
            .collect();
        let mut stream_cells: Vec<String> = self
            .turns
            .iter()
            .map(|t| format!("{}ms", t.stream_ms))
            .collect();
        let mut total_cells: Vec<String> = self
            .turns
            .iter()
            .map(|t| format!("{}ms", t.total_ms))
            .collect();
        let mut delta_cells: Vec<String> =
            self.turns.iter().map(|t| t.deltas.to_string()).collect();
        let tool_cells: Vec<String> = self.turns.iter().map(render_tools).collect();

        // The TOTAL row's cells (one past the per-turn cells).
        turn_cells.push("TOTAL".to_string());
        provider_cells.push(format!("{}ms", self.total.provider_ms));
        stream_cells.push(format!("{}ms", self.total.stream_ms));
        total_cells.push(format!("{}ms", self.total.total_ms));
        delta_cells.push(self.total.deltas.to_string());

        let width = |header: &str, cells: &[String]| {
            cells
                .iter()
                .map(|cell| cell.chars().count())
                .chain(std::iter::once(header.chars().count()))
                .max()
                .unwrap_or(header.chars().count())
        };
        let w0 = width("turn", &turn_cells);
        let w1 = width("provider", &provider_cells);
        let w2 = width("stream", &stream_cells);
        let w3 = width("total", &total_cells);
        let w4 = width("deltas", &delta_cells);

        let tool_calls_cell = format!(
            "{}{}",
            self.total.tool_calls,
            if self.total.tool_calls == 1 {
                " call"
            } else {
                " calls"
            }
        );

        let mut lines = Vec::with_capacity(self.turns.len() + 6);
        lines.push("--- Run Diagnostics ---".to_string());
        lines.push(format!(
            "{:>w0$}  {:>w1$}  {:>w2$}  {:>w3$}  {:>w4$}  tools",
            "turn", "provider", "stream", "total", "deltas",
        ));
        for index in 0..self.turns.len() {
            lines.push(format!(
                "{:>w0$}  {:>w1$}  {:>w2$}  {:>w3$}  {:>w4$}  {}",
                turn_cells[index],
                provider_cells[index],
                stream_cells[index],
                total_cells[index],
                delta_cells[index],
                tool_cells[index],
            ));
        }
        // The TOTAL row reads the compound cells' last elements.
        let total_index = self.turns.len();
        lines.push(format!(
            "{:>w0$}  {:>w1$}  {:>w2$}  {:>w3$}  {:>w4$}  {}",
            turn_cells[total_index],
            provider_cells[total_index],
            stream_cells[total_index],
            total_cells[total_index],
            delta_cells[total_index],
            tool_calls_cell,
        ));
        lines.push(format!(
            "usage: input={} output={} cache_read={} cache_write={} total={}",
            self.total.usage.input,
            self.total.usage.output,
            self.total.usage.cache_read,
            self.total.usage.cache_write,
            self.total.usage.total_tokens,
        ));
        lines.push(format!(
            "slowest_turn: {}",
            match &self.slowest_turn {
                Some(slowest) => format!("{} ({}ms)", slowest.turn, slowest.total_ms),
                None => "-".to_string(),
            }
        ));
        lines.push(format!(
            "slowest_tool: {}",
            match &self.slowest_tool {
                Some(slowest) =>
                    format!("{} ({}ms, turn {})", slowest.name, slowest.ms, slowest.turn),
                None => "-".to_string(),
            }
        ));
        lines.join("\n")
    }

    /// Print [`Self::render`] to stderr, with a leading blank line (the
    /// startup-timings report's layout).
    pub fn print_to_stderr(&self) {
        eprintln!("\n{}", self.render());
    }
}

/// One turn's breakdown (see the module docs for how each field is derived).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnDiagnostics {
    /// 1-based turn number in event order.
    pub turn: usize,
    /// Request → first token (first delta after `StreamStart`).
    pub provider_ms: u64,
    /// `StreamStart` → `StreamEnd`.
    pub stream_ms: u64,
    /// `TurnStart` → `TurnEnd`.
    pub total_ms: u64,
    /// Delta events in the turn (text / thinking / tool-call JSON chunks).
    pub deltas: u64,
    /// One entry per finished tool call.
    pub tool_calls: Vec<ToolCallDiagnostics>,
    /// Usage summed from the turn's assistant message(s).
    pub usage: UsageDiagnostics,
}

/// One finished tool call (`ToolExecutionEnd`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallDiagnostics {
    /// Tool name.
    pub name: String,
    /// The scheduler's monotonic execution duration (includes any per-file
    /// queue wait inside the call); `None` for calls that never executed
    /// (e.g. argument-validation failures).
    pub ms: Option<u64>,
    /// Whether the result was an error.
    pub is_error: bool,
    /// Whether the call was issued by another tool (nested).
    pub nested: bool,
}

/// Run totals: sums over every turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunTotals {
    /// Number of turns in the run.
    pub turns: usize,
    /// Sum of the turns' `provider_ms`.
    pub provider_ms: u64,
    /// Sum of the turns' `stream_ms`.
    pub stream_ms: u64,
    /// Sum of the turns' `total_ms`.
    pub total_ms: u64,
    /// Sum of the turns' `deltas`.
    pub deltas: u64,
    /// Total number of tool calls.
    pub tool_calls: usize,
    /// Summed usage.
    pub usage: UsageDiagnostics,
}

/// The turn with the largest `total_ms`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlowestTurn {
    /// The turn number.
    pub turn: usize,
    /// The turn's `total_ms`.
    pub total_ms: u64,
}

/// The single tool call with the largest `ms`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlowestTool {
    /// Tool name.
    pub name: String,
    /// The call's duration.
    pub ms: u64,
    /// The turn the call belongs to.
    pub turn: usize,
}

/// Token usage in the diagnostics shape (`message_end` usage).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageDiagnostics {
    /// Prompt tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Tokens read from the provider's prompt cache.
    pub cache_read: u64,
    /// Tokens written to the provider's prompt cache.
    pub cache_write: u64,
    /// Total tokens as the provider reported them.
    pub total_tokens: u64,
}

impl UsageDiagnostics {
    /// Fold one assistant message's usage into this accumulator.
    pub fn add(&mut self, usage: &Usage) {
        self.input = self.input.saturating_add(usage.input);
        self.output = self.output.saturating_add(usage.output);
        self.cache_read = self.cache_read.saturating_add(usage.cache_read);
        self.cache_write = self.cache_write.saturating_add(usage.cache_write);
        self.total_tokens = self.total_tokens.saturating_add(usage.total_tokens);
    }
}

/// One turn being recorded.
struct TurnState {
    /// `TurnStart` receipt.
    started_at: Instant,
    /// `StreamStart` receipt.
    stream_started_at: Option<Instant>,
    /// First delta receipt (`provider_ms`'s right edge).
    first_delta_at: Option<Instant>,
    stream_ms: u64,
    total_ms: u64,
    deltas: u64,
    tool_calls: Vec<ToolCallDiagnostics>,
    usage: UsageDiagnostics,
}

impl TurnState {
    fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            stream_started_at: None,
            first_delta_at: None,
            stream_ms: 0,
            total_ms: 0,
            deltas: 0,
            tool_calls: Vec::new(),
            usage: UsageDiagnostics::default(),
        }
    }

    fn summary(&self, turn: usize) -> TurnDiagnostics {
        let provider_ms = self
            .first_delta_at
            .map(|at| ms_between(self.stream_started_at.unwrap_or(self.started_at), at))
            .unwrap_or(0);
        TurnDiagnostics {
            turn,
            provider_ms,
            stream_ms: self.stream_ms,
            total_ms: self.total_ms,
            deltas: self.deltas,
            tool_calls: self.tool_calls.clone(),
            usage: self.usage,
        }
    }
}

/// The whole run's recording state (behind the collector's mutex).
#[derive(Default)]
struct CollectorState {
    /// First `AgentStart` / observed event.
    started_at: Option<Instant>,
    /// The last observed event (the wall-clock right edge).
    last_at: Option<Instant>,
    /// One entry per `TurnStart`.
    turns: Vec<TurnState>,
}

impl CollectorState {
    fn record(&mut self, event: &AgentEvent, at: Instant) {
        match event {
            AgentEvent::AgentStart { .. } => {
                self.started_at.get_or_insert(at);
            }
            AgentEvent::TurnStart { .. } => {
                self.turns.push(TurnState::new(at));
            }
            AgentEvent::StreamStart { .. } => {
                if let Some(turn) = self.turns.last_mut() {
                    turn.stream_started_at = Some(at);
                }
            }
            AgentEvent::StreamEvent { event, .. } => {
                if is_delta(event) {
                    if let Some(turn) = self.turns.last_mut() {
                        turn.deltas = turn.deltas.saturating_add(1);
                        if turn.first_delta_at.is_none() {
                            turn.first_delta_at = Some(at);
                        }
                    }
                }
            }
            AgentEvent::StreamEnd { message, .. } => {
                if let Some(turn) = self.turns.last_mut() {
                    if let Some(started) = turn.stream_started_at {
                        turn.stream_ms = turn.stream_ms.saturating_add(ms_between(started, at));
                    }
                    turn.usage.add(&message.usage);
                }
            }
            AgentEvent::ToolExecutionEnd { event, .. } => {
                if let ToolExecutionEvent::End {
                    tool_name,
                    is_error,
                    duration_ms,
                    parent_call_id,
                    ..
                } = event
                {
                    if let Some(turn) = self.turns.last_mut() {
                        turn.tool_calls.push(ToolCallDiagnostics {
                            name: tool_name.clone(),
                            ms: *duration_ms,
                            is_error: *is_error,
                            nested: parent_call_id.is_some(),
                        });
                    }
                }
            }
            AgentEvent::TurnEnd { .. } => {
                if let Some(turn) = self.turns.last_mut() {
                    turn.total_ms = ms_between(turn.started_at, at);
                }
            }
            // `ToolExecutionStart` carries no duration (the end event's
            // `duration_ms` is the scheduler's own measurement), and
            // `AgentEnd` / `Compaction` carry no per-turn measurement of their
            // own; the run's wall clock is `last_at - started_at` at finish.
            // The non-assistant `MessageStart`/`MessageEnd` pair and live
            // `ToolExecutionUpdate` partials have no timing contribution
            // either (tool calls are timed by their end event).
            AgentEvent::ToolExecutionStart { .. }
            | AgentEvent::MessageStart { .. }
            | AgentEvent::MessageEnd { .. }
            | AgentEvent::ToolExecutionUpdate { .. }
            | AgentEvent::AgentEnd { .. }
            | AgentEvent::Compaction { .. } => {}
        }
        self.last_at = Some(at);
    }

    fn summarize(&self) -> Option<RunDiagnostics> {
        if self.turns.is_empty() {
            return None;
        }
        let turns: Vec<TurnDiagnostics> = self
            .turns
            .iter()
            .enumerate()
            .map(|(index, turn)| turn.summary(index + 1))
            .collect();
        let mut total = RunTotals {
            turns: turns.len(),
            ..RunTotals::default()
        };
        for turn in &turns {
            total.provider_ms = total.provider_ms.saturating_add(turn.provider_ms);
            total.stream_ms = total.stream_ms.saturating_add(turn.stream_ms);
            total.total_ms = total.total_ms.saturating_add(turn.total_ms);
            total.deltas = total.deltas.saturating_add(turn.deltas);
            total.tool_calls += turn.tool_calls.len();
            total.usage.input = total.usage.input.saturating_add(turn.usage.input);
            total.usage.output = total.usage.output.saturating_add(turn.usage.output);
            total.usage.cache_read = total.usage.cache_read.saturating_add(turn.usage.cache_read);
            total.usage.cache_write = total
                .usage
                .cache_write
                .saturating_add(turn.usage.cache_write);
            total.usage.total_tokens = total
                .usage
                .total_tokens
                .saturating_add(turn.usage.total_tokens);
        }
        let slowest_turn = turns
            .iter()
            .max_by_key(|turn| turn.total_ms)
            .map(|turn| SlowestTurn {
                turn: turn.turn,
                total_ms: turn.total_ms,
            });
        let mut slowest_tool: Option<SlowestTool> = None;
        for turn in &turns {
            for call in &turn.tool_calls {
                let Some(ms) = call.ms else { continue };
                if slowest_tool.as_ref().is_none_or(|current| ms > current.ms) {
                    slowest_tool = Some(SlowestTool {
                        name: call.name.clone(),
                        ms,
                        turn: turn.turn,
                    });
                }
            }
        }
        let wall_ms = match (self.started_at, self.last_at) {
            (Some(started), Some(last)) => ms_between(started, last),
            _ => 0,
        };
        Some(RunDiagnostics {
            wall_ms,
            turns,
            total,
            slowest_turn,
            slowest_tool,
        })
    }
}

/// Collects [`RunDiagnostics`] from the agent loop's existing event stream.
///
/// Cheap by construction: one `Mutex` around a handful of counters, no
/// per-delta allocation, and — when constructed disabled — one `bool` check
/// per [`Self::record`] call and nothing else.
pub struct RunDiagnosticsCollector {
    enabled: bool,
    state: Mutex<CollectorState>,
}

impl RunDiagnosticsCollector {
    /// A collector that records when `enabled` is true and is inert (a single
    /// branch per event, no allocation) otherwise.
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            state: Mutex::new(CollectorState::default()),
        }
    }

    /// Whether the collector records at all.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Record one agent event at the current clock time (see
    /// [`Self::record_at`] for the deterministic seam).
    pub fn record(&self, event: &AgentEvent) {
        if !self.enabled {
            return;
        }
        self.record_at(event, Instant::now());
    }

    /// [`Self::record`] with an explicit receipt time: tests feed synthetic
    /// event sequences with controlled `Instant`s so the arithmetic is
    /// deterministic (no sleeps).
    pub fn record_at(&self, event: &AgentEvent, at: Instant) {
        if !self.enabled {
            return;
        }
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(event, at);
    }

    /// The summary of everything recorded so far: `None` when the collector
    /// is disabled or no turn was recorded.
    pub fn finish(&self) -> Option<RunDiagnostics> {
        if !self.enabled {
            return None;
        }
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .summarize()
    }
}

/// Whether one assistant stream event is a content delta (the events whose
/// count the summary reports).
fn is_delta(event: &AssistantMessageEvent) -> bool {
    matches!(
        event,
        AssistantMessageEvent::TextDelta { .. }
            | AssistantMessageEvent::ThinkingDelta { .. }
            | AssistantMessageEvent::ToolCallDelta { .. }
    )
}

/// Milliseconds between two receipt times (saturating; never panics on an
/// out-of-order `at`).
fn ms_between(started: Instant, at: Instant) -> u64 {
    let elapsed = at.saturating_duration_since(started);
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// The tools cell of one turn's row: `2 calls: bash 4000ms, read 12ms!`.
fn render_tools(turn: &TurnDiagnostics) -> String {
    if turn.tool_calls.is_empty() {
        return "-".to_string();
    }
    let calls = turn
        .tool_calls
        .iter()
        .map(|call| {
            let duration = match call.ms {
                Some(ms) => format!("{ms}ms"),
                None => "-".to_string(),
            };
            if call.is_error {
                format!("{} {duration}!", call.name)
            } else {
                format!("{} {duration}", call.name)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{} {}: {calls}",
        turn.tool_calls.len(),
        if turn.tool_calls.len() == 1 {
            "call"
        } else {
            "calls"
        }
    )
}
