//! Clipboard paste parity (upstream `handleClipboardPaste`,
//! interactive-mode.ts:3154): file paths first, then images as temp-file
//! paths, then text.
//!
//! The platform readers are injected so every precedence rule and the
//! tri-state image backend fallback are pinned without touching the real
//! clipboard.

use std::sync::atomic::{AtomicBool, Ordering};

use pir::tui::clipboard::{
    contains_control_characters, extension_for_image_mime_type, format_paths_for_insert,
    insert_with_cursor_spacing, quote_if_needed, read_image_with, read_paste_with,
    select_preferred_image_mime_type, validate_clipboard_file_paths, write_clipboard_temp_file,
    ClipboardImage, ClipboardPaste, ImageAttempt, ImageRouteEnv, CLIPBOARD_PATH_CONTROL_ERROR,
};

fn route_env(linux: bool, wayland: bool, wsl: bool, termux: bool, display: bool) -> ImageRouteEnv {
    ImageRouteEnv {
        linux,
        wayland,
        wsl,
        termux,
        display,
    }
}

fn image(label: &str) -> ClipboardImage {
    ClipboardImage {
        bytes: label.as_bytes().to_vec(),
        mime_type: "image/png".to_owned(),
    }
}

#[test]
fn extension_follows_the_supported_mime_table() {
    assert_eq!(extension_for_image_mime_type("image/png"), Some("png"));
    assert_eq!(
        extension_for_image_mime_type("image/jpeg; charset=binary"),
        Some("jpg")
    );
    assert_eq!(extension_for_image_mime_type("IMAGE/WEBP"), Some("webp"));
    assert_eq!(extension_for_image_mime_type("image/gif"), Some("gif"));
    assert_eq!(extension_for_image_mime_type("image/bmp"), None);
    assert_eq!(extension_for_image_mime_type("text/plain"), None);
}

#[test]
fn preferred_mime_selection_matches_upstream_order() {
    let offered = vec![
        "text/html".to_owned(),
        "image/gif".to_owned(),
        "image/jpeg".to_owned(),
        "image/png".to_owned(),
    ];
    assert_eq!(
        select_preferred_image_mime_type(&offered).as_deref(),
        Some("image/png")
    );

    // Parameters are preserved in the requested type (upstream returns the
    // raw trimmed entry), while matching is done on the base type.
    let offered = vec![
        "  image/webp; charset=binary  ".to_owned(),
        "image/gif".to_owned(),
    ];
    assert_eq!(
        select_preferred_image_mime_type(&offered).as_deref(),
        Some("image/webp; charset=binary")
    );

    // No preferred type: the first image/* candidate wins, even unsupported.
    let offered = vec!["text/plain".to_owned(), "image/bmp".to_owned()];
    assert_eq!(
        select_preferred_image_mime_type(&offered).as_deref(),
        Some("image/bmp")
    );

    assert_eq!(select_preferred_image_mime_type(&[]), None);
    assert_eq!(
        select_preferred_image_mime_type(&["text/plain".to_owned()]),
        None
    );
}

#[test]
fn quote_if_needed_uses_posix_single_quotes() {
    // Safe characters pass through untouched.
    assert_eq!(quote_if_needed("AZaz09_-./~:@"), "AZaz09_-./~:@");
    assert_eq!(quote_if_needed("/tmp/a_b-c.rs"), "/tmp/a_b-c.rs");

    // Anything else is single-quoted; embedded quotes use '\''.
    assert_eq!(quote_if_needed("with space"), "'with space'");
    assert_eq!(quote_if_needed("it's"), "'it'\\''s'");
    assert_eq!(quote_if_needed("报告.md"), "'报告.md'");

    // The empty string becomes an explicit empty pair, not nothing.
    assert_eq!(quote_if_needed(""), "''");
}

#[test]
fn format_paths_joins_by_mode() {
    let paths = vec!["/tmp/a b".to_owned(), "/tmp/c'd".to_owned()];

    assert_eq!(format_paths_for_insert(&paths, false), "/tmp/a b\n/tmp/c'd");
    assert_eq!(
        format_paths_for_insert(&paths, true),
        "'/tmp/a b' '/tmp/c'\\''d'"
    );

    let safe = vec!["/tmp/a".to_owned(), "/tmp/b".to_owned()];
    assert_eq!(format_paths_for_insert(&safe, true), "/tmp/a /tmp/b");
}

#[test]
fn control_characters_are_detected_and_rejected() {
    assert!(contains_control_characters("bad\u{7}path"));
    // Newline is Unicode category Cc too (upstream /\p{Cc}/u).
    assert!(contains_control_characters("bad\npath"));
    assert!(!contains_control_characters("/tmp/报告.md"));
    assert!(!contains_control_characters(""));

    assert_eq!(
        validate_clipboard_file_paths(&["/tmp/ok".to_owned()]),
        Ok(())
    );
    assert_eq!(
        validate_clipboard_file_paths(&["/tmp/ok".to_owned(), "/tmp/bad\u{1b}".to_owned()]),
        Err(CLIPBOARD_PATH_CONTROL_ERROR.to_owned())
    );
    assert_eq!(
        CLIPBOARD_PATH_CONTROL_ERROR,
        "Clipboard file path contains control characters"
    );
}

#[test]
fn cursor_spacing_only_at_non_whitespace_neighbours() {
    assert_eq!(insert_with_cursor_spacing("x", Some('a'), Some('b')), " x ");
    assert_eq!(insert_with_cursor_spacing("x", Some('a'), None), " x");
    assert_eq!(insert_with_cursor_spacing("x", None, Some('b')), "x ");
    assert_eq!(insert_with_cursor_spacing("x", Some(' '), Some('\t')), "x");
    assert_eq!(insert_with_cursor_spacing("x", None, None), "x");
}

#[test]
fn temp_file_writes_unique_exact_bytes_and_survives() {
    let payload = b"\x89PNG\r\n\x1a\nclipboard-bytes";
    let first = write_clipboard_temp_file(payload, "png").expect("first temp file");
    let second = write_clipboard_temp_file(b"second", "jpg").expect("second temp file");

    assert_ne!(first, second, "every paste gets a unique file");
    assert_eq!(first.parent(), Some(std::env::temp_dir().as_path()));
    assert_eq!(std::fs::read(&first).expect("first file readable"), payload);
    assert!(
        first.exists(),
        "the temp file is never deleted by the writer"
    );

    let name = first.file_name().unwrap().to_string_lossy();
    assert!(name.starts_with("pi-clipboard-"), "name was {name}");
    assert!(name.ends_with(".png"), "name was {name}");
    assert!(second
        .file_name()
        .unwrap()
        .to_string_lossy()
        .ends_with(".jpg"));

    // Test hygiene only: production intentionally keeps these files.
    let _ = std::fs::remove_file(&first);
    let _ = std::fs::remove_file(&second);
}

#[test]
fn paste_precedence_is_files_then_image_then_text() {
    let image_called = AtomicBool::new(false);
    let text_called = AtomicBool::new(false);
    let result = read_paste_with(
        || Some(vec!["/tmp/one".to_owned(), "/tmp/two".to_owned()]),
        || {
            image_called.store(true, Ordering::SeqCst);
            None
        },
        || {
            text_called.store(true, Ordering::SeqCst);
            None
        },
    )
    .expect("file paste succeeds");
    assert_eq!(
        result,
        Some(ClipboardPaste::FilePaths(vec![
            "/tmp/one".to_owned(),
            "/tmp/two".to_owned()
        ]))
    );
    assert!(!image_called.load(Ordering::SeqCst));
    assert!(!text_called.load(Ordering::SeqCst));

    // No files: the image wins over text.
    let text_called = AtomicBool::new(false);
    let result = read_paste_with(
        || None,
        || Some(image("from-image")),
        || {
            text_called.store(true, Ordering::SeqCst);
            None
        },
    )
    .expect("image paste succeeds")
    .expect("image present");
    match result {
        ClipboardPaste::ImagePath(path) => {
            assert!(path.ends_with(".png"), "path was {path}");
            assert_eq!(std::fs::read(&path).expect("image readable"), b"from-image");
            let _ = std::fs::remove_file(path);
        }
        other => panic!("expected image path, got {other:?}"),
    }
    assert!(!text_called.load(Ordering::SeqCst));

    // No files, no image: text fallback.
    let result =
        read_paste_with(|| None, || None, || Some("plain".to_owned())).expect("text paste");
    assert_eq!(result, Some(ClipboardPaste::Text("plain".to_owned())));

    // Nothing at all is silent.
    assert_eq!(
        read_paste_with(|| None, || None, || None).expect("empty paste"),
        None
    );
}

#[test]
fn control_character_error_stops_the_paste_chain() {
    // Neither the image nor the text reader may run after the path error.
    let result = read_paste_with(
        || Some(vec!["/tmp/bad\u{7}".to_owned()]),
        || panic!("image reader must not run"),
        || panic!("text reader must not run"),
    );
    assert_eq!(
        result,
        Err("Clipboard file path contains control characters".to_owned())
    );
}

#[test]
fn non_linux_uses_native_only() {
    let result = read_image_with(
        &route_env(false, false, true, false, true),
        || panic!("wl-paste must not run outside Linux"),
        || panic!("xclip must not run outside Linux"),
        || panic!("PowerShell must not run outside Linux"),
        || ImageAttempt::Data(image("native")),
    );
    assert_eq!(result, Some(image("native")));
}

#[test]
fn linux_x11_tries_xclip_then_native() {
    let native_called = AtomicBool::new(false);
    let result = read_image_with(
        &route_env(true, false, false, false, true),
        || panic!("wl-paste requires Wayland/WSL"),
        || ImageAttempt::Unavailable,
        || panic!("PowerShell requires WSL"),
        || {
            native_called.store(true, Ordering::SeqCst);
            ImageAttempt::Data(image("native"))
        },
    );
    assert_eq!(result, Some(image("native")));
    assert!(native_called.load(Ordering::SeqCst));

    // An xclip answer with image data ends the chain.
    let native_called = AtomicBool::new(false);
    let result = read_image_with(
        &route_env(true, false, false, false, true),
        || panic!("wl-paste requires Wayland/WSL"),
        || ImageAttempt::Data(image("xclip")),
        || panic!("PowerShell requires WSL"),
        || {
            native_called.store(true, Ordering::SeqCst);
            ImageAttempt::Data(image("native"))
        },
    );
    assert_eq!(result, Some(image("xclip")));
    assert!(!native_called.load(Ordering::SeqCst));
}

#[test]
fn linux_without_display_does_not_try_native_x11() {
    let result = read_image_with(
        &route_env(true, false, false, false, false),
        || panic!("wl-paste requires Wayland/WSL"),
        || ImageAttempt::Unavailable,
        || panic!("PowerShell requires WSL"),
        || panic!("native X11 requires DISPLAY"),
    );
    assert_eq!(result, None);
}

#[test]
fn wayland_empty_stops_x11_fallthrough() {
    let xclip_called = AtomicBool::new(false);
    let native_called = AtomicBool::new(false);
    let result = read_image_with(
        &route_env(true, true, false, false, true),
        || ImageAttempt::Empty,
        || {
            xclip_called.store(true, Ordering::SeqCst);
            ImageAttempt::Data(image("stale-x11"))
        },
        || panic!("PowerShell requires WSL"),
        || {
            native_called.store(true, Ordering::SeqCst);
            ImageAttempt::Data(image("native"))
        },
    );
    assert_eq!(result, None, "an empty Wayland clipboard is authoritative");
    assert!(!xclip_called.load(Ordering::SeqCst));
    assert!(!native_called.load(Ordering::SeqCst));
}

#[test]
fn wayland_unavailable_falls_through_to_xclip() {
    let native_called = AtomicBool::new(false);
    let result = read_image_with(
        &route_env(true, true, false, false, true),
        || ImageAttempt::Unavailable,
        || ImageAttempt::Data(image("xclip")),
        || panic!("PowerShell requires WSL"),
        || {
            native_called.store(true, Ordering::SeqCst);
            ImageAttempt::Data(image("native"))
        },
    );
    assert_eq!(result, Some(image("xclip")));
    assert!(!native_called.load(Ordering::SeqCst));
}

#[test]
fn wsl_uses_powershell_after_the_linux_backends() {
    let native_called = AtomicBool::new(false);
    let result = read_image_with(
        &route_env(true, false, true, false, true),
        || ImageAttempt::Unavailable,
        || ImageAttempt::Unavailable,
        || Some(image("powershell")),
        || {
            native_called.store(true, Ordering::SeqCst);
            ImageAttempt::Data(image("native"))
        },
    );
    assert_eq!(result, Some(image("powershell")));
    assert!(!native_called.load(Ordering::SeqCst));
}

#[test]
fn wsl_powershell_empty_does_not_fall_through_to_native() {
    let native_called = AtomicBool::new(false);
    let result = read_image_with(
        &route_env(true, false, true, false, true),
        || ImageAttempt::Unavailable,
        || ImageAttempt::Empty,
        || None,
        || {
            native_called.store(true, Ordering::SeqCst);
            ImageAttempt::Data(image("native"))
        },
    );
    assert_eq!(result, None);
    assert!(!native_called.load(Ordering::SeqCst));
}

#[test]
fn termux_never_reads_an_image() {
    let result = read_image_with(
        &route_env(true, true, false, true, true),
        || panic!("Termux has no image clipboard"),
        || panic!("Termux has no image clipboard"),
        || panic!("Termux has no image clipboard"),
        || panic!("Termux has no image clipboard"),
    );
    assert_eq!(result, None);
}
