use super::*;

// ---------------------------------------------------------------------------
// SettingsManager
// ---------------------------------------------------------------------------

/// Which settings scope an error or write refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsScope {
    Global,
    Project,
}

/// A recorded settings load/write failure. Parse errors are recorded, not
/// fatal (upstream `SettingsError`).
#[derive(Debug, Clone)]
pub struct SettingsError {
    pub scope: SettingsScope,
    pub path: Option<PathBuf>,
    pub message: String,
}

impl SettingsError {
    /// The scope name for diagnostics.
    pub fn scope_name(&self) -> &'static str {
        match self.scope {
            SettingsScope::Global => "global",
            SettingsScope::Project => "project",
        }
    }
}

/// Loaded settings plus the paths they came from. Project settings load only
/// when the project is trusted; untrusting clears and re-merges (upstream
/// `setProjectTrusted`).
///
/// `Clone`: the CLI hands its single startup snapshot to the mode hosts
/// (TUI/print/RPC) instead of re-reading both settings scopes per host.
#[derive(Clone)]
pub struct SettingsManager {
    /// Raw merged settings (project deep-merged over global), post-migration.
    pub raw: Value,
    /// Typed merged settings parsed from [`Self::raw`].
    pub settings: Settings,
    /// Raw global-scope settings (post-migration).
    pub global_raw: Value,
    /// Raw project-scope settings (post-migration; `{}` when untrusted).
    pub project_raw: Value,
    /// Global agent directory (typically `~/.pi/agent`).
    pub global_dir: PathBuf,
    /// Project config directory (typically `<cwd>/.pi`).
    pub project_dir: PathBuf,
    /// Working directory.
    pub cwd: PathBuf,
    /// Whether the project scope is trusted right now.
    pub project_trusted: bool,
    /// Load errors recorded so far (drained by [`Self::drain_errors`]).
    errors: Vec<SettingsError>,
    /// Per-leaf tolerant-parse diagnostics: a wrong-typed leaf falls back to
    /// its default (upstream reads each field independently) and only lands
    /// here — unlike `errors`, these never fail a provider/settings load,
    /// since the rest of the document parsed fine. The message names the
    /// dotted path below the top-level key (`terminal.trueColor`).
    warnings: Vec<SettingsError>,
}

impl std::fmt::Debug for SettingsManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SettingsManager")
            .field("global_dir", &self.global_dir)
            .field("project_dir", &self.project_dir)
            .field("cwd", &self.cwd)
            .field("project_trusted", &self.project_trusted)
            .field("errors", &self.errors.len())
            .field("warnings", &self.warnings.len())
            .finish()
    }
}

fn global_settings_path(global_dir: &Path) -> PathBuf {
    global_dir.join("settings.json")
}

fn project_settings_path(project_dir: &Path) -> PathBuf {
    project_dir.join("settings.json")
}

/// One step in a path below a top-level `settings.json` key.
#[derive(Debug, Clone)]
enum SettingsPathStep {
    Key(String),
    Index(usize),
}

/// One invalid leaf found while recovering a top-level entry.
struct InvalidLeaf {
    /// Path below the top-level key, used to attribute the diagnostic to the
    /// scope file that supplied the value.
    steps: Vec<SettingsPathStep>,
    /// Human-readable path below the top-level key (`trueColor`,
    /// `packages[1]`).
    display: String,
    message: String,
}

/// Result of parsing one node of a top-level entry during recovery.
enum EntryRecovery {
    /// The node parses as-is.
    Valid,
    /// The node is an object/array whose invalid children were pruned. The
    /// replacement value keeps every leaf that parsed.
    Recovered(Value, Vec<InvalidLeaf>),
    /// The node itself is invalid (no child-level repair succeeds); the
    /// string is the parse error.
    Invalid(String),
}

/// One warning queued while a failed whole-document parse is recovered, kept
/// in document order.
enum PendingWarning {
    Entry { key: String, message: String },
    Leaf { key: String, leaf: InvalidLeaf },
}

/// Parse `value` as the settings entry `top_key` when nested under `steps`
/// (the path between `top_key` and `value`).
fn parse_entry_at(
    top_key: &str,
    steps: &[SettingsPathStep],
    value: &Value,
) -> Result<(), serde_json::Error> {
    serde_json::from_value::<Settings>(entry_document(top_key, steps, value)).map(|_| ())
}

/// The smallest settings document that puts `value` at `top_key` + `steps`.
/// Array positions collapse to a single-element array: only the value at the
/// leaf matters, so a probe does not have to reproduce the whole list.
fn entry_document(top_key: &str, steps: &[SettingsPathStep], value: &Value) -> Value {
    let mut node = value.clone();
    for step in steps.iter().rev() {
        node = match step {
            SettingsPathStep::Key(key) => {
                let mut map = serde_json::Map::new();
                map.insert(key.clone(), node);
                Value::Object(map)
            }
            SettingsPathStep::Index(_) => Value::Array(vec![node]),
        };
    }
    let mut root = serde_json::Map::new();
    root.insert(top_key.to_string(), node);
    Value::Object(root)
}

/// Resolve `steps` below a raw entry value (scope attribution).
fn value_at_steps<'a>(mut value: &'a Value, steps: &[SettingsPathStep]) -> Option<&'a Value> {
    for step in steps {
        value = match step {
            SettingsPathStep::Key(key) => value.get(key)?,
            SettingsPathStep::Index(index) => value.get(*index)?,
        };
    }
    Some(value)
}

/// Recursively recover one node of a top-level entry: a wrong-typed leaf is
/// dropped on its own while its valid siblings (same object or array) are
/// kept. Unknown fields parse as-is and are therefore always kept.
fn recover_entry_node(
    top_key: &str,
    steps: &mut Vec<SettingsPathStep>,
    display: &mut String,
    value: &Value,
) -> EntryRecovery {
    let Err(node_error) = parse_entry_at(top_key, steps, value) else {
        return EntryRecovery::Valid;
    };
    match value {
        Value::Object(children) => {
            let mut recovered = serde_json::Map::new();
            let mut failures = Vec::new();
            for (child_key, child_value) in children {
                steps.push(SettingsPathStep::Key(child_key.clone()));
                let display_len = display.len();
                if !display.is_empty() {
                    display.push('.');
                }
                display.push_str(child_key);
                match recover_entry_node(top_key, steps, display, child_value) {
                    EntryRecovery::Valid => {
                        recovered.insert(child_key.clone(), child_value.clone());
                    }
                    EntryRecovery::Recovered(child, mut child_failures) => {
                        recovered.insert(child_key.clone(), child);
                        failures.append(&mut child_failures);
                    }
                    EntryRecovery::Invalid(message) => failures.push(InvalidLeaf {
                        steps: steps.clone(),
                        display: display.clone(),
                        message,
                    }),
                }
                display.truncate(display_len);
                steps.pop();
            }
            let replacement = Value::Object(recovered);
            match parse_entry_at(top_key, steps, &replacement) {
                Ok(()) => EntryRecovery::Recovered(replacement, failures),
                Err(error) => EntryRecovery::Invalid(error.to_string()),
            }
        }
        Value::Array(entries) => {
            let mut recovered = Vec::new();
            let mut failures = Vec::new();
            for (index, entry) in entries.iter().enumerate() {
                steps.push(SettingsPathStep::Index(index));
                let display_len = display.len();
                display.push_str(&format!("[{index}]"));
                match parse_entry_at(top_key, steps, entry) {
                    Ok(()) => recovered.push(entry.clone()),
                    Err(error) => failures.push(InvalidLeaf {
                        steps: steps.clone(),
                        display: display.clone(),
                        message: error.to_string(),
                    }),
                }
                display.truncate(display_len);
                steps.pop();
            }
            let replacement = Value::Array(recovered);
            match parse_entry_at(top_key, steps, &replacement) {
                Ok(()) => EntryRecovery::Recovered(replacement, failures),
                Err(error) => EntryRecovery::Invalid(error.to_string()),
            }
        }
        _ => EntryRecovery::Invalid(node_error.to_string()),
    }
}

/// Parse one top-level `settings.json` entry exactly as the whole document
/// would (renames, nested structs), for the key-by-key recovery path of
/// [`SettingsManager::parse_typed_settings`].
fn parse_settings_entry(key: &str, value: &Value) -> Result<(), serde_json::Error> {
    parse_entry_at(key, &[], value)
}

/// Whether one scope's stored value for `key` fails to parse on its own.
fn scope_entry_fails(raw: &Value, key: &str) -> bool {
    raw.get(key)
        .is_some_and(|value| parse_settings_entry(key, value).is_err())
}

/// Scope-attributed message formatting shared by `error_messages` and
/// `warning_messages`.
fn format_scope_messages(errors: &[SettingsError]) -> Vec<String> {
    errors
        .iter()
        .map(|error| {
            let scope = match error.scope {
                SettingsScope::Global => "global",
                SettingsScope::Project => "project",
            };
            format!("{scope} settings: {}", error.message)
        })
        .collect()
}

impl SettingsManager {
    /// Load settings from `global_dir/settings.json` and
    /// `project_dir/settings.json` (project wins per-field via deep merge).
    /// The project scope loads only when `project_trusted`. Missing files are
    /// skipped; malformed files are recorded in [`Self::errors`] (parse errors
    /// are not fatal).
    pub fn load(global_dir: &Path, project_dir: &Path, cwd: &Path, project_trusted: bool) -> Self {
        let mut manager = Self {
            raw: Value::Object(serde_json::Map::new()),
            settings: Settings::default(),
            global_raw: Value::Object(serde_json::Map::new()),
            project_raw: Value::Object(serde_json::Map::new()),
            global_dir: global_dir.to_path_buf(),
            project_dir: project_dir.to_path_buf(),
            cwd: cwd.to_path_buf(),
            project_trusted,
            errors: Vec::new(),
            warnings: Vec::new(),
        };
        manager.reload();
        manager
    }

    /// Re-read both scopes from disk and re-merge (upstream `reload`).
    pub fn reload(&mut self) {
        // Diagnostics are derived from the files just read: a previous
        // load's error must not survive a successful reload (the user fixed
        // the file, or `set_project_trusted` toggled the project scope).
        // Every read below records the CURRENT state again.
        self.errors.clear();
        self.warnings.clear();
        self.global_raw = self.load_scope(SettingsScope::Global);
        if self.project_trusted {
            self.project_raw = self.load_scope(SettingsScope::Project);
        } else {
            self.project_raw = Value::Object(serde_json::Map::new());
        }
        self.remerge();
    }

    /// Load one scope: read the file, strip the BOM, parse, migrate. Missing
    /// files yield `{}`; malformed content or a non-NotFound read failure
    /// yields `{}` plus a recorded error carrying the path (upstream
    /// `readFileSync` throws for unreadable files and the error is recorded
    /// with its settings path, so startup diagnostics can name the file).
    fn load_scope(&mut self, scope: SettingsScope) -> Value {
        let path = match scope {
            SettingsScope::Global => global_settings_path(&self.global_dir),
            SettingsScope::Project => project_settings_path(&self.project_dir),
        };
        let text = match read_text_file_checked(&path) {
            Ok(Some(text)) => text,
            Ok(None) => return Value::Object(serde_json::Map::new()),
            Err(error) => {
                self.errors.push(SettingsError {
                    scope,
                    path: Some(path),
                    message: error.to_string(),
                });
                return Value::Object(serde_json::Map::new());
            }
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(mut value) if value.is_object() => {
                migrate_settings(&mut value);
                value
            }
            Ok(_) => {
                self.errors.push(SettingsError {
                    scope,
                    path: Some(path),
                    message: "settings must be a JSON object".to_string(),
                });
                Value::Object(serde_json::Map::new())
            }
            Err(error) => {
                self.errors.push(SettingsError {
                    scope,
                    path: Some(path),
                    message: error.to_string(),
                });
                Value::Object(serde_json::Map::new())
            }
        }
    }

    /// Recompute the merged snapshot after a scope change.
    fn remerge(&mut self) {
        let mut raw = self.global_raw.clone();
        deep_merge_settings_values(&mut raw, &self.project_raw);
        self.raw = raw;
        self.settings = self.parse_typed_settings();
    }

    /// Parse [`Self::raw`] into typed settings. A fully valid document parses
    /// in one pass. When any field has the wrong type, a whole-document parse
    /// fails and must not reset every typed setting (upstream reads each field
    /// independently): recovery walks into each invalid top-level entry and
    /// prunes only the wrong-typed leaves, so a bad `terminal.trueColor` no
    /// longer drops a valid `terminal.images` (object fields are recovered
    /// recursively; array fields keep their valid elements). One diagnostic
    /// per pruned leaf is recorded as a warning (`warning_messages`), with the
    /// dotted path below the key (`terminal.trueColor`). A top-level entry
    /// that cannot be repaired at all (e.g. a scalar where an object is
    /// expected) still falls back to its default as a whole. Warnings — unlike
    /// [`Self::error_messages`] — never fail a settings/provider load,
    /// because the rest of the document is usable.
    fn parse_typed_settings(&mut self) -> Settings {
        // `warnings` is derived state: every call re-parses the current `raw`,
        // so stale entries must not accumulate across reloads/persists (a
        // long-lived TUI writes settings repeatedly, and the vec would
        // otherwise grow and re-report the same field on every write).
        self.warnings.clear();
        match serde_json::from_value::<Settings>(self.raw.clone()) {
            Ok(settings) => settings,
            Err(overall_error) => {
                let Some(map) = self.raw.as_object() else {
                    // Unreachable when `raw` is an object; keep a diagnostic
                    // rather than failing silently.
                    self.errors.push(SettingsError {
                        scope: SettingsScope::Global,
                        path: Some(self.global_path()),
                        message: format!("invalid settings: {overall_error}"),
                    });
                    return Settings::default();
                };
                let mut recovered = serde_json::Map::new();
                let mut pending: Vec<PendingWarning> = Vec::new();
                for (key, value) in map {
                    let mut steps = Vec::new();
                    let mut display = String::new();
                    match recover_entry_node(key, &mut steps, &mut display, value) {
                        EntryRecovery::Valid => {
                            recovered.insert(key.clone(), value.clone());
                        }
                        EntryRecovery::Recovered(replacement, leaves) => {
                            recovered.insert(key.clone(), replacement);
                            for leaf in leaves {
                                pending.push(PendingWarning::Leaf {
                                    key: key.clone(),
                                    leaf,
                                });
                            }
                        }
                        EntryRecovery::Invalid(message) => {
                            pending.push(PendingWarning::Entry {
                                key: key.clone(),
                                message,
                            });
                        }
                    }
                }
                for warning in pending {
                    let (scope, message) = match warning {
                        PendingWarning::Entry { key, message } => (
                            self.failing_scope(&key),
                            format!("invalid setting \"{key}\": {message}"),
                        ),
                        PendingWarning::Leaf { key, leaf } => {
                            // An array element appends its index directly
                            // (`packages[1]`); an object leaf is dotted
                            // (`terminal.trueColor`).
                            let leaf_path = if leaf.display.starts_with('[') {
                                format!("{key}{}", leaf.display)
                            } else {
                                format!("{key}.{}", leaf.display)
                            };
                            (
                                self.failing_scope_for_leaf(&key, &leaf.steps),
                                format!("invalid setting \"{leaf_path}\": {}", leaf.message),
                            )
                        }
                    };
                    let path = match scope {
                        SettingsScope::Global => self.global_path(),
                        SettingsScope::Project => self.project_path(),
                    };
                    self.warnings.push(SettingsError {
                        scope,
                        path: Some(path),
                        message,
                    });
                }
                match serde_json::from_value::<Settings>(Value::Object(recovered)) {
                    Ok(settings) => settings,
                    Err(recovery_error) => {
                        // Unreachable if every entry parsed on its own;
                        // never fall back without a diagnostic.
                        self.warnings.push(SettingsError {
                            scope: SettingsScope::Global,
                            path: Some(self.global_path()),
                            message: format!("invalid settings: {recovery_error}"),
                        });
                        Settings::default()
                    }
                }
            }
        }
    }

    /// Attribute a lenient-parse failure for a merged top-level `key` to the
    /// scope file that supplied it: the scope whose stored value does not
    /// parse on its own (project overrides global), otherwise the scope that
    /// defines the key, defaulting to global (e.g. for `apply_overrides`
    /// values that no file defines).
    fn failing_scope(&self, key: &str) -> SettingsScope {
        if self.project_trusted && scope_entry_fails(&self.project_raw, key) {
            return SettingsScope::Project;
        }
        if scope_entry_fails(&self.global_raw, key) {
            return SettingsScope::Global;
        }
        if self.project_trusted && self.project_raw.get(key).is_some() {
            return SettingsScope::Project;
        }
        SettingsScope::Global
    }

    /// Attribute one pruned nested leaf to the scope file that supplied that
    /// leaf. Falls back to the whole-key attribution when no scope stores the
    /// path explicitly (e.g. values introduced by `apply_overrides`) or when
    /// the path traverses an array (an element probe is not positional).
    fn failing_scope_for_leaf(&self, key: &str, steps: &[SettingsPathStep]) -> SettingsScope {
        if steps
            .iter()
            .any(|step| matches!(step, SettingsPathStep::Index(_)))
        {
            return self.failing_scope(key);
        }
        let leaf_fails = |raw: &Value| {
            raw.get(key)
                .and_then(|entry| value_at_steps(entry, steps))
                .is_some_and(|value| parse_entry_at(key, steps, value).is_err())
        };
        if self.project_trusted && leaf_fails(&self.project_raw) {
            return SettingsScope::Project;
        }
        if leaf_fails(&self.global_raw) {
            return SettingsScope::Global;
        }
        self.failing_scope(key)
    }

    /// The global settings path.
    pub fn global_path(&self) -> PathBuf {
        global_settings_path(&self.global_dir)
    }

    /// The project settings path.
    pub fn project_path(&self) -> PathBuf {
        project_settings_path(&self.project_dir)
    }

    /// Drain recorded load/write errors.
    pub fn drain_errors(&mut self) -> Vec<SettingsError> {
        std::mem::take(&mut self.errors)
    }

    /// The recorded load error messages, formatted with their scope
    /// (non-draining; parse errors are recorded, not fatal).
    pub fn error_messages(&self) -> Vec<String> {
        format_scope_messages(&self.errors)
    }

    /// The recorded per-field parse warnings (non-draining): a bad-typed
    /// field already fell back to its default, so these never fail a load.
    pub fn warning_messages(&self) -> Vec<String> {
        format_scope_messages(&self.warnings)
    }

    /// Change the project trust state: untrusting clears the project scope and
    /// re-merges (dropping project-derived values); trusting reloads the
    /// project file (upstream `setProjectTrusted`).
    pub fn set_project_trusted(&mut self, trusted: bool) {
        if self.project_trusted == trusted {
            return;
        }
        self.project_trusted = trusted;
        self.reload();
    }

    /// Apply extra overrides on top of the merged settings (upstream
    /// `applyOverrides`).
    pub fn apply_overrides(&mut self, overrides: &Value) {
        let mut raw = self.raw.clone();
        deep_merge_settings_values(&mut raw, overrides);
        self.raw = raw;
        self.settings = self.parse_typed_settings();
    }

    /// Persist one top-level field of `scope`, writing only that field into
    /// the file (merged with the file's current content, unknown keys and
    /// unmodified fields preserved). File-locked. Returns whether the write
    /// succeeded; on failure the caller must not commit an in-memory
    /// mutation.
    pub fn persist_field(&mut self, scope: SettingsScope, key: &str, value: Value) -> bool {
        self.persist_writes(
            scope,
            vec![FieldWrite {
                field: key.to_string(),
                nested: None,
                value,
            }],
        )
    }

    /// Persist one nested key inside a top-level object field of `scope`
    /// (nested-key granularity: only `key.nestedKey` is written into the
    /// file; the rest of the file's `key` object is preserved).
    pub fn persist_nested_field(
        &mut self,
        scope: SettingsScope,
        key: &str,
        nested_key: &str,
        value: Value,
    ) -> bool {
        self.persist_writes(
            scope,
            vec![FieldWrite {
                field: key.to_string(),
                nested: Some(nested_key.to_string()),
                value,
            }],
        )
    }

    /// Shared persist implementation: read + migrate the current file under a
    /// lock, overlay only the listed writes, and write the result back. A file
    /// that exists but is not a JSON object aborts the write instead of being
    /// silently replaced (upstream `persistScopedSettings` runs `JSON.parse`
    /// inside the lock, so a corrupt file throws before anything is written).
    ///
    /// Returns `true` only when the atomic replace committed. Every write in
    /// `writes` lands in the SAME temp-file replace, so a multi-field caller
    /// (first-run setup, analytics opt-in) cannot leave a half-applied
    /// document.
    fn persist_writes(&mut self, scope: SettingsScope, writes: Vec<FieldWrite>) -> bool {
        if scope == SettingsScope::Project && !self.project_trusted {
            self.errors.push(SettingsError {
                scope,
                path: Some(self.project_path()),
                message: "project is not trusted; refusing to write project settings".to_string(),
            });
            return false;
        }
        let path = match scope {
            SettingsScope::Global => self.global_path(),
            SettingsScope::Project => self.project_path(),
        };
        // Create the parent BEFORE taking the sibling lock: the lock file
        // lives in the same directory, so a first-ever write into a missing
        // directory could not otherwise be serialized.
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                if let Err(write_error) = fs::create_dir_all(parent) {
                    self.errors.push(SettingsError {
                        scope,
                        path: Some(path.clone()),
                        message: format!("cannot create {}: {write_error}", parent.display()),
                    });
                    return false;
                }
            }
        }
        let error = with_file_lock(&path, || {
            // Read + migrate the current file content. A missing or empty file
            // starts from `{}` (upstream treats a falsy `current` the same
            // way); a corrupt file refuses the write so the user's content is
            // never overwritten with only this write's fields.
            let mut current = match read_text_file(&path) {
                Some(text) if !text.is_empty() => match serde_json::from_str::<Value>(&text) {
                    Ok(mut value) if value.is_object() => {
                        migrate_settings(&mut value);
                        value
                    }
                    Ok(_) => {
                        return format!(
                            "refusing to overwrite {}: existing settings is not a JSON object",
                            path.display()
                        );
                    }
                    Err(parse_error) => {
                        return format!(
                            "refusing to overwrite {}: existing settings is not valid JSON: {parse_error}",
                            path.display()
                        );
                    }
                },
                Some(_) => Value::Object(serde_json::Map::new()),
                None if path.exists() => {
                    // A file that exists but cannot be read (I/O error, not
                    // UTF-8) is corrupt too: upstream `readFileSync` would
                    // throw inside the lock and abort the write as well.
                    return format!(
                        "refusing to overwrite {}: existing settings cannot be read as UTF-8 text",
                        path.display()
                    );
                }
                None => Value::Object(serde_json::Map::new()),
            };
            for write in &writes {
                match &write.nested {
                    Some(nested) => {
                        // Nested-key granularity: only this nested key is
                        // written; the file's other nested keys survive.
                        if !current
                            .get(&write.field)
                            .map(Value::is_object)
                            .unwrap_or(false)
                        {
                            current[&write.field] = Value::Object(serde_json::Map::new());
                        }
                        if let Some(map) =
                            current.get_mut(&write.field).and_then(Value::as_object_mut)
                        {
                            map.insert(nested.clone(), write.value.clone());
                        }
                    }
                    None => {
                        if let Some(map) = current.as_object_mut() {
                            map.insert(write.field.clone(), write.value.clone());
                        }
                    }
                }
            }
            let Ok(serialized) = serde_json::to_string_pretty(&current) else {
                return "failed to serialize settings".to_string();
            };
            if let Some(parent) = path.parent() {
                if !parent.exists() {
                    if let Err(write_error) = fs::create_dir_all(parent) {
                        return format!("cannot create {}: {write_error}", parent.display());
                    }
                }
            }
            match write_text_atomic(&path, &serialized) {
                Ok(()) => String::new(),
                Err(write_error) => format!("cannot write {}: {write_error}", path.display()),
            }
        });
        if error.is_empty() {
            true
        } else {
            self.errors.push(SettingsError {
                scope,
                path: Some(path),
                message: error,
            });
            false
        }
    }

    /// Update a whole top-level field in memory and persist it to the global
    /// scope (upstream `setXxx` pattern). The in-memory snapshot is committed
    /// only AFTER the atomic file replace succeeded: a failed write leaves
    /// the manager reporting the last persisted state instead of a value
    /// that will disappear on the next reload.
    pub fn set_global_field(&mut self, key: &str, value: Value) {
        self.set_global_fields(&[(key, value)]);
    }

    /// Set several top-level global fields as ONE atomic write (first-run
    /// setup and analytics opt-in need this: a crash or failed write must
    /// never leave a partially applied document that later reads as
    /// "setup already completed"). In-memory state is committed only when
    /// the write succeeded.
    pub fn set_global_fields(&mut self, fields: &[(&str, Value)]) -> bool {
        let writes: Vec<FieldWrite> = fields
            .iter()
            .map(|(key, value)| FieldWrite {
                field: (*key).to_string(),
                nested: None,
                value: value.clone(),
            })
            .collect();
        if !self.persist_writes(SettingsScope::Global, writes) {
            return false;
        }
        if let Some(map) = self.global_raw.as_object_mut() {
            for (key, value) in fields {
                map.insert((*key).to_string(), value.clone());
            }
        }
        self.remerge();
        true
    }

    /// Update one nested key inside a global object field, persisting with
    /// nested-key granularity (upstream `markModified(field, nestedKey)`).
    /// The in-memory mutation is committed only after the write succeeded.
    pub fn set_global_nested_field(&mut self, key: &str, nested_key: &str, value: Value) {
        if !self.persist_nested_field(SettingsScope::Global, key, nested_key, value.clone()) {
            return;
        }
        if !self
            .global_raw
            .get(key)
            .map(Value::is_object)
            .unwrap_or(false)
        {
            self.global_raw[key] = Value::Object(serde_json::Map::new());
        }
        self.global_raw[key][nested_key] = value;
        self.remerge();
    }

    /// Stable installation id, created on first use and persisted to the
    /// global scope only, so a committed project settings file cannot give
    /// every clone the same id (upstream `getOrCreateDeviceId`).
    pub fn get_or_create_device_id(&mut self) -> String {
        // GLOBAL scope only: a project settings file must never supply an id
        // shared by every clone (the same rule as `trackingId`).
        if let Some(id) = self.global_raw.get("deviceId").and_then(Value::as_str) {
            return id.to_owned();
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.set_global_field("deviceId", Value::String(id.clone()));
        id
    }

    // -- Typed accessors with upstream defaults ---------------------------

    /// The effective steering mode (default `one-at-a-time`).
    pub fn steering_mode(&self) -> SteeringMode {
        self.settings
            .steering_mode
            .unwrap_or(SteeringMode::OneAtATime)
    }

    /// The effective follow-up mode (default `one-at-a-time`).
    pub fn follow_up_mode(&self) -> FollowUpMode {
        self.settings
            .follow_up_mode
            .unwrap_or(FollowUpMode::OneAtATime)
    }

    /// The effective transport (default `auto`).
    pub fn transport(&self) -> TransportSetting {
        self.settings.transport.unwrap_or(TransportSetting::Auto)
    }

    /// The effective `/share` destination (default `auto`). Only `gist` and
    /// `radius` are meaningful values; an unset (or unparseable, see
    /// [`Self::parse_typed_settings`]) field resolves here, mirroring upstream
    /// `getSessionShareDestination` (`cea257439`).
    pub fn session_share_destination(&self) -> SessionShareDestination {
        self.settings.session_share_destination.unwrap_or_default()
    }

    /// Set `sessionShareDestination` (global scope only, upstream
    /// `setSessionShareDestination` → `markModified` + save).
    pub fn set_session_share_destination(&mut self, destination: SessionShareDestination) {
        let value = serde_json::to_value(destination)
            .unwrap_or_else(|_| Value::String(defaults::SESSION_SHARE_DESTINATION.to_owned()));
        self.set_global_field("sessionShareDestination", value);
    }

    /// The theme name, or `None` for path-like values (upstream `getTheme`).
    pub fn theme(&self) -> Option<&str> {
        match &self.settings.theme {
            Some(theme) if !theme.contains('/') => Some(theme),
            _ => None,
        }
    }

    /// The analytics opt-in (upstream `getEnableAnalytics`, default `false`
    /// from `SETTINGS_DEFAULTS.enableAnalytics`). GLOBAL scope only: a project
    /// settings file must not opt a user into analytics.
    pub fn enable_analytics(&self) -> bool {
        self.global_raw
            .get("enableAnalytics")
            .and_then(Value::as_bool)
            .unwrap_or(defaults::ENABLE_ANALYTICS)
    }

    /// The stored analytics tracking identifier, when one was generated
    /// (upstream `getTrackingId`). GLOBAL scope only: a project settings file
    /// must not choose the identifier every clone reports under.
    pub fn tracking_id(&self) -> Option<&str> {
        self.global_raw.get("trackingId").and_then(Value::as_str)
    }

    /// Set the analytics opt-in in the GLOBAL scope (upstream
    /// `setEnableAnalytics`): the first opt-in also generates and persists a
    /// `trackingId`, and an existing identifier is kept across
    /// opt-out/opt-in cycles. A project scope can never supply the ID, so a
    /// committed project settings file cannot give every clone the same one.
    /// The opt-in flag and the new identifier land in ONE atomic write.
    pub fn set_enable_analytics(&mut self, enabled: bool) {
        let mut fields: Vec<(&str, Value)> = vec![("enableAnalytics", Value::Bool(enabled))];
        if enabled
            && self
                .global_raw
                .get("trackingId")
                .and_then(Value::as_str)
                .is_none()
        {
            let id = uuid::Uuid::new_v4().to_string();
            fields.push(("trackingId", Value::String(id)));
        }
        self.set_global_fields(&fields);
    }

    /// Compaction enabled (default `true`).
    pub fn compaction_enabled(&self) -> bool {
        self.settings
            .compaction
            .as_ref()
            .and_then(|compaction| compaction.enabled)
            .unwrap_or(defaults::COMPACTION_ENABLED)
    }

    /// Resolve one compaction token setting through the model override, the
    /// ordinary setting, then the built-in default (upstream
    /// `getCompactionTokenSetting`).
    fn compaction_token(
        &self,
        pick: fn(&CompactionModelOverride) -> Option<u64>,
        ordinary: fn(&CompactionSettings) -> Option<u64>,
        fallback: u64,
        model_key: Option<(&str, &str)>,
    ) -> u64 {
        let compaction = self.settings.compaction.as_ref();
        let per_model = model_key.and_then(|(provider, id)| {
            compaction
                .and_then(|compaction| compaction.model_overrides.as_ref())
                .and_then(|overrides| overrides.get(&format!("{provider}/{id}")))
                .and_then(pick)
        });
        per_model
            .or_else(|| compaction.and_then(ordinary))
            .unwrap_or(fallback)
    }

    /// Compaction reserve tokens for `model` (default 16384).
    pub fn compaction_reserve_tokens(&self, model: Option<(&str, &str)>) -> u64 {
        self.compaction_token(
            |over| over.reserve_tokens,
            |compaction| compaction.reserve_tokens,
            defaults::COMPACTION_RESERVE_TOKENS,
            model,
        )
    }

    /// Compaction keep-recent tokens for `model` (default 20000).
    pub fn compaction_keep_recent_tokens(&self, model: Option<(&str, &str)>) -> u64 {
        self.compaction_token(
            |over| over.keep_recent_tokens,
            |compaction| compaction.keep_recent_tokens,
            defaults::COMPACTION_KEEP_RECENT_TOKENS,
            model,
        )
    }

    /// Retry settings with upstream defaults applied.
    pub fn retry_settings(&self) -> RetrySettings {
        let configured = self.settings.retry.clone().unwrap_or_default();
        RetrySettings {
            enabled: Some(configured.enabled.unwrap_or(defaults::RETRY_ENABLED)),
            max_retries: Some(
                configured
                    .max_retries
                    .unwrap_or(defaults::RETRY_MAX_RETRIES),
            ),
            base_delay_ms: Some(
                configured
                    .base_delay_ms
                    .unwrap_or(defaults::RETRY_BASE_DELAY_MS),
            ),
            max_agent_delay_ms: Some(
                configured
                    .max_agent_delay_ms
                    .unwrap_or(defaults::RETRY_MAX_AGENT_DELAY_MS),
            ),
            provider: Some(ProviderRetrySettings {
                timeout_ms: configured.provider.as_ref().and_then(|p| p.timeout_ms),
                max_retries: configured.provider.as_ref().and_then(|p| p.max_retries),
                max_retry_delay_ms: Some(
                    configured
                        .provider
                        .as_ref()
                        .and_then(|p| p.max_retry_delay_ms)
                        .unwrap_or(defaults::RETRY_PROVIDER_MAX_RETRY_DELAY_MS),
                ),
            }),
        }
    }

    /// Agent-level turn retry policy with upstream defaults applied, or `None`
    /// when the `retry` block disables retries (upstream
    /// `getRetrySettings().enabled` gate in `AgentSession._prepareRetry`).
    ///
    /// This is the runtime consumer of [`Self::retry_settings`]: the agent
    /// loop asks for it only when a turn ends with a retryable error. The
    /// provider block is deliberately ignored here — transport-level retries
    /// are configured separately (`StreamOptions::max_retries`) and counting
    /// both would double-spend the budget.
    pub fn agent_retry_policy(&self) -> Option<crate::ai::util::AgentRetryPolicy> {
        Self::retry_policy_from(&self.retry_settings())
    }

    /// [`Self::agent_retry_policy`] over raw (possibly partial) settings: the
    /// upstream defaults fill every absent field, so a host reading its own
    /// settings snapshot (print) resolves the SAME policy the session host
    /// does.
    pub fn retry_policy_from(
        settings: &RetrySettings,
    ) -> Option<crate::ai::util::AgentRetryPolicy> {
        if !settings.enabled.unwrap_or(defaults::RETRY_ENABLED) {
            return None;
        }
        let max_retries = settings.max_retries.unwrap_or(defaults::RETRY_MAX_RETRIES);
        if max_retries == 0 {
            return None;
        }
        Some(crate::ai::util::AgentRetryPolicy {
            max_retries,
            base_delay_ms: settings
                .base_delay_ms
                .unwrap_or(defaults::RETRY_BASE_DELAY_MS),
            max_agent_delay_ms: settings
                .max_agent_delay_ms
                .unwrap_or(defaults::RETRY_MAX_AGENT_DELAY_MS),
        })
    }

    /// Branch summary settings with upstream defaults applied.
    pub fn branch_summary_settings(&self) -> BranchSummarySettings {
        let configured = self.settings.branch_summary.clone().unwrap_or_default();
        BranchSummarySettings {
            reserve_tokens: Some(
                configured
                    .reserve_tokens
                    .unwrap_or(defaults::BRANCH_SUMMARY_RESERVE_TOKENS),
            ),
            skip_prompt: Some(
                configured
                    .skip_prompt
                    .unwrap_or(defaults::BRANCH_SUMMARY_SKIP_PROMPT),
            ),
        }
    }

    /// HTTP idle timeout in milliseconds (upstream
    /// `settings-manager.ts getHttpIdleTimeoutMs`): an explicit number wins
    /// — including `0` (upstream forwards it to the undici dispatcher, which
    /// treats 0 as disabled) — `"disabled"` is `0`, and unset falls back to
    /// the upstream http-dispatcher default (300000 = 5 minutes).
    pub fn http_idle_timeout_ms(&self) -> u64 {
        match self.settings.http_idle_timeout_ms {
            Some(TimeoutSetting::Milliseconds(ms)) => ms,
            Some(TimeoutSetting::Disabled) => 0,
            None => crate::config::defaults::HTTP_IDLE_TIMEOUT_MS,
        }
    }

    /// WebSocket connect timeout override (upstream
    /// `getWebSocketConnectTimeoutMs`); `None` means "unset" and disabled
    /// yields `Some(0)`.
    pub fn websocket_connect_timeout_ms(&self) -> Option<u64> {
        self.settings
            .websocket_connect_timeout_ms
            .map(|setting| match setting {
                TimeoutSetting::Milliseconds(ms) => ms,
                TimeoutSetting::Disabled => 0,
            })
    }

    /// Cache-warming mode, read from the global scope only because each
    /// refresh costs money (upstream `getCacheWarmingMode`).
    pub fn cache_warming_mode(&self) -> CacheWarmingMode {
        self.global_raw
            .get("cacheWarming")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or(CacheWarmingMode::Streaming)
    }

    /// The default project trust policy, read from the global scope only
    /// (upstream `getDefaultProjectTrust`).
    pub fn default_project_trust(&self) -> DefaultProjectTrust {
        self.global_raw
            .get("defaultProjectTrust")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or(DefaultProjectTrust::Ask)
    }

    /// `defaultDisabledPackagesEnabled` (pir-only), read from the GLOBAL
    /// scope only: opting in to a default-disabled package (currently
    /// `@malinamnam/pi-phone`, ~14 MB of QuickJS heap plus a local web
    /// server) is a user-level choice that a repository must not make on the
    /// user's behalf. Unset, `false`, or a non-boolean value keeps the
    /// default-disabled policy. An explicit CLI `-e/--extension <path>`
    /// naming the package is handled by the loader and wins regardless of
    /// this setting.
    pub fn default_disabled_packages_enabled(&self) -> bool {
        self.global_raw
            .get("defaultDisabledPackagesEnabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// `disabledBuiltinExtensions` (upstream settings key): built-in extension
    /// names the loader must drop (`builtin:<name>`), even when settings or an
    /// explicit `-e` enable them.
    ///
    /// The union of every settings layer, never the merged (project-wins)
    /// value: the deep merge replaces arrays wholesale, so a project
    /// `"disabledBuiltinExtensions": []` would otherwise re-enable every
    /// builtin the user disabled in the global scope. Disabling is a
    /// restriction, so a project scope may ADD names and never remove one
    /// (the same rule as `defaultDisabledPackagesEnabled`, which reads the
    /// global scope only); an untrusted project contributes nothing because
    /// its raw scope is empty.
    pub fn disabled_builtin_extensions(&self) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        for raw in [&self.global_raw, &self.project_raw, &self.raw] {
            if let Some(value) = raw.get("disabledBuiltinExtensions") {
                names.extend(disabled_builtin_names(value));
            }
        }
        names
    }

    /// Whether `pythonTool: false` disables the embedded `python` builtin.
    ///
    /// Either scope's explicit `false` wins: a project that sets `true` cannot
    /// re-enable the tool the user disabled globally (settings never widen a
    /// restriction). Unset or a non-boolean value leaves it enabled.
    pub fn python_tool_enabled(&self) -> bool {
        let disables = |raw: &Value| raw.get("pythonTool").and_then(Value::as_bool) == Some(false);
        !(disables(&self.global_raw) || disables(&self.project_raw) || disables(&self.raw))
    }

    /// The names a host must drop from its builtin plan: the
    /// `disabledBuiltinExtensions` union plus `python` when `pythonTool: false`
    /// (the same restriction expressed by its own key). This is the ONE set
    /// the CLI plan builder and the host builders apply; `--no-mcp`/CLI flag
    /// names compose through [`Self::disabled_builtins_with_flags`].
    pub fn disabled_builtins(&self) -> BTreeSet<String> {
        let mut names = self.disabled_builtin_extensions();
        if !self.python_tool_enabled() {
            names.insert("python".to_owned());
        }
        names
    }

    /// [`Self::disabled_builtins`] merged with names a CLI flag already
    /// disabled (`--no-mcp` → `mcp`, see
    /// [`crate::plugins::disabled_from_flags`]).
    ///
    /// This is the single composition point the CLI plan builder and the
    /// tests share: settings and flags only ever ADD names, so a settings
    /// file can never re-enable a builtin the flag disabled (nor the other
    /// way around), and a project scope can never lift a global restriction.
    pub fn disabled_builtins_with_flags(&self, cli_disabled: &[String]) -> BTreeSet<String> {
        let mut names = self.disabled_builtins();
        names.extend(cli_disabled.iter().cloned());
        names
    }

    /// `terminal.clearOnShrink`: settings first, then `PI_CLEAR_ON_SHRINK=1`,
    /// then the default (`false`).
    pub fn clear_on_shrink(&self) -> bool {
        if let Some(enabled) = self
            .settings
            .terminal
            .as_ref()
            .and_then(|terminal| terminal.clear_on_shrink)
        {
            return enabled;
        }
        std::env::var("PI_CLEAR_ON_SHRINK").is_ok_and(|value| value == "1")
    }

    /// `showHardwareCursor`: settings first, then `PI_HARDWARE_CURSOR=1`.
    pub fn show_hardware_cursor(&self) -> bool {
        self.settings
            .show_hardware_cursor
            .unwrap_or_else(|| std::env::var("PI_HARDWARE_CURSOR").is_ok_and(|value| value == "1"))
    }

    /// The resolved `defaultTools` selection, or `None` when no settings layer
    /// sets it (upstream `getDefaultTools`).
    pub fn default_tools(&self) -> Option<Vec<String>> {
        let tools = self.raw.get("defaultTools")?.as_array()?.clone();
        let tools: Vec<String> = tools
            .into_iter()
            .filter_map(|entry| entry.as_str().map(str::to_string))
            .collect();
        Some(resolve_default_tools(&tools))
    }

    /// Install telemetry: the `PI_TELEMETRY` environment override wins over
    /// the setting (upstream args.ts environment table).
    pub fn install_telemetry_enabled(&self) -> bool {
        crate::config::env::telemetry_override().unwrap_or_else(|| {
            self.settings
                .enable_install_telemetry
                .unwrap_or(defaults::ENABLE_INSTALL_TELEMETRY)
        })
    }

    /// Quiet startup: `true`, `"header"`, or `false` (default).
    pub fn quiet_startup(&self) -> QuietStartup {
        self.settings
            .quiet_startup
            .clone()
            .unwrap_or(QuietStartup::Enabled(false))
    }
}

/// One pending write for [`SettingsManager::persist_writes`]: a top-level
/// field, or (when `nested` is set) one nested key inside that field.
struct FieldWrite {
    field: String,
    nested: Option<String>,
    value: Value,
}
