//! TUI upstream-alignment drift fixes (UPDATING.md ledger, W1 wave, TUI half) — one place to
//! pin the behavior of each low-risk fix, with the upstream commit it follows.
//!
//! - D11 `3e477eab5` — `PI_CLIPBOARD_OSC52=1` also emits OSC 52 after a
//!   successful local clipboard write (remote clients without SSH markers).
//! - D18 `11f991145` — a wheel event no component handles scrolls the
//!   transcript under a focused fullscreen overlay (#10769).
//! - D19 `edb2eb966` — a fragmented terminal reply keeps its string until the
//!   terminator, so its tail cannot become keyboard input.
//! - D20 `dbc7ff87c` — the compaction status indicator paints with the editor
//!   border / thinking color, not accent+muted (#10810).
//! - D21 `78a3c10ba` — Herdr reports the Kitty graphics protocol (#10774).
//!
//! Standalone binary (no `g_*` suite registration): tests/ and tests/suites/
//! are a concurrent-edit conflict zone.

use std::sync::atomic::{AtomicUsize, Ordering};

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use pir::config::settings::{Settings, TerminalImagesMode, TerminalSettings};
use pir::tui::altscreen_search::{handle_mouse_event, AltScreenConfig, AltScreenState};
use pir::tui::ansi;
use pir::tui::clipboard::{
    emit_osc52, headless_session, is_remote_session, write_text_with, MAX_OSC52_ENCODED_LENGTH,
};
use pir::tui::core::{Terminal, Text, TuiCore};
use pir::tui::editor::Editor;
use pir::tui::settings_menu::{settings_items, SettingsMenuContext};
use pir::tui::terminal::{embed_status_in_border, transcript_mouse_route};
use pir::tui::terminal_image::{
    capabilities_with_overrides, detect_capabilities_with, overrides_from_terminal_settings,
    supports_images, ImageProtocol, TerminalCapabilities,
};
use pir::tui::terminal_reply::{is_control_string, scan_reply, ReplyScan};
use pir::tui::theme::{with_theme, ThemeColor};
use pir::tui::wheel_scroll::{WheelScrollAccelerator, WheelScrollLines};

/// An environment lookup over fixed pairs (unset names return `None`).
fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |name: &str| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_string())
    }
}

/// The base64 payload upstream's OSC 52 emission writes.
fn osc52_expected(text: &str) -> Vec<u8> {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x07").into_bytes()
}

// ---------------------------------------------------------------------------
// D11 — PI_CLIPBOARD_OSC52 forces OSC 52 alongside the local write
// (upstream 3e477eab5; clipboard.test.ts "PI_CLIPBOARD_OSC52=1 emits OSC 52
// after a successful copy")
// ---------------------------------------------------------------------------

/// Upstream's `test.each(["darwin"…], ["win32"…], ["linux", ":99"])`: with
/// `PI_CLIPBOARD_OSC52=1` the local write still happens and the OSC 52
/// sequence follows it byte for byte.
#[test]
fn d11_forced_osc52_emits_after_a_successful_local_write() {
    for linux in [false, true] {
        let writes = AtomicUsize::new(0);
        let local = |_text: &str| {
            writes.fetch_add(1, Ordering::SeqCst);
            true
        };
        let mut out: Vec<u8> = Vec::new();
        let copied = write_text_with(
            "hello",
            &local,
            &env_of(&[("PI_CLIPBOARD_OSC52", "1")]),
            linux,
            &mut out,
        );
        assert!(copied, "the local write succeeded");
        assert_eq!(writes.load(Ordering::SeqCst), 1, "local write first");
        assert_eq!(out, osc52_expected("hello"));
    }
}

/// The remote-session markers upstream always emits OSC 52 for, plus the
/// `PI_CLIPBOARD_OSC52` opt-in and its exact-match rule (`"1"` only; empty is
/// unset, like upstream's falsy-check).
#[test]
fn d11_remote_markers_and_the_exact_opt_in() {
    assert!(is_remote_session(&env_of(&[(
        "SSH_CONNECTION",
        "1.2.3.4 5 6.7.8.9 22"
    )])));
    assert!(is_remote_session(&env_of(&[(
        "SSH_CLIENT",
        "1.2.3.4 5 22"
    )])));
    assert!(is_remote_session(&env_of(&[(
        "MOSH_CONNECTION",
        "1 2 3 4"
    )])));
    assert!(is_remote_session(&env_of(&[("PI_CLIPBOARD_OSC52", "1")])));
    assert!(!is_remote_session(&env_of(&[("PI_CLIPBOARD_OSC52", "0")])));
    assert!(!is_remote_session(&env_of(&[(
        "PI_CLIPBOARD_OSC52",
        "true"
    )])));
    assert!(!is_remote_session(&env_of(&[("PI_CLIPBOARD_OSC52", "")])));
    assert!(!is_remote_session(&env_of(&[("SSH_CONNECTION", "")])));
    assert!(!is_remote_session(&env_of(&[])));
}

/// A plain local session writes nothing to the terminal (the counterfactual:
/// only the new opt-in/markers turn OSC 52 on).
#[test]
fn d11_no_osc52_without_a_remote_session() {
    let mut out: Vec<u8> = Vec::new();
    let copied = write_text_with("hello", &|_| true, &env_of(&[]), false, &mut out);
    assert!(copied);
    assert!(out.is_empty(), "no OSC 52 for a local session");
}

/// Headless Linux (containers, WSL without WSLg): the local clipboard failed,
/// so OSC 52 is the only route and reports success (upstream's `headless`
/// fallback). A Linux session with a display keeps reporting the failure.
#[test]
fn d11_headless_linux_falls_back_to_osc52() {
    let mut out: Vec<u8> = Vec::new();
    let copied = write_text_with("hi", &|_| false, &env_of(&[]), true, &mut out);
    assert!(copied, "OSC 52 replaced the missing clipboard");
    assert_eq!(out, osc52_expected("hi"));

    let mut with_display: Vec<u8> = Vec::new();
    let copied = write_text_with(
        "hi",
        &|_| false,
        &env_of(&[("DISPLAY", ":99")]),
        true,
        &mut with_display,
    );
    assert!(
        !copied,
        "a display means the failure is reported (upstream #9618)"
    );
    assert!(with_display.is_empty());

    assert!(headless_session(&env_of(&[]), true));
    assert!(!headless_session(&env_of(&[("DISPLAY", ":0")]), true));
    assert!(!headless_session(
        &env_of(&[("WAYLAND_DISPLAY", "wayland-0")]),
        true
    ));
    assert!(!headless_session(
        &env_of(&[("TERMUX_VERSION", "0.118")]),
        true
    ));
    assert!(!headless_session(&env_of(&[]), false));
}

/// OSC 52 payloads above upstream's `MAX_OSC52_ENCODED_LENGTH` are refused
/// (nothing is written) instead of spraying the terminal.
#[test]
fn d11_oversized_payload_is_refused() {
    // 75 KiB of text base64-encodes to 100_000+ characters.
    let big = "a".repeat(75 * 1024);
    let mut out: Vec<u8> = Vec::new();
    assert!(!emit_osc52(&mut out, &big));
    assert!(out.is_empty());
    assert_eq!(MAX_OSC52_ENCODED_LENGTH, 100_000);

    let copied = write_text_with(
        &big,
        &|_| false,
        &env_of(&[("PI_CLIPBOARD_OSC52", "1")]),
        false,
        &mut out,
    );
    assert!(!copied);
    assert!(out.is_empty());
}

// ---------------------------------------------------------------------------
// D18 — an unhandled wheel scrolls the transcript under a focused overlay
// (upstream 11f991145; tui-alt-screen.test.ts #10769)
// ---------------------------------------------------------------------------

/// The routing predicate: wheel rows always reach the transcript, every other
/// gesture stays blocked while an overlay or dialog owns the focus.
#[test]
fn d18_wheel_routes_to_the_transcript_under_a_focused_overlay() {
    for kind in [MouseEventKind::ScrollUp, MouseEventKind::ScrollDown] {
        assert!(
            transcript_mouse_route(kind, true, false),
            "overlay open: the wheel still scrolls the transcript"
        );
        assert!(
            transcript_mouse_route(kind, false, true),
            "dialog open: the wheel still scrolls the transcript"
        );
        assert!(transcript_mouse_route(kind, false, false));
    }
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Moved,
        MouseEventKind::ScrollLeft,
    ] {
        assert!(
            !transcript_mouse_route(kind, true, false),
            "{kind:?} stays blocked while an overlay is focused"
        );
        assert!(!transcript_mouse_route(kind, false, true));
        assert!(transcript_mouse_route(kind, false, false));
    }
}

fn wheel_event(kind: MouseEventKind) -> MouseEvent {
    MouseEvent {
        kind,
        column: 3,
        row: 2,
        modifiers: KeyModifiers::NONE,
    }
}

/// The wheel the predicate routes actually scrolls the transcript: dispatching
/// it through the alt-screen mouse family moves the viewport, and the opposite
/// wheel moves it back. (The up/down *sign* convention is
/// `WheelScrollAccelerator`'s, unchanged here.)
#[test]
fn d18_dispatched_wheel_scrolls_and_unwinds() {
    let terminal = Terminal::new();
    terminal.set_size(20, 6);
    let mut core = TuiCore::new(terminal);
    let transcript: String = (1..=30)
        .map(|index| format!("line {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    core.push_content(Box::new(Text::new(transcript)));
    core.render();
    assert!(
        core.frame_buffer_length() > core.rows(),
        "the fixture must be scrollable"
    );

    let mut state = AltScreenState::default();
    let config = AltScreenConfig::default();
    let mut wheel = WheelScrollAccelerator::with_accelerate(WheelScrollLines::Lines(2), true);
    let copy = |_text: &str| false;

    core.set_scroll_offset(5);
    assert!(transcript_mouse_route(
        MouseEventKind::ScrollUp,
        true,
        false
    ));
    assert!(handle_mouse_event(
        wheel_event(MouseEventKind::ScrollUp),
        &mut core,
        &mut state,
        &config,
        &mut wheel,
        0,
        &copy,
    ));
    let after_up = core.scroll_offset();
    assert_ne!(after_up, 5, "the wheel moved the viewport");

    assert!(handle_mouse_event(
        wheel_event(MouseEventKind::ScrollDown),
        &mut core,
        &mut state,
        &config,
        &mut wheel,
        0,
        &copy,
    ));
    assert_eq!(core.scroll_offset(), 5, "the opposite wheel unwinds it");
}

// ---------------------------------------------------------------------------
// D19 — a recognized terminal reply keeps its string until the terminator
// (upstream edb2eb966; stdin-buffer.test.ts "Terminal Reply Strings")
// ---------------------------------------------------------------------------

/// The upstream reply vectors: every split after the reply header must stay
/// `Open` (held, not flushed into the prompt), and the completed reply must
/// consume exactly its own bytes — the following keystroke is never part of
/// it. Splits are exercised at every position, including a lone trailing
/// `ESC` that could still become `ST`.
#[test]
fn d19_replies_survive_every_split_and_terminate_cleanly() {
    // (reply header, full reply) — upstream's `replies` table.
    let replies = [
        ("\x1b]4;1;", "\x1b]4;1;rgb:eaea/6c6c/6d6d\x07"), // Ghostty's Ayu Light palette color 1
        ("\x1b]10;", "\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
        ("\x1b]11;", "\x1b]11;rgb:0000/0000/0000\x07"),
        ("\x1b]4;1;", "\x1b]4;1;rgb:6464/6464/4d4d\x07"),
        ("\x1bP>|", "\x1bP>|Ghostty 1.3.1\x1b\\"),
        ("\x1b_Gi=1;", "\x1b_Gi=1;OK\x1b\\"),
        ("\x1b]7501;?", "\x1b]7501;?\x1b\\"),
    ];
    for (header, reply) in replies {
        let bytes = reply.as_bytes();
        for split in header.len()..bytes.len() {
            assert_eq!(
                scan_reply(&bytes[..split]),
                ReplyScan::Open,
                "{reply:?} split at {split} must stay buffered"
            );
        }
        assert_eq!(
            scan_reply(bytes),
            ReplyScan::Complete { len: bytes.len() },
            "{reply:?} completes at its terminator"
        );
        // The bytes after the reply belong to the keyboard: a BEL terminator
        // must never surface as Ctrl+G, and text after it must not be
        // swallowed by the reply.
        let mut with_tail = bytes.to_vec();
        with_tail.extend_from_slice(b"x");
        assert_eq!(
            scan_reply(&with_tail),
            ReplyScan::Complete { len: bytes.len() },
            "{reply:?} must not absorb the tail"
        );
    }
}

/// A recognized header that is still streaming keeps the string buffered
/// across reads, even through the ambiguous-input window (upstream "keeps an
/// OSC reply buffered across multiple expired sequence timeouts").
#[test]
fn d19_streaming_reply_stays_open_until_the_terminator() {
    let mut collected = b"\x1b]4;1;rgb:eaea/6c".to_vec();
    assert_eq!(scan_reply(&collected), ReplyScan::Open);
    collected.extend_from_slice(b"6c/6d6d");
    assert_eq!(scan_reply(&collected), ReplyScan::Open);
    collected.push(0x07);
    assert_eq!(
        scan_reply(&collected),
        ReplyScan::Complete {
            len: collected.len()
        }
    );

    // A header assembled after the window closed is recognized too
    // (upstream "recognizes a reply header assembled within the ambiguous
    // input timeout").
    assert_eq!(scan_reply(b"\x1b]1"), ReplyScan::NotAReply);
    assert_eq!(scan_reply(b"\x1b]10;"), ReplyScan::Open);
    let full = b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\";
    assert_eq!(scan_reply(full), ReplyScan::Complete { len: full.len() });
}

/// Ambiguous input — a bare introducer, an incomplete header, `OSC 0` window
/// titles — is *not* a delimited reply: it keeps the bounded recovery
/// (upstream "still times out bare string introducers as Alt keys" and
/// "recovers batched legacy Alt input").
#[test]
fn d19_ambiguous_input_is_not_held() {
    let ambiguous: [&[u8]; 14] = [
        b"\x1b",
        b"\x1b]",
        b"\x1bP",
        b"\x1b_",
        b"\x1b]a",
        b"\x1b]1",
        b"\x1b]10",
        b"\x1b]0;title",
        b"\x1b]4;",
        b"\x1b]4;a;",
        b"\x1bPa",
        b"\x1bP>",
        b"\x1b_a",
        b"\x1b_Gi=1",
    ];
    for bytes in ambiguous {
        assert_eq!(
            scan_reply(bytes),
            ReplyScan::NotAReply,
            "{bytes:?} must keep bounded recovery"
        );
    }
    // A complete kitty header is recognized (upstream `_G(?:[a-z]=\d+,)*…`:
    // the parameter values must be numbers, so `a=T` is not a header).
    assert_eq!(scan_reply(b"\x1b_Gi=1;OK"), ReplyScan::Open);
    assert_eq!(scan_reply(b"\x1b_Gi=1,m=1;OK"), ReplyScan::Open);
    assert_eq!(scan_reply(b"\x1b_Gi=1,a=T;OK"), ReplyScan::NotAReply);
    assert!(is_control_string(b"\x1b]11;"));
    assert!(!is_control_string(b"\x1b[11;"));
}

/// `CAN`/`SUB` cancel the string without swallowing the input after it
/// (upstream "discards a canceled reply without swallowing subsequent
/// input").
#[test]
fn d19_can_and_sub_cancel_the_reply() {
    for cancel in [0x18u8, 0x1a] {
        let mut bytes = b"\x1b]11;rgb:0000".to_vec();
        let cancel_index = bytes.len();
        bytes.push(cancel);
        bytes.push(b'x');
        assert_eq!(
            scan_reply(&bytes),
            ReplyScan::Cancelled {
                len: cancel_index + 1
            },
            "the cancel byte ends the reply; 'x' stays outside it"
        );
    }
}

/// A new escape sequence aborts an unterminated reply, and the aborting `ESC`
/// stays outside the dropped span so it can be reparsed (upstream "recovers
/// from an unterminated reply when a new escape sequence arrives").
#[test]
fn d19_new_escape_aborts_an_unterminated_reply() {
    let mut bytes = b"\x1b]4;1;rgb:eaea/6c".to_vec();
    let abort_index = bytes.len();
    bytes.extend_from_slice(b"\x1b[Ahello");
    assert_eq!(
        scan_reply(&bytes),
        ReplyScan::Aborted { len: abort_index },
        "the new CSI sequence is not part of the reply"
    );
    // A trailing ESC alone could still become an ST terminator: keep it
    // buffered until the next byte decides.
    let mut trailing = b"\x1b]10;rgb:ffff".to_vec();
    trailing.push(0x1b);
    assert_eq!(scan_reply(&trailing), ReplyScan::Open);
    trailing.push(b'\\');
    assert_eq!(
        scan_reply(&trailing),
        ReplyScan::Complete {
            len: trailing.len()
        }
    );
}

// ---------------------------------------------------------------------------
// D20 — the compaction status takes the editor border / thinking color
// (upstream dbc7ff87c; #10810)
// ---------------------------------------------------------------------------

/// Every SGR sequence in `line`.
fn sgr_sequences(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut found = Vec::new();
    let mut index = 0usize;
    while index + 1 < bytes.len() {
        if bytes[index] == 0x1b && bytes[index + 1] == b'[' {
            if let Some(offset) = bytes[index + 2..]
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
            {
                let end = index + 2 + offset + 1;
                found.push(line[index..end].to_string());
                index = end;
                continue;
            }
        }
        index += 1;
    }
    found
}

/// The compaction row is spliced into the editor's top border and paints with
/// the border's color — `editor.borderColor ?? theme.getThinkingBorderColor`
/// (upstream `colorFn`, interactive-mode.ts:2346-2358). The old accent
/// spinner / muted text pair must not appear.
#[test]
fn d20_compaction_status_uses_the_editor_border_color() {
    let border = with_theme(|theme| theme.thinking_border("high"));
    assert!(!border.is_empty(), "the thinking level has a border color");

    let editor = Editor::new();
    editor.set_border_color(border.clone());
    let (lines, _, _) = editor.render_with_cursor_rows(40, 3);
    let top = lines.first().cloned().unwrap_or_default();
    assert!(
        top.starts_with(&border),
        "the editor border carries the thinking color"
    );

    let status = embed_status_in_border(&top, 40, "⠋", "⠋ Compacting context... (working)");
    assert!(status.contains("Compacting context"));
    assert!(
        status.starts_with(&border),
        "the status takes the border paint"
    );
    for sequence in sgr_sequences(&status) {
        assert!(
            sequence == border || sequence == ansi::FG_RESET,
            "unexpected SGR {sequence:?} in the compaction status line"
        );
    }

    // The pre-fix colors (accent spinner, muted text) are a different paint,
    // and neither appears.
    let accent = with_theme(|theme| theme.fg(ThemeColor::Accent, ""));
    let accent_paint = accent.trim_end_matches(ansi::FG_RESET);
    assert!(!accent_paint.is_empty() && accent_paint != border);
    assert!(!status.contains(accent_paint));
    let muted = with_theme(|theme| theme.fg(ThemeColor::Muted, ""));
    let muted_paint = muted.trim_end_matches(ansi::FG_RESET);
    assert!(!status.contains(muted_paint));
}

// ---------------------------------------------------------------------------
// D21 — Herdr reports the Kitty graphics protocol (upstream 78a3c10ba; #10774)
// ---------------------------------------------------------------------------

fn caps_of(pairs: &[(&str, &str)]) -> TerminalCapabilities {
    detect_capabilities_with(&env_of(pairs), &|| false)
}

/// Upstream's Herdr test, verbatim: `KITTY_WINDOW_ID` may leak in from the
/// outer terminal, so Herdr is detected first and now yields Kitty images.
#[test]
fn d21_herdr_reports_kitty_images() {
    let caps = caps_of(&[
        ("TERM_PROGRAM", "herdr"),
        ("TERM", "xterm-256color"),
        ("COLORTERM", "truecolor"),
        ("KITTY_WINDOW_ID", "1"),
    ]);
    assert_eq!(caps.images, Some(ImageProtocol::Kitty));
    assert!(caps.hyperlinks);
    assert!(caps.true_color);

    let caps = caps_of(&[("TERM_PROGRAM", "herdr"), ("PI_HYPERLINKS", "0")]);
    assert!(!caps.hyperlinks, "the hyperlink override still wins");
    assert_eq!(caps.images, Some(ImageProtocol::Kitty));
}

/// The surrounding detection table (unchanged by D21) still holds: tmux and
/// screen get no image protocol, the kitty/ghostty/wezterm/warp terminals do,
/// iTerm2 uses its own protocol.
#[test]
fn d21_protocol_detection_table() {
    assert_eq!(
        caps_of(&[("TMUX", "/tmp/tmux-1000/default,1234,0")]).images,
        None
    );
    assert_eq!(caps_of(&[("TERM", "tmux-256color")]).images, None);
    assert_eq!(caps_of(&[("TERM", "screen-256color")]).images, None);
    assert_eq!(
        caps_of(&[("KITTY_WINDOW_ID", "1")]).images,
        Some(ImageProtocol::Kitty)
    );
    assert_eq!(
        caps_of(&[("TERM_PROGRAM", "ghostty")]).images,
        Some(ImageProtocol::Kitty)
    );
    assert_eq!(
        caps_of(&[("WEZTERM_PANE", "0")]).images,
        Some(ImageProtocol::Kitty)
    );
    assert_eq!(
        caps_of(&[("TERM_PROGRAM", "WarpTerminal")]).images,
        Some(ImageProtocol::Kitty)
    );
    assert_eq!(
        caps_of(&[("ITERM_SESSION_ID", "w0t0p0")]).images,
        Some(ImageProtocol::Iterm2)
    );
    assert_eq!(caps_of(&[("WT_SESSION", "abc")]).images, None);

    // tmux without an OSC 8-forwarding client: hyperlinks off; with one: on.
    assert!(!detect_capabilities_with(&env_of(&[("TERM", "tmux-256color")]), &|| false).hyperlinks);
    assert!(detect_capabilities_with(&env_of(&[("TERM", "tmux-256color")]), &|| true).hyperlinks);
    // An explicit hyperlink override bypasses the tmux probe entirely.
    let probed = std::cell::Cell::new(false);
    let caps = detect_capabilities_with(
        &env_of(&[
            ("TMUX", "/tmp/tmux-1000/default,1234,0"),
            ("PI_HYPERLINKS", "1"),
            ("PI_IMAGE_PROTOCOL", "kitty"),
        ]),
        &|| {
            probed.set(true);
            false
        },
    );
    assert!(!probed.get(), "the override must bypass the tmux probe");
    assert!(caps.hyperlinks);
    assert_eq!(caps.images, Some(ImageProtocol::Kitty));
}

/// The `PI_*` environment overrides (upstream terminal-image.test.ts):
/// `PI_IMAGE_PROTOCOL` forces or disables the protocol, `PI_TRUE_COLOR`
/// forces the color flag, and `"auto"` keeps the detected value.
#[test]
fn d21_environment_overrides() {
    let caps = caps_of(&[
        ("PI_HYPERLINKS", "1"),
        ("PI_IMAGE_PROTOCOL", "kitty"),
        ("PI_TRUE_COLOR", "1"),
    ]);
    assert_eq!(caps.images, Some(ImageProtocol::Kitty));
    assert!(caps.true_color);
    assert!(caps.hyperlinks);

    let caps = caps_of(&[
        ("TERM_PROGRAM", "iterm.app"),
        ("PI_HYPERLINKS", "0"),
        ("PI_IMAGE_PROTOCOL", "none"),
        ("PI_TRUE_COLOR", "0"),
    ]);
    assert_eq!(caps.images, None);
    assert!(!caps.true_color);
    assert!(!caps.hyperlinks);

    let caps = caps_of(&[
        ("TERM_PROGRAM", "ghostty"),
        ("PI_HYPERLINKS", "auto"),
        ("PI_IMAGE_PROTOCOL", "auto"),
        ("PI_TRUE_COLOR", "auto"),
    ]);
    assert_eq!(caps.images, Some(ImageProtocol::Kitty));
    assert!(caps.true_color);
    assert!(caps.hyperlinks);
}

/// The settings bridge (upstream
/// `SettingsManager.getTerminalCapabilityOverrides`) and the "does this
/// terminal show image settings at all" gate the settings menu uses.
#[test]
fn d21_settings_overrides_and_the_image_rows_gate() {
    let kitty = TerminalSettings {
        images: Some(TerminalImagesMode::Kitty),
        ..Default::default()
    };
    assert_eq!(
        overrides_from_terminal_settings(Some(&kitty)).images,
        Some(Some(ImageProtocol::Kitty))
    );
    let iterm = TerminalSettings {
        images: Some(TerminalImagesMode::Iterm2),
        ..Default::default()
    };
    assert_eq!(
        overrides_from_terminal_settings(Some(&iterm)).images,
        Some(Some(ImageProtocol::Iterm2))
    );
    let off = TerminalSettings {
        images: Some(TerminalImagesMode::Off),
        ..Default::default()
    };
    assert_eq!(
        overrides_from_terminal_settings(Some(&off)).images,
        Some(None),
        "terminal.images: false disables images"
    );
    let auto = TerminalSettings {
        images: Some(TerminalImagesMode::Auto),
        ..Default::default()
    };
    assert_eq!(
        overrides_from_terminal_settings(Some(&auto)).images,
        None,
        "\"auto\" keeps the detected protocol"
    );

    // Herdr's detected protocol (D21) resolves the gate to "supported"…
    let herdr: TerminalCapabilities = caps_of(&[("TERM_PROGRAM", "herdr")]);
    assert_eq!(herdr.images, Some(ImageProtocol::Kitty));
    // …and the settings override wins in both directions, independently of the
    // process environment.
    assert!(supports_images(Some(&kitty)));
    assert!(!supports_images(Some(&off)));
    assert!(
        !capabilities_with_overrides(overrides_from_terminal_settings(Some(&off)))
            .images
            .is_some()
    );
}

/// Upstream splices the "Show images" and "Image width" rows in only when the
/// terminal supports images (`settings-selector.ts:759-776`); the rest of the
/// list keeps its upstream order either way.
#[test]
fn d21_settings_menu_image_rows_follow_the_capability_gate() {
    let with_images = settings_items(
        &Settings {
            terminal: Some(TerminalSettings {
                images: Some(TerminalImagesMode::Kitty),
                ..Default::default()
            }),
            ..Settings::default()
        },
        &SettingsMenuContext::default(),
    );
    let ids: Vec<&str> = with_images.iter().map(|item| item.id).collect();
    assert_eq!(ids[0], "autocompact");
    assert_eq!(&ids[1..3], ["show-images", "image-width-cells"]);
    assert_eq!(ids[3], "auto-resize-images");
    assert_eq!(ids[4], "block-images");

    let without_images = settings_items(
        &Settings {
            terminal: Some(TerminalSettings {
                images: Some(TerminalImagesMode::Off),
                ..Default::default()
            }),
            ..Settings::default()
        },
        &SettingsMenuContext::default(),
    );
    let ids: Vec<&str> = without_images.iter().map(|item| item.id).collect();
    assert!(!ids.contains(&"show-images"));
    assert!(!ids.contains(&"image-width-cells"));
    assert_eq!(
        &ids[0..3],
        ["autocompact", "auto-resize-images", "block-images"]
    );
    // The always-available image rows and the theme row stay.
    assert!(ids.contains(&"auto-resize-images"));
    assert_eq!(ids.last().copied(), Some("theme"));
}
