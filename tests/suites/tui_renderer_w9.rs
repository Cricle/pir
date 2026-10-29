//! Wave 9-C TUI renderer residuals.
//!
//! 1. `ChatEntry::Raw` invalidation: a renderer-produced block re-renders
//!    EXACTLY ONCE per width / theme-generation / Ctrl+O expansion move,
//!    never on an unchanged frame, and keeps its call identity (the entry is
//!    replaced at its index with the same `call_id` / custom payload).
//! 2. The retained-child diff rebuilds such a block from the new lines (the
//!    provenance is part of `EntryTag::Raw`).
//! 3. Footer memo keys: state (cwd / usage / statuses / subscription) +
//!    width + palette generation. A steady frame is one key compare; every
//!    input move rebuilds once.
//! 4. Factory widget/footer/header caches key on the palette generation too,
//!    so a theme install re-renders them even when no call site remembered to
//!    mark the slot dirty (settings-menu `setTheme`, theme-file hot reload,
//!    system-theme watcher).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use pir::tui::app::{
    footer_row_rebuilds, render_footer, rerender_stale_raw, reset_footer_row_rebuilds,
    ChatContainer, ChatEntry, Footer, FooterState, RawCustomRender, RawRender, RawToolRender,
    UsageTotals,
};
use pir::tui::core::Component;
use pir::tui::renderers::{
    factory_slot_needs_render, refresh_factory_bars, refresh_factory_widgets, set_factory_slot,
    upsert_factory_widget, RendererInvoker, RendererKind, RendererOutcome, RendererRequest,
};
use pir::tui::theme::set_theme;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn tool_render(call_id: &str, width: usize, theme_generation: u64, expanded: bool) -> RawRender {
    RawRender::Tool(RawToolRender {
        call_id: Some(call_id.to_string()),
        tool_name: "read".into(),
        args: Arc::new(serde_json::json!({ "path": "src/lib.rs" })),
        output: Arc::from("body"),
        is_error: false,
        details: None,
        expanded,
        width,
        theme_generation,
        shell: None,
        pending: false,
        images: Arc::new(Vec::new()),
        image_lines: Arc::new(Vec::new()),
    })
}

fn tool_raw(
    call_id: &str,
    lines: &[&str],
    width: usize,
    theme_generation: u64,
    expanded: bool,
) -> ChatEntry {
    ChatEntry::Raw {
        call_id: Some(call_id.to_string()),
        lines: lines.iter().map(|line| (*line).to_string()).collect(),
        render: Some(tool_render(call_id, width, theme_generation, expanded)),
    }
}

fn custom_raw(
    custom_type: &str,
    lines: &[&str],
    width: usize,
    theme_generation: u64,
    expanded: bool,
) -> ChatEntry {
    ChatEntry::Raw {
        call_id: None,
        lines: lines.iter().map(|line| (*line).to_string()).collect(),
        render: Some(RawRender::Custom(RawCustomRender {
            payload: Arc::new(serde_json::json!({ "customType": custom_type, "value": 1 })),
            custom_type: custom_type.into(),
            is_message: false,
            expanded,
            width,
            theme_generation,
        })),
    }
}

/// What a real renderer would return for `provenance` under the new key:
/// the same call identity / payload with fresh lines stamped for
/// `(width, theme_generation, expanded)`.
fn rendered(
    provenance: &RawRender,
    width: usize,
    theme_generation: u64,
    expanded: bool,
    tag: &str,
) -> ChatEntry {
    match provenance {
        RawRender::Tool(tool) => ChatEntry::Raw {
            call_id: tool.call_id.clone(),
            lines: vec![format!("{} {tag}", tool.tool_name)],
            render: Some(RawRender::Tool(RawToolRender {
                call_id: tool.call_id.clone(),
                tool_name: tool.tool_name.clone(),
                args: Arc::clone(&tool.args),
                output: Arc::clone(&tool.output),
                is_error: tool.is_error,
                details: tool.details.clone(),
                expanded,
                width,
                theme_generation,
                shell: tool.shell,
                pending: tool.pending,
                images: Arc::clone(&tool.images),
                image_lines: Arc::clone(&tool.image_lines),
            })),
        },
        RawRender::Custom(custom) => ChatEntry::Raw {
            call_id: None,
            lines: vec![format!("{} {tag}", custom.custom_type)],
            render: Some(RawRender::Custom(RawCustomRender {
                payload: Arc::clone(&custom.payload),
                custom_type: custom.custom_type.clone(),
                is_message: custom.is_message,
                expanded,
                width,
                theme_generation,
            })),
        },
    }
}

fn raw_lines(entry: &ChatEntry) -> &[String] {
    match entry {
        ChatEntry::Raw { lines, .. } => lines,
        other => panic!("expected a Raw entry, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. Raw invalidation: exactly one render per moved input, none otherwise
// ---------------------------------------------------------------------------

#[test]
fn raw_block_rerenders_exactly_once_per_input_change() {
    let mut entries = vec![tool_raw("call-1", &["stale @80"], 80, 7, false)];
    let mut calls = 0usize;

    // The block answers the key it was rendered under: no renderer call.
    let rebuilt = rerender_stale_raw(&mut entries, 80, 7, false, |provenance| {
        calls += 1;
        rendered(provenance, 80, 7, false, "@80")
    });
    assert_eq!(rebuilt, 0, "an unchanged frame must not rebuild");
    assert_eq!(calls, 0, "the renderer must not run on an unchanged frame");

    // Width move: exactly one render, exactly one replaced entry.
    let rebuilt = rerender_stale_raw(&mut entries, 100, 7, false, |provenance| {
        calls += 1;
        rendered(provenance, 100, 7, false, "@100")
    });
    assert_eq!(rebuilt, 1, "a width move rebuilds the block once");
    assert_eq!(calls, 1);
    assert!(raw_lines(&entries[0])[0].contains("@100"));

    // Same key again: the fresh bytes answer it.
    let rebuilt = rerender_stale_raw(&mut entries, 100, 7, false, |provenance| {
        calls += 1;
        rendered(provenance, 100, 7, false, "@100-again")
    });
    assert_eq!(rebuilt, 0);
    assert_eq!(calls, 1, "no second render without a key move");

    // Palette move.
    let rebuilt = rerender_stale_raw(&mut entries, 100, 8, false, |provenance| {
        calls += 1;
        rendered(provenance, 100, 8, false, "@gen8")
    });
    assert_eq!(rebuilt, 1);
    assert_eq!(calls, 2);

    // Expansion move.
    let rebuilt = rerender_stale_raw(&mut entries, 100, 8, true, |provenance| {
        calls += 1;
        rendered(provenance, 100, 8, true, "@expanded")
    });
    assert_eq!(rebuilt, 1);
    assert_eq!(calls, 3);
    assert!(raw_lines(&entries[0])[0].contains("@expanded"));

    // And the steady frame after the last move stays free.
    let rebuilt = rerender_stale_raw(&mut entries, 100, 8, true, |provenance| {
        calls += 1;
        rendered(provenance, 100, 8, true, "@steady")
    });
    assert_eq!(rebuilt, 0);
    assert_eq!(calls, 3, "each input move rendered exactly once");
}

#[test]
fn raw_rerender_keeps_call_identity_and_skips_verbatim_blocks() {
    let mut entries = vec![
        ChatEntry::User("hello".into()),
        custom_raw("todo", &["stale todo"], 80, 7, false),
        ChatEntry::Raw {
            call_id: None,
            lines: vec!["verbatim image preview".into()],
            render: None,
        },
        tool_raw("call-9", &["stale tool"], 80, 7, false),
    ];
    let custom_ptr = match &entries[1] {
        ChatEntry::Raw {
            render: Some(RawRender::Custom(custom)),
            ..
        } => Arc::as_ptr(&custom.payload) as usize,
        other => panic!("expected a custom Raw block, got {other:?}"),
    };
    let mut rendered_kinds = Vec::new();
    let rebuilt = rerender_stale_raw(&mut entries, 120, 7, false, |provenance| {
        rendered_kinds.push(match provenance {
            RawRender::Tool(tool) => format!("tool:{}", tool.call_id.as_deref().unwrap_or("")),
            RawRender::Custom(custom) => format!("custom:{}", custom.custom_type),
        });
        rendered(provenance, 120, 7, false, "@120")
    });

    assert_eq!(rebuilt, 2, "only renderer-produced blocks are rebuilt");
    assert_eq!(rendered_kinds, vec!["custom:todo", "tool:call-9"]);
    // The dispatch order is transcript order, and both blocks kept their
    // identity in place: the tool call id and the custom payload allocation.
    assert!(matches!(
        &entries[3],
        ChatEntry::Raw {
            call_id: Some(id),
            ..
        } if id == "call-9"
    ));
    match &entries[1] {
        ChatEntry::Raw {
            render: Some(RawRender::Custom(custom)),
            ..
        } => assert_eq!(Arc::as_ptr(&custom.payload) as usize, custom_ptr),
        other => panic!("expected the custom block to stay Raw, got {other:?}"),
    }
    assert_eq!(raw_lines(&entries[2]), ["verbatim image preview"]);
    assert!(matches!(&entries[0], ChatEntry::User(text) if text.as_str() == "hello"));
}

// ---------------------------------------------------------------------------
// 2. The retained-child diff picks the fresh lines up
// ---------------------------------------------------------------------------

#[test]
fn container_rebuilds_the_raw_children_when_the_key_moves() {
    let mut chat = ChatContainer::new();
    let mut entries = vec![tool_raw("call-1", &["stale @80"], 80, 7, false)];
    chat.set_entries(&entries);
    assert!(chat.render(80).iter().any(|row| row.contains("stale @80")));

    let rebuilt = rerender_stale_raw(&mut entries, 100, 7, false, |provenance| {
        rendered(provenance, 100, 7, false, "fresh @100")
    });
    assert_eq!(rebuilt, 1);
    chat.set_entries(&entries);
    let rows = chat.render(100);
    assert!(
        rows.iter().any(|row| row.contains("fresh @100")),
        "the rebuilt block's rows must be rendered: {rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("stale @80")),
        "no stale renderer output may survive: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// 3. Footer memo: keyed by state + width + palette generation
// ---------------------------------------------------------------------------

fn footer_state() -> FooterState {
    FooterState {
        cwd: "/work/project".into(),
        git_branch: Some("main".into()),
        session_name: None,
        session_tab: None,
        usage: UsageTotals {
            input: 1_200,
            output: 340,
            cache_read: 500,
            cache_write: 0,
            cost: 0.125,
        },
        context_percent: Some(12.5),
        context_window: 200_000,
        auto_compact: true,
        model_id: "gpt-5".into(),
        model_provider: "openai".into(),
        model_reasoning: true,
        thinking_level: "medium".into(),
        extension_statuses: Vec::new(),
        cache_hit_rate: Some(29.3),
        subscription: false,
        experimental: false,
        routed: None,
        available_providers: 2,
    }
}

#[test]
fn footer_memo_rebuilds_once_per_input_change_and_never_per_frame() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    reset_footer_row_rebuilds();

    let mut state = footer_state();
    let footer = Footer::new(state.clone());
    let first = footer.render(80);
    assert_eq!(footer_row_rebuilds(), 1, "the first frame builds the rows");
    assert_eq!(first, render_footer(80, &state));

    for _ in 0..5 {
        let _ = footer.render(80);
    }
    assert_eq!(
        footer_row_rebuilds(),
        1,
        "a steady frame is one key compare, not a rebuild"
    );

    // Width.
    let _ = footer.render(81);
    assert_eq!(footer_row_rebuilds(), 2);

    // cwd.
    state.cwd = "/work/elsewhere".into();
    footer.update(state.clone());
    let _ = footer.render(81);
    assert_eq!(footer_row_rebuilds(), 3);

    // Usage totals.
    state.usage.output += 10;
    footer.update(state.clone());
    let _ = footer.render(81);
    assert_eq!(footer_row_rebuilds(), 4);

    // Extension status line.
    state.extension_statuses = vec![("phone".into(), "● online".into())];
    footer.update(state.clone());
    let _ = footer.render(81);
    assert_eq!(footer_row_rebuilds(), 5);

    // Auth/subscription marker (`(sub)` after the cost).
    state.subscription = true;
    footer.update(state.clone());
    let _ = footer.render(81);
    assert_eq!(footer_row_rebuilds(), 6);

    // Palette generation.
    set_theme("light");
    let _ = footer.render(81);
    assert_eq!(footer_row_rebuilds(), 7);

    // The memoized rows stay byte-identical to the uncached renderer.
    assert_eq!(footer.render(81), render_footer(81, &state));
    // And the steady frame after the last change hits again.
    let _ = footer.render(81);
    assert_eq!(footer_row_rebuilds(), 7);

    // Restore the process-global palette for the sibling suites.
    set_theme("dark");
}

// ---------------------------------------------------------------------------
// 4. Factory widget / footer / header caches: palette generation is a key
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CountingInvoker {
    calls: AtomicUsize,
}

impl CountingInvoker {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl RendererInvoker for CountingInvoker {
    fn invoke(&self, request: &RendererRequest) -> anyhow::Result<RendererOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let width = request
            .options
            .get("width")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        Ok(RendererOutcome {
            lines: Some(vec![format!(
                "{} {} @{width}",
                request.kind.as_str(),
                request.key
            )]),
            text: None,
        })
    }
}

#[test]
fn factory_widgets_rekey_on_the_palette_generation() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let invoker = CountingInvoker::default();
    let mut widgets = Vec::new();
    upsert_factory_widget(&mut widgets, "todo", "above_transcript");

    let _ = refresh_factory_widgets(&mut widgets, &invoker, 80);
    assert_eq!(invoker.calls(), 1);
    let _ = refresh_factory_widgets(&mut widgets, &invoker, 80);
    assert_eq!(
        invoker.calls(),
        1,
        "a clean frame must not call the factory"
    );

    // No dirty flag, same width: only the palette moved. The generation key
    // alone must force the re-render (settings-menu / watcher theme swaps do
    // not go through the notification drain).
    set_theme("light");
    let _ = refresh_factory_widgets(&mut widgets, &invoker, 80);
    assert_eq!(
        invoker.calls(),
        2,
        "a theme install must re-render the factory widget"
    );

    // A width move stays a key move too.
    let _ = refresh_factory_widgets(&mut widgets, &invoker, 60);
    assert_eq!(invoker.calls(), 3);

    set_theme("dark");
}

#[test]
fn factory_footer_slot_rekeys_on_the_palette_generation() {
    let _guard = super::tui_system_theme::lock();
    set_theme("dark");
    let invoker = CountingInvoker::default();
    let mut footer_slot = None;
    let mut header_slot = None;
    set_factory_slot(&mut footer_slot, "__footer", true);

    let mut snapshots = 0usize;
    let _ = refresh_factory_bars(&mut footer_slot, &mut header_slot, &invoker, 80, || {
        snapshots += 1;
        serde_json::json!({ "gitBranch": "main" })
    });
    assert_eq!(invoker.calls(), 1);
    assert_eq!(snapshots, 1);

    // Clean tick: no factory call and no footerData snapshot built.
    let _ = refresh_factory_bars(&mut footer_slot, &mut header_slot, &invoker, 80, || {
        snapshots += 1;
        serde_json::json!({})
    });
    assert_eq!(invoker.calls(), 1);
    assert_eq!(snapshots, 1);

    // Width move.
    let _ = refresh_factory_bars(&mut footer_slot, &mut header_slot, &invoker, 60, || {
        snapshots += 1;
        serde_json::json!({})
    });
    assert_eq!(invoker.calls(), 2);
    assert_eq!(snapshots, 2);

    // Palette move without any dirty mark (the settings-menu / hot-reload
    // path): the slot's generation key must report the need.
    set_theme("light");
    assert!(
        factory_slot_needs_render(&footer_slot, 60),
        "the palette generation is part of the slot key"
    );
    let _ = refresh_factory_bars(&mut footer_slot, &mut header_slot, &invoker, 60, || {
        snapshots += 1;
        serde_json::json!({})
    });
    assert_eq!(invoker.calls(), 3);
    assert_eq!(snapshots, 3);
    assert!(
        !factory_slot_needs_render(&footer_slot, 60),
        "the re-render recorded the new generation"
    );

    assert_eq!(
        footer_slot.as_ref().map(|slot| slot.theme_generation),
        Some(pir::tui::theme::Theme::generation()),
        "the cached lines are stamped with the generation they were built under"
    );

    set_theme("dark");
}

/// Re-export check: the test compiles against the public factory cache shape
/// (`FactoryWidget` gains the generation field the refresh helpers key on).
#[test]
fn factory_widget_shape_exposes_the_generation_key() {
    let widget = pir::tui::renderers::FactoryWidget {
        key: "k".into(),
        placement: "above_transcript".into(),
        lines: vec!["row".into()],
        width: 80,
        theme_generation: 3,
        dirty: false,
    };
    assert_eq!(widget.theme_generation, 3);
    assert!(!widget.dirty);
    let _ = RendererKind::Footer;
}
