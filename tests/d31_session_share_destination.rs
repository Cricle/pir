//! DRIFT W3 D31 (upstream `cea257439` / #10870): the `sessionShareDestination`
//! setting (`"auto" | "gist" | "radius"`, default `"auto"`) selects where
//! `/share` uploads the session, and `/share` honors it.
//!
//! Upstream anchors (`packages/coding-agent`):
//! - `src/core/settings-defaults.ts` — `sessionShareDestination: "auto"`.
//! - `src/core/settings-schema.ts` / `src/core/settings-manager.ts:807-816` —
//!   the field plus `getSessionShareDestination()` (only `gist`/`radius` are
//!   accepted, everything else resolves to the `auto` default) and
//!   `setSessionShareDestination()` (global scope, `markModified` + save).
//! - `src/modes/interactive/session-share.ts:69-76` — the routing decision:
//!   `gist` skips Radius entirely; otherwise Radius is attempted first, and
//!   when it is unavailable `auto` falls back to a gist while `radius` reports
//!   `Radius authentication is not configured. Run '/login radius' first.`
//!   with no fallback.
//! - `src/modes/interactive/session-share.ts:124-140` — the Radius upload:
//!   `POST /v1/artifacts?visibility=organization&title=Pi+session` with
//!   `Content-Type: application/x-ndjson` and a Bearer token, answering with
//!   `{ "artifact": { "canonical_url": ... } }`.
//! - `src/modes/interactive/components/settings-selector.ts:515-522` — the
//!   `Session share destination` menu row cycling `auto`, `gist`, `radius`.
//! - `src/core/slash-commands.ts` — the rewritten `/share` description
//!   `Share session via a Radius artifact or a GitHub gist`.
//!
//! Standalone binary (no `g_*` suite registration): tests/ and tests/suites/
//! are a concurrent-edit conflict zone.

use std::path::PathBuf;

use pir::config::defaults::settings_defaults;
use pir::config::settings::{SessionShareDestination, Settings, SettingsManager};
use pir::tui::settings_menu::{
    apply_settings_action, item_action, settings_items, SettingsMenuContext,
};
use pir::tui::slash::{
    radius_artifact_error_from_response, radius_artifact_request,
    radius_artifact_url_from_response, share_route, ShareRoute, RADIUS_AUTH_MISSING,
};

/// A fresh settings directory per test (the manager writes the global scope).
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pir-d31-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp settings dir");
    dir
}

// ---------------------------------------------------------------------------
// The setting: default, tolerance, persistence
// ---------------------------------------------------------------------------

#[test]
fn destination_defaults_to_auto() {
    // Upstream `SETTINGS_DEFAULTS.sessionShareDestination`.
    assert_eq!(settings_defaults::SESSION_SHARE_DESTINATION, "auto");
    assert_eq!(
        SessionShareDestination::default(),
        SessionShareDestination::Auto
    );
    // An unset field is absent from the typed settings and reads as `auto`.
    let settings = Settings::default();
    assert_eq!(settings.session_share_destination, None);
    assert_eq!(
        settings.session_share_destination.unwrap_or_default(),
        SessionShareDestination::Auto
    );

    let dir = temp_dir("default");
    let manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, false);
    assert_eq!(
        manager.session_share_destination(),
        SessionShareDestination::Auto
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn destination_values_use_the_upstream_wire_spelling() {
    for (value, wire) in [
        (SessionShareDestination::Auto, "auto"),
        (SessionShareDestination::Gist, "gist"),
        (SessionShareDestination::Radius, "radius"),
    ] {
        assert_eq!(
            serde_json::to_value(value).unwrap(),
            serde_json::json!(wire)
        );
        assert_eq!(
            serde_json::from_value::<SessionShareDestination>(serde_json::json!(wire)).unwrap(),
            value
        );
    }
}

/// Upstream only accepts `gist`/`radius` and resolves everything else to the
/// `auto` default; a wrong-typed field is diagnosed per-field and never fails
/// the load (the pre-D31 behaviour was "unknown key ignored", so this also
/// pins that the newly-known key is not a load hazard).
#[test]
fn unknown_destination_values_fall_back_to_auto() {
    let dir = temp_dir("unknown");
    std::fs::write(
        dir.join("settings.json"),
        r#"{"sessionShareDestination":"carrier-pigeon"}"#,
    )
    .expect("write settings.json");
    let manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, false);
    assert_eq!(
        manager.session_share_destination(),
        SessionShareDestination::Auto
    );
    assert!(
        manager
            .warning_messages()
            .iter()
            .any(|message| message.contains("sessionShareDestination")),
        "the invalid value must be diagnosed: {:?}",
        manager.warning_messages()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Upstream `setSessionShareDestination` writes the global scope only.
#[test]
fn destination_persists_to_the_global_scope() {
    let dir = temp_dir("persist");
    let mut manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, false);
    manager.set_session_share_destination(SessionShareDestination::Radius);
    let reread = SettingsManager::load(&dir, &dir.join(".pi"), &dir, false);
    assert_eq!(
        reread.session_share_destination(),
        SessionShareDestination::Radius
    );
    assert_eq!(
        reread.global_raw["sessionShareDestination"],
        serde_json::json!("radius")
    );
    // The project scope is not involved.
    assert!(reread.project_raw.get("sessionShareDestination").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// The settings-menu row (upstream `settings-selector.ts:515-522`)
// ---------------------------------------------------------------------------

/// The row's stored value selects the matching display index, and a cycled
/// row writes the `sessionShareDestination` key the accessor reads back.
#[test]
fn menu_row_round_trips_through_settings_json() {
    for (wire, index) in [("auto", 0), ("gist", 1), ("radius", 2)] {
        let settings = Settings {
            session_share_destination: serde_json::from_value(serde_json::json!(wire)).ok(),
            ..Settings::default()
        };
        let items = settings_items(&settings, &SettingsMenuContext::default());
        let row = items
            .iter()
            .find(|item| item.id == "share-destination")
            .expect("the D31 share-destination row is missing");
        assert_eq!(row.current, index, "stored {wire}");
    }

    // Cycled to `radius` through the menu's change dispatch, the write lands
    // on the global scope and the typed accessor reads it back.
    let dir = temp_dir("menu");
    let mut manager = SettingsManager::load(&dir, &dir.join(".pi"), &dir, false);
    let items = settings_items(&Settings::default(), &SettingsMenuContext::default());
    let mut row = items
        .iter()
        .find(|item| item.id == "share-destination")
        .expect("the D31 share-destination row is missing")
        .clone();
    row.current = 2;
    let action = item_action(&row);
    apply_settings_action(&mut manager, &action).expect("the row writes a settings path");
    let reread = SettingsManager::load(&dir, &dir.join(".pi"), &dir, false);
    assert_eq!(
        reread.session_share_destination(),
        SessionShareDestination::Radius
    );
    assert_eq!(
        reread.settings.session_share_destination,
        Some(SessionShareDestination::Radius)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// /share routing (upstream `shareSession`, session-share.ts:69-76)
// ---------------------------------------------------------------------------

#[test]
fn share_route_follows_the_destination_setting() {
    use SessionShareDestination::{Auto, Gist, Radius};
    // (destination, radius credential available) → route
    for (destination, radius_available, expected) in [
        // `gist` never consults Radius, even when it is authenticated.
        (Gist, true, ShareRoute::Gist),
        (Gist, false, ShareRoute::Gist),
        // `radius` uses Radius when authenticated and otherwise reports the
        // missing credential — it never falls back to a gist.
        (Radius, true, ShareRoute::Radius),
        (Radius, false, ShareRoute::RadiusUnconfigured),
        // `auto` resolves to Radius when authenticated, gist otherwise.
        (Auto, true, ShareRoute::Radius),
        (Auto, false, ShareRoute::Gist),
    ] {
        assert_eq!(
            share_route(destination, radius_available),
            expected,
            "share_route({destination:?}, radius_available: {radius_available})"
        );
    }
}

#[test]
fn forced_radius_reports_the_upstream_missing_credential_error() {
    // Upstream's message, verbatim (`session-share.ts:75`).
    assert_eq!(
        RADIUS_AUTH_MISSING,
        "Radius authentication is not configured. Run '/login radius' first."
    );
    assert_eq!(
        share_route(SessionShareDestination::Radius, false),
        ShareRoute::RadiusUnconfigured
    );
}

// ---------------------------------------------------------------------------
// The Radius upload request (upstream `tryShareViaRadius`)
// ---------------------------------------------------------------------------

#[test]
fn radius_artifact_request_matches_the_upstream_upload() {
    let request = radius_artifact_request(
        "{\"type\":\"session\"}\n",
        "radius-token-1",
        "https://radius.pi.dev",
    );
    // `new URL("/v1/artifacts", DEFAULT_RADIUS_GATEWAY)` +
    // `searchParams.set("visibility", "organization")` +
    // `searchParams.set("title", "Pi session")`: URLSearchParams serializes the
    // space as `+`.
    assert_eq!(
        request.url,
        "https://radius.pi.dev/v1/artifacts?visibility=organization&title=Pi+session"
    );
    assert_eq!(request.token, "radius-token-1");
    // The body is the exported session JSONL itself (x-ndjson, no wrapper).
    assert_eq!(request.body, "{\"type\":\"session\"}\n");

    // A trailing slash on the configured gateway does not double up, and a
    // self-hosted gateway is honored (pir routes every Radius gateway endpoint
    // through `PI_RADIUS_GATEWAY`; upstream's /share pins the published
    // default here).
    let custom = radius_artifact_request("{}", "tok", "https://radius.example/");
    assert_eq!(
        custom.url,
        "https://radius.example/v1/artifacts?visibility=organization&title=Pi+session"
    );
}

#[test]
fn radius_artifact_responses_parse() {
    let ok = r#"{"artifact":{"canonical_url":"https://radius.pi.dev/a/abc123"}}"#;
    assert_eq!(
        radius_artifact_url_from_response(ok).as_deref(),
        Some("https://radius.pi.dev/a/abc123")
    );
    assert_eq!(radius_artifact_error_from_response(ok), None);

    let failed = r#"{"error":"artifact too large"}"#;
    assert_eq!(radius_artifact_url_from_response(failed), None);
    assert_eq!(
        radius_artifact_error_from_response(failed).as_deref(),
        Some("artifact too large")
    );

    // Malformed / empty payloads yield None instead of panicking.
    for body in ["not json", "{}", r#"{"artifact":{}}"#] {
        assert_eq!(radius_artifact_url_from_response(body), None, "{body}");
        assert_eq!(radius_artifact_error_from_response(body), None, "{body}");
    }
}
