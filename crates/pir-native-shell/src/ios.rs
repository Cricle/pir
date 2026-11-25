//! iOS host boundary placeholder.
//!
//! The NSH iOS backend is an embedding API: an Objective-C/Swift host creates
//! `AppView`, supplies `SurfaceOptions::host`, and adds the returned UIView to
//! its hierarchy. The shared pir bridge remains in `gui_core`; the concrete
//! UIKit lifecycle belongs in the iOS host app.

#[cfg(target_os = "ios")]
pub use nsh_platform_ios::{AppView, SurfaceOptions};

/// The iOS adapter is intentionally host-driven. This function documents the
/// required setup without pretending a Rust CLI can create a UIKit window.
#[cfg(target_os = "ios")]
pub fn supported() -> bool {
    true
}
