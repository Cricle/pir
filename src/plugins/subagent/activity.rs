//! Live subagent activity: the host-side run list, the compact status
//! widget, and the keyboard-selectable overlay.
//!
//! The tool streams per-task partials through `tool_execution_update`; this
//! module adds the pieces partials cannot provide: a stable run id, a
//! per-task abort handle, and a list that survives between updates. The
//! overlay is an ordinary native `ctx.ui.custom` component, so it uses the
//! same focus/admission/settlement path as every other TUI overlay.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde_json::Value as JsonValue;

use super::results::{is_failed, SingleResult, UsageTotals};
use crate::ai::types::AbortSignal;
use crate::plugins::services::{
    DialogOptions, FactoryRenderContext, NotifyLevel, OverlayComponent, OverlayComponentFactory,
    OverlayDone, UiService, WidgetFactory, WidgetPlacement,
};

/// How long a finished run stays visible in the compact widget.
const RECENT_WINDOW: Duration = Duration::from_secs(12);
/// Widget key (the TUI factory registry slot).
pub const WIDGET_KEY: &str = "subagents";
/// Overlay command/shortcut.
pub const COMMAND_NAME: &str = "subagents";
pub const SHORTCUT: &str = "ctrl+shift+a";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Running,
    Done,
    Failed,
    Aborted,
}

impl RunState {
    fn glyph(self) -> &'static str {
        match self {
            Self::Running => "◐",
            Self::Done => "✓",
            Self::Failed => "✗",
            Self::Aborted => "⊘",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunSnapshot {
    pub id: u64,
    pub agent: String,
    pub task: String,
    pub mode: &'static str,
    pub step: Option<usize>,
    pub state: RunState,
    pub elapsed: Duration,
    pub finished: Option<Instant>,
    pub turns: u64,
    pub tokens: u64,
    pub model: Option<String>,
    pub last: String,
    pub output: String,
    pub error: Option<String>,
}

struct RunEntry {
    id: u64,
    agent: String,
    task: String,
    mode: &'static str,
    step: Option<usize>,
    state: RunState,
    started: Instant,
    finished: Option<Instant>,
    usage: UsageTotals,
    model: Option<String>,
    last: String,
    output: String,
    error: Option<String>,
    signal: AbortSignal,
}

impl RunEntry {
    fn snapshot(&self) -> RunSnapshot {
        RunSnapshot {
            id: self.id,
            agent: self.agent.clone(),
            task: self.task.clone(),
            mode: self.mode,
            step: self.step,
            state: self.state,
            elapsed: self
                .finished
                .unwrap_or_else(Instant::now)
                .saturating_duration_since(self.started),
            finished: self.finished,
            turns: self.usage.turns,
            tokens: self
                .usage
                .input
                .saturating_add(self.usage.output)
                .saturating_add(self.usage.cache_read),
            model: self.model.clone(),
            last: self.last.clone(),
            output: self.output.clone(),
            error: self.error.clone(),
        }
    }
}

/// A started run: the id the overlay selects and the signal it can abort.
pub struct RunHandle {
    pub id: u64,
    pub signal: AbortSignal,
}

/// The mutex-guarded board shared with the widget. Kept separate from the
/// plugin-facing handle so the widget factory never holds a cycle back to the
/// UI service.
struct ActivityData {
    runs: Mutex<Vec<RunEntry>>,
}

impl ActivityData {
    fn snapshots(&self) -> Vec<RunSnapshot> {
        let runs = self
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut snapshots: Vec<RunSnapshot> = runs.iter().map(RunEntry::snapshot).collect();
        snapshots.sort_by_key(|snapshot| snapshot.id);
        snapshots
    }
}

/// Shared activity board. One instance belongs to the `subagent` builtin and
/// is cloned into the tool, the overlay factory and the widget.
pub struct SubagentActivity {
    data: Arc<ActivityData>,
    next_id: AtomicU64,
    ui: RwLock<Option<Arc<dyn UiService>>>,
    widget_registered: AtomicBool,
}

impl Default for SubagentActivity {
    fn default() -> Self {
        Self::new()
    }
}

impl SubagentActivity {
    pub fn new() -> Self {
        Self {
            data: Arc::new(ActivityData {
                runs: Mutex::new(Vec::new()),
            }),
            next_id: AtomicU64::new(1),
            ui: RwLock::new(None),
            widget_registered: AtomicBool::new(false),
        }
    }

    /// Bind the session UI service (called from `SubagentPlugin::on_context`).
    pub fn bind_ui(&self, ui: Option<Arc<dyn UiService>>) {
        *self
            .ui
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ui;
        self.widget_registered.store(false, Ordering::Relaxed);
        self.repaint();
    }

    fn ui(&self) -> Option<Arc<dyn UiService>> {
        self.ui
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Start one task. `mode` is `"single"`, `"parallel"` or `"chain"`.
    pub fn begin(
        &self,
        agent: impl Into<String>,
        task: impl Into<String>,
        mode: &'static str,
        step: Option<usize>,
    ) -> RunHandle {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let signal = AbortSignal::new();
        let mut runs = self
            .data
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        const MAX_RUNS: usize = 64;
        while runs.len() >= MAX_RUNS {
            let Some(position) = runs.iter().position(|run| run.state != RunState::Running) else {
                break;
            };
            runs.remove(position);
        }
        runs.push(RunEntry {
            id,
            agent: agent.into(),
            task: task.into(),
            mode,
            step,
            state: RunState::Running,
            started: Instant::now(),
            finished: None,
            usage: UsageTotals::default(),
            model: None,
            last: "starting…".to_owned(),
            output: String::new(),
            error: None,
            signal: signal.clone(),
        });
        drop(runs);
        self.repaint();
        RunHandle { id, signal }
    }

    /// Apply one live partial. `result.pending` keeps the run running.
    pub fn update(&self, id: u64, result: &SingleResult) {
        let mut runs = self
            .data
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(run) = runs.iter_mut().find(|run| run.id == id) else {
            return;
        };
        run.usage = result.usage;
        run.model = result.model.clone();
        let output = super::get_final_output(&result.messages).unwrap_or_default();
        if !output.is_empty() {
            run.last = first_line(&output).to_owned();
            run.output = output;
        } else if run.last.is_empty() {
            run.last = "(running…)".to_owned();
        }
        if !result.pending {
            run.state = state_of(result);
            run.finished = Some(Instant::now());
            run.error = result
                .error_message
                .clone()
                .or_else(|| result.stderr.clone());
        }
        drop(runs);
        self.repaint();
    }

    /// Mark one run terminal and notify. Safe to call once per task.
    pub fn finish(&self, id: u64, result: &SingleResult) {
        let mut runs = self
            .data
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(run) = runs.iter_mut().find(|run| run.id == id) else {
            return;
        };
        run.usage = result.usage;
        run.model = result.model.clone();
        run.state = state_of(result);
        run.finished = Some(Instant::now());
        run.output = super::get_final_output(&result.messages).unwrap_or_default();
        run.last = if run.output.is_empty() {
            run.state.label().to_owned()
        } else {
            first_line(&run.output).to_owned()
        };
        run.error = result
            .error_message
            .clone()
            .or_else(|| result.stderr.clone());
        let elapsed = run
            .finished
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(run.started);
        let agent = run.agent.clone();
        let state = run.state;
        let turns = run.usage.turns;
        let tokens = run
            .usage
            .input
            .saturating_add(run.usage.output)
            .saturating_add(run.usage.cache_read);
        drop(runs);
        self.repaint();
        self.notify(
            match state {
                RunState::Failed => NotifyLevel::Error,
                RunState::Aborted => NotifyLevel::Warning,
                _ => NotifyLevel::Info,
            },
            format!(
                "Subagent {agent} {} in {} · {turns} turns · {} tokens",
                state.label(),
                format_duration(elapsed),
                format_tokens(tokens)
            ),
        );
    }

    /// Abort one selected run. Returns `false` for an unknown or terminal id.
    pub fn abort(&self, id: u64) -> bool {
        let mut runs = self
            .data
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(run) = runs
            .iter_mut()
            .find(|run| run.id == id && run.state == RunState::Running)
        else {
            return false;
        };
        run.signal.abort();
        run.last = "aborting…".to_owned();
        let agent = run.agent.clone();
        drop(runs);
        self.repaint();
        self.notify(NotifyLevel::Warning, format!("Aborting subagent {agent}…"));
        true
    }

    pub fn snapshots(&self) -> Vec<RunSnapshot> {
        self.data.snapshots()
    }

    pub fn running_count(&self) -> usize {
        self.data
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|run| run.state == RunState::Running)
            .count()
    }

    fn notify(&self, level: NotifyLevel, message: String) {
        if let Some(ui) = self.ui() {
            ui.notify(level, &message);
        }
    }

    /// Register the compact status widget once per bound UI service. The
    /// widget renders empty once no run is running and the last finish is
    /// older than [`RECENT_WINDOW`].
    fn repaint(&self) {
        let Some(ui) = self.ui() else {
            return;
        };
        if !self.widget_registered.swap(true, Ordering::Relaxed) {
            ui.set_widget(
                WIDGET_KEY,
                Arc::new(SubagentWidget {
                    data: Arc::clone(&self.data),
                }),
                WidgetPlacement::BelowEditor,
            );
        }
    }

    /// Open the keyboard-selectable overlay. Returns `false` when the session
    /// has no UI.
    pub async fn open_overlay(self: &Arc<Self>) -> bool {
        let Some(ui) = self.ui() else {
            return false;
        };
        let factory = Arc::new(SubagentOverlayFactory {
            activity: Arc::clone(self),
        });
        ui.custom(
            factory,
            DialogOptions::with_timeout(Duration::from_secs(60 * 60)),
        )
        .await
        .is_ok()
    }
}

fn state_of(result: &SingleResult) -> RunState {
    if result.pending {
        return RunState::Running;
    }
    if result
        .stop_reason
        .as_deref()
        .is_some_and(|reason| reason == "aborted")
    {
        return RunState::Aborted;
    }
    if is_failed(result) {
        RunState::Failed
    } else {
        RunState::Done
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();
    if seconds < 10.0 {
        format!("{seconds:.1}s")
    } else if seconds < 60.0 {
        format!("{seconds:.0}s")
    } else {
        format!("{}m{:02}s", (seconds as u64) / 60, (seconds as u64) % 60)
    }
}

fn format_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        format!("{:.2}M", tokens as f64 / 1_000_000.0)
    }
}

fn clip(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(width.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

fn wrap(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = Vec::new();
    for raw in text.lines() {
        let mut current = String::new();
        for word in raw.split_whitespace() {
            if current.is_empty() {
                current.push_str(word);
            } else if current.chars().count() + 1 + word.chars().count() <= width {
                current.push(' ');
                current.push_str(word);
            } else {
                lines.push(current);
                current = word.to_owned();
            }
            if lines.len() >= max_lines {
                break;
            }
        }
        if !current.is_empty() && lines.len() < max_lines {
            lines.push(current);
        }
        if lines.len() >= max_lines {
            break;
        }
    }
    if lines.len() == max_lines && text.chars().count() > width * max_lines {
        if let Some(last) = lines.last_mut() {
            *last = clip(last, width.saturating_sub(1));
            last.push('…');
        }
    }
    lines
}

struct SubagentWidget {
    data: Arc<ActivityData>,
}

impl WidgetFactory for SubagentWidget {
    fn render(&self, context: &FactoryRenderContext<'_>) -> Vec<String> {
        let snapshots = self.data.snapshots();
        let running = snapshots
            .iter()
            .filter(|snapshot| snapshot.state == RunState::Running)
            .count();
        let recent = snapshots.iter().rev().find(|snapshot| {
            snapshot
                .finished
                .is_some_and(|finished| finished.elapsed() <= RECENT_WINDOW)
        });
        if running == 0 && recent.is_none() {
            return Vec::new();
        }
        let line = if running > 0 {
            let done = snapshots.len().saturating_sub(running);
            format!("Agents · {running} running · {done} done · {SHORTCUT}")
        } else if let Some(snapshot) = recent {
            format!(
                "Agents · {} {} {} · {} · {SHORTCUT}",
                snapshot.state.glyph(),
                snapshot.agent,
                format_duration(snapshot.elapsed),
                snapshot.state.label()
            )
        } else {
            return Vec::new();
        };
        vec![clip(&line, context.width.max(12))]
    }
}

/// Test seam: render the compact widget without binding a UI service.
#[doc(hidden)]
pub fn widget_lines_for_tests(activity: &SubagentActivity, width: usize) -> Vec<String> {
    SubagentWidget {
        data: Arc::clone(&activity.data),
    }
    .render(&FactoryRenderContext {
        width,
        theme: None,
        footer_data: None,
        request_render: None,
    })
}

/// Test seam: build the overlay component without a UI service.
#[doc(hidden)]
pub fn overlay_component_for_tests(
    activity: Arc<SubagentActivity>,
    done: OverlayDone,
) -> Arc<dyn OverlayComponent> {
    Arc::new(SubagentOverlay {
        activity,
        selected: AtomicUsize::new(0),
        expanded: AtomicBool::new(false),
        done,
    })
}

struct SubagentOverlayFactory {
    activity: Arc<SubagentActivity>,
}

impl OverlayComponentFactory for SubagentOverlayFactory {
    fn create(&self, done: OverlayDone) -> Option<Arc<dyn OverlayComponent>> {
        Some(Arc::new(SubagentOverlay {
            activity: Arc::clone(&self.activity),
            selected: AtomicUsize::new(0),
            expanded: AtomicBool::new(false),
            done,
        }))
    }
}

struct SubagentOverlay {
    activity: Arc<SubagentActivity>,
    selected: AtomicUsize,
    expanded: AtomicBool,
    done: OverlayDone,
}

impl SubagentOverlay {
    fn close(&self) {
        (self.done)(JsonValue::Null);
    }
}

impl OverlayComponent for SubagentOverlay {
    fn render(&self, context: &FactoryRenderContext<'_>) -> Vec<String> {
        let width = context.width.max(24);
        let snapshots = self.activity.snapshots();
        let running = snapshots
            .iter()
            .filter(|snapshot| snapshot.state == RunState::Running)
            .count();
        let mut lines = vec![clip(
            &format!(
                "Subagents · {running} running · {} total · ↑/↓ select",
                snapshots.len()
            ),
            width,
        )];
        if snapshots.is_empty() {
            lines.push("No subagent runs in this session.".to_owned());
            lines.push("Esc closes.".to_owned());
            return lines;
        }
        let selected = self
            .selected
            .load(Ordering::Relaxed)
            .min(snapshots.len() - 1);
        for (index, snapshot) in snapshots.iter().enumerate() {
            let marker = if index == selected { "▶" } else { " " };
            let step = snapshot
                .step
                .map(|step| format!(" step {step}"))
                .unwrap_or_default();
            let row = format!(
                "{marker} {} #{} {}{} · {} · {} · {} tok",
                snapshot.state.glyph(),
                snapshot.id,
                snapshot.agent,
                step,
                snapshot.state.label(),
                format_duration(snapshot.elapsed),
                format_tokens(snapshot.tokens)
            );
            lines.push(clip(&row, width));
        }
        if self.expanded.load(Ordering::Relaxed) {
            if let Some(snapshot) = snapshots.get(selected) {
                lines.push(String::new());
                lines.push(clip(&format!("#{} {}", snapshot.id, snapshot.agent), width));
                for line in wrap(&format!("task: {}", snapshot.task), width, 6) {
                    lines.push(line);
                }
                if let Some(model) = &snapshot.model {
                    lines.push(clip(&format!("model: {model}"), width));
                }
                let last = if snapshot.last.is_empty() {
                    String::new()
                } else {
                    format!(" · last: {}", snapshot.last)
                };
                lines.push(clip(
                    &format!(
                        "usage: {} turns · {} tokens{last}",
                        snapshot.turns,
                        format_tokens(snapshot.tokens)
                    ),
                    width,
                ));
                if let Some(error) = &snapshot.error {
                    for line in wrap(&format!("error: {error}"), width, 4) {
                        lines.push(line);
                    }
                }
                if !snapshot.output.is_empty() {
                    for line in wrap(&format!("output: {}", snapshot.output), width, 8) {
                        lines.push(line);
                    }
                }
            }
        }
        lines.push(String::new());
        lines.push(
            "↑/↓ select · enter details · x abort · esc close"
                .chars()
                .take(width)
                .collect(),
        );
        lines
    }

    fn handle_input(&self, data: &str) {
        match data {
            "\u{1b}[A" | "\u{1b}OA" => {
                let len = self.activity.snapshots().len();
                if len > 0 {
                    let current = self.selected.load(Ordering::Relaxed);
                    self.selected
                        .store(current.saturating_sub(1), Ordering::Relaxed);
                }
            }
            "\u{1b}[B" | "\u{1b}OB" => {
                let len = self.activity.snapshots().len();
                if len > 0 {
                    let current = self.selected.load(Ordering::Relaxed);
                    self.selected
                        .store((current + 1).min(len - 1), Ordering::Relaxed);
                }
            }
            "\r" | "\n" => {
                let expanded = self.expanded.load(Ordering::Relaxed);
                self.expanded.store(!expanded, Ordering::Relaxed);
            }
            "x" | "X" => {
                let snapshots = self.activity.snapshots();
                if let Some(snapshot) = snapshots.get(self.selected.load(Ordering::Relaxed)) {
                    self.activity.abort(snapshot.id);
                }
            }
            "\u{1b}" | "q" | "Q" => self.close(),
            _ => {}
        }
    }
}
