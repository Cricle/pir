//! `ctx_resume`: the compact session-state block rebuilt from the journal.
//!
//! Upstream `context-mode` keeps one `events` row per tool call and renders a
//! bounded block (2,048 bytes) from it when the host compacts the transcript.
//! This port writes the same rows (`IndexStore::append_event`) from
//! `ctx_execute*`, `ctx_batch*`, `ctx_index`, `ctx_fetch_and_index` and the
//! `tool_result` hook, and renders the block here:
//!
//! - `seq` returns that one row whole (the pointer form);
//! - otherwise the newest rows are selected by weight (errors first, then
//!   index writes, execution, and remaining events), whole lines only — a row
//!   that does not fit becomes a one-line `ctx_resume({seq:…})` pointer, so no
//!   row's text is ever cut mid-line.

use std::sync::Arc;

use serde_json::Value as JsonValue;

use super::index::{JournalEvent, RESUME_CAP};
use super::ContextModeState;
use crate::agent::tool_batch::ToolResult;
use crate::agent::tool_execution::{AgentTool, BoxFuture, ToolContext};

/// How many recent events one block render considers.
const RESUME_SCAN_ROWS: usize = 200;

pub struct CtxResumeTool {
    state: Arc<ContextModeState>,
}

impl CtxResumeTool {
    pub fn new(state: Arc<ContextModeState>) -> Self {
        Self { state }
    }
}

impl AgentTool for CtxResumeTool {
    fn name(&self) -> &'static str {
        super::CTX_RESUME_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Rebuild the session's place after a compaction: the bounded state block (2,048 bytes) rendered from this session's journal — recent errors first, then index writes, execution results and other events, newest first within each class. `seq` returns one journal row whole. Read-only: it never writes or starts the sandbox. Use it after a long transcript was summarized, or to find the sequence number of a row you want in full."
    }

    fn prompt_snippet(&self) -> Option<String> {
        Some(
            "Rebuild what this session already learned after a compaction: recent errors, \
index writes and execution results from the journal"
                .to_owned(),
        )
    }

    fn parameters(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "properties": {
                "session": {
                    "type": "string",
                    "description": "Session id to render. Defaults to this session."
                },
                "seq": {
                    "type": "number",
                    "description": "Return this journal row whole (its sequence number)."
                }
            },
            "additionalProperties": false
        }))
    }

    fn has_external_side_effects(&self) -> bool {
        false
    }

    fn run(&self, args: &JsonValue, _ctx: &ToolContext) -> BoxFuture<'static, ToolResult> {
        let state = Arc::clone(&self.state);
        let session = args
            .get("session")
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|session| !session.is_empty())
            .map(str::to_owned);
        let seq = args.get("seq").and_then(JsonValue::as_u64);

        Box::pin(async move {
            let session = match session.or_else(|| state.hooks.session_id()) {
                Some(session) => session,
                None => {
                    return ToolResult::error(
                        "ctx_resume: no session id is available (pass `session` explicitly)",
                    )
                }
            };
            if let Some(seq) = seq {
                return match state.index.event_by_seq(seq as i64).await {
                    Ok(Some(event)) if event.session_id == session => ToolResult::ok(event.body),
                    Ok(_) => ToolResult::error(format!(
                        "ctx_resume: no journal row #{seq} for session {session}"
                    )),
                    Err(error) => ToolResult::error(format!("ctx_resume: {error}")),
                };
            }
            let events = match state
                .index
                .session_events(session.clone(), Some(RESUME_SCAN_ROWS))
                .await
            {
                Ok(events) => events,
                Err(error) => return ToolResult::error(format!("ctx_resume: {error}")),
            };
            if events.is_empty() {
                return ToolResult::ok(format!(
                    "No journal rows for session {session} yet. ctx_execute, ctx_batch, ctx_index and large tool results add rows."
                ));
            }
            ToolResult::ok(render_block(&session, &events))
        })
    }
}

/// Render whole rows within [`RESUME_CAP`] bytes.
fn render_block(session: &str, events: &[JournalEvent]) -> String {
    let newest = events.last().map(|event| event.seq).unwrap_or(0);
    let mut ordered: Vec<&JournalEvent> = events.iter().collect();
    ordered.sort_by(|left, right| {
        weight(right)
            .cmp(&weight(left))
            .then_with(|| right.seq.cmp(&left.seq))
    });

    let mut out = format!(
        "Session {session}: {} event(s), newest #{newest}.\n",
        events.len()
    );
    let mut omitted: Option<i64> = None;
    for event in ordered {
        let line = event_line(event);
        let extra = line.len() + 1;
        if out.len() + extra > RESUME_CAP {
            omitted.get_or_insert(event.seq);
            continue;
        }
        out.push_str(&line);
        out.push('\n');
    }
    if let Some(seq) = omitted {
        let pointer = format!("Older rows: ctx_resume({{seq:{seq}}}) continues the journal.\n");
        if out.len() + pointer.len() <= RESUME_CAP {
            out.push_str(&pointer);
        }
    }
    out.trim_end().to_owned()
}

/// Priority class: errors first, then index writes, execution, the rest.
fn weight(event: &JournalEvent) -> u8 {
    if !event.ok {
        return 4;
    }
    match event.kind.as_str() {
        "index" => 3,
        "exec" | "batch" | "fetch" => 2,
        "tool_result" => 1,
        _ => 0,
    }
}

fn event_line(event: &JournalEvent) -> String {
    let status = if event.ok {
        event
            .exit_code
            .filter(|code| *code != 0)
            .map(|code| format!(" exit {code}"))
            .unwrap_or_default()
    } else {
        " FAILED".to_owned()
    };
    let path = event
        .path
        .as_deref()
        .map(|path| format!(" [{path}]"))
        .unwrap_or_default();
    let body: String = event
        .body
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(160)
        .collect();
    format!(
        "#{} {}/{}{}{}: {} ({} B)",
        event.seq, event.kind, event.tool, path, status, body, event.bytes
    )
}
