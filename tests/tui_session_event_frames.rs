//! TUI session-event frame regressions.
//!
//! The interactive worker used to subscribe to only `queue_update`,
//! `thinking_level_changed` and `background_tool_end`; automatic compaction
//! start/end, out-of-band user appends and `session_info_changed` never
//! reached the render loop. These tests drive the production
//! [`SessionEventBridgeProbe`] (the real `session_event_bridge`) and the pure
//! transcript/tab helpers used by the new match arms, so deleting any of them
//! fails deterministically without a terminal.

use pir::agent_session::SessionEvent;
use pir::tui::app::{ChatEntry, NoticeKind, SessionTab};
use pir::tui::terminal::{
    apply_session_info_changed, entry_appended_user_text, user_message_already_rendered,
    SessionBridgeEvent, SessionEventBridgeProbe,
};

fn tab(path: &str, title: &str) -> SessionTab {
    SessionTab {
        path: path.to_string(),
        title: title.to_string(),
        draft: String::new(),
    }
}

fn user_wire(id: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "message": {
            "role": "user",
            "content": [{ "type": "text", "text": text }],
        },
    })
}

#[test]
fn automatic_compaction_pair_reaches_the_ui_bridge() {
    let mut probe = SessionEventBridgeProbe::new();
    probe.handle(SessionEvent::CompactionStart {
        reason: "threshold",
    });
    probe.handle(SessionEvent::CompactionEnd {
        reason: "threshold",
        result: None,
        aborted: false,
        will_retry: false,
        error_message: None,
    });

    assert_eq!(
        probe.take_events(),
        vec![
            SessionBridgeEvent::CompactionStarted,
            SessionBridgeEvent::CompactionEnded {
                aborted: false,
                error: None,
                manual: false,
            },
        ],
        "an automatic threshold compaction must drive the OSC/in-editor status pair"
    );
}

#[test]
fn manual_compaction_is_bridged_once_with_the_manual_flag() {
    // `/compact` no longer sends a second WorkerEvent pair itself; the
    // session-owned events are the single source (extension `ctx.compact`
    // uses the same `manual` reason and must be covered too).
    let mut probe = SessionEventBridgeProbe::new();
    probe.handle(SessionEvent::CompactionStart { reason: "manual" });
    probe.handle(SessionEvent::CompactionEnd {
        reason: "manual",
        result: Some(serde_json::json!({ "summary": "done" })),
        aborted: false,
        will_retry: false,
        error_message: None,
    });

    assert_eq!(
        probe.take_events(),
        vec![
            SessionBridgeEvent::CompactionStarted,
            SessionBridgeEvent::CompactionEnded {
                aborted: false,
                error: None,
                manual: true,
            },
        ]
    );
}

#[test]
fn failed_or_aborted_compaction_end_carries_its_outcome() {
    let mut probe = SessionEventBridgeProbe::new();
    probe.handle(SessionEvent::CompactionEnd {
        reason: "overflow",
        result: None,
        aborted: true,
        will_retry: false,
        error_message: None,
    });
    probe.handle(SessionEvent::CompactionEnd {
        reason: "overflow",
        result: None,
        aborted: false,
        will_retry: true,
        error_message: Some("summarizer failed".to_string()),
    });

    assert_eq!(
        probe.take_events(),
        vec![
            SessionBridgeEvent::CompactionEnded {
                aborted: true,
                error: None,
                manual: false,
            },
            SessionBridgeEvent::CompactionEnded {
                aborted: false,
                error: Some("summarizer failed".to_string()),
                manual: false,
            },
        ]
    );
}

#[test]
fn only_user_entry_appends_reach_the_transcript_bridge() {
    let mut probe = SessionEventBridgeProbe::new();
    let user = user_wire("e1", "hello");
    probe.handle(SessionEvent::EntryAppended {
        entry: user.clone(),
    });
    assert_eq!(
        probe.take_events(),
        vec![SessionBridgeEvent::EntryAppended { entry: user }],
        "an out-of-band user message is the one entry shape the live run stream misses"
    );

    // Assistant/tool-result message entries are already rendered live, and a
    // custom/non-message entry has its own `CustomEntry`/no-row path: none may
    // append a duplicate transcript bubble.
    for entry in [
        serde_json::json!({
            "id": "e2",
            "message": { "role": "assistant", "content": [] },
        }),
        serde_json::json!({
            "id": "e3",
            "message": { "role": "toolResult", "toolCallId": "c1", "content": [] },
        }),
        serde_json::json!({ "id": "e4", "type": "compaction" }),
        serde_json::json!({ "id": "e5", "type": "custom", "customType": "note" }),
    ] {
        probe.handle(SessionEvent::EntryAppended { entry });
    }
    assert!(
        probe.take_events().is_empty(),
        "non-user appends must not reach the transcript arm"
    );
}

#[test]
fn session_info_changed_forwards_names_and_clears() {
    let mut probe = SessionEventBridgeProbe::new();
    probe.handle(SessionEvent::SessionInfoChanged {
        name: Some("out-of-band".to_string()),
    });
    probe.handle(SessionEvent::SessionInfoChanged { name: None });

    assert_eq!(
        probe.take_events(),
        vec![
            SessionBridgeEvent::SessionInfoChanged {
                name: Some("out-of-band".to_string()),
            },
            SessionBridgeEvent::SessionInfoChanged { name: None },
        ],
        "out-of-band rename and clear must both reach the tab/title arm"
    );
}

#[test]
fn entry_appended_user_text_reads_only_non_empty_user_text() {
    assert_eq!(
        entry_appended_user_text(&user_wire("e1", "hello")),
        Some("hello".to_string())
    );
    assert_eq!(
        entry_appended_user_text(&serde_json::json!({
            "id": "e2",
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "hi" }] },
        })),
        None
    );
    assert_eq!(
        entry_appended_user_text(&serde_json::json!({
            "id": "e3",
            "message": { "role": "user", "content": [] },
        })),
        None,
        "an empty user message has no bubble to render"
    );
    assert_eq!(
        entry_appended_user_text(&serde_json::json!({ "id": "e4", "type": "label" })),
        None
    );
}

#[test]
fn an_already_visible_user_bubble_is_not_appended_twice() {
    let entries = vec![ChatEntry::User("hello".to_string())];
    assert!(user_message_already_rendered(&entries, "hello", "hello"));

    // Typed prompts store the markdown-transformed display text while the
    // wire entry carries the raw text.
    let entries = vec![ChatEntry::User("*HELLO*".to_string())];
    assert!(user_message_already_rendered(&entries, "hello", "*HELLO*"));

    // A queue delivery pushes all bubbles together; matching any member of
    // the trailing user run is enough.
    let entries = vec![
        ChatEntry::User("first".to_string()),
        ChatEntry::User("second".to_string()),
    ];
    assert!(user_message_already_rendered(&entries, "first", "first"));
    assert!(user_message_already_rendered(&entries, "second", "second"));

    // The last row is not a user bubble: this append is a NEW message even
    // when an older identical prompt is still higher in the transcript.
    let entries = vec![
        ChatEntry::User("hello".to_string()),
        ChatEntry::Notice {
            kind: NoticeKind::Status,
            text: "run settled".to_string(),
        },
    ];
    assert!(!user_message_already_rendered(&entries, "hello", "hello"));
    assert!(!user_message_already_rendered(&[], "hello", "hello"));
}

#[test]
fn session_info_changed_updates_only_the_active_tab_and_clears_cleanly() {
    let mut tabs = vec![tab("/sessions/a.jsonl", "a"), tab("/sessions/b.jsonl", "b")];

    let title = apply_session_info_changed(
        &mut tabs,
        Some("/sessions/b.jsonl"),
        Some("renamed"),
        Some("first prompt"),
    );
    assert_eq!(title.as_deref(), Some("renamed"));
    assert_eq!(tabs[0].title, "a", "inactive tabs keep their title");
    assert_eq!(tabs[1].title, "renamed");

    // A clear falls back to the normal precedence: first user preview, then
    // the file stem — never the stale name.
    let title = apply_session_info_changed(
        &mut tabs,
        Some("/sessions/b.jsonl"),
        None,
        Some("first prompt"),
    );
    assert_eq!(title.as_deref(), Some("first prompt"));
    assert_eq!(tabs[1].title, "first prompt");

    let title = apply_session_info_changed(&mut tabs, Some("/sessions/b.jsonl"), None, None);
    assert_eq!(title.as_deref(), Some("b"));
    assert_eq!(tabs[1].title, "b");

    // No matching tab (`--no-session`) leaves the list untouched.
    let title = apply_session_info_changed(&mut tabs, None, Some("ignored"), None);
    assert_eq!(title, None);
    assert_eq!(tabs[1].title, "b");
}

#[test]
fn entry_appended_user_text_ignores_image_only_content() {
    let entry = serde_json::json!({
        "id": "e1",
        "message": {
            "role": "user",
            "content": [{ "type": "image", "data": "AAAA", "mimeType": "image/png" }],
        },
    });
    assert_eq!(entry_appended_user_text(&entry), None);
}
