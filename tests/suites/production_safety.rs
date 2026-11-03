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

/// Security regression (R2-5-N02): a non-loopback listener without an auth
/// token would let any host on the network drive the server unauthenticated;
/// binding such an address must fail with `PermissionDenied`. Supplying a
/// token still allows the bind, and loopback keeps working without one.
#[test]
fn non_loopback_bind_requires_auth_token() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let error =
            pir::server::Server::bind_with_security("0.0.0.0:0", ServerSecurityConfig::default())
                .await;
        let error = match error {
            Ok(_) => panic!("a non-loopback bind without a token must be refused"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);

        let server = pir::server::Server::bind_with_security(
            "0.0.0.0:0",
            ServerSecurityConfig {
                auth_token: Some("test-token".to_owned()),
                ..ServerSecurityConfig::default()
            },
        )
        .await
        .expect("a non-loopback bind with a token must be accepted");
        assert!(
            !server
                .local_addr()
                .expect("local address")
                .ip()
                .is_loopback(),
            "the test must exercise a non-loopback listener"
        );
        drop(server);

        let server = pir::server::Server::bind("127.0.0.1:0")
            .await
            .expect("a loopback bind needs no token");
        drop(server);
    });
}

/// Security regression (R2-5-N05): `Config` and `ServerSecurityConfig` must
/// not print the server auth token through `Debug`, and serializing `Config`
/// must omit it. Deserialization must still accept the token (env/JSON input
/// path), otherwise the redaction would silently disable authentication.
#[test]
fn config_and_security_debug_and_serialization_redact_server_auth_token() {
    let secret = "sk-secret-that-must-not-leak";
    let config = Config {
        server_auth_token: Some(secret.to_owned()),
        ..Config::default()
    };
    let debug = format!("{config:?}");
    assert!(
        debug.contains("server_auth_token"),
        "Config Debug must still name the field: {debug}"
    );
    assert!(
        !debug.contains(secret),
        "Config Debug leaked the server token: {debug}"
    );

    let json = serde_json::to_value(&config).expect("serialize Config");
    assert!(
        json.get("server_auth_token").is_none(),
        "serialized Config must not carry the token: {json}"
    );
    assert!(
        json.get("max_turns").is_some(),
        "the other fields must still serialize: {json}"
    );

    let security = ServerSecurityConfig {
        auth_token: Some(secret.to_owned()),
        ..ServerSecurityConfig::default()
    };
    let debug = format!("{security:?}");
    assert!(
        debug.contains("auth_token"),
        "ServerSecurityConfig Debug must still name the field: {debug}"
    );
    assert!(
        !debug.contains(secret),
        "ServerSecurityConfig Debug leaked the token: {debug}"
    );

    // `skip_serializing` must not affect deserialization.
    let parsed: Config = serde_json::from_value(serde_json::json!({
        "server_auth_token": secret,
    }))
    .expect("Config must still deserialize the token");
    assert_eq!(parsed.server_auth_token.as_deref(), Some(secret));
}

/// Security regression (R2-5-N05): `Server::security()` is public, so it must
/// return a redacted view — never the configured token. `auth_required()`
/// carries the only bit hosts need from the secret's presence.
#[test]
fn server_security_accessor_never_exposes_the_token() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let secret = "sk-accessor-secret-that-must-not-leak";
        let server = pir::server::Server::bind_with_security(
            "127.0.0.1:0",
            ServerSecurityConfig {
                auth_token: Some(secret.to_owned()),
                ..ServerSecurityConfig::default()
            },
        )
        .await
        .expect("bind loopback server with a token");
        assert!(server.auth_required(), "the token must still gate auth");
        let view = server.security();
        assert!(
            view.auth_token.is_none(),
            "the public accessor must not carry the token"
        );
        assert!(
            !format!("{view:?}").contains(secret),
            "the redacted view must not print the token"
        );
        assert_eq!(
            view.max_sessions,
            ServerSecurityConfig::default().max_sessions,
            "the non-secret limits must survive the redaction"
        );
    });
}
