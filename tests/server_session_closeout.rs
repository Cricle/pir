//! Server-side session closeout tests (independent binary; the g_*.rs and
//! tests/suites/ files belong to another agent's conflict scope):
//!
//! * `remove_session` — the pir extension member backing upstream
//!   `SessionRouter.removeSession`: after removal the session is invisible to
//!   `list`, attach fails with `not_found`, the durable JSONL file is gone,
//!   and every subscriber receives the `closed` invalidation service update.
//!   Fencing: a connection without an attachment is rejected with
//!   `session_locked`.
//! * P2-k — reopening a persisted session restores model/thinking state along
//!   the PROJECTION path (leaf→root parent chain), never from entries parked
//!   on abandoned branches.

use std::path::{Path, PathBuf};

use pir::client::{Client, ClientError};
use pir::protocol::{Command, RpcTarget, SESSION_SERVICE_ID};
use pir::server::{Server, ServerState};

/// A unique temp directory per call (tests run in one process; the prefix
/// keeps parallel runs from colliding).
fn temp_session_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pir-closeout-{}-{}-{tag}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("create temp session dir");
    dir
}

/// Write a valid v3 session JSONL file (header line first) holding `lines`.
fn write_session_file(dir: &Path, id: &str, lines: &[String]) -> PathBuf {
    let path = dir.join(format!("{id}.jsonl"));
    let cwd = dir.to_string_lossy().replace('\\', "/");
    let mut content = format!(
        "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\
         \"timestamp\":\"2026-01-01T00:00:00.000Z\",\"cwd\":\"{cwd}\"}}\n"
    );
    for line in lines {
        content.push_str(line);
        content.push('\n');
    }
    std::fs::write(&path, content).expect("write session file");
    path
}

fn user_message(id: &str, parent_id: &str, text: &str) -> String {
    format!(
        "{{\"type\":\"message\",\"id\":\"{id}\",\"parentId\":\"{parent_id}\",\
         \"timestamp\":\"2026-01-01T00:00:01.000Z\",\
         \"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\
         \"text\":\"{text}\"}}],\"timestamp\":1000}}}}"
    )
}

fn model_change(id: &str, parent_id: &str, provider: &str, model: &str) -> String {
    format!(
        "{{\"type\":\"model_change\",\"id\":\"{id}\",\"parentId\":\"{parent_id}\",\
         \"timestamp\":\"2026-01-01T00:00:02.000Z\",\
         \"provider\":\"{provider}\",\"modelId\":\"{model}\"}}"
    )
}

/// Root-level entry: `parentId` is a JSON null.
fn model_change_root(id: &str, provider: &str, model: &str) -> String {
    format!(
        "{{\"type\":\"model_change\",\"id\":\"{id}\",\"parentId\":null,\
         \"timestamp\":\"2026-01-01T00:00:02.000Z\",\
         \"provider\":\"{provider}\",\"modelId\":\"{model}\"}}"
    )
}

fn thinking_change(id: &str, parent_id: &str, level: &str) -> String {
    format!(
        "{{\"type\":\"thinking_level_change\",\"id\":\"{id}\",\"parentId\":\"{parent_id}\",\
         \"timestamp\":\"2026-01-01T00:00:03.000Z\",\"thinkingLevel\":\"{level}\"}}"
    )
}

/// Bind a state with the session dir and serve it in the background.
async fn spawn_server(dir: PathBuf) -> std::net::SocketAddr {
    let state = ServerState::default().with_session_dir(dir);
    let server = Server::bind_with_state("127.0.0.1:0", state)
        .await
        .expect("bind closeout test server");
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

#[tokio::test]
async fn remove_session_hides_lists_blocks_attach_deletes_file_and_broadcasts_closed() {
    let dir = temp_session_dir("remove");
    let session_id = "closeout-remove-target";
    let path = write_session_file(&dir, session_id, &[user_message("m1", "ma", "hello")]);
    assert!(path.exists());

    let address = spawn_server(dir.clone()).await;
    let mut client = Client::connect(address).await.expect("connect");
    let server_id = client.server_id().to_owned();
    let mut subscription = client
        .subscribe(SESSION_SERVICE_ID)
        .await
        .expect("subscribe");

    // The attach reopens the on-disk session (recovery source) and the
    // session becomes visible to list.
    client
        .request(Command::Attach {
            session_id: session_id.to_owned(),
        })
        .await
        .expect("attach persisted session");
    let listed = client
        .list()
        .await
        .expect("list")
        .into_iter()
        .any(|meta| meta["id"] == session_id);
    assert!(listed, "session must be listed before removal");

    // Remove through the pir extension member (attachment-fenced call).
    let attachment_id = client
        .attachment_id()
        .expect("attach recorded the attachment id")
        .to_owned();
    let result = client
        .call(
            RpcTarget::session(&server_id, session_id, &attachment_id),
            SESSION_SERVICE_ID,
            "remove_session",
            Vec::new(),
        )
        .await
        .expect("remove_session succeeds");
    assert_eq!(result["command"], "remove_session");
    assert_eq!(result["sessionId"], session_id);

    // list no longer shows the session.
    let listed = client
        .list()
        .await
        .expect("list after removal")
        .into_iter()
        .any(|meta| meta["id"] == session_id);
    assert!(!listed, "removed session must not be listed");

    // Attach fails: the recovery source is gone.
    let error = client
        .request(Command::Attach {
            session_id: session_id.to_owned(),
        })
        .await
        .expect_err("attach of a removed session");
    assert_eq!(server_error_code(error), "not_found");

    // The durable JSONL file is deleted.
    assert!(!path.exists(), "removed session file must be deleted");

    // Every subscriber receives the `closed` invalidation update (queued
    // service updates from the attach/ack path come first).
    let mut closed = None;
    for _ in 0..16 {
        let update = client.next_update(&subscription).await.expect("update");
        if update["type"] == "closed" {
            closed = Some(update);
            break;
        }
    }
    let update = closed.expect("closed invalidation update must arrive");
    assert_eq!(update["instance"]["key"], session_id);
    let _ = &mut subscription; // keep the subscription alive to the end

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn remove_session_is_attachment_fenced_and_missing_sessions_are_not_found() {
    let dir = temp_session_dir("fence");
    let session_id = "closeout-fence-target";
    let path = write_session_file(&dir, session_id, &[user_message("m1", "ma", "hello")]);

    let address = spawn_server(dir.clone()).await;

    // Client A attaches and holds the only attachment lease.
    let mut attached = Client::connect(address).await.expect("connect A");
    attached
        .request(Command::Attach {
            session_id: session_id.to_owned(),
        })
        .await
        .expect("attach A");

    // Client B (own connection, never attached) may not remove it.
    let mut outsider = Client::connect(address).await.expect("connect B");
    let server_id = outsider.server_id().to_owned();
    let error = outsider
        .call(
            RpcTarget::session(&server_id, session_id, outsider.connection_id()),
            SESSION_SERVICE_ID,
            "remove_session",
            Vec::new(),
        )
        .await
        .expect_err("unattached remove_session must be fenced");
    assert_eq!(server_error_code(error), "session_locked");

    // An unknown session answers not_found.
    let error = outsider
        .call(
            RpcTarget::session(&server_id, "missing-session", outsider.connection_id()),
            SESSION_SERVICE_ID,
            "remove_session",
            Vec::new(),
        )
        .await
        .expect_err("remove of an unknown session");
    assert_eq!(server_error_code(error), "not_found");

    // The fenced session survives: file on disk, still listed by A.
    assert!(path.exists(), "a fenced removal must not delete the file");
    let listed = attached
        .list()
        .await
        .expect("list after fenced removal")
        .into_iter()
        .any(|meta| meta["id"] == session_id);
    assert!(listed, "a fenced removal must not evict the session");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn reopen_restores_model_and_thinking_along_the_projection_path() {
    let dir = temp_session_dir("projection");
    let session_id = "closeout-projection-target";
    // Tree (file order = append order):
    //   ma(root) <- m1 <- mb <- m2   active path, leaf = m2
    //   ma <- mc <- th               abandoned branch, appended LATER
    // The buggy file-order scan would restore model-c/high; the projection
    // path must restore model-b and the "off" default (no thinking change on
    // the active path).
    let _ = write_session_file(
        &dir,
        session_id,
        &[
            model_change_root("ma", "prov-a", "model-a"),
            user_message("m1", "ma", "first"),
            model_change("mb", "m1", "prov-b", "model-b"),
            model_change("mc", "ma", "prov-c", "model-c"),
            thinking_change("th", "mc", "high"),
            user_message("m2", "mb", "second"),
        ],
    );
    let address = spawn_server(dir).await;
    let mut client = Client::connect(address).await.expect("connect");
    let result = client
        .request(Command::Attach {
            session_id: session_id.to_owned(),
        })
        .await
        .expect("attach persisted session");
    let session = &result["session"];
    assert_eq!(session["model"]["provider"], "prov-b");
    assert_eq!(session["model"]["id"], "model-b");
    assert_eq!(session["thinkingLevel"], "off");
}
