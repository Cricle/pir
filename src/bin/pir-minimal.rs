//! `pir-minimal`: the zero-optional-feature executable agent.
//!
//! Built by `cargo build --no-default-features` and needs no `cli`, `tui`,
//! `sdk`, `session`, `tools`, `extensions` or `provider-*` feature. It speaks
//! the OpenAI-compatible `/chat/completions` protocol to one endpoint, exposes
//! one `read_file` tool, keeps the transcript in memory and runs either one
//! prompt (`--prompt`) or a stdin REPL.
//!
//! ```text
//! cargo run --no-default-features --bin pir-minimal
//! cargo run --no-default-features --bin pir-minimal -- \
//!   --base-url http://127.0.0.1:11434/v1 --model llama3 --prompt "hello"
//! ```

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use pir::agent::agent_loop::AgentEvent;
use pir::ai::types::{AssistantMessageEvent, StopReason};
use pir::minimal::{config_from_env, message_text, MinimalAgent};

const USAGE: &str = "\
pir-minimal — the minimal, zero-optional-feature pir agent

USAGE:
    pir-minimal [OPTIONS] [PROMPT]

OPTIONS:
    -p, --prompt <TEXT>       Run one prompt and exit (otherwise: stdin REPL)
        --model <ID>          Model id (default: gpt-4o-mini)
        --base-url <URL>      OpenAI-compatible base URL (default: https://api.openai.com/v1)
        --api-key <KEY>       API key (default: PIR_MIN_API_KEY, then OPENAI_API_KEY)
        --system-prompt <T>   System prompt override
        --cwd <DIR>           Session working directory (default: current dir)
        --max-turns <N>       Maximum model turns per prompt
    -h, --help                Print this help
    -V, --version             Print the crate version

ENVIRONMENT:
    PIR_MIN_BASE_URL, PIR_MIN_API_KEY, PIR_MIN_MODEL, PIR_MIN_SYSTEM_PROMPT,
    PIR_MIN_CWD, OPENAI_API_KEY
";

struct Args {
    prompt: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    system_prompt: Option<String>,
    cwd: Option<PathBuf>,
    max_turns: Option<usize>,
}

fn parse_args() -> anyhow::Result<Option<Args>> {
    let mut args = Args {
        prompt: None,
        model: None,
        base_url: None,
        api_key: None,
        system_prompt: None,
        cwd: None,
        max_turns: None,
    };
    let mut positional: Vec<String> = Vec::new();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        let mut value = |name: &str| -> anyhow::Result<String> {
            iter.next()
                .ok_or_else(|| anyhow::anyhow!("missing value for {name}"))
        };
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("pir-minimal {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-p" | "--prompt" => args.prompt = Some(value("--prompt")?),
            "--model" => args.model = Some(value("--model")?),
            "--base-url" => args.base_url = Some(value("--base-url")?),
            "--api-key" => args.api_key = Some(value("--api-key")?),
            "--system-prompt" => args.system_prompt = Some(value("--system-prompt")?),
            "--cwd" => args.cwd = Some(PathBuf::from(value("--cwd")?)),
            "--max-turns" => {
                let raw = value("--max-turns")?;
                args.max_turns =
                    Some(raw.parse().map_err(|_| {
                        anyhow::anyhow!("--max-turns expects an integer, got {raw}")
                    })?);
            }
            other if other.starts_with('-') => {
                anyhow::bail!("unknown flag `{other}` (try --help)")
            }
            other => positional.push(other.to_owned()),
        }
    }
    if args.prompt.is_none() && !positional.is_empty() {
        args.prompt = Some(positional.join(" "));
    }
    Ok(Some(args))
}

fn event_printer(prefix: &'static str) -> pir::agent::agent_loop::EventHandler {
    Arc::new(move |event| match event {
        AgentEvent::StreamEvent {
            event: AssistantMessageEvent::TextDelta { delta, .. },
            ..
        } => {
            print!("{delta}");
            let _ = std::io::stdout().flush();
        }
        AgentEvent::ToolExecutionStart {
            event: pir::agent::tool_batch::ToolExecutionEvent::Start { tool_name, .. },
            ..
        } => eprintln!("\n{prefix}[tool: {tool_name}]"),
        _ => {}
    })
}

async fn run_prompt(
    agent: &MinimalAgent,
    prompt: &str,
    prefix: &'static str,
) -> anyhow::Result<()> {
    let message = agent
        .prompt_with_events(prompt, event_printer(prefix))
        .await?;
    if message_text(&message).is_empty() {
        // Tool-only terminal turns can legally end with no text; surface the
        // stop reason instead of printing nothing.
        eprintln!("\n{prefix}(no text; stop reason: {})", message.stop_reason);
    } else {
        println!();
    }
    if message.stop_reason == StopReason::Error {
        anyhow::bail!(
            "model request failed: {}",
            message.error_message.as_deref().unwrap_or("unknown error")
        );
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    // Windows gives the main thread a 1 MiB stack; the agent-session build and
    // the first prompt overflow it in debug builds (the same reason
    // `src/main.rs` runs the CLI on a big-stack worker). Keep one shared
    // budget with the full binary.
    let worker = std::thread::Builder::new()
        .name("pir-minimal".to_owned())
        .stack_size(pir::env_daemon::stack_budget_bytes(cfg!(debug_assertions)))
        .spawn(|| -> anyhow::Result<()> {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async_main())
        })?;
    match worker.join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

async fn async_main() -> anyhow::Result<()> {
    let Some(args) = parse_args()? else {
        return Ok(());
    };
    let config = config_from_env(
        args.base_url,
        args.api_key,
        args.model,
        args.system_prompt,
        args.cwd,
        args.max_turns,
    );
    let agent = MinimalAgent::build(config).await?;

    let result = match args.prompt {
        Some(prompt) => run_prompt(&agent, &prompt, "").await,
        None => {
            eprintln!(
                "pir-minimal: model {} at {} (Ctrl-D or `exit` to quit)",
                agent.model().id,
                agent.model().base_url
            );
            let stdin = std::io::stdin();
            let mut line = String::new();
            loop {
                print!("you> ");
                std::io::stdout().flush()?;
                line.clear();
                if stdin.lock().read_line(&mut line)? == 0 {
                    break;
                }
                let prompt = line.trim();
                if prompt.is_empty() {
                    continue;
                }
                if prompt == "exit" || prompt == "quit" {
                    break;
                }
                print!("pir> ");
                std::io::stdout().flush()?;
                if let Err(error) = run_prompt(&agent, prompt, "").await {
                    eprintln!("error: {error:#}");
                }
            }
            Ok(())
        }
    };

    agent.shutdown().await;
    result
}
