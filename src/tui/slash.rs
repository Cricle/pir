//! Built-in slash-command parsing and routing.
//!
//! Mirrors upstream `core/slash-commands.ts` (`BUILTIN_SLASH_COMMANDS`) and the
//! interactive-mode dispatch in `modes/interactive/interactive-mode.ts`
//! (`handleSlashCommand`). The parsing and routing are pure so the TUI
//! dispatch and the test suite share one contract; the terminal loop only
//! performs the effects.

use super::app::ChatEntry;

/// A parsed `/command args…` invocation (upstream splits on the first space).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashInvocation<'a> {
    /// Command name without the leading `/`.
    pub name: &'a str,
    /// Everything after the first space, trimmed of surrounding whitespace.
    pub args: &'a str,
}

/// Parse a `/command …` line. Returns `None` when the text does not start with
/// `/` or the name is empty (`/` alone).
pub fn parse_slash_command(text: &str) -> Option<SlashInvocation<'_>> {
    let rest = text.strip_prefix('/')?;
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (rest, ""),
    };
    if name.is_empty() {
        return None;
    }
    Some(SlashInvocation { name, args })
}

/// The built-in commands pir implements, in upstream `BUILTIN_SLASH_COMMANDS`
/// order. Each name routes to a [`SlashAction`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashAction {
    /// `/settings` — show the effective settings (upstream opens a menu).
    Settings,
    /// `/model [provider/model]` — open the model selector.
    Model,
    /// `/thinking [level]` — set the thinking level.
    Thinking,
    /// `/copy` — copy the last agent message to the clipboard.
    Copy,
    /// `/name <name>` — set the session display name.
    Name,
    /// `/session` — show session info and stats.
    Session,
    /// `/sessions [new|next|prev|close|<index>|<path>]` — list and switch the
    /// in-window session tabs (bare `/sessions` opens the tab/session picker).
    Sessions,
    /// `/fork` — fork the current session.
    Fork,
    /// `/clone` — duplicate the current session at the current position.
    Clone,
    /// `/new` — start a fresh session.
    New,
    /// `/compact [instructions]` — manually compact the session context.
    Compact,
    /// `/export [path]` — export the session to a self-contained HTML file.
    Export,
    /// `/resume` — resume a different session.
    Resume,
    /// `/clear` — clear the visible conversation.
    Clear,
    /// `/quit` — exit the TUI.
    Quit,
    /// `/help` — list the available commands.
    Help,
    /// `/tree` — open the session-tree selector and branch from an entry.
    Tree,
    /// `/scoped-models` — show the `--models` scope in effect.
    ScopedModels,
    /// `/trust` — show the current project trust state.
    Trust,
    /// `/login [provider]` — OAuth login guidance (upstream opens a dialog).
    Login,
    /// `/logout` — OAuth logout guidance (upstream opens a dialog).
    Logout,
    /// `/share` — upload the session to the destination the
    /// `sessionShareDestination` setting picks (upstream `session-share.ts`:
    /// a Radius artifact when authenticated, otherwise a secret GitHub gist;
    /// pir posts the gist through the GitHub REST API instead of shelling out
    /// to `gh`).
    Share,
    /// `/import <path.jsonl>` — import and resume a session file (upstream
    /// `handleImportCommand`).
    Import,
    /// `/bug [description]` — write a bug-report bundle to the working
    /// directory (upstream `reportBug`; pir keeps the export-to-disk leg,
    /// without the Radius upload).
    Bug,
    /// `/changelog` — what's new in this build.
    Changelog,
    /// `/hotkeys` — list the default keybindings.
    Hotkeys,
    /// `/reload` — reload the extension runtime.
    Reload,
    /// `/debug [enable|disable|last]` — session debug mode + run timings.
    Debug,
}

/// Parse a `/thinking <level>` argument into a [`crate::ai::types::ThinkingLevel`]
/// (upstream `handleThinkingCommand`: case-insensitive exact match against the
/// available levels).
pub fn thinking_level_argument(term: &str) -> Result<crate::ai::types::ThinkingLevel, String> {
    use crate::ai::types::ThinkingLevel;
    let normalized = term.trim().to_lowercase();
    match normalized.as_str() {
        "off" => Ok(ThinkingLevel::Off),
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "xhigh" => Ok(ThinkingLevel::XHigh),
        "max" => Ok(ThinkingLevel::Max),
        _ => Err(format!(
            "Unknown thinking level \"{term}\". Available levels: off, minimal, low, medium, high, xhigh, max."
        )),
    }
}

/// Outcome of matching a `/model <term>` argument against the registry
/// (upstream `findExactModelReferenceMatch`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelMatch {
    /// Exactly one model matched the term.
    Found { provider: String, model_id: String },
    /// Several models matched: the caller shows the candidates.
    Ambiguous(Vec<(String, String)>),
    /// No model matched: the caller opens the selector pre-filtered.
    NotFound,
}

/// Match a `/model <term>` argument against `(provider, id)` pairs
/// (upstream `findExactModelReferenceMatch`: a case-insensitive match on the
/// `provider/id` reference, the bare model id, or — when the term carries no
/// `/` — a provider-suffix match; ambiguity falls back to the selector).
pub fn resolve_model_argument(term: &str, models: &[(String, String)]) -> ModelMatch {
    let term = term.trim();
    if term.is_empty() {
        return ModelMatch::NotFound;
    }
    let normalized = term.to_lowercase();
    let matches: Vec<(String, String)> = models
        .iter()
        .filter(|(provider, id)| {
            let reference = format!("{provider}/{id}").to_lowercase();
            if reference == normalized {
                return true;
            }
            if term.contains('/') {
                return false;
            }
            // Bare id: match the id exactly, or the tail of the reference
            // (`anthropic/claude-x` ends with `claude-x`).
            id.to_lowercase() == normalized || reference.ends_with(&format!("/{normalized}"))
        })
        .cloned()
        .collect();
    match matches.len() {
        0 => ModelMatch::NotFound,
        1 => ModelMatch::Found {
            provider: matches[0].0.clone(),
            model_id: matches[0].1.clone(),
        },
        _ => ModelMatch::Ambiguous(matches),
    }
}

/// The custom instructions of `/compact [instructions]`: `None` for a bare
/// command (upstream `handleCompactCommand(undefined)`), `Some` when passed.
pub fn compact_instructions(args: &str) -> Option<String> {
    let trimmed = args.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Route a built-in command name to its action. `None` means the name is not a
/// built-in: the caller falls back to the extension command registry and then
/// to the unknown-command notice (upstream dispatch order).
pub fn builtin_action(name: &str) -> Option<SlashAction> {
    Some(match name {
        "settings" => SlashAction::Settings,
        "model" => SlashAction::Model,
        "thinking" => SlashAction::Thinking,
        "copy" => SlashAction::Copy,
        "name" => SlashAction::Name,
        "session" => SlashAction::Session,
        "sessions" => SlashAction::Sessions,
        "fork" => SlashAction::Fork,
        "clone" => SlashAction::Clone,
        "new" => SlashAction::New,
        "compact" => SlashAction::Compact,
        "export" => SlashAction::Export,
        "resume" => SlashAction::Resume,
        "clear" => SlashAction::Clear,
        "quit" => SlashAction::Quit,
        "help" => SlashAction::Help,
        "tree" => SlashAction::Tree,
        "scoped-models" => SlashAction::ScopedModels,
        "trust" => SlashAction::Trust,
        "login" => SlashAction::Login,
        "logout" => SlashAction::Logout,
        "changelog" => SlashAction::Changelog,
        "hotkeys" => SlashAction::Hotkeys,
        "reload" => SlashAction::Reload,
        "debug" => SlashAction::Debug,
        "share" => SlashAction::Share,
        "import" => SlashAction::Import,
        "bug" => SlashAction::Bug,
        _ => return None,
    })
}

/// Whether an action ends the TUI event loop (upstream `/quit` calls
/// `shutdown()` and returns).
pub fn action_quits(action: SlashAction) -> bool {
    action == SlashAction::Quit
}

/// Whether an action starts a brand-new session (upstream
/// `handleClearCommand` — reached by `/clear` and by the `app.session.new`
/// keybinding — and the `/new` built-in both call `runtimeHost.newSession()`).
/// `/clear` is therefore not a transcript-only wipe; the TUI dispatch routes
/// both actions through one `start_new_session` effect.
pub fn action_starts_new_session(action: SlashAction) -> bool {
    matches!(action, SlashAction::Clear | SlashAction::New)
}

/// The text of one assistant message's text blocks, joined by `\n`; `None`
/// when the message carries no non-empty text (upstream
/// `getLastAssistantText` filters to text blocks only).
fn assistant_message_text(message: &crate::ai::types::AssistantMessage) -> Option<String> {
    let text = message
        .content
        .iter()
        .filter_map(|block| match block {
            crate::ai::types::ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

/// The last `count` assistant messages with text, oldest first (`count` of
/// `None` or `0` means one; `usize::MAX` means every message).
pub fn assistant_texts(entries: &[ChatEntry], count: Option<usize>) -> Vec<String> {
    let count = count.unwrap_or(1).max(1);
    let mut texts: Vec<String> = entries
        .iter()
        .rev()
        .filter_map(|entry| match entry {
            ChatEntry::Assistant(message) => assistant_message_text(message),
            _ => None,
        })
        .take(count)
        .collect();
    texts.reverse();
    texts
}

/// The text of the last assistant message in the transcript, or `None` when
/// there is none (upstream `session.getLastAssistantText()`). Text-less
/// assistant entries are skipped, so a tool-only final turn falls back to the
/// previous message with text.
pub fn last_assistant_text(entries: &[ChatEntry]) -> Option<String> {
    assistant_texts(entries, Some(1)).pop()
}

/// Outcome of `/copy` (upstream `handleCopyCommand`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOutcome {
    /// The last agent message was copied.
    Copied,
    /// `n` agent messages were copied (`/copy n` / `/copy all`).
    CopiedMessages(usize),
    /// No agent message exists yet.
    NothingToCopy,
    /// The clipboard write failed (no clipboard available).
    ClipboardUnavailable,
}

/// Parse the `/copy` argument: empty copies the last assistant message;
/// `<count>` copies the last `count` messages; `all` copies every message.
pub fn copy_count_argument(args: &str) -> Result<Option<usize>, String> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok(Some(usize::MAX));
    }
    match trimmed.parse::<usize>() {
        Ok(count) if count > 0 => Ok(Some(count)),
        _ => Err(format!(
            "Unknown /copy argument \"{trimmed}\". Usage: /copy [count|all]."
        )),
    }
}

/// Run `/copy [count|all]` against a transcript using `writer` as the
/// clipboard sink. `writer` returns `false` when the clipboard is
/// unavailable; production passes [`super::clipboard::write_text`], tests pass
/// a mock. Multiple messages are joined by a blank line, oldest first.
pub fn copy_assistant_messages(
    entries: &[ChatEntry],
    count: Option<usize>,
    writer: impl FnOnce(&str) -> bool,
) -> CopyOutcome {
    let texts = assistant_texts(entries, count);
    if texts.is_empty() {
        return CopyOutcome::NothingToCopy;
    }
    let copied_count = texts.len();
    let text = texts.join("\n\n");
    if !writer(&text) {
        return CopyOutcome::ClipboardUnavailable;
    }
    if copied_count == 1 {
        CopyOutcome::Copied
    } else {
        CopyOutcome::CopiedMessages(copied_count)
    }
}

/// Run `/copy` (no count) against a transcript: the last assistant message.
pub fn copy_last_assistant_message(
    entries: &[ChatEntry],
    writer: impl FnOnce(&str) -> bool,
) -> CopyOutcome {
    copy_assistant_messages(entries, None, writer)
}

/// Validate a `/name <name>` argument (upstream: an empty name shows usage).
pub fn name_argument(args: &str) -> Result<String, String> {
    let name = args.trim();
    if name.is_empty() {
        Err("Usage: /name <name>".to_owned())
    } else {
        Ok(name.to_owned())
    }
}

// ---------------------------------------------------------------------------
// /sessions — same-window session tabs.
//
// Grammar (pure, so the dispatch and the tests share one contract):
//
//   /sessions              -> Picker (open tabs, then the cwd's other sessions)
//   /sessions new          -> New
//   /sessions next         -> Next (wraps)
//   /sessions prev|previous -> Prev (wraps)
//   /sessions close        -> Close (the active tab)
//   /sessions <index>      -> Index (1-based tab position)
//   /sessions <path>       -> Path (attach that session file)
//
// The keyword/2-word names win over paths of the same spelling; a word that
// parses as an integer is a tab index, everything else is a path.
// ---------------------------------------------------------------------------

/// A parsed `/sessions` argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionsArgument<'a> {
    /// Bare `/sessions`: open the tab/session selector.
    Picker,
    /// `/sessions new`: start a fresh session (and tab).
    New,
    /// `/sessions next`: activate the next tab, wrapping.
    Next,
    /// `/sessions prev` (or `previous`): activate the previous tab, wrapping.
    Prev,
    /// `/sessions close`: close the active tab.
    Close,
    /// `/sessions <index>`: activate the 1-based tab position.
    Index(usize),
    /// `/sessions <path>`: attach that session file.
    Path(&'a str),
}

/// Parse the `/sessions` argument (see [`SessionsArgument`]). A quoted path
/// loses its matching surrounding quotes.
pub fn sessions_argument(args: &str) -> SessionsArgument<'_> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return SessionsArgument::Picker;
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "new" => return SessionsArgument::New,
        "next" => return SessionsArgument::Next,
        "prev" | "previous" => return SessionsArgument::Prev,
        "close" => return SessionsArgument::Close,
        _ => {}
    }
    if let Ok(index) = trimmed.parse::<usize>() {
        return SessionsArgument::Index(index);
    }
    let path = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        })
        .unwrap_or(trimmed);
    SessionsArgument::Path(path)
}

// ---------------------------------------------------------------------------
// /debug — session debug mode + the last run's timing metrics (v1).
//
// The metrics source is `crate::diagnostics::RunDiagnostics`: `AgentSession`
// collects it on every run and exposes it through
// `AgentSession::last_run_diagnostics()`, so this command renders numbers the
// print mode's `PI_TIMING=1` table already reports — the same measurement,
// laid out for a transcript line instead of an aligned table. The worker owns
// the mode latch ([`DebugArgument`] is its wire request); the parsing and the
// rendering here stay pure so the dispatch and the tests share one contract.
// ---------------------------------------------------------------------------

/// A parsed `/debug [enable|disable|last]` argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugArgument {
    /// Bare `/debug`: report the mode state plus the last run's metrics.
    Status,
    /// `/debug enable`: append the metrics block after every completed run.
    Enable,
    /// `/debug disable`: stop appending them.
    Disable,
    /// `/debug last`: only the last run's metrics block.
    Last,
}

/// Parse the `/debug` argument: an empty argument is [`DebugArgument::Status`]
/// and the three words are case-insensitive exact matches. An unknown word is
/// a usage error — never a silent fallback to the status dump (a typo like
/// `/debug enabl` must say so instead of looking like it worked).
pub fn debug_command_argument(args: &str) -> Result<DebugArgument, String> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Ok(DebugArgument::Status);
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "enable" => Ok(DebugArgument::Enable),
        "disable" => Ok(DebugArgument::Disable),
        "last" => Ok(DebugArgument::Last),
        _ => Err(format!(
            "Unknown /debug option \"{trimmed}\". Usage: /debug [enable|disable|last]."
        )),
    }
}

/// Tool names longer than this truncate with a trailing `…` in
/// [`run_diagnostics_text`]: the transcript wraps long lines, and one runaway
/// MCP tool name must not push the numbers off screen.
const DEBUG_TOOL_NAME_CHARS: usize = 24;

/// The `/debug` frame-dump log path (upstream `getDebugLogPath`:
/// `<agentDir>/<APP_NAME>-debug.log`; pir's app name is `pir`).
pub fn debug_dump_path(agent_dir: &std::path::Path) -> std::path::PathBuf {
    agent_dir.join("pir-debug.log")
}

/// The painted-frame geometry recorded in a `/debug` dump (upstream reads
/// `ui.terminal.columns/rows` and `ui.render(width)`; pir records the last
/// undecorated frame plus the layout `TuiCore` published for the mouse
/// router).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DebugFrameLayout {
    pub columns: usize,
    pub rows: usize,
    /// Total rows of the last undecorated frame.
    pub frame_len: usize,
    /// `true` when the last frame came from the windowed (scrolled) path.
    pub windowed: bool,
    /// Absolute buffer row painted at the top of the transcript area.
    pub window_start: usize,
    /// Transcript rows painted in the window.
    pub window_len: usize,
    /// Absolute buffer row one past the last transcript row (the dock start).
    pub transcript_end: usize,
    /// Dock rows painted below the window.
    pub dock_len: usize,
    /// Absolute buffer row at the top of the visible viewport.
    pub visible_top: usize,
    /// Monotonic frame counter (search caches / selection refresh).
    pub frame_version: u64,
}

/// Build the `/debug` dump (upstream `handleDebugCommand`): timestamp and
/// terminal size, the painted layout, every rendered line with its visible
/// width and JSON-escaped text, then the agent messages as JSONL (already
/// serialized by the caller).
pub fn debug_dump_text(
    layout: DebugFrameLayout,
    frame: &[String],
    messages: &[String],
    timestamp: &str,
) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(frame.len() + messages.len() + 12);
    lines.push(format!("Debug output at {timestamp}"));
    lines.push(format!("Terminal: {}x{}", layout.columns, layout.rows));
    lines.push(format!("Total lines: {}", frame.len()));
    lines.push(format!(
        "Layout: windowed={} window_start={} window_len={} transcript_end={} dock_len={} visible_top={} frame_version={}",
        layout.windowed,
        layout.window_start,
        layout.window_len,
        layout.transcript_end,
        layout.dock_len,
        layout.visible_top,
        layout.frame_version
    ));
    lines.push(String::new());
    lines.push("=== All rendered lines with visible widths ===".to_owned());
    for (index, line) in frame.iter().enumerate() {
        let visible = super::ansi::visible_width(line);
        let escaped =
            serde_json::to_string(line).unwrap_or_else(|_| "\"<unrenderable>\"".to_owned());
        lines.push(format!("[{index}] (w={visible}) {escaped}"));
    }
    lines.push(String::new());
    lines.push("=== Agent messages (JSONL) ===".to_owned());
    lines.extend(messages.iter().cloned());
    lines.push(String::new());
    lines.join("\n")
}

/// Serialize the transcript's assistant messages as JSONL rows (upstream
/// `this.session.messages.map(JSON.stringify)`).
pub fn debug_message_jsonl(entries: &[ChatEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            ChatEntry::Assistant(message) => serde_json::to_string(message.as_ref()).ok(),
            _ => None,
        })
        .collect()
}

/// `8.99s` — a duration as the transcript renders it (the print-mode table
/// uses raw aligned `ms` columns; a one-line-per-turn row reads better in
/// seconds).
fn debug_seconds(ms: u64) -> String {
    format!("{:.2}s", ms as f64 / 1000.0)
}

/// One tool call's cell inside a turn's `tools:` segment: `bash 0.42s`,
/// `read -` when the result carried no duration, `bash 0.42s!` on error
/// (same rules as the print table's cells).
fn debug_tool_cell(call: &crate::diagnostics::ToolCallDiagnostics) -> String {
    let name = if call.name.chars().count() > DEBUG_TOOL_NAME_CHARS {
        let mut truncated: String = call.name.chars().take(DEBUG_TOOL_NAME_CHARS - 1).collect();
        truncated.push('…');
        truncated
    } else {
        call.name.clone()
    };
    let duration = match call.ms {
        Some(ms) => debug_seconds(ms),
        None => "-".to_owned(),
    };
    if call.is_error {
        format!("{name} {duration}!")
    } else {
        format!("{name} {duration}")
    }
}

/// A turn's `tools:` cell: `-` without calls, else the comma-joined cells
/// (`bash 0.42s, read 0.03s`).
fn debug_tools_cell(calls: &[crate::diagnostics::ToolCallDiagnostics]) -> String {
    if calls.is_empty() {
        return "-".to_owned();
    }
    calls
        .iter()
        .map(debug_tool_cell)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `tokens in X/out Y` segment; `None` when the provider reported no
/// usage at all (a `0/0` line would claim a measurement nobody made). Cache
/// activity rides the same segment when there was any.
fn debug_usage_segment(usage: &crate::diagnostics::UsageDiagnostics) -> Option<String> {
    let any_reported =
        usage.input | usage.output | usage.cache_read | usage.cache_write | usage.total_tokens;
    if any_reported == 0 {
        return None;
    }
    let mut segment = format!("tokens in {}/out {}", usage.input, usage.output);
    if usage.cache_read > 0 || usage.cache_write > 0 {
        segment.push_str(&format!(
            " cache r {}/w {}",
            usage.cache_read, usage.cache_write
        ));
    }
    Some(segment)
}

/// The TOTAL row's `tools:` cell: `2 calls` / `1 call` (the print table's
/// wording).
fn debug_total_tools_cell(total: &crate::diagnostics::RunTotals) -> String {
    format!(
        "{} {}",
        total.tool_calls,
        if total.tool_calls == 1 {
            "call"
        } else {
            "calls"
        }
    )
}

/// The `/debug` timing block in transcript width (pure): one line per turn,
/// then a `TOTAL` line when the run had more than one turn. The numbers are
/// [`crate::diagnostics::RunDiagnostics`]'s own — provider wait, stream,
/// turn total, delta count, per-tool durations, summed token usage:
///
/// ```text
/// turn 1  provider 8.99s  stream 9.01s  total 9.13s  deltas 12  tools: bash 0.42s, read 0.03s  tokens in 17708/out 3
/// turn 2  provider 1.20s  stream 2.00s  total 2.10s  deltas 7  tools: -  tokens in 100/out 50
/// TOTAL  provider 10.19s  stream 11.01s  total 11.23s  deltas 19  tools: 2 calls  tokens in 17808/out 53
/// ```
///
/// A turn without tool calls renders `tools: -`; a tool whose result carried
/// no duration renders `name -`; an error result is marked `!`; a usage of
/// all zeros omits the `tokens` segment. An empty turn list (the collector
/// never stores one, but the type allows it) renders a single explanatory
/// line.
pub fn run_diagnostics_text(diagnostics: &crate::diagnostics::RunDiagnostics) -> String {
    if diagnostics.turns.is_empty() {
        return "no turn timings recorded".to_owned();
    }
    let mut lines: Vec<String> = Vec::with_capacity(diagnostics.turns.len() + 1);
    for turn in &diagnostics.turns {
        let mut line = format!(
            "turn {}  provider {}  stream {}  total {}  deltas {}  tools: {}",
            turn.turn,
            debug_seconds(turn.provider_ms),
            debug_seconds(turn.stream_ms),
            debug_seconds(turn.total_ms),
            turn.deltas,
            debug_tools_cell(&turn.tool_calls),
        );
        if let Some(tokens) = debug_usage_segment(&turn.usage) {
            line.push_str("  ");
            line.push_str(&tokens);
        }
        lines.push(line);
    }
    if diagnostics.turns.len() > 1 {
        let mut line = format!(
            "TOTAL  provider {}  stream {}  total {}  deltas {}  tools: {}",
            debug_seconds(diagnostics.total.provider_ms),
            debug_seconds(diagnostics.total.stream_ms),
            debug_seconds(diagnostics.total.total_ms),
            diagnostics.total.deltas,
            debug_total_tools_cell(&diagnostics.total),
        );
        if let Some(tokens) = debug_usage_segment(&diagnostics.total.usage) {
            line.push_str("  ");
            line.push_str(&tokens);
        }
        lines.push(line);
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// /share — session upload (upstream `modes/interactive/session-share.ts`).
// The destination comes from the `sessionShareDestination` setting (`cea257439`):
// the Radius artifact endpoint when Radius is authenticated, otherwise a
// secret GitHub gist. Upstream shells out to `gh gist create --public=false`;
// pir posts the equivalent secret-gist request through the GitHub REST API
// with a token from `GH_TOKEN` / `GITHUB_TOKEN` (the `gh` CLI reads the same
// variables), keeping the network on pir's reqwest + bounded-timeout seam.
// ---------------------------------------------------------------------------

/// The gist endpoint pir posts to (upstream relies on the `gh` CLI default).
pub const GIST_API_URL: &str = "https://api.github.com/gists";

/// The file name inside the gist (upstream writes the branch export to
/// `session.jsonl` in a temp dir before `gh gist create`).
pub const GIST_FILE_NAME: &str = "session.jsonl";

/// The request pir needs for `POST /gists` (typed assembly; the caller turns
/// it into a reqwest call with a bounded timeout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GistRequest {
    /// Target URL (`GIST_API_URL`).
    pub url: String,
    /// `Authorization: Bearer <token>` value.
    pub token: String,
    /// The JSON body: `{"description", "public": false, "files": {...}}`.
    pub body: String,
}

/// Assemble the secret-gist creation request (pure; the body embeds the
/// session JSONL text, `public: false` per upstream `--public=false`).
pub fn gist_request(content: &str, description: &str, token: &str) -> GistRequest {
    let mut file = serde_json::Map::new();
    file.insert(
        "content".to_owned(),
        serde_json::Value::String(content.to_owned()),
    );
    let mut files = serde_json::Map::new();
    files.insert(GIST_FILE_NAME.to_owned(), serde_json::Value::Object(file));
    let body = serde_json::json!({
        "description": description,
        "public": false,
        "files": serde_json::Value::Object(files),
    });
    GistRequest {
        url: GIST_API_URL.to_owned(),
        token: token.to_owned(),
        body: body.to_string(),
    }
}

/// Resolve the GitHub token from the environment (upstream checks
/// `gh auth status`; the CLI honors `GH_TOKEN` then `GITHUB_TOKEN`).
/// `lookup` isolates the process env so tests can drive both orders.
pub fn gist_token_from(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    lookup("GH_TOKEN")
        .filter(|token| !token.trim().is_empty())
        .or_else(|| lookup("GITHUB_TOKEN").filter(|token| !token.trim().is_empty()))
}

/// The upstream no-credential error (session-share.ts:74, reworded for the
/// env-token path pir takes instead of the `gh` CLI).
pub const GIST_TOKEN_MISSING: &str =
    "GitHub credentials not found. Set GH_TOKEN (or GITHUB_TOKEN), or run 'gh auth login' first.";

/// Parse `html_url` out of the GitHub gist-creation response (upstream takes
/// the URL `gh` prints and derives the viewer link from the gist id).
pub fn gist_url_from_response(response_body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(response_body).ok()?;
    value
        .get("html_url")?
        .as_str()
        .map(str::to_owned)
        .filter(|url| !url.is_empty())
}

/// The gist id from its `https://gist.github.com/<user>/<id>` URL (upstream
/// `gistUrl?.split("/").pop()`).
pub fn gist_id_from_url(url: &str) -> Option<&str> {
    url.trim()
        .split('/')
        .next_back()
        .filter(|id| !id.is_empty())
}

/// The share viewer URL for a gist id (upstream `getShareViewerUrl`,
/// config.ts:595-598): `PI_SHARE_VIEWER_URL` overrides the default base, and
/// the gist id rides a `#` fragment.
pub fn share_viewer_url(gist_id: &str) -> String {
    const DEFAULT_SHARE_VIEWER_URL: &str = "https://pi.dev/session/";
    let base = std::env::var("PI_SHARE_VIEWER_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_SHARE_VIEWER_URL.to_owned());
    format!("{base}#{gist_id}")
}

// ---------------------------------------------------------------------------
// /share destination routing (upstream `cea257439`, `session-share.ts:69-76`)
// ---------------------------------------------------------------------------

/// Where `/share` uploads the session, after the `sessionShareDestination`
/// setting and the Radius credential availability are resolved (upstream
/// `shareSession`'s destination block).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareRoute {
    /// Upload through the Radius artifact endpoint.
    Radius,
    /// Create a secret GitHub gist.
    Gist,
    /// The destination is `radius` but no Radius credential is configured:
    /// report [`RADIUS_AUTH_MISSING`] and stop — upstream never falls back to
    /// a gist once Radius is forced.
    RadiusUnconfigured,
}

/// Decide the `/share` route (upstream `session-share.ts:69-76`):
/// - `gist` skips Radius entirely (it does not even read the credential);
/// - otherwise Radius is attempted first, so `auto` resolves to
///   [`ShareRoute::Radius`] when a credential is configured;
/// - `auto` without a credential falls back to [`ShareRoute::Gist`], while
///   `radius` without one is [`ShareRoute::RadiusUnconfigured`].
pub fn share_route(
    destination: crate::config::settings::SessionShareDestination,
    radius_available: bool,
) -> ShareRoute {
    use crate::config::settings::SessionShareDestination as Destination;
    match destination {
        Destination::Gist => ShareRoute::Gist,
        Destination::Radius if radius_available => ShareRoute::Radius,
        Destination::Radius => ShareRoute::RadiusUnconfigured,
        Destination::Auto if radius_available => ShareRoute::Radius,
        Destination::Auto => ShareRoute::Gist,
    }
}

/// The upstream missing-credential error (`session-share.ts:75`), shown when
/// the destination is forced to `radius` and no Radius credential exists.
pub const RADIUS_AUTH_MISSING: &str =
    "Radius authentication is not configured. Run '/login radius' first.";

/// The request pir needs for the Radius artifact upload (upstream
/// `tryShareViaRadius`, session-share.ts:124-140): `POST /v1/artifacts` with
/// the exported session JSONL as an `application/x-ndjson` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadiusArtifactRequest {
    /// `"<gateway>/v1/artifacts?visibility=organization&title=Pi+session"`.
    pub url: String,
    /// `Authorization: Bearer <token>` value.
    pub token: String,
    /// The session JSONL export (the `pi.share` trailing entry included).
    pub body: String,
}

/// Assemble the Radius artifact upload request (pure; the caller turns it into
/// a reqwest call with a bounded timeout). Upstream builds the URL with
/// `new URL("/v1/artifacts", DEFAULT_RADIUS_GATEWAY)` plus
/// `searchParams.set("visibility", "organization")` and
/// `searchParams.set("title", "Pi session")`; `URLSearchParams` serializes the
/// space as `+`, which is what this reproduces. Upstream pins the published
/// gateway constant for `/share`; pir takes the resolved gateway so the
/// `PI_RADIUS_GATEWAY` override works here as it does for the bug-report
/// upload.
pub fn radius_artifact_request(content: &str, token: &str, gateway: &str) -> RadiusArtifactRequest {
    RadiusArtifactRequest {
        url: format!(
            "{}/v1/artifacts?visibility=organization&title=Pi+session",
            gateway.trim_end_matches('/')
        ),
        token: token.to_owned(),
        body: content.to_owned(),
    }
}

/// The uploaded artifact's canonical URL (upstream
/// `json.artifact.canonical_url`).
pub fn radius_artifact_url_from_response(response_body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(response_body).ok()?;
    value
        .pointer("/artifact/canonical_url")?
        .as_str()
        .map(str::to_owned)
        .filter(|url| !url.is_empty())
}

/// The gateway's error detail (upstream `json.error`), used to build the
/// `Failed to upload Radius artifact: …` message.
pub fn radius_artifact_error_from_response(response_body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(response_body).ok()?;
    value
        .get("error")?
        .as_str()
        .map(str::to_owned)
        .filter(|message| !message.is_empty())
}

// ---------------------------------------------------------------------------
// /import — argument validation (upstream `handleImportCommand`: an empty
// path shows `Usage: /import <path.jsonl>`).
// ---------------------------------------------------------------------------

/// Validate a `/import <path>` argument.
pub fn import_argument(args: &str) -> Result<String, String> {
    let path = args.trim();
    if path.is_empty() {
        Err("Usage: /import <path.jsonl>".to_owned())
    } else {
        Ok(path.to_owned())
    }
}

/// The Anthropic subscription-auth startup warning (upstream
/// `ANTHROPIC_SUBSCRIPTION_AUTH_WARNING`, interactive-mode.ts:321-322).
pub const ANTHROPIC_SUBSCRIPTION_AUTH_WARNING: &str =
    "Anthropic subscription auth is active. Third-party harness usage draws from extra usage and \
     is billed per token, not your Claude plan limits. Manage extra usage at \
     https://claude.ai/settings/usage. Disable this warning in /settings.";

// ---------------------------------------------------------------------------
// /export — JSONL branch (upstream `handleExportCommand`,
// interactive-mode.ts:6520-6536 + `core/session-export.ts`). A path ending in
// `.jsonl` exports the session branch as JSONL (header + entries on the
// current branch path, each entry's `parentId` re-chained); anything else
// exports the HTML page.
// ---------------------------------------------------------------------------

/// Which exporter a `/export [path]` argument selects (upstream
/// `outputPath?.endsWith(".jsonl")` — case-sensitive, matching upstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Html,
    Jsonl,
}

/// Classify a `/export` path argument; `None` (no argument) is HTML.
pub fn export_format(path: Option<&str>) -> ExportFormat {
    match path {
        Some(path) if path.ends_with(".jsonl") => ExportFormat::Jsonl,
        _ => ExportFormat::Html,
    }
}

/// The derived output name for a bare `/export` (upstream `exportSessionToJsonl`'s
/// `session-${new Date().toISOString().replace(/[:.]/g, "-")}.jsonl`). `stamp`
/// is a pre-formatted timestamp; `:`/`.` become `-`.
pub fn default_jsonl_export_name(stamp: &str) -> String {
    let clean: String = stamp
        .chars()
        .map(|ch| if ch == ':' || ch == '.' { '-' } else { ch })
        .collect();
    format!("session-{clean}.jsonl")
}

/// Serialize the current session branch as JSONL (upstream
/// `serializeSessionBranch`): the session header line followed by every entry
/// on the branch path, each entry's `parentId` rewritten to the previous
/// entry's id so the export is a single linear chain.
pub fn serialize_session_branch(manager: &crate::session_manager::SessionManager) -> String {
    serialize_session_branch_with_trailing(manager, &[])
}

/// Serialize the branch with export-only trailing entries appended after it
/// (upstream `createShareTrailingEntries`, session-share.ts:25-47 / session
/// -export.ts:9-29): the `pi.share` custom entry carries the system prompt
/// and the declared tool list; the entry id is an 8-char uuid slice and its
/// `parentId` chains onto the last branch entry.
pub fn serialize_session_branch_with_trailing(
    manager: &crate::session_manager::SessionManager,
    trailing: &[serde_json::Value],
) -> String {
    let timestamp = crate::session_manager::types::iso_from_ms(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0),
    );
    let header =
        crate::session_manager::types::SessionHeader::new(manager.id(), manager.cwd(), timestamp);
    let Ok(branch) = manager.branch_entries(None) else {
        return String::new();
    };
    let mut lines: Vec<String> = Vec::with_capacity(branch.len() + 1);
    let mut header_value = serde_json::to_value(&header).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(object) = header_value.as_object_mut() {
        // The standalone header line carries the same `type` tag the enum
        // injects on persisted files (upstream `{ type: "session", … }`).
        object.insert(
            "type".to_owned(),
            serde_json::Value::String("session".to_owned()),
        );
    }
    lines.push(header_value.to_string());
    let mut parent_id: Option<String> = None;
    for entry in branch {
        let id = entry.id().map(str::to_owned);
        if let Ok(mut value) = serde_json::to_value(&entry) {
            if let Some(object) = value.as_object_mut() {
                object.insert(
                    "parentId".to_owned(),
                    parent_id
                        .as_deref()
                        .map(|pid| serde_json::Value::String(pid.to_owned()))
                        .unwrap_or(serde_json::Value::Null),
                );
            }
            lines.push(value.to_string());
        }
        parent_id = id;
    }
    for entry in trailing {
        let mut value = entry.clone();
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "parentId".to_owned(),
                parent_id
                    .as_deref()
                    .map(|pid| serde_json::Value::String(pid.to_owned()))
                    .unwrap_or(serde_json::Value::Null),
            );
        }
        parent_id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        lines.push(value.to_string());
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// The `pi.share` trailing entry (upstream `createShareTrailingEntries`,
/// session-share.ts:25-47): export-only metadata describing the session.
pub fn share_trailing_entry(
    system_prompt: Option<&str>,
    tools: &[(String, String, Option<serde_json::Value>)],
) -> serde_json::Value {
    serde_json::json!({
        "type": "custom",
        "customType": "pi.share",
        "id": uuid::Uuid::new_v4().simple().to_string()[..8].to_owned(),
        "timestamp": crate::session_manager::types::iso_from_ms(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0),
        ),
        "data": {
            "systemPrompt": system_prompt,
            "tools": tools.iter().map(|(name, description, parameters)| {
                serde_json::json!({
                    "name": name,
                    "description": description,
                    "parameters": parameters,
                })
            }).collect::<Vec<_>>(),
        },
    })
}

/// The optional `/bug [description]` hint (upstream prompts for it; pir
/// takes it from the command arguments).
pub fn bug_hint(args: &str) -> Option<String> {
    let hint = args.trim();
    (!hint.is_empty()).then(|| hint.to_owned())
}

// ---------------------------------------------------------------------------
// /bug — report bundle (upstream `reportBug` + `collectBugReportMetadata`).
// Upstream uploads through Radius with a zip-export fallback; pir keeps the
// export leg as a markdown bundle in the working directory.
// ---------------------------------------------------------------------------

/// One line of the bug-report bundle.
fn bug_report_line(label: &str, value: &str) -> String {
    format!("- {label}: {value}")
}

/// Assemble the bug-report markdown (pure): pir version, OS, cwd, the
/// current model/provider, the session file, the user hint, and the
/// upstream disclaimer so the report says what it contains.
pub fn bug_report_text(inputs: &BugReportInputs) -> String {
    bug_report_text_with_crash(inputs, None)
}

/// [`bug_report_text`] with the crash-log diagnostics attached (upstream
/// `collectBugReportDiagnostics` carries the persisted crash records and the
/// report says the details are attached, interactive-mode.ts:1224). pir has
/// no Radius upload runtime — the report is a local markdown bundle, so the
/// crash summary rides inside it.
pub fn bug_report_text_with_crash(
    inputs: &BugReportInputs,
    crash: Option<&crate::coding_agent::crash_log::CrashRecord>,
) -> String {
    let mut lines = vec![
        "# pir bug report".to_owned(),
        String::new(),
        bug_report_line("pir version", &inputs.pir_version),
        bug_report_line("OS", &inputs.os),
        bug_report_line("cwd", &inputs.cwd),
        bug_report_line(
            "model",
            &format!("{}/{}", inputs.model_provider, inputs.model_id),
        ),
        bug_report_line("thinking level", &inputs.thinking_level),
        bug_report_line(
            "session storage",
            inputs.session_path.as_deref().unwrap_or("(in memory)"),
        ),
        bug_report_line("context entries", &inputs.entry_count.to_string()),
        bug_report_line("report generated at", &inputs.generated_at),
    ];
    if let Some(hint) = &inputs.hint {
        lines.push(bug_report_line("description", hint));
    }
    if let Some(crash) = crash {
        lines.push(bug_report_line(
            "last crash",
            &format!(
                "{} ({}): {}",
                crash.timestamp,
                match crash.kind {
                    crate::coding_agent::crash_log::CrashKind::UncaughtException => {
                        "uncaught_exception"
                    }
                    crate::coding_agent::crash_log::CrashKind::FatalError => "fatal_error",
                },
                crash.message
            ),
        ));
        if let Some(stack) = &crash.stack {
            lines.push(String::new());
            lines.push("Crash stack (truncated):".to_owned());
            // A bounded excerpt keeps the markdown bundle readable.
            for frame in stack.lines().take(20) {
                lines.push(format!("    {frame}"));
            }
        }
    }
    lines.push(String::new());
    lines.push(
        "This report is NOT uploaded automatically: attach it to a pir issue \
         manually. It includes the pir version, operating system, current \
         model/provider, and session metadata — no credentials. The transcript \
         itself stays on your machine (share it at your own discretion)."
            .to_owned(),
    );
    lines.join("\n")
}

/// Inputs to [`bug_report_text`] (upstream `collectBugReportMetadata`, the
/// fields pir can fill without the Radius runtime).
#[derive(Debug, Clone, Default)]
pub struct BugReportInputs {
    pub pir_version: String,
    pub os: String,
    pub cwd: String,
    pub model_provider: String,
    pub model_id: String,
    pub thinking_level: String,
    pub session_path: Option<String>,
    pub entry_count: usize,
    pub generated_at: String,
    pub hint: Option<String>,
}

/// The `bug-report-<id>.md` file name (upstream `bugReportArchiveFileName`
/// shape, markdown instead of zip).
pub fn bug_report_file_name(generated_at: &str) -> String {
    let stamp: String = generated_at
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    format!("bug-report-{stamp}.md")
}

// ---------------------------------------------------------------------------
// /changelog — parse the repo CHANGELOG.md (upstream
// `utils/changelog.ts:parseChangelog`: scan `## ` headers for `x.y.z`
// versions and collect content until the next header).
// ---------------------------------------------------------------------------

/// One parsed changelog entry (upstream `ChangelogEntry`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangelogEntry {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub content: String,
}

/// Parse changelog entries from a CHANGELOG.md body (pure; upstream
/// `parseChangelog`). A `## ` header without a parseable version resets the
/// current entry.
pub fn parse_changelog(content: &str) -> Vec<ChangelogEntry> {
    let mut entries: Vec<ChangelogEntry> = Vec::new();
    let mut current_version: Option<(u32, u32, u32)> = None;
    let mut current_lines: Vec<&str> = Vec::new();
    for line in content.split('\n') {
        if let Some(rest) = line.strip_prefix("## ") {
            if let Some((major, minor, patch)) = current_version.take() {
                if !current_lines.is_empty() {
                    entries.push(ChangelogEntry {
                        major,
                        minor,
                        patch,
                        content: current_lines.join("\n").trim().to_owned(),
                    });
                }
            }
            let version = version_from_header(rest);
            match version {
                Some((major, minor, patch)) => {
                    current_version = Some((major, minor, patch));
                    current_lines = vec![line];
                }
                None => current_lines.clear(),
            }
        } else if current_version.is_some() {
            current_lines.push(line);
        }
    }
    if let Some((major, minor, patch)) = current_version {
        if !current_lines.is_empty() {
            entries.push(ChangelogEntry {
                major,
                minor,
                patch,
                content: current_lines.join("\n").trim().to_owned(),
            });
        }
    }
    entries
}

/// `x.y.z` out of a `## [1.2.3] …` / `## 1.2.3 …` header line (upstream
/// `versionMatch`).
fn version_from_header(header: &str) -> Option<(u32, u32, u32)> {
    let trimmed = header.trim_start_matches('[');
    let mut parts = trimmed.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch: String = parts
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if patch.is_empty() {
        return None;
    }
    Some((major, minor, patch.parse().ok()?))
}

/// Render the `/changelog` notice: newest first (upstream reverses the
/// parsed order), full content per entry.
pub fn changelog_text(content: &str) -> String {
    let mut entries = parse_changelog(content);
    if entries.is_empty() {
        return "No changelog entries found.".to_owned();
    }
    entries.reverse();
    let mut lines = vec!["What's New".to_owned()];
    for entry in entries {
        lines.push(String::new());
        lines.push(format!(
            "## v{}.{}.{}",
            entry.major, entry.minor, entry.patch
        ));
        lines.push(entry.content);
    }
    lines.join("\n")
}

/// The collapsed changelog notice (upstream `collapseChangelog`,
/// interactive-mode.ts:857-863): `Updated to v<latest>. Use /changelog to
/// view full changelog.` The latest version comes from the newest parsed
/// entry, falling back to the running version like upstream's regex miss.
pub fn changelog_collapse_notice(content: &str, fallback_version: &str) -> String {
    let latest = parse_changelog(content)
        .last()
        .map(|entry| format!("{}.{}.{}", entry.major, entry.minor, entry.patch))
        .unwrap_or_else(|| fallback_version.to_owned());
    format!("Updated to v{latest}. Use /changelog to view full changelog.")
}

/// Render the `/hotkeys` table (upstream `handleHotkeysCommand`): sections
/// of `(keys — action)` rows built from the effective keybindings. `rows`
/// carries already-formatted key specs so the caller stays in charge of the
/// combo formatting.
pub fn hotkeys_text(sections: &[(&str, Vec<(String, String)>)]) -> String {
    let mut lines = vec!["Keyboard Shortcuts".to_owned()];
    for (title, rows) in sections {
        if rows.is_empty() {
            continue;
        }
        lines.push(String::new());
        lines.push((*title).to_owned());
        for (keys, action) in rows {
            lines.push(format!("{keys} — {action}"));
        }
    }
    lines.join("\n")
}

/// The `/hotkeys` "Extensions" section rows (upstream
/// `handleHotkeysCommand`'s extension-registered shortcuts block,
/// interactive-mode.ts:6901-6914): one `(key — description)` row per
/// registered shortcut, the description falling back to the extension path.
pub fn extension_hotkey_rows(
    shortcuts: &[crate::extensions::runtime::ShortcutDef],
) -> Vec<(String, String)> {
    shortcuts
        .iter()
        .map(|shortcut| {
            let description = shortcut
                .description
                .clone()
                .unwrap_or_else(|| shortcut.source_info.path.clone());
            (shortcut.key.clone(), description)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// /session — session info + cost/tokens breakdown (upstream
// `handleSessionCommand`, interactive-mode.ts:6693-6772, fed by
// `getUsageCostBreakdown` + `computeCacheWaste`). The aggregation lives in
// `agent_session` (next to `session_stats`); this is the renderer.
// ---------------------------------------------------------------------------

/// One per-model cost bucket (upstream `UsageCostBreakdownEntry`); the
/// aggregation lives in [`crate::agent_session::usage_cost_breakdown`].
pub use crate::agent_session::UsageCostEntry;

/// The inputs [`session_info_text`] renders (upstream snapshots
/// `getSessionStats` + the breakdown + cache-warming status).
#[derive(Debug, Clone, Default)]
pub struct SessionInfoData {
    pub session_name: Option<String>,
    pub session_file: Option<String>,
    pub session_id: String,
    pub total_messages: usize,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_write: u64,
    pub tokens_total: u64,
    pub cost: f64,
    /// Per-model cost buckets, cost-descending (upstream `usageBreakdown`).
    pub usage_breakdown: Vec<UsageCostEntry>,
    /// The `provider/id` key of the currently selected model.
    pub selected_model_key: String,
    /// Cache-warming mode label (upstream `getCacheWarmingMode()`).
    pub cache_warming_mode: String,
    /// Cache-warming status label; `None` renders upstream's
    /// "Inactive (cache warming unavailable)".
    pub cache_warming_status: Option<String>,
    /// Cache re-billing totals (upstream `computeCacheWaste`).
    pub cache_missed_tokens: u64,
    pub cache_missed_cost: f64,
    pub cache_miss_count: u64,
}

/// `1,234,567` (upstream `Number.prototype.toLocaleString()` with the default
/// en-US grouping).
pub fn format_thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Compact token counts (upstream `formatTokens`, footer.ts: 1.2k / 12k /
/// 1.2M / 12M).
pub fn format_tokens(count: u64) -> String {
    if count < 1000 {
        return count.to_string();
    }
    if count < 10_000 {
        return format!("{:.1}k", count as f64 / 1000.0);
    }
    if count < 1_000_000 {
        return format!("{}k", (count as f64 / 1000.0).round() as u64);
    }
    if count < 10_000_000 {
        return format!("{:.1}M", count as f64 / 1_000_000.0);
    }
    format!("{}M", (count as f64 / 1_000_000.0).round() as u64)
}

/// The compaction / branch-summary billed notice (upstream
/// `addCompactionCostNotice`, interactive-mode.ts:4110-4126, gated by
/// `showCacheMissNotices`): `Compaction: 12k tokens billed (~$0.05)`.
pub fn billed_usage_notice(label: &str, usage: &crate::ai::types::Usage) -> String {
    let tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
    let cost = if usage.cost.total >= 0.01 {
        format!(" (~${:.2})", usage.cost.total)
    } else {
        String::new()
    };
    format!("{label}: {} tokens billed{cost}", format_tokens(tokens))
}

/// Render `/session` (upstream `handleSessionCommand`'s `renderInfo`):
/// Session Info, Messages, Tokens (with the cached/uncached prompt split),
/// Cache Warming, and the Cost block with the per-model breakdown and the
/// Cache Re-billed line.
pub fn session_info_text(data: &SessionInfoData) -> String {
    let mut info = String::from("Session Info\n\n");
    if let Some(name) = &data.session_name {
        info.push_str(&format!("Name: {name}\n"));
    }
    info.push_str(&format!(
        "File: {}\n",
        data.session_file.as_deref().unwrap_or("In-memory")
    ));
    info.push_str(&format!("ID: {}\n", data.session_id));
    info.push_str("\nMessages\n");
    info.push_str(&format!("Total: {}\n", data.total_messages));
    info.push_str(&format!("User: {}\n", data.user_messages));
    info.push_str(&format!("Assistant: {}\n", data.assistant_messages));
    info.push_str(&format!(
        "Tools: {} calls, {} results\n",
        data.tool_calls, data.tool_results
    ));

    info.push_str("\nTokens\n");
    // "Input" is the full prompt volume; with cache activity it splits into
    // cached (read) vs uncached (everything else).
    let prompt_tokens = data.tokens_input + data.tokens_cache_read + data.tokens_cache_write;
    info.push_str(&format!("Input: {}\n", format_thousands(prompt_tokens)));
    if prompt_tokens > 0 && (data.tokens_cache_read > 0 || data.tokens_cache_write > 0) {
        let hit_rate = (data.tokens_cache_read as f64 / prompt_tokens as f64 * 100.0).max(0.0);
        info.push_str(&format!(
            "  Cached: {} ({hit_rate:.1}%)\n",
            format_thousands(data.tokens_cache_read)
        ));
        let written = if data.tokens_cache_write > 0 {
            format!(
                " ({} written to cache)",
                format_thousands(data.tokens_cache_write)
            )
        } else {
            String::new()
        };
        info.push_str(&format!(
            "  Uncached: {}{written}\n",
            format_thousands(data.tokens_input + data.tokens_cache_write)
        ));
    }
    info.push_str(&format!(
        "Output: {}\n",
        format_thousands(data.tokens_output)
    ));
    info.push_str(&format!("Total: {}\n", format_thousands(data.tokens_total)));

    info.push_str("\nCache Warming\n");
    info.push_str(&format!("Mode: {}\n", data.cache_warming_mode));
    info.push_str(&format!(
        "Status: {}\n",
        data.cache_warming_status
            .as_deref()
            .unwrap_or("Inactive (cache warming unavailable)")
    ));

    let has_breakdown = data.usage_breakdown.len() > 1
        || data
            .usage_breakdown
            .first()
            .is_some_and(|entry| entry.key != data.selected_model_key);
    if data.cost > 0.0 || data.cache_missed_tokens > 0 {
        info.push_str("\nCost\n");
        info.push_str(&format!("Total: ${:.3}", data.cost));
        // A single entry repeats the total, unless it names another model.
        if has_breakdown {
            for entry in &data.usage_breakdown {
                info.push_str(&format!(
                    "\n  {}: ${:.3} ({} tokens)",
                    entry.key,
                    entry.cost,
                    format_tokens(entry.tokens)
                ));
            }
        }
        if data.cache_missed_tokens > 0 {
            let miss_label = if data.cache_miss_count == 1 {
                "1 miss".to_owned()
            } else {
                format!("{} misses", data.cache_miss_count)
            };
            let detail = format!(
                "{} tokens, {miss_label}",
                format_thousands(data.cache_missed_tokens)
            );
            if data.cache_missed_cost >= 0.0001 {
                info.push_str(&format!(
                    "\nCache Re-billed: ${:.3} ({detail})",
                    data.cache_missed_cost
                ));
            } else {
                info.push_str(&format!("\nCache Re-billed: {detail}"));
            }
        }
    }
    info
}

// ---------------------------------------------------------------------------
// /scoped-models — the session-only enabled-model set (upstream
// `components/scoped-models-selector.ts` pure helpers). `None` means "all
// models enabled" (no filter); `Some(list)` is the explicit ordered
// enabled-id list, whose order drives model cycling.
// ---------------------------------------------------------------------------

/// The ordered enabled-model set for Ctrl+P-style cycling (upstream
/// `EnabledIds`), a typed state instead of scattered flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnabledModels {
    /// `None` = all enabled; `Some(ids)` = explicit ordered enabled list.
    enabled: Option<Vec<String>>,
}

impl EnabledModels {
    /// All models enabled (upstream `enabledIds === null`).
    pub fn all() -> Self {
        Self { enabled: None }
    }

    /// An explicit ordered enabled list (empty means "nothing enabled").
    pub fn explicit(ids: Vec<String>) -> Self {
        Self { enabled: Some(ids) }
    }

    /// The current enabled id list, or `None` when everything is enabled.
    pub fn enabled_ids(&self) -> Option<&[String]> {
        self.enabled.as_deref()
    }

    /// Whether `id` is enabled (upstream `isEnabled`).
    pub fn is_enabled(&self, id: &str) -> bool {
        match &self.enabled {
            None => true,
            Some(ids) => ids.iter().any(|enabled| enabled == id),
        }
    }

    /// Whether every model is enabled (the `null` representation).
    pub fn is_all_enabled(&self) -> bool {
        self.enabled.is_none()
    }

    /// Toggle one id (upstream `toggle`): from "all enabled" the id is
    /// removed; from an explicit list it is removed, or added back — the
    /// add collapses to "all enabled" when the list covers every available
    /// model (`normalizeEnabled`).
    pub fn toggle(&mut self, id: &str, all_ids: &[String]) {
        match self.enabled.take() {
            // Upstream `allIds.filter(≠ id)` — the complement cannot cover
            // every model, so no collapse check.
            None => {
                self.enabled = Some(
                    all_ids
                        .iter()
                        .filter(|model_id| model_id.as_str() != id)
                        .cloned()
                        .collect(),
                );
            }
            Some(mut ids) => {
                if let Some(index) = ids.iter().position(|enabled| enabled == id) {
                    ids.remove(index);
                    self.enabled = Some(ids);
                } else {
                    ids.push(id.to_owned());
                    self.enabled = Self::normalized(ids, all_ids);
                }
            }
        }
    }

    /// Enable ids (upstream `enableAll`): from "all enabled" this is a
    /// no-op; explicit lists append the missing targets then normalize
    /// (collapsing back to "all enabled" on full coverage).
    pub fn enable_all(&mut self, target_ids: Option<&[String]>, all_ids: &[String]) {
        let Some(mut ids) = self.enabled.take() else {
            return;
        };
        let targets = target_ids.unwrap_or(all_ids);
        for id in targets {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        self.enabled = Self::normalized(ids, all_ids);
    }

    /// Disable ids (upstream `clearAll`): from "all enabled" this builds the
    /// complement; explicit lists drop the targets (no collapse check,
    /// matching upstream).
    pub fn clear_all(&mut self, target_ids: Option<&[String]>, all_ids: &[String]) {
        match self.enabled.take() {
            None => {
                self.enabled = Some(match target_ids {
                    Some(targets) => all_ids
                        .iter()
                        .filter(|id| !targets.contains(id))
                        .cloned()
                        .collect(),
                    None => Vec::new(),
                });
            }
            Some(ids) => {
                self.enabled = Some(match target_ids {
                    Some(targets) => ids.into_iter().filter(|id| !targets.contains(id)).collect(),
                    None => Vec::new(),
                });
            }
        }
    }

    /// Display order (upstream `getSortedIds`): explicit enabled ids first
    /// in their order, then the rest of the available ids.
    pub fn sorted_ids(&self, all_ids: &[String]) -> Vec<String> {
        match &self.enabled {
            None => all_ids.to_vec(),
            Some(ids) => {
                let mut ordered = ids.clone();
                for id in all_ids {
                    if !ordered.contains(id) {
                        ordered.push(id.clone());
                    }
                }
                ordered
            }
        }
    }

    /// Collapse an explicit list back to "all enabled" (`None`) when it
    /// covers every available model (upstream `normalizeEnabled`).
    fn normalized(ids: Vec<String>, all_ids: &[String]) -> Option<Vec<String>> {
        let covers_all = ids.len() == all_ids.len() && all_ids.iter().all(|id| ids.contains(id));
        if covers_all {
            None
        } else {
            Some(ids)
        }
    }
}

/// The next scoped model to cycle to (upstream
/// `AgentSession._cycleScopedModel`): filter the scope to models the
/// registry can still serve, find the current position (falling back to 0
/// when the current model is outside the scope), and step forward with
/// wrap-around. `None` when fewer than two scoped models remain available.
///
/// The policy itself lives in [`crate::agent_session::scoped_cycle_target`],
/// which headless hosts use too; this is the TUI's name for it.
pub fn scoped_cycle_target(
    scoped: &[(String, String)],
    available: &[(String, String)],
    current: (&str, &str),
) -> Option<(String, String)> {
    crate::agent_session::scoped_cycle_target(scoped, available, current)
}
