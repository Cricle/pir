//! Pure phone helpers: config precedence, URL normalization, envelopes.

use pir_builtin_phone::*;
use serde_json::json;

fn inputs() -> ConfigInputs {
    ConfigInputs {
        default_device_name: "device".to_owned(),
        generated_device_id: Some("generated".to_owned()),
        ..ConfigInputs::default()
    }
}

#[test]
fn url_upgrades_and_appends_agent() {
    assert_eq!(normalize_url("https://x"), "wss://x/agent");
    assert_eq!(normalize_url("http://x/"), "ws://x/agent");
    assert_eq!(normalize_url("wss://x/agent"), "wss://x/agent");
    assert_eq!(normalize_url(" wss://x/agent/ "), "wss://x/agent");
    assert!(is_server_url("ws://x"));
    assert!(!is_server_url("x"));
}

#[test]
fn args_beat_environment() {
    let env = ConfigInputs {
        env_server_url: Some("ws://env".to_owned()),
        env_device_token: Some("env-token".to_owned()),
        ..inputs()
    };
    let config = RemoteConfig::resolve("--server ws://cli --token cli-token", &env).expect("cfg");
    assert_eq!(config.server_url, "ws://cli/agent");
    assert_eq!(config.token, "cli-token");
    assert_eq!(config.device_id, "generated");
    assert!(config.reconnect);
}

#[test]
fn missing_fields_fail_with_typed_errors() {
    assert_eq!(
        RemoteConfig::resolve("", &ConfigInputs::default()),
        Err(ConfigError::MissingServerUrl)
    );
    assert_eq!(
        RemoteConfig::resolve("ws://x", &ConfigInputs::default()),
        Err(ConfigError::MissingDeviceId)
    );
    let mut no_token = inputs();
    no_token.generated_device_id = Some("id".to_owned());
    assert_eq!(
        RemoteConfig::resolve("ws://x", &no_token),
        Err(ConfigError::MissingDeviceToken)
    );
}

#[test]
fn custom_events_are_filtered_deduped_and_capped() {
    let raw = "phone.custom.a, phone.custom.a, bad, phone.custom.b";
    assert_eq!(
        parse_custom_events(raw),
        vec!["phone.custom.a", "phone.custom.b"]
    );
    let many: Vec<String> = (0..40).map(|i| format!("phone.custom.{i}")).collect();
    assert_eq!(
        parse_custom_events(&many.join(",")).len(),
        MAX_CUSTOM_EVENTS
    );
}

#[test]
fn device_file_round_trips() {
    let json = device_file_json("id-1", "name-1");
    let stored = parse_device_file(&json).expect("stored");
    assert_eq!(stored.device_id, "id-1");
    assert_eq!(stored.device_name.as_deref(), Some("name-1"));
    assert!(parse_device_file("{}").is_none());
}

#[test]
fn resync_skips_parent_select() {
    assert_eq!(resync_client_lines(None), vec![r#"{"kind":"refresh"}"#]);
    assert_eq!(
        resync_client_lines(Some(PARENT_SESSION_ID)),
        vec![r#"{"kind":"refresh"}"#]
    );
    let lines = resync_client_lines(Some("child"));
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("session-select"));
}

#[test]
fn envelopes_have_stable_shapes() {
    let hello = hello_frame("id", "name", "win-x64", Some("/w"), true, 5);
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["sessions"][0]["id"], PARENT_SESSION_ID);
    assert_eq!(hello["sessions"][0]["hasPendingUiRequest"], true);
    assert_eq!(AGENT_CAPABILITIES.len(), 5);

    assert!(session_upsert_frame(&json!({"id": PARENT_SESSION_ID})).is_none());
    let upsert = session_upsert_frame(&json!({"id": "child"})).expect("upsert");
    assert_eq!(upsert["session"]["kind"], "parallel");

    assert_eq!(session_remove_frame("x")["type"], "session-remove");
    assert_eq!(frame_for(None, json!({}))["sessionId"], PARENT_SESSION_ID);
}

#[test]
fn status_text_reflects_state() {
    assert_eq!(
        remote_status_text(2, "ws://s", "dev", "id", Some(7), ""),
        "Pi Phone connected to ws://s as dev (id) for 7s"
    );
    assert!(remote_status_text(1, "ws://s", "dev", "id", None, "").contains("connecting"));
    assert!(remote_status_text(3, "ws://s", "dev", "id", None, "boom").contains("boom"));
    assert_eq!(
        remote_status_text(0, "ws://s", "dev", "id", None, ""),
        "Pi Phone disconnected"
    );
}

#[test]
fn quota_windows_are_derived_from_usage() {
    assert!(!quota_visible(Some("other"), Some("gpt-5")));
    assert!(quota_visible(Some("openai-codex"), Some("gpt-5")));

    let usage = json!({
        "rate_limit": {
            "primary_window": {"used_percent": 25.0, "reset_after_seconds": 60.0},
            "secondary_window": {"used_percent": 90.4},
            "allowed": false
        }
    });
    let quota = quota_from_usage(&usage, "gpt-5", 1_000.0);
    assert_eq!(quota["limited"], true);
    assert_eq!(quota["primaryWindow"]["leftPercent"], 75.0);
    assert_eq!(quota["primaryWindow"]["text"], "75%");
    assert_eq!(quota["secondaryWindow"]["leftPercent"], 10.0);

    let spark = json!({
        "additional_rate_limits": [{
            "limit_name": SPARK_LIMIT_NAME,
            "rate_limit": {"primary_window": {"used_percent": 0.0}}
        }]
    });
    let spark_quota = quota_from_usage(&spark, SPARK_MODEL_ID, 0.0);
    assert_eq!(spark_quota["primaryWindow"]["leftPercent"], 100.0);

    assert_eq!(unavailable_quota()["visible"], false);
    assert!(unavailable_quota_with_error("e")["error"] == "e");
}

#[test]
fn catalog_activity_index_skips_parent() {
    let catalog = json!({
        "data": {"sessions": [
            {"id": PARENT_SESSION_ID, "lastActivityAt": 1},
            {"id": "child", "lastActivityAt": 9},
        ]}
    });
    let index = catalog_activity_index(&catalog);
    assert_eq!(index.len(), 1);
    assert_eq!(index.get("child"), Some(&9));
}

#[test]
fn plugin_identity_is_the_builtin_path() {
    let plugin = PhonePlugin::new();
    use pir_plugin_api::BuiltinPlugin;
    assert_eq!(plugin.info().path, "builtin:phone");
}
