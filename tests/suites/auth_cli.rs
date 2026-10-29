//! `pir auth` CLI surface tests: clap parsing of the subcommands and flags,
//! mirroring the upstream `auth-command.ts` grammar (`--provider`, `--json`,
//! `--credentials`, `--no-refresh`).

use clap::Parser;
use pir::coding_agent::cli::{AuthCommand, Cli, Command};

fn parse(args: &[&str]) -> Command {
    Cli::try_parse_from(std::iter::once("pir").chain(args.iter().copied()))
        .unwrap_or_else(|error| panic!("parse failed for {args:?}: {error}"))
        .command
        .expect("subcommand expected")
}

#[test]
fn bare_auth_defaults_to_status() {
    match parse(&["auth"]) {
        Command::Auth { command } => {
            assert!(matches!(command, None | Some(AuthCommand::Status)));
        }
        other => panic!("expected auth command, got {other:?}"),
    }
}

#[test]
fn auth_status_subcommand_parses() {
    assert!(matches!(
        parse(&["auth", "status"]),
        Command::Auth {
            command: Some(AuthCommand::Status)
        }
    ));
}

#[test]
fn auth_login_takes_provider_and_agent_name() {
    match parse(&["auth", "login", "anthropic"]) {
        Command::Auth {
            command:
                Some(AuthCommand::Login {
                    provider,
                    agent_name,
                }),
        } => {
            assert_eq!(provider, "anthropic");
            assert_eq!(agent_name, None);
        }
        other => panic!("unexpected: {other:?}"),
    }
    match parse(&["auth", "login", "openai", "--agent-name", "Pi"]) {
        Command::Auth {
            command:
                Some(AuthCommand::Login {
                    provider,
                    agent_name,
                }),
        } => {
            assert_eq!(provider, "openai");
            assert_eq!(agent_name.as_deref(), Some("Pi"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn auth_logout_takes_provider() {
    match parse(&["auth", "logout", "openrouter"]) {
        Command::Auth {
            command: Some(AuthCommand::Logout { provider }),
        } => assert_eq!(provider, "openrouter"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn auth_check_mirrors_upstream_flags() {
    match parse(&["auth", "check", "--provider", "anthropic", "--json"]) {
        Command::Auth {
            command:
                Some(AuthCommand::Check {
                    provider,
                    model,
                    json,
                    credentials,
                    no_refresh,
                }),
        } => {
            assert_eq!(provider.as_deref(), Some("anthropic"));
            assert_eq!(model, None);
            assert!(json);
            assert!(!credentials);
            assert!(!no_refresh);
        }
        other => panic!("unexpected: {other:?}"),
    }
    match parse(&[
        "auth",
        "check",
        "--provider",
        "openai",
        "--credentials",
        "--no-refresh",
    ]) {
        Command::Auth {
            command:
                Some(AuthCommand::Check {
                    credentials,
                    no_refresh,
                    ..
                }),
        } => {
            assert!(credentials);
            assert!(no_refresh);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn auth_print_api_key_mirrors_upstream_flag() {
    match parse(&["auth", "print-api-key", "--provider", "github-copilot"]) {
        Command::Auth {
            command: Some(AuthCommand::PrintApiKey { provider, model }),
        } => {
            assert_eq!(provider.as_deref(), Some("github-copilot"));
            assert_eq!(model, None);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn auth_check_requires_provider_or_model() {
    // Upstream `auth check` accepts `--provider` OR `--model`
    // (cli/auth-command.ts:108-112); clap no longer requires `--provider`,
    // the runtime resolver enforces the pair.
    let parsed = Cli::try_parse_from(["pir", "auth", "check"])
        .expect("auth check without flags parses; the runtime rejects it");
    assert!(matches!(
        parsed.command,
        Some(Command::Auth {
            command: Some(AuthCommand::Check { .. })
        })
    ));
    let error = pir::ai::auth::cli::resolve_provider_from_flags(
        None,
        None,
        "Auth checks require --provider <provider> or --model <model>",
    )
    .expect_err("auth check without provider or model must fail");
    assert!(
        error
            .to_string()
            .contains("Auth checks require --provider <provider> or --model <model>"),
        "{error}"
    );
}

#[test]
fn auth_unknown_flag_is_rejected() {
    let error = Cli::try_parse_from(["pir", "auth", "check", "--provider", "x", "--bogus"]).err();
    assert!(error.is_some(), "unknown flags must be rejected");
}

/// `print-bearer-token`'s model pattern resolves its provider: a canonical
/// `provider/model` reference needs no environment lookup (upstream
/// `resolveCredentialForPrint`, cli/credential-print.ts).
#[test]
fn print_bearer_token_resolves_provider_from_model_pattern() {
    assert_eq!(
        pir::ai::auth::cli::provider_for_model_pattern("anthropic/claude-opus")
            .expect("canonical reference"),
        "anthropic"
    );
    // Bare ids and unknown references need the model environment; without a
    // single '/' a bare id is not a canonical provider reference, so it goes
    // through the environment lookup and fails with the upstream-style text
    // when no such model exists anywhere.
    let error = pir::ai::auth::cli::provider_for_model_pattern("no-such-model-xyz").unwrap_err();
    assert!(error.to_string().contains("not found"), "{error}");
}

// ---------------------------------------------------------------------------
// Upstream credential-print.ts error surface (round-2 parity wave)
// ---------------------------------------------------------------------------

/// Acquire the SHARED process-global agent-dir lock for a sync test.
///
/// `PIR_AGENT_DIR` is process-wide, and this binary merges several suites into
/// one link unit whose tests run in parallel. A mutex declared in this module
/// only excludes this module's tests: `ai_codex_ws_w13` pins `PIR_AGENT_DIR`
/// through [`crate::common::AgentDirEnvGuard`] (the shared tokio mutex in
/// `tests/common/mod.rs`), so a suite-local lock here let that suite restore
/// its saved agent dir underneath this one mid-test. The shared guard both
/// serializes the mutation and restores the previous value on drop.
fn env_guard(label: &str) -> (tokio::runtime::Runtime, crate::common::AgentDirEnvGuard) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let guard = runtime.block_on(crate::common::AgentDirEnvGuard::scratch(label, None));
    (runtime, guard)
}

/// An explicit `--provider` naming no known provider errors with the
/// upstream text (credential-print.ts:57-59).
#[test]
fn unknown_provider_errors_with_upstream_text() {
    let error = pir::ai::auth::cli::resolve_provider_from_flags(
        Some("definitely-not-a-provider-xyz"),
        None,
        "unused requirement",
    )
    .expect_err("an unknown provider must fail");
    assert!(
        error.to_string().contains(
            "Unknown provider \"definitely-not-a-provider-xyz\". Use --list-models to see available providers.",
        ),
        "upstream credential-print.ts:57-59 text, got: {error}"
    );

    // A known catalog provider still resolves.
    assert_eq!(
        pir::ai::auth::cli::resolve_provider_from_flags(Some("anthropic"), None, "unused")
            .expect("a known provider resolves"),
        "anthropic"
    );
}

/// `print-api-key` on an OAuth-configured provider errors with the upstream
/// text instead of printing the OAuth-derived key
/// (credential-print.ts:92-94, 110-112).
#[test]
fn print_api_key_rejects_oauth_configured_providers() {
    let (runtime, guard) = env_guard("auth-cli-oauth-print");

    // Store an OAuth credential for a known provider.
    pir::ai::auth::FileAuthStorage::default_storage()
        .store(
            "anthropic",
            pir::ai::auth::StoredCredential::OAuth(pir::ai::auth::OAuthCredential::new(
                "access-token",
                "refresh-token",
                i64::MAX,
            )),
        )
        .expect("store oauth credential");

    let error = runtime
        .block_on(pir::ai::auth::cli::print_api_key(Some("anthropic"), None))
        .expect_err("print-api-key must refuse an OAuth-configured provider");
    assert!(
        error
            .to_string()
            .contains("Provider \"anthropic\" is configured with OAuth, not an API key",),
        "upstream credential-print.ts:110-112 text, got: {error}"
    );
    drop(guard);
}

/// A model pattern that resolves under several configured credentials errors
/// with the upstream ambiguity text (credential-print.ts:84-85).
#[test]
fn model_pattern_matching_multiple_configured_providers_errors() {
    let (_runtime, guard) = env_guard("auth-cli-multi-match");
    let agent_dir = guard.dir().expect("scratch agent dir").to_path_buf();

    // Two custom providers, each defining the same unique model id (the id is
    // unique enough that no built-in catalog model fuzzy-matches it).
    std::fs::write(
        agent_dir.join("models.json"),
        r#"{
            "providers": {
                "zz-prov-b": {
                    "name": "Probe B",
                    "baseUrl": "http://127.0.0.1:9/v1",
                    "api": "openai-chat",
                    "models": [{ "id": "zz-shared-model-xyz", "name": "Shared Model" }]
                },
                "zz-prov-a": {
                    "name": "Probe A",
                    "baseUrl": "http://127.0.0.1:9/v1",
                    "api": "openai-chat",
                    "models": [{ "id": "zz-shared-model-xyz", "name": "Shared Model" }]
                }
            }
        }"#,
    )
    .expect("write models.json");
    let storage = pir::ai::auth::FileAuthStorage::default_storage();
    for provider in ["zz-prov-a", "zz-prov-b"] {
        storage
            .store(
                provider,
                pir::ai::auth::StoredCredential::ApiKey(pir::ai::types::ApiKeyCredential::new(
                    Some("key".to_string()),
                )),
            )
            .expect("store api-key credential");
    }

    let error = pir::ai::auth::cli::resolve_provider_from_flags(
        None,
        Some("zz-shared-model-xyz"),
        "unused requirement",
    )
    .expect_err("two configured providers match");
    assert!(
        error.to_string().contains(
            "Multiple configured providers matched (zz-prov-a, zz-prov-b). Specify --provider.",
        ),
        "upstream credential-print.ts:84-85 text, got: {error}"
    );

    // With only one configured credential the same pattern resolves.
    storage.delete("zz-prov-b").expect("delete one credential");
    assert_eq!(
        pir::ai::auth::cli::resolve_provider_from_flags(
            None,
            Some("zz-shared-model-xyz"),
            "unused requirement"
        )
        .expect("one configured provider resolves"),
        "zz-prov-a"
    );

    // A canonical `provider/model` reference still resolves without any
    // configured credential (pir's existing canonical shortcut).
    storage.delete("zz-prov-a").expect("delete last credential");
    assert_eq!(
        pir::ai::auth::cli::resolve_provider_from_flags(
            None,
            Some("zz-prov-a/zz-shared-model-xyz"),
            "unused requirement"
        )
        .expect("canonical reference resolves"),
        "zz-prov-a"
    );
    drop(guard);
}
