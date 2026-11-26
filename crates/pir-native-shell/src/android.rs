//! Android host boundary placeholder.
//!
//! NSH Android exposes a JNI `libnsh.so` contract rather than a public Rust
//! `HostChannel`/window API. The Android host app must forward `Native.*`
//! host messages to the shared `gui_core` bridge; this module keeps the mobile
//! feature target-gated without making GNU/headless builds depend on JNI.

#[cfg(target_os = "android")]
pub use nsh_platform_android::{Registry, Root};

#[cfg(target_os = "android")]
pub fn supported() -> bool {
    true
}
