//! Editor autocomplete — port of upstream `packages/tui/src/autocomplete.ts`
//! and `fuzzy.ts`.
//!
//! Two completion surfaces, matching the upstream `CombinedAutocompleteProvider`:
//!
//! - **Slash commands**: typing `/…` at the start of a line completes against
//!   a pluggable command list (see [`CommandProvider`]). Built-in commands are
//!   served by the default provider; extension-registered commands are
//!   injected through the global hook [`register_extension_command_provider`]
//!   (see its doc comment — that is the documented injection point for the
//!   extensions stream).
//! - **File paths**: cwd-relative path tokens (`src/`, `./`, `@file`,
//!   `"quoted paths"`) trigger a bounded directory scan (skipped dirs:
//!   `node_modules`/`target`/`.git`/`dist`/`build`, entry cap
//!   [`MAX_SCAN_ENTRIES`], depth cap [`MAX_SCAN_DEPTH`]) — this replaces the
//!   upstream `fd` subprocess so no external tool is required (deviation).
//!
//! The fuzzy matcher is a direct transcription of upstream `fuzzyMatch` /
//! `fuzzyFilter` scoring (subsequence with consecutive-run, gap and word
//! boundary bonuses/penalties, exact-match bonus, swapped letters/digits
//! fallback). Lower score = better match, exactly like upstream.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use super::fuzzy::fuzzy_filter;

/// Characters that end/start a path token (upstream `PATH_DELIMITERS`).
const PATH_DELIMITERS: [char; 5] = [' ', '\t', '"', '\'', '='];

/// Maximum entries collected by one directory scan.
pub const MAX_SCAN_ENTRIES: usize = 200;

/// Maximum recursion depth of the bounded directory scan.
pub const MAX_SCAN_DEPTH: usize = 8;

/// Maximum fuzzy file suggestions returned (upstream `topEntries` cap).
const MAX_FUZZY_SUGGESTIONS: usize = 20;

/// Natural autocomplete trigger characters (upstream
/// `DEFAULT_AUTOCOMPLETE_TRIGGER_CHARACTERS`, `editor.ts:254`). `@` opens
/// file attachment completion; `#` is recognized by the editor as a trigger
/// character but the built-in combined provider serves no suggestions for
/// `#` tokens (upstream parity — only extension wrapper providers do, which
/// pir does not have), so typing `#` never opens the popup on its own.
pub const DEFAULT_TRIGGER_CHARACTERS: [char; 2] = ['@', '#'];

/// Directories never descended into during scans.
const SKIPPED_DIRS: [&str; 5] = ["node_modules", "target", ".git", "dist", "build"];

/// One suggestion row (upstream `AutocompleteItem`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteItem {
    /// Value inserted into the editor on accept.
    pub value: String,
    /// Label shown in the popup (file name, command name, …).
    pub label: String,
    /// Optional hint shown after the label (path, description, …).
    pub description: Option<String>,
}

impl AutocompleteItem {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            description: None,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// A suggestion set plus the prefix being completed (upstream
/// `AutocompleteSuggestions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutocompleteSuggestions {
    pub items: Vec<AutocompleteItem>,
    /// What we are matching against, e.g. `/mo`, `src/`, `@fi`.
    pub prefix: String,
}

// ---------------------------------------------------------------------------
// Native (`CompletionService`) merge
//
// The extension dispatcher converts the JS reply (already wire JSON) and the
// native `AutocompleteItem`s into this module's popup item, merges them with
// the SAME first-value-wins dedupe and prefix rules the JS-only path used, and
// emits the merged list as the existing `WorkerEvent::AutocompleteItems` wire
// value. Native items always come AFTER the JS items (the JS chain keeps its
// load-order precedence). An empty native chain leaves the JS list (and its
// byte-for-byte behavior) untouched.
// ---------------------------------------------------------------------------

/// Convert one wire completion item (`{value, label?, description?}`) into the
/// popup item, exactly as the TUI's merge always has.
pub fn autocomplete_item_from_json(item: &serde_json::Value) -> Option<AutocompleteItem> {
    let value = item.get("value")?.as_str()?;
    let label = item
        .get("label")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(value);
    let mut converted = AutocompleteItem::new(value, label);
    if let Some(description) = item.get("description").and_then(serde_json::Value::as_str) {
        converted = converted.with_description(description);
    }
    Some(converted)
}

/// Serialize one popup item back into the completion wire shape (the shape
/// `WorkerEvent::AutocompleteItems` carries).
pub fn autocomplete_item_to_json(item: &AutocompleteItem) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    object.insert(
        "value".to_owned(),
        serde_json::Value::String(item.value.clone()),
    );
    object.insert(
        "label".to_owned(),
        serde_json::Value::String(item.label.clone()),
    );
    if let Some(description) = &item.description {
        object.insert(
            "description".to_owned(),
            serde_json::Value::String(description.clone()),
        );
    }
    serde_json::Value::Object(object)
}

/// Convert a native service item into the popup item (same fields, typed).
pub fn autocomplete_item_from_service(
    item: crate::plugins::plugin::services::AutocompleteItem,
) -> AutocompleteItem {
    let mut converted = AutocompleteItem::new(item.value, item.label);
    converted.description = item.description;
    converted
}

/// Merge the native chain AFTER the JS chain: the first item per `value`
/// wins, preserving each chain's order (`js` first, then unseen `native`
/// rows). This is the TUI's prefix dedupe applied across faces.
pub fn merge_suggestion_items(
    js: Vec<AutocompleteItem>,
    native: Vec<AutocompleteItem>,
) -> Vec<AutocompleteItem> {
    if native.is_empty() {
        return js;
    }
    let mut seen: std::collections::HashSet<String> =
        js.iter().map(|item| item.value.clone()).collect();
    let mut merged = js;
    for item in native {
        if seen.insert(item.value.clone()) {
            merged.push(item);
        }
    }
    merged
}

/// Whether a completion reply still matches the editor line before the cursor
/// (the stale-response drop the JS path applies): the reply's `prefix` must
/// equal the whole editor prefix OR the argument piece after `/command `.
pub fn completion_response_is_current(before: &str, response_prefix: &str) -> bool {
    let argument = before
        .split_once(' ')
        .map(|(_, argument)| argument)
        .unwrap_or("");
    before == response_prefix || argument == response_prefix
}

/// A slash command (upstream `SlashCommand`, minus the async
/// `getArgumentCompletions` hook — pir does not yet complete arguments).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SlashCommand {
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
}

impl SlashCommand {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: Some(description.into()),
            argument_hint: None,
        }
    }

    /// Attach an argument hint (upstream `argumentHint`).
    pub fn with_argument_hint(mut self, hint: impl Into<String>) -> Self {
        self.argument_hint = Some(hint.into());
        self
    }

    fn to_item(&self) -> AutocompleteItem {
        let desc = self
            .argument_hint
            .as_deref()
            .map(|hint| match &self.description {
                Some(desc) => format!("{hint} — {desc}"),
                None => hint.to_string(),
            })
            .or_else(|| self.description.clone());
        let mut item = AutocompleteItem::new(self.name.clone(), self.name.clone());
        item.description = desc;
        item
    }
}

/// Source of the current slash-command list (upstream command array).
///
/// Implement this to supply (or extend) the commands offered by the
/// autocomplete popup. The default implementation serves the built-in
/// pir commands; see [`register_extension_command_provider`] for how
/// extension-registered commands join the list at runtime.
pub trait CommandProvider: Send + Sync {
    fn commands(&self) -> Vec<SlashCommand>;
}

/// The built-in slash commands (mirrors `terminal::handle_command`; the
/// upstream `BUILTIN_SLASH_COMMANDS` subset pir implements).
pub fn builtin_commands() -> Vec<SlashCommand> {
    vec![
        SlashCommand::new("model", "Select a model"),
        SlashCommand::new("thinking", "Set the thinking level"),
        SlashCommand::new("clear", "Clear the conversation"),
        SlashCommand::new("compact", "Manually compact the session context")
            .with_argument_hint("[instructions]"),
        SlashCommand::new("export", "Export session to HTML").with_argument_hint("<path>"),
        SlashCommand::new("new", "Start a new session"),
        SlashCommand::new("resume", "Resume a different session"),
        SlashCommand::new("fork", "Fork the current session"),
        SlashCommand::new("clone", "Duplicate the current session"),
        SlashCommand::new("name", "Set session display name").with_argument_hint("<name>"),
        SlashCommand::new("session", "Show session info and stats"),
        SlashCommand::new("sessions", "List and switch same-window session tabs")
            .with_argument_hint("[new|next|prev|close|<index>|<path>]"),
        SlashCommand::new("copy", "Copy last agent message to clipboard"),
        SlashCommand::new("settings", "Show the effective settings"),
        SlashCommand::new("tree", "Browse the session tree and branch"),
        SlashCommand::new("scoped-models", "Show the --models scope in effect"),
        SlashCommand::new("import", "Import and resume a session from a JSONL file"),
        // Upstream `slash-commands.ts` rewrote this description with
        // `cea257439` (D31), once `sessionShareDestination` made the target
        // configurable.
        SlashCommand::new(
            "share",
            "Share session via a Radius artifact or a GitHub gist",
        ),
        SlashCommand::new("bug", "Report a bug to the Pi developers")
            .with_argument_hint("<description>"),
        SlashCommand::new("trust", "Show the project trust state"),
        SlashCommand::new("login", "OAuth login (opens a provider dialog upstream)")
            .with_argument_hint("[provider]"),
        SlashCommand::new("logout", "OAuth logout (opens a provider dialog upstream)"),
        SlashCommand::new("changelog", "Show what's new"),
        SlashCommand::new("hotkeys", "Show the default keybindings"),
        SlashCommand::new("reload", "Reload extensions"),
        SlashCommand::new("debug", "Toggle debug mode and show run timings")
            .with_argument_hint("[enable|disable|last]"),
        SlashCommand::new("quit", "Quit pir"),
        SlashCommand::new("help", "Show available commands"),
    ]
}

/// INJECTION POINT for extension-registered slash commands.
///
/// A later stream that loads extensions can call this once its command list
/// is known (and again whenever it changes) with an
/// [`Arc<dyn CommandProvider>`] of its own:
///
/// ```ignore
/// use std::sync::Arc;
/// use pir::tui::autocomplete::{register_extension_command_provider, CommandProvider, SlashCommand};
/// struct MyCommands(Vec<SlashCommand>);
/// impl CommandProvider for MyCommands {
///     fn commands(&self) -> Vec<SlashCommand> { self.0.clone() }
/// }
/// register_extension_command_provider(Some(Arc::new(MyCommands(my_extension_commands))));
/// ```
///
/// Passing `None` unregisters. The combined autocomplete provider merges
/// these commands after its built-in list on every query, so injection at
/// any time (before or after TUI startup) is picked up. A provider that
/// panics is treated as unavailable — the global read uses `catch_unwind`.
static EXTENSION_PROVIDER: OnceLock<Mutex<Option<std::sync::Arc<dyn CommandProvider>>>> =
    OnceLock::new();

fn extension_provider_cell() -> &'static Mutex<Option<std::sync::Arc<dyn CommandProvider>>> {
    EXTENSION_PROVIDER.get_or_init(|| Mutex::new(None))
}

/// Register (or clear) the extension command provider. See the type-level
/// docs for the expected call pattern.
pub fn register_extension_command_provider(provider: Option<std::sync::Arc<dyn CommandProvider>>) {
    *extension_provider_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = provider;
}

/// Read the currently registered extension command provider, if any.
pub fn extension_command_provider() -> Option<std::sync::Arc<dyn CommandProvider>> {
    extension_provider_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// INJECTION POINT for skill and prompt-template slash commands (upstream
/// `skillCommands` + `templateCommands` in the combined autocomplete
/// provider). Registered once per interactive run with the discovered
/// `/skill:<name>` and `/template` commands; merged after built-ins.
static RESOURCE_PROVIDER: OnceLock<Mutex<Option<std::sync::Arc<dyn CommandProvider>>>> =
    OnceLock::new();

fn resource_provider_cell() -> &'static Mutex<Option<std::sync::Arc<dyn CommandProvider>>> {
    RESOURCE_PROVIDER.get_or_init(|| Mutex::new(None))
}

/// Register (or clear) the skill/prompt-template command provider.
pub fn register_resource_command_provider(provider: Option<std::sync::Arc<dyn CommandProvider>>) {
    *resource_provider_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = provider;
}

/// Read the currently registered resource command provider, if any.
pub fn resource_command_provider() -> Option<std::sync::Arc<dyn CommandProvider>> {
    resource_provider_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// INJECTION POINT for NATIVE (Rust-backed) plugin slash commands.
///
/// The TUI worker sends the session's live
/// [`PluginRegistry`](crate::plugins::PluginRegistry) snapshot at startup
/// (`WorkerEvent::NativeCommands`) and this provider is registered from it —
/// command names and descriptions are never a fixed list in the TUI. Adding
/// a Rust-backed command to any builtin plugin is therefore enough for it to
/// appear in the slash menu.
static NATIVE_PROVIDER: OnceLock<Mutex<Option<std::sync::Arc<dyn CommandProvider>>>> =
    OnceLock::new();

fn native_provider_cell() -> &'static Mutex<Option<std::sync::Arc<dyn CommandProvider>>> {
    NATIVE_PROVIDER.get_or_init(|| Mutex::new(None))
}

/// Register (or clear) the native-plugin command provider.
pub fn register_native_command_provider(provider: Option<std::sync::Arc<dyn CommandProvider>>) {
    *native_provider_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = provider;
}

/// Read the currently registered native-plugin command provider, if any.
pub fn native_command_provider() -> Option<std::sync::Arc<dyn CommandProvider>> {
    native_provider_cell()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

// ---------------------------------------------------------------------------
// Token helpers (upstream private functions)
// ---------------------------------------------------------------------------

fn to_display_path(value: &str) -> String {
    value.replace('\\', "/")
}

fn find_last_delimiter(text: &str) -> Option<usize> {
    text.char_indices()
        .rev()
        .find(|(_, c)| PATH_DELIMITERS.contains(c))
        .map(|(i, _)| i)
}

/// Byte offset of the last path token start.
fn token_start(text: &str) -> usize {
    match find_last_delimiter(text) {
        Some(i) => {
            let mut next = i + 1;
            while !text.is_char_boundary(next) {
                next += 1;
            }
            next
        }
        None => 0,
    }
}

fn is_token_start(text: &str, byte_index: usize) -> bool {
    byte_index == 0
        || text[..byte_index]
            .chars()
            .next_back()
            .is_some_and(|c| PATH_DELIMITERS.contains(&c))
}

/// Offset of the opening quote of an unclosed `"` quote, if any.
fn find_unclosed_quote_start(text: &str) -> Option<usize> {
    let mut in_quotes = false;
    let mut quote_start = None;
    for (i, c) in text.char_indices() {
        if c == '"' {
            in_quotes = !in_quotes;
            if in_quotes {
                quote_start = Some(i);
            }
        }
    }
    if in_quotes {
        quote_start
    } else {
        None
    }
}

/// Extract a `"...` / `@"...` quoted prefix (upstream `extractQuotedPrefix`).
fn extract_quoted_prefix(text: &str) -> Option<String> {
    let quote_start = find_unclosed_quote_start(text)?;
    let start = if quote_start > 0 && text[..quote_start].ends_with('@') {
        let at = quote_start - '@'.len_utf8();
        if !is_token_start(text, at) {
            return None;
        }
        at
    } else if !is_token_start(text, quote_start) {
        return None;
    } else {
        quote_start
    };
    Some(text[start..].to_string())
}

/// `@"x`, `"x`, `@x` decomposition (upstream `parsePathPrefix`).
#[derive(Debug, PartialEq, Eq)]
struct PathPrefix {
    raw: String,
    at: bool,
    quoted: bool,
}

fn parse_path_prefix(prefix: &str) -> PathPrefix {
    if let Some(rest) = prefix.strip_prefix("@\"") {
        return PathPrefix {
            raw: rest.to_string(),
            at: true,
            quoted: true,
        };
    }
    if let Some(rest) = prefix.strip_prefix('"') {
        return PathPrefix {
            raw: rest.to_string(),
            at: false,
            quoted: true,
        };
    }
    if let Some(rest) = prefix.strip_prefix('@') {
        return PathPrefix {
            raw: rest.to_string(),
            at: true,
            quoted: false,
        };
    }
    PathPrefix {
        raw: prefix.to_string(),
        at: false,
        quoted: false,
    }
}

/// Wrap a completed path in quotes when required (upstream
/// `buildCompletionValue`).
fn build_completion_value(path: &str, is_at: bool, is_quoted: bool) -> String {
    let needs_quotes = is_quoted || path.contains(' ');
    if !needs_quotes {
        return format!("{}{path}", if is_at { "@" } else { "" });
    }
    format!("{}\"{path}\"", if is_at { "@\"" } else { "\"" })
}

// ---------------------------------------------------------------------------
// Bounded directory scan
// ---------------------------------------------------------------------------

/// One entry produced by [`scan_directory_bounded`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanEntry {
    /// Path relative to the scan base, `/`-separated (no trailing slash).
    pub path: String,
    pub is_dir: bool,
}

fn is_skipped_dir(name: &str) -> bool {
    SKIPPED_DIRS
        .iter()
        .any(|skip| name.eq_ignore_ascii_case(skip))
}

/// Walk `base_dir` recursively, collecting up to `max_entries` entries and
/// never descending past `max_depth` or into [`SKIPPED_DIRS`]. Symlinks are
/// not followed. Best-effort: unreadable directories are skipped.
pub fn scan_directory_bounded(
    base_dir: &Path,
    max_entries: usize,
    max_depth: usize,
) -> Vec<ScanEntry> {
    let mut out = Vec::new();
    walk_dir(base_dir, Path::new(""), 0, max_depth, max_entries, &mut out);
    out
}

fn walk_dir(
    base: &Path,
    rel: &Path,
    depth: usize,
    max_depth: usize,
    max_entries: usize,
    out: &mut Vec<ScanEntry>,
) {
    if depth > max_depth || out.len() >= max_entries {
        return;
    }
    let Ok(entries) = std::fs::read_dir(base.join(rel)) else {
        return;
    };
    // Deterministic order: directories first, then names.
    let mut names: Vec<(String, bool)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(file_type) = entry.file_type() else {
                return None;
            };
            Some((name, file_type.is_dir()))
        })
        .collect();
    names.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (name, is_dir) in names {
        if out.len() >= max_entries {
            return;
        }
        let child = rel.join(&name);
        if is_dir {
            if is_skipped_dir(&name) {
                continue;
            }
            out.push(ScanEntry {
                path: to_display_path(&child.to_string_lossy()),
                is_dir: true,
            });
            walk_dir(base, &child, depth + 1, max_depth, max_entries, out);
        } else {
            out.push(ScanEntry {
                path: to_display_path(&child.to_string_lossy()),
                is_dir: false,
            });
        }
    }
}

/// Score a scanned entry against the query (upstream `scoreEntry`).
///
/// `lower_query` is the query already lowercased by the caller: the scan
/// scores thousands of entries against ONE query, so lowercasing it per
/// candidate was pure repetition.
fn score_entry(file_path: &str, lower_query: &str, is_dir: bool) -> i64 {
    let file_name = file_path.rsplit('/').next().unwrap_or(file_path);
    let lower_name = file_name.to_lowercase();
    let mut score = if lower_name == lower_query {
        100
    } else if lower_name.starts_with(lower_query) {
        80
    } else if lower_name.contains(lower_query) {
        50
    } else if file_path.to_lowercase().contains(lower_query) {
        30
    } else {
        0
    };
    if is_dir && score > 0 {
        score += 10;
    }
    score
}

// ---------------------------------------------------------------------------
// The combined provider
// ---------------------------------------------------------------------------

/// Static argument candidates for the built-in commands (upstream splices
/// `getArgumentCompletions` onto the `thinking` / `model` / `login` command
/// objects in `interactive-mode.ts:737-800`; the rest are pir's parser
/// vocabularies). Dynamic sets (`/model` refs, the model's actual thinking
/// levels, login providers) are installed through
/// [`CombinedAutocompleteProvider::set_argument_candidates`].
pub fn builtin_argument_candidates() -> std::collections::BTreeMap<String, Vec<AutocompleteItem>> {
    let mut map = std::collections::BTreeMap::new();
    map.insert(
        "thinking".to_string(),
        thinking_argument_items(&["off", "minimal", "low", "medium", "high", "xhigh", "max"]),
    );
    map.insert(
        "sessions".to_string(),
        ["new", "next", "prev", "close"]
            .iter()
            .map(|word| AutocompleteItem::new(*word, *word))
            .collect(),
    );
    map.insert(
        "debug".to_string(),
        ["enable", "disable", "last"]
            .iter()
            .map(|word| AutocompleteItem::new(*word, *word))
            .collect(),
    );
    map
}

/// `/model` argument rows (`provider/id`, label = id, description =
/// provider), the upstream `modelCommand.getArgumentCompletions` shape.
pub fn model_argument_items(pairs: &[(String, String)]) -> Vec<AutocompleteItem> {
    pairs
        .iter()
        .map(|(provider, id)| {
            AutocompleteItem::new(format!("{provider}/{id}"), id.clone())
                .with_description(provider.clone())
        })
        .collect()
}

/// `/thinking` argument rows from the model's supported levels.
pub fn thinking_argument_items(levels: &[&str]) -> Vec<AutocompleteItem> {
    levels
        .iter()
        .map(|level| AutocompleteItem::new(*level, *level))
        .collect()
}

/// The default provider: slash commands + cwd-relative file paths
/// (upstream `CombinedAutocompleteProvider`).
pub struct CombinedAutocompleteProvider {
    commands: Vec<SlashCommand>,
    base_path: PathBuf,
    /// Per-command argument candidates (upstream
    /// `SlashCommand.getArgumentCompletions`). The TUI installs the dynamic
    /// sets (models, thinking levels, login providers) at startup and when
    /// they change; the static sets live in [`builtin_argument_candidates`].
    argument_candidates: Mutex<std::collections::BTreeMap<String, Vec<AutocompleteItem>>>,
}

impl Default for CombinedAutocompleteProvider {
    fn default() -> Self {
        Self::with_builtin_commands(std::env::current_dir().unwrap_or_default())
    }
}

impl CombinedAutocompleteProvider {
    pub fn new(base_path: PathBuf, commands: Vec<SlashCommand>) -> Self {
        let argument_candidates = builtin_argument_candidates();
        Self {
            commands,
            base_path,
            argument_candidates: Mutex::new(argument_candidates),
        }
    }

    pub fn with_builtin_commands(base_path: PathBuf) -> Self {
        Self::new(base_path, builtin_commands())
    }

    /// Replace one command's argument candidates (upstream
    /// `modelCommand.getArgumentCompletions = …` and friends). An empty list
    /// removes the entry, so the popup stays closed for that command.
    pub fn set_argument_candidates(&self, command: &str, items: Vec<AutocompleteItem>) {
        let mut candidates = self
            .argument_candidates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if items.is_empty() {
            candidates.remove(command);
        } else {
            candidates.insert(command.to_string(), items);
        }
    }

    /// The argument candidates for one command name.
    pub fn argument_items(&self, command: &str) -> Vec<AutocompleteItem> {
        self.argument_candidates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(command)
            .cloned()
            .unwrap_or_default()
    }

    /// All commands: built-in first, extension-registered and
    /// skill/prompt-template commands last.
    fn all_commands(&self) -> Vec<SlashCommand> {
        let mut commands = self.commands.clone();
        for provider in [
            extension_command_provider(),
            native_command_provider(),
            resource_command_provider(),
        ] {
            let Some(provider) = provider else {
                continue;
            };
            let extension_commands =
                super::terminal::catch_contained(|| provider.commands()).unwrap_or_default();
            commands.extend(extension_commands);
        }
        commands
    }

    /// Suggestions for the current cursor position (upstream
    /// `getSuggestions`). Returns `None` when nothing applies.
    ///
    /// `force` mirrors upstream's `options.force` (Tab): path completion is
    /// offered even when the token does not look path-like.
    pub fn suggestions(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        force: bool,
    ) -> Option<AutocompleteSuggestions> {
        let line = lines.get(cursor_line)?;
        let before: String = line.chars().take(cursor_col).collect();

        // 1. `@file` attachment completion.
        if let Some(at_prefix) = extract_at_prefix(&before) {
            let parsed = parse_path_prefix(&at_prefix);
            let items = self.fuzzy_file_suggestions(&parsed.raw, parsed.quoted);
            if items.is_empty() {
                return None;
            }
            return Some(AutocompleteSuggestions {
                items,
                prefix: at_prefix,
            });
        }

        // 2. Slash-command completion at line start. The slash menu is
        // restricted to the FIRST editor line (upstream `isSlashMenuAllowed`,
        // `editor.ts:2189-2192`): `/commands` only complete on line 0.
        if !force && cursor_line == 0 && before.starts_with('/') {
            let space_index = before.find(' ');
            let Some(space_index) = space_index else {
                let prefix = &before[1..];
                let commands = self.all_commands();
                // Two-pass ordering for `skill:` commands (upstream
                // `autocomplete.ts:338-377`): bare skill names match first,
                // then `skill:`-prefixed commands that ONLY match by their
                // full name.
                let bare_name_matches = fuzzy_filter(&commands, prefix, |command| {
                    command.name.strip_prefix("skill:").unwrap_or(&command.name)
                });
                let bare_name_match_set: std::collections::HashSet<&SlashCommand> =
                    bare_name_matches.iter().collect();
                let full_name_only_matches =
                    fuzzy_filter(&commands, prefix, |command| &command.name)
                        .into_iter()
                        .filter(|command| {
                            command.name.starts_with("skill:")
                                && !bare_name_match_set.contains(command)
                        })
                        .collect::<Vec<_>>();
                let mut items: Vec<AutocompleteItem> = bare_name_matches
                    .into_iter()
                    .chain(full_name_only_matches)
                    .map(|command| command.to_item())
                    .collect();
                items.truncate(MAX_SCAN_ENTRIES);
                if items.is_empty() {
                    return None;
                }
                return Some(AutocompleteSuggestions {
                    items,
                    prefix: before,
                });
            };
            // Argument completion (upstream `getArgumentCompletions`,
            // autocomplete.ts:378-396): the command's own candidate list,
            // fuzzy-filtered on the typed argument text. A command with no
            // completer (or no match) yields no suggestions.
            let command_name = &before[1..space_index];
            let argument_text = &before[space_index + 1..];
            let candidates = self.argument_items(command_name);
            if candidates.is_empty() {
                return None;
            }
            let items = fuzzy_filter(&candidates, argument_text, |item| item.value.as_str());
            if items.is_empty() {
                return None;
            }
            return Some(AutocompleteSuggestions {
                items,
                prefix: argument_text.to_string(),
            });
        }

        // 3. Plain path completion.
        let path_match = extract_path_prefix(&before, force)?;
        let parsed = parse_path_prefix(&path_match);
        let items = self.file_suggestions(&parsed);
        if items.is_empty() {
            return None;
        }
        Some(AutocompleteSuggestions {
            items,
            prefix: path_match,
        })
    }

    /// Gate for explicit Tab file completion (upstream
    /// `shouldTriggerFileCompletion`, `autocomplete.ts:849-860`): a bare
    /// slash command being typed at line start (no space yet) is not a file
    /// completion target.
    pub fn should_trigger_file_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
    ) -> bool {
        let Some(line) = lines.get(cursor_line) else {
            return false;
        };
        let before: String = line.chars().take(cursor_col).collect();
        let trimmed = before.trim();
        if trimmed.starts_with('/') && !trimmed.contains(' ') {
            return false;
        }
        true
    }

    /// Apply the selected completion (upstream `applyCompletion`). Returns
    /// the new lines and cursor position.
    pub fn apply_completion(
        &self,
        lines: &[String],
        cursor_line: usize,
        cursor_col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> (Vec<String>, usize, usize) {
        let line = lines.get(cursor_line).cloned().unwrap_or_default();
        let before_chars: Vec<char> = line.chars().collect();
        let col = cursor_col.min(before_chars.len());
        let prefix_len = prefix.chars().count();
        let before_prefix: String = before_chars[..col.saturating_sub(prefix_len)]
            .iter()
            .collect();
        let after_cursor: String = before_chars[col..].iter().collect();

        let is_quoted = prefix.starts_with('"') || prefix.starts_with("@\"");
        let has_leading_quote = after_cursor.starts_with('"');
        let has_trailing_quote = item.value.ends_with('"');
        let after = if is_quoted && has_trailing_quote && has_leading_quote {
            &after_cursor[1..]
        } else {
            after_cursor.as_str()
        };

        // Slash commands are completed at the very start of a line and never
        // contain a path separator after the leading `/`.
        let is_slash_command = prefix.starts_with('/')
            && before_prefix.trim().is_empty()
            && !prefix[1..].contains('/');

        let is_dir = item.label.ends_with('/');
        let value_chars = item.value.chars().count();
        let cursor_offset = if is_dir && has_trailing_quote {
            value_chars.saturating_sub(1)
        } else {
            value_chars
        };

        // Directories keep no trailing space so completion can continue;
        // `@file` attachments gain one so the next token starts cleanly.
        let suffix = if prefix.starts_with('@') && !is_dir {
            " "
        } else {
            ""
        };

        let new_line = if is_slash_command {
            format!("{before_prefix}/{} {after}", item.value)
        } else {
            format!("{before_prefix}{}{suffix}{after}", item.value)
        };

        let mut new_lines: Vec<String> = lines.to_vec();
        if cursor_line < new_lines.len() {
            new_lines[cursor_line] = new_line;
        } else {
            new_lines.push(new_line);
        }
        let new_col = if is_slash_command {
            before_prefix.chars().count() + value_chars + 2
        } else {
            before_prefix.chars().count() + cursor_offset + suffix.chars().count()
        };
        (new_lines, cursor_line, new_col)
    }

    /// Fuzzy file search for `@` prefixes (upstream
    /// `getFuzzyFileSuggestions`, `fd` replaced by [`scan_directory_bounded`]).
    fn fuzzy_file_suggestions(&self, query: &str, is_quoted: bool) -> Vec<AutocompleteItem> {
        let (base_dir, query, display_base) = resolve_scoped_query(&self.base_path, query);
        let entries = scan_directory_bounded(&base_dir, MAX_SCAN_ENTRIES, MAX_SCAN_DEPTH);
        let lower_query = query.to_lowercase();
        let mut scored: Vec<(i64, usize, &ScanEntry)> = entries
            .iter()
            .map(|entry| {
                let score = if query.is_empty() {
                    1
                } else {
                    score_entry(&entry.path, &lower_query, entry.is_dir)
                };
                (
                    score,
                    entry.path.split('/').filter(|p| !p.is_empty()).count(),
                    entry,
                )
            })
            .filter(|(score, _, _)| *score > 0)
            .collect();
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.path.len().cmp(&b.2.path.len()))
                .then_with(|| a.2.path.cmp(&b.2.path))
        });

        let mut items = Vec::new();
        for (_, _, entry) in scored.into_iter().take(MAX_FUZZY_SUGGESTIONS) {
            let display_path = if display_base == "/" {
                format!("/{}", entry.path)
            } else {
                format!("{display_base}{}", entry.path)
            };
            let name = entry
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&entry.path)
                .to_string();
            let completion_path = if entry.is_dir {
                format!("{display_path}/")
            } else {
                display_path.clone()
            };
            let value = build_completion_value(&completion_path, true, is_quoted);
            let label = if entry.is_dir {
                format!("{name}/")
            } else {
                name
            };
            items.push(AutocompleteItem::new(value, label).with_description(display_path));
        }
        items
    }

    /// Single-level path completion (upstream `getFileSuggestions`).
    fn file_suggestions(&self, parsed: &PathPrefix) -> Vec<AutocompleteItem> {
        let raw = &parsed.raw;
        let expanded = expand_home(raw);
        let is_root_prefix = raw.is_empty()
            || raw == "./"
            || raw == "../"
            || raw == "~"
            || raw == "~/"
            || raw == "/"
            || (parsed.at && raw.is_empty());

        // The display prefix mirrors upstream `displayPrefix = rawPrefix`: the
        // suggestion path is rebuilt from the *typed* prefix (so `~/…`, `/`
        // and `./` stay in the completion), never from the expanded FS path.
        let (search_dir, search_prefix, display_prefix): (PathBuf, String, String) =
            if is_root_prefix || raw.ends_with('/') {
                let dir = if raw.starts_with('~') || expanded.starts_with('/') {
                    PathBuf::from(&expanded)
                } else {
                    self.base_path.join(&expanded)
                };
                (dir, String::new(), to_display_path(raw))
            } else {
                let dir = match Path::new(&expanded).parent() {
                    Some(parent) if !expanded.starts_with('/') && !raw.starts_with('~') => {
                        self.base_path.join(parent)
                    }
                    Some(parent) => parent.to_path_buf(),
                    None => self.base_path.clone(),
                };
                let file = Path::new(&expanded)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (dir, file, to_display_path(raw))
            };

        let Ok(entries) = std::fs::read_dir(&search_dir) else {
            return Vec::new();
        };
        // One lowercase pass for the prefix, not one per directory entry.
        let lower_search_prefix = search_prefix.to_lowercase();
        let mut rows: Vec<(String, bool)> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.to_lowercase().starts_with(&lower_search_prefix) {
                    return None;
                }
                let Ok(file_type) = entry.file_type() else {
                    return None;
                };
                let is_dir = if file_type.is_dir() {
                    true
                } else if file_type.is_symlink() {
                    entry.path().is_dir()
                } else {
                    false
                };
                Some((name, is_dir))
            })
            .collect();

        // Directories first, then alphabetical (upstream sort).
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        rows.truncate(MAX_SCAN_ENTRIES);

        let mut items = Vec::new();
        for (name, is_dir) in rows {
            // Upstream relative-path reconstruction (autocomplete.ts:668-705):
            // prefix concatenation, with the `~/…`, `/` and `./` forms of the
            // typed prefix preserved in the suggestion value.
            let relative = display_relative_path(&display_prefix, &name);
            let path_value = if is_dir {
                format!("{relative}/")
            } else {
                relative
            };
            let value = build_completion_value(&path_value, parsed.at, parsed.quoted);
            let label = if is_dir { format!("{name}/") } else { name };
            items.push(AutocompleteItem::new(value, label));
        }
        items
    }
}

/// Rebuild one suggestion's display path from the typed prefix (upstream
/// `getFileSuggestions` relative-path block, autocomplete.ts:668-705).
/// `display_prefix` is the full raw prefix (`~/doc`, `src/`, `/etc`, …);
/// `name` is the entry's file name.
fn display_relative_path(display_prefix: &str, name: &str) -> String {
    if display_prefix.ends_with('/') {
        // `src/` → `src/name`, `~/` → `~/name`, `/` → `/name`, `./` → `./name`.
        format!("{display_prefix}{name}")
    } else if display_prefix.contains('/') {
        if let Some(rest) = display_prefix.strip_prefix("~/") {
            // Preserve the `~/` form: `~/doc` → `~/name`, `~/a/b` → `~/a/name`.
            let parent = parent_display(rest);
            if parent.is_empty() || parent == "." {
                format!("~/{name}")
            } else {
                format!("~/{parent}/{name}")
            }
        } else if display_prefix.starts_with('/') {
            // Absolute path — rebuild from the dirname as upstream.
            let parent = parent_display(display_prefix);
            if parent == "/" {
                format!("/{name}")
            } else {
                format!("{parent}/{name}")
            }
        } else {
            // Relative path; `path.join` drops a `./` prefix, restore it.
            let parent = parent_display(display_prefix);
            let mut relative = if parent.is_empty() || parent == "." {
                name.to_string()
            } else {
                format!("{parent}/{name}")
            };
            if display_prefix.starts_with("./") && !relative.starts_with("./") {
                relative = format!("./{relative}");
            }
            relative
        }
    } else if display_prefix.starts_with('~') {
        // Standalone `~` keeps the `~/name` form (autocomplete.ts:698-700).
        format!("~/{name}")
    } else {
        name.to_string()
    }
}

/// Display form of a path's parent (dirname), `/`-separated.
fn parent_display(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|parent| to_display_path(&parent.to_string_lossy()))
        .unwrap_or_default()
}

/// `@`-prefix extraction (upstream `extractAtPrefix`): an unclosed `@"`
/// quote wins, otherwise the current token must start with `@`.
fn extract_at_prefix(text: &str) -> Option<String> {
    if let Some(quoted) = extract_quoted_prefix(text) {
        if quoted.starts_with("@\"") {
            return Some(quoted);
        }
    }
    let start = token_start(text);
    if text[start..].starts_with('@') {
        Some(text[start..].to_string())
    } else {
        None
    }
}

/// Path-prefix extraction (upstream `extractPathPrefix`).
fn extract_path_prefix(text: &str, force: bool) -> Option<String> {
    if let Some(quoted) = extract_quoted_prefix(text) {
        return Some(quoted);
    }
    let last_delim = find_last_delimiter(text);
    let path_prefix = match last_delim {
        Some(i) => {
            let mut start = i + 1;
            while !text.is_char_boundary(start) {
                start += 1;
            }
            &text[start..]
        }
        None => text,
    }
    .to_string();

    if force {
        return Some(path_prefix);
    }
    if path_prefix.contains('/') || path_prefix.starts_with('.') || path_prefix.starts_with("~/") {
        return Some(path_prefix);
    }
    // pir divergence from upstream (upstream autocomplete.ts:542 returns an
    // empty prefix after whitespace for the natural trigger, listing the whole
    // cwd): a plain "word + space" must not pop the full file list — only the
    // forced Tab path (already returned above) may complete from an empty
    // prefix.
    None
}

/// Expand `~`/`~/` against `$HOME` (`$USERPROFILE` on Windows).
fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

fn expand_home(path: &str) -> String {
    if path == "~" {
        return home_dir()
            .map(|home| home.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            let expanded = home.join(rest).to_string_lossy().into_owned();
            return expanded;
        }
    }
    path.to_string()
}

/// Split a scoped query like `src/fo` into `(base_dir, query, display_base)`
/// (upstream `resolveScopedFuzzyQuery`); `display_base` defaults to `""` when
/// unscoped, `"/"` for absolute roots.
fn resolve_scoped_query(base_path: &Path, raw_query: &str) -> (PathBuf, String, String) {
    let normalized = to_display_path(raw_query);
    let Some(slash_index) = normalized.rfind('/') else {
        return (base_path.to_path_buf(), normalized, String::new());
    };
    let display_base = normalized[..=slash_index].to_string();
    let query = normalized[slash_index + 1..].to_string();
    let base_dir = if display_base.starts_with("~/") {
        PathBuf::from(expand_home(&display_base))
    } else if display_base.starts_with('/') {
        PathBuf::from(&display_base)
    } else {
        base_path.join(&display_base)
    };
    if !base_dir.is_dir() {
        return (base_path.to_path_buf(), normalized, String::new());
    }
    (base_dir, query, display_base)
}

// ---------------------------------------------------------------------------
// Popup state + rendering
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Popup layout constants (upstream `select-list.ts` / `editor.ts`)
// ---------------------------------------------------------------------------

/// Slash-command primary-column bounds (upstream
/// `SLASH_COMMAND_SELECT_LIST_LAYOUT`, editor.ts:248-251).
const SLASH_PRIMARY_COLUMN_MIN: usize = 12;
const SLASH_PRIMARY_COLUMN_MAX: usize = 32;

/// Fixed primary width used when no explicit layout applies
/// (`DEFAULT_PRIMARY_COLUMN_WIDTH`, select-list.ts:5; an undefined layout
/// resolves to min = max = 32, select-list.ts:231-241 — the editor passes a
/// layout only for `/` prefixes, editor.ts:2240).
const DEFAULT_PRIMARY_COLUMN_WIDTH: usize = 32;
/// Blank cells between the primary column and its description
/// (`PRIMARY_COLUMN_GAP`, select-list.ts:6).
const PRIMARY_COLUMN_GAP: usize = 2;
/// Minimum remaining width for a description column
/// (`MIN_DESCRIPTION_WIDTH`, select-list.ts:7).
const MIN_DESCRIPTION_WIDTH: usize = 10;
/// Upstream renders the description column only when `width > 40`
/// (select-list.ts:193).
const DESCRIPTION_COLUMN_MIN_WIDTH: usize = 40;

/// Upstream autocomplete window clamp: `Math.max(3, Math.min(20,
/// Math.floor(maxVisible)))`, falling back to 5 when unset/non-finite
/// (editor.ts:380-381; re-applied by `setAutocompleteMaxVisible`,
/// editor.ts:411). `None` = unset.
pub const AUTOCOMPLETE_MAX_VISIBLE_DEFAULT: usize = 5;
pub const AUTOCOMPLETE_MAX_VISIBLE_MIN: usize = 3;
pub const AUTOCOMPLETE_MAX_VISIBLE_MAX: usize = 20;

/// Apply the upstream `3..=20` clamp to a requested window size
/// (editor.ts:380-381 / editor.ts:411).
pub fn clamp_max_visible(requested: Option<u32>) -> usize {
    match requested {
        Some(value) => {
            (value as usize).clamp(AUTOCOMPLETE_MAX_VISIBLE_MIN, AUTOCOMPLETE_MAX_VISIBLE_MAX)
        }
        None => AUTOCOMPLETE_MAX_VISIBLE_DEFAULT,
    }
}

/// Collapse CR/LF runs to a single space, then trim (upstream
/// `normalizeToSingleLine`, select-list.ts:9).
fn normalize_to_single_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_break = false;
    for c in text.chars() {
        if c == '\r' || c == '\n' {
            if !in_break {
                out.push(' ');
                in_break = true;
            }
        } else {
            in_break = false;
            out.push(c);
        }
    }
    out.trim().to_string()
}

/// Upstream `getDisplayValue` (select-list.ts:258-260): the label, falling
/// back to the value when the label is empty.
fn display_value(item: &AutocompleteItem) -> &str {
    if item.label.is_empty() {
        &item.value
    } else {
        &item.label
    }
}

/// The active autocomplete popup (selection + current suggestions).
#[derive(Debug, Clone)]
pub struct AutocompletePopup {
    pub suggestions: AutocompleteSuggestions,
    pub selected: usize,
    pub max_visible: usize,
    /// Whether the popup was opened by a FORCED request (explicit Tab, i.e.
    /// upstream `autocompleteState === "force"`, `editor.ts:2432-2442`):
    /// subsequent re-queries keep the forced mode so file suggestions
    /// survive while typing.
    pub forced: bool,
}

impl AutocompletePopup {
    pub fn new(suggestions: AutocompleteSuggestions) -> Self {
        Self {
            suggestions,
            selected: 0,
            max_visible: clamp_max_visible(None),
            forced: false,
        }
    }

    /// Set the visible-window size from caller settings with the upstream
    /// clamp (editor.ts:380-381, option `autocompleteMaxVisible`); `None` =
    /// unset falls back to 5.
    pub fn with_max_visible(mut self, requested: Option<u32>) -> Self {
        self.max_visible = clamp_max_visible(requested);
        self
    }

    /// Upstream `setAutocompleteMaxVisible` (editor.ts:411-416): re-clamp a
    /// live settings change.
    pub fn set_max_visible(&mut self, requested: Option<u32>) {
        self.max_visible = clamp_max_visible(requested);
    }

    pub fn selected_item(&self) -> Option<&AutocompleteItem> {
        self.suggestions.items.get(self.selected)
    }

    /// Default highlight (upstream `getBestAutocompleteMatchIndex`,
    /// editor.ts:2217-2233): the first item whose value equals the typed
    /// prefix always wins, then the first item whose value starts with it;
    /// no match keeps index 0 (the fuzzy-sorted best match).
    pub fn select_best_match(&mut self) {
        let prefix = self.suggestions.prefix.as_str();
        if prefix.is_empty() {
            return;
        }
        let mut first_prefix_index: Option<usize> = None;
        for (index, item) in self.suggestions.items.iter().enumerate() {
            if item.value == prefix {
                self.selected = index;
                return;
            }
            if first_prefix_index.is_none() && item.value.starts_with(prefix) {
                first_prefix_index = Some(index);
            }
        }
        if let Some(index) = first_prefix_index {
            self.selected = index;
        }
    }

    pub fn move_up(&mut self) {
        if self.suggestions.items.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.suggestions.items.len() - 1
        } else {
            self.selected - 1
        };
    }

    pub fn move_down(&mut self) {
        if self.suggestions.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.suggestions.items.len();
    }

    /// The visible item window `(start, end)` the popup paints (upstream
    /// `SelectList.getVisibleRange`), shared by [`Self::render_lines`] and the
    /// mouse hit-test.
    fn visible_range(&self) -> (usize, usize) {
        let items = &self.suggestions.items;
        if items.is_empty() {
            return (0, 0);
        }
        let max_visible = self
            .max_visible
            .clamp(AUTOCOMPLETE_MAX_VISIBLE_MIN, AUTOCOMPLETE_MAX_VISIBLE_MAX)
            .min(items.len());
        let start = self
            .selected
            .saturating_sub(max_visible / 2)
            .min(items.len().saturating_sub(max_visible));
        (start, (start + max_visible).min(items.len()))
    }

    /// How many ITEM rows [`Self::render_lines`] paints for this popup (the
    /// trailing `(n/m)` scroll counter is not an item).
    pub fn visible_item_rows(&self) -> usize {
        let (start, end) = self.visible_range();
        end - start
    }

    /// Select the popup item at visible row `row` (upstream
    /// `SelectList.handleMouse` press): returns whether the selection moved.
    /// The trailing scroll-counter row is not an item, so it is ignored.
    pub fn click_row(&mut self, row: usize) -> bool {
        let (start, end) = self.visible_range();
        if row >= end - start {
            return false;
        }
        let index = start + row;
        let changed = self.selected != index;
        self.selected = index;
        changed
    }

    /// Render the popup lines (upstream `SelectList.render` plus the
    /// slash-command layout — select-list.ts:75-107, editor.ts:248-251).
    pub fn render_lines(&self, width: usize) -> Vec<String> {
        let items = &self.suggestions.items;
        // Upstream empty state (select-list.ts:79-81): a muted
        // `  No matching commands` line instead of an empty popup.
        if items.is_empty() {
            return vec![with_popup_theme(|theme| {
                theme.fg(super::theme::ThemeColor::Muted, "  No matching commands")
            })];
        }
        let (start, end) = self.visible_range();
        let max_visible = end - start;
        let primary_column_width = self.primary_column_width();
        let mut lines = Vec::new();
        for (index, item) in items[start..end].iter().enumerate() {
            let absolute = start + index;
            lines.push(self.render_item(
                item,
                absolute == self.selected,
                width,
                primary_column_width,
            ));
        }
        if items.len() > max_visible {
            // Upstream truncates the scroll line to `width - 2`
            // (select-list.ts:100-103).
            let counter = format!("  ({}/{})", self.selected + 1, items.len());
            let counter = super::ansi::truncate_to_width(&counter, width.saturating_sub(2), "");
            lines.push(with_popup_theme(|theme| {
                theme.fg(super::theme::ThemeColor::Muted, &counter)
            }));
        }
        lines
    }

    /// Primary column width: `clamp(widest label + gap, bounds)` (upstream
    /// `getPrimaryColumnWidth` / `getPrimaryColumnBounds`,
    /// select-list.ts:222-241). `/` prefixes use the slash-command bounds
    /// (editor.ts:248-251, 2240); everything else the fixed default 32.
    fn primary_column_width(&self) -> usize {
        let (min, max) = if self.suggestions.prefix.starts_with('/') {
            (SLASH_PRIMARY_COLUMN_MIN, SLASH_PRIMARY_COLUMN_MAX)
        } else {
            (DEFAULT_PRIMARY_COLUMN_WIDTH, DEFAULT_PRIMARY_COLUMN_WIDTH)
        };
        let widest = self
            .suggestions
            .items
            .iter()
            .map(|item| super::ansi::visible_width(display_value(item)) + PRIMARY_COLUMN_GAP)
            .max()
            .unwrap_or(0);
        widest.clamp(min, max)
    }

    /// One row, port of upstream `SelectList.renderItem`
    /// (select-list.ts:183-220): fixed primary column (labels hard-clipped
    /// into it, upstream truncates with an empty ellipsis), then
    /// `PRIMARY_COLUMN_GAP` before the description. Styling keeps pir's
    /// audited mapping of the upstream theme (`getSelectListTheme`,
    /// theme.ts:1061-1068): accent cursor + selected label, muted
    /// description (upstream wraps the whole selected row in `selectedText`,
    /// select-list.ts:204-205).
    fn render_item(
        &self,
        item: &AutocompleteItem,
        is_selected: bool,
        width: usize,
        primary_column_width: usize,
    ) -> String {
        let prefix = if is_selected { "→ " } else { "  " };
        let prefix_width = super::ansi::visible_width(prefix);
        let value = display_value(item);
        let description = item
            .description
            .as_deref()
            .map(normalize_to_single_line)
            .filter(|description| !description.is_empty());

        if let Some(description) = description
            .as_deref()
            .filter(|_| width > DESCRIPTION_COLUMN_MIN_WIDTH)
        {
            // effectivePrimaryColumnWidth = max(1, min(col, width - prefix - 4))
            let effective_primary = primary_column_width
                .min(width.saturating_sub(prefix_width + 4))
                .max(1);
            // maxPrimaryWidth = max(1, effectivePrimaryColumnWidth - gap)
            let max_primary = effective_primary.saturating_sub(PRIMARY_COLUMN_GAP).max(1);
            let truncated_value = super::ansi::truncate_to_width(value, max_primary, "");
            let truncated_value_width = super::ansi::visible_width(&truncated_value);
            let spacing = " ".repeat(
                effective_primary
                    .saturating_sub(truncated_value_width)
                    .max(1),
            );
            let description_start = prefix_width + truncated_value_width + spacing.len();
            let remaining = width.saturating_sub(description_start + 2); // upstream -2 safety
            if remaining > MIN_DESCRIPTION_WIDTH {
                let truncated_desc = super::ansi::truncate_to_width(description, remaining, "");
                return styled_row(
                    prefix,
                    &truncated_value,
                    Some((&spacing, &truncated_desc)),
                    is_selected,
                );
            }
        }

        let max_width = width.saturating_sub(prefix_width + 2);
        let truncated_value = super::ansi::truncate_to_width(value, max_width, "");
        styled_row(prefix, &truncated_value, None, is_selected)
    }
}

/// Theme one rendered row: selected rows accent the cursor and label,
/// descriptions stay muted (the audited pir mapping for upstream
/// `selectedText` / `description`).
fn styled_row(
    prefix: &str,
    value: &str,
    description: Option<(&str, &str)>,
    is_selected: bool,
) -> String {
    let (prefix, value) = if is_selected {
        (
            with_popup_theme(|theme| theme.fg(super::theme::ThemeColor::Accent, prefix)),
            with_popup_theme(|theme| theme.fg(super::theme::ThemeColor::Accent, value)),
        )
    } else {
        (prefix.to_string(), value.to_string())
    };
    let mut line = format!("{prefix}{value}");
    if let Some((spacing, description)) = description {
        line.push_str(spacing);
        line.push_str(&with_popup_theme(|theme| {
            theme.fg(super::theme::ThemeColor::Muted, description)
        }));
    }
    line
}

fn with_popup_theme<T>(f: impl FnOnce(&super::theme::Theme) -> T) -> T {
    super::theme::with_theme(f)
}
