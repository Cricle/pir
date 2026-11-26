//! The remote-environment override slot closes what it displaces: replacing
//! or clearing the installed connection must not leak the old one.
//!
//! Moved out of `src/env_client/selection.rs` — unit tests do not live in `src/`.

use std::sync::Arc;

use pir_core::env_client::connection::Connection;
use pir_core::env_client::remote_env::{RemoteExecutionEnv, RemoteExecutionEnvOptions};
use pir_core::env_client::selection::{clear_override, install_override};

/// A connection that was never started: replacing the slot must still mark
/// it closed, which is the observable half of "close, don't leak" (a
/// started one would also drop its daemon/SSH session through the same
/// `close()`).
fn inert_environment(id: &str) -> Arc<RemoteExecutionEnv> {
    let options = RemoteExecutionEnvOptions::new(
        id.to_string(),
        Connection::argv(vec!["definitely-not-a-pir-image".to_string()]),
    );
    Arc::new(RemoteExecutionEnv::new(options))
}

#[test]
fn replacing_or_clearing_the_override_closes_the_old_connection() {
    clear_override();
    let first = inert_environment("first");
    let first_connection = first.connection.clone();
    install_override(first.clone());
    assert!(
        !first_connection.is_closed(),
        "the installed connection is open"
    );

    let second = inert_environment("second");
    let second_connection = second.connection.clone();
    install_override(second.clone());
    assert!(
        first_connection.is_closed(),
        "replacing the override closes the displaced connection"
    );
    assert!(
        !second_connection.is_closed(),
        "the new connection stays open"
    );

    // Re-installing the same Arc must not close the environment that is
    // still installed.
    install_override(second.clone());
    assert!(!second_connection.is_closed());

    clear_override();
    assert!(
        second_connection.is_closed(),
        "clearing the override closes the removed connection"
    );
}
