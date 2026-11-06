//! Clipboard-image attachment helper for the TUI's Ctrl+V path.
//!
//! [`crate::tui::clipboard::read_paste`] is the upstream-shaped paste
//! pipeline (file list → image as a temp-file PATH → text). That shape keeps a
//! pasted image as a plain path in the editor, which the model only sees if it
//! chooses to call `read`; it is not an image *attachment*. This module adds
//! the attachment half without changing the public paste enum:
//!
//! - [`read_image_attachment`] returns the clipboard image already encoded for
//!   an `ImageContent` block (base64 + MIME), so a host can attach it to the
//!   next prompt;
//! - [`image_path_reference`] renders a paste-pipeline temp path as an
//!   `@"…"` prompt reference. The shared prompt expander
//!   (`coding_agent::context::expand_prompt_file_args`) then turns it into an
//!   image attachment on submit — the minimal TUI hunk documented in
//!   `fix/IMGREF.md`.
//!
//! Windows retry route: `arboard::Clipboard::get_image` (CF_DIB / CF_DIBV5)
//! is the fast path. Some Windows clipboard owners only publish the legacy
//! DIB (or a registered `PNG` payload) that arboard rejects; when that fast
//! path returns nothing, [`read_image_attachment`] falls back to PowerShell's
//! `System.Windows.Forms.Clipboard::GetImage()` / `GetData('PNG')`, saving the
//! result as a real PNG temp file. PowerShell is already a dependency-free
//! Windows built-in and the same mechanism the WSL route in `clipboard.rs`
//! uses, so no new crate is added.

use base64::Engine as _;

use crate::ai::types::ImageContent;
use crate::tui::clipboard::ClipboardImage;

/// Encode one clipboard image as the inline `ImageContent` attachment shape
/// (base64 payload + MIME type). Pure so tests can pin the wire shape without
/// touching the real clipboard.
pub fn image_content_from_clipboard_image(image: &ClipboardImage) -> ImageContent {
    ImageContent {
        data: base64::engine::general_purpose::STANDARD.encode(&image.bytes),
        mime_type: image.mime_type.clone(),
    }
}

/// Render one file path as a prompt `@"…"` reference understood by
/// `coding_agent::context::expand_prompt_file_args`.
///
/// Backslashes and double quotes are escaped for the prompt scanner's quoted
/// token grammar (inside a quoted token `\\` → `\` and `\"` → `"`); the
/// surrounding quotes keep spaces and Windows separators intact.
pub fn image_path_reference(path: &str) -> String {
    let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
    format!("@\"{escaped}\"")
}

/// Read the clipboard image as an inline attachment for Ctrl+V.
///
/// Route order: the existing platform reader first (arboard on Windows/macOS,
/// wl-paste/xclip/PowerShell-WSL on Linux), then — on native Windows only —
/// the PowerShell PNG/CF_DIB retry. Unsupported formats were already
/// normalized to PNG by [`crate::tui::clipboard::read_image`]; the Windows
/// retry always produces PNG.
pub fn read_image_attachment() -> Option<ImageContent> {
    let image = crate::tui::clipboard::read_image().or_else(read_windows_png_image)?;
    Some(image_content_from_clipboard_image(&image))
}

/// The PowerShell script that saves the Windows clipboard image (CF_DIB,
/// CF_DIBV5 or the registered `PNG` format) to `path`. The path is
/// single-quoted with `''` escaping. Pure so tests can pin the quoting and the
/// two format routes.
pub fn powershell_save_png_script(path: &str) -> String {
    let escaped = path.replace('\'', "''");
    [
        "Add-Type -AssemblyName System.Windows.Forms".to_owned(),
        "Add-Type -AssemblyName System.Drawing".to_owned(),
        format!("$path = '{escaped}'"),
        "$img = [System.Windows.Forms.Clipboard]::GetImage()".to_owned(),
        "if ($img) { $img.Save($path, [System.Drawing.Imaging.ImageFormat]::Png); Write-Output 'ok' }".to_owned(),
        "else { $data = [System.Windows.Forms.Clipboard]::GetData('PNG'); if ($data -is [System.IO.MemoryStream]) { [System.IO.File]::WriteAllBytes($path, $data.ToArray()); Write-Output 'ok' } elseif ($data -is [byte[]]) { [System.IO.File]::WriteAllBytes($path, $data); Write-Output 'ok' } else { Write-Output 'empty' } }".to_owned(),
    ]
    .join("; ")
}

/// Windows-only PowerShell fallback (CF_DIB / registered PNG): save the
/// clipboard image to a temp PNG and read it back. Returns `None` when the
/// clipboard holds no image, PowerShell fails, or the saved file is not a
/// non-empty PNG.
#[cfg(windows)]
fn read_windows_png_image() -> Option<ClipboardImage> {
    use std::time::{Duration, Instant};

    let temp_path =
        std::env::temp_dir().join(format!("pir-clipboard-{}.png", uuid::Uuid::new_v4()));
    let temp_text = temp_path.to_string_lossy().into_owned();
    let result = (|| {
        let script = powershell_save_png_script(&temp_text);
        let mut command = std::process::Command::new("powershell.exe");
        command
            .args(["-NoProfile", "-STA", "-Command", &script])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        // CREATE_NO_WINDOW: no console flash while Ctrl+V runs on the reader
        // thread.
        {
            use std::os::windows::process::CommandExt as _;
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn().ok()?;
        let stdout = child.stdout.take()?;
        let reader = std::thread::Builder::new()
            .name("pir-clipboard-image".to_owned())
            .spawn(move || {
                use std::io::Read as _;
                let mut output = String::new();
                let mut handle = stdout.take(64 * 1024);
                handle.read_to_string(&mut output).ok()?;
                Some(output)
            })
            .ok()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return None;
                }
            }
        };
        let output = reader.join().ok()??;
        if !status.success() || output.trim() != "ok" {
            return None;
        }
        let bytes = std::fs::read(&temp_path).ok()?;
        if bytes.is_empty() {
            return None;
        }
        // The save route always writes PNG; verify the signature so a broken
        // `GetData('PNG')` payload is not attached as an unreadable image.
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return None;
        }
        Some(ClipboardImage {
            bytes,
            mime_type: "image/png".to_owned(),
        })
    })();
    let _ = std::fs::remove_file(&temp_path);
    result
}

#[cfg(not(windows))]
fn read_windows_png_image() -> Option<ClipboardImage> {
    None
}
