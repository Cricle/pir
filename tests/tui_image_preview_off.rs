//! Feature-off arm of the TUI image preview.
//!
//! A slim TUI build without `image-protocol` must still answer
//! `terminal.showImages: true` with the upstream `[Image: …]` text fallback.
//! This target is compiled with the default `full` too; the whole test is
//! cfg'd out there, where `tui_image_preview_units` covers the renderer.

#![cfg(not(feature = "image-protocol"))]

use std::path::Path;

use pir::tui::image_preview::{render_image_with_settings, ImageSettings};

#[test]
fn show_true_without_the_feature_renders_the_text_fallback() {
    let visible = render_image_with_settings(
        Path::new("definitely-missing.png"),
        10,
        ImageSettings {
            width_cells: 60,
            show: true,
        },
    )
    .expect("falls back instead of erroring");
    assert_eq!(visible.len(), 1);
    assert!(visible[0].starts_with("[Image: definitely-missing.png "));
    assert!(visible[0].contains("[image/png]"));
    // The read error is carried, exactly as in the `show = false` arm.
    assert!(visible[0].contains("No such file") || visible[0].contains("os error"));
}
