//! Independent, engine-free package for the `phone` builtin.
//!
//! This crate owns the pure half of the phone client: argument/config
//! resolution, server-URL normalization, the Codex quota response derivation,
//! relay envelope/resync shaping and the remote status text.
//!
//! It depends on `pir-plugin-api` and third-party crates only — never on
//! `pir-core` or `pir-sdk`. The host supplies every impure edge:
//!
//! - process environment values and the agent directory
//!   ([`config::ConfigInputs`]);
//! - device-id generation/persistence ([`config::device_file_json`]);
//! - the HTTP transport for the Codex quota lookup;
//! - the WebSocket transport and the `SessionBridge` runtime.
//!
//! Keeping the transport host-side is deliberate: the relay client is
//! transport-bound, while the envelopes and derivation rules above are pure
//! and testable without a runtime.

#![deny(unsafe_code)]

pub mod config;
pub mod protocol;
pub mod quota;
pub mod url;

pub use config::{
    default_device_name_with, device_file_json, parse_custom_events, parse_device_file,
    ConfigError, ConfigInputs, RemoteConfig, StoredDevice, DEVICE_FILE, ENV_CUSTOM_EVENTS,
    ENV_DEVICE_ID, ENV_DEVICE_NAME, ENV_DEVICE_TOKEN, ENV_SERVER_URL, ENV_TOKEN, MAX_CUSTOM_EVENTS,
    MAX_CUSTOM_EVENT_NAME_BYTES, PHONE_CUSTOM_EVENT_MAX_BYTES, PHONE_CUSTOM_EVENT_PREFIX,
    PHONE_HOST_EVENT_NAMES,
};
pub use protocol::{
    frame_for, hello_frame, remote_status_text, resync_client_lines, session_remove_frame,
    session_upsert_frame, AGENT_CAPABILITIES, PARENT_SESSION_ID, STATE_CONNECTED, STATE_CONNECTING,
    STATE_OFF, STATE_RECONNECTING,
};
pub use quota::{
    catalog_activity_index, now_secs, quota_from_usage, quota_visible, unavailable_quota,
    unavailable_quota_with_error, SPARK_LIMIT_NAME, SPARK_MODEL_ID,
};
pub use url::{is_server_url, normalize_url};

use pir_plugin_api::{BuiltinPlugin, PluginInfo};

/// Minimal protocol-facing shell for the phone builtin.
pub struct PhonePlugin {
    info: PluginInfo,
}

impl PhonePlugin {
    pub fn new() -> Self {
        Self {
            info: PluginInfo::builtin(PLUGIN_NAME),
        }
    }
}

impl BuiltinPlugin for PhonePlugin {
    fn info(&self) -> &PluginInfo {
        &self.info
    }
}

pir_plugin_api::builtin_plugin_shell! {
    plugin = PhonePlugin,
    name = "phone",
    factory = new_phone_plugin,
}
