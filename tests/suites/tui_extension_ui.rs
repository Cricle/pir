//! Extension UI surface tests: dialog overlay state machines, renderer
//! marshalling through the extension actor (real QuickJS host), autocomplete
//! merge, editor text round-trip and the theme registry exposure.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::KeyCode;
use serde_json::{json, Value};

use pir::extensions::{ExtensionConfig, ExtensionHandle, Notification};
use pir::tui::ansi::strip_ansi;
use pir::tui::autocomplete::{AutocompleteItem, AutocompletePopup, AutocompleteSuggestions};
use pir::tui::core::Component;
use pir::tui::dialogs::{dialog_key_from_crossterm, DialogKey, DialogOutcome, DialogOverlay};
use pir::tui::editor::Editor;
use pir::tui::renderers::{
    factory_slot_needs_render, mark_factory_slot_dirty, refresh_factory_bars, renderer_registered,
    set_factory_slot, ExtensionRendererInvoker, RendererInvoker, RendererKind, RendererOutcome,
    RendererRequest, RENDERER_CALL_TIMEOUT,
};
use pir::tui::theme::{
    all_theme_entries, current_theme_name, is_known_theme, parse_theme_json, register_custom_theme,
    register_custom_theme_path, set_theme, theme_snapshot,
};

// ---------------------------------------------------------------------------
// Dialog state machines
// ---------------------------------------------------------------------------

fn select_options() -> Value {
    json!({
        "title": "Pick one",
        "options": [
            { "value": "a", "label": "Alpha" },
            { "value": "b", "label": "Beta" },
        ],
    })
}

#[test]
fn select_dialog_submit_cancel_and_filter() {
    let mut dialog = DialogOverlay::from_request(1, "select", &select_options());
    assert_eq!(dialog.handle_key(DialogKey::Down), DialogOutcome::None);
    assert_eq!(
        dialog.handle_key(DialogKey::Enter),
        DialogOutcome::Submit(json!("b"))
    );

    let mut dialog = DialogOverlay::from_request(2, "select", &select_options());
    assert_eq!(dialog.handle_key(DialogKey::Esc), DialogOutcome::Cancel);

    // Default option preselection (upstream `default` index).
    let with_default = json!({
        "title": "Pick one",
        "default": 1,
        "options": [
            { "value": "a", "label": "Alpha" },
            { "value": "b", "label": "Beta" },
        ],
    });
    let mut dialog = DialogOverlay::from_request(3, "select", &with_default);
    assert_eq!(
        dialog.handle_key(DialogKey::Enter),
        DialogOutcome::Submit(json!("b"))
    );

    // Type-to-filter narrows the list; the enter selection follows the
    // filtered order.
    let mut dialog = DialogOverlay::from_request(4, "select", &select_options());
    for c in "alph".chars() {
        assert_eq!(dialog.handle_key(DialogKey::Char(c)), DialogOutcome::None);
    }
    assert_eq!(
        dialog.handle_key(DialogKey::Enter),
        DialogOutcome::Submit(json!("a"))
    );
}

#[test]
fn select_dialog_string_options_and_malformed_payloads() {
    // String options normalize to `{value: s, label: s}` (prelude shape).
    let options = json!({ "title": "t", "options": ["only"] });
    let mut dialog = DialogOverlay::from_request(1, "select", &options);
    assert_eq!(
        dialog.handle_key(DialogKey::Enter),
        DialogOutcome::Submit(json!("only"))
    );

    // Malformed payloads still produce a cancellable dialog (never wedge the
    // awaiting JS prompt).
    let mut dialog = DialogOverlay::from_request(2, "select", &json!({}));
    assert_eq!(dialog.handle_key(DialogKey::Enter), DialogOutcome::Cancel);
    assert_eq!(dialog.handle_key(DialogKey::Esc), DialogOutcome::Cancel);
}

#[test]
fn confirm_dialog_yes_no_default_and_escape() {
    let options = json!({ "title": "Sure?", "message": "Proceed with the deploy?" });
    let mut dialog = DialogOverlay::from_request(1, "confirm", &options);
    assert_eq!(
        dialog.handle_key(DialogKey::Char('y')),
        DialogOutcome::Submit(Value::Bool(true))
    );

    // Bare Enter submits the configured default (false).
    let mut dialog = DialogOverlay::from_request(2, "confirm", &options);
    assert_eq!(
        dialog.handle_key(DialogKey::Enter),
        DialogOutcome::Submit(Value::Bool(false))
    );

    // defaultYes flips the Enter default.
    let mut dialog =
        DialogOverlay::from_request(3, "confirm", &json!({ "title": "t", "defaultYes": true }));
    assert_eq!(
        dialog.handle_key(DialogKey::Enter),
        DialogOutcome::Submit(Value::Bool(true))
    );

    // Toggle + Enter.
    let mut dialog = DialogOverlay::from_request(4, "confirm", &options);
    assert_eq!(dialog.handle_key(DialogKey::Right), DialogOutcome::None);
    assert_eq!(
        dialog.handle_key(DialogKey::Enter),
        DialogOutcome::Submit(Value::Bool(true))
    );

    let mut dialog = DialogOverlay::from_request(5, "confirm", &options);
    assert_eq!(dialog.handle_key(DialogKey::Esc), DialogOutcome::Cancel);
    // n submits false (not cancel).
    let mut dialog = DialogOverlay::from_request(6, "confirm", &options);
    assert_eq!(
        dialog.handle_key(DialogKey::Char('n')),
        DialogOutcome::Submit(Value::Bool(false))
    );
}

#[test]
fn input_dialog_round_trip_with_default_and_placeholder() {
    let options = json!({
        "title": "Name",
        "placeholder": "your name",
        "defaultValue": "ada",
    });
    let mut dialog = DialogOverlay::from_request(1, "input", &options);
    for c in "lovelace".chars() {
        dialog.handle_key(DialogKey::Char(c));
    }
    assert_eq!(dialog.handle_key(DialogKey::Backspace), DialogOutcome::None);
    assert_eq!(
        dialog.handle_key(DialogKey::Enter),
        DialogOutcome::Submit(json!("adalovelac"))
    );

    // Esc cancels; the empty default submits an empty string.
    let mut dialog = DialogOverlay::from_request(2, "input", &options);
    assert_eq!(dialog.handle_key(DialogKey::Esc), DialogOutcome::Cancel);
    let mut dialog = DialogOverlay::from_request(3, "input", &json!({ "title": "t" }));
    assert_eq!(
        dialog.handle_key(DialogKey::Enter),
        DialogOutcome::Submit(json!(""))
    );
    // Vertical keys are inert on the single-line input.
    let mut dialog = DialogOverlay::from_request(4, "input", &options);
    assert_eq!(dialog.handle_key(DialogKey::Up), DialogOutcome::None);
    assert_eq!(dialog.handle_key(DialogKey::Down), DialogOutcome::None);
}

#[test]
fn editor_dialog_prefill_newline_and_submit() {
    let options = json!({ "title": "Edit", "prefill": "line one" });
    let mut dialog = DialogOverlay::from_request(1, "editor", &options);
    assert!(dialog.is_editor());
    dialog.editor_newline();
    for c in "line two".chars() {
        dialog.handle_key(DialogKey::Char(c));
    }
    assert_eq!(
        dialog.handle_key(DialogKey::Enter),
        DialogOutcome::Submit(json!("line one\nline two"))
    );
    let mut dialog = DialogOverlay::from_request(2, "editor", &options);
    assert_eq!(dialog.handle_key(DialogKey::Esc), DialogOutcome::Cancel);
}

#[test]
fn unsupported_dialog_kind_never_wedges() {
    let mut dialog = DialogOverlay::from_request(1, "custom-overlay", &json!({ "title": "?" }));
    // The unknown kind renders a confirm-shaped body; Esc always cancels.
    assert_eq!(dialog.handle_key(DialogKey::Esc), DialogOutcome::Cancel);
}

#[test]
fn crossterm_key_mapping() {
    use crossterm::event::KeyModifiers as KM;
    assert_eq!(
        dialog_key_from_crossterm(KeyCode::Up, KM::NONE),
        Some(DialogKey::Up)
    );
    assert_eq!(
        dialog_key_from_crossterm(KeyCode::Enter, KM::NONE),
        Some(DialogKey::Enter)
    );
    assert_eq!(
        dialog_key_from_crossterm(KeyCode::Char('y'), KM::NONE),
        Some(DialogKey::Char('y'))
    );
    // Ctrl+C is `tui.select.cancel` (upstream keybindings default:
    // escape + ctrl+c for every dialog component): it maps onto the shared
    // Esc path instead of being dropped.
    assert_eq!(
        dialog_key_from_crossterm(KeyCode::Char('c'), KM::CONTROL),
        Some(DialogKey::Esc)
    );
    // Ctrl+J is the editor-newline binding (most terminals report
    // Shift+Enter as plain Enter, so Ctrl+J is the reliable multi-line key).
    assert_eq!(
        dialog_key_from_crossterm(KeyCode::Char('j'), KM::CONTROL),
        Some(DialogKey::Newline)
    );
    assert_eq!(
        dialog_key_from_crossterm(KeyCode::Esc, KM::NONE),
        Some(DialogKey::Esc)
    );
}

#[test]
fn editor_dialog_ctrl_j_inserts_newline() {
    // The mapped Ctrl+J key inserts a newline in the editor dialog instead
    // of submitting (the hint advertises "Ctrl+J newline").
    let options = json!({ "title": "Edit", "prefill": "one" });
    let mut dialog = DialogOverlay::from_request(1, "editor", &options);
    assert_eq!(
        dialog.handle_key(DialogKey::Newline),
        DialogOutcome::None,
        "Ctrl+J does not submit"
    );
    for c in "two".chars() {
        dialog.handle_key(DialogKey::Char(c));
    }
    assert_eq!(
        dialog.handle_key(DialogKey::Enter),
        DialogOutcome::Submit(json!("one\ntwo")),
        "the newline landed in the buffer"
    );
    // Plain Enter still submits, not newline.
    let mut dialog = DialogOverlay::from_request(2, "editor", &options);
    assert!(matches!(
        dialog.handle_key(DialogKey::Enter),
        DialogOutcome::Submit(_)
    ));
    // In the single-line input Ctrl+J is inert (no newline, no submit).
    let mut dialog = DialogOverlay::from_request(3, "input", &json!({ "title": "t" }));
    assert_eq!(dialog.handle_key(DialogKey::Newline), DialogOutcome::None);
    assert_eq!(
        dialog.handle_key(DialogKey::Enter),
        DialogOutcome::Submit(json!(""))
    );
}

#[test]
fn dialog_overlay_renders_framed_lines() {
    // Rendering smoke test (line-based frame assertions): title, option
    // labels and hint footer all appear in the padded frame output.
    let dialog = DialogOverlay::from_request(1, "select", &select_options());
    let lines = dialog.render(60);
    assert!(lines.len() > 6, "expected a framed overlay, got {lines:?}");
    let plain: Vec<String> = lines.iter().map(|l| strip_ansi(l)).collect();
    let joined = plain.join("\n");
    assert!(joined.contains("Pick one"), "title missing: {joined}");
    assert!(joined.contains("Alpha"), "option missing: {joined}");
    assert!(joined.contains("Beta"), "option missing: {joined}");
    // Upstream hint `↑↓ navigate  enter select  esc cancel`
    // (extension-selector.ts:68-78).
    assert!(joined.contains("esc cancel"), "hint missing: {joined}");

    let confirm = DialogOverlay::from_request(
        2,
        "confirm",
        &json!({ "title": "Sure?", "message": "Proceed?" }),
    );
    let lines = confirm.render(60);
    let joined = lines
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    // Upstream showExtensionConfirm is a Yes/No selector
    // (interactive-mode.ts:2750-2759).
    assert!(joined.contains("Yes") && joined.contains("No"), "{joined}");

    let input = DialogOverlay::from_request(
        3,
        "input",
        &json!({ "title": "Name", "placeholder": "type here" }),
    );
    let lines = input.render(60);
    let joined = lines
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join("\n");
    // Upstream ExtensionInputComponent ignores the placeholder and shows the
    // plain `> ` input row with the `enter submit  esc cancel` footer
    // (extension-input.ts:37,69-75).
    assert!(
        joined.contains("enter submit"),
        "input hint missing: {joined}"
    );
}

// ---------------------------------------------------------------------------
// Renderer marshalling through the real extension actor
// ---------------------------------------------------------------------------

fn fixture_dir(test_name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pir-tui-ext-ui-{}-{test_name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("fixture dir");
    dir
}

fn start_handle(dir: &std::path::Path) -> ExtensionHandle {
    ExtensionHandle::start(ExtensionConfig {
        cwd: dir.to_path_buf(),
        mode: "interactive".to_string(),
        has_ui: true,
        model: Some("test-model".to_string()),
        session_id: None,
        system_prompt: String::new(),
        is_idle: true,
        limits: Some(Default::default()),
    })
    .expect("runtime start")
}

fn load_extension(handle: &ExtensionHandle, dir: &std::path::Path, name: &str, source: &str) {
    std::fs::write(dir.join(name), source).expect("write fixture");
    let path = dir.join(name);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let result = runtime.block_on(handle.load(std::slice::from_ref(&path)));
    let result = result.expect("load");
    assert!(
        result.errors.is_empty(),
        "extension load errors: {:?}",
        result.errors
    );
}

fn invoke(
    invoker: &dyn RendererInvoker,
    kind: RendererKind,
    key: &str,
    payload: Value,
) -> anyhow::Result<pir::tui::renderers::RendererOutcome> {
    invoker.invoke(&RendererRequest {
        kind,
        key: key.to_string(),
        payload,
        options: json!({ "expanded": false, "width": 60, "messageType": "user" }),
    })
}

const MESSAGE_RENDERER_TS: &str = r#"
export default function (pi) {
  pi.registerMessageRenderer("plan", (message, options, theme) => {
    if (message.fail) throw new Error("boom");
    return ["PLAN: " + message.title, "  steps: " + (message.steps ?? 0)];
  });
  pi.registerMessageRenderer("component", () => ({
    render: (width) => ["W" + width + "-line"],
  }));
  pi.registerMessageRenderer("undefined-case", () => undefined);
  pi.registerEntryRenderer("widget", (entry) => ["WIDGET " + entry.data.name]);
  pi.registerMarkdownTransformer("shout", (markdown, context) =>
    markdown.toUpperCase() + "|" + context.messageType);
  pi.registerToolRenderer("bash", async (tool, next) => {
    const base = next ? await next() : undefined;
    if (base) return base;
    return ["BASH> " + tool.args.command];
  });
}
"#;

#[test]
fn renderer_marshalling_lines_components_and_fallbacks() {
    let dir = fixture_dir("renderers");
    let handle = start_handle(&dir);
    load_extension(&handle, &dir, "renderers.ts", MESSAGE_RENDERER_TS);
    let invoker = ExtensionRendererInvoker::new(Arc::new(handle));

    // Message renderer returning styled lines.
    let outcome = invoke(
        &invoker,
        RendererKind::Message,
        "plan",
        json!({ "title": "Refactor", "steps": 3 }),
    )
    .expect("renderer invoke");
    assert_eq!(
        outcome.lines,
        Some(vec!["PLAN: Refactor".to_string(), "  steps: 3".to_string()])
    );

    // Component-like result adapted through render(width).
    let outcome =
        invoke(&invoker, RendererKind::Message, "component", json!({})).expect("component invoke");
    assert_eq!(outcome.lines, Some(vec!["W60-line".to_string()]));

    // undefined result → unhandled (default rendering stays).
    let outcome = invoke(&invoker, RendererKind::Message, "undefined-case", json!({}))
        .expect("undefined invoke");
    assert!(outcome.is_unhandled());

    // No registration for the customType → unhandled.
    let outcome =
        invoke(&invoker, RendererKind::Message, "missing", json!({})).expect("missing invoke");
    assert!(outcome.is_unhandled());

    // Throwing renderer → Err (degrades at the call site).
    let error = invoke(
        &invoker,
        RendererKind::Message,
        "plan",
        json!({ "fail": true }),
    )
    .expect_err("throwing renderer must error");
    assert!(error.to_string().contains("boom"));

    // Entry renderer.
    let outcome = invoke(
        &invoker,
        RendererKind::Entry,
        "widget",
        json!({ "data": { "name": "status" } }),
    )
    .expect("entry invoke");
    assert_eq!(outcome.lines, Some(vec!["WIDGET status".to_string()]));

    // Markdown transformer returns transformed text.
    let outcome = invoke(
        &invoker,
        RendererKind::Markdown,
        "",
        json!({ "markdown": "hello" }),
    )
    .expect("markdown invoke");
    assert_eq!(outcome.text.as_deref(), Some("HELLO|user"));

    // Tool renderer resolver chain: next() is undefined → our lines.
    let outcome = invoke(
        &invoker,
        RendererKind::Tool,
        "bash",
        json!({ "args": { "command": "ls" } }),
    )
    .expect("tool invoke");
    assert_eq!(outcome.lines, Some(vec!["BASH> ls".to_string()]));
}

#[test]
fn renderer_runaway_loop_times_out_bounded() {
    let dir = fixture_dir("renderer-timeout");
    let handle = start_handle(&dir);
    load_extension(
        &handle,
        &dir,
        "spinner.ts",
        r#"
export default function (pi) {
  pi.registerMessageRenderer("loop", () => {
    while (true) {}
  });
}
"#,
    );
    let invoker = ExtensionRendererInvoker::new(Arc::new(handle));
    let started = std::time::Instant::now();
    let error = invoke(&invoker, RendererKind::Message, "loop", json!({}))
        .expect_err("runaway renderer must time out");
    let elapsed = started.elapsed();
    // The rejection must be the renderer deadline firing, not an unrelated
    // early failure: prove BOTH bounds.
    assert!(
        elapsed >= RENDERER_CALL_TIMEOUT,
        "the renderer deadline must have fired, took {elapsed:?} (deadline {RENDERER_CALL_TIMEOUT:?})"
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "timeout must be bounded, took {elapsed:?}"
    );
    let _ = error;
    // The runtime stays usable after the interrupt.
    let outcome = invoke(&invoker, RendererKind::Message, "loop", json!({}));
    assert!(outcome.is_err());
}

#[test]
fn renderer_defs_registry_resolution() {
    let defs = json!({
        "messageRenderers": [{ "customType": "plan", "extId": 1 }],
        "entryRenderers": [],
        "markdownTransformers": [{ "name": "shout", "extId": 1 }],
        "toolRenderers": [{ "toolName": "bash", "extId": 2 }],
    });
    assert!(renderer_registered(&defs, RendererKind::Message, "plan"));
    assert!(!renderer_registered(&defs, RendererKind::Message, "other"));
    assert!(renderer_registered(&defs, RendererKind::Markdown, "shout"));
    // Empty key = any transformer registered (single-transformer surface).
    assert!(renderer_registered(&defs, RendererKind::Markdown, ""));
    assert!(!renderer_registered(
        &defs,
        RendererKind::Markdown,
        "missing"
    ));
    assert!(renderer_registered(&defs, RendererKind::Tool, "bash"));
    assert!(!renderer_registered(&defs, RendererKind::Tool, "read"));
    assert!(!renderer_registered(
        &Value::Null,
        RendererKind::Message,
        "plan"
    ));
}

// ---------------------------------------------------------------------------
// Autocomplete provider merge + argument completions + suggesters
// ---------------------------------------------------------------------------

const COMPLETIONS_TS: &str = r#"
export default function (pi) {
  pi.registerCommand("deploy", {
    description: "deploy the app",
    getArgumentCompletions: (prefix) => {
      const all = ["prod", "staging", "dev"];
      return all
        .filter((env) => env.startsWith(prefix))
        .map((env) => ({ value: env, label: env, description: "env" }));
    },
    handler: async () => {},
  });
  pi.registerCommand("plain", { handler: async () => {} });
}
"#;

#[test]
fn argument_completions_round_trip() {
    let dir = fixture_dir("arg-completions");
    let handle = Arc::new(start_handle(&dir));
    load_extension(&handle, &dir, "completions.ts", COMPLETIONS_TS);

    // The exported def carries the hook marker.
    let defs = handle.command_defs();
    let deploy = defs
        .iter()
        .find(|def| def.name == "deploy")
        .expect("deploy");
    assert!(deploy.argument_completions);
    let plain = defs.iter().find(|def| def.name == "plain").expect("plain");
    assert!(!plain.argument_completions);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let items = runtime
        .block_on(handle.argument_completions("deploy", "s"))
        .expect("completions");
    let values: Vec<&str> = items
        .iter()
        .filter_map(|item| item.get("value").and_then(Value::as_str))
        .collect();
    assert_eq!(values, vec!["staging"]);

    // Unknown command → empty (no renderer failure).
    let items = runtime
        .block_on(handle.argument_completions("nope", ""))
        .expect("unknown command");
    assert!(items.is_empty());
}

const SUGGESTERS_TS: &str = r#"
export default function (pi) {
  pi.registerCommand("setup", { handler: async (args, ctx) => {
    ctx.ui.addAutocompleteProvider((context) => {
      if (context.prefix === "") return null;
      return [{ value: "suggest:" + context.prefix, label: context.prefix }];
    });
  }});
}
"#;

#[test]
fn add_autocomplete_provider_suggester_consultation() {
    let dir = fixture_dir("suggesters");
    let handle = Arc::new(start_handle(&dir));
    load_extension(&handle, &dir, "suggesters.ts", SUGGESTERS_TS);
    assert!(!handle.has_autocomplete_suggesters());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime
        .block_on(handle.dispatch_command("setup", ""))
        .expect("dispatch");
    assert!(handle.has_autocomplete_suggesters());

    let context = json!({
        "lines": ["fix bu"],
        "cursorLine": 0,
        "cursorCol": 6,
        "prefix": "fix bu",
    })
    .to_string();
    let items = runtime
        .block_on(handle.extension_suggestions(&context))
        .expect("suggestions");
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].get("value").and_then(Value::as_str),
        Some("suggest:fix bu")
    );

    // Null result passes to the next suggester (none) → empty items.
    let empty_context = json!({
        "lines": [""],
        "cursorLine": 0,
        "cursorCol": 0,
        "prefix": "",
    })
    .to_string();
    let items = runtime
        .block_on(handle.extension_suggestions(&empty_context))
        .expect("empty suggestions");
    assert!(items.is_empty());
}

#[test]
fn popup_merge_from_extension_items() {
    // The TUI merge path shape: extension items convert into popup
    // suggestions and navigate (mirrors apply_extension_completions).
    let items = vec![
        AutocompleteItem::new("staging", "staging").with_description("env"),
        AutocompleteItem::new("prod", "prod"),
    ];
    let mut popup = AutocompletePopup::new(AutocompleteSuggestions {
        items,
        prefix: "s".to_string(),
    });
    assert_eq!(
        popup.selected_item().map(|i| i.value.as_str()),
        Some("staging")
    );
    popup.move_down();
    assert_eq!(
        popup.selected_item().map(|i| i.value.as_str()),
        Some("prod")
    );
    popup.move_down();
    assert_eq!(
        popup.selected_item().map(|i| i.value.as_str()),
        Some("staging")
    );
}

// ---------------------------------------------------------------------------
// Editor bridge round-trip
// ---------------------------------------------------------------------------

const EDITOR_TS: &str = r#"
export default function (pi) {
  pi.registerCommand("editor-probe", { handler: async (args, ctx) => {
    ctx.ui.setEditorText("from extension");
    ctx.ui.pasteToEditor(" plus paste");
  }});
}
"#;

#[test]
fn editor_set_get_paste_bridge() {
    let dir = fixture_dir("editor-bridge");
    let handle = Arc::new(start_handle(&dir));
    load_extension(&handle, &dir, "editor.ts", EDITOR_TS);

    // The TUI pushes the live editor text; `ctx.ui.getEditorText()` reads it
    // back through the pushed snapshot (sync host fn).
    let editor = Editor::new();
    editor.set_text("typed by user");
    handle.update_editor_state(editor.get_text());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let observed = runtime
        .block_on(handle.eval_json("JSON.stringify({ text: __pir_host.getEditorTextValue() })"))
        .expect("eval");
    assert_eq!(
        observed.get("text").and_then(Value::as_str),
        Some("typed by user")
    );

    // setEditorText/pasteToEditor produce presentation notifications for the
    // TUI drain (which applies them to the live editor Arc).
    runtime
        .block_on(handle.dispatch_command("editor-probe", ""))
        .expect("dispatch");

    let mut saw_set_text = false;
    let mut saw_paste = false;
    for notification in handle.drain_notifications() {
        match notification {
            Notification::EditorSetText(text) => {
                saw_set_text = true;
                assert_eq!(text, "from extension");
            }
            Notification::EditorPaste(text) => {
                saw_paste = true;
                assert_eq!(text, " plus paste");
            }
            _ => {}
        }
    }
    assert!(saw_set_text, "setEditorText notification missing");
    assert!(saw_paste, "pasteToEditor notification missing");

    // The editor bridge apply (TUI side): set → get → paste round-trip.
    editor.set_text("from extension");
    assert_eq!(editor.get_text(), "from extension");
    editor.paste_text(" plus paste");
    assert_eq!(editor.get_text(), "from extension plus paste");
}

// ---------------------------------------------------------------------------
// Theme registry exposure
// ---------------------------------------------------------------------------

/// A complete custom theme document: every required color token maps to a
/// concrete hex (the parse gate requires the full token set).
fn test_theme_text() -> String {
    const REQUIRED: [&str; 51] = [
        "accent",
        "border",
        "borderAccent",
        "borderMuted",
        "success",
        "error",
        "warning",
        "muted",
        "dim",
        "text",
        "thinkingText",
        "selectedBg",
        "userMessageBg",
        "userMessageText",
        "customMessageBg",
        "customMessageText",
        "customMessageLabel",
        "toolPendingBg",
        "toolSuccessBg",
        "toolErrorBg",
        "toolTitle",
        "toolOutput",
        "mdHeading",
        "mdLink",
        "mdLinkUrl",
        "mdCode",
        "mdCodeBlock",
        "mdCodeBlockBorder",
        "mdQuote",
        "mdQuoteBorder",
        "mdHr",
        "mdListBullet",
        "toolDiffAdded",
        "toolDiffRemoved",
        "toolDiffContext",
        "syntaxComment",
        "syntaxKeyword",
        "syntaxFunction",
        "syntaxVariable",
        "syntaxString",
        "syntaxNumber",
        "syntaxType",
        "syntaxOperator",
        "syntaxPunctuation",
        "thinkingOff",
        "thinkingMinimal",
        "thinkingLow",
        "thinkingMedium",
        "thinkingHigh",
        "thinkingXhigh",
        "bashMode",
    ];
    let mut colors = serde_json::Map::new();
    for key in REQUIRED {
        colors.insert(key.to_string(), json!("#111111"));
    }
    // One var reference resolves to a concrete hex in the snapshot.
    colors.insert("accent".to_string(), json!("brand"));
    json!({
        "name": "pir-test-theme",
        "vars": { "brand": "#123456" },
        "colors": colors,
    })
    .to_string()
}

#[test]
fn theme_registry_snapshots_and_entries() {
    let theme = parse_theme_json(&test_theme_text()).expect("valid theme json");
    register_custom_theme(theme);
    register_custom_theme_path(
        "pir-test-theme",
        std::env::temp_dir().join("pir-test-theme.json"),
    );

    // Custom snapshot resolves var references to concrete hex.
    let snapshot = theme_snapshot("pir-test-theme").expect("custom snapshot");
    assert_eq!(snapshot.name, "pir-test-theme");
    assert_eq!(
        snapshot.colors.get("accent").map(String::as_str),
        Some("#123456")
    );
    assert_eq!(
        snapshot.colors.get("border").map(String::as_str),
        Some("#111111")
    );

    // Built-ins snapshot from the transcribed palettes.
    let dark = theme_snapshot("dark").expect("dark snapshot");
    assert_eq!(dark.name, "dark");
    assert!(dark.colors.contains_key("accent"));
    assert!(theme_snapshot("light").is_some());
    assert!(theme_snapshot("nope").is_none());

    // getAllThemes surface: built-ins first, then custom themes with paths.
    let entries = all_theme_entries();
    assert_eq!(entries[0].name, "dark");
    assert!(entries[0].path.is_none());
    let custom = entries
        .iter()
        .find(|entry| entry.name == "pir-test-theme")
        .expect("custom entry");
    assert_eq!(
        custom.path,
        Some(std::env::temp_dir().join("pir-test-theme.json"))
    );

    assert!(is_known_theme("dark"));
    assert!(is_known_theme("pir-test-theme"));
    assert!(!is_known_theme("nope"));
}

const THEME_TS: &str = r#"
export default function (pi) {
  pi.registerCommand("theme-probe", { handler: async (args, ctx) => {
    const themes = ctx.ui.getAllThemes();
    if (themes.length === 0) throw new Error("getAllThemes returned no themes");
    if (!themes.some((t) => t.name === "dark")) throw new Error("dark missing from getAllThemes");
    const current = ctx.ui.theme;
    if (!current || !current.name) throw new Error("ctx.ui.theme missing");
    const looked = ctx.ui.getTheme("dark");
    if (!looked || looked.name !== "dark") throw new Error("getTheme('dark') failed");
    const bad = ctx.ui.setTheme("nope");
    if (bad.success !== false) throw new Error("unknown theme must fail validation");
    const result = ctx.ui.setTheme("dark");
    if (result.success !== true) throw new Error("setTheme('dark') must succeed");
  }});
}
"#;

#[test]
fn theme_get_set_surface_through_extension() {
    // The process-global theme: serialize with the sibling suites' swaps.
    let _guard = super::tui_system_theme::lock();
    let dir = fixture_dir("theme-surface");
    let handle = Arc::new(start_handle(&dir));

    // The TUI pushes the registry at startup (mirrors theme_registry_json).
    let entries = all_theme_entries();
    let themes: Vec<Value> = entries
        .iter()
        .map(|entry| {
            json!({
                "name": entry.name,
                "path": entry.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
            })
        })
        .collect();
    let mut snapshots = serde_json::Map::new();
    for entry in &entries {
        if let Some(snapshot) = theme_snapshot(&entry.name) {
            snapshots.insert(
                entry.name.clone(),
                serde_json::to_value(&snapshot).expect("snapshot"),
            );
        }
    }
    // `current` is pinned to "dark" rather than read from the global theme:
    // other tests in this binary flip the process-global theme concurrently
    // (including to unknown names, whose snapshot this registry cannot
    // carry), which would race this read and leave `ui.theme` undefined.
    handle.update_theme_registry(json!({
        "themes": themes,
        "current": "dark",
        "snapshots": snapshots,
    }));

    load_extension(&handle, &dir, "theme.ts", THEME_TS);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let outcome = runtime
        .block_on(handle.dispatch_command("theme-probe", ""))
        .expect("dispatch");
    assert!(outcome.ok, "theme probe failed: {:?}", outcome.error);

    // setTheme produced a SetTheme notification the TUI applies live; the
    // failed unknown-theme attempt produced none.
    let mut saw_set_theme = 0;
    for notification in handle.drain_notifications() {
        if let Notification::SetTheme(name) = notification {
            saw_set_theme += 1;
            assert_eq!(name, "dark");
        }
    }
    assert_eq!(
        saw_set_theme, 1,
        "exactly one SetTheme notification expected"
    );

    // The live swap applies (name resolves; unknown falls back to dark).
    set_theme("dark");
    assert_eq!(current_theme_name(), "dark");
}

// ---------------------------------------------------------------------------
// Factory bars: the per-tick refresh must not build the footer snapshot for a
// slot that answers from its cache
// ---------------------------------------------------------------------------

/// A mock invoker that counts calls and returns one deterministic line per
/// request (key + width), so a refresh's rendered lines are observable
/// without a QuickJS round-trip. The counter is atomic: [`RendererInvoker`]
/// is `Send + Sync`.
struct CountingBarsInvoker {
    calls: std::sync::atomic::AtomicUsize,
}

impl RendererInvoker for CountingBarsInvoker {
    fn invoke(&self, request: &RendererRequest) -> anyhow::Result<RendererOutcome> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let width = request
            .options
            .get("width")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        Ok(RendererOutcome {
            lines: Some(vec![format!("{} @{width}", request.key)]),
            text: None,
        })
    }
}

/// The event loop refreshes the registered factory bars every tick. A slot
/// that is clean and was rendered at the current width answers from its cached
/// lines, so the refresh must not build the request's options for it: the
/// footer's `footerData` snapshot (git branch + every extension status + the
/// provider count) is a per-tick allocation that would be discarded unread.
/// A dirty slot or a width change still refreshes exactly once.
#[test]
fn factory_bars_snapshot_is_built_only_for_a_slot_that_needs_a_render() {
    use std::cell::Cell;

    let invoker = CountingBarsInvoker {
        calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let calls = || invoker.calls.load(std::sync::atomic::Ordering::SeqCst);
    let snapshot_calls = Cell::new(0);
    let width = 80;

    let mut footer = None;
    let mut header = None;
    set_factory_slot(&mut footer, "footer", true);
    set_factory_slot(&mut header, "header", true);
    assert!(
        factory_slot_needs_render(&footer, width) && factory_slot_needs_render(&header, width),
        "a freshly registered slot must need a render"
    );

    // First refresh: both slots render; the snapshot is built once.
    let changed = refresh_factory_bars(&mut footer, &mut header, &invoker, width, || {
        snapshot_calls.set(snapshot_calls.get() + 1);
        json!({ "footerData": { "gitBranch": "main" } })
    });
    assert!(changed, "the first refresh must produce lines");
    assert_eq!(
        snapshot_calls.get(),
        1,
        "the footer render needs its snapshot"
    );
    assert_eq!(calls(), 2, "both bars render once");
    assert_eq!(
        footer.as_ref().map(|slot| slot.lines.clone()),
        Some(vec!["footer @80".to_string()])
    );
    assert_eq!(
        header.as_ref().map(|slot| slot.lines.clone()),
        Some(vec!["header @80".to_string()])
    );

    // Steady state: both slots are clean at this width — no invoker call and
    // NO snapshot construction.
    let changed = refresh_factory_bars(&mut footer, &mut header, &invoker, width, || {
        snapshot_calls.set(snapshot_calls.get() + 1);
        json!({ "footerData": { "gitBranch": "main" } })
    });
    assert!(!changed, "a clean tick changes nothing");
    assert_eq!(
        snapshot_calls.get(),
        1,
        "a clean tick must not build the footer snapshot"
    );
    assert_eq!(calls(), 2, "a clean tick must not invoke renderers");

    // A resize re-renders both slots exactly once.
    let changed = refresh_factory_bars(&mut footer, &mut header, &invoker, 60, || {
        snapshot_calls.set(snapshot_calls.get() + 1);
        json!({ "footerData": { "gitBranch": "main" } })
    });
    assert!(changed, "a width change repaints the bars");
    assert_eq!(
        snapshot_calls.get(),
        2,
        "the resized footer needs its snapshot"
    );
    assert_eq!(calls(), 4, "each bar re-renders once");
    assert_eq!(
        footer.as_ref().map(|slot| slot.lines.clone()),
        Some(vec!["footer @60".to_string()])
    );

    // A dirty footer alone still builds the snapshot (and only it renders).
    mark_factory_slot_dirty(&mut footer);
    let changed = refresh_factory_bars(&mut footer, &mut header, &invoker, 60, || {
        snapshot_calls.set(snapshot_calls.get() + 1);
        json!({ "footerData": { "gitBranch": "feature" } })
    });
    assert!(
        !changed,
        "the mock renders the same bytes for the same key/width, so nothing changed"
    );
    assert_eq!(snapshot_calls.get(), 3, "a dirty footer needs its snapshot");
    assert_eq!(calls(), 5, "only the dirty bar re-renders");

    // A cleared slot disappears from the refresh (and contributes no render).
    set_factory_slot(&mut footer, "footer", false);
    assert!(!factory_slot_needs_render(&footer, 60));
}
