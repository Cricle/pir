//! Native Shell GUI bridge for pir.
//!
//! The headless state/reducer is intentionally usable without desktop
//! dependencies. The optional `desktop` feature adds the Native Shell/GPUI
//! runner and keeps the engine/UI boundary explicit.

pub mod gui_core;

#[cfg(feature = "desktop")]
pub mod desktop;

#[cfg(all(feature = "mobile", target_os = "android"))]
pub mod android;
/// Mobile platform adapters are target-gated. Android uses the NSH JNI host
/// contract and iOS uses the UIKit `AppView`; both reuse `gui_core`.
#[cfg(all(feature = "mobile", target_os = "ios"))]
pub mod ios;

pub use gui_core::{GuiCommand, GuiEvent, GuiMessage, GuiState, NativeShellBridge};
