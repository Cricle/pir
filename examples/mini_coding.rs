//! A miniature coding assistant, end to end.
//!
//! The scripted model performs a small but real repair: it reads a broken
//! Python module, edits it with the built-in `edit` tool, runs the real test
//! suite with the built-in `bash` tool, and reports the result. The project
//! lives in a fresh temporary directory; the test runner is real (set
//! `PIR_PYTHON` if `python` is not on PATH).
//!
//! ```text
//! PIR_PYTHON=/path/to/python.exe \
//!   cargo run --example mini_coding --no-default-features --features "sdk,tools"
//! ```

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;

use pir::ai::types::ThinkingLevel;
use pir::sdk::{AgentBuilder, SdkError};

const BROKEN: &str = r#"def add(a, b):
    return a - b


def mul(a, b):
    return a * b
"#;

const FIXED: &str = r#"def add(a, b):
    return a + b


def mul(a, b):
    return a * b
"#;

const TEST: &str = r#"from calc import add, mul

assert add(2, 3) == 5, f"add(2, 3) = {add(2, 3)}"
assert mul(2, 3) == 6
print("all tests passed")
"#;

fn python_program() -> String {
    std::env::var("PIR_PYTHON").unwrap_or_else(|_| "python".to_owned())
}

fn run_tests(dir: &Path) -> (bool, String) {
    let output = Command::new(python_program())
        .arg("-B") // no __pycache__: same-mtime, same-size edits must rerun
        .arg("test_calc.py")
        .current_dir(dir)
        .output()
        .expect("spawn the Python test runner (set PIR_PYTHON if python is missing)");
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
    std::fs::write(dir.join("calc.py"), BROKEN)?;
    std::fs::write(dir.join("test_calc.py"), TEST)?;

    // 1. The project really fails before the assistant touches it.
    let (passes_before, output_before) = run_tests(&dir);
    println!("before: tests pass={passes_before} output={output_before:?}");
    assert!(!passes_before, "the fixture must start broken");

    // 2. Script the model: read → edit → run tests → summarize.
    let (registry, model) = common::ScriptedProvider::registry(vec![
        common::tool_calls_message(&[("read", json!({ "path": "calc.py" }))]),
        common::tool_calls_message(&[(
            "edit",
            json!({
                "path": "calc.py",
                "edits": [{
                    "oldText": "    return a - b",
                    "newText": "    return a + b"
                }]
            }),
        )]),
        common::tool_calls_message(&[(
            "bash",
            json!({ "command": format!("\"{}\" -B test_calc.py", python_program()) }),
        )]),
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
    let fixed = std::fs::read_to_string(dir.join("calc.py"))?;
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
