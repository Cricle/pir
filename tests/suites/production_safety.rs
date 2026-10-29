//! Production-safety invariants for the public binary and remote server.

use std::process::Command;

use pir::config::Config;
use pir::server::ServerSecurityConfig;

#[test]
fn default_remote_policy_is_conservative() {
    let config = Config::default();
    assert!(!config.allow_remote_bash);
    assert!(config.allow_remote_agent);
    assert_eq!(config.server_address, "127.0.0.1:4321");
    assert!(config.server_max_request_body_bytes > 0);
    assert!(config.server_max_connection_body_bytes >= config.server_max_request_body_bytes);

    let security = ServerSecurityConfig::default();
    assert!(!security.allow_remote_bash);
    assert!(security.allow_remote_agent);
    assert!(security.max_connections > 0);
    assert!(security.max_request_body_bytes > 0);
    assert!(security.max_connection_body_bytes >= security.max_request_body_bytes);
}

#[test]
fn config_command_redacts_remote_auth_token() {
    // `pir config` now prints the package/resource listing (upstream
    // `handleConfigCommand`); the auth-token redaction guarantee is that the
    // secret value never reaches stdout under any output mode.
    let output = Command::new(env!("CARGO_BIN_EXE_pir"))
        .arg("config")
        .env(
            "PIR_SERVER_AUTH_TOKEN",
            "production-secret-that-must-not-leak",
        )
        .output()
        .expect("pir config should start");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stdout.contains("production-secret-that-must-not-leak"));
    assert!(!stderr.contains("production-secret-that-must-not-leak"));
}

#[test]
fn invalid_server_limits_are_rejected_before_bind() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let error = pir::server::Server::bind_with_security(
            "127.0.0.1:0",
            ServerSecurityConfig {
                max_connections: 0,
                ..ServerSecurityConfig::default()
            },
        )
        .await;
        let error = match error {
            Ok(_) => panic!("zero connection limit must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    });
}
