use super::*;

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::sync::atomic::Ordering;

use rquickjs::Object;

pub(super) fn register_session_host_fns(
    host: &Object,
    state: &Arc<StdMutex<RuntimeState>>,
) -> rquickjs::Result<()> {
    macro_rules! sync_fn {
        ($key:literal, $closure:expr) => {
            host.set($key, Func::from($closure))?;
        };
    }

    {
        let st = state.clone();
        sync_fn!("registerExtId", move |ext_id: i64| -> () {
            lock_state(&st).registered_ext_ids.insert(ext_id);
        });
    }
    {
        let st = state.clone();
        sync_fn!("isActive", move |ext_id: i64| -> bool {
            let s = lock_state(&st);
            !s.invalidated && s.registered_ext_ids.contains(&ext_id)
        });
    }
    {
        let st = state.clone();
        sync_fn!("isInvalidated", move || -> bool {
            lock_state(&st).invalidated
        });
    }
    {
        let st = state.clone();
        sync_fn!("getContextJson", move || -> String {
            context_json(&lock_state(&st))
        });
    }
    {
        let st = state.clone();
        sync_fn!("registerFlagJson", move |json: String| -> () {
            if let Ok(flag) = serde_json::from_str::<FlagDef>(&json) {
                let mut s = lock_state(&st);
                if !s.flags.iter().any(|f| f.name == flag.name) {
                    s.flags.push(flag);
                    // Definition-class write: the list is what the plugin
                    // host's flag fingerprint reads, so the host gate's
                    // generation moves with it. A duplicate registration
                    // changes nothing and skips the bump (no read to pay).
                    s.defs_gen = s.defs_gen.wrapping_add(1);
                }
            }
        });
    }
    {
        let st = state.clone();
        sync_fn!("getFlagValue", move |name: String| -> Option<String> {
            let s = lock_state(&st);
            // CLI/explicit value first, then the registered default (upstream
            // `getFlag` resolves the default when no CLI value was set).
            s.flag_values
                .get(&name)
                .or_else(|| {
                    s.flags
                        .iter()
                        .find(|flag| flag.name == name)
                        .and_then(|flag| flag.default.as_ref())
                })
                .map(|v| serde_json::to_string(v).unwrap_or_default())
        });
    }
    {
        let st = state.clone();
        sync_fn!("notify", move |message: String, level: String| -> () {
            lock_state(&st).push(Notification::Notify { message, level });
        });
    }
    {
        let st = state.clone();
        sync_fn!("setStatus", move |key: String,
                                    text: Option<String>|
              -> () {
            lock_state(&st).push(Notification::Status { key, text });
        });
    }
    {
        let st = state.clone();
        sync_fn!("sendMessageJson", move |json: String| -> () {
            if let Ok(payload) = serde_json::from_str::<JsonValue>(&json) {
                lock_state(&st).push(Notification::SendMessage { payload });
            }
        });
    }
    {
        let st = state.clone();
        sync_fn!("sendUserMessageJson", move |json: String| -> () {
            if let Ok(payload) = serde_json::from_str::<JsonValue>(&json) {
                lock_state(&st).push(Notification::SendUserMessage { payload });
            }
        });
    }
    {
        let st = state.clone();
        sync_fn!("appendEntryJson", move |json: String| -> () {
            if let Ok(payload) = serde_json::from_str::<JsonValue>(&json) {
                lock_state(&st).push(Notification::AppendEntry { payload });
            }
        });
    }
    {
        let st = state.clone();
        sync_fn!("setSessionName", move |name: Option<String>| -> () {
            let mut s = lock_state(&st);
            s.session_name = name.clone();
            s.push(Notification::SetSessionName(name));
        });
    }
    {
        let st = state.clone();
        sync_fn!("getSessionName", move || -> Option<String> {
            lock_state(&st).session_name.clone()
        });
    }
    {
        let st = state.clone();
        sync_fn!("setLabel", move |entry_id: String,
                                   label: Option<String>|
              -> () {
            lock_state(&st).push(Notification::SetLabel { entry_id, label });
        });
    }
    {
        let st = state.clone();
        sync_fn!("setTitle", move |title: String| -> () {
            lock_state(&st).push(Notification::SetTitle(title));
        });
    }
    {
        let st = state.clone();
        sync_fn!("setWorkingMessage", move |message: Option<String>| -> () {
            // Upstream `setWorkingMessage(message?)` (types.ts:174-175): no
            // argument restores the default label, encoded here as the
            // empty-string reset signal. A hide request NEVER rides this
            // channel (see `setWorkingVisible`): an empty message means
            // "streaming with the default label" to the TUI.
            lock_state(&st).push(Notification::WorkingMessage(message.unwrap_or_default()));
        });
    }
    {
        let st = state.clone();
        sync_fn!("setWorkingVisible", move |visible: bool| -> () {
            // Upstream `setWorkingVisible(visible)` (types.ts:171-172): show
            // or hide the built-in working loader row. Distinct notification
            // so `false` can never be mistaken for the working-message reset
            // signal (which would leave the TUI believing the run is still
            // streaming).
            lock_state(&st).push(Notification::WorkingVisible(visible));
        });
    }
    {
        let st = state.clone();
        sync_fn!("setEditorText", move |text: String| -> () {
            lock_state(&st).push(Notification::EditorSetText(text));
        });
    }
    {
        let st = state.clone();
        sync_fn!("pasteToEditor", move |text: String| -> () {
            lock_state(&st).push(Notification::EditorPaste(text));
        });
    }
    {
        let st = state.clone();
        sync_fn!("getEditorTextValue", move || -> String {
            lock_state(&st).editor_text.clone()
        });
    }
    {
        let st = state.clone();
        sync_fn!("getToolsExpanded", move || -> bool {
            // `ctx.ui.getToolsExpanded()` (upstream types.ts:296-299): reads
            // the ONE shared cell the TUI's tool blocks and Ctrl+O toggle
            // use. No JS-local mirror exists, so a TUI toggle is visible on
            // the next call.
            lock_state(&st).tools_expanded.load(Ordering::SeqCst)
        });
    }
    {
        let st = state.clone();
        sync_fn!("setToolsExpanded", move |expanded: bool| -> () {
            // `ctx.ui.setToolsExpanded(expanded)`: writes the shared cell the
            // live TUI reads per frame (the prelude coerces the argument to
            // a boolean, matching the upstream `setExpanded(boolean)`).
            lock_state(&st)
                .tools_expanded
                .store(expanded, Ordering::SeqCst);
        });
    }
    {
        let st = state.clone();
        sync_fn!("autocompleteProviderRegistered", move || -> () {
            lock_state(&st).has_autocomplete_suggesters = true;
            lock_state(&st).push(Notification::AutocompleteProviderRegistered);
        });
    }
    {
        let st = state.clone();
        sync_fn!("handlersDirty", move || -> () {
            // JS handler-table mutation (`pi.on` registration / unsubscribe,
            // plus the provider `refreshModels` row `pi.registerProvider`
            // installs directly): bump the generation so the fast-path
            // `has_handlers` gate stops trusting `handler_counts` until a
            // refresh re-publishes it with a matching `handlers_snapshot_gen`.
            // The whole signal is this increment — no notification, no JS
            // work. Called from JS with a `typeof` guard, so a host-less
            // prelude evaluation simply skips it.
            let mut s = lock_state(&st);
            s.handlers_gen = s.handlers_gen.wrapping_add(1);
        });
    }
    {
        let st = state.clone();
        sync_fn!("terminalInputHandlers", move |count: i64| -> () {
            // Live `ctx.ui.onTerminalInput` handler count (upstream
            // `ExtensionUIContext.onTerminalInput`). The prelude calls this on
            // every registration/unregistration, so the TUI's per-input fast
            // path is an atomic load on the handle
            // (`ExtensionHandle::has_terminal_input_handlers`) instead of a
            // runtime-state lock. A changed count also marks the handler table
            // dirty, keeping `has_handlers("terminal_input")` conservative
            // until the next counts publication.
            let mut s = lock_state(&st);
            let count = usize::try_from(count.max(0)).unwrap_or(usize::MAX);
            let previous = s.terminal_input_handlers.swap(count, Ordering::SeqCst);
            if previous != count {
                s.handlers_gen = s.handlers_gen.wrapping_add(1);
            }
        });
    }
    {
        let st = state.clone();
        sync_fn!("registryDirty", move || -> () {
            // JS registry mutation (`pi.registerTool` / `registerCommand` /
            // `registerShortcut` / the `register*Renderer` family /
            // `ctx.ui.setWidget` factory registrations / the load-discard
            // cleanup): bump the generation so the next
            // `ExtensionRuntime::refresh_registry` re-exports the registry
            // (tools, commands, shortcuts, renderers) instead of early-outing
            // on an unchanged generation. Like `handlersDirty`, the whole
            // signal is this increment. Called from JS with a `typeof` guard,
            // so a host-less prelude evaluation simply skips it.
            //
            // The same call also writes JS-definition classes the plugin
            // host's registry gate fingerprints (tools, commands, shortcuts),
            // so `defs_gen` — the host gate's O(1) probe — moves too. It is
            // kept separate from `registry_gen` on purpose: `registry_gen`
            // additionally drives the runtime's own expensive
            // `__pir_exportRegistryJson` re-export, which the definition
            // classes written through the other host fns (flags, providers,
            // MCP servers, virtual models) must never force.
            let mut s = lock_state(&st);
            s.registry_gen = s.registry_gen.wrapping_add(1);
            s.defs_gen = s.defs_gen.wrapping_add(1);
        });
    }
    // NOTE: the `messageUpdateSnapshotJson` host function was REMOVED with the
    // per-delta wire lane (third round). It existed so the JS delta accumulator
    // could pull the authoritative cumulative snapshot of one dispatch when it
    // hit a delta shape it could not apply — a reply-sized host call whose
    // source `Arc` (a runtime state slot) had to stay alive
    // across the dispatch, which pinned the provider parser's accumulated
    // message and turned its copy-on-write mutation into one whole-reply deep
    // copy per delta (~630 MB of churn on a 10 000-delta stream). The host now
    // classifies each event up front
    // (`extensions::runtime::message_update_stream_json`): shapes the JS
    // accumulator knows ship as O(delta) delta lines, everything else ships as
    // the full reference payload — no host round trip, no reply-sized source to
    // retain. The Rust-side serializer
    // (`extensions::runtime::message_update_snapshot_json`) is kept as the wire
    // tests' reference.

    // -- ctx.ui theme API (host-pushed registry; TUI applies setTheme live) --

    {
        let st = state.clone();
        sync_fn!("getThemesJson", move || -> String {
            let registry = &lock_state(&st).theme_registry;
            let themes = registry
                .get("themes")
                .cloned()
                .unwrap_or(JsonValue::Array(Vec::new()));
            serde_json::json!({ "themes": themes }).to_string()
        });
    }
    {
        let st = state.clone();
        sync_fn!("getThemeJson", move || -> Option<String> {
            let registry = &lock_state(&st).theme_registry;
            let current = registry.get("current").and_then(JsonValue::as_str)?;
            registry
                .get("snapshots")
                .and_then(|snapshots| snapshots.get(current))
                .map(|snapshot| snapshot.to_string())
        });
    }
    {
        let st = state.clone();
        sync_fn!(
            "getThemeJsonByName",
            move |name: String| -> Option<String> {
                let registry = &lock_state(&st).theme_registry;
                registry
                    .get("snapshots")
                    .and_then(|snapshots| snapshots.get(name))
                    .map(|snapshot| snapshot.to_string())
            }
        );
    }
    {
        let st = state.clone();
        sync_fn!("setThemeJson", move |name: String| -> String {
            let mut s = lock_state(&st);
            let known = s
                .theme_registry
                .get("themes")
                .and_then(JsonValue::as_array)
                .is_some_and(|themes| {
                    themes.iter().any(|theme| {
                        theme.get("name").and_then(JsonValue::as_str) == Some(name.as_str())
                    })
                });
            if !known {
                return serde_json::json!({
                    "success": false,
                    "error": format!("unknown theme: {name}"),
                })
                .to_string();
            }
            s.theme_registry["current"] = JsonValue::String(name.clone());
            s.push(Notification::SetTheme(name));
            serde_json::json!({ "success": true }).to_string()
        });
    }
    {
        let st = state.clone();
        sync_fn!("setWidgetJson", move |id: String,
                                        content_json: Option<String>|
              -> () {
            let content = content_json
                .and_then(|json| serde_json::from_str::<JsonValue>(&json).ok())
                .unwrap_or(JsonValue::Null);
            lock_state(&st).push(Notification::Widget { id, content });
        });
    }
    {
        let st = state.clone();
        sync_fn!("setFooterJson", move |active: bool| -> () {
            // `ctx.ui.setFooter(factory|undefined)` (upstream types.ts:195-204):
            // the prelude registers/removes the JS factory slot; the TUI
            // renders its component through the `footerRenderers` renderer
            // round-trip (or restores the built-in footer on `false`).
            lock_state(&st).push(Notification::Footer(active));
        });
    }
    {
        let st = state.clone();
        sync_fn!("setHeaderJson", move |active: bool| -> () {
            // `ctx.ui.setHeader(factory|undefined)` (upstream types.ts:206-208):
            // same pipeline as the footer, rendered as the top bar.
            lock_state(&st).push(Notification::Header(active));
        });
    }
    {
        let st = state.clone();
        sync_fn!("setWorkingIndicatorJson", move |json: Option<String>| {
            // `ctx.ui.setWorkingIndicator(options?)` (upstream
            // types.ts:176-184): `null` restores the default spinner;
            // otherwise the parsed `{frames?, intervalMs?}` options the TUI
            // applies to its live indicator (`frames: []` = hidden).
            let options = json
                .and_then(|json| serde_json::from_str::<JsonValue>(&json).ok())
                .unwrap_or(JsonValue::Null);
            lock_state(&st).push(Notification::WorkingIndicator(options));
        });
    }
    {
        let st = state.clone();
        sync_fn!(
            "setHiddenThinkingLabel",
            move |label: Option<String>| -> () {
                // `ctx.ui.setHiddenThinkingLabel(label?)` (upstream
                // types.ts:185): `null` restores the default label.
                lock_state(&st).push(Notification::HiddenThinkingLabel(label));
            }
        );
    }
    {
        let st = state.clone();
        sync_fn!("getActiveToolsJson", move || -> String {
            let s = lock_state(&st);
            // The override wins; without one the active set is every
            // host-pushed def (core tools plus Rust-backend plugin tools)
            // plus every extension-registered tool.
            match &s.active_tools_override {
                Some(names) => serde_json::to_string(names).unwrap_or_else(|_| "[]".into()),
                None => {
                    // Borrows only: serializing `&str` avoids cloning every host
                    // tool name plus every not-yet-listed extension tool name.
                    let mut names: Vec<&str> =
                        s.core_tools.iter().map(|def| def.name.as_str()).collect();
                    for def in &s.tool_defs {
                        if !names.contains(&def.name.as_str()) {
                            names.push(def.name.as_str());
                        }
                    }
                    serde_json::to_string(&names).unwrap_or_else(|_| "[]".into())
                }
            }
        });
    }
    {
        let st = state.clone();
        sync_fn!("getAllToolsJson", move || -> String {
            let mut s = lock_state(&st);
            // Memoized: this host fn runs on every tool-context creation and
            // the array embeds every parameter schema. The fingerprint is
            // recomputed per call (a hash pass over the same inputs, no JSON
            // string built) so ANY change to the inputs — core defs,
            // extension defs, or a per-tool extra — invalidates the entry.
            let fingerprint = all_tools_fingerprint(&s);
            if let Some(fingerprint) = fingerprint {
                if let Some((cached, json)) = &s.all_tools_json {
                    if *cached == fingerprint {
                        return json.clone();
                    }
                }
            }
            let json = build_all_tools_json(&s);
            if let Some(fingerprint) = fingerprint {
                s.all_tools_json = Some((fingerprint, json.clone()));
            }
            json
        });
    }
    {
        let st = state.clone();
        sync_fn!("setActiveToolsJson", move |json: String| -> () {
            let names: Vec<String> = serde_json::from_str(&json).unwrap_or_default();
            let mut s = lock_state(&st);
            // Persist the override in runtime state so `getActiveTools`
            // reflects it immediately and the session host can apply it
            // when building the tool registry.
            s.active_tools_override = Some(names.clone());
            s.push(Notification::ActiveToolsOverride(names));
        });
    }
    {
        let st = state.clone();
        sync_fn!("getCommandsJson", move || -> String {
            let s = lock_state(&st);
            // Builtin (Rust plugin) commands first, then extension commands —
            // every entry carries the upstream `source` tag.
            // Borrows only: serialize both slices in order without cloning every
            // command def (builtin first, then extension commands).
            let defs: Vec<&CommandDef> = s
                .builtin_commands
                .iter()
                .chain(s.command_defs.iter())
                .collect();
            serde_json::to_string(&defs).unwrap_or_else(|_| "[]".into())
        });
    }
    {
        let st = state.clone();
        sync_fn!("getSettingsJson", move || -> String {
            lock_state(&st).settings.to_string()
        });
    }
    {
        let st = state.clone();
        sync_fn!("hasUI", move || -> bool { lock_state(&st).config.has_ui });
    }
    {
        let st = state.clone();
        sync_fn!("toolUpdate", move |tool_call_id: String,
                                     tool_name: Option<String>,
                                     args_json: Option<String>,
                                     partial: Option<String>|
              -> () {
            let parsed = partial.and_then(|p| serde_json::from_str::<JsonValue>(&p).ok());
            let args = args_json.and_then(|a| serde_json::from_str::<JsonValue>(&a).ok());
            // Nested call ids are `<parent>/<n>`; the parent prefix flows back
            // on the `tool_execution_update` payload.
            let parent_call_id = tool_call_id
                .rsplit_once('/')
                .map(|(parent, _)| parent.to_owned());
            lock_state(&st).push(Notification::ToolUpdate {
                tool_call_id,
                tool_name,
                args,
                parent_call_id,
                partial: parsed,
            });
        });
    }
    {
        let st = state.clone();
        sync_fn!("abort", move || -> () {
            {
                let mut s = lock_state(&st);
                s.tool_call_aborted = true;
                s.push(Notification::Abort);
            }
        });
    }
    {
        let st = state.clone();
        sync_fn!("isAborted", move || -> bool {
            lock_state(&st).tool_call_aborted
        });
    }
    {
        // streamSimple options hooks (upstream `SimpleStreamOptions.onPayload`
        // / `onResponse` / `onProviderStreamEvent`): the JS callback's
        // `options.onPayload(payload)` / `onResponse(status, headers)` /
        // `onProviderStreamEvent(event)` wrappers land here. Fire-and-forget:
        // an unbound session, a malformed payload, or a hook error never
        // interrupts the stream (the prelude wrapper also isolates throws).
        let st = state.clone();
        sync_fn!("streamHookEvent", move |kind: String,
                                          session: String,
                                          payload_json: String|
              -> () {
            let Ok(payload) = serde_json::from_str::<JsonValue>(&payload_json) else {
                return;
            };
            let bridge = {
                let s = lock_state(&st);
                s.stream_hook_bridges.get(&session).cloned()
            };
            let Some(bridge) = bridge else { return };
            match kind.as_str() {
                "payload" => {
                    if let Some(hook) = &bridge.on_payload {
                        // Observation-only: mutations cannot round-trip to JS
                        // (the JS callback owns the request body).
                        let mut value = payload;
                        hook(&mut value);
                    }
                }
                "response" => {
                    if let Some(hook) = &bridge.on_response {
                        let status = payload
                            .get("status")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0) as u16;
                        let mut headers = std::collections::BTreeMap::new();
                        if let Some(map) = payload.get("headers").and_then(JsonValue::as_object) {
                            for (key, value) in map {
                                if let Some(text) = value.as_str() {
                                    headers.insert(key.clone(), text.to_owned());
                                }
                            }
                        }
                        hook(status, &headers);
                    }
                }
                "event" => {
                    if let Some(hook) = &bridge.on_provider_stream_event {
                        hook(&payload);
                    }
                }
                _ => {}
            }
        });
    }

    Ok(())
}

/// The serialized `getAllToolsJson` array: the host-pushed defs first (core
/// builtin tools plus Rust-backend plugin tools — REAL descriptions and
/// parameter schemas), then the extension tool defs with the wire extras
/// merged back in: the `reachesTools` marker (upstream
/// `ToolDefinition.reachesTools`; `pi.getAllTools()` re-attaches the live hook
/// in QuickJS) and the raw `namespace` object with its `aliases` (upstream
/// `ToolNamespace.aliases`).
fn build_all_tools_json(s: &RuntimeState) -> String {
    let mut defs: Vec<JsonValue> = Vec::with_capacity(s.core_tools.len() + s.tool_defs.len());
    // First name wins with CORE first, exactly like
    // `merge_plugin_tools` builds the run registry: a name appears at most
    // once, so an extension tool shadowing a core tool adds no second row
    // (the core def is the one the run registry resolves).
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for def in &s.core_tools {
        if seen.insert(def.name.as_str()) {
            if let Ok(value) = serde_json::to_value(def) {
                defs.push(value);
            }
        }
    }
    for def in &s.tool_defs {
        if !seen.insert(def.name.as_str()) {
            continue;
        }
        if let Ok(mut value) = serde_json::to_value(def) {
            if let Some(extras) = s.tool_extras.get(&def.name) {
                if let Some(object) = value.as_object_mut() {
                    if let Some(namespace) = extras.get("namespace") {
                        if !namespace.is_null() {
                            object.insert("namespace".to_owned(), namespace.clone());
                        }
                    }
                    if extras
                        .get("reachesTools")
                        .and_then(JsonValue::as_bool)
                        .unwrap_or(false)
                    {
                        object.insert("reachesTools".to_owned(), JsonValue::Bool(true));
                    }
                }
            }
            defs.push(value);
        }
    }
    serde_json::to_string(&defs).unwrap_or_else(|_| "[]".into())
}

/// Fingerprint of every input [`build_all_tools_json`] reads, computed by
/// hashing the inputs' serialized form into a [`DefaultHasher`] — no JSON
/// string and no `JsonValue` is materialized (an allocation-free walk).
/// `None` when the hash walk fails (it cannot for these types); the caller
/// then skips the memo instead of risking a stale hit.
///
/// Exactness: the hash covers the complete inputs, so a schema-only edit under
/// an unchanged tool name still invalidates the memo. `tool_extras` hashes as
/// a map (its iteration order is stable for one map instance); a wholesale
/// replacement with an equivalent map may change the hash and cost exactly one
/// rebuild — conservative, never stale.
fn all_tools_fingerprint(s: &RuntimeState) -> Option<u64> {
    let mut hasher = DefaultHasher::new();
    let mut sink = HashSink(&mut hasher);
    serde_json::to_writer(&mut sink, &(&s.core_tools, &s.tool_defs, &s.tool_extras)).ok()?;
    Some(hasher.finish())
}

/// `io::Write` adapter feeding a [`DefaultHasher`]: lets `serde_json::to_writer`
/// fingerprint a value without building its JSON text.
struct HashSink<'a>(&'a mut DefaultHasher);

impl std::io::Write for HashSink<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
