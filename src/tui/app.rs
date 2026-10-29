//! Interactive-mode assembly: header, footer, status, and the chat
//! transcript composition — port of upstream `interactive-mode.ts` +
//! `footer.ts` + `status-indicator.ts` line shapes.

// Pre-existing baseline lints (tui is frozen for the parity batch).
#![allow(clippy::unnecessary_unwrap)]
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::useless_format)]
#![allow(clippy::useless_vec)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::ansi::{truncate_to_width, visible_width};
use super::core::{Component, Container, Rows, Spacer, Text};
use super::markdown::{Markdown, MarkdownOptions, MarkdownTheme};
use super::messages::{
    parse_skill_block, AssistantMessageComponent, SkillInvocationComponent, StreamingThinking,
    ToolExecutionComponent, UserMessageComponent,
};
use super::renderers::ToolRenderShell;
use super::theme::{with_theme, Theme, ThemeColor};
use crate::ai::types::AssistantMessage;

/// Usage totals for the footer (upstream `createUsageTotals` semantics).
#[derive(Debug, Clone, Copy, Default)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
}

impl UsageTotals {
    pub fn add(&mut self, usage: &crate::ai::types::Usage) {
        self.input += usage.input;
        self.output += usage.output;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write;
        // Cost follows the model rates when present; usage carries the
        // provider-reported total when available.
        if usage.cost.total > 0.0 {
            self.cost += usage.cost.total;
        }
    }
}

/// Exact equality for the footer render-key ([`FooterRowsKey`]).
///
/// `cost` compares by bit pattern rather than `==`: the footer renders
/// `$-0.000` for `-0.0` where `0.0` renders `$0.000`, and a NaN renders as
/// `NaN`, so `==` would call two different byte outputs equal. Bit equality
/// is also reflexive, so a NaN key hits its own entry instead of rebuilding
/// on every frame.
impl PartialEq for UsageTotals {
    fn eq(&self, other: &Self) -> bool {
        self.input == other.input
            && self.output == other.output
            && self.cache_read == other.cache_read
            && self.cache_write == other.cache_write
            && self.cost.to_bits() == other.cost.to_bits()
    }
}

/// Compact token formatting (upstream `formatTokens`).
pub fn format_tokens(count: u64) -> String {
    if count < 1000 {
        return count.to_string();
    }
    if count < 10_000 {
        return format!("{:.1}k", count as f64 / 1000.0);
    }
    if count < 1_000_000 {
        return format!("{}k", count / 1000);
    }
    if count < 10_000_000 {
        return format!("{:.1}M", count as f64 / 1_000_000.0);
    }
    format!("{}M", count / 1_000_000)
}

/// Replace the home prefix with `~` (upstream `formatCwdForFooter`).
pub fn format_cwd_for_footer(cwd: &str, home: Option<&str>) -> String {
    let Some(home) = home else {
        return cwd.to_string();
    };
    let home = home.trim_end_matches(['/', '\\']);
    if cwd == home {
        return "~".to_string();
    }
    let prefix = format!("{home}/");
    if let Some(rest) = cwd.strip_prefix(&prefix) {
        return format!("~/{rest}");
    }
    let prefix_back = format!("{home}\\");
    if let Some(rest) = cwd.strip_prefix(&prefix_back) {
        return format!("~\\{rest}");
    }
    cwd.to_string()
}

// ---------------------------------------------------------------------------
// Session tabs (same-window multi-session).
//
// A tab is one session file the TUI can attach to. Only the ACTIVE tab's
// session is live in the worker: switching reuses the existing session-attach
// machinery (`/resume`'s `AgentSession::replace_session_storage` + transcript
// replay), so N open tabs never mean N live `AgentSession`s. The tab list is
// in-memory only — nothing here is persisted.
//
// `draft` holds the editor text of an INACTIVE tab. The active tab's draft
// lives in the editor itself: restoring a tab takes its draft out (see
// [`take_active_draft`]) and switching away saves the editor text back (see
// [`save_active_draft`]), so a draft survives any number of round trips
// without ever being duplicated.
// ---------------------------------------------------------------------------

/// One open session tab.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionTab {
    /// The session file the worker attaches to (the tab's identity).
    pub path: String,
    /// Display title (see [`session_tab_title`]).
    pub title: String,
    /// Editor text remembered while the tab is inactive.
    pub draft: String,
}

/// Longest first-user-message preview kept by [`session_tab_title`].
const SESSION_TAB_TITLE_CHARS: usize = 60;

/// The display title of a session tab, by precedence:
///
/// 1. the session's user-set name (`/name`),
/// 2. the first user message's preview (first non-empty line, whitespace
///    collapsed, capped at [`SESSION_TAB_TITLE_CHARS`] chars),
/// 3. the session file's stem.
pub fn session_tab_title(
    name: Option<&str>,
    first_user_message: Option<&str>,
    path: &str,
) -> String {
    if let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) {
        return name.to_owned();
    }
    if let Some(preview) = first_user_message.and_then(session_tab_preview) {
        return preview;
    }
    std::path::Path::new(path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| path.to_owned())
}

/// The first non-empty line of `text`, whitespace-collapsed and truncated.
fn session_tab_preview(text: &str) -> Option<String> {
    let line = text.lines().find_map(|line| {
        let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        (!collapsed.is_empty()).then_some(collapsed)
    })?;
    if line.chars().count() > SESSION_TAB_TITLE_CHARS {
        let mut truncated: String = line
            .chars()
            .take(SESSION_TAB_TITLE_CHARS.saturating_sub(1))
            .collect();
        truncated.push('…');
        Some(truncated)
    } else {
        Some(line)
    }
}

/// The index of the active tab, or `None` when the path is not an open tab.
pub fn active_tab_index(tabs: &[SessionTab], active_path: Option<&str>) -> Option<usize> {
    let active = active_path?;
    tabs.iter().position(|tab| tab.path == active)
}

/// Add `path` as a tab (deduplicating by path) and activate it. An existing
/// tab keeps its draft and takes the fresh `title` when `title` is non-empty.
/// Returns the tab index.
pub fn add_or_activate_tab(
    tabs: &mut Vec<SessionTab>,
    active_path: &mut Option<String>,
    path: &str,
    title: &str,
) -> usize {
    if let Some(index) = tabs.iter().position(|tab| tab.path == path) {
        if !title.is_empty() {
            tabs[index].title = title.to_owned();
        }
        *active_path = Some(path.to_owned());
        return index;
    }
    tabs.push(SessionTab {
        path: path.to_owned(),
        title: title.to_owned(),
        draft: String::new(),
    });
    *active_path = Some(path.to_owned());
    tabs.len() - 1
}

/// Activate an open tab. Returns whether the path names one.
pub fn activate_tab(tabs: &[SessionTab], active_path: &mut Option<String>, path: &str) -> bool {
    if tabs.iter().any(|tab| tab.path == path) {
        *active_path = Some(path.to_owned());
        true
    } else {
        false
    }
}

/// The path of the next (`forward`) or previous tab, wrapping around. A
/// missing active tab starts at the first (forward) or last (backward) tab.
pub fn cycle_tab(tabs: &[SessionTab], active_path: Option<&str>, forward: bool) -> Option<String> {
    if tabs.is_empty() {
        return None;
    }
    let count = tabs.len();
    let index = match active_tab_index(tabs, active_path) {
        Some(index) => index,
        None => return Some(tabs[if forward { 0 } else { count - 1 }].path.clone()),
    };
    let next = if forward {
        (index + 1) % count
    } else {
        (index + count - 1) % count
    };
    Some(tabs[next].path.clone())
}

/// Remove `path` from the tabs. When it was the active tab the nearest
/// survivor becomes active — the tab that slid into its slot, else the one
/// before it, else `None` (no tabs left). Returns the new active path.
pub fn close_tab(
    tabs: &mut Vec<SessionTab>,
    active_path: &mut Option<String>,
    path: &str,
) -> Option<String> {
    let Some(index) = tabs.iter().position(|tab| tab.path == path) else {
        return active_path.clone();
    };
    let was_active = active_path.as_deref() == Some(path);
    tabs.remove(index);
    if !was_active {
        return active_path.clone();
    }
    let next = tabs
        .get(index)
        .or_else(|| index.checked_sub(1).and_then(|previous| tabs.get(previous)))
        .map(|tab| tab.path.clone());
    *active_path = next.clone();
    next
}

/// Remember `draft` as the active tab's editor text.
pub fn save_active_draft(tabs: &mut [SessionTab], active_path: Option<&str>, draft: &str) {
    if let Some(index) = active_tab_index(tabs, active_path) {
        tabs[index].draft = draft.to_owned();
    }
}

/// Take the active tab's draft out of the tab (the editor now owns it).
pub fn take_active_draft(tabs: &mut [SessionTab], active_path: Option<&str>) -> String {
    active_tab_index(tabs, active_path)
        .map(|index| std::mem::take(&mut tabs[index].draft))
        .unwrap_or_default()
}

/// Rename the active tab (`/name`).
pub fn set_active_title(tabs: &mut [SessionTab], active_path: Option<&str>, title: &str) {
    if let Some(index) = active_tab_index(tabs, active_path) {
        if !title.is_empty() {
            tabs[index].title = title.to_owned();
        }
    }
}

/// The footer's tab indicator, `[n/N] title`, or `None` when fewer than two
/// tabs are open (a single tab is not worth a footer marker).
pub fn session_tab_indicator(tabs: &[SessionTab], active_path: Option<&str>) -> Option<String> {
    if tabs.len() < 2 {
        return None;
    }
    let index = active_tab_index(tabs, active_path).unwrap_or(0);
    let title = tabs.get(index)?.title.as_str();
    Some(format!("[{}/{}] {}", index + 1, tabs.len(), title))
}

/// Footer state (upstream `FooterComponent.render` inputs).
#[derive(Debug, Clone, Default)]
pub struct FooterState {
    pub cwd: String,
    pub git_branch: Option<String>,
    pub session_name: Option<String>,
    /// The multi-session tab indicator (`[n/N] title`), rendered after the
    /// session name; `None` with fewer than two open tabs.
    pub session_tab: Option<String>,
    pub usage: UsageTotals,
    pub context_percent: Option<f64>,
    pub context_window: u64,
    pub auto_compact: bool,
    pub model_id: String,
    pub model_provider: String,
    pub model_reasoning: bool,
    pub thinking_level: String,
    pub extension_statuses: Vec<(String, String)>,
    /// Latest assistant turn's cache-hit rate (upstream `latestCacheHitRate`,
    /// footer.ts:130-133: `cacheRead / (input + cacheRead + cacheWrite)` of
    /// the most recent assistant usage).
    pub cache_hit_rate: Option<f64>,
    /// Subscription-backed billing for the current provider (upstream
    /// `usingSubscription`, footer.ts:190-196: kimi-coding or an OAuth
    /// credential); renders the `(sub)` marker after the cost.
    pub subscription: bool,
    /// Experimental features enabled (upstream `areExperimentalFeaturesEnabled`,
    /// footer.ts:213-215); renders the `• xp` marker.
    pub experimental: bool,
    /// The physical model the latest assistant response routed to, with its
    /// thinking level (upstream `session.routedModel`, footer.ts:240-245 —
    /// virtual models route per request; `None` when not routing).
    pub routed: Option<(String, Option<String>)>,
    /// Number of distinct providers with available models (upstream
    /// `footerData.getAvailableProviderCount()`, footer.ts:249-255: the
    /// provider prefix renders only when there is more than one).
    pub available_providers: usize,
}

/// Exact equality for the footer memo's render key ([`FooterRowsKey`]): every
/// field [`render_footer_into`] reads, compared by value. The optional floats
/// compare by bit pattern (see [`UsageTotals`]'s `PartialEq` — `-0.0` and
/// `0.0` render differently, a NaN key must still hit itself), and the
/// statuses compare structurally. The relation is deliberately stricter than
/// the output in one spot: the render sorts the statuses before joining, so a
/// state that only reordered them compares unequal and rebuilds — a spurious
/// rebuild, never a stale hit.
impl PartialEq for FooterState {
    fn eq(&self, other: &Self) -> bool {
        self.cwd == other.cwd
            && self.git_branch == other.git_branch
            && self.session_name == other.session_name
            && self.session_tab == other.session_tab
            && self.usage == other.usage
            && self.context_percent.map(f64::to_bits) == other.context_percent.map(f64::to_bits)
            && self.context_window == other.context_window
            && self.auto_compact == other.auto_compact
            && self.model_id == other.model_id
            && self.model_provider == other.model_provider
            && self.model_reasoning == other.model_reasoning
            && self.thinking_level == other.thinking_level
            && self.extension_statuses == other.extension_statuses
            && self.cache_hit_rate.map(f64::to_bits) == other.cache_hit_rate.map(f64::to_bits)
            && self.subscription == other.subscription
            && self.experimental == other.experimental
            && self.routed == other.routed
            && self.available_providers == other.available_providers
    }
}

/// Sanitize a status line (upstream `sanitizeStatusText`).
fn sanitize_status_text(text: &str) -> String {
    let collapsed: String = text
        .replace(['\r', '\n', '\t'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    collapsed.trim().to_string()
}

/// Render the footer lines: pwd line + stats line + optional extension
/// statuses (exact upstream shapes).
///
/// The trait's owned-line form: the same rows, built through
/// [`render_footer_into`] in a fresh pool. The frame renderer drives the
/// canvas path; this bridge keeps `render_footer` correct for callers that ask
/// for lines (tests, alternate drivers).
pub fn render_footer(width: usize, state: &FooterState) -> Vec<String> {
    let mut lines = Vec::new();
    {
        let mut rows = Rows::new(&mut lines);
        render_footer_into(width, state, &mut rows);
        rows.trim();
    }
    lines
}

/// [`render_footer`]'s body writing straight into the frame canvas: each
/// footer row is pushed into a recycled row buffer instead of into a
/// per-frame `Vec<String>` (the line buffers themselves are the same strings
/// the owned form produced). Row for row byte-identical to [`render_footer`].
pub fn render_footer_into(width: usize, state: &FooterState, out: &mut Rows<'_>) {
    let mut pwd = format_cwd_for_footer(&state.cwd, home_dir().as_deref());
    if let Some(branch) = &state.git_branch {
        pwd = format!("{pwd} ({branch})");
    }
    if let Some(name) = &state.session_name {
        pwd = format!("{pwd} • {name}");
    }
    if let Some(tab) = &state.session_tab {
        pwd = format!("{pwd} • {tab}");
    }

    let mut stats_parts: Vec<String> = Vec::new();
    if state.usage.input > 0 {
        stats_parts.push(format!("↑{}", format_tokens(state.usage.input)));
    }
    if state.usage.output > 0 {
        stats_parts.push(format!("↓{}", format_tokens(state.usage.output)));
    }
    if state.usage.cache_read > 0 {
        stats_parts.push(format!("R{}", format_tokens(state.usage.cache_read)));
    }
    if state.usage.cache_write > 0 {
        stats_parts.push(format!("W{}", format_tokens(state.usage.cache_write)));
    }
    if let Some(cache_hit_rate) = state
        .cache_hit_rate
        .filter(|_| state.usage.cache_read > 0 || state.usage.cache_write > 0)
    {
        stats_parts.push(format!("CH{cache_hit_rate:.1}%"));
    }
    // Cost + subscription marker (upstream footer.ts:189-196): the cost line
    // renders when any cost accrued, or when the provider is
    // subscription-backed (Kimi Coding bills via API-key auth but against a
    // plan).
    let using_subscription = state.subscription || state.model_provider == "kimi-coding";
    // Upstream gates on truthiness (`if (usageTotals.cost || usingSubscription)`,
    // footer.ts:193): any non-zero cost renders the line.
    if state.usage.cost != 0.0 || using_subscription {
        let suffix = if using_subscription { " (sub)" } else { "" };
        stats_parts.push(format!("${:.3}{suffix}", state.usage.cost));
    }

    // Context percentage with warning/error coloring (upstream thresholds).
    let auto_indicator = if state.auto_compact { " (auto)" } else { "" };
    let context_display = match state.context_percent {
        Some(percent) => format!(
            "{:.1}%/{}{}",
            percent,
            format_tokens(state.context_window),
            auto_indicator
        ),
        None => format!(
            "?/{}{}",
            format_tokens(state.context_window),
            auto_indicator
        ),
    };
    let percent = state.context_percent.unwrap_or(0.0);
    let context_colored = if percent > 90.0 {
        with_theme(|theme| theme.fg(ThemeColor::Error, &context_display))
    } else if percent > 70.0 {
        with_theme(|theme| theme.fg(ThemeColor::Warning, &context_display))
    } else {
        context_display
    };
    stats_parts.push(context_colored);
    // Experimental-features marker (upstream footer.ts:213-215).
    if state.experimental {
        let marker = with_theme(|theme| {
            format!(
                "{} {}",
                theme.fg(ThemeColor::Dim, "•"),
                theme.bold(&theme.fg(ThemeColor::Warning, "xp"))
            )
        });
        stats_parts.push(marker);
    }
    let stats_left = stats_parts.join(" ");

    // Right side: model + thinking level (+ provider when multiple).
    let mut right = if state.model_reasoning {
        let level = if state.thinking_level.is_empty() || state.thinking_level == "off" {
            format!("{} • thinking off", state.model_id)
        } else {
            format!("{} • {}", state.model_id, state.thinking_level)
        };
        level
    } else {
        state.model_id.clone()
    };
    // Virtual-model routing display (upstream footer.ts:240-245): show where
    // the latest response went (` → routed.id[ • level]`); the level renders
    // when truthy (upstream `routed.thinkingLevel ? … : ""`).
    if let Some((routed_id, routed_level)) = &state.routed {
        let level = routed_level
            .as_deref()
            .filter(|level| !level.is_empty())
            .map(|level| format!(" • {level}"))
            .unwrap_or_default();
        right.push_str(&format!(" → {routed_id}{level}"));
    }
    // Provider prefix only when MORE THAN ONE provider has available models
    // (upstream footer.ts:249-255).
    if state.available_providers > 1 && !state.model_provider.is_empty() {
        let with_provider = format!("({}) {}", state.model_provider, right);
        if visible_width(&stats_left) + 2 + visible_width(&with_provider) <= width {
            right = with_provider;
        }
    }

    // Compose stats line with right-aligned model (upstream padding rules).
    let min_padding = 2usize;
    let stats_width = visible_width(&stats_left);
    let right_width = visible_width(&right);
    let stats_line = if stats_width + min_padding + right_width <= width {
        let padding = width - stats_width - right_width;
        format!("{stats_left}{}{right}", " ".repeat(padding))
    } else {
        let available_for_right = width.saturating_sub(stats_width + min_padding);
        if available_for_right > 0 {
            let truncated_right = truncate_to_width(&right, available_for_right, "");
            let padding = width.saturating_sub(stats_width + visible_width(&truncated_right));
            format!("{stats_left}{}{truncated_right}", " ".repeat(padding))
        } else {
            truncate_to_width(&stats_left, width, "...")
        }
    };

    let dim = |text: &str| with_theme(|theme| theme.fg(ThemeColor::Dim, text));
    let pwd_line = truncate_to_width(&dim(&pwd), width, &dim("..."));
    out.push(&pwd_line);
    out.push(&dim(&stats_line));

    if !state.extension_statuses.is_empty() {
        let mut statuses = state.extension_statuses.clone();
        statuses.sort_by(|a, b| a.0.cmp(&b.0));
        let status_line = statuses
            .iter()
            .map(|(_, text)| sanitize_status_text(text))
            .collect::<Vec<_>>()
            .join(" ");
        out.push(&truncate_to_width(&status_line, width, &dim("...")));
    }
}

fn home_dir() -> Option<String> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
}

/// The complete render input set of one footer row batch: the whole
/// [`FooterState`], the render width, the active palette generation, and the
/// process home directory.
///
/// The home dir is the one input [`render_footer_into`] does not read from the
/// state — `format_cwd_for_footer` resolves `$HOME`/`$USERPROFILE` per call —
/// so the memo pins the value its rows were built with and the per-frame
/// `home_dir()` read is what decides the hit. The state is held BY VALUE (a
/// miss clones it): a content compare is the only sound identity for it —
/// pointer tags could alias a freed buffer once the state was replaced without
/// an intervening render — and misses only happen when the rows must be
/// rebuilt anyway.
struct FooterRowsKey {
    state: FooterState,
    width: usize,
    theme_generation: u64,
    home: Option<String>,
}

impl FooterRowsKey {
    fn matches(
        &self,
        state: &FooterState,
        width: usize,
        theme_generation: u64,
        home: &Option<String>,
    ) -> bool {
        self.width == width
            && self.theme_generation == theme_generation
            && self.home == *home
            && self.state == *state
    }
}

/// The footer's row memo: the key its rows were built under plus the rows
/// themselves, in one slot (a footer frame is a handful of rows, so a
/// one-entry memo is the whole win). `key = None` forces a rebuild.
#[derive(Default)]
struct FooterRowsMemo {
    key: Option<FooterRowsKey>,
    rows: Vec<String>,
}

/// Process-wide count of footer row rebuilds (memo misses), a measurement
/// seam in the spirit of `messages::ToolMemoWork`: a steady frame must leave
/// it unchanged, and a key move must bump it exactly once. Tests read
/// deltas; production never looks at it.
static FOOTER_ROW_REBUILDS: AtomicU64 = AtomicU64::new(0);

/// The footer memo's rebuild counter as of now (tests / probes).
#[doc(hidden)]
pub fn footer_row_rebuilds() -> u64 {
    FOOTER_ROW_REBUILDS.load(Ordering::Relaxed)
}

/// Zero the footer memo's rebuild counter (tests read a per-frame delta).
#[doc(hidden)]
pub fn reset_footer_row_rebuilds() {
    FOOTER_ROW_REBUILDS.store(0, Ordering::Relaxed);
}

/// The footer component.
pub struct Footer {
    state: Mutex<FooterState>,
    /// Memo of the rendered rows (see [`FooterRowsMemo`]): the built-in footer
    /// re-derives its pwd line (`HOME` lookup + `format!`), the stats line
    /// (per-part `format!`, `visible_width` scans, `truncate_to_width`) and
    /// the status line on every frame otherwise, for a footer that only
    /// changes when its state, the width, the palette or `HOME` does.
    memo: Mutex<FooterRowsMemo>,
}

impl Footer {
    pub fn new(state: FooterState) -> Self {
        Self {
            state: Mutex::new(state),
            memo: Mutex::new(FooterRowsMemo::default()),
        }
    }

    pub fn update(&self, state: FooterState) {
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state;
    }

    /// The memoized rows for `width` under the current state — the same rows
    /// [`render_footer_into`] produces, rebuilt in place only when the key
    /// moved. The guard is returned so callers read the stored rows in place;
    /// nothing is ever taken out of the memo.
    ///
    /// Lock order is always state → memo (the only two locks this component
    /// takes), so the two cannot deadlock.
    fn rows_for(&self, width: usize) -> std::sync::MutexGuard<'_, FooterRowsMemo> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut memo = self
            .memo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // `format_cwd_for_footer` reads the home dir per call, so the fresh
        // read is part of the key — an env change repaints like a state
        // change instead of serving a stale pwd line.
        let home = home_dir();
        // One palette generation per render, like the tool-result memo: the
        // rows are built with `with_theme`, and a generation that moved
        // mid-render misses on the next frame instead of sticking.
        let generation = Theme::generation();
        let fresh = !memo
            .key
            .as_ref()
            .is_some_and(|key| key.matches(&state, width, generation, &home));
        if fresh {
            FOOTER_ROW_REBUILDS.fetch_add(1, Ordering::Relaxed);
            // The frame pool is rewound over the previous row buffers: the
            // rebuild writes into them in place (only the rows past the new
            // frame are trimmed away), so a miss does not pay for the buffers.
            let mut rows = Rows::new(&mut memo.rows);
            render_footer_into(width, &state, &mut rows);
            rows.trim();
            memo.key = Some(FooterRowsKey {
                state: state.clone(),
                width,
                theme_generation: generation,
                home,
            });
        }
        memo
    }
}

impl Component for Footer {
    /// The trait's owned-line form: the same rows, cloned out of the memo when
    /// the inputs did not move. The frame renderer drives the canvas path;
    /// this bridge keeps `render` correct for callers that ask for lines
    /// (tests, alternate drivers) — and byte-identical to the uncached
    /// [`render_footer`] entry for the same state.
    fn render(&self, width: usize) -> Vec<String> {
        self.rows_for(width).rows.clone()
    }

    /// Direct-write path: the memoized rows are copied straight into the
    /// frame's recycled row buffers (the line buffers themselves are the same
    /// strings [`render_footer_into`] produced). Row for row byte-identical
    /// to [`Component::render`].
    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        let memo = self.rows_for(width);
        for row in &memo.rows {
            out.push(row);
        }
    }

    fn invalidate(&self) {
        self.memo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .key = None;
    }
}

/// The app name/title (upstream `APP_NAME`/`APP_TITLE`: "pi"/"π"; pir
/// reports its own binary name, but the header shape is identical).
pub const APP_NAME: &str = "pir";
pub const APP_TITLE: &str = "pir";

/// Startup header (upstream `ExpandableText` collapsed/expanded shapes).
///
/// P2-12: header lines are identified structurally ([`HeaderLineKind`]) and
/// carry RAW text; the theme styling is derived at render time
/// ([`header_line_styled`]), so a live `setTheme` swap restyles the startup
/// header instead of keeping the palette baked in at construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderLineKind {
    Blank,
    Logo,
    Compact,
    Onboarding,
    /// Expanded keybinding list (upstream expanded shape).
    ExpandedKeys,
}

const ONBOARDING_TEXT: &str =
    "Pi can explain its own features and look up its docs. Ask it how to use or extend Pi.";

/// The expanded startup key hints in upstream order (upstream
/// `expandedInstructions`, interactive-mode.ts:1024-1048), with every chord
/// resolved from the LIVE keybinding registry like upstream's `keyText`
/// (keybinding-hints.ts:34-36): settings overrides win, then the
/// platform-resolved upstream defaults (Windows: `ctrl+q` follow-up,
/// `alt+q` dequeue, `alt+v` paste — `core/keybindings.ts:67-78`).
pub fn expanded_key_hints(kb: &super::keybindings::Keybindings) -> Vec<(String, String)> {
    let cycle_models = format!(
        "{}/{}",
        kb.hint_key_text("app.model.cycleForward"),
        kb.hint_key_text("app.model.cycleBackward")
    );
    vec![
        (
            kb.hint_key_text("app.interrupt"),
            "to interrupt".to_string(),
        ),
        (kb.hint_key_text("app.clear"), "to clear".to_string()),
        (
            format!("{} twice", kb.hint_key_text("app.clear")),
            "to exit".to_string(),
        ),
        (kb.hint_key_text("app.exit"), "to exit (empty)".to_string()),
        (kb.hint_key_text("app.suspend"), "to suspend".to_string()),
        (
            kb.keys_for_id("tui.editor.deleteToLineEnd")
                .map(|combos| {
                    combos
                        .iter()
                        .map(super::terminal::combo_text)
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .unwrap_or_default(),
            "to delete to end".to_string(),
        ),
        (
            kb.hint_key_text("app.thinking.cycle"),
            "to cycle thinking level".to_string(),
        ),
        (cycle_models, "to cycle models".to_string()),
        (
            kb.hint_key_text("app.model.select"),
            "to select model".to_string(),
        ),
        (
            kb.hint_key_text("app.tools.expand"),
            "to expand tools".to_string(),
        ),
        (
            kb.hint_key_text("app.thinking.toggle"),
            "to expand thinking".to_string(),
        ),
        (
            kb.hint_key_text("app.editor.external"),
            "for external editor".to_string(),
        ),
        ("/".to_string(), "for commands".to_string()),
        ("!".to_string(), "to run bash".to_string()),
        ("!!".to_string(), "to run bash (no context)".to_string()),
        (
            kb.hint_key_text("app.message.followUp"),
            "to queue follow-up".to_string(),
        ),
        (
            kb.hint_key_text("app.message.dequeue"),
            "to edit all queued messages".to_string(),
        ),
        (
            kb.hint_key_text("app.clipboard.pasteImage"),
            "to paste files on macOS, images, or text".to_string(),
        ),
        ("drop files".to_string(), "to attach".to_string()),
    ]
}

/// The compact startup line (upstream `compactInstructions`,
/// interactive-mode.ts:1049-1056), chords resolved from the live registry.
pub fn compact_line(kb: &super::keybindings::Keybindings) -> String {
    [
        format!("{} interrupt", kb.hint_key_text("app.interrupt")),
        format!(
            "{}/{} clear/exit",
            kb.hint_key_text("app.clear"),
            kb.hint_key_text("app.exit")
        ),
        "/ commands".to_string(),
        "! bash".to_string(),
        format!("{} more", kb.hint_key_text("app.tools.expand")),
    ]
    .join(" · ")
}

/// The platform-default expanded hints ([`expanded_key_hints`] over
/// [`super::keybindings::Keybindings::new`]).
pub fn default_expanded_key_hints() -> Vec<(String, String)> {
    expanded_key_hints(&super::keybindings::Keybindings::new())
}

/// The raw (unstyled) text of one header line — platform defaults
/// ([`super::keybindings::Keybindings::new`]). For lines resolved from a live
/// registry use [`LiveHeaderLine`].
pub fn header_line_raw(kind: HeaderLineKind) -> String {
    header_line_raw_for(kind, &super::keybindings::Keybindings::new())
}

/// The raw (unstyled) text of one header line, chords resolved from `kb`.
pub fn header_line_raw_for(kind: HeaderLineKind, kb: &super::keybindings::Keybindings) -> String {
    match kind {
        HeaderLineKind::Blank => String::new(),
        HeaderLineKind::Logo => format!("{APP_NAME} v{}", env!("CARGO_PKG_VERSION")),
        HeaderLineKind::Compact => compact_line(kb),
        HeaderLineKind::Onboarding => ONBOARDING_TEXT.to_string(),
        HeaderLineKind::ExpandedKeys => expanded_key_hints(kb)
            .iter()
            .map(|(key, description)| format!("{key} {description}"))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Style one header line with the CURRENT theme (applied per render) —
/// platform defaults.
pub fn header_line_styled(kind: HeaderLineKind) -> String {
    header_line_styled_for(kind, &super::keybindings::Keybindings::new())
}

/// Style one header line with the CURRENT theme (applied per render), chords
/// resolved from `kb` (upstream `keyHint`: dim key + muted description).
pub fn header_line_styled_for(
    kind: HeaderLineKind,
    kb: &super::keybindings::Keybindings,
) -> String {
    super::theme::with_theme(|theme| header_line_styled_with(theme, kind, kb))
}

/// Style one header line under an EXPLICIT theme (no global state — the
/// race-free seam for tests and explicit-theme renderers).
pub fn header_line_styled_with(
    theme: &super::theme::Theme,
    kind: HeaderLineKind,
    kb: &super::keybindings::Keybindings,
) -> String {
    let dim = |text: &str| theme.fg(ThemeColor::Dim, text);
    let muted = |text: &str| theme.fg(ThemeColor::Muted, text);
    let hint = |(key, description): &(String, String)| {
        format!("{}{}", dim(key), muted(&format!(" {description}")))
    };
    match kind {
        HeaderLineKind::Blank => String::new(),
        HeaderLineKind::Logo => {
            theme.bold(&theme.fg(ThemeColor::Accent, APP_NAME))
                + &dim(&format!(" v{}", env!("CARGO_PKG_VERSION")))
        }
        HeaderLineKind::Compact => compact_line(kb)
            .split(" · ")
            .map(|part| {
                // Each compact part is "key description" (or "key1/key2
                // description"): dim the leading key text, mute the rest.
                match part.split_once(' ') {
                    Some((key, description)) => {
                        format!("{}{}", dim(key), muted(&format!(" {description}")))
                    }
                    None => dim(part),
                }
            })
            .collect::<Vec<_>>()
            .join(&muted(" · ")),
        HeaderLineKind::Onboarding => dim(ONBOARDING_TEXT),
        HeaderLineKind::ExpandedKeys => expanded_key_hints(kb)
            .iter()
            .map(hint)
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// The header line sequence (upstream collapsed/expanded shapes).
pub fn startup_header_kinds(expanded: bool) -> Vec<HeaderLineKind> {
    if expanded {
        vec![
            HeaderLineKind::Blank,
            HeaderLineKind::Logo,
            HeaderLineKind::Compact,
            HeaderLineKind::Blank,
            HeaderLineKind::ExpandedKeys,
            HeaderLineKind::Blank,
            HeaderLineKind::Onboarding,
            HeaderLineKind::Blank,
        ]
    } else {
        vec![
            HeaderLineKind::Blank,
            HeaderLineKind::Logo,
            HeaderLineKind::Compact,
            HeaderLineKind::Blank,
            HeaderLineKind::Onboarding,
            HeaderLineKind::Blank,
        ]
    }
}

/// Process-wide work counters for [`ChatContainer::set_entries`] (W11-C
/// F2/F5 pins). Mirrors `messages::ToolMemoWork`: relaxed atomics, snapshot +
/// reset are test/probe seams, and the values are informationally useful
/// (they count work that was actually done, never intent).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EntryDiffWork {
    /// `set_entries` calls.
    pub frames: u64,
    /// Entries whose children were rebuilt.
    pub entries_rebuilt: u64,
    /// Components constructed for those entries.
    pub children_rebuilt: u64,
    /// Tool / live-stream entries updated in place (no component rebuilt).
    pub entries_patched: u64,
    /// Entries kept untouched from both the committed prefix and suffix.
    pub entries_retained: u64,
    /// Frames that reused a non-empty committed suffix (two-sided diff).
    pub suffix_reused: u64,
}

struct EntryDiffCounters {
    frames: AtomicU64,
    entries_rebuilt: AtomicU64,
    children_rebuilt: AtomicU64,
    entries_patched: AtomicU64,
    entries_retained: AtomicU64,
    suffix_reused: AtomicU64,
}

static ENTRY_DIFF_WORK: EntryDiffCounters = EntryDiffCounters {
    frames: AtomicU64::new(0),
    entries_rebuilt: AtomicU64::new(0),
    children_rebuilt: AtomicU64::new(0),
    entries_patched: AtomicU64::new(0),
    entries_retained: AtomicU64::new(0),
    suffix_reused: AtomicU64::new(0),
};

impl EntryDiffWork {
    /// The process-wide counters as of now (tests / probes).
    #[doc(hidden)]
    pub fn snapshot() -> Self {
        Self {
            frames: ENTRY_DIFF_WORK.frames.load(Ordering::Relaxed),
            entries_rebuilt: ENTRY_DIFF_WORK.entries_rebuilt.load(Ordering::Relaxed),
            children_rebuilt: ENTRY_DIFF_WORK.children_rebuilt.load(Ordering::Relaxed),
            entries_patched: ENTRY_DIFF_WORK.entries_patched.load(Ordering::Relaxed),
            entries_retained: ENTRY_DIFF_WORK.entries_retained.load(Ordering::Relaxed),
            suffix_reused: ENTRY_DIFF_WORK.suffix_reused.load(Ordering::Relaxed),
        }
    }

    /// Zero every counter (test isolation).
    #[doc(hidden)]
    pub fn reset() {
        ENTRY_DIFF_WORK.frames.store(0, Ordering::Relaxed);
        ENTRY_DIFF_WORK.entries_rebuilt.store(0, Ordering::Relaxed);
        ENTRY_DIFF_WORK.children_rebuilt.store(0, Ordering::Relaxed);
        ENTRY_DIFF_WORK.entries_patched.store(0, Ordering::Relaxed);
        ENTRY_DIFF_WORK.entries_retained.store(0, Ordering::Relaxed);
        ENTRY_DIFF_WORK.suffix_reused.store(0, Ordering::Relaxed);
    }
}

/// Chat transcript: header + messages + tool executions
/// (upstream `chatContainer`).
#[derive(Default)]
pub struct ChatContainer {
    inner: Container,
    /// `settings.hideThinkingBlock`: assistant transcript entries collapse
    /// their thinking blocks (upstream `hideThinkingBlock` forwarded to every
    /// `AssistantMessageComponent`).
    hide_thinking: bool,
    /// Shared live hidden-thinking label (`ctx.ui.setHiddenThinkingLabel`,
    /// upstream `hiddenThinkingLabel`): handed to every
    /// `AssistantMessageComponent` so a relabel reaches already-rendered
    /// entries in place. `None` = the default "Thinking...".
    hidden_thinking_label: Arc<Mutex<Option<String>>>,
    /// App-level Ctrl+O expansion (upstream `toolOutputExpanded`, propagated
    /// to every `ToolExecutionComponent.setExpanded`); shared with the blocks
    /// so a toggle restyles them on the next frame without a rebuild.
    tools_expanded: Arc<AtomicBool>,
    /// The entries `inner`'s children were built from, one record per entry in
    /// order: the diff [`ChatContainer::set_entries`] runs.
    committed: Vec<CommittedEntry>,
    /// The environment the committed children were built under; `None` (fresh
    /// container) rebuilds everything.
    committed_epoch: Option<CommitEpoch>,
    /// What the LAST [`ChatContainer::set_entries`] call did (W11-C F2/F5
    /// test seam; see [`ContainerDiff`]).
    last_diff: ContainerDiff,
}

/// Per-container report of the last [`ChatContainer::set_entries`] diff
/// (W11-C F2 two-sided retention + F5 in-place patching). Counters are
/// per-container, so a test can assert on its own transcript while sibling
/// tests run in parallel (the process-wide [`EntryDiffWork`] counters cannot).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContainerDiff {
    /// Entries kept untouched at the FRONT (the matching prefix).
    pub prefix_kept: usize,
    /// Entries kept untouched at the END (the matching suffix — F2).
    pub suffix_reused: usize,
    /// Entries in the changed middle band rebuilt from scratch.
    pub entries_rebuilt: usize,
    /// Entries in the changed middle band patched in place (F5).
    pub entries_patched: usize,
}

/// One chat entry (message or tool block) for diff-friendly rebuilds.
#[derive(Debug, Clone)]
pub enum ChatEntry {
    User(String),
    /// Assistant payloads are `Arc`-shared with the component tree so
    /// rebuilding the tree (per frame while streaming) never deep-copies a
    /// transcript-sized message.
    Assistant(Arc<AssistantMessage>),
    ToolExecution {
        /// Provider tool-call id (upstream `toolCallId`); `None` for `!`
        /// bash, which has no assistant tool call behind it.
        call_id: Option<String>,
        tool_name: String,
        args: Arc<serde_json::Value>,
        output: Arc<str>,
        is_error: bool,
        /// Monotonic `durationMs` from `tool_execution_end` (drives the bash
        /// `Took Xs` tail, renderers/bash.ts:104-106).
        duration_ms: Option<u64>,
        /// Still running (upstream `isPartial`): the block renders on
        /// `toolPendingBg` with the call line only.
        pending: bool,
        /// Final result `details` (truncation/fullOutputPath/diff): drives
        /// the truncation warnings and the edit diff body (upstream renderers
        /// read `result.details`).
        details: Option<Arc<serde_json::Value>>,
        /// Wall clock when the pending block was created (upstream shell
        /// renderers' `startedAt`): drives the live `Elapsed` tail.
        started_at: Option<std::time::Instant>,
        /// Edit live preview (upstream `computeEditsDiff` on complete args).
        preview: Option<ToolPreview>,
        /// Result image content blocks (upstream `ToolExecutionComponent`'s
        /// `imageBlocks`): rendered after the tool box through the shared
        /// Kitty/iTerm2 path.
        images: Arc<Vec<crate::ai::types::ImageContent>>,
    },
    /// Errors / warnings / statuses appended to the chat (upstream
    /// `showError`/`showWarning`).
    Notice {
        kind: NoticeKind,
        text: String,
    },
    /// Pre-rendered raw ANSI lines (e.g. image previews); appended verbatim.
    /// Tool renderer output carries the provider call id so later lifecycle
    /// events still find and update the same block instead of appending a
    /// duplicate, plus the [`RawRender`] provenance needed to re-render the
    /// cached lines when a render input moves (width / theme / expansion).
    /// `render` is `None` for lines that are not renderer output (image
    /// previews, the custom-entry JSON fallback): those are not re-rendered.
    Raw {
        call_id: Option<String>,
        lines: Vec<String>,
        render: Option<RawRender>,
    },
    /// Live-streaming assistant entry (upstream creates the assistant entry on
    /// message_start and updates it per delta); replaced by the final
    /// [`ChatEntry::Assistant`] when the turn ends.
    Streaming {
        /// True while this is the active stream; `AssistantDone` clears the
        /// flag on every segment it does not replace with the final message, so
        /// no stale segment keeps live-update handles or streaming state.
        live: bool,
        /// Accumulated thinking of the in-flight message (upstream's live
        /// assistant component renders the thinking block as it streams); a
        /// thinking-only reply shows the provider's thinking text while it
        /// arrives instead of a silent screen until `message_done`.
        thinking: String,
        /// Accumulated assistant text.
        text: String,
    },
}

/// The render inputs one [`ChatEntry::Raw`] block's cached lines were built
/// from, so a frame where one of them moved can RE-RENDER the block instead of
/// keeping output rendered for a stale width / palette / expansion posture.
///
/// The lines are whole ANSI strings produced by a tool renderer (JS or native)
/// on the TUI thread; nothing on the render path can rebuild them, so the
/// provenance travels with the entry. `theme_generation` and `width` are read
/// once per frame at the render site (exactly like every other memo key in this
/// module); `expanded` is the Ctrl+O posture the renderer request carried.
///
/// `payload` is the renderer request's call data (`toolName` / `args` /
/// `output` / `isError` for the JS face; the native face rebuilds its own from
/// the same three fields) — `Arc`-shared with the [`ChatEntry::ToolExecution`]
/// the render replaced, so carrying it costs one refcount bump and a re-render
/// never deep-copies the tool output.
#[derive(Debug, Clone)]
pub struct RawToolRender {
    /// Provider tool-call id, when the block came from one (upstream
    /// `toolCallId`); `None` for renderer output with no call behind it.
    pub call_id: Option<String>,
    pub tool_name: String,
    pub args: Arc<serde_json::Value>,
    pub output: Arc<str>,
    pub is_error: bool,
    pub details: Option<Arc<serde_json::Value>>,
    pub expanded: bool,
    pub width: usize,
    pub theme_generation: u64,
    /// Upstream `ToolDefinition.renderShell` (types.ts:595) when the
    /// registration declared one. `None` keeps the historical bare-line
    /// painting; `Some(Default)` paints the lines inside the standard colored
    /// tool shell.
    pub shell: Option<super::renderers::ToolRenderShell>,
    /// Upstream `isPartial` at render time: a partial default-shell block
    /// paints `toolPendingBg`, a settled error `toolErrorBg`, a settled
    /// success `toolSuccessBg`. Flipped to `false` in place by
    /// `tool_execution_end`.
    pub pending: bool,
    /// Result image content blocks the renderer output does not include:
    /// the tool-execution shell appends them after its own lines (upstream
    /// `imageComponents` outside the content box).
    pub images: Arc<Vec<crate::ai::types::ImageContent>>,
    /// The pre-rendered image rows for `width` (a leading spacer + the
    /// protocol lines per image), rebuilt with the block when a render input
    /// moves.
    pub image_lines: Arc<Vec<String>>,
}

impl RawToolRender {
    /// Whether the cached lines still answer a request rendered under
    /// `width` / `theme_generation` / `expanded`. A width, palette or
    /// expansion move is a miss (the renderer would return different bytes);
    /// the call data cannot move without the entry being rebuilt, so it is
    /// deliberately not part of the comparison.
    pub fn answers(&self, width: usize, theme_generation: u64, expanded: bool) -> bool {
        self.width == width
            && self.theme_generation == theme_generation
            && self.expanded == expanded
    }
}

/// Provenance of a custom message/entry renderer's cached lines
/// ([`ChatEntry::Raw`] from `registerMessageRenderer` /
/// `registerEntryRenderer`): the payload and lookup key needed to re-run the
/// renderer when a render input moves.
#[derive(Debug, Clone)]
pub struct RawCustomRender {
    /// The original `CustomMessageEntry` / `CustomEntry` payload.
    pub payload: Arc<serde_json::Value>,
    /// The renderer lookup key (`customType`).
    pub custom_type: String,
    /// `sendMessage` (message renderers) vs `appendEntry` (entry renderers).
    pub is_message: bool,
    pub expanded: bool,
    pub width: usize,
    pub theme_generation: u64,
}

/// Provenance of one pre-rendered renderer block: enough to answer "were these
/// lines built for this frame's width / palette / expansion?" and to re-run the
/// renderer when the answer is no.
#[derive(Debug, Clone)]
pub enum RawRender {
    Tool(RawToolRender),
    Custom(RawCustomRender),
}

impl RawRender {
    /// Whether the cached lines still answer a request rendered under
    /// `width` / `theme_generation` / `expanded`.
    pub fn answers(&self, width: usize, theme_generation: u64, expanded: bool) -> bool {
        match self {
            RawRender::Tool(render) => render.answers(width, theme_generation, expanded),
            RawRender::Custom(render) => {
                render.width == width
                    && render.theme_generation == theme_generation
                    && render.expanded == expanded
            }
        }
    }

    /// Cheap identity for the retained-child diff: the call id buffer (or the
    /// payload allocation for a custom renderer), plus the render inputs. The
    /// trailing tool-only pair is `(pending, default_shell)`: an in-place
    /// phase flip or a shell change must rebuild the block's paint posture
    /// even though its cached lines did not move.
    pub fn tag(&self) -> (usize, bool, usize, u64, bool, bool) {
        match self {
            RawRender::Tool(render) => (
                render.call_id.as_ref().map_or(0, |id| id.as_ptr() as usize),
                render.expanded,
                render.width,
                render.theme_generation,
                render.pending,
                render.shell == Some(super::renderers::ToolRenderShell::Default),
            ),
            RawRender::Custom(render) => (
                Arc::as_ptr(&render.payload) as usize,
                render.expanded,
                render.width,
                render.theme_generation,
                false,
                false,
            ),
        }
    }
}

/// Re-render every renderer-produced [`ChatEntry::Raw`] block whose cached
/// lines no longer answer `(width, theme_generation, expanded)`.
///
/// This is the pure invalidation walk behind the interactive loop's
/// stale-output pass (the loop itself lives in `terminal.rs`, which also owns
/// the epoch guard and the renderer-failure notices): each stale block's
/// provenance is handed to `render` EXACTLY ONCE, in transcript order, and
/// the returned entry replaces it at the same index — so a tool block keeps
/// its `call_id` and its position (a later `locate`/`finish`/`update` still
/// finds it), and a custom block keeps its payload. A block that answers the
/// current key is never passed to `render`, so a steady frame performs only
/// the provenance compares (no renderer call, no allocation).
///
/// Returns the number of blocks rebuilt (`0` when no render input moved).
/// Verbatim blocks (image previews, the custom-entry JSON fallback) carry no
/// provenance and are never touched: there is nothing to re-render them
/// from.
pub fn rerender_stale_raw(
    entries: &mut [ChatEntry],
    width: usize,
    theme_generation: u64,
    expanded: bool,
    mut render: impl FnMut(&RawRender) -> ChatEntry,
) -> usize {
    let mut rebuilt = 0;
    for entry in entries.iter_mut() {
        let ChatEntry::Raw {
            render: Some(provenance),
            ..
        } = entry
        else {
            continue;
        };
        if provenance.answers(width, theme_generation, expanded) {
            continue;
        }
        let replacement = render(provenance);
        *entry = replacement;
        rebuilt += 1;
    }
    rebuilt
}

/// Edit live preview state (upstream `EditCallRenderComponent.preview` +
/// `previewArgsKey`): computed off-thread from complete call args before the
/// tool result arrives; the box shows the diff and switches to the
/// success/error background as soon as it resolves.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolPreview {
    /// `JSON({path, edits})` of the args the preview was computed for; a
    /// mismatch with the block's current args invalidates the preview
    /// (upstream `previewArgsKey`).
    pub args_key: String,
    /// Resolved display diff (`details.diff` shape) when the preview applied
    /// cleanly.
    pub diff: Option<String>,
    /// First changed line in the new file (upstream `firstChangedLine`).
    pub first_changed_line: Option<usize>,
    /// Error text when the edit cannot apply (upstream `EditDiffError`).
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Error,
    Warning,
    Status,
}

/// Cheap identity of one owned text buffer: the address, length and capacity
/// of the allocation behind it, with no allocation and no scan.
///
/// For this codebase's mutation set the triple is exact — an entry's owned
/// text is either replaced together with the whole entry (`terminal.rs` writes
/// the entry value: `entries[index] = …`), in which case the fresh allocation
/// is built while the old one is still alive and cannot reuse its address, or
/// appended to in place (`entries[index] … entry_text.push_str(&text)`, the
/// live-stream accumulator at terminal.rs:2902, which always moves `len` —
/// `realloc`-in-place moves `cap`). Nothing ever rewrites an entry's text in
/// place at the same length. A tag mismatch only ever costs a rebuild, so the
/// unsafe direction stays covered.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TextTag {
    ptr: usize,
    len: usize,
    cap: usize,
}

impl TextTag {
    fn of_string(text: &String) -> Self {
        Self {
            ptr: text.as_ptr() as usize,
            len: text.len(),
            cap: text.capacity(),
        }
    }

    fn of_arc_str(text: &Arc<str>) -> Self {
        Self {
            ptr: text.as_ptr() as usize,
            len: text.len(),
            cap: 0,
        }
    }

    fn of_optional(text: &Option<String>) -> Option<Self> {
        text.as_ref().map(Self::of_string)
    }
}

/// The bytes `new` gained over the buffer `old` identifies, when the tag still
/// describes the SAME allocation grown in place (the live accumulator's
/// append-only contract). `None` means the tag cannot prove an append (a
/// replacement, a reallocation, or a different buffer): the caller falls back
/// to the snapshot path. An equal-length tag yields an empty delta, which the
/// component treats as an idle re-feed.
fn appended_text(old: TextTag, new: &str) -> Option<&str> {
    if new.len() >= old.len && new.as_ptr() as usize == old.ptr && new.is_char_boundary(old.len) {
        Some(&new[old.len..])
    } else {
        None
    }
}

/// [`ToolPreview`] identity for the retained-child diff: the preview is only
/// ever assigned wholesale (`*preview = Some(ToolPreview { … })`), so its text
/// buffers behave exactly like the entry-owned ones (see [`TextTag`]).
#[derive(Debug, Clone, Copy, PartialEq)]
struct PreviewTag {
    args_key: TextTag,
    diff: Option<TextTag>,
    first_changed_line: Option<usize>,
    error: Option<TextTag>,
}

impl PreviewTag {
    fn of(preview: &ToolPreview) -> Self {
        Self {
            args_key: TextTag::of_string(&preview.args_key),
            diff: preview.diff.as_ref().map(TextTag::of_string),
            first_changed_line: preview.first_changed_line,
            error: preview.error.as_ref().map(TextTag::of_string),
        }
    }
}

/// The render inputs of one tool block, all of them cheap to compare: shared
/// payloads by `Arc` pointer (`Value`/`str` behind an `Arc` are never mutated
/// in place), owned text by buffer ([`TextTag`]) and the scalars by value —
/// including the fields `terminal.rs` flips in place on a pending block
/// (`is_error`, `duration_ms`, `pending`).
#[derive(Debug, Clone, Copy, PartialEq)]
struct ToolTag {
    call_id: Option<TextTag>,
    tool_name: TextTag,
    args: usize,
    output: TextTag,
    details: usize,
    is_error: bool,
    duration_ms: Option<u64>,
    pending: bool,
    started_at: Option<Instant>,
    preview: Option<PreviewTag>,
    /// Result image payload identity (never mutated in place behind the
    /// `Arc`), so a late image block lands on the retained block.
    images: usize,
}

// NOTE: `Tool` is the large variant (every rendering input of a tool block).
// Boxing it would cost `Copy` and one allocation per entry on every
// `set_entries` diff — the hot path this tag exists for — so the size
// difference is kept (same trade as `ai/types/messages.rs`'s enum).
#[allow(clippy::large_enum_variant)]
/// Identity of one [`ChatEntry`] for the retained-child diff
/// ([`ChatContainer::set_entries`]): every field that shapes the entry's
/// children, in a form that costs nothing to build and compare.
#[derive(Debug, Clone, Copy, PartialEq)]
enum EntryTag {
    User(TextTag),
    Assistant(usize),
    Tool(ToolTag),
    Notice {
        kind: NoticeKind,
        text: TextTag,
    },
    Raw {
        buffer: usize,
        lines: usize,
        /// Renderer-output provenance (see [`RawRender::tag`]): absent for
        /// verbatim blocks (image previews, the custom-entry JSON fallback)
        /// that are never re-rendered.
        render: Option<(usize, bool, usize, u64, bool, bool)>,
    },
    /// Both accumulators of a live entry: either growing moves the tag, so the
    /// retained children are re-fed the new text in place. `live` forces a
    /// rebuild when a segment is finalized in place.
    Streaming {
        live: bool,
        thinking: TextTag,
        text: TextTag,
    },
}

impl EntryTag {
    fn of(entry: &ChatEntry) -> Self {
        match entry {
            ChatEntry::User(text) => EntryTag::User(TextTag::of_string(text)),
            ChatEntry::Assistant(message) => EntryTag::Assistant(Arc::as_ptr(message) as usize),
            ChatEntry::ToolExecution {
                call_id,
                tool_name,
                args,
                output,
                is_error,
                duration_ms,
                pending,
                details,
                started_at,
                preview,
                images,
                ..
            } => EntryTag::Tool(ToolTag {
                call_id: TextTag::of_optional(call_id),
                tool_name: TextTag::of_string(tool_name),
                args: Arc::as_ptr(args) as usize,
                output: TextTag::of_arc_str(output),
                details: details
                    .as_ref()
                    .map_or(0, |details| Arc::as_ptr(details) as usize),
                is_error: *is_error,
                duration_ms: *duration_ms,
                pending: *pending,
                started_at: *started_at,
                preview: preview.as_ref().map(PreviewTag::of),
                images: Arc::as_ptr(images) as usize,
            }),
            ChatEntry::Notice { kind, text } => EntryTag::Notice {
                kind: *kind,
                text: TextTag::of_string(text),
            },
            // `Raw` lines are immutable once committed (image / extension
            // previews) and a renderer replacement allocates a fresh vector
            // while the old one is still alive, so buffer + length identify
            // the rendered content exactly. Renderer-output blocks also move
            // when a render input does (width / palette / expansion): the tag
            // follows the provenance so the diff rebuilds the block instead of
            // serving lines rendered for the old inputs.
            ChatEntry::Raw { lines, render, .. } => EntryTag::Raw {
                buffer: lines.as_ptr() as usize,
                lines: lines.len(),
                render: render.as_ref().map(RawRender::tag),
            },
            ChatEntry::Streaming {
                live,
                thinking,
                text,
            } => EntryTag::Streaming {
                live: *live,
                thinking: TextTag::of_string(thinking),
                text: TextTag::of_string(text),
            },
        }
    }
}

/// One committed transcript entry: the tag its children were built from, how
/// many children it contributed (so a later diff drops exactly its share), and
/// — for a live stream — the retained component handles whose text is updated
/// in place while the reply grows.
struct CommittedEntry {
    tag: EntryTag,
    children: usize,
    streaming: Option<CommittedStreaming>,
    /// W11-C F5: the retained tool block, so a result/metadata update patches
    /// the existing component instead of rebuilding it (the block's memo
    /// caches — call rows, result rows, timer tail — survive the update).
    tool: Option<Arc<ToolExecutionComponent>>,
    /// The `pending` phase the retained tool block was committed in. The
    /// component's setters can only move a block FORWARD out of pending
    /// (`update_result` always clears the flag), so a patch is only provably
    /// identical to a rebuild when both the committed and the new phase are
    /// settled; a block entering or leaving pending rebuilds instead
    /// (`set_entries` F5 gate).
    pending: bool,
    /// Whether any [`ChatEntry::User`] preceded this entry when it was
    /// committed. The spacer before a user message depends on that flag, so
    /// the two-sided diff may only reuse a suffix whose flag still matches
    /// the new entry list's prefix state.
    user_before: bool,
}

/// The retained children of a live [`ChatEntry::Streaming`] entry: the body
/// markdown and (once the provider has emitted thinking) the thinking block,
/// both updated in place per delta.
struct CommittedStreaming {
    thinking: Option<Arc<StreamingThinking>>,
    markdown: Arc<Markdown>,
}

impl Clone for CommittedStreaming {
    fn clone(&self) -> Self {
        Self {
            thinking: self.thinking.clone(),
            markdown: Arc::clone(&self.markdown),
        }
    }
}

/// The environment the committed children were built under. Every component
/// bakes at least one of these at construction (the theme palette into
/// `Notice`'s `Text`, `hideThinkingBlock` into every assistant component, the
/// expansion slot into every tool block), so a mismatch rebuilds the whole
/// transcript — exactly the pre-reuse behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CommitEpoch {
    theme_generation: u64,
    hide_thinking: bool,
    tools_expanded: usize,
    hidden_label: usize,
}

/// A child that renders a *shared* [`Markdown`], so [`ChatContainer`] can keep
/// a handle to the live stream's component (added children are boxed behind
/// `dyn Component`, so the handle is the only way back to it) and update its
/// text through [`Markdown::set_text_growing`] instead of building a fresh
/// component per delta.
///
/// The override forwards to [`Markdown`]'s own canvas path, so the live body
/// writes its cached rows straight into the frame's recycled buffers (a body
/// that did not change allocates no row).
struct SharedMarkdown(Arc<Markdown>);

impl Component for SharedMarkdown {
    fn render(&self, width: usize) -> Vec<String> {
        self.0.render(width)
    }

    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        self.0.render_into(width, out);
    }

    fn invalidate(&self) {
        self.0.invalidate();
    }
}

/// One full-width row carrying a background painter
/// (`theme.bg(color, &" ".repeat(width))`). Used for the `renderShell:
/// "default"` tool shell's `Box` padding rows and for blank renderer rows:
/// [`Text`] deliberately drops whitespace-only bodies, but a shell row must
/// still paint.
struct PaintedBlank(Arc<dyn Fn(&str) -> String + Send + Sync>);

impl Component for PaintedBlank {
    fn render(&self, width: usize) -> Vec<String> {
        vec![(self.0)(&" ".repeat(width))]
    }

    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        out.push("");
        let painted = (self.0)(&" ".repeat(width));
        out.line_mut().push_str(&painted);
    }
}

/// A child that renders a *shared* [`StreamingThinking`] (the live entry's
/// thinking block): the same handle discipline and canvas forwarding as
/// [`SharedMarkdown`].
struct SharedThinking(Arc<StreamingThinking>);

impl Component for SharedThinking {
    fn render(&self, width: usize) -> Vec<String> {
        self.0.render(width)
    }

    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        self.0.render_into(width, out);
    }

    fn invalidate(&self) {
        self.0.invalidate();
    }
}

/// A child that renders a *shared* [`ToolExecutionComponent`] (W11-C F5): the
/// container owns the wrapper while [`ChatContainer`] keeps the `Arc`, so a
/// later entry update reaches the live block (`apply_entry`) and patches it in
/// place. Rendering forwards child-for-child, so the bytes are identical to a
/// directly boxed component.
struct SharedTool(Arc<ToolExecutionComponent>);

impl Component for SharedTool {
    fn render(&self, width: usize) -> Vec<String> {
        self.0.render(width)
    }

    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        self.0.render_into(width, out);
    }

    fn invalidate(&self) {
        self.0.invalidate();
    }
}

impl ChatContainer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `hideThinkingBlock` (upstream settings → assistant components).
    /// Rebuilds the transcript so existing entries pick the new posture up.
    pub fn set_hide_thinking(&mut self, hide_thinking: bool) {
        if self.hide_thinking == hide_thinking {
            return;
        }
        self.hide_thinking = hide_thinking;
        self.inner.invalidate();
    }

    /// Apply `ctx.ui.setHiddenThinkingLabel(label?)` (upstream
    /// interactive-mode `setHiddenThinkingLabel`, types.ts:185): the shared
    /// slot every assistant component reads, so the relabel is live for
    /// already-rendered entries too; `None` restores the default.
    pub fn set_hidden_thinking_label(&mut self, label: Option<String>) {
        {
            let mut current = self
                .hidden_thinking_label
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *current = label;
        }
        self.inner.invalidate();
    }

    /// Share the app-level Ctrl+O expansion flag with every builtin tool
    /// block (upstream `app.tools.expand` → `setToolsExpanded`,
    /// interactive-mode.ts:4557-4575).
    pub fn set_tools_expanded(&mut self, tools_expanded: Arc<AtomicBool>) {
        self.tools_expanded = tools_expanded;
    }

    /// Replace the transcript with `entries`, rebuilding only what changed.
    ///
    /// Every entry whose cheap [`EntryTag`] still matches the tag it was
    /// committed with keeps its component — and that component's own render
    /// caches (`AssistantMessageComponent`'s per-segment cache, the markdown
    /// caches inside user messages and the live stream) — exactly where it is.
    ///
    /// W11-C F2: the diff is TWO-SIDED. A matching prefix AND a matching
    /// suffix are retained, and only the changed middle band is re-committed,
    /// so a result landing on an old tool block no longer rebuilds every
    /// entry after it (the pre-F2 longest-prefix diff rebuilt the whole
    /// tail). W11-C F5: a changed tool block whose identity (call id, tool
    /// name, args allocation) is unchanged is patched IN PLACE through
    /// `ToolExecutionComponent::apply_entry`; the component and every memo
    /// cache on it survive, and only the moved section re-renders. A live
    /// streaming entry inside the changed band is likewise updated in place
    /// (the same handles contract as before).
    ///
    /// The child sequence stays byte-for-byte what a full rebuild produced,
    /// so rendered lines are unchanged. A frame that changes nothing (the
    /// ~60 fps working-spinner tick) costs one tag comparison per entry and
    /// constructs nothing; [`EntryDiffWork`] counts the work that was done.
    pub fn set_entries(&mut self, entries: &[ChatEntry]) {
        ENTRY_DIFF_WORK.frames.fetch_add(1, Ordering::Relaxed);
        let epoch = CommitEpoch {
            theme_generation: Theme::generation(),
            hide_thinking: self.hide_thinking,
            tools_expanded: Arc::as_ptr(&self.tools_expanded) as usize,
            hidden_label: Arc::as_ptr(&self.hidden_thinking_label) as usize,
        };
        let reusable = self.committed_epoch == Some(epoch);
        let old_len = self.committed.len();
        // Where the first user message lives: the spacer before every user
        // message but the first depends only on this position, which lets the
        // suffix scan (and the rebuild) decide it in O(1) per entry.
        let first_user = entries
            .iter()
            .position(|entry| matches!(entry, ChatEntry::User(_)));
        // Longest prefix of entries the committed children can serve as-is.
        let mut keep = 0usize;
        if reusable {
            while keep < entries.len()
                && keep < old_len
                && self.committed[keep].tag == EntryTag::of(&entries[keep])
            {
                keep += 1;
            }
            // The live stream's accumulated text (or thinking) grew: same
            // variant at the same slot (the tag could only differ in the
            // accumulators), so the retained children take the new text in
            // place — the components, their caches and their position stay put,
            // where a rebuild constructed fresh ones and re-cloned the whole
            // text per delta.
            if keep < entries.len()
                && self.patch_streaming_entry(
                    &entries[keep],
                    keep,
                    first_user.is_some_and(|first| first < keep),
                )
            {
                keep += 1;
            }
        }
        // Longest common suffix. A suffix entry is reusable when its tag
        // still matches AND its "user message before me" state is unchanged
        // (that state decides whether the user entry carries a spacer).
        let mut back = 0usize;
        if reusable {
            let max = entries
                .len()
                .saturating_sub(keep)
                .min(old_len.saturating_sub(keep));
            while back < max {
                let new_index = entries.len() - 1 - back;
                let old_index = old_len - 1 - back;
                if self.committed[old_index].tag != EntryTag::of(&entries[new_index]) {
                    break;
                }
                if self.committed[old_index].user_before
                    != first_user.is_some_and(|first| first < new_index)
                {
                    break;
                }
                back += 1;
            }
        }
        if back > 0 {
            ENTRY_DIFF_WORK
                .suffix_reused
                .fetch_add(1, Ordering::Relaxed);
        }
        let new_middle_end = entries.len() - back;
        let old_middle_end = old_len - back;
        let front_children: usize = self.committed[..keep]
            .iter()
            .map(|entry| entry.children)
            .sum();
        let old_middle_children: usize = self.committed[keep..old_middle_end]
            .iter()
            .map(|entry| entry.children)
            .sum();
        let retained = keep + back;
        if retained > 0 {
            ENTRY_DIFF_WORK
                .entries_retained
                .fetch_add(retained as u64, Ordering::Relaxed);
        }
        // Rebuild (or patch) the changed middle band. Every splice lands at
        // `cursor`; retained children after the band shift as a whole, so the
        // suffix keeps its components and caches.
        let mut cursor = front_children;
        let mut old_consumed = 0usize;
        let mut user_before = first_user.is_some_and(|first| first < keep);
        let mut rebuilt_records: Vec<CommittedEntry> =
            Vec::with_capacity(new_middle_end.saturating_sub(keep));
        let mut diff = ContainerDiff {
            prefix_kept: keep,
            suffix_reused: back,
            ..ContainerDiff::default()
        };
        for (offset, entry) in entries[keep..new_middle_end].iter().enumerate() {
            let index = keep + offset;
            // Every old record in the band contributes children that the new
            // build replaces, whether or not its entry can be patched.
            let old_children = if index < old_middle_end {
                self.committed[index].children
            } else {
                0
            };
            // A patch only ever addresses the OLD middle band: past
            // `old_middle_end` the slot `index` holds a retained SUFFIX record
            // (a middle that grew), whose children sit after the band and must
            // never be patched from a new entry — that would overwrite an
            // unrelated block and desync the child cursor from the records.
            let patchable = reusable && index < old_middle_end;
            if patchable && self.patch_middle_entry(entry, index, user_before) {
                cursor += self.committed[index].children;
                old_consumed += old_children;
                diff.entries_patched += 1;
                rebuilt_records.push(CommittedEntry {
                    tag: self.committed[index].tag,
                    children: self.committed[index].children,
                    streaming: self.committed[index].streaming.clone(),
                    tool: self.committed[index].tool.clone(),
                    pending: self.committed[index].pending,
                    user_before: self.committed[index].user_before,
                });
            } else {
                let (record, children) = self.build_entry(entry, user_before);
                let children_built = children.len();
                self.inner.splice_children(cursor, old_children, children);
                cursor += children_built;
                old_consumed += old_children;
                diff.entries_rebuilt += 1;
                ENTRY_DIFF_WORK
                    .entries_rebuilt
                    .fetch_add(1, Ordering::Relaxed);
                ENTRY_DIFF_WORK
                    .children_rebuilt
                    .fetch_add(children_built as u64, Ordering::Relaxed);
                rebuilt_records.push(record);
            }
            user_before = user_before || matches!(entry, ChatEntry::User(_));
        }
        // A shorter middle band leaves the surplus old children in place:
        // drop them (they sit right after the freshly built ones).
        let leftover = old_middle_children.saturating_sub(old_consumed);
        if leftover > 0 {
            self.inner.splice_children(cursor, leftover, Vec::new());
        }
        self.committed.splice(keep..old_middle_end, rebuilt_records);
        self.committed_epoch = Some(epoch);
        self.last_diff = diff;
        // Transcript reset (`/clear`, `/new`, `/resume`, fork): the diff above
        // dropped every child and every commit record, but both vectors keep
        // their per-session slot array. Invariant: this only runs on an EMPTY
        // transcript — the reset is the one moment the retained capacity is
        // known-dead, and the next session's entries would otherwise hold the
        // outgoing session's slot memory for the rest of the process (the
        // entry payloads themselves are already freed by the drops above).
        // A non-empty update never shrinks: it is the per-frame path.
        if entries.is_empty() {
            self.committed.shrink_to_fit();
            self.inner.shrink_children();
        }
    }

    /// The live-stream in-place patch (the original `set_entries` boundary
    /// rule): the entry at `index` is a live stream whose accumulators grew,
    /// so the retained markdown/thinking components take the new text and no
    /// child moves. `expected_user_before` is the spacer state the retained
    /// children were built under.
    fn patch_streaming_entry(
        &mut self,
        entry: &ChatEntry,
        index: usize,
        expected_user_before: bool,
    ) -> bool {
        let ChatEntry::Streaming {
            live: true,
            thinking,
            text,
        } = entry
        else {
            return false;
        };
        let Some(committed) = self.committed.get(index) else {
            return false;
        };
        // The thinking child only exists when the entry was committed with
        // thinking text; a later first thinking delta (a provider that
        // reported text first) must rebuild the entry instead of silently
        // dropping it into a missing child.
        let Some(streaming) = &committed.streaming else {
            return false;
        };
        if !(streaming.thinking.is_some() || thinking.is_empty()) {
            return false;
        }
        if committed.user_before != expected_user_before {
            return false;
        }
        // The tags the committed children were fed: the live entry only
        // appends to its accumulators in place, so the same allocation's
        // grown length gives the delta without comparing or copying the
        // accumulated body. A tag mismatch falls back to the snapshot path.
        let (old_thinking, old_text) = match committed.tag {
            EntryTag::Streaming { thinking, text, .. } => (thinking, text),
            _ => return false,
        };
        let markdown = Arc::clone(&streaming.markdown);
        let thinking_block = streaming.thinking.clone();
        // The entry owns the accumulators and only ever grows them in place,
        // so each component extends its own copy by the delta alone: no
        // full-text copy per frame. The render then reuses the lines of the
        // text's final prefix and re-parses only the mutable tail.
        match appended_text(old_text, text) {
            Some(delta) => markdown.set_text_growing_delta(delta),
            None => markdown.set_text_growing(text),
        }
        if let Some(thinking_block) = &thinking_block {
            thinking_block.set_text_growing(thinking);
        }
        debug_assert!(
            old_thinking.len <= thinking.len(),
            "a live streaming entry only grows its thinking accumulator"
        );
        self.committed[index].tag = EntryTag::of(entry);
        ENTRY_DIFF_WORK
            .entries_patched
            .fetch_add(1, Ordering::Relaxed);
        true
    }

    /// O(1) live-stream fast path: patch the final committed `Streaming`
    /// component with its grown text/thinking without re-diffing the transcript
    /// prefix. Returns false when the last entry is not a live stream or the
    /// component is not committed yet (the caller falls back to
    /// [`Self::set_entries`]).
    ///
    /// `window_start` is the first entry index materialized in the container
    /// (`TranscriptWindow::start`; 0 when the whole transcript is materialized).
    /// The committed array indexes the window, so the live tail's committed
    /// slot is `entries.len() - 1 - window_start`; without the origin the
    /// patch always missed once the tail window had scrolled (the full-diff
    /// fallback then ran on every token of a long session).
    pub fn patch_last_streaming(&mut self, entries: &[ChatEntry], window_start: usize) -> bool {
        let Some(entry) = entries.last() else {
            return false;
        };
        let Some(index) = entries.len().checked_sub(1 + window_start) else {
            return false;
        };
        let expected_user_before = self
            .committed
            .get(index)
            .is_some_and(|committed| committed.user_before);
        self.patch_streaming_entry(entry, index, expected_user_before)
    }

    /// W11-C F5: patch a retained tool block in place when only its result
    /// fields moved. Identity is the call id, tool name and args allocation
    /// (`ToolExecutionComponent` bakes those at construction); everything
    /// else the component can apply through `apply_entry`. Returns whether
    /// the patch happened.
    fn patch_middle_entry(&mut self, entry: &ChatEntry, index: usize, user_before: bool) -> bool {
        if index >= self.committed.len() {
            return false;
        }
        if self.committed[index].user_before != user_before {
            return false;
        }
        match entry {
            ChatEntry::ToolExecution {
                call_id,
                tool_name,
                args,
                output,
                is_error,
                duration_ms,
                pending,
                details,
                started_at,
                preview,
                images,
            } => {
                let committed = &self.committed[index];
                let Some(tool) = committed.tool.as_ref().map(Arc::clone) else {
                    return false;
                };
                let EntryTag::Tool(tag) = &committed.tag else {
                    return false;
                };
                if tag.call_id != TextTag::of_optional(call_id)
                    || tag.tool_name != TextTag::of_string(tool_name)
                    || tag.args != Arc::as_ptr(args) as usize
                {
                    return false;
                }
                // A settled→pending flip is NOT patchable: the component's
                // setters only move a block OUT of pending (`update_result`
                // clears the flag unconditionally), so it would keep the
                // settled background. The rebuild path constructs a component
                // in exactly the right phase, so hand that direction back to
                // it. The reverse (a result landing on the pending block — the
                // hot F5 case) and a pending→pending partial update both patch.
                if !committed.pending && *pending {
                    return false;
                }
                // W11-C F5: patch the retained block in place through the
                // component's public setters (`set_meta` + `update_result`),
                // exactly the calls a fresh build makes — so the end state is
                // identical while every memo cache on the component survives.
                // A pending→pending update only carries meta (the call line is
                // the whole block there); `update_result` would leave pending.
                tool.set_meta(details.clone(), *started_at, preview.clone());
                tool.set_images(Arc::clone(images));
                if !*pending {
                    tool.update_result(Arc::clone(output), *is_error, *duration_ms);
                }
                self.committed[index].tag = EntryTag::of(entry);
                ENTRY_DIFF_WORK
                    .entries_patched
                    .fetch_add(1, Ordering::Relaxed);
                true
            }
            ChatEntry::Streaming { .. } => self.patch_streaming_entry(entry, index, user_before),
            _ => false,
        }
    }

    /// Capacity of the commit-record slots (0 on a fresh container). Test seam
    /// for the transcript-reset release in [`ChatContainer::set_entries`].
    #[doc(hidden)]
    pub fn committed_capacity(&self) -> usize {
        self.committed.capacity()
    }

    /// Per-container diff report of the LAST [`ChatContainer::set_entries`]
    /// call (W11-C F2/F5 test seam): which slots the two-sided diff retained,
    /// patched or rebuilt, and how many suffix entries were reused.
    ///
    /// The process-wide [`EntryDiffWork`] counters are shared by every
    /// container in the test binary (and the `g_tui` suites run in parallel),
    /// so a single-container assertion reads THIS snapshot instead.
    #[doc(hidden)]
    pub fn last_diff(&self) -> ContainerDiff {
        self.last_diff
    }

    /// Capacity of the underlying child-container slots (see
    /// [`Container::children_capacity`]). Test seam for the same release.
    #[doc(hidden)]
    pub fn children_capacity(&self) -> usize {
        self.inner.children_capacity()
    }

    /// Build one entry's children — the exact sequence the pre-reuse
    /// `set_entries` produced, spacers included — into `children` and return
    /// the record the next diff compares against. `user_before` is the
    /// "a user message precedes this entry" spacer state.
    fn build_entry(
        &self,
        entry: &ChatEntry,
        user_before: bool,
    ) -> (CommittedEntry, Vec<Box<dyn Component>>) {
        let mut children: Vec<Box<dyn Component>> = Vec::new();
        let mut streaming = None;
        let mut tool = None;
        match entry {
            ChatEntry::User(text) => {
                if user_before {
                    children.push(Box::new(Spacer::new(1)));
                }
                // `/skill:<name>` prompts are stored as a `<skill …>` block
                // (optionally followed by a `\n\n`-separated user message).
                // Upstream renders those as a collapsible
                // `SkillInvocationMessageComponent` and the user text as a
                // SEPARATE user message (`addMessageToChat`'s user case,
                // interactive-mode.ts:3899-3938; `parseSkillBlock`,
                // agent-session.ts:177-186). Everything else keeps the plain
                // user-message component.
                match parse_skill_block(text) {
                    Some(block) => {
                        children.push(Box::new(SkillInvocationComponent::new(
                            &block,
                            1,
                            Arc::clone(&self.tools_expanded),
                        )));
                        if let Some(user_message) = &block.user_message {
                            children.push(Box::new(Spacer::new(1)));
                            children.push(Box::new(UserMessageComponent::new(user_message, 1)));
                        }
                    }
                    None => children.push(Box::new(UserMessageComponent::new(text, 1))),
                }
            }
            ChatEntry::Assistant(message) => {
                children.push(Box::new(
                    AssistantMessageComponent::new_from(Arc::clone(message), self.hide_thinking, 1)
                        .with_hidden_label(Arc::clone(&self.hidden_thinking_label)),
                ));
            }
            ChatEntry::ToolExecution {
                tool_name,
                args,
                output,
                is_error,
                duration_ms,
                pending,
                details,
                started_at,
                preview,
                images,
                ..
            } => {
                let component = Arc::new(ToolExecutionComponent::new(
                    tool_name,
                    Arc::clone(args),
                    Arc::clone(&self.tools_expanded),
                ));
                component.set_meta(details.clone(), *started_at, preview.clone());
                component.set_images(Arc::clone(images));
                if !pending {
                    component.update_result(Arc::clone(output), *is_error, *duration_ms);
                }
                tool = Some(Arc::clone(&component));
                children.push(Box::new(SharedTool(component)));
            }
            ChatEntry::Notice { kind, text } => {
                children.push(Box::new(Spacer::new(1)));
                let line = match kind {
                    NoticeKind::Error => {
                        with_theme(|theme| theme.fg(ThemeColor::Error, &format!("Error: {text}")))
                    }
                    NoticeKind::Warning => with_theme(|theme| {
                        theme.fg(ThemeColor::Warning, &format!("Warning: {text}"))
                    }),
                    NoticeKind::Status => with_theme(|theme| theme.fg(ThemeColor::Dim, text)),
                };
                children.push(Box::new(Text::with_padding(line, 1, 0)));
            }
            ChatEntry::Raw {
                lines,
                render: Some(RawRender::Tool(tool)),
                ..
            } if tool.shell == Some(ToolRenderShell::Default) => {
                // Upstream `ToolExecutionComponent`'s `contentBox`
                // (tool-execution.ts:89-120): with `renderShell: "default"`
                // the tool's rendered component is wrapped in the standard
                // colored shell (Box paddingX = outputPad, paddingY = 1, the
                // pending/success/error background). `renderShell: "self"`
                // (and legacy registrations that declared no shell) keep the
                // bare lines below.
                children.push(Box::new(Spacer::new(1)));
                let color = if tool.pending {
                    ThemeColor::ToolPendingBg
                } else if tool.is_error {
                    ThemeColor::ToolErrorBg
                } else {
                    ThemeColor::ToolSuccessBg
                };
                let painter: Arc<dyn Fn(&str) -> String + Send + Sync> =
                    Arc::new(move |text: &str| with_theme(|theme| theme.bg(color, text)));
                // Box paddingY = 1: one painted blank row above and below.
                children.push(Box::new(PaintedBlank(Arc::clone(&painter))));
                for line in lines {
                    if line.trim().is_empty() {
                        // `Text` drops whitespace-only bodies; an empty
                        // renderer row is still a painted shell row.
                        children.push(Box::new(PaintedBlank(Arc::clone(&painter))));
                    } else {
                        let row = Text::with_padding(line.clone(), 1, 0);
                        row.set_bg(Some(Arc::clone(&painter)));
                        children.push(Box::new(row));
                    }
                }
                children.push(Box::new(PaintedBlank(painter)));
                // Result images render after the shell box (upstream
                // `ToolExecutionComponent.updateDisplay` adds the image
                // components outside `contentBox`).
                for line in tool.image_lines.iter() {
                    children.push(Box::new(Text::with_padding(line.clone(), 0, 0)));
                }
            }
            ChatEntry::Raw {
                lines,
                render: Some(RawRender::Tool(tool)),
                ..
            } => {
                // `renderShell: "self"` (or a legacy registration with no
                // shell): the renderer owns the block, and result images are
                // appended after its lines.
                children.push(Box::new(Spacer::new(1)));
                for line in lines {
                    children.push(Box::new(Text::with_padding(line.clone(), 0, 0)));
                }
                for line in tool.image_lines.iter() {
                    children.push(Box::new(Text::with_padding(line.clone(), 0, 0)));
                }
            }
            ChatEntry::Raw { lines, .. } => {
                children.push(Box::new(Spacer::new(1)));
                for line in lines {
                    children.push(Box::new(Text::with_padding(line.clone(), 0, 0)));
                }
            }
            ChatEntry::Streaming {
                live,
                thinking,
                text,
            } => {
                children.push(Box::new(Spacer::new(1)));
                // Live thinking block (upstream's live assistant component
                // renders the thinking run while the provider streams it): dim
                // `thinkingText` markdown — or the hidden-thinking label — so
                // the thinking phase is visible instead of a silent screen.
                // The handle is retained so the next thinking delta swaps the
                // text in place.
                let thinking_block = if thinking.trim().is_empty() {
                    None
                } else {
                    let component = Arc::new(StreamingThinking::new(
                        thinking,
                        1,
                        self.hide_thinking,
                        Arc::clone(&self.hidden_thinking_label),
                    ));
                    children.push(Box::new(SharedThinking(Arc::clone(&component))));
                    Some(component)
                };
                // Blank separator between the thinking run and the text block
                // (upstream's assistant component pushes one when content
                // follows a thinking run). It is laid down as soon as the
                // thinking block exists — not when the first text delta lands —
                // so the block below does not shift a row mid-stream.
                if thinking_block.is_some() {
                    children.push(Box::new(Spacer::new(1)));
                }
                // Live assistant text renders as markdown like the final entry
                // (upstream re-renders the message component per delta); the
                // accumulation replaces the whole block. The handle is kept so
                // the next delta extends the text in place — the first commit
                // copies the body once (the entry owns the accumulator;
                // nothing can hand it over without one copy).
                //
                // `markdown.mermaid` rides the live body too (upstream
                // registers the mermaid transformer on the assistant path and
                // passes `isStreaming`): in `streaming` mode an open mermaid
                // fence re-renders per delta; the partial source falls back to
                // the raw fence until the engine parses it, and `final` mode
                // keeps the fence until the settled entry replaces this one.
                let markdown = Arc::new(Markdown::with_options(
                    text.as_str(),
                    1,
                    0,
                    MarkdownTheme::default(),
                    MarkdownOptions {
                        mermaid: super::markdown::global_mermaid_mode(),
                        is_streaming: true,
                        ..MarkdownOptions::default()
                    },
                ));
                children.push(Box::new(SharedMarkdown(Arc::clone(&markdown))));
                if *live {
                    streaming = Some(CommittedStreaming {
                        thinking: thinking_block,
                        markdown,
                    });
                }
            }
        }
        let record = CommittedEntry {
            // The record carries every render input of the children just built
            // (see [`EntryTag`]), so the next `set_entries` can keep them.
            tag: EntryTag::of(entry),
            children: children.len(),
            streaming,
            tool,
            pending: matches!(entry, ChatEntry::ToolExecution { pending: true, .. }),
            user_before,
        };
        (record, children)
    }
}

impl Component for ChatContainer {
    /// The trait's owned-line form: the same rows, built through
    /// [`Component::render_into`] in a fresh pool, exactly like the migrated
    /// `messages.rs` components. The frame renderer drives the canvas path;
    /// this bridge keeps `render` correct for callers that ask for lines
    /// (tests, alternate drivers).
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        {
            let mut rows = Rows::new(&mut lines);
            self.render_into(width, &mut rows);
            rows.trim();
        }
        lines
    }

    /// Canvas-direct render: the transcript's children write their rows
    /// straight into the frame's recycled row buffers, in the same order the
    /// `render` path concatenates them (the [`Container::render_into`]
    /// contract), so the frame no longer pays one `Vec<String>` plus one
    /// `String` per transcript row on the way to the canvas.
    ///
    /// Wrappers around this component must forward here rather than to
    /// `render`: the live app holds it in `terminal.rs`'s lock-holding
    /// `SharedChat`, whose trait-default `render_into` would bridge back
    /// through the allocating `render`. That wrapper needs the same one-line
    /// `render_into` override for the canvas path to reach the transcript.
    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        self.inner.render_into(width, out);
    }

    fn invalidate(&self) {
        self.inner.invalidate();
    }
}

/// The working status indicator spinner (upstream `Loader` frames).
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn spinner_frame(frame: usize) -> &'static str {
    SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]
}

/// The working status body (upstream `WorkingStatusIndicator` message shape,
/// status-indicator.ts:38-48): spinner frame + label. The label is the
/// upstream `workingMessage ?? defaultWorkingMessage` ("Working",
/// interactive-mode.ts:484-487). Upstream embeds it into the editor's TOP
/// border with the indicator painted in the editor border color
/// (`interactive-mode.ts:2346-2358`; `CustomEditor.renderTopBorder`,
/// custom-editor.ts:36-79), so this returns the RAW body; the dock paints
/// it with the border color and splices it into the border line.
pub fn render_working_indicator(frame: usize, message: &str) -> String {
    format!("{} {}", spinner_frame(frame), message)
}

/// `ctx.ui.setWorkingIndicator(options?)` runtime state (upstream
/// `WorkingIndicatorOptions`, types.ts:126-133): custom frames render
/// verbatim (upstream: "custom frames are rendered as provided, so extensions
/// must add their own colors"), `intervalMs` paces the frame advance, and an
/// empty `frames` list hides the indicator entirely.
#[derive(Debug, Clone)]
pub struct WorkingIndicatorFrames {
    /// `None` = the built-in spinner frames; `Some(vec![])` = hidden;
    /// `Some(frames)` = the custom frames.
    pub frames: Option<Vec<String>>,
    /// Frame interval in ms (0 = advance every tick, the built-in cadence).
    pub interval_ms: u64,
    /// When the current frame was entered (paces `interval_ms`).
    pub last_advance: Instant,
}

/// The frame text for `frame`: the custom `ctx.ui.setWorkingIndicator` frames
/// when set, the built-in spinner frames otherwise.
pub fn working_frame_text(custom: Option<&WorkingIndicatorFrames>, frame: usize) -> String {
    match custom.and_then(|state| state.frames.as_ref()) {
        Some(frames) if !frames.is_empty() => frames[frame % frames.len()].clone(),
        _ => spinner_frame(frame).to_string(),
    }
}

/// [`render_working_indicator`] over an explicit frame text (the custom-frames
/// path; the built-in path keeps the `usize` signature its callers use).
pub fn render_working_indicator_with(spinner: &str, message: &str) -> String {
    format!("{spinner} {message}")
}

/// Queued pending messages display (upstream
/// `updatePendingMessagesDisplay`): `Steering:`/`Follow-up:` lines plus the
/// dequeue hint.
///
/// `dequeue_hint` is the LIVE `app.message.dequeue` chord text (upstream
/// `getAppKeyDisplay("app.message.dequeue")`; `alt+q` on Windows, `alt+up`
/// elsewhere). It must come from the keybinding registry — a hardcoded chord
/// told Windows users to press a key that navigates history there. An empty
/// hint (unbound) drops the key text instead of inventing one.
pub fn render_pending_messages(
    steering: &[String],
    follow_up: &[String],
    dequeue_hint: &str,
) -> Vec<String> {
    if steering.is_empty() && follow_up.is_empty() {
        return Vec::new();
    }
    let dim = |text: &str| with_theme(|theme| theme.fg(ThemeColor::Dim, text));
    let mut lines = vec![String::new()];
    for message in steering {
        lines.push(truncate_to_width(
            &dim(&format!("Steering: {message}")),
            80,
            "…",
        ));
    }
    for message in follow_up {
        lines.push(truncate_to_width(
            &dim(&format!("Follow-up: {message}")),
            80,
            "…",
        ));
    }
    let hint = if dequeue_hint.trim().is_empty() {
        "↳ to edit all queued messages".to_string()
    } else {
        format!("↳ {dequeue_hint} to edit all queued messages")
    };
    lines.push(truncate_to_width(&dim(&hint), 80, "…"));
    lines
}

/// Replace the pending-message display state with one authoritative queue
/// snapshot (upstream `updatePendingMessagesDisplay` clears the container and
/// re-adds from `getAllQueuedMessages()`).
///
/// The snapshot REPLACES, never appends: the session's `queue_update` carries
/// both queues' full pending texts, and the drain that delivers a steering
/// message re-emits it with that message removed. An append would leave the
/// delivered line on screen forever; an empty snapshot (delivery, `clearQueue`,
/// run end) empties the display.
pub fn apply_queue_snapshot(
    pending_steering: &Mutex<Vec<String>>,
    pending_follow_up: &Mutex<Vec<String>>,
    steering: Vec<String>,
    follow_up: Vec<String>,
) {
    let poison = std::sync::PoisonError::into_inner;
    *pending_steering.lock().unwrap_or_else(poison) = steering;
    *pending_follow_up.lock().unwrap_or_else(poison) = follow_up;
}

/// Terminal title (upstream `updateTerminalTitle`).
pub fn terminal_title(session_name: Option<&str>, cwd: &str) -> String {
    let cwd_basename = std::path::Path::new(cwd)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.to_string());
    match session_name {
        Some(name) => format!("{APP_TITLE} - {name} - {cwd_basename}"),
        None => format!("{APP_TITLE} - {cwd_basename}"),
    }
}

/// A markdown chat block re-exported for the app assembly.
pub struct MarkdownBlock(pub Markdown);

impl Component for MarkdownBlock {
    fn render(&self, width: usize) -> Vec<String> {
        self.0.render(width)
    }

    fn invalidate(&self) {
        self.0.invalidate();
    }
}

/// Shared app state handle for the event loop.
pub type SharedFooter = Arc<Footer>;

/// A startup header line whose Compact/ExpandedKeys text is resolved from the
/// LIVE keybinding registry once at startup (upstream builds the hints from
/// `getKeybindings()` at construction, interactive-mode.ts:1021-1056).
/// Blank/Logo/Onboarding render identically to the static default.
pub struct LiveHeaderLine {
    kind: HeaderLineKind,
    kb: super::keybindings::Keybindings,
    /// Memo of this line's styled+padded row (see [`HeaderLineCache`]).
    cache: Mutex<Option<HeaderLineCache>>,
}

/// Memo of one [`LiveHeaderLine`]'s rendered row: the styled line padded to
/// the render width, exactly the row the canvas path writes into the frame.
///
/// The key is complete: `kind` and the resolved keybinding registry are
/// construction-fixed (both fields are private and there is no setter, so the
/// `kb` clone [`LiveHeaderLine::new`] took can never move), the theme
/// participates through [`Theme::generation`] — every `set_theme` bump means
/// a new palette, and `fg`/`bold`/`muted` are pure reads of it — and the
/// padding is a function of the styled line plus `width`. Without the memo
/// the 18-chord `ExpandedKeys` line re-derives its chord list AND re-styles
/// every hint on every frame, for a line that only moves when one of those
/// three inputs does.
struct HeaderLineCache {
    kind: HeaderLineKind,
    width: usize,
    theme_generation: u64,
    row: String,
}

impl HeaderLineCache {
    fn matches(&self, kind: HeaderLineKind, width: usize, theme_generation: u64) -> bool {
        self.kind == kind && self.width == width && self.theme_generation == theme_generation
    }
}

impl LiveHeaderLine {
    pub fn new(kind: HeaderLineKind, kb: &super::keybindings::Keybindings) -> Self {
        Self {
            kind,
            kb: kb.clone(),
            cache: Mutex::new(None),
        }
    }

    /// The memoized row for `width`, rebuilt only when the line, the width or
    /// the active palette moved. The guard is returned so callers read the
    /// stored row in place — it is never moved out of the cache.
    fn row_for(&self, width: usize) -> std::sync::MutexGuard<'_, Option<HeaderLineCache>> {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // One palette generation per render, exactly like the tool-result
        // memo (`messages.rs` `ResultRowsKey`): the styled line is built
        // through `with_theme`, so tagging the entry with the generation read
        // here can only mis-tag it across a concurrent install, and a
        // generation that moved is a miss on the next frame — never a stale
        // hit.
        let generation = Theme::generation();
        let fresh = !cache
            .as_ref()
            .is_some_and(|entry| entry.matches(self.kind, width, generation));
        if fresh {
            let mut row = header_line_styled_for(self.kind, &self.kb);
            // `pad_to_width`'s padding rule, applied in place.
            let pad = width.saturating_sub(visible_width(&row));
            row.reserve(pad);
            for _ in 0..pad {
                row.push(' ');
            }
            *cache = Some(HeaderLineCache {
                kind: self.kind,
                width,
                theme_generation: generation,
                row,
            });
        }
        cache
    }
}

impl Component for LiveHeaderLine {
    /// The trait's owned-line form: the same row, cloned out of the memo when
    /// the inputs did not move (the frame renderer drives the canvas path;
    /// this bridge keeps `render` correct for callers that ask for lines).
    fn render(&self, width: usize) -> Vec<String> {
        let cache = self.row_for(width);
        match cache.as_ref() {
            Some(entry) => vec![entry.row.clone()],
            None => Vec::new(),
        }
    }

    /// Direct-write path: the styled line is built once per input change and
    /// the memoized row is copied into the frame's recycled row buffer (the
    /// padding is part of the cached row), dropping both the per-frame
    /// restyle and the `pad_to_width` copy the owned form builds. Row for row
    /// byte-identical to [`Component::render`] (`pad_to_width`'s exact
    /// padding rule).
    fn render_into(&self, width: usize, out: &mut Rows<'_>) {
        let cache = self.row_for(width);
        if let Some(entry) = cache.as_ref() {
            out.push(&entry.row);
        }
    }

    fn invalidate(&self) {
        *self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}
