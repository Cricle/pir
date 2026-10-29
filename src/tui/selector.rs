//! Full-screen selector components — port of upstream `model-selector.ts`
//! and `thinking-selector.ts`: DynamicBorder-framed, search input on top,
//! windowed list with `→` cursor and `✓` current marker, footer hints.

use std::sync::Mutex;

use super::core::{Component, Container, DynamicBorder, Spacer, Text};
use super::theme::{with_theme, ThemeColor};
use super::{truncate_to_width, visible_width};

/// One selectable entry.
#[derive(Debug, Clone)]
pub struct SelectItem<T> {
    pub value: T,
    pub id: String,
    pub provider: String,
    pub description: String,
    pub is_current: bool,
    pub is_default: bool,
}

/// Shared selector state + rendering (upstream windowed list with
/// `(n/total)` scroll info and `No matching models` placeholder).
/// The search-text getter one select item scores against (upstream
/// `fuzzyFilter`'s text getter).
type SearchTextFn<T> = Box<dyn Fn(&SelectItem<T>) -> String + Send>;

pub struct SelectList<T> {
    items: Vec<SelectItem<T>>,
    selected: usize,
    max_visible: usize,
    query: String,
    /// Raw filter text as typed, kept for the visible search line
    /// (upstream `searchInput.getValue()`, model-selector.ts:209).
    raw_query: String,
    /// The search text a query scores against (upstream `fuzzyFilter`'s text
    /// getter). Defaults to the `id provider description` join; the model
    /// selector overrides it with the upstream provider-prefixed ordering
    /// (model-search.ts:17-21).
    search_text: SearchTextFn<T>,
}

impl<T> SelectList<T> {
    pub fn new(items: Vec<SelectItem<T>>) -> Self {
        let selected = items.iter().position(|item| item.is_current).unwrap_or(0);
        Self {
            items,
            selected,
            max_visible: 10,
            query: String::new(),
            raw_query: String::new(),
            search_text: Box::new(|item: &SelectItem<T>| {
                format!("{} {} {}", item.id, item.provider, item.description)
            }),
        }
    }

    /// Replace the fuzzy search-text getter (upstream `getModelSelectorSearchText`).
    pub fn set_search_text(&mut self, search_text: SearchTextFn<T>) {
        self.search_text = search_text;
    }

    pub fn set_query(&mut self, query: &str) {
        // Keep the selection pointing at the same value when possible. All
        // ends are FILTERED positions: map the old filtered position to the
        // item's raw index first (comparing raw indices against filtered
        // positions, as this used to, mixed the two orders), then re-find
        // that item in the new filtered list.
        let previously_selected = self.filtered().get(self.selected).map(|(index, _)| *index);
        self.raw_query = query.to_string();
        self.query = query.to_lowercase();
        self.selected = previously_selected
            .and_then(|previous| {
                self.filtered()
                    .iter()
                    .position(|(index, _)| *index == previous)
            })
            .unwrap_or(0);
    }

    /// Indices of items matching the fuzzy query, best score first (upstream
    /// `fuzzyFilter`: whitespace/`/`-separated tokens, every token must
    /// match, ascending accumulated score). The search text defaults to the
    /// `id provider description` join unless the selector installed the
    /// upstream provider-prefixed ordering
    /// ([`SelectList::set_search_text`], model-search.ts:17-21).
    fn filtered(&self) -> Vec<(usize, &SelectItem<T>)> {
        let tokens: Vec<String> = self
            .query
            .split(|c: char| c.is_whitespace() || c == '/')
            .filter(|token| !token.is_empty())
            .map(str::to_string)
            .collect();
        if tokens.is_empty() {
            return self.items.iter().enumerate().collect();
        }
        let mut results: Vec<(usize, &SelectItem<T>, f64)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let text = (self.search_text)(item);
                let mut total = 0.0f64;
                for token in &tokens {
                    let matched = super::fuzzy::fuzzy_match(token, &text);
                    if matched.matches {
                        total += matched.score;
                    } else {
                        return None;
                    }
                }
                Some((index, item, total))
            })
            .collect();
        results.sort_by(|a, b| a.2.total_cmp(&b.2));
        results
            .into_iter()
            .map(|(index, item, _)| (index, item))
            .collect()
    }

    pub fn selected_item(&self) -> Option<&SelectItem<T>> {
        self.filtered().get(self.selected).map(|(_, item)| *item)
    }

    /// The raw (unfiltered) items — the marker lookups a scope switch needs
    /// (upstream `allModels`/`scopedModelItems`); test seam.
    pub fn items(&self) -> &[SelectItem<T>] {
        &self.items
    }

    /// Raw filter text as typed (upstream `searchInput.getValue()`); the
    /// lowercased filter key lives in `query`.
    pub fn raw_query(&self) -> &str {
        &self.raw_query
    }

    /// Number of items matching the current query (upstream
    /// `filteredModels.length`, model-selector.ts:349).
    pub fn filtered_len(&self) -> usize {
        self.filtered().len()
    }

    pub fn move_up(&mut self) {
        let count = self.filtered().len();
        if count == 0 {
            return;
        }
        self.selected = if self.selected == 0 {
            count - 1
        } else {
            self.selected - 1
        };
    }

    pub fn move_down(&mut self) {
        let count = self.filtered().len();
        if count == 0 {
            return;
        }
        self.selected = if self.selected + 1 >= count {
            0
        } else {
            self.selected + 1
        };
    }

    /// Jump up one page (upstream `tui.select.pageUp`).
    pub fn page_up(&mut self) {
        let count = self.filtered().len();
        if count == 0 {
            return;
        }
        self.selected = self.selected.saturating_sub(self.max_visible);
    }

    /// Jump down one page (upstream `tui.select.pageDown`).
    pub fn page_down(&mut self) {
        let count = self.filtered().len();
        if count == 0 {
            return;
        }
        self.selected = (self.selected + self.max_visible).min(count - 1);
    }

    /// The visible window `(start, end)` over the filtered list (upstream
    /// `getVisibleRange`), shared by the renderer and the mouse hit-test.
    fn visible_range(&self) -> (usize, usize) {
        let len = self.filtered().len();
        let max_visible = self.max_visible;
        let start = self
            .selected
            .saturating_sub(max_visible / 2)
            .min(len.saturating_sub(max_visible.min(len)));
        (start, (start + max_visible).min(len))
    }

    /// W11-C selector click: the FILTERED position a click on visible list row
    /// `row` (0-based; the row the list's first item painted on) selects.
    /// `None` when the row is past the window (a click on the scroll
    /// indicator is not an item, matching upstream `SelectList.handleMouse`).
    pub fn position_at_row(&self, row: usize) -> Option<usize> {
        let (start, end) = self.visible_range();
        let position = start.checked_add(row)?;
        (position < end).then_some(position)
    }

    /// Select the filtered position a click resolved to; returns whether the
    /// selection moved (upstream `handleMouse` press/click).
    pub fn select_position(&mut self, position: usize) -> bool {
        if position >= self.filtered().len() {
            return false;
        }
        let changed = self.selected != position;
        self.selected = position;
        changed
    }

    /// Render the visible window (upstream `updateList` line shapes).
    pub fn render_lines(&self, width: usize) -> Vec<String> {
        let all: Vec<(usize, &SelectItem<T>)> = self.filtered();
        if all.is_empty() {
            return vec![with_theme(|theme| {
                theme.fg(ThemeColor::Muted, "  No matching models")
            })];
        }
        let (start, end) = self.visible_range();
        let mut lines = Vec::new();
        for (offset, (_, item)) in all[start..end].iter().enumerate() {
            // `self.selected` is a FILTERED position (move_up/move_down/
            // selected_item all operate on `filtered()`), so the highlight
            // compares window-relative positions, not raw item indices
            // (upstream model-selector.ts:320 compares filtered positions).
            let is_selected = start + offset == self.selected;
            let cursor = if is_selected {
                with_theme(|theme| theme.fg(ThemeColor::Accent, "→ "))
            } else {
                "  ".to_string()
            };
            let current_marker = if item.is_current {
                with_theme(|theme| theme.fg(ThemeColor::Accent, "✓ "))
            } else {
                "  ".to_string()
            };
            let model_text = if is_selected {
                with_theme(|theme| theme.fg(ThemeColor::Accent, &item.id))
            } else {
                item.id.clone()
            };
            let provider_badge =
                with_theme(|theme| theme.fg(ThemeColor::Muted, &format!("[{}]", item.provider)));
            let default_badge = if item.is_default {
                with_theme(|theme| theme.fg(ThemeColor::Muted, " · default"))
            } else {
                String::new()
            };
            lines.push(truncate_to_width(
                &format!("{cursor}{current_marker}{model_text} {provider_badge}{default_badge}"),
                width,
                "…",
            ));
        }
        if start > 0 || end < all.len() {
            lines.push(with_theme(|theme| {
                theme.fg(
                    ThemeColor::Muted,
                    &format!("  ({}/{})", self.selected + 1, all.len()),
                )
            }));
        }
        lines
    }
}

/// The visible search line (upstream `Input.render`, pi-tui
/// `input.ts:419-500`): the default `> ` prompt, the raw filter text, and a
/// reverse-video cursor after it. Long queries scroll horizontally to keep
/// the cursor visible (upstream `startCol` logic, input.ts:444-476); the
/// cursor style follows the pir editor convention (editor.rs:663-677).
fn search_input_line(query: &str, width: usize) -> String {
    const PROMPT: &str = "> ";
    let cursor = "\x1b[7m \x1b[0m";
    let available = width.saturating_sub(PROMPT.len() + 1);
    let mut tail = String::new();
    let mut used = 0usize;
    for ch in query.chars().rev() {
        let mut buf = [0u8; 4];
        let ch_width = visible_width(ch.encode_utf8(&mut buf));
        if used + ch_width > available {
            break;
        }
        used += ch_width;
        tail.insert(0, ch);
    }
    format!("{PROMPT}{tail}{cursor}")
}

/// The model selector's catalog scope (upstream `ModelScope`,
/// model-selector.ts:35): every registry model, or only the `--models`
/// scoped subset. Tab toggles between them while the selector is open
/// (upstream `tui.input.tab`, model-selector.ts:363-370).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelScope {
    All,
    Scoped,
}

impl ModelScope {
    /// The other scope (upstream `nextScope`, model-selector.ts:366).
    pub fn toggled(self) -> Self {
        match self {
            ModelScope::All => ModelScope::Scoped,
            ModelScope::Scoped => ModelScope::All,
        }
    }

    /// The scope row's label text (upstream `getScopeText`).
    pub fn label(self) -> &'static str {
        match self {
            ModelScope::All => "all",
            ModelScope::Scoped => "scoped",
        }
    }
}

/// One `(provider, id)` pair list rendered as model selector items (the
/// current/default markers are matched by full `provider/id` reference).
fn model_items(
    models: &[(String, String)],
    current: Option<&str>,
    default: Option<&str>,
) -> Vec<SelectItem<String>> {
    models
        .iter()
        .map(|(provider, id)| {
            let key = format!("{provider}/{id}");
            SelectItem {
                value: key.clone(),
                id: id.clone(),
                provider: provider.clone(),
                description: String::new(),
                is_current: current == Some(key.as_str()),
                is_default: default == Some(key.as_str()),
            }
        })
        .collect()
}

/// [`SelectList`] over model items with the upstream search-text ordering
/// (model-search.ts:17-21): the provider-prefixed query must rank ahead of
/// bare proxy-provider ids like openrouter/openai/gpt-5. The default-model
/// marker joins the search text like upstream's `defaultText`
/// (model-selector.ts:282).
fn model_select_list(items: Vec<SelectItem<String>>) -> SelectList<String> {
    let mut list = SelectList::new(items);
    list.set_search_text(Box::new(|item: &SelectItem<String>| {
        let name = item.description.trim_end_matches(" · default");
        let default_text = if item.is_default { " default" } else { "" };
        format!(
            "{p} {p}/{id} {p} {id}{name}{default_text}",
            p = item.provider,
            id = item.id,
            name = if name.is_empty() {
                String::new()
            } else {
                format!(" {name}")
            },
        )
    }));
    list
}

/// The model selector overlay (upstream `ModelSelectorComponent` shape).
pub struct ModelSelector {
    container: Mutex<SelectorContainer>,
}

struct SelectorContainer {
    frame: Container,
    list: SelectList<String>,
    status: Option<String>,
    /// Every registry model, the `scope == all` catalog (upstream
    /// `allModels`).
    all_items: Vec<SelectItem<String>>,
    /// The `--models` scoped catalog (upstream `scopedModelItems`); empty
    /// when no scope is configured, which hides the `Scope:` row and shows
    /// upstream's login warning instead.
    scoped_items: Vec<SelectItem<String>>,
    /// The active catalog scope (upstream `scope`, initial `scoped` when a
    /// scope exists, model-selector.ts:94).
    scope: ModelScope,
}

impl SelectorContainer {
    /// Whether the scope row exists (upstream `scopedModels.length > 0`).
    fn scope_available(&self) -> bool {
        !self.scoped_items.is_empty()
    }

    /// The active scope's items (upstream `activeModels`).
    fn active_items(&self) -> Vec<SelectItem<String>> {
        match self.scope {
            ModelScope::All => self.all_items.clone(),
            ModelScope::Scoped => self.scoped_items.clone(),
        }
    }
}

impl ModelSelector {
    /// `models`: `(provider, id)` pairs; `current`: the active
    /// `provider/id`; `default`: the persisted default.
    pub fn new(models: &[(String, String)], current: Option<&str>, default: Option<&str>) -> Self {
        let all_items = model_items(models, current, default);
        let list = model_select_list(all_items.clone());
        let mut frame = Container::new();
        frame.add_child(DynamicBorder::new());
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding(
            with_theme(|theme| {
                // No-scope login hint in warning color (upstream
                // model-selector.ts:110-111).
                theme.fg(
                    ThemeColor::Warning,
                    "Only showing models from configured providers. Use /login to add providers.",
                )
            }),
            0,
            0,
        ));
        frame.add_child(Spacer::new(1));
        Self {
            container: Mutex::new(SelectorContainer {
                frame,
                list,
                status: None,
                all_items,
                scoped_items: Vec::new(),
                scope: ModelScope::All,
            }),
        }
    }

    pub fn set_query(&self, query: &str) {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .set_query(query);
    }

    /// Install the `--models` scoped catalog (upstream `scopedModels`): the
    /// `Scope: all | scoped` row becomes available and the selector starts in
    /// the `scoped` scope, like upstream's constructor (`scope =
    /// scopedModels.length > 0 ? "scoped" : "all"`, model-selector.ts:94).
    /// An empty list leaves the selector scope-less (login warning row).
    pub fn set_scoped_models(&self, scoped_models: &[(String, String)]) {
        let mut container = self
            .container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let query = container.list.raw_query().to_string();
        let current = container
            .list
            .items()
            .iter()
            .find(|item| item.is_current)
            .map(|item| item.value.clone());
        let default = container
            .list
            .items()
            .iter()
            .find(|item| item.is_default)
            .map(|item| item.value.clone());
        container.scoped_items = model_items(scoped_models, current.as_deref(), default.as_deref());
        if container.scoped_items.is_empty() {
            return;
        }
        container.scope = ModelScope::Scoped;
        container.list = model_select_list(container.active_items());
        if !query.is_empty() {
            container.list.set_query(&query);
        }
    }

    /// The active catalog scope (upstream `scope`; test seam).
    pub fn scope(&self) -> ModelScope {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .scope
    }

    /// Whether the scope row exists (upstream `scopedModels.length > 0`).
    pub fn scope_available(&self) -> bool {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .scope_available()
    }

    /// `tui.input.tab` (upstream model-selector.ts:363-370): switch the
    /// catalog scope while the selector is open. Only a configured scope can
    /// toggle (upstream `scopedModelItems.length > 0`). Returns whether the
    /// scope changed. The new catalog re-selects the current model when it is
    /// present, else the first row (upstream `setScope`), keeping the active
    /// filter text.
    pub fn toggle_scope(&self) -> bool {
        let mut container = self
            .container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !container.scope_available() {
            return false;
        }
        let next = container.scope.toggled();
        if container.scope == next {
            return false;
        }
        container.scope = next;
        let query = container.list.raw_query().to_string();
        // `SelectList::new` selects the CURRENT model when the catalog
        // contains it, else the first row — upstream `setScope`'s
        // `selectedIndex = currentIndex >= 0 ? currentIndex : 0` followed by
        // `filterModels(searchInput.getValue())`.
        container.list = model_select_list(container.active_items());
        if !query.is_empty() {
            container.list.set_query(&query);
        }
        true
    }

    pub fn move_up(&self) {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .move_up();
    }

    pub fn move_down(&self) {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .move_down();
    }

    pub fn page_up(&self) {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .page_up();
    }

    pub fn page_down(&self) {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .page_down();
    }

    pub fn selected(&self) -> Option<String> {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .selected_item()
            .map(|item| item.value.clone())
    }

    pub fn set_status(&self, status: Option<&str>) {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .status = status.map(str::to_string);
    }

    /// W11-C main-screen mouse: `row` is a click row relative to the
    /// selector frame's first painted row. Rows landing on the visible list
    /// select the clicked item (upstream `SelectList.handleMouse` press);
    /// returns whether the click was consumed.
    pub fn click_row(&self, row: usize) -> bool {
        let mut container = self
            .container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Frame rows before the list: border, spacer, [scope + hint], spacer,
        // search input, spacer (model-selector.ts:100-128).
        let list_start = 5 + usize::from(container.scope_available()) * 2;
        let Some(row) = row.checked_sub(list_start) else {
            return false;
        };
        let Some(position) = container.list.position_at_row(row) else {
            return false;
        };
        container.list.select_position(position);
        true
    }
}

impl Component for ModelSelector {
    fn render(&self, width: usize) -> Vec<String> {
        let mut container = self
            .container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Rebuild the frame in upstream child order
        // (model-selector.ts:99-151).
        let mut frame = Container::new();
        frame.add_child(DynamicBorder::new()); // model-selector.ts:100
        frame.add_child(Spacer::new(1)); // model-selector.ts:101
        if container.scope_available() {
            // `Scope: all | scoped`, active scope in accent (upstream
            // `getScopeText`, model-selector.ts:248-252).
            let scope = container.scope;
            frame.add_child(Text::with_padding(
                with_theme(|theme| {
                    let text = |value: ModelScope| {
                        if value == scope {
                            theme.fg(ThemeColor::Accent, value.label())
                        } else {
                            theme.fg(ThemeColor::Muted, value.label())
                        }
                    };
                    format!(
                        "{}{}{}{}",
                        theme.fg(ThemeColor::Muted, "Scope: "),
                        text(ModelScope::All),
                        theme.fg(ThemeColor::Muted, " | "),
                        text(ModelScope::Scoped),
                    )
                }),
                0,
                0,
            ));
            // Tab hint (upstream `getScopeHintText`, model-selector.ts:254-256
            // via `keyHint`: dim key + muted description).
            frame.add_child(Text::with_padding(
                with_theme(|theme| {
                    format!(
                        "{}{}{}",
                        theme.fg(ThemeColor::Dim, "tab"),
                        theme.fg(ThemeColor::Muted, " scope"),
                        theme.fg(ThemeColor::Muted, " (all/scoped)"),
                    )
                }),
                0,
                0,
            ));
        } else {
            // No scope configured: login hint in warning color (upstream
            // model-selector.ts:110-111).
            frame.add_child(Text::with_padding(
                with_theme(|theme| {
                    theme.fg(
                        ThemeColor::Warning,
                        "Only showing models from configured providers. Use /login to add providers.",
                    )
                }),
                0,
                0,
            ));
        }
        frame.add_child(Spacer::new(1)); // model-selector.ts:113
                                         // Visible search input line (upstream searchInput,
                                         // model-selector.ts:116-126; `Input.render` input.ts:419-500).
        frame.add_child(Text::with_padding(
            search_input_line(container.list.raw_query(), width),
            0,
            0,
        ));
        frame.add_child(Spacer::new(1)); // model-selector.ts:128
        for line in container.list.render_lines(width) {
            frame.add_child(Text::with_padding(line, 0, 0));
        }
        if container.list.filtered_len() > 0 {
            // Highlighted entry detail (upstream model-selector.ts:350-352):
            // spacer + muted `  Model Name: …`. Pir carries no display name
            // in the selector state, so the item id stands in for the
            // upstream `model.name`.
            frame.add_child(Spacer::new(1));
            let name = container
                .list
                .selected_item()
                .map(|item| {
                    if item.description.is_empty() {
                        item.id.clone()
                    } else {
                        item.description.clone()
                    }
                })
                .unwrap_or_default();
            frame.add_child(Text::with_padding(
                with_theme(|theme| theme.fg(ThemeColor::Muted, &format!("  Model Name: {name}"))),
                0,
                0,
            ));
        }
        if let Some(status) = &container.status {
            // Refresh status (upstream model-selector.ts:354-358): spacer +
            // indented success/muted message inside the list region.
            frame.add_child(Spacer::new(1));
            frame.add_child(Text::with_padding(
                with_theme(|theme| theme.fg(ThemeColor::Success, &format!("  {status}"))),
                0,
                0,
            ));
        }
        frame.add_child(Spacer::new(1)); // model-selector.ts:134
                                         // Footer hint (upstream model-selector.ts:137-148, dim): Enter
                                         // selects for the session; ctrl+s (`app.models.save`) persists the
                                         // selection as the default model.
        frame.add_child(Text::with_padding(
            with_theme(|theme| {
                theme.fg(
                    ThemeColor::Dim,
                    "  Enter to select · ctrl+s to set as default · Escape to cancel",
                )
            }),
            0,
            0,
        ));
        frame.add_child(DynamicBorder::new()); // model-selector.ts:151
        container.frame = frame;
        container.frame.render(width)
    }

    fn invalidate(&self) {}
}

/// Thinking level descriptions (upstream `LEVEL_DESCRIPTIONS`).
pub const LEVEL_DESCRIPTIONS: [(&str, &str); 7] = [
    ("off", "No reasoning"),
    ("minimal", "Very brief reasoning (~1k tokens)"),
    ("low", "Light reasoning (~2k tokens)"),
    ("medium", "Moderate reasoning (~8k tokens)"),
    ("high", "Deep reasoning (~16k tokens)"),
    ("xhigh", "Extra-high reasoning (~32k tokens)"),
    ("max", "Maximum reasoning"),
];

/// The thinking-level selector (upstream `ThinkingSelectorComponent`).
pub struct ThinkingSelector {
    container: Mutex<SelectorFrame>,
}

struct SelectorFrame {
    frame: Container,
    list: SelectList<String>,
}

impl ThinkingSelector {
    pub fn new(current: &str, available: &[String], default: Option<&str>) -> Self {
        let items: Vec<SelectItem<String>> = available
            .iter()
            .map(|level| {
                let description = LEVEL_DESCRIPTIONS
                    .iter()
                    .find(|(name, _)| *name == level)
                    .map(|(_, description)| description.to_string())
                    .unwrap_or_default();
                let description = if default == Some(level.as_str()) {
                    format!("{description} · default")
                } else {
                    description
                };
                SelectItem {
                    value: level.clone(),
                    id: level.clone(),
                    provider: String::new(),
                    description,
                    is_current: level == current,
                    is_default: default == Some(level.as_str()),
                }
            })
            .collect();
        let mut frame = Container::new();
        frame.add_child(DynamicBorder::new());
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding("Thinking Level", 0, 0));
        frame.add_child(Spacer::new(1));
        Self {
            container: Mutex::new(SelectorFrame {
                frame,
                list: SelectList::new(items),
            }),
        }
    }

    /// Filter the level list and echo the raw text on the visible search
    /// line (upstream `applyFilter` + searchInput, thinking-selector.ts:83-85,
    /// 119-127, 147-148). Wiring gap: terminal.rs `handle_overlay_key` routes
    /// typed characters only to the model/tree selectors, so this stays
    /// empty until the call site is added.
    pub fn set_query(&self, query: &str) {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .set_query(query);
    }

    pub fn move_up(&self) {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .move_up();
    }

    pub fn move_down(&self) {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .move_down();
    }

    pub fn page_up(&self) {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .page_up();
    }

    pub fn page_down(&self) {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .page_down();
    }

    pub fn selected(&self) -> Option<String> {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .selected_item()
            .map(|item| item.value.clone())
    }

    /// W11-C main-screen mouse: select the level on click row `row`
    /// (relative to the selector frame); consumed when the row is an item.
    pub fn click_row(&self, row: usize) -> bool {
        let mut container = self
            .container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Border, spacer, title, spacer, cycle hint, spacer, search, spacer
        // (thinking-selector.ts:76-86).
        let Some(row) = row.checked_sub(8) else {
            return false;
        };
        let Some(position) = container.list.position_at_row(row) else {
            return false;
        };
        container.list.select_position(position);
        true
    }
}

impl Component for ThinkingSelector {
    fn render(&self, width: usize) -> Vec<String> {
        let mut container = self
            .container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Rebuild the frame in upstream child order
        // (thinking-selector.ts:75-105).
        let mut frame = Container::new();
        frame.add_child(DynamicBorder::new()); // thinking-selector.ts:76
        frame.add_child(Spacer::new(1)); // thinking-selector.ts:77
        frame.add_child(Text::with_padding("Thinking Level", 0, 0)); // thinking-selector.ts:78
        frame.add_child(Spacer::new(1)); // thinking-selector.ts:79
                                         // In-session cycle hint, plain text (upstream
                                         // thinking-selector.ts:80; `keyDisplayText("app.thinking.cycle")`
                                         // renders the default `shift+tab` binding as `Shift+Tab`).
        frame.add_child(Text::with_padding(
            "Shift+Tab cycles thinking levels in-session",
            0,
            0,
        ));
        frame.add_child(Spacer::new(1)); // thinking-selector.ts:81
                                         // Visible search input line (upstream searchInput,
                                         // thinking-selector.ts:83-85).
        frame.add_child(Text::with_padding(
            search_input_line(container.list.raw_query(), width),
            0,
            0,
        ));
        frame.add_child(Spacer::new(1)); // thinking-selector.ts:86
        for line in container.list.render_lines(width) {
            frame.add_child(Text::with_padding(line, 0, 0));
        }
        frame.add_child(Spacer::new(1)); // thinking-selector.ts:92
                                         // Footer hint (upstream thinking-selector.ts:93-102, dim): Enter
                                         // applies for the session; ctrl+s (`app.thinking.save`) persists
                                         // the level as the default.
        frame.add_child(Text::with_padding(
            with_theme(|theme| {
                theme.fg(
                    ThemeColor::Dim,
                    "  Enter to select · ctrl+s to set as default · Escape to cancel",
                )
            }),
            0,
            0,
        ));
        frame.add_child(DynamicBorder::new()); // thinking-selector.ts:105
        container.frame = frame;
        container.frame.render(width)
    }

    fn invalidate(&self) {}
}

/// One `/tree` entry row (upstream `SessionTreeNode` reduced to pir's display
/// rows): entry id, display text, the raw label (None = unlabeled), the label
/// timestamp, the entry kind the filter modes act on, and the tree position
/// (`depth` + `parent_id`) that drives indentation and folding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub id: String,
    pub display: String,
    pub label: Option<String>,
    pub label_timestamp: Option<String>,
    pub kind: TreeEntryKind,
    /// Depth from the root (0 for a root).
    pub depth: usize,
    /// The resolved parent entry id (`None` for a root).
    pub parent_id: Option<String>,
}

impl TreeEntry {
    /// A depth-0, parentless row (the shape a flat/manual listing uses).
    pub fn root(id: impl Into<String>, display: impl Into<String>, kind: TreeEntryKind) -> Self {
        Self {
            id: id.into(),
            display: display.into(),
            label: None,
            label_timestamp: None,
            kind,
            depth: 0,
            parent_id: None,
        }
    }
}

/// The entry classification behind the `/tree` filter modes (upstream
/// `FilterMode` semantics: "bookkeeping" entries — label changes, context
/// edits, custom entries, model/thinking changes — are hidden by default,
/// usage entries are always hidden at the source).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeEntryKind {
    User,
    Assistant,
    Tool,
    Compaction,
    /// Label/context-edit/custom/model-change/thinking-change entries.
    Bookkeeping,
    /// Everything else (custom messages, unknown shapes).
    Other,
}

impl TreeEntryKind {
    fn as_str(self) -> &'static str {
        match self {
            TreeEntryKind::User => "user",
            TreeEntryKind::Assistant => "assistant",
            TreeEntryKind::Tool => "tool",
            TreeEntryKind::Compaction => "compaction",
            TreeEntryKind::Bookkeeping => "bookkeeping",
            TreeEntryKind::Other => "other",
        }
    }
}

/// The `/tree` filter modes (upstream `FilterMode`, driven by the
/// `treeFilterMode` setting and the `app.tree.filter.*` keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeFilterMode {
    Default,
    NoTools,
    UserOnly,
    LabeledOnly,
    All,
}

impl TreeFilterMode {
    /// The upstream cycle order: default → no-tools → user-only →
    /// labeled-only → all → default.
    pub const CYCLE: [TreeFilterMode; 5] = [
        TreeFilterMode::Default,
        TreeFilterMode::NoTools,
        TreeFilterMode::UserOnly,
        TreeFilterMode::LabeledOnly,
        TreeFilterMode::All,
    ];

    pub fn next(self) -> Self {
        let index = Self::CYCLE
            .iter()
            .position(|mode| *mode == self)
            .unwrap_or(0);
        Self::CYCLE[(index + 1) % Self::CYCLE.len()]
    }

    pub fn previous(self) -> Self {
        let index = Self::CYCLE
            .iter()
            .position(|mode| *mode == self)
            .unwrap_or(0);
        Self::CYCLE[(index + Self::CYCLE.len() - 1) % Self::CYCLE.len()]
    }

    /// The status label suffix (upstream `getStatusLabels`).
    pub fn label(self) -> &'static str {
        match self {
            TreeFilterMode::Default => "",
            TreeFilterMode::NoTools => " [no-tools]",
            TreeFilterMode::UserOnly => " [user]",
            TreeFilterMode::LabeledOnly => " [labeled]",
            TreeFilterMode::All => " [all]",
        }
    }

    /// From the settings value (the `/tree` initial filter, upstream
    /// `initialFilterMode`).
    pub fn from_settings(mode: crate::config::settings::TreeFilterMode) -> Self {
        match mode {
            crate::config::settings::TreeFilterMode::Default => Self::Default,
            crate::config::settings::TreeFilterMode::NoTools => Self::NoTools,
            crate::config::settings::TreeFilterMode::UserOnly => Self::UserOnly,
            crate::config::settings::TreeFilterMode::LabeledOnly => Self::LabeledOnly,
            crate::config::settings::TreeFilterMode::All => Self::All,
        }
    }

    fn passes(self, entry: &TreeEntry) -> bool {
        match self {
            TreeFilterMode::All => true,
            TreeFilterMode::Default => entry.kind != TreeEntryKind::Bookkeeping,
            TreeFilterMode::NoTools => {
                entry.kind != TreeEntryKind::Bookkeeping && entry.kind != TreeEntryKind::Tool
            }
            TreeFilterMode::UserOnly => entry.kind == TreeEntryKind::User,
            TreeFilterMode::LabeledOnly => entry.label.is_some(),
        }
    }
}

/// Parse the tree filter mode from a settings.json raw value (worker-side
/// convenience; mirrors `TreeFilterMode::from_settings`).
pub fn tree_filter_mode_from_value(value: Option<&serde_json::Value>) -> TreeFilterMode {
    value
        .and_then(|value| {
            serde_json::from_value::<crate::config::settings::TreeFilterMode>(value.clone()).ok()
        })
        .map(TreeFilterMode::from_settings)
        .unwrap_or(TreeFilterMode::Default)
}

/// The session-tree selector (upstream `TreeSelectorComponent`): pick a
/// previous entry to branch the session from; `/tree` opens it. pir's tree
/// is a flat windowed list (upstream renders a real tree with fold
/// navigation); the filter modes, label display, and label-timestamp toggle
/// carry the upstream semantics.
pub struct TreeSelector {
    state: Mutex<TreeListState>,
}

struct TreeListState {
    entries: Vec<TreeEntry>,
    /// Indices into `entries` after the filter + query + fold pass.
    visible: Vec<usize>,
    selected: usize,
    query: String,
    filter_mode: TreeFilterMode,
    show_label_timestamps: bool,
    /// Entry ids whose subtree is collapsed (upstream tree fold state).
    collapsed: std::collections::BTreeSet<String>,
}

impl TreeSelector {
    /// `entries`: the flattened (id, display, label, timestamp, kind) rows.
    /// `initial_filter_mode`: the `treeFilterMode` setting (upstream
    /// `initialFilterMode`).
    pub fn new(entries: &[TreeEntry], initial_filter_mode: TreeFilterMode) -> Self {
        let mut state = TreeListState {
            entries: entries.to_vec(),
            visible: Vec::new(),
            selected: 0,
            query: String::new(),
            filter_mode: initial_filter_mode,
            show_label_timestamps: false,
            collapsed: std::collections::BTreeSet::new(),
        };
        Self::apply_filter(&mut state);
        Self {
            state: Mutex::new(state),
        }
    }

    /// Re-run the filter-mode + query + fold pass over the entries (upstream
    /// `applyFilter`; the selection keeps pointing at the same entry when
    /// possible, else clamps).
    fn apply_filter(state: &mut TreeListState) {
        let selected_id = state
            .visible
            .get(state.selected)
            .and_then(|index| state.entries.get(*index))
            .map(|entry| entry.id.clone());
        // Entries hidden because an ancestor is collapsed: entries arrive in
        // DFS pre-order, so a parent is always seen before its children.
        let mut folded_away: std::collections::HashSet<String> = std::collections::HashSet::new();
        for entry in &state.entries {
            if let Some(parent) = &entry.parent_id {
                if state.collapsed.contains(parent) || folded_away.contains(parent) {
                    folded_away.insert(entry.id.clone());
                }
            }
        }
        let query = state.query.to_lowercase();
        let tokens: Vec<&str> = query.split_whitespace().collect();
        state.visible = state
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                if folded_away.contains(&entry.id) {
                    return false;
                }
                if !state.filter_mode.passes(entry) {
                    return false;
                }
                if tokens.is_empty() {
                    return true;
                }
                let mut text = entry.display.clone();
                if let Some(label) = &entry.label {
                    text.push(' ');
                    text.push_str(label);
                }
                text.push(' ');
                text.push_str(entry.kind.as_str());
                let text = text.to_lowercase();
                tokens.iter().all(|token| text.contains(token))
            })
            .map(|(index, _)| index)
            .collect();
        state.selected = selected_id
            .and_then(|id| {
                state.visible.iter().position(|index| {
                    state
                        .entries
                        .get(*index)
                        .is_some_and(|entry| entry.id == id)
                })
            })
            .unwrap_or_else(|| state.selected.min(state.visible.len().saturating_sub(1)));
    }

    pub fn set_query(&self, query: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.query = query.to_string();
        Self::apply_filter(&mut state);
    }

    pub fn move_up(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.visible.is_empty() {
            return;
        }
        state.selected = if state.selected == 0 {
            state.visible.len() - 1
        } else {
            state.selected - 1
        };
    }

    pub fn move_down(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.visible.is_empty() {
            return;
        }
        state.selected = if state.selected + 1 >= state.visible.len() {
            0
        } else {
            state.selected + 1
        };
    }

    /// Does the entry have any children (listed or not)?
    fn has_children(state: &TreeListState, id: &str) -> bool {
        state
            .entries
            .iter()
            .any(|entry| entry.parent_id.as_deref() == Some(id))
    }

    /// Is the entry's subtree collapsed?
    pub fn is_collapsed(&self, entry_id: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .collapsed
            .contains(entry_id)
    }

    /// `app.tree.foldOrUp` (upstream `treeFoldOrUp`): collapse the selected
    /// entry when it has children and is expanded; otherwise move up one row.
    pub fn fold_or_up(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let selected_id = state
            .visible
            .get(state.selected)
            .and_then(|index| state.entries.get(*index))
            .map(|entry| entry.id.clone());
        if let Some(id) = selected_id {
            if Self::has_children(&state, &id) && !state.collapsed.contains(&id) {
                state.collapsed.insert(id);
                Self::apply_filter(&mut state);
                return;
            }
        }
        if !state.visible.is_empty() {
            state.selected = if state.selected == 0 {
                state.visible.len() - 1
            } else {
                state.selected - 1
            };
        }
    }

    /// `app.tree.unfoldOrDown` (upstream `treeUnfoldOrDown`): expand the
    /// selected entry when it is collapsed; otherwise move down one row.
    pub fn unfold_or_down(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let selected_id = state
            .visible
            .get(state.selected)
            .and_then(|index| state.entries.get(*index))
            .map(|entry| entry.id.clone());
        if let Some(id) = selected_id {
            if state.collapsed.remove(&id) {
                Self::apply_filter(&mut state);
                return;
            }
        }
        if !state.visible.is_empty() {
            state.selected = if state.selected + 1 >= state.visible.len() {
                0
            } else {
                state.selected + 1
            };
        }
    }

    /// Set one entry's fold state directly (tests / restored selection).
    pub fn set_collapsed(&self, entry_id: &str, collapsed: bool) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if collapsed {
            state.collapsed.insert(entry_id.to_string());
        } else {
            state.collapsed.remove(entry_id);
        }
        Self::apply_filter(&mut state);
    }

    /// Number of rows currently shown (folded subtrees excluded).
    pub fn visible_len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .visible
            .len()
    }

    pub fn page_up(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.selected = state.selected.saturating_sub(10);
    }

    pub fn page_down(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.visible.is_empty() {
            state.selected = (state.selected + 10).min(state.visible.len() - 1);
        }
    }

    /// Select a specific entry id (upstream `showTreeSelector(initialSelectedId)`:
    /// the branch-summary abort re-opens the tree on the entry the user had
    /// picked). Returns whether the entry is visible under the current filter
    /// (a hidden entry keeps the previous selection).
    pub fn select_id(&self, entry_id: &str) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let found = state.visible.iter().position(|index| {
            state
                .entries
                .get(*index)
                .is_some_and(|entry| entry.id == entry_id)
        });
        match found {
            Some(position) => {
                state.selected = position;
                true
            }
            None => false,
        }
    }

    /// The selected entry id (branch target).
    pub fn selected(&self) -> Option<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .visible
            .get(state.selected)
            .and_then(|index| state.entries.get(*index))
            .map(|entry| entry.id.clone())
    }

    /// The selected entry's raw label + display text (the label editor
    /// prefill; upstream `onLabelEdit(entryId, currentLabel)`).
    pub fn selected_label(&self) -> Option<(String, Option<String>)> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .visible
            .get(state.selected)
            .and_then(|index| state.entries.get(*index))
            .map(|entry| (entry.id.clone(), entry.label.clone()))
    }

    /// Update one entry's label after a save (upstream
    /// `updateNodeLabel`); a `None` label stamps the clear time.
    pub fn set_label(&self, entry_id: &str, label: Option<String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for entry in &mut state.entries {
            if entry.id == entry_id {
                entry.label = label.clone();
                entry.label_timestamp = label.as_ref().map(|_| unix_timestamp_text());
            }
        }
        Self::apply_filter(&mut state);
    }

    /// Set the filter mode directly (upstream `app.tree.filter.default`).
    pub fn set_filter_mode(&self, mode: TreeFilterMode) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.filter_mode = mode;
        Self::apply_filter(&mut state);
    }

    pub fn filter_mode(&self) -> TreeFilterMode {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .filter_mode
    }

    /// Toggle the label-timestamp display (upstream
    /// `app.tree.toggleLabelTimestamp`).
    pub fn toggle_label_timestamps(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.show_label_timestamps = !state.show_label_timestamps;
    }

    pub fn shows_label_timestamps(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .show_label_timestamps
    }

    /// The visible window `(start, end)` over the filtered entries (upstream
    /// `MAX_VISIBLE` window rule), shared by the renderer and the mouse
    /// hit-test.
    fn visible_range(state: &TreeListState) -> (usize, usize) {
        const MAX_VISIBLE: usize = 10;
        let start = state.selected.saturating_sub(MAX_VISIBLE / 2).min(
            state
                .visible
                .len()
                .saturating_sub(MAX_VISIBLE.min(state.visible.len())),
        );
        (start, (start + MAX_VISIBLE).min(state.visible.len()))
    }

    /// W11-C main-screen mouse: select the entry on click row `row`
    /// (relative to the selector's first painted row; the three header rows
    /// are not entries). Consumed when the row is a visible entry.
    pub fn click_row(&self, row: usize) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        const HEADER_ROWS: usize = 3;
        let Some(offset) = row.checked_sub(HEADER_ROWS) else {
            return false;
        };
        let (start, end) = Self::visible_range(&state);
        let index = start + offset;
        if index >= end {
            return false;
        }
        state.selected = index;
        true
    }
}

impl Component for TreeSelector {
    fn render(&self, width: usize) -> Vec<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut lines = vec![
            with_theme(|theme| theme.fg(ThemeColor::Accent, &theme.bold("Session Tree"))),
            with_theme(|theme| {
                theme.fg(
                    ThemeColor::Muted,
                    "Enter to branch from this point · shift+l edit label · ctrl+o cycle filter · ctrl+←/→ fold · Esc to cancel",
                )
            }),
            if state.query.is_empty() {
                with_theme(|theme| theme.fg(ThemeColor::Muted, "Type to search:"))
            } else {
                with_theme(|theme| {
                    theme.fg(
                        ThemeColor::Muted,
                        &format!(
                            "Type to search: {}",
                            theme.fg(ThemeColor::Accent, &state.query)
                        ),
                    )
                })
            },
        ];
        if state.visible.is_empty() {
            lines.push(with_theme(|theme| {
                theme.fg(ThemeColor::Muted, "  No entries found")
            }));
            lines.push(with_theme(|theme| theme.fg(ThemeColor::Muted, "  (0/0)")));
            return lines;
        }
        const MAX_VISIBLE: usize = 10;
        let start = state.selected.saturating_sub(MAX_VISIBLE / 2).min(
            state
                .visible
                .len()
                .saturating_sub(MAX_VISIBLE.min(state.visible.len())),
        );
        let end = (start + MAX_VISIBLE).min(state.visible.len());
        for index in start..end {
            let entry_index = state.visible[index];
            let entry = &state.entries[entry_index];
            let is_selected = index == state.selected;
            let cursor = if is_selected {
                with_theme(|theme| theme.fg(ThemeColor::Accent, "→ "))
            } else {
                "  ".to_string()
            };
            let mut row = String::new();
            // Tree indentation + fold marker (upstream `tree-selector.ts`
            // renders the hierarchy with indentation and a fold glyph; pir
            // previously rendered a flat list).
            row.push_str(&"  ".repeat(entry.depth.min(32)));
            if Self::has_children(&state, &entry.id) {
                row.push_str(if state.collapsed.contains(&entry.id) {
                    "▸ "
                } else {
                    "▾ "
                });
            } else {
                row.push_str("  ");
            }
            if let Some(label) = &entry.label {
                let timestamp = if state.show_label_timestamps {
                    entry
                        .label_timestamp
                        .as_deref()
                        .map(|stamp| format!(" {stamp}"))
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                row.push_str(&format!("[{label}]{timestamp} "));
            }
            row.push_str(&entry.display);
            let line = format!("{cursor}{row}");
            lines.push(truncate_to_width(
                &if is_selected {
                    with_theme(|theme| theme.fg(ThemeColor::Accent, &line))
                } else {
                    line
                },
                width,
                "…",
            ));
        }
        lines.push(with_theme(|theme| {
            theme.fg(
                ThemeColor::Muted,
                &format!(
                    "  ({}/{}){}{}",
                    state.selected + 1,
                    state.visible.len(),
                    state.filter_mode.label(),
                    if state.show_label_timestamps {
                        " [+label time]"
                    } else {
                        ""
                    },
                ),
            )
        }));
        lines
    }

    fn invalidate(&self) {}
}

/// The `/fork` user-message selector (upstream `UserMessageSelectorComponent`
/// with `showUserMessageSelector`): pick a previous user message to branch
/// the new session from.
pub struct ForkSelector {
    container: Mutex<SelectorFrame>,
}

impl ForkSelector {
    /// `messages`: `(entry_id, text)` pairs, oldest first (upstream
    /// `getUserMessagesForForking` order; the selection defaults to the last
    /// message, upstream `initialSelectedId`).
    pub fn new(messages: &[(String, String)]) -> Self {
        let last_id = messages.last().map(|(id, _)| id.clone());
        let items: Vec<SelectItem<String>> = messages
            .iter()
            .map(|(id, text)| {
                let display = text.replace(['\n', '\t'], " ");
                SelectItem {
                    value: id.clone(),
                    id: display,
                    provider: String::new(),
                    description: String::new(),
                    // Upstream `initialSelectedId` is the LAST user message
                    // (interactive-mode.ts:5508): mark it current so the
                    // selection defaults there instead of the first entry.
                    is_current: last_id.as_deref() == Some(id.as_str()),
                    is_default: false,
                }
            })
            .collect();
        let mut frame = Container::new();
        frame.add_child(DynamicBorder::new());
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding("Fork from message", 0, 0));
        frame.add_child(Spacer::new(1));
        Self {
            container: Mutex::new(SelectorFrame {
                frame,
                list: SelectList::new(items),
            }),
        }
    }

    pub fn set_query(&self, query: &str) {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .set_query(query);
    }

    pub fn move_up(&self) {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .move_up();
    }

    pub fn move_down(&self) {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .move_down();
    }

    pub fn page_up(&self) {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .page_up();
    }

    pub fn page_down(&self) {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .page_down();
    }

    /// The selected user-message entry id.
    pub fn selected(&self) -> Option<String> {
        self.container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .list
            .selected_item()
            .map(|item| item.value.clone())
    }

    /// W11-C main-screen mouse: select the message on click row `row`
    /// (relative to the selector frame; border/spacer/title/spacer/search/
    /// spacer precede the list).
    pub fn click_row(&self, row: usize) -> bool {
        let mut container = self
            .container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(row) = row.checked_sub(6) else {
            return false;
        };
        let Some(position) = container.list.position_at_row(row) else {
            return false;
        };
        container.list.select_position(position);
        true
    }
}

impl Component for ForkSelector {
    fn render(&self, width: usize) -> Vec<String> {
        let mut container = self
            .container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut frame = Container::new();
        frame.add_child(DynamicBorder::new());
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding("Fork from message", 0, 0));
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding(
            search_input_line(container.list.raw_query(), width),
            0,
            0,
        ));
        frame.add_child(Spacer::new(1));
        for line in container.list.render_lines(width) {
            frame.add_child(Text::with_padding(line, 0, 0));
        }
        frame.add_child(Spacer::new(1));
        frame.add_child(Text::with_padding(
            with_theme(|theme| {
                theme.fg(
                    ThemeColor::Dim,
                    "  Enter to fork from this message · Esc to cancel",
                )
            }),
            0,
            0,
        ));
        frame.add_child(DynamicBorder::new());
        container.frame = frame;
        container.frame.render(width)
    }

    fn invalidate(&self) {}
}

/// `HH:MM` label-timestamp text from the unix epoch seconds (upstream
/// `formatLabelTimestamp`'s same-day shape; the older shapes need a local
/// timezone table pir does not carry here).
fn unix_timestamp_text() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let minutes_total = secs / 60;
    let hour = (minutes_total / 60) % 24;
    let minute = minutes_total % 60;
    format!("{hour:02}:{minute:02}")
}

/// Outcome of one label-editor key press (upstream `LabelInput`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelEditOutcome {
    None,
    /// Save the label (`None` = empty input clears it).
    Submit(Option<String>),
    /// Cancel without changing anything.
    Cancel,
}

/// The `/tree` label editor (upstream `LabelInput`): a one-line input seeded
/// with the current label; Enter saves (empty clears), Esc cancels.
pub struct TreeLabelEditor {
    state: Mutex<LabelEditState>,
}

struct LabelEditState {
    entry_id: String,
    buffer: super::editor::Editor,
}

impl TreeLabelEditor {
    pub fn new(entry_id: String, current_label: Option<&str>) -> Self {
        let buffer = super::editor::Editor::new();
        buffer.set_text(current_label.unwrap_or_default());
        Self {
            state: Mutex::new(LabelEditState { entry_id, buffer }),
        }
    }

    /// The entry being relabeled (test seam).
    pub fn entry_id(&self) -> String {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry_id
            .clone()
    }

    /// The buffer text (test seam).
    pub fn text(&self) -> String {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .buffer
            .get_text()
    }

    /// Handle one dialog key (the same mapping the extension input dialogs
    /// use). Enter submits; empty input clears the label.
    pub fn handle_key(&self, key: super::dialogs::DialogKey) -> LabelEditOutcome {
        use super::dialogs::DialogKey;
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match key {
            DialogKey::Enter => {
                let text = state.buffer.get_text();
                let trimmed = text.trim();
                LabelEditOutcome::Submit(if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                })
            }
            DialogKey::Esc => LabelEditOutcome::Cancel,
            DialogKey::Left => {
                state.buffer.move_cursor_left();
                LabelEditOutcome::None
            }
            DialogKey::Right => {
                state.buffer.move_cursor_right();
                LabelEditOutcome::None
            }
            DialogKey::Backspace => {
                state.buffer.backspace();
                LabelEditOutcome::None
            }
            DialogKey::Char(c) => {
                state.buffer.insert_text(&c.to_string());
                LabelEditOutcome::None
            }
            DialogKey::Up | DialogKey::Down | DialogKey::Newline => LabelEditOutcome::None,
            DialogKey::Home => {
                state.buffer.move_cursor_home();
                LabelEditOutcome::None
            }
            DialogKey::End => {
                state.buffer.move_cursor_end();
                LabelEditOutcome::None
            }
            DialogKey::Delete => {
                state.buffer.delete_char_forward();
                LabelEditOutcome::None
            }
            DialogKey::WordLeft => {
                state.buffer.move_word_left();
                LabelEditOutcome::None
            }
            DialogKey::WordRight => {
                state.buffer.move_word_right();
                LabelEditOutcome::None
            }
            DialogKey::DeleteWordBackward => {
                state.buffer.delete_word_backward();
                LabelEditOutcome::None
            }
            DialogKey::DeleteWordForward => {
                state.buffer.delete_word_forward();
                LabelEditOutcome::None
            }
            DialogKey::DeleteToLineStart => {
                state.buffer.delete_to_line_start();
                LabelEditOutcome::None
            }
            DialogKey::DeleteToLineEnd => {
                state.buffer.delete_to_line_end();
                LabelEditOutcome::None
            }
            DialogKey::Undo => {
                state.buffer.undo();
                LabelEditOutcome::None
            }
            DialogKey::Yank => {
                state.buffer.yank();
                LabelEditOutcome::None
            }
            DialogKey::YankPop => {
                state.buffer.yank_pop();
                LabelEditOutcome::None
            }
        }
    }
}

impl Component for TreeLabelEditor {
    fn render(&self, width: usize) -> Vec<String> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut lines = vec![with_theme(|theme| {
            theme.fg(ThemeColor::Muted, "  Label (empty to remove):")
        })];
        lines.extend(state.buffer.render(width.saturating_sub(2)));
        lines.push(with_theme(|theme| {
            theme.fg(ThemeColor::Dim, "  enter save · esc cancel")
        }));
        lines
    }

    fn invalidate(&self) {}
}
