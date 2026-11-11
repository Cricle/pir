// Round-13 SDK `on_event` foreground coverage. Included textually by
// `tests/sdk_extended.rs` (so every test keeps the `sdk_extended::` prefix);
// not a standalone test target (the crate sets autotests = false).
//
// Covers:
// - `on_event("tool_call")` blocking real foreground `read` / `edit` / `bash`:
//   the tools never execute, the model gets the handler's error reason as the
//   tool result, and `on_event("tool_result")` never runs for them.
// - `on_event("tool_call")` rewriting `input` for a real foreground `write`
//   and `bash`: the rewritten arguments are what execute, reach the next
//   provider request, and persist in the session JSONL.
// - `on_event("tool_result")` rewriting content/details/isError/usage on real
//   foreground tools: the rewritten state is what the next request and the
//   session JSONL carry, with one ordered tool_call -> tool_result per call.
// - `AgentBuilder::resource_loader`: explicit skills enter the derived system
//   prompt, no loader stays hermetic, and an explicit system_prompt is pinned.

#[tokio::test]
async fn on_event_tool_call_blocks_real_read_edit_bash_foreground() {
    let cwd = TempDir::new("sdk-on-event-block");
    let secret = cwd.path().join("secret.txt");
    std::fs::write(&secret, "TOP-SECRET-CONTENT\n").expect("write secret");
    let edit_target = cwd.path().join("edit.txt");
    std::fs::write(&edit_target, "original\n").expect("write edit target");
    let marker = cwd.path().join("bash-ran.marker");
    let secret_arg = secret.to_string_lossy().to_string();
    let edit_arg = edit_target.to_string_lossy().to_string();
    let command = format!("printf ran > \"{}\"", marker.display());
    let command_for_registry = command.clone();

    let seen: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let registry = registry(Arc::new(move |index, context| match index {
        0 => assistant(
            vec![
                tool_call_block("read-1", "read", vec![("path", json!(secret_arg))]),
                tool_call_block(
                    "edit-1",
                    "edit",
                    vec![
                        ("path", json!(edit_arg)),
                        (
                            "edits",
                            json!([{ "oldText": "original", "newText": "mutated" }]),
                        ),
                    ],
                ),
                tool_call_block("bash-1", "bash", vec![("command", json!(command_for_registry))]),
            ],
            StopReason::ToolUse,
        ),
        1 => {
            recorded.lock().unwrap().push(context.clone());
            assistant(vec![text_block("blocked turn done")], StopReason::Stop)
        }
        _ => assistant(vec![text_block("done")], StopReason::Stop),
    }));

    let gate_log: Arc<Mutex<Vec<(String, String, serde_json::Value)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let gate_for_handler = Arc::clone(&gate_log);
    let result_handler_calls = Arc::new(AtomicUsize::new(0));
    let result_calls_for_handler = Arc::clone(&result_handler_calls);
    let agent = agent_builder(registry, cwd.path())
        .agent_dir(cwd.path().join("agent"))
        .session(SessionMode::Dir(cwd.path().join("sessions")))
        .core_tools(true)
        .on_event("tool_call", move |payload| {
            let call_id = payload["toolCallId"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let tool_name = payload["toolName"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            gate_for_handler.lock().unwrap().push((
                call_id.clone(),
                tool_name,
                payload["input"].clone(),
            ));
            let reason = match call_id.as_str() {
                "read-1" => Some("policy: read blocked"),
                "edit-1" => Some("policy: edit blocked"),
                "bash-1" => Some("policy: bash blocked"),
                _ => None,
            };
            match reason {
                Some(reason) => PluginEventOutcome::ToolCall {
                    block: true,
                    reason: Some(reason.to_owned()),
                    terminate: false,
                },
                None => PluginEventOutcome::None,
            }
        })
        .on_event("tool_result", move |_payload| {
            result_calls_for_handler.fetch_add(1, Ordering::SeqCst);
            PluginEventOutcome::None
        })
        .build()
        .await
        .expect("build blocking on_event agent");

    let outcome = agent
        .prompt("try to read, edit and run")
        .await
        .expect("prompt");
    assert_eq!(outcome.tool_results.len(), 3, "one result per call");
    let mut by_id = std::collections::HashMap::new();
    for result in &outcome.tool_results {
        by_id.insert(result.tool_call_id.clone(), result);
    }
    for (call_id, reason) in [
        ("read-1", "policy: read blocked"),
        ("edit-1", "policy: edit blocked"),
        ("bash-1", "policy: bash blocked"),
    ] {
        let result = by_id
            .get(call_id)
            .unwrap_or_else(|| panic!("missing result for {call_id}"));
        assert!(result.is_error, "{call_id} must be an error result");
        assert_eq!(
            content_text(&result.content),
            reason,
            "{call_id} carries the handler reason"
        );
    }
    assert!(
        !content_text(&by_id["read-1"].content).contains("TOP-SECRET-CONTENT"),
        "a blocked read must not execute (no file content in the result)"
    );

    // The gate saw each call once, in source order, with the model's original
    // input; the tool_result face never ran because no tool executed.
    {
        let gate = gate_log.lock().unwrap();
        assert_eq!(gate.len(), 3, "exactly one tool_call per call: {gate:?}");
        assert_eq!(gate[0].0, "read-1");
        assert_eq!(gate[0].1, "read");
        assert_eq!(gate[0].2["path"], secret.to_string_lossy().as_ref());
        assert_eq!(gate[1].0, "edit-1");
        assert_eq!(gate[1].1, "edit");
        assert_eq!(
            gate[1].2["path"],
            edit_target.to_string_lossy().as_ref()
        );
        assert_eq!(gate[1].2["edits"][0]["oldText"], "original");
        assert_eq!(gate[2].0, "bash-1");
        assert_eq!(gate[2].1, "bash");
        assert_eq!(gate[2].2["command"], command);
    }
    assert_eq!(
        result_handler_calls.load(Ordering::SeqCst),
        0,
        "tool_result must not run for blocked calls"
    );

    // No side effect of any blocked tool reached the filesystem.
    assert!(
        !marker.exists(),
        "a blocked bash call must not execute its command"
    );
    assert_eq!(
        std::fs::read_to_string(&edit_target).expect("read edit target"),
        "original\n",
        "a blocked edit must leave the file untouched"
    );
    assert_eq!(
        std::fs::read_to_string(&secret).expect("read secret"),
        "TOP-SECRET-CONTENT\n"
    );

    // The next provider request carries all three blocked error results.
    {
        let contexts = seen.lock().unwrap();
        let context = contexts.first().expect("follow-up request");
        for (call_id, reason) in [
            ("read-1", "policy: read blocked"),
            ("edit-1", "policy: edit blocked"),
            ("bash-1", "policy: bash blocked"),
        ] {
            let result = context
                .messages
                .iter()
                .find_map(|message| match message {
                    Message::ToolResult(result) if result.tool_call_id == call_id => Some(result),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{call_id} missing from the follow-up request"));
            assert!(
                result.is_error,
                "{call_id} reaches the next request as an error"
            );
            assert_eq!(content_text(&result.content), reason);
        }
    }

    // The durable session carries the same error results.
    let path = agent.session_path().expect("session path");
    let reopened = SessionManager::open(&path).expect("reopen session");
    let entries = reopened.build_context_entries().expect("context entries");
    let stored: Vec<_> = entries
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(stored.len(), 3, "three stored tool results: {path:?}");
    for result in stored {
        assert!(result.is_error);
        assert!(
            [
                "policy: read blocked",
                "policy: edit blocked",
                "policy: bash blocked"
            ]
            .contains(&content_text(&result.content).as_str()),
            "stored blocked result: {result:?}"
        );
    }

    agent.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn on_event_tool_call_rewrites_input_on_real_write_and_bash_foreground() {
    let cwd = TempDir::new("sdk-on-event-rewrite");
    let original = cwd.path().join("original.txt");
    let rewritten = cwd.path().join("rewritten.txt");
    let original_arg = original.to_string_lossy().to_string();
    let rewritten_arg = rewritten.to_string_lossy().to_string();

    let seen: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let registry = registry(Arc::new(move |index, context| match index {
        0 => assistant(
            vec![
                tool_call_block(
                    "write-1",
                    "write",
                    vec![
                        ("path", json!(original_arg)),
                        ("content", json!("model-content")),
                    ],
                ),
                tool_call_block("bash-1", "bash", vec![("command", json!("printf ORIGINAL"))]),
            ],
            StopReason::ToolUse,
        ),
        1 => {
            recorded.lock().unwrap().push(context.clone());
            assistant(vec![text_block("rewritten turn done")], StopReason::Stop)
        }
        _ => assistant(vec![text_block("done")], StopReason::Stop),
    }));

    let gate_log: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let gate_for_handler = Arc::clone(&gate_log);
    let agent = agent_builder(registry, cwd.path())
        .agent_dir(cwd.path().join("agent"))
        .session(SessionMode::Dir(cwd.path().join("sessions")))
        .core_tools(true)
        .on_event("tool_call", move |payload| {
            let call_id = payload["toolCallId"].as_str().unwrap_or_default();
            gate_for_handler
                .lock()
                .unwrap()
                .push((call_id.to_owned(), payload["input"].clone()));
            match call_id {
                "write-1" => {
                    payload["input"]["path"] = json!(rewritten_arg);
                    payload["input"]["content"] = json!("handler-content");
                }
                "bash-1" => {
                    payload["input"]["command"] = json!("printf REWRITTEN");
                }
                _ => {}
            }
        })
        .build()
        .await
        .expect("build rewriting on_event agent");

    let outcome = agent.prompt("write and run").await.expect("prompt");
    assert_eq!(outcome.tool_results.len(), 2);
    let mut by_id = std::collections::HashMap::new();
    for result in &outcome.tool_results {
        by_id.insert(result.tool_call_id.clone(), result);
    }
    assert!(!by_id["write-1"].is_error, "rewritten write succeeds");
    assert!(content_text(&by_id["write-1"].content).contains("rewritten.txt"));
    assert!(!by_id["bash-1"].is_error, "rewritten bash succeeds");
    assert_eq!(content_text(&by_id["bash-1"].content).trim(), "REWRITTEN");

    // The rewritten arguments are what executed and persisted.
    assert!(
        !original.exists(),
        "the model's original path must not be written"
    );
    assert_eq!(
        std::fs::read_to_string(&rewritten).expect("read rewritten target"),
        "handler-content",
        "the write executed with the handler's rewritten path and content"
    );
    {
        let gate = gate_log.lock().unwrap();
        assert_eq!(gate.len(), 2, "one tool_call per call: {gate:?}");
        assert_eq!(gate[0].0, "write-1");
        assert_eq!(gate[0].1["path"], original.to_string_lossy().as_ref());
        assert_eq!(gate[0].1["content"], "model-content");
        assert_eq!(gate[1].0, "bash-1");
        assert_eq!(gate[1].1["command"], "printf ORIGINAL");
    }

    // The next request and the session record the executed (rewritten) form.
    {
        let contexts = seen.lock().unwrap();
        let context = contexts.first().expect("follow-up request");
        let write_result = context
            .messages
            .iter()
            .find_map(|message| match message {
                Message::ToolResult(result) if result.tool_call_id == "write-1" => Some(result),
                _ => None,
            })
            .expect("write result");
        assert!(content_text(&write_result.content).contains("rewritten.txt"));
        let bash_result = context
            .messages
            .iter()
            .find_map(|message| match message {
                Message::ToolResult(result) if result.tool_call_id == "bash-1" => Some(result),
                _ => None,
            })
            .expect("bash result");
        assert_eq!(content_text(&bash_result.content).trim(), "REWRITTEN");
    }
    let path = agent.session_path().expect("session path");
    let persisted = std::fs::read_to_string(&path).expect("read session file");
    assert!(
        persisted.contains("rewritten.txt"),
        "the executed rewritten path is durable: {persisted}"
    );
    assert!(
        persisted.contains("REWRITTEN"),
        "the rewritten command output is durable"
    );

    agent.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn on_event_tool_result_rewrites_content_details_error_usage_foreground() {
    let cwd = TempDir::new("sdk-on-event-result");

    let seen: Arc<Mutex<Vec<Context>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&seen);
    let registry = registry(Arc::new(move |index, context| match index {
        0 => assistant(
            vec![
                tool_call_block("ok-1", "bash", vec![("command", json!("printf hello"))]),
                tool_call_block(
                    "fail-1",
                    "bash",
                    vec![("command", json!("printf boom; exit 7"))],
                ),
            ],
            StopReason::ToolUse,
        ),
        1 => {
            recorded.lock().unwrap().push(context.clone());
            assistant(vec![text_block("result turn done")], StopReason::Stop)
        }
        _ => assistant(vec![text_block("done")], StopReason::Stop),
    }));

    let event_log: Arc<Mutex<Vec<(&'static str, String, serde_json::Value)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let call_log = Arc::clone(&event_log);
    let result_log = Arc::clone(&event_log);
    let agent = agent_builder(registry, cwd.path())
        .agent_dir(cwd.path().join("agent"))
        .session(SessionMode::Dir(cwd.path().join("sessions")))
        .core_tools(true)
        .on_event("tool_call", move |payload| {
            call_log.lock().unwrap().push((
                "tool_call",
                payload["toolCallId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                payload.clone(),
            ));
        })
        .on_event("tool_result", move |payload| {
            let call_id = payload["toolCallId"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            result_log.lock().unwrap().push((
                "tool_result",
                call_id.clone(),
                payload.clone(),
            ));
            match call_id.as_str() {
                "ok-1" => PluginEventOutcome::ToolResult {
                    content: Some(json!([{ "type": "text", "text": "REWRITTEN OK" }])),
                    details: Some(json!({ "rewritten": true, "call": "ok-1" })),
                    structured_content: None,
                    is_error: Some(true),
                    usage: Some(json!({ "input": 11, "output": 22, "totalTokens": 33 })),
                },
                "fail-1" => PluginEventOutcome::ToolResult {
                    content: Some(json!([{ "type": "text", "text": "SOFTENED FAILURE" }])),
                    details: Some(json!({ "rewritten": true, "call": "fail-1" })),
                    structured_content: None,
                    is_error: Some(false),
                    usage: Some(json!({ "input": 5, "output": 6, "totalTokens": 11 })),
                },
                _ => PluginEventOutcome::None,
            }
        })
        .build()
        .await
        .expect("build tool_result rewrite agent");

    let outcome = agent.prompt("run both").await.expect("prompt");
    assert_eq!(outcome.tool_results.len(), 2);
    let mut by_id = std::collections::HashMap::new();
    for result in &outcome.tool_results {
        by_id.insert(result.tool_call_id.clone(), result);
    }
    let ok = by_id["ok-1"];
    assert!(
        ok.is_error,
        "the handler flipped the successful result to an error"
    );
    assert_eq!(content_text(&ok.content), "REWRITTEN OK");
    assert_eq!(
        ok.details,
        Some(json!({ "rewritten": true, "call": "ok-1" }))
    );
    assert_eq!(
        ok.usage,
        Some(Usage {
            input: 11,
            output: 22,
            total_tokens: 33,
            ..Usage::default()
        })
    );
    let failed = by_id["fail-1"];
    assert!(
        !failed.is_error,
        "the handler cleared the failing result's error"
    );
    assert_eq!(content_text(&failed.content), "SOFTENED FAILURE");
    assert_eq!(
        failed.details,
        Some(json!({ "rewritten": true, "call": "fail-1" }))
    );
    assert_eq!(
        failed.usage,
        Some(Usage {
            input: 5,
            output: 6,
            total_tokens: 11,
            ..Usage::default()
        })
    );

    // Event order: each call has exactly one tool_call before one tool_result;
    // the tool_result payload carries the PRE-merge execution state.
    {
        let log = event_log.lock().unwrap();
        for call_id in ["ok-1", "fail-1"] {
            assert_eq!(
                log.iter()
                    .filter(|(name, id, _)| *name == "tool_call" && id == call_id)
                    .count(),
                1,
                "{call_id}: exactly one tool_call"
            );
            assert_eq!(
                log.iter()
                    .filter(|(name, id, _)| *name == "tool_result" && id == call_id)
                    .count(),
                1,
                "{call_id}: exactly one tool_result"
            );
            let call_index = log
                .iter()
                .position(|(name, id, _)| *name == "tool_call" && id == call_id)
                .expect("tool_call logged");
            let result_index = log
                .iter()
                .position(|(name, id, _)| *name == "tool_result" && id == call_id)
                .expect("tool_result logged");
            assert!(
                call_index < result_index,
                "{call_id}: tool_call must precede tool_result: {log:?}"
            );
        }
        let ok_result = log
            .iter()
            .find(|(name, id, _)| *name == "tool_result" && id == "ok-1")
            .expect("ok tool_result");
        assert_eq!(ok_result.2["isError"], false, "pre-merge success state");
        assert_eq!(ok_result.2["content"][0]["text"], "hello");
        let fail_result = log
            .iter()
            .find(|(name, id, _)| *name == "tool_result" && id == "fail-1")
            .expect("fail tool_result");
        assert_eq!(fail_result.2["isError"], true, "pre-merge failure state");
        assert!(
            fail_result.2["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("boom"),
            "pre-merge failure content: {fail_result:?}"
        );
    }

    // The rewritten values are what the next provider request carries.
    {
        let contexts = seen.lock().unwrap();
        let context = contexts.first().expect("follow-up request");
        let ok = context
            .messages
            .iter()
            .find_map(|message| match message {
                Message::ToolResult(result) if result.tool_call_id == "ok-1" => Some(result),
                _ => None,
            })
            .expect("ok result");
        assert!(ok.is_error);
        assert_eq!(content_text(&ok.content), "REWRITTEN OK");
        assert_eq!(
            ok.details,
            Some(json!({ "rewritten": true, "call": "ok-1" }))
        );
        assert_eq!(
            ok.usage,
            Some(Usage {
                input: 11,
                output: 22,
                total_tokens: 33,
                ..Usage::default()
            })
        );
        let failed = context
            .messages
            .iter()
            .find_map(|message| match message {
                Message::ToolResult(result) if result.tool_call_id == "fail-1" => Some(result),
                _ => None,
            })
            .expect("fail result");
        assert!(!failed.is_error);
        assert_eq!(content_text(&failed.content), "SOFTENED FAILURE");
    }

    // And the same rewritten values are durable in the session JSONL.
    let path = agent.session_path().expect("session path");
    let reopened = SessionManager::open(&path).expect("reopen session");
    let entries = reopened.build_context_entries().expect("context entries");
    let mut stored = std::collections::HashMap::new();
    for message in &entries {
        if let Message::ToolResult(result) = message {
            stored.insert(result.tool_call_id.clone(), result);
        }
    }
    let ok = stored.get("ok-1").expect("stored ok result");
    assert!(ok.is_error);
    assert_eq!(content_text(&ok.content), "REWRITTEN OK");
    assert_eq!(
        ok.details,
        Some(json!({ "rewritten": true, "call": "ok-1" }))
    );
    assert_eq!(
        ok.usage,
        Some(Usage {
            input: 11,
            output: 22,
            total_tokens: 33,
            ..Usage::default()
        }),
        "handler usage is persisted on the tool result"
    );
    let failed = stored.get("fail-1").expect("stored fail result");
    assert!(!failed.is_error);
    assert_eq!(content_text(&failed.content), "SOFTENED FAILURE");
    assert_eq!(
        failed.usage,
        Some(Usage {
            input: 5,
            output: 6,
            total_tokens: 11,
            ..Usage::default()
        })
    );

    agent.shutdown().await.expect("shutdown");
}

// ---------------------------------------------------------------------------
// AgentBuilder::resource_loader: explicit skills reach the derived system
// prompt; no loader stays hermetic; an explicit system_prompt stays pinned.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resource_loader_skills_reach_system_prompt_and_explicit_prompt_stays_pinned() {
    let cwd = TempDir::new("sdk-resource-loader");
    let skill_dir = cwd.path().join("skills");
    let skill = skill_dir.join("round13-skill");
    std::fs::create_dir_all(&skill).expect("create skill dir");
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: round13-skill\ndescription: round13 marker skill\n---\nBody\n",
    )
    .expect("write SKILL.md");

    let capture_prompt = |label: &'static str| {
        let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let recorded = Arc::clone(&seen);
        let registry = registry(Arc::new(move |_index, context| {
            recorded
                .lock()
                .unwrap()
                .replace(context.system_prompt.clone().unwrap_or_default());
            assistant(vec![text_block(label)], StopReason::Stop)
        }));
        (seen, registry)
    };

    // 1) Explicit loader: the skill is installed and rendered into the prompt.
    let (seen_loaded, registry_loaded) = capture_prompt("loaded");
    let loader = DefaultResourceLoader::new(cwd.path(), cwd.path().join("agent"))
        .with_home(cwd.path().join("home"))
        .with_project_trusted(true)
        .with_skill_paths([skill_dir.clone()]);
    let agent = agent_builder(registry_loaded, cwd.path())
        .agent_dir(cwd.path().join("agent"))
        .core_tools(true)
        .no_session()
        .resource_loader(loader)
        .build()
        .await
        .expect("build loaded agent");
    agent.prompt("use the skill").await.expect("prompt loaded");
    let loaded_prompt = seen_loaded
        .lock()
        .unwrap()
        .clone()
        .expect("first request prompt");
    assert!(
        loaded_prompt.contains("<available_skills>")
            && loaded_prompt.contains("<name>round13-skill</name>")
            && loaded_prompt.contains("round13 marker skill"),
        "the explicit loader's skill must enter the system prompt: {loaded_prompt}"
    );
    agent.shutdown().await.expect("shutdown loaded");

    // 3) Explicit system_prompt pins the whole prompt; loader skills do not
    // append to it.
    let (seen_pinned, registry_pinned) = capture_prompt("pinned");
    let loader = DefaultResourceLoader::new(cwd.path(), cwd.path().join("agent"))
        .with_home(cwd.path().join("home"))
        .with_project_trusted(true)
        .with_skill_paths([skill_dir.clone()]);
    let agent = agent_builder(registry_pinned, cwd.path())
        .agent_dir(cwd.path().join("agent"))
        .core_tools(true)
        .no_session()
        .system_prompt("PINNED R13 PROMPT")
        .resource_loader(loader)
        .build()
        .await
        .expect("build pinned agent");
    agent.prompt("pinned").await.expect("prompt pinned");
    let pinned_prompt = seen_pinned
        .lock()
        .unwrap()
        .clone()
        .expect("first request prompt");
    assert_eq!(
        pinned_prompt, "PINNED R13 PROMPT",
        "an explicit system_prompt must stay exactly pinned (no skill section)"
    );
    agent.shutdown().await.expect("shutdown pinned");
}

// Documented contract (docs/sdk.md `resource_loader` row and
// "System-prompt discovery"): without a loader the SDK is hermetic and does
// no implicit skill discovery. PIR-SDK-004 regression: the derived session
// prompt used to run ambient prompt-skill discovery (`<agentDir>/skills`,
// `~/.agents/skills`, ancestor `.agents/skills`) unconditionally. The skill
// below sits in the agent-dir root discovery scans, so a regression is
// deterministic and independent of the developer's real home directory.
#[tokio::test]
async fn resource_loader_unset_stays_hermetic_and_does_not_discover_skills() {
    let cwd = TempDir::new("sdk-resource-loader-hermetic");
    let agent_dir = cwd.path().join("agent");
    let skill = agent_dir
        .join("skills")
        .join("round13-hermetic-skill");
    std::fs::create_dir_all(&skill).expect("create skill dir");
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: round13-hermetic-skill\ndescription: must stay undiscovered\n---\nBody\n",
    )
    .expect("write SKILL.md");

    let seen: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let recorded = Arc::clone(&seen);
    let registry_hermetic = registry(Arc::new(move |_index, context| {
        recorded
            .lock()
            .unwrap()
            .replace(context.system_prompt.clone().unwrap_or_default());
        assistant(vec![text_block("hermetic")], StopReason::Stop)
    }));
    let agent = agent_builder(registry_hermetic, cwd.path())
        .agent_dir(agent_dir.clone())
        .core_tools(true)
        .no_session()
        .build()
        .await
        .expect("build hermetic agent");
    agent.prompt("no loader").await.expect("prompt hermetic");
    let prompt = seen
        .lock()
        .unwrap()
        .clone()
        .expect("first request prompt");
    assert!(
        !prompt.contains("round13-hermetic-skill") && !prompt.contains("<available_skills>"),
        "without a loader the SDK must not discover skills: {prompt}"
    );
    agent.shutdown().await.expect("shutdown hermetic");

    // Positive control / explicit opt-in: `.discover_system_prompt(true)` is
    // the documented escape hatch, and it must reach the same skill root the
    // hermetic build ignored. This proves the assertion above is not vacuous.
    let seen_opt_in: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let recorded_opt_in = Arc::clone(&seen_opt_in);
    let registry_opt_in = registry(Arc::new(move |_index, context| {
        recorded_opt_in
            .lock()
            .unwrap()
            .replace(context.system_prompt.clone().unwrap_or_default());
        assistant(vec![text_block("opt-in")], StopReason::Stop)
    }));
    let agent = agent_builder(registry_opt_in, cwd.path())
        .agent_dir(agent_dir)
        .core_tools(true)
        .no_session()
        .discover_system_prompt(true)
        .build()
        .await
        .expect("build opt-in agent");
    agent.prompt("discover").await.expect("prompt opt-in");
    let opt_in_prompt = seen_opt_in
        .lock()
        .unwrap()
        .clone()
        .expect("opt-in first request prompt");
    assert!(
        opt_in_prompt.contains("<available_skills>")
            && opt_in_prompt.contains("<name>round13-hermetic-skill</name>"),
        "explicit discovery opt-in must discover <agentDir>/skills: {opt_in_prompt}"
    );
    agent.shutdown().await.expect("shutdown opt-in");
}
