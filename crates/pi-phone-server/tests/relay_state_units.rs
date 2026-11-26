//! Unit suites for the relay's auth/store/registry layer, moved out of
//! `src/{auth,store,registry}.rs`.
//!
//! Source hygiene forbids inline `#[cfg(test)]` modules in `src/`. Everything
//! here drives the same public API the relay uses; nothing reaches into a
//! private field, so the suites also double as a contract check on what an
//! embedder can do.

use pir_phone_server::auth::{authenticate_token, TicketStore};
use pir_phone_server::protocol::SessionDescriptor;
use pir_phone_server::registry::{PendingReply, Registry};
use pir_phone_server::store::Store;

fn descriptor(id: &str) -> SessionDescriptor {
    SessionDescriptor {
        id: id.to_owned(),
        session_id: None,
        has_pending_ui_request: None,
        last_activity_at: None,
        label: None,
        kind: None,
        cwd: None,
        model: None,
        thinking_level: None,
        is_streaming: None,
        is_compacting: None,
        message_count: None,
        last_error: None,
    }
}

#[test]
fn tickets_are_bounded_per_user_and_one_use() {
    let store = TicketStore::default();
    let first = store.issue("user-a", "token-a").expect("first ticket");
    assert!(store.issue("user-a", "token-a").is_some());
    assert!(store.issue("user-a", "token-a").is_some());
    assert!(store.issue("user-a", "token-a").is_some());
    assert!(store.issue("user-a", "token-a").is_none());

    let identity = store.redeem(&first).expect("redeem ticket");
    assert_eq!(identity.user_id, "user-a");
    assert_eq!(identity.token_id, "token-a");
    assert!(store.redeem(&first).is_none());
    // Redeeming frees one slot, so the next issue succeeds again.
    assert!(store.issue("user-a", "token-a").is_some());
}

#[test]
fn list_tokens_reads_key_suffixes_and_device_revoke_kills_device_tokens() {
    let dir = std::env::temp_dir().join(format!("pir-phone-store-test-{}", uuid::Uuid::new_v4()));
    let store = Store::open(&dir).expect("open store");
    let user = store.create_user("token-user", None).expect("user");
    let (user_token, user_row) = store
        .create_token(&user.id, "user", Some("user token"), None)
        .expect("user token");
    let device = store
        .create_device(&user.id, "device", None)
        .expect("device");
    let (device_token, device_row) = store
        .create_token(&user.id, "device", Some("device token"), Some(&device.id))
        .expect("device token");

    let rows = store.list_tokens(&user.id).expect("list tokens");
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row.id == user_row.id));
    assert!(rows.iter().any(|row| row.id == device_row.id));

    assert!(store
        .revoke_device(&user.id, &device.id)
        .expect("revoke device"));
    let rows = store
        .list_tokens(&user.id)
        .expect("list tokens after revoke");
    assert!(rows
        .iter()
        .find(|row| row.id == device_row.id)
        .is_some_and(|row| row.revoked_at.is_some()));
    assert!(rows
        .iter()
        .find(|row| row.id == user_row.id)
        .is_some_and(|row| row.revoked_at.is_none()));
    assert!(!store
        .revoke_device(&user.id, &device.id)
        .expect("repeat revoke"));
    assert!(authenticate_token(&store, &device_token, "device").is_err());
    assert!(authenticate_token(&store, &user_token, "user").is_ok());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pending_reply_is_scoped_to_owner() {
    let mut registry = Registry::new();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    registry.add_pending(
        "f-1".to_owned(),
        PendingReply::Http { tx },
        "user-a",
        "device-a",
        "device-a:session",
    );
    assert!(registry
        .take_pending_for("f-1", "user-b", "device-a", "device-a:session")
        .is_none());
    assert!(registry
        .take_pending_for("f-1", "user-a", "device-b", "device-a:session")
        .is_none());
    assert!(registry
        .take_pending_for("f-1", "user-a", "device-a", "device-b:session")
        .is_none());
    assert!(registry
        .take_pending_for("f-1", "user-a", "device-a", "device-a:session")
        .is_some());
}

#[test]
fn upsert_session_merges_live_pending_ui_and_activity() {
    let mut registry = Registry::new();
    assert!(
        registry
            .upsert_session("user-a", "device-a", &descriptor("active-session-1"), true)
            .1
    );

    // A catalog push for a pool child may differ only in the live fields
    // the bridge tracks; it must still mark the row changed so the relay
    // persists and broadcasts them.
    let mut live = descriptor("active-session-1");
    live.session_id = Some("native-child".to_owned());
    live.has_pending_ui_request = Some(true);
    live.last_activity_at = Some(1750);
    assert!(registry.upsert_session("user-a", "device-a", &live, true).1);
    let runtime = registry
        .sessions
        .get("device-a:active-session-1")
        .expect("session row");
    assert_eq!(
        runtime.descriptor.has_pending_ui_request,
        Some(true),
        "pending flag must survive the merge"
    );
    assert_eq!(runtime.descriptor.last_activity_at, Some(1750));
    assert_eq!(
        runtime.descriptor.session_id.as_deref(),
        Some("native-child")
    );

    // Answering the dialog clears the flag through the same path.
    let mut cleared = descriptor("active-session-1");
    cleared.has_pending_ui_request = Some(false);
    assert!(
        registry
            .upsert_session("user-a", "device-a", &cleared, true)
            .1
    );
    let runtime = registry
        .sessions
        .get("device-a:active-session-1")
        .expect("session row");
    assert_eq!(runtime.descriptor.has_pending_ui_request, Some(false));
    assert_eq!(runtime.descriptor.last_activity_at, Some(1750));
}

#[test]
fn forward_ids_are_not_predictable_sequences() {
    let mut registry = Registry::new();
    let first = registry.next_forward_id();
    let second = registry.next_forward_id();
    assert_ne!(first, second);
    assert!(!second.ends_with("-1"));
    assert!(first.len() > 20);
}

/// Only offline, unsubscribed sessions are evictable; the online row and the
/// live counter must both survive the cap.
#[test]
fn high_water_evicts_only_offline_unsubscribed_sessions() {
    let mut registry = Registry::new();
    // Seed through the public upsert path so the online counter is maintained
    // by the code under test rather than assigned in the fixture.
    for index in 0..4 {
        let online = index == 3;
        registry.upsert_session(
            "user",
            "device",
            &descriptor(&format!("session-{index}")),
            online,
        );
    }
    assert_eq!(registry.sessions.len(), 4);
    assert_eq!(registry.online_sessions(), 1);

    registry.set_max_resident_sessions(2);
    assert_eq!(registry.sessions.len(), 2);
    assert!(registry.sessions.contains_key("device:session-3"));
    assert_eq!(registry.online_sessions(), 1);
}

/// The eviction filter is "offline AND no subscribers". A session that was
/// online and subscribed, then went offline, must survive the cap — that is
/// the one path to the filtered state, because `subscribe` itself refuses an
/// offline session.
#[test]
fn subscribed_offline_sessions_are_not_evicted() {
    let mut registry = Registry::new();
    // The protected row: online first (so it can be subscribed), then offline.
    registry.upsert_session("user", "device", &descriptor("pinned"), true);
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    let client = registry.alloc_client("user", tx);
    assert!(registry.subscribe(client, "device:pinned").is_some());
    assert!(
        registry
            .upsert_session("user", "device", &descriptor("pinned"), false)
            .1
    );
    // Plain offline rows: nothing protects these.
    for index in 0..3 {
        registry.upsert_session("user", "device", &descriptor(&format!("s-{index}")), false);
    }
    assert_eq!(registry.sessions.len(), 4);

    registry.set_max_resident_sessions(1);
    assert!(
        registry.sessions.contains_key("device:pinned"),
        "a subscribed session must never be evicted"
    );
    // Every unprotected row is gone; the cap is reached by evicting only them.
    assert_eq!(registry.sessions.len(), 1);
    assert_eq!(registry.online_sessions(), 0);
}
