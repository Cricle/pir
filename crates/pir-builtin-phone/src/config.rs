//! Pure phone configuration resolution.
//!
//! The engine copy read the process environment and the agent directory
//! directly. Here the host passes those values in through [`ConfigInputs`], so
//! the precedence rules are testable without touching global state or disk.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::url::{is_server_url, normalize_url};

/// Device identity file, relative to the agent directory.
pub const DEVICE_FILE: &str = "phone-device.json";

pub const ENV_SERVER_URL: &str = "PI_PHONE_SERVER_URL";
pub const ENV_DEVICE_TOKEN: &str = "PI_PHONE_DEVICE_TOKEN";
pub const ENV_TOKEN: &str = "PI_PHONE_TOKEN";
pub const ENV_DEVICE_NAME: &str = "PI_PHONE_DEVICE_NAME";
pub const ENV_DEVICE_ID: &str = "PI_PHONE_DEVICE_ID";
pub const ENV_CUSTOM_EVENTS: &str = "PI_PHONE_CUSTOM_EVENTS";

/// Namespaced custom event prefix accepted by `PI_PHONE_CUSTOM_EVENTS`.
pub const PHONE_CUSTOM_EVENT_PREFIX: &str = "phone.custom.";
/// Upper bound on one forwarded custom-event payload.
pub const PHONE_CUSTOM_EVENT_MAX_BYTES: usize = 64 * 1024;
/// Maximum number of opt-in custom event names.
pub const MAX_CUSTOM_EVENTS: usize = 32;
/// Maximum accepted length of one custom event name.
pub const MAX_CUSTOM_EVENT_NAME_BYTES: usize = 128;

/// Host events the bridge maps to phone envelopes.
pub const PHONE_HOST_EVENT_NAMES: [&str; 15] = [
    "agent_start",
    "agent_end",
    "message_start",
    "message_update",
    "message_end",
    "tool_execution_start",
    "tool_execution_update",
    "tool_execution_end",
    "queue_update",
    "compaction_start",
    "compaction_end",
    "auto_retry_start",
    "auto_retry_end",
    "extension_ui_request",
    "status",
];

/// Resolved remote connection settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfig {
    pub server_url: String,
    pub token: String,
    pub device_id: String,
    pub device_name: String,
    pub reconnect: bool,
}

/// Device identity persisted in [`DEVICE_FILE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredDevice {
    pub device_id: String,
    #[serde(default)]
    pub device_name: Option<String>,
}

/// Values the host resolves before calling [`RemoteConfig::resolve`].
#[derive(Debug, Clone, Default)]
pub struct ConfigInputs {
    /// `PI_PHONE_SERVER_URL`.
    pub env_server_url: Option<String>,
    /// `PI_PHONE_DEVICE_TOKEN` (wins over [`Self::env_token`]).
    pub env_device_token: Option<String>,
    /// `PI_PHONE_TOKEN` fallback.
    pub env_token: Option<String>,
    /// `PI_PHONE_DEVICE_NAME`.
    pub env_device_name: Option<String>,
    /// `PI_PHONE_DEVICE_ID`.
    pub env_device_id: Option<String>,
    /// Hostname-derived default (`COMPUTERNAME`/`HOSTNAME`/`pir-device`).
    pub default_device_name: String,
    /// Parsed [`DEVICE_FILE`], when the host found a valid one.
    pub stored_device: Option<StoredDevice>,
    /// A host-generated UUID for a first run, when no file existed.
    pub generated_device_id: Option<String>,
}

/// Configuration resolution failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    MissingServerUrl,
    MissingDeviceToken,
    MissingDeviceId,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingServerUrl => formatter.write_str(
                "missing phone server URL. Usage: /phone-start <ws(s)://server> [device-token] \
                 [--device-name name]",
            ),
            Self::MissingDeviceToken => formatter.write_str(
                "missing device token. Pass it after the URL, use --token, or set \
                 PI_PHONE_DEVICE_TOKEN.",
            ),
            Self::MissingDeviceId => formatter
                .write_str("missing phone device id; the host must generate and persist one"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl RemoteConfig {
    /// Resolve configuration from CLI arguments and host-provided inputs.
    ///
    /// Precedence matches the engine copy: arguments beat environment values,
    /// which beat stored state. A stored device id is only consulted when no
    /// device id was supplied, and its name replaces an empty/default name.
    pub fn resolve(args: &str, inputs: &ConfigInputs) -> Result<Self, ConfigError> {
        let mut server_url = inputs.env_server_url.clone().unwrap_or_default();
        let mut token = inputs
            .env_device_token
            .clone()
            .or_else(|| inputs.env_token.clone())
            .unwrap_or_default();
        let mut device_name = inputs
            .env_device_name
            .clone()
            .unwrap_or_else(|| inputs.default_device_name.clone());
        let mut device_id = inputs.env_device_id.clone().unwrap_or_default();
        let mut reconnect = true;

        let tokens: Vec<&str> = args.split_whitespace().collect();
        let mut index = 0;
        while index < tokens.len() {
            let value = tokens[index];
            match value {
                "--server" | "--url" if index + 1 < tokens.len() => {
                    server_url = tokens[index + 1].to_owned();
                    index += 2;
                }
                "--token" if index + 1 < tokens.len() => {
                    token = tokens[index + 1].to_owned();
                    index += 2;
                }
                "--device-name" | "--name" if index + 1 < tokens.len() => {
                    device_name = tokens[index + 1].to_owned();
                    index += 2;
                }
                "--device-id" if index + 1 < tokens.len() => {
                    device_id = tokens[index + 1].to_owned();
                    index += 2;
                }
                "--no-reconnect" => {
                    reconnect = false;
                    index += 1;
                }
                value if value.starts_with("--server=") => {
                    server_url = value["--server=".len()..].to_owned();
                    index += 1;
                }
                value if value.starts_with("--url=") => {
                    server_url = value["--url=".len()..].to_owned();
                    index += 1;
                }
                value if value.starts_with("--token=") => {
                    token = value["--token=".len()..].to_owned();
                    index += 1;
                }
                value if value.starts_with("--device-name=") => {
                    device_name = value["--device-name=".len()..].to_owned();
                    index += 1;
                }
                value if value.starts_with("--device-id=") => {
                    device_id = value["--device-id=".len()..].to_owned();
                    index += 1;
                }
                value if !value.starts_with('-') && is_server_url(value) => {
                    server_url = value.to_owned();
                    index += 1;
                }
                value if !value.starts_with('-') && token.is_empty() => {
                    token = value.to_owned();
                    index += 1;
                }
                _ => {
                    index += 1;
                }
            }
        }

        if server_url.trim().is_empty() {
            return Err(ConfigError::MissingServerUrl);
        }
        let server_url = normalize_url(&server_url);
        if device_id.trim().is_empty() {
            if let Some(stored) = &inputs.stored_device {
                device_id = stored.device_id.clone();
                if let Some(name) = &stored.device_name {
                    if device_name.is_empty() || device_name == inputs.default_device_name {
                        device_name = name.clone();
                    }
                }
            } else if let Some(generated) = &inputs.generated_device_id {
                device_id = generated.clone();
            }
        }
        if device_id.trim().is_empty() {
            return Err(ConfigError::MissingDeviceId);
        }
        if token.trim().is_empty() {
            return Err(ConfigError::MissingDeviceToken);
        }

        Ok(Self {
            server_url,
            token,
            device_id,
            device_name,
            reconnect,
        })
    }

    /// Path of [`DEVICE_FILE`] inside a host-provided agent directory.
    pub fn device_file_path(agent_dir: &Path) -> PathBuf {
        agent_dir.join(DEVICE_FILE)
    }
}

/// Default device name from hostname candidates.
pub fn default_device_name_with(computer_name: Option<&str>, hostname: Option<&str>) -> String {
    computer_name
        .or(hostname)
        .unwrap_or("pir-device")
        .to_owned()
}

/// Encode the device identity file written on first run.
pub fn device_file_json(device_id: &str, device_name: &str) -> String {
    serde_json::json!({ "deviceId": device_id, "deviceName": device_name }).to_string()
}

/// Parse a persisted device identity, tolerating an absent `deviceName`.
pub fn parse_device_file(text: &str) -> Option<StoredDevice> {
    let value: Value = serde_json::from_str(text).ok()?;
    let device_id = value.get("deviceId").and_then(Value::as_str)?;
    let device_name = value
        .get("deviceName")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(StoredDevice {
        device_id: device_id.to_owned(),
        device_name,
    })
}

/// Parse `PI_PHONE_CUSTOM_EVENTS`: comma-separated namespaced event names.
///
/// Names are filtered, deduplicated (first occurrence wins) and capped at
/// [`MAX_CUSTOM_EVENTS`], in that order.
pub fn parse_custom_events(value: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in value.split(',').map(str::trim) {
        if !name.starts_with(PHONE_CUSTOM_EVENT_PREFIX) || name.len() > MAX_CUSTOM_EVENT_NAME_BYTES
        {
            continue;
        }
        if names.len() >= MAX_CUSTOM_EVENTS {
            break;
        }
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_owned());
        }
    }
    names
}
