//! Client-side removeSession support tests (independent binary; the g_*.rs
//! and tests/suites/ files belong to another agent's conflict scope):
//!
//! * `Command::Remove` translates to the pir extension `pir.session`
//!   `remove_session` member (session-target attachment fencing).
//! * A successful remove clears the client's attachment lease and attach
//!   bookkeeping (the out-of-band `attachment: null` frame itself carries no
//!   `attachmentId` and is ignored by the update queue — the cleanup happens
//!   on the response outcome, like the Detach path).
//! * After removal the session is invisible to `list` and re-attach fails
//!   with `not_found`.
//! * A `closed` service-provider update naming the attached session
//!   invalidates the attach state on a subscribed connection without any
//!   connection-state transition (a removed session is not a transport
//!   failure).

use pir::client::{Client, ClientError, ConnectionState};
use pir::protocol::{Command, SESSION_SERVICE_ID};
use pir::server::Server;

/// Bind a test server and serve it in the background (the run loop accepts
/// unlimited connections, so several clients can share one server).
async fn spawn_server() -> std::net::SocketAddr {
    let server = Server::bind("127.0.0.1:0")
        .await
        .expect("bind remove-session test server");
    let address = server.local_addr().expect("local address");
    tokio::spawn(async move { server.run().await });
    address
}

fn server_error_code(error: ClientError) -> String {
    match error {
        ClientError::Server { code, .. } => code,
        other => panic!("expected a server error code, got: {other}"),
    }
}

async fn create_session(client: &mut Client, name: &str) -> String {
    let created = client
        .request(Command::Create {
            cwd: None,
            name: Some(name.to_owned()),
            model: None,
            thinking_level: None,
        })
        .await
        .expect("create response");
    created["session"]["id"]
        .as_str()
        .expect("session id")
        .to_owned()
}

#[tokio::test]
async fn remove_command_removes_the_session_and_clears_attachment_state() {
    let address = spawn_server().await;
    let mut client = Client::connect(address).await.expect("hello handshake");

    let session_id = create_session(&mut client, "remove-target").await;
    client
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect("attach response");
    assert!(
        client.attachment_id().is_some(),
        "attach must lease an attachment route"
    );
    assert_eq!(client.attached_session(), Some(session_id.as_str()));

    let removed = client
        .request(Command::Remove {
            session_id: session_id.clone(),
        })
        .await
        .expect("remove response");
    assert_eq!(removed["command"], "remove_session");
    assert_eq!(removed["sessionId"], session_id.as_str());

    // The attach bookkeeping is cleared on success: the connection holds no
    // route and reconnect will not try to restore the removed session.
    assert_eq!(client.attached_session(), None);
    assert_eq!(client.attachment_id(), None);

    // list no longer shows the session.
    let listed = client
        .list()
        .await
        .expect("list after removal")
        .into_iter()
        .any(|meta| meta["id"] == session_id);
    assert!(!listed, "removed session must not be listed");

    // Re-attach fails: the session (and its recovery source) is gone.
    let error = client
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect_err("attach of a removed session");
    assert_eq!(server_error_code(error), "not_found");

    // Removing again answers not_found too.
    let error = client
        .request(Command::Remove { session_id })
        .await
        .expect_err("remove of a removed session");
    assert_eq!(server_error_code(error), "not_found");
}

#[tokio::test]
async fn closed_update_invalidates_another_attached_subscriber() {
    let address = spawn_server().await;

    // Client A creates the session and holds an attachment.
    let mut attached = Client::connect(address).await.expect("connect A");
    let session_id = create_session(&mut attached, "closed-target").await;
    attached
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect("attach A");

    // Client B subscribes and attaches to the same session.
    let mut subscriber = Client::connect(address).await.expect("connect B");
    let subscription = subscriber
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("subscribe B");
    subscriber
        .request(Command::Attach {
            session_id: session_id.clone(),
        })
        .await
        .expect("attach B");
    assert_eq!(subscriber.attached_session(), Some(session_id.as_str()));

    // A removes the session; B's subscription receives the `closed`
    // invalidation (queued updates from the attach path may come first).
    attached
        .request(Command::Remove {
            session_id: session_id.clone(),
        })
        .await
        .expect("remove response");
    let mut closed = None;
    for _ in 0..16 {
        let update = subscriber.next_update(&subscription).await.expect("update");
        if update["type"] == "closed" {
            closed = Some(update);
            break;
        }
    }
    let update = closed.expect("closed invalidation update must arrive");
    assert_eq!(update["instance"]["key"], session_id.as_str());

    // The closed update invalidates B's attach state without touching the
    // transport: no connection-state transition, only the dead route's
    // bookkeeping is dropped (observable through attached_session()).
    assert_eq!(subscriber.attached_session(), None);
    assert_eq!(subscriber.attachment_id(), None);
    assert_eq!(
        subscriber.connection_state(),
        ConnectionState::Connected,
        "a removed session is not a transport failure"
    );

    // The invalidated client can no longer reach the dead route.
    let error = subscriber
        .request(Command::Abort {
            session_id: session_id.clone(),
        })
        .await
        .expect_err("abort of a removed session");
    assert_eq!(server_error_code(error), "not_found");
}
