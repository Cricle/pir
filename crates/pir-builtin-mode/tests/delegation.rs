// Integration tests (kept out of src/).

use pir_builtin_mode::*;
use serde_json::json;

#[test]
fn collects_single_parallel_and_chain_agents() {
    assert_eq!(subagent_agents(&json!({"agent": "worker"})), vec!["worker"]);
    assert_eq!(
        subagent_agents(&json!({"tasks": [{"agent": "worker"}, {"agent": "scout"}]})),
        vec!["worker", "scout"]
    );
    assert_eq!(
        subagent_agents(&json!({"chain": [{"agent": "planner"}]})),
        vec!["planner"]
    );
    assert!(subagent_agents(&json!({})).is_empty());
}

#[test]
fn approval_id_is_stable_and_field_sensitive() {
    let base = json!({"agent": "worker", "task": "fix it"});
    assert_eq!(approval_id(&base), approval_id(&base.clone()));
    assert!(approval_id(&base).starts_with('a'));
    let wider = json!({"agent": "worker", "task": "fix it", "tools": ["bash"]});
    assert_ne!(approval_id(&base), approval_id(&wider));
    let reordered = json!({"task": "fix it", "agent": "worker"});
    // Serialized values are hashed per field, so key order does not matter.
    assert_eq!(approval_id(&base), approval_id(&reordered));
}

#[test]
fn plan_id_changes_with_verification() {
    let plan = "1. do the thing";
    let a = plan_approval_id(plan, None);
    let b = plan_approval_id(plan, Some(&Verification(vec!["cargo test".into()])));
    let c = plan_approval_id(plan, Some(&Verification(vec!["cargo check".into()])));
    assert_ne!(a, b);
    assert_ne!(b, c);
    assert!(a.starts_with('p'));
}

#[test]
fn summaries_and_truncation() {
    assert_eq!(plan_summary("\n\n  first line\nsecond"), "plan: first line");
    assert_eq!(truncate_chars("abc", 3), "abc");
    assert_eq!(truncate_chars("abcd", 3), "abc…");
    assert_eq!(
        approval_summary(&json!({"tasks": [{"agent": "worker", "task": "do it"}]})),
        "worker: do it"
    );
    assert_eq!(approval_summary(&json!({})), "subagent: (no task text)");
}

#[test]
fn routing_respects_settings() {
    let mut input = json!({"agent": "worker"});
    apply_foreman_routing(
        &mut input,
        ModeSettings {
            worktree: true,
            router: RouterMode::Process,
            ..ModeSettings::default()
        },
        true,
    );
    assert_eq!(input["isolation"], "worktree");
    assert_eq!(input["worktreeMerge"], "auto");
    assert_eq!(input["executor"], "process");

    let mut reviewer = json!({"agent": "reviewer"});
    apply_foreman_routing(&mut reviewer, ModeSettings::default(), false);
    assert_eq!(reviewer["isolation"], "none");
}

#[test]
fn readonly_override_covers_nested_tasks() {
    let mut input = json!({
        "agent": "scout",
        "tasks": [{"agent": "scout"}, {"agent": "planner"}],
        "chain": [{"agent": "planner"}]
    });
    force_readonly_subagent_tools(&mut input);
    assert_eq!(input["isolation"], "none");
    assert_eq!(input["worktreeMerge"], "none");
    assert_eq!(input["tasks"][0]["tools"], json!(PLAN_CHILD_TOOLS));
    assert_eq!(input["chain"][0]["tools"], json!(PLAN_CHILD_TOOLS));
}

#[test]
fn scan_advances_generation_and_tracks_calls() {
    let message = json!({
        "role": "assistant",
        "content": [
            {"type": "toolCall", "name": "subagent", "id": "w1", "arguments": {"agent": "worker"}},
            {"type": "toolCall", "name": "subagent", "id": "r1", "arguments": {"agent": "reviewer"}},
            {"type": "text", "text": "ignored"}
        ]
    });
    let mut generation = 0;
    let mut last_reviewer = None;
    let mut last_worker = None;
    scan_subagent_calls(
        &message,
        &mut generation,
        &mut last_reviewer,
        &mut last_worker,
    );
    assert_eq!(generation, 1);
    assert_eq!(last_worker.as_deref(), Some("w1"));
    assert_eq!(last_reviewer.as_deref(), Some("r1"));
}

#[test]
fn tool_policy_per_mode() {
    assert!(tool_allowed_in_mode(Mode::Plan, "submit_plan"));
    assert!(!tool_allowed_in_mode(Mode::Foreman, "submit_plan"));
    assert!(!tool_allowed_in_mode(Mode::Foreman, "edit"));
    assert!(tool_allowed_in_mode(Mode::Normal, "edit"));
    assert!(tool_visible_in_mode(Mode::Plan, "submit_plan"));
    assert!(!tool_visible_in_mode(Mode::Normal, "submit_plan"));
}
