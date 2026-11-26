//! A miniature coding assistant, end to end.
//!
//! The scripted model performs a small but real repair: it reads a broken
//! shell module, edits it with the built-in `edit` tool, runs the real test
//! suite with the built-in `bash` tool, and reports the result. The project
//! lives in a fresh temporary directory; the test runner is a plain POSIX
//! shell script (no interpreter dependency).
//!
//! ```text
//! cargo run --example mini_coding --no-default-features --features "sdk,tools"
//! ```

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

use pir::ai::types::ThinkingLevel;
use pir::sdk::{AgentBuilder, SdkError};

const BROKEN: &str = r#"add() {
    printf '%s\n' "$(($1 - $2))"
}

mul() {
    printf '%s\n' "$(($1 * $2))"
}
"#;

const FIXED: &str = r#"add() {
    printf '%s\n' "$(($1 + $2))"
}

mul() {
    printf '%s\n' "$(($1 * $2))"
}
"#;

const TEST: &str = r#". ./calc.sh

result="$(add 2 3)"
[ "$result" = "5" ] || { printf 'add(2, 3) = %s\n' "$result"; exit 1; }
[ "$(mul 2 3)" = "6" ] || { echo "mul(2, 3) failed"; exit 1; }
echo "all tests passed"
"#;

fn run_tests(dir: &Path) -> (bool, String) {
    use std::io::Write as _;
    use std::process::Stdio;

    // Use the same shell resolution as the bash tool: on Windows a bare
    // `bash` may name the WSL launcher instead of the Git Bash the tool uses.
    let shell = pir::coding_agent::tools::resolve_shell_config(None)
        .expect("resolve the shell used by the bash tool");
    let mut command = Command::new(shell.program());
    command
        .args(shell.args())
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if shell.stdin_transport() {
        command.stdin(Stdio::piped());
    } else {
        command.arg("bash test_calc.sh");
    }
    let mut child = command
        .spawn()
        .expect("spawn the shell test runner (the bash tool must have a shell)");
    if shell.stdin_transport() {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin
            .write_all(b"bash test_calc.sh\n")
            .expect("send the test command");
        drop(stdin);
    }
    let output = child
        .wait_with_output()
        .expect("wait for the shell test runner");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), text.trim().to_owned())
}

/// A minimal line diff, enough to show what the model changed.
fn print_diff(before: &str, after: &str) {
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    for (index, line) in before_lines.iter().enumerate() {
        match after_lines.get(index) {
            Some(new_line) if new_line == line => {}
            Some(new_line) => println!("  - {line}\n  + {new_line}"),
            None => println!("  - {line}"),
        }
    }
    for new_line in after_lines.iter().skip(before_lines.len()) {
        println!("  + {new_line}");
    }
}

fn main() -> std::process::ExitCode {
    common::run_big_stack(async {
        if let Err(error) = run().await {
            eprintln!("error: {error:?}");
            return std::process::ExitCode::FAILURE;
        }
        std::process::ExitCode::SUCCESS
    })
}

async fn run() -> Result<(), SdkError> {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("pir-sdk-mini-coding-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("calc.sh"), BROKEN)?;
    std::fs::write(dir.join("test_calc.sh"), TEST)?;

    // 1. The project really fails before the assistant touches it.
    let (passes_before, output_before) = run_tests(&dir);
    println!("before: tests pass={passes_before} output={output_before:?}");
    assert!(!passes_before, "the fixture must start broken");

    // 2. Script the model: read → edit → run tests → summarize.
    let (registry, model) = common::ScriptedProvider::registry(vec![
        common::tool_calls_message(&[("read", json!({ "path": "calc.sh" }))]),
        common::tool_calls_message(&[(
            "edit",
            json!({
                "path": "calc.sh",
                "edits": [{
                    "oldText": "\"$(($1 - $2))\"",
                    "newText": "\"$(($1 + $2))\""
                }]
            }),
        )]),
        common::tool_calls_message(&[("bash", json!({ "command": "bash test_calc.sh" }))]),
        common::text_message("Fixed add() and confirmed the test suite passes."),
    ]);

    // 3. A real agent: built-in coding tools, this directory as cwd.
    let agent = AgentBuilder::new()
        .with_registry(registry, model)
        .system_prompt("You are a careful coding assistant. Read before you edit.")
        .cwd(dir.clone())
        .core_tools(true)
        .no_session()
        .no_compaction()
        .thinking_level(ThinkingLevel::Off)
        .build()
        .await?;

    let outcome = agent
        .prompt("Fix the failing tests in this project.")
        .await?;

    println!("assistant: {}", outcome.message_text().unwrap_or_default());
    for result in outcome.tool_result_texts() {
        println!("tool result:\n{result}\n---");
    }
    println!(
        "run: turns={} aborted={} tokens={} tool_results={}",
        outcome.turns,
        outcome.aborted,
        outcome.usage.total_tokens,
        outcome.tool_results.len()
    );
    println!("messages in run: {}", outcome.messages.len());

    // 4. The repair must be real: the file changed and the tests now pass.
    let fixed = std::fs::read_to_string(dir.join("calc.sh"))?;
    println!("diff:");
    print_diff(BROKEN, &fixed);
    assert_eq!(fixed, FIXED, "the edit tool must have applied the fix");
    let (passes_after, output_after) = run_tests(&dir);
    println!("after: tests pass={passes_after} output={output_after:?}");
    assert!(passes_after, "the repaired project must pass");

    assert!(!outcome.aborted);
    assert_eq!(outcome.turns, 4, "one model request per scripted reply");
    assert_eq!(outcome.tool_results.len(), 3, "read + edit + bash");
    assert!(
        outcome.usage.total_tokens > 0,
        "the terminal message carries usage"
    );

    agent.shutdown().await?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
