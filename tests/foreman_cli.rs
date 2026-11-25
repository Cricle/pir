//! `pir foreman` CLI surface and the public journal/sidecar bridge.

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "pir-foreman-cli-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("temp dir");
    path
}

#[test]
fn foreman_help_lists_the_v2_subcommands() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pir"))
        .args(["foreman", "--help"])
        .output()
        .expect("run pir foreman --help");
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    for command in [
        "run", "status", "logs", "abort", "approve", "deny", "resume", "clean",
    ] {
        assert!(text.contains(command), "help missing {command}: {text}");
    }
}

#[test]
fn sidecar_is_written_in_v2_shape() {
    let dir = temp_dir("sidecar");
    let session = dir.join("session.jsonl");
    pir::foreman::write_running_sidecar(
        &session,
        true,
        pir::foreman::ForemanRouter::Process,
        pir::foreman::ForemanApprovals::Manual,
    )
    .expect("write sidecar");
    let text = std::fs::read_to_string(dir.join("session.jsonl.mode.json")).expect("sidecar");
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(value["version"], 2);
    assert_eq!(value["mode"], "foreman");
    assert_eq!(value["settings"]["worktree"], true);
    assert_eq!(value["settings"]["router"], "process");
    assert_eq!(value["settings"]["approvals"], "manual");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn approval_decisions_update_the_journal_and_clear_pending() {
    let dir = temp_dir("decision");
    let session = dir.join("session.jsonl");
    let mut journal = session.as_os_str().to_owned();
    journal.push(".foreman.jsonl");
    std::fs::write(
        PathBuf::from(journal),
        serde_json::to_string(&json!({
            "v": 1,
            "t": "snapshot",
            "mode": "foreman",
            "phase": "PLANNING",
            "generation": 0,
            "reviewer": null,
            "nudges": 0,
            "settings": {"approvals": "manual", "router": "inline", "worktree": false},
            "approvals": [{
                "id": "a1",
                "call": "call-1",
                "summary": "worker: write",
                "state": "requested",
                "createdAtMs": now_ms()
            }],
            "allow_next_worker": false
        }))
        .expect("snapshot JSON"),
    )
    .expect("write journal");
    let status = pir::foreman::read_status(&session).expect("status");
    assert_eq!(status.pending.len(), 1);
    let decided = pir::foreman::decide_approval(&session, Some("a1"), true).expect("approve");
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0].state, "approved");
    let status = pir::foreman::read_status(&session).expect("status after");
    assert!(status.pending.is_empty());
    assert!(status.allow_next_worker);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn approving_a_plan_switches_the_journal_to_normal() {
    let dir = temp_dir("plan-decision");
    let session = dir.join("session.jsonl");
    let mut journal_path = session.as_os_str().to_owned();
    journal_path.push(".foreman.jsonl");
    std::fs::write(
        PathBuf::from(journal_path),
        serde_json::to_string(&json!({
            "v": 1,
            "t": "snapshot",
            "mode": "plan",
            "phase": "PLANNING",
            "generation": 0,
            "reviewer": null,
            "nudges": 0,
            "settings": {"approvals": "auto", "router": "inline", "worktree": false},
            "approvals": [{
                "id": "p1",
                "call": "call-plan",
                "summary": "plan: step one",
                "state": "requested",
                "plan": "1. do step one\n2. verify",
                "kind": "plan",
                "createdAtMs": now_ms()
            }],
            "allow_next_worker": false,
            "submitted_plan": "1. do step one\n2. verify"
        }))
        .expect("snapshot JSON"),
    )
    .expect("write journal");

    let decided = pir::foreman::decide_approval(&session, Some("p1"), true).expect("approve plan");
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0].kind, "plan");
    assert_eq!(
        decided[0].plan.as_deref(),
        Some("1. do step one\n2. verify")
    );

    // The journal (authoritative on resume) must now say normal, with no
    // pending approvals and no stale worker allowance.
    let status = pir::foreman::read_status(&session).expect("status after");
    assert_eq!(status.mode, "normal", "journal mode after plan approval");
    assert!(status.pending.is_empty());
    assert!(
        !status.allow_next_worker,
        "plan approval must not admit a worker"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn approving_a_plan_carries_its_verification_into_an_active_contract() {
    let dir = temp_dir("plan-contract");
    let session = dir.join("session.jsonl");
    let mut journal_path = session.as_os_str().to_owned();
    journal_path.push(".foreman.jsonl");
    std::fs::write(
        PathBuf::from(journal_path.clone()),
        serde_json::to_string(&json!({
            "v": 1,
            "t": "snapshot",
            "mode": "plan",
            "phase": "PLANNING",
            "generation": 0,
            "reviewer": null,
            "nudges": 0,
            "settings": {"approvals": "auto", "router": "inline", "worktree": false},
            "approvals": [{
                "id": "p1",
                "call": "call-plan",
                "summary": "plan: step one",
                "state": "requested",
                "plan": "1. do step one",
                "verification": ["cargo test --all"],
                "kind": "plan",
                "createdAtMs": now_ms()
            }],
            "allow_next_worker": false,
            "submitted_plan": "1. do step one"
        }))
        .expect("snapshot JSON"),
    )
    .expect("write journal");

    let decided = pir::foreman::decide_approval(&session, Some("p1"), true).expect("approve plan");
    assert_eq!(decided[0].verification.as_deref(), Some("cargo test --all"));

    // The authoritative snapshot now carries the execution contract, visible
    // to the resumed Normal run and to the verification gate.
    let text = std::fs::read_to_string(PathBuf::from(journal_path)).expect("journal after");
    assert!(text.contains("active_contract"), "{text}");
    assert!(text.contains("cargo test --all"), "{text}");
    assert!(text.contains("\"normal\""), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn starting_a_foreman_run_overrides_an_existing_journal() {
    let dir = temp_dir("restart");
    let session = dir.join("session.jsonl");
    let mut journal_path = session.as_os_str().to_owned();
    journal_path.push(".foreman.jsonl");
    std::fs::write(
        PathBuf::from(journal_path),
        serde_json::to_string(&json!({
            "v": 1,
            "t": "snapshot",
            "mode": "plan",
            "phase": "PLANNING",
            "generation": 3,
            "reviewer": "call-old",
            "nudges": 2,
            "settings": {"approvals": "auto", "router": "inline", "worktree": false},
            "approvals": [],
            "allow_next_worker": false
        }))
        .expect("snapshot JSON"),
    )
    .expect("write journal");

    pir::foreman::write_running_sidecar(
        &session,
        true,
        pir::foreman::ForemanRouter::Process,
        pir::foreman::ForemanApprovals::Manual,
    )
    .expect("start foreman session");

    // Journal is authoritative on restore: it must reflect the new CLI flags.
    let status = pir::foreman::read_status(&session).expect("status");
    assert_eq!(status.mode, "foreman");
    assert_eq!(status.generation, 0, "fresh run resets stale gate state");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unverified_phase_is_the_only_strict_exit_signal() {
    let dir = temp_dir("unverified-exit");
    let session = dir.join("session.jsonl");
    let journal = {
        let mut path = session.as_os_str().to_owned();
        path.push(".foreman.jsonl");
        PathBuf::from(path)
    };
    let write = |phase: &str| {
        std::fs::write(
            &journal,
            serde_json::to_string(&json!({
                "v": 1,
                "t": "snapshot",
                "mode": "foreman",
                "phase": phase,
                "generation": 0,
                "reviewer": null,
                "nudges": 0,
                "settings": {"approvals": "auto", "router": "inline", "worktree": false},
                "approvals": [],
                "allow_next_worker": false
            }))
            .expect("snapshot JSON"),
        )
        .expect("write journal");
    };

    // No journal yet: nothing to gate on, so a strict child exits normally.
    assert!(pir::foreman::unverified_exit_error(&session).is_none());
    write("VERIFIED");
    assert!(pir::foreman::unverified_exit_error(&session).is_none());
    write("UNVERIFIED");
    let message =
        pir::foreman::unverified_exit_error(&session).expect("unverified phase must fail");
    assert!(message.contains("UNVERIFIED"), "{message}");
    assert!(message.contains("session.jsonl"), "{message}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn expired_requests_are_not_listed_as_pending() {
    let dir = temp_dir("expired-pending");
    let session = dir.join("session.jsonl");
    let mut journal = session.as_os_str().to_owned();
    journal.push(".foreman.jsonl");
    std::fs::write(
        PathBuf::from(journal),
        serde_json::to_string(&json!({
            "v": 1,
            "t": "snapshot",
            "mode": "foreman",
            "phase": "DELEGATING",
            "generation": 1,
            "reviewer": null,
            "nudges": 0,
            "settings": {"approvals": "manual", "router": "inline", "worktree": false},
            "approvals": [
                {
                    "id": "old",
                    "call": "call-old",
                    "summary": "worker: old",
                    "state": "requested",
                    "createdAtMs": now_ms().saturating_sub(25 * 60 * 60 * 1000)
                },
                {
                    "id": "fresh",
                    "call": "call-fresh",
                    "summary": "worker: fresh",
                    "state": "requested",
                    "createdAtMs": now_ms()
                }
            ],
            "allow_next_worker": false
        }))
        .expect("snapshot JSON"),
    )
    .expect("write journal");

    let status = pir::foreman::read_status(&session).expect("status");
    let ids: Vec<&str> = status
        .pending
        .iter()
        .map(|pending| pending.id.as_str())
        .collect();
    assert_eq!(ids, ["fresh"], "expired request must not look decidable");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn approving_a_plan_with_strict_commands_persists_the_contract() {
    let dir = temp_dir("plan-strict-contract");
    let session = dir.join("session.jsonl");
    let mut journal_path = session.as_os_str().to_owned();
    journal_path.push(".foreman.jsonl");
    std::fs::write(
        PathBuf::from(journal_path.clone()),
        serde_json::to_string(&json!({
            "v": 1,
            "t": "snapshot",
            "mode": "plan",
            "phase": "PLANNING",
            "generation": 0,
            "reviewer": null,
            "nudges": 0,
            "settings": {"approvals": "auto", "router": "inline", "worktree": false},
            "approvals": [{
                "id": "p1",
                "call": "call-plan",
                "summary": "plan: step one",
                "state": "requested",
                "plan": "1. do step one",
                "verification": ["cargo test --all", "cargo clippy --all"],
                "kind": "plan",
                "createdAtMs": now_ms()
            }],
            "allow_next_worker": false,
            "submitted_plan": "1. do step one"
        }))
        .expect("snapshot JSON"),
    )
    .expect("write journal");

    let decided = pir::foreman::decide_approval(&session, Some("p1"), true).expect("approve plan");
    // Public CLI surface stays a plain string (commands joined for display).
    assert_eq!(
        decided[0].verification.as_deref(),
        Some("cargo test --all\ncargo clippy --all")
    );

    let text = std::fs::read_to_string(PathBuf::from(journal_path)).expect("journal after");
    assert!(text.contains("active_contract"), "{text}");
    assert!(text.contains("cargo test --all"), "{text}");
    assert!(text.contains("\"normal\""), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}
