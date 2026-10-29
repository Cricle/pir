//! CLI argument parsing (upstream `packages/coding-agent/src/cli/args.ts`
//! subset), shared by the binary so integration tests can exercise the flag
//! surface directly.
//!
//! pir keeps its subcommand grammar (`pir print "..."`, `pir interactive`,
//! `pir rpc`); the upstream session/mode/context flags are global options so
//! they compose with every subcommand.

// The clap-derived argument surface (the `Cli` struct, its subcommand enums,
// and the `pir mcp` runtime that consumes them) is the `cli` feature. The
// dependency-free argv helpers below (mode validation, `@file` processing,
// unknown-flag collection) stay in every build: the print host and the
// extension loader call them.
#[cfg(feature = "cli")]
use clap::{Parser, Subcommand};
use std::collections::{BTreeMap, BTreeSet};

/// Output/run mode (upstream `Mode = "text" | "json" | "rpc"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Text,
    Json,
    Rpc,
}

/// Validate every `--mode`/`--mode=` occurrence in raw argv. Upstream
/// (`args.ts` diagnostics + `main.ts`) treats an invalid mode as a hard error
/// and exits 1; pir does the same via [`preprocess_args`].
pub fn validate_mode_arg(args: &[String]) -> Result<(), String> {
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            return Ok(());
        }
        if let Some(value) = arg.strip_prefix("--mode=") {
            validate_mode_value(value)?;
            index += 1;
            continue;
        }
        if arg == "--mode" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| "--mode requires a value (text, json, or rpc)".to_owned())?;
            validate_mode_value(value)?;
            index += 2;
            continue;
        }
        index += 1;
    }
    Ok(())
}

/// One `--mode` value: must be `text`, `json`, or `rpc` (upstream
/// `args.ts`: `Invalid mode "…". Valid values: text, json, rpc`).
fn validate_mode_value(value: &str) -> Result<(), String> {
    match value.trim() {
        "text" | "json" | "rpc" => Ok(()),
        other => Err(format!(
            "Invalid mode \"{other}\". Valid values: text, json, rpc"
        )),
    }
}

/// Parse the `--min-expiry` value into whole seconds for the bearer-token
/// credential check (upstream `parseAuthCommand` accepts
/// `<digits><ms|s|m|h>`, e.g. `30m` or `1h`; pir additionally accepts bare
/// seconds for backward compatibility). Sub-second results round up to one
/// second — the credential-freshness check works in whole seconds. Public
/// for tests/.
pub fn parse_min_expiry(value: &str) -> Result<u64, String> {
    let invalid = || {
        "--min-expiry must use a duration such as 30m or 1h (accepted forms: seconds, ms, s, m, h; e.g. 30000, 30s, 30m, 1h)"
            .to_string()
    };
    let trimmed = value.trim();
    let split = trimmed
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(trimmed.len());
    let (digits, unit) = trimmed.split_at(split);
    let amount: u64 = digits.parse().map_err(|_| invalid())?;
    let unit_ms: u128 = match unit {
        "" | "s" => 1_000,
        "ms" => 1,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return Err(invalid()),
    };
    let ms = u128::from(amount) * unit_ms;
    let seconds = ms.div_ceil(1_000);
    u64::try_from(seconds).map_err(|_| invalid())
}

/// Normalize the `--mode` value. An invalid value is a hard error (upstream
/// `args.ts` diagnostics; `main.ts` exits 1) — [`preprocess_args`] validates
/// argv before parsing, so the CLI never reaches the fallback here.
pub fn normalize_mode(value: Option<&str>) -> crate::Result<Option<Mode>> {
    match value.map(str::trim) {
        Some("text") => Ok(Some(Mode::Text)),
        Some("json") => Ok(Some(Mode::Json)),
        Some("rpc") => Ok(Some(Mode::Rpc)),
        Some(other) => Err(anyhow::anyhow!(
            "Invalid mode \"{other}\". Valid values: text, json, rpc"
        )),
        None => Ok(None),
    }
}

/// Convert one OS argv entry to a `String`, replacing invalid UTF-8 lossily.
/// `std::env::args` panics on non-UTF-8 argv; `env::args_os` plus this
/// conversion keeps such invocations running (matching the tolerant upstream
/// Node argv handling).
pub fn lossy_arg(arg: impl AsRef<std::ffi::OsStr>) -> String {
    arg.as_ref().to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// Upstream entry contract (main.ts `resolveAppMode` + `readPipedStdin` +
// `prepareInitialMessage`/file-processor.ts)
// ---------------------------------------------------------------------------

/// Resolved application mode (upstream `AppMode`: "print" | "json" | "rpc" |
/// "interactive").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    /// Non-interactive single execution (text output).
    Print,
    /// Non-interactive single execution with a JSON event stream.
    Json,
    /// RPC loop over stdin/stdout.
    Rpc,
    /// Interactive TUI.
    Interactive,
}

/// Resolve the application mode (upstream `resolveAppMode`, main.ts:112-123):
/// `--mode rpc`/`--mode json` win, then an explicit `--print`, then the
/// non-TTY auto-print rule (`stdin` or `stdout` is not a terminal), else
/// interactive.
pub fn resolve_app_mode(
    mode: Option<Mode>,
    print_flag: bool,
    stdin_is_tty: bool,
    stdout_is_tty: bool,
) -> AppMode {
    match mode {
        Some(Mode::Rpc) => AppMode::Rpc,
        Some(Mode::Json) => AppMode::Json,
        _ if print_flag || !stdin_is_tty || !stdout_is_tty => AppMode::Print,
        _ => AppMode::Interactive,
    }
}

/// Split positional arguments into messages and `@file` references
/// (upstream `parseArgs`: `@x` positionals become `fileArgs` with the `@`
/// prefix stripped).
pub fn split_at_args(positionals: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut messages = Vec::new();
    let mut file_args = Vec::new();
    for arg in positionals {
        // Upstream `arg.startsWith("@")` → `fileArgs.push(arg.slice(1))`:
        // every `@`-prefixed positional is a file reference, even when the
        // remainder is empty or whitespace (the file open then fails
        // downstream, exactly like upstream).
        match arg.strip_prefix('@') {
            Some(rest) => file_args.push(rest.to_string()),
            None => messages.push(arg),
        }
    }
    (messages, file_args)
}

/// The upstream entry contract for one invocation: the resolved app mode, the
/// positional message list, and the `@file` arguments (upstream `parseArgs` +
/// `resolveAppMode`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryPlan {
    pub app_mode: AppMode,
    /// Positional messages (never `@file` references).
    pub messages: Vec<String>,
    /// `@`-stripped file references (upstream `fileArgs`).
    pub file_args: Vec<String>,
}

#[cfg(feature = "cli")]
impl Cli {
    /// The resolved entry plan (see [`EntryPlan`]). The `--print` optional
    /// value joins the message list; a value that starts with `@` is a file
    /// argument instead (upstream `-p` never consumes an `@file` reference).
    pub fn entry_plan(&self, stdin_is_tty: bool, stdout_is_tty: bool) -> EntryPlan {
        let mut messages = self.messages.clone();
        let mut file_args: Vec<String> = Vec::new();
        if let Some(first) = self.print.clone() {
            if let Some(rest) = first.strip_prefix('@') {
                if !rest.is_empty() {
                    file_args.push(rest.to_string());
                }
            } else if !first.is_empty() {
                messages.insert(0, first);
            }
        }
        let (messages, more_files) = split_at_args(messages);
        file_args.extend(more_files);
        EntryPlan {
            app_mode: resolve_app_mode(
                self.mode(),
                self.print.is_some(),
                stdin_is_tty,
                stdout_is_tty,
            ),
            messages,
            file_args,
        }
    }

    /// Whether this invocation is a runtime METADATA command: `--list-models`,
    /// `--export` or `--help-extensions` (upstream
    /// `isPlainRuntimeMetadataCommand`, main.ts:129-131, plus the
    /// `parsed.export`/`parsed.help` branches that exit before a mode starts).
    /// These commands read the model/session/extension stores but never need a
    /// project trust selector to EXECUTE.
    pub fn is_metadata_command(&self) -> bool {
        self.list_models.is_some() || self.export.is_some() || self.help_extensions
    }

    /// Whether the interactive project-trust selector may run for this
    /// invocation (upstream `trustPromptMode`, main.ts:726 + 773): a metadata
    /// command resolves trust through the stored/default decision even on a
    /// TTY, so it never blocks on the selector before printing its output;
    /// every other interactive run keeps the prompt.
    pub fn trust_prompt_has_ui(&self, interactive: bool) -> bool {
        interactive && !self.is_metadata_command()
    }
}

/// Maximum bytes read from piped stdin. Upstream `readPipedStdin` is
/// unbounded; pir bounds the read to 10 MiB.
pub const PIPED_STDIN_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Read the piped-stdin prompt (upstream `readPipedStdin`): `None` when the
/// input is a TTY (interactive terminal) or the content is empty after
/// trimming; the read is capped at [`PIPED_STDIN_MAX_BYTES`].
pub fn read_piped_input(is_tty: bool, reader: impl std::io::Read) -> Option<String> {
    use std::io::Read;
    if is_tty {
        return None;
    }
    let mut limited = reader.take(PIPED_STDIN_MAX_BYTES);
    let mut buf = Vec::new();
    limited.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Upstream main.ts:659: `@file` arguments combined with RPC mode are a hard
/// error (exit 1).
pub fn validate_rpc_file_args(app_mode: AppMode, file_args: &[String]) -> Result<(), String> {
    if app_mode == AppMode::Rpc && !file_args.is_empty() {
        return Err("@file arguments are not supported in RPC mode".to_owned());
    }
    Ok(())
}

/// Upstream main.ts post-parse validation, all hard errors (exit 1):
/// `--provider` without `--model` (main.ts:469-474), an empty `--name`
/// (main.ts:709-716), and `--session-id` id validation (main.ts:315-336 via
/// [`crate::session_manager::assert_valid_session_id`]).
#[cfg(feature = "cli")]
pub fn validate_parsed_flags(cli: &Cli) -> Result<(), String> {
    // `auth` subcommands carry their OWN `--provider` (clap's global flag
    // still captures it), so the run-mode rule must not fire for them.
    let provider_is_run_flag = !matches!(cli.command, Some(Command::Auth { .. }));
    if provider_is_run_flag && cli.provider.is_some() && cli.model.is_none() {
        return Err(
            "--provider requires --model (for example: --provider <provider> --model <model>)"
                .to_owned(),
        );
    }
    if let Some(name) = &cli.name {
        if name.trim().is_empty() {
            return Err("--name requires a non-empty value".to_owned());
        }
    }
    if let Some(id) = &cli.session_id {
        crate::session_manager::assert_valid_session_id(id).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// The prompt set for one print run (upstream `buildInitialMessage` +
/// `runPrintMode`): the combined initial prompt plus the remaining positional
/// messages, each sent as its own follow-up prompt.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrintPrompts {
    /// stdin content, `@file` text, and the first message joined with no
    /// separator (upstream `parts.join("")`); `None` when every part is empty.
    pub initial_message: Option<String>,
    /// The remaining positional messages, in order (upstream shifts the first
    /// message out of `parsed.messages` and prompts each leftover).
    pub additional_messages: Vec<String>,
}

/// Build the print prompt set (upstream `buildInitialMessage`): the initial
/// prompt is the stdin content, the `@file` text, and the FIRST positional
/// message joined with no separator; every remaining message is prompted
/// separately, in order, by the print host.
pub fn build_print_prompts(
    stdin_content: Option<&str>,
    file_text: &str,
    messages: &[String],
) -> PrintPrompts {
    let mut parts: Vec<String> = Vec::new();
    if let Some(content) = stdin_content {
        parts.push(content.to_string());
    }
    if !file_text.is_empty() {
        parts.push(file_text.to_string());
    }
    let mut rest = messages;
    if let Some((first, tail)) = messages.split_first() {
        // Upstream only shifts when a first message exists, and only a
        // non-empty join becomes the initial message.
        parts.push(first.clone());
        rest = tail;
    }
    let joined = parts.join("");
    PrintPrompts {
        initial_message: (!joined.is_empty()).then_some(joined),
        additional_messages: rest.to_vec(),
    }
}

/// Strip a leading UTF-8 BOM (upstream `stripBom`).
fn strip_bom(content: &str) -> &str {
    content.strip_prefix('\u{feff}').unwrap_or(content)
}

/// Expand a leading `~`/`~/` against `$HOME`/`$USERPROFILE` (upstream
/// `resolveReadPath`).
fn expand_tilde(path: &str) -> std::path::PathBuf {
    if path == "~" || path.starts_with("~/") || path.starts_with("~\\") {
        if let Some(home) = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(std::path::PathBuf::from)
        {
            return home.join(path.trim_start_matches('~').trim_start_matches(['/', '\\']));
        }
    }
    std::path::PathBuf::from(path)
}

/// Process `@file` arguments into a `<file name="…">…</file>` text block
/// (upstream `cli/file-processor.ts`; text files only — image `@file`
/// arguments are split out beforehand by the print path's
/// [`crate::coding_agent::print::partition_file_args`]). Missing and
/// unreadable files are hard errors (upstream exits 1); empty files are
/// skipped.
pub fn process_file_arguments(
    file_args: &[String],
    cwd: &std::path::Path,
) -> crate::Result<String> {
    let mut text = String::new();
    for file_arg in file_args {
        let path = expand_tilde(file_arg);
        let absolute = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        if !absolute.exists() {
            return Err(anyhow::anyhow!("File not found: {}", absolute.display()));
        }
        let metadata = absolute.metadata().map_err(|error| {
            anyhow::anyhow!("Could not stat file {}: {error}", absolute.display())
        })?;
        if metadata.len() == 0 {
            // Empty files are skipped (upstream `processFileArguments`).
            continue;
        }
        let bytes = std::fs::read(&absolute).map_err(|error| {
            anyhow::anyhow!("Could not read file {}: {error}", absolute.display())
        })?;
        let raw = String::from_utf8_lossy(&bytes).into_owned();
        let content = strip_bom(&raw);
        text.push_str(&format!(
            "<file name=\"{}\">\n{content}\n</file>\n",
            absolute.display()
        ));
    }
    Ok(text)
}

/// The trailing `after_help` block of `pir --help`: the upstream
/// extension-flag note (`cli/args.ts:348`) plus the `Environment Variables:`
/// section (`cli/args.ts:418-467`). Every upstream entry is listed: the
/// provider API/endpoint keys are resolved by the provider adapters and
/// [`crate::ai::auth::env_api_keys::all`], the Bedrock chain by
/// `ai::api::bedrock::credentials`, and the `PI_*` directory/offline/telemetry/
/// share overrides by `config::env` and the TUI. pir-native aliases and the
/// extra provider keys it recognizes are inserted next to their families.
///
/// `after_help` (not `after_long_help`) keeps the section in both the long
/// (`--help`) and short (`-h`) help.
#[cfg(feature = "cli")]
const ENVIRONMENT_VARIABLES_HELP: &str = "\
Extensions can register additional flags (e.g., --plan from plan-mode extension).
Run `pir --help-extensions` to list the flags registered by the loaded extensions
(it loads them, so --help itself never executes extension code).

Environment Variables:
  ANTHROPIC_AUTH_TOKEN             - Anthropic bearer auth token
  ANTHROPIC_API_KEY                - Anthropic Claude API key
  ANTHROPIC_OAUTH_TOKEN            - Anthropic OAuth token (alternative to API key)
  ANT_LING_API_KEY                 - Ant Ling API key
  OPENAI_API_KEY                   - OpenAI GPT API key
  AZURE_OPENAI_API_KEY             - Azure OpenAI API key
  AZURE_OPENAI_BASE_URL            - Azure OpenAI base URL (https://{resource}.openai.azure.com)
  AZURE_OPENAI_RESOURCE_NAME       - Azure OpenAI resource name (alternative to base URL)
  AZURE_OPENAI_API_VERSION         - Azure OpenAI API version (default: v1)
  AZURE_OPENAI_DEPLOYMENT_NAME_MAP - Azure OpenAI model=deployment map (comma-separated)
  DEEPSEEK_API_KEY                 - DeepSeek API key
  NVIDIA_API_KEY                   - NVIDIA NIM API key
  HF_TOKEN                         - Hugging Face token
  GEMINI_API_KEY                   - Google Gemini API key
  GOOGLE_API_KEY                   - Google Gemini API key (legacy alias)
  GOOGLE_CLOUD_API_KEY             - Google Vertex API key
  GROQ_API_KEY                     - Groq API key
  CEREBRAS_API_KEY                 - Cerebras API key
  XAI_API_KEY                      - xAI Grok API key
  FIREWORKS_API_KEY                - Fireworks API key
  TOGETHER_API_KEY                 - Together AI API key
  BASETEN_API_KEY                  - Baseten API key
  OPENROUTER_API_KEY               - OpenRouter API key
  AI_GATEWAY_API_KEY               - Vercel AI Gateway API key
  TYPESAFE_API_KEY                 - Typesafe API key
  RADIUS_API_KEY                   - Radius API key
  ZAI_API_KEY                      - ZAI Coding Plan API key (Global)
  ZAI_CODING_CN_API_KEY            - ZAI Coding Plan API key (China)
  MISTRAL_API_KEY                  - Mistral API key
  MINIMAX_API_KEY                  - MiniMax API key
  MINIMAX_CN_API_KEY               - MiniMax API key (China)
  MOONSHOT_API_KEY                 - Moonshot AI API key
  OPENCODE_API_KEY                 - OpenCode Zen/OpenCode Go API key
  COPILOT_GITHUB_TOKEN             - GitHub Copilot token
  KIMI_API_KEY                     - Kimi For Coding API key
  META_API_KEY                     - Meta Model API key
  CLOUDFLARE_API_KEY               - Cloudflare API token (Workers AI and AI Gateway)
  CLOUDFLARE_ACCOUNT_ID            - Cloudflare account id (required for both)
  CLOUDFLARE_GATEWAY_ID            - Cloudflare AI Gateway slug (required for AI Gateway)
  QWEN_TOKEN_PLAN_API_KEY          - Qwen Token Plan API key (international region)
  QWEN_TOKEN_PLAN_CN_API_KEY       - Qwen Token Plan API key (China region)
  XIAOMI_API_KEY                   - Xiaomi MiMo API key (api.xiaomimimo.com billing)
  XIAOMI_TOKEN_PLAN_CN_API_KEY     - Xiaomi MiMo Token Plan API key (China region)
  XIAOMI_TOKEN_PLAN_AMS_API_KEY    - Xiaomi MiMo Token Plan API key (Amsterdam region)
  XIAOMI_TOKEN_PLAN_SGP_API_KEY    - Xiaomi MiMo Token Plan API key (Singapore region)
  AWS_PROFILE                      - AWS profile for Amazon Bedrock
  AWS_ACCESS_KEY_ID                - AWS access key for Amazon Bedrock
  AWS_SECRET_ACCESS_KEY            - AWS secret key for Amazon Bedrock
  AWS_BEARER_TOKEN_BEDROCK         - Bedrock API key (bearer token)
  AWS_REGION                       - AWS region for Amazon Bedrock (e.g., us-east-1)
  PI_CODING_AGENT_DIR              - Config directory (default: ~/.pi/agent; PIR_AGENT_DIR alias)
  PI_CODING_AGENT_SESSION_DIR      - Session storage directory (overridden by --session-dir)
  PI_PACKAGE_DIR                   - Override package directory (for Nix/Guix store paths)
  PI_OFFLINE                       - Disable startup network operations when set to 1/true/yes
  PI_SKIP_VERSION_CHECK            - Disable the startup version check when set to 1/true/yes
  PI_TELEMETRY                     - Override install telemetry when set to 1/true/yes or 0/false/no
  PI_SHARE_VIEWER_URL              - Base URL for /share command (default: https://pi.dev/session/)
  PIR_TOKIO_WORKERS                - Main tokio runtime worker threads (1-64; default: one per core)
  PIR_TOKIO_MAX_BLOCKING           - Main tokio blocking-pool ceiling (1-512; default: 2 per core, min 4, max 32)
  PIR_ENV_DAEMON_WORKERS           - `pir env-daemon` file-op worker threads (1-64)
  PIR_ENV_DAEMON_MAX_LONG_REQUESTS - Concurrent exec/watch/pty requests per env-daemon connection (1-256; default: 32)
";

#[cfg(feature = "cli")]
#[derive(Parser)]
#[command(
    name = "pir",
    version,
    about = "Pi coding agent in Rust",
    after_help = ENVIRONMENT_VARIABLES_HELP
)]
pub struct Cli {
    /// Extension file or directory to load (repeatable); `builtin:<name>`
    /// loads a compiled builtin plugin
    #[arg(short = 'e', long = "extension", value_name = "PATH", global = true)]
    pub extension: Vec<std::path::PathBuf>,

    /// Output mode: text (default), json, or rpc (upstream `--mode`)
    #[arg(long = "mode", value_name = "MODE", global = true)]
    pub mode: Option<String>,

    /// Continue the most recent session for the current cwd (upstream `--continue, -c`)
    #[arg(short = 'c', long = "continue", global = true)]
    pub continue_session: bool,

    /// Select a session to resume (upstream `--resume, -r`)
    #[arg(short = 'r', long = "resume", global = true)]
    pub resume: bool,

    /// Use a specific session file or partial UUID (upstream `--session`)
    #[arg(long = "session", value_name = "ID_OR_PATH", global = true)]
    pub session: Option<String>,

    /// Use an exact project session id, creating it when missing (upstream `--session-id`)
    #[arg(long = "session-id", value_name = "UUID", global = true)]
    pub session_id: Option<String>,

    /// Fork a session file or partial UUID into a new session (upstream `--fork`)
    #[arg(long = "fork", value_name = "ID_OR_PATH", global = true)]
    pub fork: Option<String>,

    /// Don't save the session; ephemeral run (upstream `--no-session`)
    #[arg(long = "no-session", global = true)]
    pub no_session: bool,

    /// Disable AGENTS.md and CLAUDE.md discovery and loading (upstream
    /// `--no-context-files, -nc`)
    #[arg(long = "no-context-files", global = true)]
    pub no_context_files: bool,

    /// Provider to search for `--model` (upstream `--provider`; requires `--model`)
    #[arg(long = "provider", value_name = "NAME", global = true)]
    pub provider: Option<String>,

    /// Model pattern or id, `provider/id` or `pattern:thinking` (upstream `--model`)
    #[arg(long = "model", value_name = "PATTERN", global = true)]
    pub model: Option<String>,

    /// API key for the selected provider (upstream `--api-key`)
    #[arg(long = "api-key", value_name = "KEY", global = true)]
    pub api_key: Option<String>,

    /// Replace the system prompt (upstream `--system-prompt`)
    #[arg(long = "system-prompt", value_name = "TEXT", global = true)]
    pub system_prompt: Option<String>,

    /// Append text or file contents to the system prompt (repeatable,
    /// upstream `--append-system-prompt`)
    #[arg(
        long = "append-system-prompt",
        value_name = "TEXT_OR_FILE",
        global = true
    )]
    pub append_system_prompt: Vec<String>,

    /// Set the thinking level: off, minimal, low, medium, high, xhigh, max
    /// (upstream `--thinking`; takes precedence over the `--model` suffix)
    #[arg(long = "thinking", value_name = "LEVEL", global = true)]
    pub thinking: Option<String>,

    /// Directory for session storage and lookup (upstream `--session-dir`)
    #[arg(long = "session-dir", value_name = "DIR", global = true)]
    pub session_dir: Option<String>,

    /// Set the session display name (upstream `--name, -n`)
    #[arg(long = "name", value_name = "NAME", global = true)]
    pub name: Option<String>,

    /// Comma-separated model patterns for cycling (upstream `--models`).
    ///
    /// Not `global`: the `update` subcommand reuses the `--models` long for
    /// its own bool flag (upstream `pi update --models`), and a propagated
    /// global would collide with it (clap requires unique longs per command).
    #[arg(
        long = "models",
        value_name = "PATTERNS",
        help = "Comma-separated model patterns for cycling (upstream `--models`)",
        long_help = "Comma-separated model patterns for cycling (upstream `--models`)"
    )]
    pub models: Option<String>,

    /// Disable all tools by default (upstream `--no-tools, -nt`)
    #[arg(long = "no-tools", global = true)]
    pub no_tools: bool,

    /// Disable built-in tools by default but keep extension/custom tools
    /// (upstream `--no-builtin-tools, -nbt`)
    #[arg(long = "no-builtin-tools", global = true)]
    pub no_builtin_tools: bool,

    /// Comma-separated allowlist of tool names or patterns (*) to enable;
    /// pure `+name`/`-name` modifier lists add to or remove from the defaults
    /// (upstream `--tools, -t`)
    #[arg(long = "tools", value_name = "TOOLS", global = true)]
    pub tools: Option<String>,

    /// Comma-separated denylist of tool names or patterns (*) to disable,
    /// MCP tools included (upstream `--exclude-tools, -xt`)
    #[arg(long = "exclude-tools", value_name = "TOOLS", global = true)]
    pub exclude_tools: Option<String>,

    /// Disable settings/default extension discovery and built-in extensions;
    /// explicit `-e` paths (including `builtin:<name>`) still load (upstream
    /// `--no-extensions, -ne`)
    #[arg(long = "no-extensions", alias = "ne", global = true)]
    pub no_extensions: bool,

    /// Load ONLY the Rust builtin plugins (subagent, mcp, codemode, python,
    /// context-mode, tool-search, tool-repair, todo): no JS extension files,
    /// no installed packages, no settings/default discovery. An explicit
    /// `-e builtin:<name>` still works (`-e <file>` is ignored). Combined
    /// with `--no-extensions`, `--no-extensions` wins.
    #[arg(long = "only-builtin", global = true)]
    pub only_builtin: bool,

    /// Disable the built-in MCP extension (same as
    /// `disabledBuiltinExtensions: ["mcp"]`; upstream `--no-mcp`)
    #[arg(long = "no-mcp", global = true)]
    pub no_mcp: bool,

    /// Load a skill file or directory (repeatable, upstream `--skill`)
    #[arg(long = "skill", value_name = "PATH", global = true)]
    pub skill: Vec<std::path::PathBuf>,

    /// Disable skills discovery and loading (upstream `--no-skills, -ns`)
    #[arg(long = "no-skills", global = true)]
    pub no_skills: bool,

    /// Load a prompt template file or directory (repeatable, upstream `--prompt-template`)
    #[arg(long = "prompt-template", value_name = "PATH", global = true)]
    pub prompt_template: Vec<std::path::PathBuf>,

    /// Disable prompt template discovery (upstream `--no-prompt-templates, -np`)
    #[arg(long = "no-prompt-templates", global = true)]
    pub no_prompt_templates: bool,

    /// Load a theme file or directory (repeatable, upstream `--theme`)
    #[arg(long = "theme", value_name = "PATH", global = true)]
    pub theme: Vec<std::path::PathBuf>,

    /// Set the initial interactive theme for this run (upstream `--use-theme`)
    #[arg(long = "use-theme", value_name = "NAME", global = true)]
    pub use_theme: Option<String>,

    /// Disable theme discovery and loading (upstream `--no-themes`)
    #[arg(long = "no-themes", global = true)]
    pub no_themes: bool,

    /// List available models, with an optional fuzzy search (upstream `--list-models [search]`)
    #[arg(
        long = "list-models",
        value_name = "SEARCH",
        num_args = 0..=1,
        default_missing_value = "",
        global = true
    )]
    pub list_models: Option<String>,

    /// Export a session to a self-contained HTML file and exit; the optional
    /// second value is the output path (upstream `--export <file> [output]`)
    #[arg(
        long = "export",
        value_name = "SESSION_FILE [OUTPUT]",
        num_args = 1..=2
    )]
    pub export: Option<Vec<String>>,

    /// Force verbose startup (upstream `--verbose`)
    #[arg(long = "verbose", global = true)]
    pub verbose: bool,

    /// TUI mode: fullscreen (default) or regular (upstream `--tui-mode`)
    #[arg(long = "tui-mode", value_name = "MODE", global = true)]
    pub tui_mode: Option<String>,

    /// Trust project-local files for this run (upstream `--approve, -a`)
    #[arg(long = "approve", short = 'a', global = true)]
    pub approve: bool,

    /// Ignore project-local files for this run (upstream `--no-approve, -na`)
    #[arg(long = "no-approve", global = true)]
    pub no_approve: bool,

    /// List CLI flags registered by the loaded extensions and exit (upstream
    /// `Extension CLI Flags:` section of `printHelp`). Loads extensions, so it
    /// is opt-in: `--help` itself never executes extension code.
    #[arg(long = "help-extensions", global = true)]
    pub help_extensions: bool,

    /// Disable startup network operations (upstream `--offline`; same as PI_OFFLINE=1)
    #[arg(long = "offline", global = true)]
    pub offline: bool,

    /// Unknown `--flags` collected for extensions (populated by
    /// [`preprocess_args`], upstream `unknownFlags`)
    #[arg(skip)]
    pub unknown_flags: BTreeMap<String, String>,

    /// Non-interactive print mode: process the prompt (the optional value,
    /// positional messages, or piped stdin) and exit; an `@file` value is read
    /// as a file reference (upstream `--print, -p`)
    #[arg(
        short = 'p',
        long = "print",
        value_name = "MESSAGE",
        num_args = 0..=1,
        default_missing_value = "",
        global = true
    )]
    pub print: Option<String>,

    /// Prompt message(s): with `--print`, a non-TTY stdin/stdout, or piped
    /// stdin they form the print prompt; otherwise they start the interactive
    /// session. `@file` entries are read as file references.
    #[arg(value_name = "MESSAGE")]
    pub messages: Vec<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[cfg(feature = "cli")]
#[derive(Debug, Clone, Subcommand)]
// clap subcommand enums hold their sub-enums inline by design; boxing them
// would complicate the derive for no real gain (the whole enum lives in one
// CLI struct).
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// Interactive TUI agent mode
    Interactive,
    /// Non-interactive single execution mode
    Print {
        /// Prompt to execute
        prompt: String,
    },
    /// RPC mode (JSON lines over stdin/stdout)
    Rpc,
    /// Remote session server
    Server {
        /// Bind a Unix-domain socket at this path instead of TCP (upstream
        /// unix transport preset; Unix only)
        #[arg(long = "unix")]
        unix: Option<String>,
    },
    /// Connect to a remote server and print its session count
    Client,
    /// Show API key and OAuth configuration status
    Auth {
        #[command(subcommand)]
        command: Option<AuthCommand>,
    },
    /// Show the configured model
    Models {
        #[command(subcommand)]
        command: Option<ModelsCommand>,
    },
    /// Show session storage status
    Session,
    /// MCP server management (upstream `pi mcp` surface)
    Mcp {
        #[command(subcommand)]
        command: Option<McpCommand>,
    },
    /// Install a package source and record it in settings (upstream
    /// `pi install <source> [-l]`)
    Install {
        /// Package source: a git URL (`git:host/path`, `https://…`, `ssh://…`),
        /// an `npm:<name>[@version]` registry package, or a local path
        source: String,
        /// Install project-locally (<cwd>/.pi/settings.json)
        #[arg(short = 'l', long = "local")]
        local: bool,
    },
    /// Remove a package source from settings (upstream `pi remove <source> [-l]`)
    Remove {
        /// Package source previously installed
        source: String,
        /// Remove from project settings (<cwd>/.pi/settings.json)
        #[arg(short = 'l', long = "local")]
        local: bool,
    },
    /// Alias for remove (upstream `pi uninstall <source> [-l]`)
    Uninstall {
        /// Package source previously installed
        source: String,
        /// Remove from project settings (<cwd>/.pi/settings.json)
        #[arg(short = 'l', long = "local")]
        local: bool,
    },
    /// Update installed packages (upstream `pi update [source|self|pi]`;
    /// self-updating the Rust binary is not supported and fails honestly)
    #[command(
        after_help = "Upstream option spelling:\n  --extension <source>    Update one extension source (same as `pir update <source>`)"
    )]
    Update {
        /// Package source to refresh, or `self`/`pi` (default: self)
        source: Option<String>,
        /// Refresh the model catalogs (upstream `pi update --models`)
        #[arg(long = "models")]
        models: bool,
        /// Update all installed packages (upstream `pi update --extensions`;
        /// may combine with a `self`/`pi` positional to mean `--all`)
        #[arg(long = "extensions")]
        extensions: bool,
        /// Update pi itself and all installed packages (upstream
        /// `pi update --all`; conflicts with every other target selection)
        #[arg(long = "all")]
        all: bool,
        /// Update pi itself (upstream `pi update --self`)
        #[arg(long = "self")]
        self_update: bool,
        /// Update one extension source (upstream `pi update --extension
        /// <source>`); [`preprocess_args`] rewrites the upstream spelling to
        /// this long because the global `-e/--extension` already owns
        /// `--extension`
        #[arg(long = "update-extension", value_name = "SOURCE", hide = true)]
        update_extension: Option<String>,
        /// Re-install the pi package itself even when it is up to date
        /// (upstream `--force`). pir cannot self-update, so a target that
        /// includes self still fails with the honest self-update error
        #[arg(long = "force")]
        force: bool,
    },
    /// List installed packages from user and project settings (upstream `pi list`)
    List,
    /// Configuration management: the package/resource listing (upstream
    /// `pi config [-l]`; the interactive selector is TUI-only)
    Config {
        /// Show the project-scope configuration (<cwd>/.pi/settings.json)
        #[arg(short = 'l', long = "local")]
        local: bool,
        /// Print the resource configuration as JSON (non-interactive)
        #[arg(long = "json")]
        json: bool,
        /// Enable one package resource: `<kind>:<pattern>` (for example
        /// `extensions:extensions/foo.js`)
        #[arg(long = "enable", value_name = "KIND:PATTERN")]
        enable: Option<String>,
        /// Disable one package resource (same selector shape as `--enable`)
        #[arg(long = "disable", value_name = "KIND:PATTERN")]
        disable: Option<String>,
        /// Restrict an `--enable`/`--disable` write to one package source
        /// (needed when several packages provide the same pattern)
        #[arg(long = "source", value_name = "PACKAGE")]
        source: Option<String>,
    },
    /// Native `grep` (same engine as the grep tool); unknown flags exec the
    /// system `grep` with the original argv
    Grep {
        /// Arguments forwarded verbatim (GNU grep syntax)
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },
    /// Native `sed` (the supported subset); unsupported syntax execs the
    /// system `sed` with the original argv
    Sed {
        /// Arguments forwarded verbatim (GNU sed syntax)
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },
    /// Native `python` (the embedded RustPython 3); unsupported flags exec
    /// the system `python3` with the original argv
    Python {
        /// Arguments forwarded verbatim (CPython syntax)
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },
    /// Show version information
    Version,
}

/// The parsed `pir update` request (upstream `UpdateTarget` subset): which
/// parts of the installation the command refreshes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateRequest {
    /// `pir update` / `pir update self|pi`: upstream updates the pi package
    /// itself; pir reports that the Rust binary cannot self-update.
    SelfUpdate,
    /// `pir update <source>`: refresh one package source.
    Package(String),
    /// `pir update --extensions`: refresh all installed packages (upstream
    /// `updateTarget { type: "extensions" }`).
    Extensions,
    /// `pir update --extension <source>`: refresh one extension source
    /// (upstream `updateTarget { type: "extensions", source }`, identical to
    /// the positional form).
    Extension(String),
    /// `pir update --models`: refresh the model catalogs (upstream
    /// `updateTarget { type: "models" }`).
    Models,
    /// `pir update --all` (and `pir update self --extensions`): upstream
    /// updates pi itself and all installed packages.
    All,
}

/// One fully resolved `pir update` invocation (upstream `parsePackageCommand`'s
/// `updateTarget` + `force` + `showExtensionsSkippedNote`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePlan {
    /// The target selection.
    pub request: UpdateRequest,
    /// Whether the self-update should be forced (upstream `--force`).
    pub force: bool,
    /// Bare `pir update` prints the skipped-extensions note
    /// (upstream `showExtensionsSkippedNote`).
    pub show_extensions_skipped_note: bool,
}

/// The `pir mcp` subcommands (upstream `pi mcp`,
/// `extensions/mcp/cli.ts`: `add`/`remove` edit `mcp.json`; `check` tests
/// connectivity; `login`/`logout` run and clear the OAuth sign-in).
#[cfg(feature = "cli")]
#[derive(Debug, Clone, Subcommand)]
// clap subcommand enums hold their fields inline by design (see the allow on
// [`Command`]).
#[allow(clippy::large_enum_variant)]
pub enum McpCommand {
    /// Add or replace an MCP server (--url <url> or -- <command> [args...])
    Add {
        /// Server name (letters, digits, "_" and "-")
        name: String,
        /// Streamable HTTP server URL (instead of a `--` command)
        #[arg(long = "url", value_name = "URL")]
        url: Option<String>,
        /// The stdio server command and arguments, after `--`
        #[arg(last = true, value_name = "COMMAND [ARGS...]")]
        command: Vec<String>,
        /// Environment variable for a stdio server (repeatable)
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// Working directory for a stdio server
        #[arg(long = "cwd", value_name = "DIR")]
        cwd: Option<String>,
        /// HTTP header (repeatable)
        #[arg(long = "header", value_name = "KEY=VALUE")]
        header: Vec<String>,
        /// Send "Authorization: Bearer ${NAME}"
        #[arg(long = "bearer-token-env-var", value_name = "NAME")]
        bearer_token_env_var: Option<String>,
        /// Pre-registered OAuth client id
        #[arg(long = "oauth-client-id", value_name = "ID")]
        oauth_client_id: Option<String>,
        /// OAuth client secret (may be ${NAME} or !command)
        #[arg(long = "oauth-client-secret", value_name = "SECRET")]
        oauth_client_secret: Option<String>,
        /// Fixed OAuth callback port
        #[arg(long = "oauth-callback-port", value_name = "PORT")]
        oauth_callback_port: Option<u16>,
        /// Client name sent when registering with the OAuth server
        #[arg(long = "oauth-client-name", value_name = "NAME")]
        oauth_client_name: Option<String>,
        /// Tool exposure: codemode (default), deferred, direct, or hidden
        #[arg(long = "exposure", value_name = "MODE")]
        exposure: Option<String>,
        /// What the server offers, shown in the system prompt
        #[arg(long = "description", value_name = "TEXT")]
        description: Option<String>,
        /// Use <cwd>/.pi/mcp.json instead of the global file
        #[arg(short = 'l', long = "local")]
        local: bool,
    },
    /// Remove a server from mcp.json (upstream `pi mcp remove <server> [-l]`)
    Remove {
        /// Server name from mcp.json
        name: String,
        /// Remove from <cwd>/.pi/mcp.json instead of the global file
        #[arg(short = 'l', long = "local")]
        local: bool,
    },
    /// Check configured servers by test-connecting each with a bounded
    /// timeout (default when no subcommand is given; upstream `pi mcp`'s
    /// default check behavior)
    Check {
        /// Only check this server
        server: Option<String>,
        /// Emit JSON status
        #[arg(long = "json")]
        json: bool,
    },
    /// List configured servers with their scope, exposure, connection state
    /// and errors (upstream `pi mcp list`; exits non-zero when a server fails
    /// to connect or a config file is invalid)
    List {
        /// Emit JSON status (`{servers, errors, note?}`)
        #[arg(long = "json")]
        json: bool,
    },
    /// Sign in to an OAuth MCP server through the browser (upstream
    /// `pi mcp login <server> [--timeout <seconds>]`)
    Login {
        /// Server name from mcp.json
        server: String,
        /// How long login waits for the browser (default: 300)
        #[arg(long = "timeout", default_value_t = 300)]
        timeout_seconds: u64,
    },
    /// Delete the stored OAuth credentials of a server (upstream
    /// `pi mcp logout <server>`)
    Logout {
        /// Server name from mcp.json
        server: String,
    },
}

#[cfg(feature = "cli")]
#[derive(Debug, Clone, Subcommand)]
pub enum ModelsCommand {
    /// Show the configured model
    List,
}

/// The `pir auth` subcommands (upstream `cli/auth-command.ts` surface:
/// `auth check`, `auth print-api-key`; login/logout mirror the `/login`
/// `/logout` slash-command orchestration).
#[cfg(feature = "cli")]
#[derive(Debug, Clone, Subcommand)]
pub enum AuthCommand {
    /// Show per-provider auth status (default when no subcommand is given)
    Status,
    /// Run the provider's OAuth login flow and store the credential
    Login {
        /// Provider id (e.g. anthropic, openai, openai-codex, github-copilot,
        /// openrouter, kimi-coding, xai, meta, radius)
        provider: String,
        /// Agent name used in the login flow (upstream `LoginOptions.agentName`)
        #[arg(long = "agent-name", value_name = "NAME")]
        agent_name: Option<String>,
    },
    /// Remove the stored credential for a provider
    Logout {
        /// Provider id
        provider: String,
    },
    /// Check whether a provider is configured (upstream `pi auth check`:
    /// `--provider <provider>` or `--model <model>` resolves the provider)
    Check {
        /// Provider id (`--provider <provider>`)
        #[arg(long = "provider", value_name = "PROVIDER")]
        provider: Option<String>,
        /// Model pattern or id whose provider is checked (upstream
        /// `--model <model>`); one of `--provider`/`--model` is required
        #[arg(long = "model", value_name = "MODEL")]
        model: Option<String>,
        /// Emit JSON status (upstream `--json`)
        #[arg(long = "json")]
        json: bool,
        /// Include the credential in the output (upstream `--credentials`)
        #[arg(long = "credentials")]
        credentials: bool,
        /// Do not refresh an expired OAuth credential (upstream `--no-refresh`)
        #[arg(long = "no-refresh")]
        no_refresh: bool,
    },
    /// Print the provider's resolved credential (upstream `pi auth
    /// print-api-key [--provider <provider>] [--model <model>]`)
    PrintApiKey {
        /// Provider id (`--provider <provider>`)
        #[arg(long = "provider", value_name = "PROVIDER")]
        provider: Option<String>,
        /// Model pattern or id whose provider's credential is printed
        /// (upstream `--model <model>`)
        #[arg(long = "model", value_name = "MODEL")]
        model: Option<String>,
    },
    /// Print the provider's OAuth bearer token, refreshing when expired
    /// (upstream `pi auth print-bearer-token [--provider <provider>]
    /// [--model <model>]`)
    PrintBearerToken {
        /// Provider id (upstream `--provider <provider>`); one of
        /// `--provider`/`--model` is required
        #[arg(long = "provider", value_name = "PROVIDER")]
        provider: Option<String>,
        /// Model id the token is issued for (upstream `--model <model>`)
        #[arg(long = "model", value_name = "MODEL")]
        model: Option<String>,
        /// Minimum remaining validity (upstream `--min-expiry <duration>`):
        /// bare seconds or `<digits>` with an `ms`/`s`/`m`/`h` suffix, such
        /// as `30000` or `30m` (see [`parse_min_expiry`])
        #[arg(
            long = "min-expiry",
            value_name = "DURATION",
            value_parser = parse_min_expiry
        )]
        min_expiry: Option<u64>,
    },
}

#[cfg(feature = "cli")]
impl Cli {
    /// The parsed `--mode` value (`None` for absent; invalid values are
    /// rejected before parsing by [`validate_mode_arg`] — upstream exits 1 on
    /// them).
    pub fn mode(&self) -> Option<Mode> {
        normalize_mode(self.mode.as_deref()).ok().flatten()
    }

    /// Resolve and validate a `pir update` invocation (upstream
    /// `parsePackageCommand` + `updateTarget` resolution,
    /// package-manager-cli.ts:442-448,488-495,497-514,531-580). `Ok(None)`
    /// when this is not an `update` command; `Err` carries the upstream
    /// conflict text (the caller prints it and exits 1).
    ///
    /// Conflict rules mirrored from upstream:
    /// - `--all` rejects `--self`, `--extensions`, `--models`,
    ///   `--extension`, and a positional source;
    /// - `--models` rejects `--self`, `--extensions`, `--all`,
    ///   `--extension`, and a positional source;
    /// - `--extension` rejects `--self`, `--extensions`, `--all`, and a
    ///   positional source;
    /// - a non-self positional source rejects `--self`, `--extensions`, and
    ///   `--all` (but `self`/`pi` + `--extensions` widens to all).
    pub fn resolve_update_plan(&self) -> Result<Option<UpdatePlan>, String> {
        let Some(Command::Update {
            source,
            models,
            extensions,
            all,
            self_update,
            update_extension,
            force,
        }) = self.command.as_ref()
        else {
            return Ok(None);
        };
        if *all && (*self_update || *extensions || *models || update_extension.is_some()) {
            return Err(
                "--all cannot be combined with --self, --extensions, --models, or --extension"
                    .to_string(),
            );
        }
        if *all && source.is_some() {
            return Err("--all cannot be combined with a positional source".to_string());
        }
        if *models {
            if *self_update || *extensions || *all || update_extension.is_some() {
                return Err(
                    "--models cannot be combined with --self, --extensions, --all, or --extension"
                        .to_string(),
                );
            }
            if source.is_some() {
                return Err("--models cannot be combined with a positional source".to_string());
            }
            return Ok(Some(UpdatePlan {
                request: UpdateRequest::Models,
                force: *force,
                show_extensions_skipped_note: false,
            }));
        }
        if let Some(extension) = update_extension {
            if *self_update || *extensions || *all {
                return Err(
                    "--extension cannot be combined with --self, --extensions, or --all"
                        .to_string(),
                );
            }
            if source.is_some() {
                return Err("--extension cannot be combined with a positional source".to_string());
            }
            return Ok(Some(UpdatePlan {
                request: UpdateRequest::Extension(extension.clone()),
                force: *force,
                show_extensions_skipped_note: false,
            }));
        }
        if let Some(source) = source {
            let source_is_self = matches!(source.as_str(), "self" | "pi");
            if source_is_self {
                return Ok(Some(UpdatePlan {
                    request: if *extensions {
                        UpdateRequest::All
                    } else {
                        UpdateRequest::SelfUpdate
                    },
                    force: *force,
                    show_extensions_skipped_note: false,
                }));
            }
            if *extensions || *self_update || *all {
                return Err(
                    "positional update targets cannot be combined with --self, --extensions, or --all"
                        .to_string(),
                );
            }
            return Ok(Some(UpdatePlan {
                request: UpdateRequest::Package(source.clone()),
                force: *force,
                show_extensions_skipped_note: false,
            }));
        }
        let (request, show_extensions_skipped_note) = if *all || (*self_update && *extensions) {
            (UpdateRequest::All, false)
        } else if *self_update {
            (UpdateRequest::SelfUpdate, false)
        } else if *extensions {
            (UpdateRequest::Extensions, false)
        } else {
            (UpdateRequest::SelfUpdate, true)
        };
        Ok(Some(UpdatePlan {
            request,
            force: *force,
            show_extensions_skipped_note,
        }))
    }

    /// The parsed `pir update` request, without validation: the first valid
    /// target selection, or `None` when the command is absent or the flags
    /// conflict. Public for tests/; the CLI uses
    /// [`Cli::resolve_update_plan`] so conflicts surface as errors.
    pub fn update_request(&self) -> Option<UpdateRequest> {
        self.resolve_update_plan()
            .ok()
            .flatten()
            .map(|plan| plan.request)
    }

    /// The session-related flags, mirrored out of the clap struct.
    pub fn session_flags(&self) -> crate::coding_agent::session_resolve::SessionFlags {
        crate::coding_agent::session_resolve::SessionFlags {
            no_session: self.no_session,
            continue_recent: self.continue_session,
            resume: self.resume,
            session: self.session.clone(),
            session_id: self.session_id.clone(),
            fork: self.fork.clone(),
        }
    }

    /// The `--models` patterns (comma-separated, trimmed, empties dropped).
    ///
    /// `None` means the flag was absent (the settings `enabledModels` scope
    /// applies); `Some([])` is an explicit empty `--models ""`, which upstream
    /// uses to clear the scope instead of falling back to the settings list
    /// (`parsed.models ?? settingsManager.getEnabledModels()`, main.ts:813).
    pub fn models_patterns(&self) -> Option<Vec<String>> {
        self.models
            .as_deref()
            .map(|value| split_comma_list(Some(value)))
    }

    /// The `--tools` entries (comma-separated).
    pub fn tools_entries(&self) -> Vec<String> {
        split_comma_list(self.tools.as_deref())
    }

    /// The `--exclude-tools` entries (comma-separated).
    pub fn exclude_tools_entries(&self) -> Vec<String> {
        split_comma_list(self.exclude_tools.as_deref())
    }

    /// Snapshot the global tool-selection inputs, folding in the resolved
    /// settings `defaultTools` layer.
    ///
    /// The CLI host builds this ONCE and hands it to every mode host (print
    /// already takes the pieces directly; RPC and the server resolve it
    /// against their own registries). The request is intentionally not a
    /// resolved `ToolSelection`: each host's available set differs (RPC
    /// merges builtin/JS tools, the server physically drops the shell tools
    /// under its policy), and the shared resolver needs that set to emit the
    /// same patterns/modifier/diagnostic behavior everywhere.
    pub fn tool_selection_request(
        &self,
        settings: &crate::config::settings::SettingsManager,
    ) -> crate::coding_agent::tool_selection::ToolSelectionRequest {
        crate::coding_agent::tool_selection::ToolSelectionRequest {
            settings_default_tools: settings.default_tools(),
            cli_tools: self.tools_entries(),
            cli_exclude: self.exclude_tools_entries(),
            no_tools: self.no_tools,
            no_builtin_tools: self.no_builtin_tools,
        }
    }

    /// The parsed `--thinking` level (upstream: an invalid value warns and is
    /// ignored).
    pub fn thinking_level(&self) -> Option<crate::ai::types::ThinkingLevel> {
        let value = self.thinking.as_deref()?;
        match super::super::ai::providers::resolution::parse_thinking_level(value.trim()) {
            Some(level) => Some(level),
            None => {
                eprintln!(
                    "Warning: invalid thinking level \"{value}\". Valid values: off, minimal, low, medium, high, xhigh, max"
                );
                None
            }
        }
    }

    /// The `--export` request as `(session reference, output path)`
    /// (upstream `parsed.export` plus `parsed.messages[0]` as the output
    /// path). `None` when the flag is absent.
    pub fn export_request(&self) -> Option<(String, Option<String>)> {
        let values = self.export.as_deref()?;
        let session = values.first()?.trim();
        if session.is_empty() {
            return None;
        }
        let output = values
            .get(1)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        Some((session.to_owned(), output))
    }

    /// The project-trust override from `--approve` / `--no-approve`.
    pub fn trust_override(&self) -> Option<bool> {
        if self.approve {
            Some(true)
        } else if self.no_approve {
            Some(false)
        } else {
            None
        }
    }
}

/// Split a comma-separated flag value into trimmed, non-empty entries
/// (upstream args.ts `split(",")` handling).
pub fn split_comma_list(value: Option<&str>) -> Vec<String> {
    value
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a CLI path value is a local filesystem path (upstream
/// `isLocalPath`, utils/paths.ts:50-65). Package sources (`npm:`, `git:`,
/// `github:`), remote URLs (`http:`, `https:`, `ssh:`) and `builtin:` names are
/// not local and must pass through [`resolve_cli_path`] untouched.
pub fn is_local_cli_path(value: &str) -> bool {
    let trimmed = value.trim();
    !(trimmed.starts_with("npm:")
        || trimmed.starts_with("git:")
        || trimmed.starts_with("github:")
        || trimmed.starts_with("http:")
        || trimmed.starts_with("https:")
        || trimmed.starts_with("ssh:")
        || trimmed.starts_with("builtin:"))
}

/// Resolve a CLI path argument the way upstream `resolvePath` does
/// (`utils/paths.ts:76-107`): expand a leading `~`/`~/`/`~\`, convert a
/// `file://` URL to a filesystem path (`fileURLToPath`), then resolve a
/// still-relative path against `base_dir` (the process cwd for the CLI).
///
/// Used for `--session-dir` and the repeatable `-e/--extension` paths; the
/// settings scope resolvers keep their own base-directory rule.
pub fn resolve_cli_path(input: &str, base_dir: &std::path::Path) -> std::path::PathBuf {
    let expanded = expand_cli_path(input);
    if expanded.is_absolute() || expanded.has_root() {
        expanded
    } else {
        base_dir.join(expanded)
    }
}

/// The cwd-independent half of [`resolve_cli_path`]: `~` expansion plus
/// `file://` URL conversion.
pub fn expand_cli_path(input: &str) -> std::path::PathBuf {
    // Upstream `normalizePath` expands `~` before the file-URL branch, and a
    // tilde-expanded path never starts with `file://`.
    let expanded = crate::coding_agent::resources::expand_tilde(input);
    if expanded.is_absolute() || !input.starts_with("file://") {
        return expanded;
    }
    file_url_to_path(input).unwrap_or(expanded)
}

/// The `fileURLToPath` equivalent for the `file://` URLs the CLI accepts.
/// `None` when the URL names a non-local host on a platform that has no UNC
/// concept (Node throws for that case too, so the caller keeps the raw path).
fn file_url_to_path(input: &str) -> Option<std::path::PathBuf> {
    let rest = input.strip_prefix("file://")?;
    // Split the authority (empty, `localhost`, or a Windows UNC host) from
    // the URL path.
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let decoded = percent_decode(path);
    #[cfg(windows)]
    {
        if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
            // UNC: `file://server/share/x` -> `\\server\share\x`.
            let mut unc = String::from("\\\\");
            unc.push_str(authority);
            unc.push_str(&decoded.replace('/', "\\"));
            return Some(std::path::PathBuf::from(unc));
        }
        // `file:///C:/x` -> `C:\x` (drop the URL root slash in front of the
        // drive letter; a drive-less `/home/x` stays rooted).
        let bytes = decoded.as_bytes();
        let has_drive = bytes.len() >= 3
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && (bytes[2] == b':' || bytes[2] == b'|');
        let path = if has_drive { &decoded[1..] } else { &decoded };
        Some(std::path::PathBuf::from(path.replace('/', "\\")))
    }
    #[cfg(not(windows))]
    {
        if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
            return None;
        }
        Some(std::path::PathBuf::from(decoded))
    }
}

/// Decode the `%XX` escapes of a file URL path (UTF-8, lossy for invalid
/// sequences). Node's `fileURLToPath` percent-decodes the pathname.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = |byte: u8| (byte as char).to_digit(16);
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                decoded.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

// ---------------------------------------------------------------------------
// `pir mcp` command runtime
// ---------------------------------------------------------------------------

/// One server's check outcome (`pir mcp check`).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCheckEntry {
    pub name: String,
    /// `stdio` or `http`.
    pub transport: String,
    /// Where the server is configured / registered.
    pub source: String,
    /// `ok` or `failed`.
    pub status: String,
    /// Tool names when the connection succeeded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// Whether the server offers resources (upstream `list --json`'s
    /// `resources: true`). They are not listed: listing every one can take
    /// long, so the report only says a server offers them (upstream #10526).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Bounded overall deadline for one server's test-connect (`pir mcp check`).
#[cfg(all(feature = "mcp", feature = "cli"))]
const MCP_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Run a `pir mcp` subcommand: `add`/`remove` edit `mcp.json`; `check` test-
/// connects; `list` reports the upstream per-server surface; `login`/`logout`
/// run and clear the OAuth sign-in. A failed `list` becomes an `Err` here;
/// the binary path uses [`run_mcp_command_code`] for its exact exit status.
#[cfg(feature = "cli")]
pub async fn run_mcp_command(command: McpCommand) -> crate::Result<()> {
    run_mcp_command_with_trust(command, None).await
}

/// [`run_mcp_command`] with the CLI's explicit `--approve`/`--no-approve`
/// override (upstream `projectTrustOverride`). The explicit override wins over
/// the ambient trust store / `PIR_TRUST_PROJECT` order, exactly like the mode
/// hosts' startup gate.
#[cfg(feature = "cli")]
pub async fn run_mcp_command_with_trust(
    command: McpCommand,
    trust_override: Option<bool>,
) -> crate::Result<()> {
    let code = run_mcp_command_code_with_trust(command, trust_override).await?;
    if code != 0 {
        return Err(anyhow::anyhow!("MCP command reported failures"));
    }
    Ok(())
}

/// Run a `pir mcp` subcommand and return its process exit code (upstream
/// `runMcpCommand`'s number return). Only `list` reports failure (exit 1)
/// after printing; `check` keeps its historical always-success status.
#[cfg(feature = "cli")]
pub async fn run_mcp_command_code(command: McpCommand) -> crate::Result<i32> {
    run_mcp_command_code_with_trust(command, None).await
}

/// [`run_mcp_command_code`] with the CLI's explicit trust override. Every
/// config-reading arm (`check`/`list`/`login`/`logout`, and `add`/`remove`'s
/// project-scope diagnostics) uses the SAME resolved boolean, so a
/// `--no-approve` invocation cannot read the project `mcp.json` through the
/// ambient `PIR_TRUST_PROJECT=1` / trust-store fallback.
#[cfg(feature = "cli")]
pub async fn run_mcp_command_code_with_trust(
    command: McpCommand,
    trust_override: Option<bool>,
) -> crate::Result<i32> {
    let agent_dir = crate::coding_agent::context::agent_dir();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let project_trusted =
        trust_override.unwrap_or_else(|| crate::config::trust::resolve_cwd_trust(&cwd, &agent_dir));
    match command {
        McpCommand::Add {
            name,
            url,
            command,
            env,
            cwd,
            header,
            bearer_token_env_var,
            oauth_client_id,
            oauth_client_secret,
            oauth_callback_port,
            oauth_client_name,
            exposure,
            description,
            local,
        } => {
            run_mcp_add(
                McpAddOptions {
                    name,
                    url,
                    command,
                    env,
                    cwd,
                    header,
                    bearer_token_env_var,
                    oauth_client_id,
                    oauth_client_secret,
                    oauth_callback_port,
                    oauth_client_name,
                    exposure,
                    description,
                    local,
                },
                project_trusted,
            )?;
            Ok(0)
        }
        McpCommand::Remove { name, local } => {
            run_mcp_remove(&name, local, project_trusted)?;
            Ok(0)
        }
        // The arms below need the `mcp` module (a live client, OAuth), so
        // without the `mcp` feature they are not built; the catch-all reports
        // that honestly instead of dropping the subcommand silently.
        #[cfg(feature = "mcp")]
        McpCommand::Check { server, json } => {
            run_mcp_check(server.as_deref(), json, project_trusted).await?;
            Ok(0)
        }
        #[cfg(feature = "mcp")]
        McpCommand::List { json } => Ok(if run_mcp_list(json, project_trusted).await? {
            0
        } else {
            1
        }),
        #[cfg(feature = "mcp")]
        McpCommand::Login {
            server,
            timeout_seconds,
        } => {
            run_mcp_login(&server, timeout_seconds, project_trusted).await?;
            Ok(0)
        }
        #[cfg(feature = "mcp")]
        McpCommand::Logout { server } => {
            run_mcp_logout(&server, project_trusted).await?;
            Ok(0)
        }
        #[cfg(not(feature = "mcp"))]
        _ => Err(anyhow::anyhow!(
            "`pir mcp check`, `pir mcp list`, `pir mcp login` and `pir mcp logout` are unavailable: this build was built without the `mcp` feature"
        )),
    }
}

/// The parsed `pir mcp add` options (upstream `add`, extensions/mcp/cli.ts:277).
#[cfg(feature = "cli")]
struct McpAddOptions {
    name: String,
    url: Option<String>,
    command: Vec<String>,
    env: Vec<String>,
    cwd: Option<String>,
    header: Vec<String>,
    bearer_token_env_var: Option<String>,
    oauth_client_id: Option<String>,
    oauth_client_secret: Option<String>,
    oauth_callback_port: Option<u16>,
    oauth_client_name: Option<String>,
    exposure: Option<String>,
    description: Option<String>,
    local: bool,
}

/// The `pir mcp add` usage line (upstream `add`'s usage error,
/// extensions/mcp/cli.ts:280-281).
#[cfg(feature = "cli")]
const MCP_ADD_USAGE: &str =
    "Usage: pir mcp add <server> [options] (--url <url> | -- <command> [args...])";

/// Parse `KEY=VALUE` pairs of a repeatable option (upstream `parsePairs`,
/// extensions/mcp/cli.ts:254-268): the separator must exist and the key must
/// be non-empty.
#[cfg(feature = "cli")]
fn parse_key_value_pairs(
    option: &str,
    pairs: &[String],
) -> crate::Result<BTreeMap<String, String>> {
    let mut record = BTreeMap::new();
    for pair in pairs {
        let Some(separator) = pair.find('=') else {
            return Err(anyhow::anyhow!(
                "--{option} expects KEY=VALUE, got \"{pair}\"."
            ));
        };
        if separator == 0 {
            return Err(anyhow::anyhow!(
                "--{option} expects KEY=VALUE, got \"{pair}\"."
            ));
        }
        record.insert(
            pair[..separator].to_string(),
            pair[separator + 1..].to_string(),
        );
    }
    Ok(record)
}

/// Resolve the `--exposure` value (upstream `isExposure` +
/// `MCP_EXPOSURE_ALIASES`: `codemode-deferred` is accepted as an alias of
/// `codemode`).
#[cfg(feature = "cli")]
fn parse_mcp_exposure(name: &str, value: &str) -> crate::Result<&'static str> {
    match value.trim() {
        "codemode" | "codemode-deferred" => Ok("codemode"),
        "deferred" => Ok("deferred"),
        "direct" => Ok("direct"),
        "hidden" => Ok("hidden"),
        _ => Err(anyhow::anyhow!(
            "server \"{name}\": exposure must be one of \"codemode\", \"deferred\", \"direct\", \"hidden\""
        )),
    }
}

/// Build the `mcp.json` server entry from the `pir mcp add` options (upstream
/// `add`'s config assembly, extensions/mcp/cli.ts:311-361) and validate it
/// with the same loader rules `pir mcp` reads back (`config::mcp`).
#[cfg(feature = "cli")]
fn build_mcp_add_config(options: &McpAddOptions) -> crate::Result<serde_json::Value> {
    let name = options.name.trim();
    if name.is_empty() {
        return Err(anyhow::anyhow!("{MCP_ADD_USAGE}"));
    }
    // Same server-name contract the loader enforces (`validate_server`): an
    // entry `mcp add` writes must be loadable, not silently dropped on the
    // next start with a validation error.
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(anyhow::anyhow!(
            "server \"{name}\": name must match [A-Za-z0-9_-]+"
        ));
    }
    // Exactly one of `--url` and a `--` command (upstream add: `(url ===
    // undefined) === (command.length === 0)` is a usage error).
    if options.url.is_none() == options.command.is_empty() {
        return Err(anyhow::anyhow!("{MCP_ADD_USAGE}"));
    }
    let exposure = options
        .exposure
        .as_deref()
        .map(|value| parse_mcp_exposure(name, value))
        .transpose()?;
    // Upstream rejects options that belong to the other transport
    // (extensions/mcp/cli.ts:296-304).
    let url_given = options.url.is_some();
    let misplaced = if url_given {
        ["env", "cwd"].into_iter().find(|option| match *option {
            "env" => !options.env.is_empty(),
            _ => options.cwd.is_some(),
        })
    } else {
        [
            "header",
            "bearer-token-env-var",
            "oauth-client-id",
            "oauth-client-secret",
            "oauth-callback-port",
            "oauth-client-name",
        ]
        .into_iter()
        .find(|option| match *option {
            "header" => !options.header.is_empty(),
            "bearer-token-env-var" => options.bearer_token_env_var.is_some(),
            "oauth-client-id" => options.oauth_client_id.is_some(),
            "oauth-client-secret" => options.oauth_client_secret.is_some(),
            "oauth-callback-port" => options.oauth_callback_port.is_some(),
            _ => options.oauth_client_name.is_some(),
        })
    };
    if let Some(option) = misplaced {
        return Err(anyhow::anyhow!(
            "--{option} only applies to {}.",
            if url_given {
                "stdio servers"
            } else {
                "HTTP servers (--url)"
            }
        ));
    }

    let mut config = serde_json::Map::new();
    if let Some(url) = options
        .url
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        // Upstream `validateMcpServerConfig`: the url must parse as http(s).
        let parsed = reqwest::Url::parse(url)
            .map_err(|_| anyhow::anyhow!("server \"{name}\": url must be an http or https URL"))?;
        if parsed.scheme() != "http" && parsed.scheme() != "https" {
            return Err(anyhow::anyhow!(
                "server \"{name}\": url must be an http or https URL"
            ));
        }
        let mut headers = parse_key_value_pairs("header", &options.header)?;
        if let Some(bearer) = options
            .bearer_token_env_var
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            headers.insert("Authorization".to_string(), format!("Bearer ${{{bearer}}}"));
        }
        let mut oauth = serde_json::Map::new();
        if let Some(client_id) = options
            .oauth_client_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            oauth.insert("clientId".to_string(), serde_json::json!(client_id));
        }
        if let Some(client_secret) = options
            .oauth_client_secret
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            oauth.insert("clientSecret".to_string(), serde_json::json!(client_secret));
        }
        if let Some(port) = options.oauth_callback_port {
            // Upstream: callbackPort must be a port number (1-65535).
            if port == 0 {
                return Err(anyhow::anyhow!(
                    "server \"{name}\": oauth.callbackPort must be a port number"
                ));
            }
            oauth.insert("callbackPort".to_string(), serde_json::json!(port));
        }
        if let Some(client_name) = options
            .oauth_client_name
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            oauth.insert("clientName".to_string(), serde_json::json!(client_name));
        }
        config.insert("url".to_string(), serde_json::json!(url));
        if !headers.is_empty() {
            config.insert("headers".to_string(), serde_json::json!(headers));
        }
        if !oauth.is_empty() {
            config.insert("oauth".to_string(), serde_json::Value::Object(oauth));
        }
    } else {
        // stdio: the first `--` argument is the executable, the rest are args.
        let mut command = options.command.iter().map(String::as_str);
        let Some(executable) = command.next() else {
            return Err(anyhow::anyhow!("{MCP_ADD_USAGE}"));
        };
        config.insert("command".to_string(), serde_json::json!(executable));
        let args: Vec<&str> = command.collect();
        if !args.is_empty() {
            config.insert("args".to_string(), serde_json::json!(args));
        }
        let env = parse_key_value_pairs("env", &options.env)?;
        if !env.is_empty() {
            config.insert("env".to_string(), serde_json::json!(env));
        }
        if let Some(cwd) = options
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            config.insert("cwd".to_string(), serde_json::json!(cwd));
        }
    }
    if let Some(exposure) = exposure {
        config.insert("exposure".to_string(), serde_json::json!(exposure));
    }
    if let Some(description) = options
        .description
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        config.insert("description".to_string(), serde_json::json!(description));
    }
    let value = serde_json::Value::Object(config);
    // Validate against the shape the MCP loader reads back (rejects unknown
    // fields and per-transport requirements).
    let parsed: crate::config::mcp::McpServerConfig = serde_json::from_value(value.clone())
        .map_err(|error| anyhow::anyhow!("server \"{name}\": {error}"))?;
    if let Some(problem) = crate::config::mcp::validate_server(name, &parsed) {
        return Err(anyhow::anyhow!("{problem}"));
    }
    Ok(value)
}

/// The `mcpServers` member of a parsed `mcp.json` root, in the upstream
/// wrapper shape (extensions/mcp/config.ts `editMcpServers`, :228-242).
/// pir's writers emit this shape so upstream pi can read pir's files.
///
/// A legacy pir flat map (a root object without `mcpServers`) is migrated
/// in place: its top-level entries move under `mcpServers`, so pir's older
/// files keep working and upgrade to the interop shape on the next edit.
/// Any other top-level content is kept verbatim.
fn mcp_servers_member<'a>(
    root: &'a mut serde_json::Value,
    path: &std::path::Path,
) -> crate::Result<&'a mut serde_json::Map<String, serde_json::Value>> {
    let Some(map) = root.as_object_mut() else {
        return Err(anyhow::anyhow!(
            "{}: expected an object with an \"mcpServers\" object",
            path.display()
        ));
    };
    if let Some(existing) = map.get("mcpServers") {
        if !existing.is_object() {
            return Err(anyhow::anyhow!(
                "{}: expected an object with an \"mcpServers\" object",
                path.display()
            ));
        }
    } else {
        // Legacy flat map (or an empty file): migrate the top-level entries
        // under the `mcpServers` key. An entry literally named `mcpServers`
        // cannot exist in the legacy shape (the loader treats that key as the
        // wrapper marker), so this is lossless.
        let legacy = std::mem::take(map);
        map.insert("mcpServers".to_string(), serde_json::Value::Object(legacy));
    }
    match map
        .get_mut("mcpServers")
        .and_then(serde_json::Value::as_object_mut)
    {
        Some(servers) => Ok(servers),
        // Unreachable in practice: the key was either validated as an object
        // or inserted above, but no panics.
        None => Err(anyhow::anyhow!(
            "{}: expected an object with an \"mcpServers\" object",
            path.display()
        )),
    }
}

/// Insert or replace one server entry in an `mcp.json` (upstream
/// `addMcpServerConfig`, extensions/mcp/config.ts:194-210): the file is
/// created when missing, other content is kept, and the return value reports
/// whether an entry was replaced. The file is written in the upstream
/// `{ "mcpServers": { … } }` wrapper shape (a legacy pir flat map is
/// migrated). Public for tests/.
#[doc(hidden)]
pub fn mcp_config_upsert(
    path: &std::path::Path,
    name: &str,
    config: &serde_json::Value,
) -> crate::Result<bool> {
    // Read-modify-write under the shared settings-file lock: two concurrent
    // `pir mcp add` invocations (or an add racing a remove) must not drop each
    // other's entries. The atomic temp-file replace below keeps lock-free
    // readers from observing a torn file. Create the parent first so the lock
    // file itself can be created for a first-ever write.
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::config::settings::with_settings_file_lock(path, || {
        let mut root: serde_json::Value = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(text.trim_start_matches('\u{feff}'))
                .map_err(|error| anyhow::anyhow!("{}: {}", path.display(), error))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                serde_json::Value::Object(serde_json::Map::new())
            }
            // An existing file that cannot be read (permissions, invalid UTF-8,
            // a directory) must not be silently replaced with only this entry:
            // that would destroy every server the user configured.
            Err(error) => return Err(anyhow::anyhow!("cannot read {}: {error}", path.display())),
        };
        let servers = mcp_servers_member(&mut root, path)?;
        let replaced = servers.contains_key(name);
        servers.insert(name.to_string(), config.clone());
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Upstream `editMcpServers` writes with the file's own indentation,
        // defaulting to two spaces (serde_json's pretty printer). `mcp.json`
        // can carry bearer tokens/headers, so a newly created file is
        // private (`0o600` on Unix) like `auth.json`; an existing file keeps
        // its mode.
        crate::config::settings::write_private_text_atomic(
            path,
            &format!("{}\n", serde_json::to_string_pretty(&root)?),
        )?;
        Ok(replaced)
    })
}

/// Remove one server entry from an `mcp.json` (upstream
/// `removeMcpServerConfig`, extensions/mcp/config.ts:212-222): `false` when
/// the file does not define the name. Reads the upstream `mcpServers`
/// wrapper shape and migrates a legacy pir flat map like
/// [`mcp_config_upsert`]. Public for tests/.
#[doc(hidden)]
pub fn mcp_config_remove(path: &std::path::Path, name: &str) -> crate::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    crate::config::settings::with_settings_file_lock(path, || {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            // The file disappeared between the existence probe and the
            // locked read: there is nothing left to remove.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let mut root: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
            .map_err(|error| anyhow::anyhow!("{}: {}", path.display(), error))?;
        let servers = mcp_servers_member(&mut root, path)?;
        if servers.remove(name).is_none() {
            return Ok(false);
        }
        crate::config::settings::write_private_text_atomic(
            path,
            &format!("{}\n", serde_json::to_string_pretty(&root)?),
        )?;
        Ok(true)
    })
}

/// `pir mcp add <server> [options] (--url <url> | -- <command> [args...])`
/// (upstream `add`, extensions/mcp/cli.ts:277-391).
#[cfg(feature = "cli")]
#[allow(clippy::too_many_lines)]
fn run_mcp_add(options: McpAddOptions, project_trusted: bool) -> crate::Result<()> {
    let name = options.name.trim().to_string();
    let config = build_mcp_add_config(&options)?;
    let agent_dir = crate::coding_agent::context::agent_dir();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let path = if options.local {
        crate::config::mcp::project_mcp_path(&cwd)
    } else {
        crate::config::mcp::global_mcp_path(&agent_dir)
    };
    let scope = if options.local { "project" } else { "global" };
    let replaced = mcp_config_upsert(&path, &name, &config)
        .map_err(|error| anyhow::anyhow!("Could not update {}: {error}", path.display()))?;
    println!(
        "{} {scope} MCP server \"{name}\" in {}.",
        if replaced { "Replaced" } else { "Added" },
        path.display()
    );
    if options.local && !project_trusted {
        println!(
            "The project is not trusted, so {} is ignored until you start pir in the project and trust it.",
            path.display()
        );
    }
    // HTTP servers without an Authorization header may use OAuth (upstream
    // `mayNeedSignIn`).
    let has_authorization_header = config
        .get("headers")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|headers| {
            headers
                .keys()
                .any(|header| header.eq_ignore_ascii_case("authorization"))
        });
    let may_need_sign_in = config.get("url").is_some() && !has_authorization_header;
    println!(
        "Check it with: pir mcp list{}",
        if may_need_sign_in {
            format!(". If it requires sign-in: pir mcp login {name}")
        } else {
            String::new()
        }
    );
    Ok(())
}

/// `pir mcp remove <server> [-l]` (upstream `remove`,
/// extensions/mcp/cli.ts:396-435).
#[cfg(feature = "cli")]
fn run_mcp_remove(name: &str, local: bool, _project_trusted: bool) -> crate::Result<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err(anyhow::anyhow!("Usage: pir mcp remove <server> [-l]"));
    }
    let agent_dir = crate::coding_agent::context::agent_dir();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let path = if local {
        crate::config::mcp::project_mcp_path(&cwd)
    } else {
        crate::config::mcp::global_mcp_path(&agent_dir)
    };
    let scope = if local { "project" } else { "global" };
    let removed = mcp_config_remove(&path, name)
        .map_err(|error| anyhow::anyhow!("Could not update {}: {error}", path.display()))?;
    if removed {
        println!(
            "Removed {scope} MCP server \"{name}\" from {}.",
            path.display()
        );
        return Ok(());
    }
    // Upstream points at the config file that DOES define the server when the
    // requested scope does not (extensions/mcp/cli.ts:423-433).
    // The hint is a READ-ONLY lookup of the other scope; it must see the
    // project file even when the project is untrusted (execution is still
    // gated elsewhere). Otherwise a remove in the wrong scope cannot name the
    // defining file at all.
    let other = crate::config::mcp::load_mcp_configs(&agent_dir, &cwd, true)
        .into_iter()
        .find(|loaded| loaded.path != path && loaded.servers.contains_key(name));
    let hint = other
        .map(|loaded| {
            let other_scope = if loaded.path == crate::config::mcp::project_mcp_path(&cwd) {
                "project"
            } else {
                "global"
            };
            format!(
                " It is defined in {}{}.",
                loaded.path.display(),
                if other_scope == "project" {
                    "; use --local"
                } else {
                    "; omit --local"
                }
            )
        })
        .unwrap_or_default();
    Err(anyhow::anyhow!(
        "No {scope} MCP server named \"{name}\" in {}.{hint}",
        path.display()
    ))
}

/// The interactive sign-in prompt (upstream `waitForRedirectUrl`,
/// `extensions/mcp/cli.ts:584`): reads the pasted redirect URL in a
/// terminal; without one, only the browser callback or the timeout finishes
/// the sign-in.
#[cfg(all(feature = "mcp", feature = "cli"))]
struct McpCliSignInPrompt {
    interactive: bool,
}

#[cfg(all(feature = "mcp", feature = "cli"))]
impl McpCliSignInPrompt {
    fn new() -> Self {
        use std::io::IsTerminal;
        Self {
            interactive: std::io::stdin().is_terminal(),
        }
    }
}

#[cfg(all(feature = "mcp", feature = "cli"))]
impl crate::mcp::oauth::McpSignInPrompt for McpCliSignInPrompt {
    fn show_authorization_url(&self, url: &str) {
        println!("{url}");
        crate::ai::auth::oauth::open_browser(url);
    }

    fn prompt_for_redirect_url(&self) -> crate::mcp::oauth::PromptFuture<'_> {
        if !self.interactive {
            // Without a terminal there is nothing to paste into; the browser
            // callback or the sign-in timeout finishes the flow.
            return Box::pin(std::future::pending());
        }
        Box::pin(async {
            tokio::task::spawn_blocking(|| {
                use std::io::BufRead;
                eprint!(
                    "If the browser cannot reach this machine, paste the URL it was redirected to: "
                );
                let mut line = String::new();
                match std::io::stdin().lock().read_line(&mut line) {
                    Ok(0) => None,
                    Ok(_) => Some(line.trim().to_string()),
                    Err(_) => None,
                }
            })
            .await
            .ok()
            .flatten()
        })
    }
}

/// `pir mcp login <server> [--timeout <seconds>]` (upstream `login`,
/// `extensions/mcp/cli.ts:521`): connect first (a stored refresh token may
/// already suffice), run the OAuth flow when the server demands a sign-in,
/// then reconnect and report the tool count.
#[cfg(all(feature = "mcp", feature = "cli"))]
async fn run_mcp_login(
    server: &str,
    timeout_seconds: u64,
    project_trusted: bool,
) -> crate::Result<()> {
    let agent_dir = crate::coding_agent::context::agent_dir();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let manager = crate::mcp::manager::McpManager::new(&agent_dir, &cwd);
    let config_errors = manager.load_config_files(&agent_dir, project_trusted);
    let Some(entry) = manager.registry().get(server) else {
        let configured: Vec<String> = manager
            .registry()
            .list()
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        let note = if config_errors.is_empty() {
            String::new()
        } else {
            format!(" ({})", config_errors.join("; "))
        };
        return Err(anyhow::anyhow!(
            "No MCP server named \"{server}\"{note}. Configured: {}.",
            configured.join(", ")
        ));
    };
    let Some(url) = crate::mcp::oauth::oauth_server_url(&entry.config) else {
        return Err(anyhow::anyhow!(
            "MCP server \"{server}\" does not use OAuth. Only HTTP servers without an Authorization header do."
        ));
    };
    let url = url.to_owned();

    // Connecting first answers whether a sign-in is needed (upstream cli.ts:532).
    match manager.check_server(server, MCP_CHECK_TIMEOUT).await {
        Ok(tools) => {
            println!(
                "Already signed in to MCP server \"{server}\" ({} tools).",
                tools.len()
            );
            manager.shutdown().await;
            return Ok(());
        }
        Err(crate::mcp::McpError::LoginRequired { .. }) => {}
        Err(error) if crate::mcp::oauth::is_authorization_required(&error) => {}
        Err(error) => {
            manager.shutdown().await;
            return Err(anyhow::anyhow!(
                "MCP server \"{server}\" failed to connect: {error}"
            ));
        }
    }

    let crate::config::mcp::McpServerConfig::Http(http) = &entry.config else {
        return Err(anyhow::anyhow!(
            "MCP server \"{server}\" does not use OAuth. Only HTTP servers without an Authorization header do."
        ));
    };
    let settings = crate::mcp::oauth::McpOAuthSettings::from_config(http, server)
        .map_err(|error| anyhow::anyhow!("MCP server \"{server}\": {error}"))?;
    let store = manager.oauth_store().for_server(server, &url);
    let prompt = McpCliSignInPrompt::new();
    println!("Sign in to MCP server \"{server}\" in your browser:");
    let result = crate::mcp::oauth::sign_in(
        &store,
        &settings,
        &url,
        None,
        &prompt,
        std::time::Duration::from_secs(timeout_seconds),
    )
    .await;
    match result {
        Ok(()) => {}
        Err(crate::mcp::oauth::McpOAuthError::Cancelled) => {
            manager.shutdown().await;
            return Err(anyhow::anyhow!(
                "Sign-in to MCP server \"{server}\" was cancelled or not completed within {timeout_seconds} seconds."
            ));
        }
        Err(error) => {
            manager.shutdown().await;
            return Err(anyhow::anyhow!(
                "Sign-in to MCP server \"{server}\" failed: {error}"
            ));
        }
    }

    match manager.check_server(server, MCP_CHECK_TIMEOUT).await {
        Ok(tools) => {
            println!(
                "Signed in to MCP server \"{server}\" ({} tools).",
                tools.len()
            );
        }
        Err(error) => {
            manager.shutdown().await;
            return Err(anyhow::anyhow!("Signed in, but {error}"));
        }
    }
    manager.shutdown().await;
    Ok(())
}

/// `pir mcp logout <server>` (upstream `logout`): delete the stored OAuth
/// credentials of the server.
#[cfg(all(feature = "mcp", feature = "cli"))]
async fn run_mcp_logout(server: &str, project_trusted: bool) -> crate::Result<()> {
    let agent_dir = crate::coding_agent::context::agent_dir();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let manager = crate::mcp::manager::McpManager::new(&agent_dir, &cwd);
    manager.load_config_files(&agent_dir, project_trusted);
    let Some(entry) = manager.registry().get(server) else {
        return Err(anyhow::anyhow!("No MCP server named \"{server}\"."));
    };
    let Some(url) = crate::mcp::oauth::oauth_server_url(&entry.config) else {
        return Err(anyhow::anyhow!(
            "MCP server \"{server}\" does not use OAuth. Only HTTP servers without an Authorization header do."
        ));
    };
    let removed = manager
        .oauth_store()
        .for_server(server, url)
        .remove()
        .await
        .map_err(|error| anyhow::anyhow!("Could not update the MCP OAuth credentials: {error}"))?;
    if removed {
        println!("Signed out of MCP server \"{server}\".");
    } else {
        println!("No stored credentials for MCP server \"{server}\".");
    }
    Ok(())
}

/// `pir mcp check [server] [--json]`.
#[cfg(all(feature = "mcp", feature = "cli"))]
async fn run_mcp_check(only: Option<&str>, json: bool, project_trusted: bool) -> crate::Result<()> {
    let agent_dir = crate::coding_agent::context::agent_dir();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let manager = crate::mcp::manager::McpManager::new(&agent_dir, &cwd);
    manager.load_config_files(&agent_dir, project_trusted);

    let mut names: Vec<String> = manager
        .registry()
        .list()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    names.sort();
    if let Some(only) = only {
        names.retain(|name| name == only);
        if names.is_empty() {
            return Err(anyhow::anyhow!("MCP server \"{only}\" is not configured"));
        }
    }
    if names.is_empty() {
        if json {
            println!("[]");
        } else {
            println!("No MCP servers configured.");
            println!("Add ~/.pi/agent/mcp.json or <cwd>/.pi/mcp.json (project files need trust).");
        }
        return Ok(());
    }

    let mut entries: Vec<McpCheckEntry> = Vec::new();
    for name in names {
        let Some(entry) = manager.registry().get(&name) else {
            continue;
        };
        let transport = match &entry.config {
            crate::config::mcp::McpServerConfig::Stdio(_) => "stdio",
            crate::config::mcp::McpServerConfig::Http(_) => "http",
        };
        let source = match &entry.source {
            crate::mcp::registry::McpServerSource::GlobalConfig(path)
            | crate::mcp::registry::McpServerSource::ProjectConfig(path) => {
                path.display().to_string()
            }
            crate::mcp::registry::McpServerSource::Extension(path) => path.clone(),
        };
        if !entry.config.is_enabled() {
            entries.push(McpCheckEntry {
                name,
                transport: transport.to_owned(),
                source,
                status: "disabled".to_owned(),
                tools: Vec::new(),
                resources: None,
                error: None,
            });
            continue;
        }
        match manager.check_server_report(&name, MCP_CHECK_TIMEOUT).await {
            Ok((tools, has_resources)) => entries.push(McpCheckEntry {
                name,
                transport: transport.to_owned(),
                source,
                status: "ok".to_owned(),
                tools,
                // Upstream reports the flag as `true` only for servers that
                // offer resources; servers without them omit the field.
                resources: has_resources.then_some(true),
                error: None,
            }),
            Err(error) => entries.push(McpCheckEntry {
                name,
                transport: transport.to_owned(),
                source,
                status: "failed".to_owned(),
                tools: Vec::new(),
                resources: None,
                error: Some(error.to_string()),
            }),
        }
    }
    // Shut down any connections the check opened.
    manager.shutdown().await;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_owned())
        );
        return Ok(());
    }
    for entry in &entries {
        match entry.status.as_str() {
            "ok" => {
                println!(
                    "{} ({}): ok, {} tools",
                    entry.name,
                    entry.transport,
                    entry.tools.len()
                );
                for tool in &entry.tools {
                    println!("  - {tool}");
                }
                // Upstream logs "  offers resources" instead of the counts it
                // used to fetch while connecting (upstream #10526).
                if entry.resources == Some(true) {
                    println!("  offers resources");
                }
            }
            "disabled" => {
                println!("{} ({}): disabled", entry.name, entry.transport);
            }
            _ => {
                println!(
                    "{} ({}): failed: {}",
                    entry.name,
                    entry.transport,
                    entry.error.clone().unwrap_or_default()
                );
            }
        }
    }
    Ok(())
}

/// One server row of `pir mcp list` (upstream `ServerReport`,
/// extensions/mcp/cli.ts:91-107).
#[cfg(all(feature = "mcp", feature = "cli"))]
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct McpListEntry {
    name: String,
    /// `global`, `project`, or `extension`.
    scope: String,
    /// The config file (or extension path) that defines the entry.
    source: String,
    /// The project `mcp.json` overriding a global entry (upstream `override`).
    #[serde(rename = "override", skip_serializing_if = "Option::is_none")]
    override_path: Option<String>,
    enabled: bool,
    exposure: String,
    /// `command args` for stdio, the URL for HTTP.
    transport: String,
    /// `disabled`, `connected`, `needs-auth`, `disconnected`, `closed`, or
    /// `failed` (upstream `ServerState`).
    state: String,
    tools: Vec<String>,
    /// Tools whose exposure differs from the server's (upstream
    /// `toolExposure`).
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_exposure: Option<BTreeMap<String, String>>,
    /// Present only when the server offers resources.
    #[serde(skip_serializing_if = "Option::is_none")]
    resources: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// The `pir mcp list --json` document (upstream `{servers, errors, note?}`,
/// extensions/mcp/cli.ts:477-485).
#[cfg(all(feature = "mcp", feature = "cli"))]
#[derive(serde::Serialize)]
struct McpListOutput {
    servers: Vec<McpListEntry>,
    errors: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

/// `pir mcp list [--json]` (upstream `list`, extensions/mcp/cli.ts:434-514):
/// per server report the scope, exposure, transport, connection state, tools,
/// tool-level exposure overrides, resources, and error. Returns whether the
/// listing succeeded (false = exit 1): a config-file error or any enabled
/// server that is not connected fails the command.
#[cfg(all(feature = "mcp", feature = "cli"))]
async fn run_mcp_list(json: bool, project_trusted: bool) -> crate::Result<bool> {
    let agent_dir = crate::coding_agent::context::agent_dir();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let manager = crate::mcp::manager::McpManager::new(&agent_dir, &cwd);
    let config_errors = manager.load_config_files(&agent_dir, project_trusted);
    let project_config = crate::config::mcp::project_mcp_path(&cwd);
    let untrusted_note = (!project_trusted && project_config.exists()).then(|| {
        format!(
            "{} is ignored because the project is not trusted. Start pir in the project to trust it.",
            project_config.display()
        )
    });

    let mut entries = manager.registry().list();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    let mut reports: Vec<McpListEntry> = Vec::with_capacity(entries.len());
    let mut failed = !config_errors.is_empty();
    for entry in entries {
        let enabled = entry.config.is_enabled();
        // Upstream `list` reports the configured exposure, defaulting to
        // codemode (extensions/mcp/cli.ts:450).
        let base_exposure = entry
            .config
            .base()
            .exposure
            .unwrap_or(crate::config::mcp::McpExposure::Codemode);
        let (scope, source) = match &entry.source {
            crate::mcp::registry::McpServerSource::GlobalConfig(path) => {
                ("global", path.display().to_string())
            }
            crate::mcp::registry::McpServerSource::ProjectConfig(path) => {
                ("project", path.display().to_string())
            }
            crate::mcp::registry::McpServerSource::Extension(path) => ("extension", path.clone()),
        };
        let transport = match &entry.config {
            crate::config::mcp::McpServerConfig::Stdio(config) => {
                let mut command = config.command.clone();
                if let Some(args) = &config.args {
                    for arg in args {
                        command.push(' ');
                        command.push_str(arg);
                    }
                }
                command
            }
            crate::config::mcp::McpServerConfig::Http(config) => config.url.clone(),
        };
        let mut state = "disabled".to_owned();
        let mut tools: Vec<String> = Vec::new();
        let mut resources = None;
        let mut error = None;
        if enabled {
            match manager
                .check_server_report(&entry.name, MCP_CHECK_TIMEOUT)
                .await
            {
                Ok((names, has_resources)) => {
                    state = "connected".to_owned();
                    tools = names;
                    resources = has_resources.then_some(true);
                }
                Err(check_error) => {
                    failed = true;
                    let states = manager.connection_states().await;
                    let connection_state = states.get(&entry.name);
                    state = match connection_state {
                        Some(crate::mcp::manager::ConnectionState::NeedsAuth { .. }) => {
                            "needs-auth".to_owned()
                        }
                        Some(crate::mcp::manager::ConnectionState::Disconnected { .. }) => {
                            "disconnected".to_owned()
                        }
                        Some(crate::mcp::manager::ConnectionState::Closed) => "closed".to_owned(),
                        _ => "failed".to_owned(),
                    };
                    // The state carries the connect/sign-in failure; fall
                    // back to the check's own error text.
                    error = connection_state
                        .and_then(crate::mcp::manager::ConnectionState::error)
                        .map(str::to_owned)
                        .or_else(|| Some(check_error.to_string()));
                }
            }
        }
        let tool_exposure = if tools.is_empty() {
            None
        } else {
            let overrides: BTreeMap<String, String> = tools
                .iter()
                .filter_map(|tool| {
                    let exposure = crate::mcp::registry::get_mcp_tool_exposure(&entry.config, tool);
                    (exposure != base_exposure).then(|| {
                        (
                            tool.clone(),
                            crate::mcp::manager::exposure_label(exposure).to_owned(),
                        )
                    })
                })
                .collect();
            (!overrides.is_empty()).then_some(overrides)
        };
        reports.push(McpListEntry {
            name: entry.name,
            scope: scope.to_owned(),
            source,
            override_path: None,
            enabled,
            exposure: crate::mcp::manager::exposure_label(base_exposure).to_owned(),
            transport,
            state,
            tools,
            tool_exposure,
            resources,
            error,
        });
    }
    manager.shutdown().await;

    if json {
        let output = McpListOutput {
            servers: reports,
            errors: config_errors,
            note: untrusted_note,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&output)
                .unwrap_or_else(|_| "{\"servers\":[],\"errors\":[]}".to_owned())
        );
        return Ok(!failed);
    }
    if reports.is_empty() && config_errors.is_empty() {
        println!(
            "No MCP servers configured. Add them to {} or .pi/mcp.json.",
            crate::config::mcp::global_mcp_path(&agent_dir).display()
        );
    }
    for report in &reports {
        let state = if report.state == "connected" {
            format!(
                "connected, {} tool{}",
                report.tools.len(),
                if report.tools.len() == 1 { "" } else { "s" }
            )
        } else if report.state == "needs-auth" {
            "needs sign-in".to_owned()
        } else {
            report.state.clone()
        };
        println!(
            "{}: {} ({}, {})",
            report.name, state, report.exposure, report.scope
        );
        println!("  {}", report.transport);
        if let Some(override_path) = &report.override_path {
            println!("  project override: {override_path}");
        }
        if report.state == "needs-auth" {
            println!("  sign in with: pir mcp login {}", report.name);
        }
        if !report.tools.is_empty() {
            let tools = report
                .tools
                .iter()
                .map(
                    |tool| match report.tool_exposure.as_ref().and_then(|map| map.get(tool)) {
                        Some(exposure) => format!("{tool} [{exposure}]"),
                        None => tool.clone(),
                    },
                )
                .collect::<Vec<_>>()
                .join(", ");
            println!("  tools: {tools}");
        }
        if report.resources == Some(true) {
            println!("  offers resources");
        }
        if let Some(error) = &report.error {
            for line in error.lines() {
                println!("  {line}");
            }
        }
    }
    for config_error in &config_errors {
        println!("config error: {config_error}");
    }
    if let Some(note) = &untrusted_note {
        println!("{note}");
    }
    Ok(!failed)
}

/// The bundled short flags clap cannot express (upstream args.ts spellings),
/// normalized to their long forms by [`preprocess_args`].
const BUNDLED_SHORTS: &[(&str, &str)] = &[
    ("-nc", "--no-context-files"),
    ("-nt", "--no-tools"),
    ("-nbt", "--no-builtin-tools"),
    ("-ne", "--no-extensions"),
    ("-ns", "--no-skills"),
    ("-np", "--no-prompt-templates"),
    ("-na", "--no-approve"),
    ("-a", "--approve"),
    ("-t", "--tools"),
    ("-xt", "--exclude-tools"),
    ("-n", "--name"),
    // Upstream `-v` prints the version (main.ts: `--version` || `-v`).
    ("-v", "--version"),
];

/// Long flags pir itself understands. Anything else in `--flag` form is
/// collected as an extension flag (upstream `unknownFlags`).
const KNOWN_LONG_FLAGS: &[&str] = &[
    "extension",
    "mode",
    "json",
    "continue",
    "resume",
    "session",
    "session-id",
    "fork",
    "no-session",
    "no-context-files",
    "provider",
    "model",
    "api-key",
    "system-prompt",
    "append-system-prompt",
    "thinking",
    "session-dir",
    "name",
    "models",
    "no-tools",
    "no-builtin-tools",
    "tools",
    "exclude-tools",
    "no-extensions",
    "only-builtin",
    "no-mcp",
    "skill",
    "no-skills",
    "prompt-template",
    "no-prompt-templates",
    "theme",
    "use-theme",
    "no-themes",
    "list-models",
    "export",
    "verbose",
    "tui-mode",
    "approve",
    "no-approve",
    "offline",
    // `clap`-defined longs that the list must not swallow: a missing entry
    // makes `preprocess_args` treat the flag as an extension flag and drop it
    // (with a detached value) from argv before clap ever sees it — e.g.
    // `pir --print "msg"` lost both the flag and the message.
    "print",
    // `Server --unix <path>`: silently degrading to TCP was a transport
    // safety bug, not only a lost flag.
    "unix",
    // The `install`/`remove`/`uninstall`/`config` `--local` (`-l`) spelling,
    // plus `config --json/--enable/--disable/--source` and
    // `auth check --json` (the latter already covered above).
    "local",
    "enable",
    "disable",
    "source",
    // `update --extensions|--all|--self|--force|--extension`.
    "extensions",
    "all",
    "self",
    "update-extension",
    "force",
    // `mcp login --timeout <seconds>`.
    "timeout",
    // `mcp add`: `--url`, `--env`, `--cwd`, `--header`,
    // `--bearer-token-env-var`, `--oauth-client-id`, `--oauth-client-secret`,
    // `--oauth-callback-port`, `--oauth-client-name`, `--exposure`,
    // `--description`.
    "url",
    "env",
    "cwd",
    "header",
    "bearer-token-env-var",
    "oauth-client-id",
    "oauth-client-secret",
    "oauth-callback-port",
    "oauth-client-name",
    "exposure",
    "description",
    // `auth login --agent-name`, `auth check --json --credentials
    // --no-refresh`, `auth print-bearer-token --min-expiry`.
    "agent-name",
    "credentials",
    "no-refresh",
    "min-expiry",
    // `--no-extensions`' clap alias (`--ne` is a valid long spelling, the
    // bundled `-ne` shorthand is normalized separately by BUNDLED_SHORTS).
    "ne",
    "help",
    // `--help-extensions` is a real clap flag (not an extension-registered
    // one), so the collector must leave it in argv.
    "help-extensions",
    "version",
];

/// Collected unknown `--flag[=value]` arguments from [`preprocess_args`],
/// readable by the extension loader after startup flag parsing (the extension
/// runtime applies them to `registerFlag` definitions once loaded; the map is
/// empty when no unknown flags were seen).
static COLLECTED_UNKNOWN_FLAGS: std::sync::OnceLock<BTreeMap<String, String>> =
    std::sync::OnceLock::new();

/// Names of the collected unknown flags written WITHOUT a value (the upstream
/// `unknownFlags` map stores boolean `true` for these). A registered string
/// flag in this set is the upstream `Extension flag "--name" requires a
/// value` error diagnostic; an explicit `--name=` or `--name ""` carries a
/// (possibly empty) string value and is accepted.
static COLLECTED_UNKNOWN_FLAGS_WITHOUT_VALUE: std::sync::OnceLock<BTreeSet<String>> =
    std::sync::OnceLock::new();

/// The unknown flags collected by [`preprocess_args`] (first call wins; the
/// CLI preprocesses argv exactly once per process).
pub fn collected_unknown_flags() -> BTreeMap<String, String> {
    COLLECTED_UNKNOWN_FLAGS.get().cloned().unwrap_or_default()
}

/// The collected unknown flag names that arrived without a value (see
/// [`COLLECTED_UNKNOWN_FLAGS_WITHOUT_VALUE`]).
pub fn collected_unknown_flags_without_value() -> BTreeSet<String> {
    COLLECTED_UNKNOWN_FLAGS_WITHOUT_VALUE
        .get()
        .cloned()
        .unwrap_or_default()
}

/// Rewrite upstream `pi update --extension <source>` to the clap-internal
/// `--update-extension` spelling. The global `-e/--extension` already owns the
/// `--extension` long (clap requires unique longs per command tree), so the
/// update-only spelling is normalized before parsing exactly like the bundled
/// short flags. Only arguments after the `update` subcommand are rewritten;
/// `pir --extension <path> update` keeps its global meaning.
fn rewrite_update_extension(mut args: Vec<String>) -> Vec<String> {
    // Upstream `handlePackageCommand` only recognises the command when it is
    // the first argument (`const [rawCommand, ...rest] = args`), so the
    // rewrite is anchored the same way; a prompt that happens to be the word
    // "update" in a later position never reaches it.
    if args.first().map(String::as_str) != Some("update") {
        return args;
    }
    for arg in args.iter_mut().skip(1) {
        if arg == "--extension" {
            *arg = "--update-extension".to_owned();
        } else if let Some(value) = arg.strip_prefix("--extension=") {
            *arg = format!("--update-extension={value}");
        }
    }
    args
}

/// Preprocess raw argv: normalize the bundled upstream short flags
/// (`-nt`, `-xt value`, …) to their long forms, rewrite `update --extension`,
/// collect unknown `--flag[=value]` options for extensions (values attach when
/// the next argument is not a flag), and pass everything else through to
/// clap. Returns the cleaned argv and the collected unknown-flag map.
pub fn preprocess_args(args: Vec<String>) -> (Vec<String>, BTreeMap<String, String>) {
    // Hard error on an invalid `--mode` (upstream exits 1 before doing any
    // work; a silent fallback would run the wrong mode).
    if let Err(message) = validate_mode_arg(&args) {
        eprintln!("Error: {message}");
        std::process::exit(1);
    }
    let args = rewrite_update_extension(args);
    let mut cleaned: Vec<String> = Vec::with_capacity(args.len());
    let mut unknown: BTreeMap<String, String> = BTreeMap::new();
    let mut unknown_without_value: BTreeSet<String> = BTreeSet::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].clone();
        if arg == "--" {
            cleaned.extend(args[index..].iter().cloned());
            break;
        }
        // Bundled short normalization (exact matches only).
        if let Some((_, long)) = BUNDLED_SHORTS.iter().find(|(short, _)| *short == arg) {
            cleaned.push((*long).to_string());
            index += 1;
            continue;
        }
        if let Some(rest) = arg.strip_prefix("--") {
            if let Some((name, value)) = rest.split_once('=') {
                if !KNOWN_LONG_FLAGS.contains(&name) {
                    unknown.insert(name.to_string(), value.to_string());
                    index += 1;
                    continue;
                }
            } else {
                let name = rest;
                if !KNOWN_LONG_FLAGS.contains(&name) {
                    // Attach the next argument as the value when it does not
                    // look like a flag or a `@file` reference (upstream
                    // unknown-flag handling).
                    let next = args.get(index + 1);
                    let attach =
                        next.is_some_and(|next| !next.starts_with('-') && !next.starts_with('@'));
                    if attach {
                        unknown.insert(name.to_string(), args[index + 1].clone());
                        index += 2;
                    } else {
                        unknown.insert(name.to_string(), "true".to_string());
                        unknown_without_value.insert(name.to_string());
                        index += 1;
                    }
                    continue;
                }
            }
        }
        cleaned.push(arg);
        index += 1;
    }
    let _ = COLLECTED_UNKNOWN_FLAGS.set(unknown.clone());
    let _ = COLLECTED_UNKNOWN_FLAGS_WITHOUT_VALUE.set(unknown_without_value);
    (cleaned, unknown)
}
