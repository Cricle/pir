use pir_phone_server::protocol::SessionDescriptor;
use pir_phone_server::store::Store;

#[test]
fn tenant_isolation_and_session_pagination() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("pir-phone-store-{}", uuid::Uuid::new_v4()));
    let store = Store::open(&dir)?;
    let alice = store.create_user("alice", None)?;
    let bob = store.create_user("bob", None)?;
    let alice_device = store.create_device(&alice.id, "alice-device", None)?;
    let bob_device = store.create_device(&bob.id, "bob-device", None)?;

    for index in 0..3 {
        store.upsert_session(
            &alice.id,
            &alice_device.id,
            &SessionDescriptor {
                id: format!("session-{index}"),
                label: Some(format!("Alice session {index}")),
                kind: Some("parent".into()),
                cwd: Some("/alice".into()),
                model: None,
                thinking_level: None,
                is_streaming: None,
                is_compacting: None,
                message_count: Some(index),
                last_error: None,
            },
            true,
        )?;
    }
    store.upsert_session(
        &bob.id,
        &bob_device.id,
        &SessionDescriptor {
            id: "session-0".into(),
            label: Some("Bob session".into()),
            kind: Some("parent".into()),
            cwd: Some("/bob".into()),
            model: None,
            thinking_level: None,
            is_streaming: None,
            is_compacting: None,
            message_count: None,
            last_error: None,
        },
        false,
    )?;

    let alice_sessions = store.list_sessions(&alice.id, 0, 50, None, None)?;
    assert_eq!(alice_sessions.len(), 3);
    let bob_sessions = store.list_sessions(&bob.id, 0, 50, None, None)?;
    assert_eq!(bob_sessions.len(), 1);
    assert_eq!(bob_sessions[0].label.as_deref(), Some("Bob session"));

    let alice_session_id = &alice_sessions[0].id;
    assert!(store.get_session(&bob.id, alice_session_id)?.is_none());
    assert!(store.get_session(&alice.id, alice_session_id)?.is_some());

    // Pagination is deterministic and honors the requested limit.
    let first_page = store.list_sessions(&alice.id, 0, 2, None, None)?;
    assert_eq!(first_page.len(), 2);
    let second_page = store.list_sessions(&alice.id, 2, 2, None, None)?;
    assert_eq!(second_page.len(), 1);

    let _ = std::fs::remove_dir_all(dir);
    Ok(())
}
