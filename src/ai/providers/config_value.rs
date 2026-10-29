//! Configuration value templates (upstream `core/resolve-config-value.ts`):
//! API keys and header values in `models.json` may be shell commands
//! (`!command`), environment variable templates (`$ENV`, `${ENV}`), or
//! literals. `$$` escapes a literal `$` and `$!` escapes a literal `!` in
//! non-command values. Command results are bounded to a 10s timeout and
//! cached per (agent dir, command), validated against the agent dir's config
//! files on every lookup (see [`command_cache`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::settings::FileStamp;

/// The upstream per-command timeout (`resolve-config-value.ts`).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Upstream `execSync`'s default `maxBuffer`: a command whose stdout exceeds
/// this many bytes is treated as failed (`undefined`), never truncated.
const COMMAND_OUTPUT_CAP_BYTES: usize = 1024 * 1024;

/// Capacity bound of the `!command` result cache: at most this many
/// `(agent dir, command)` entries are retained (least-recently-used evicted
/// first), so a session that rotates many agent dirs or command strings
/// cannot grow the map without limit.
const COMMAND_CACHE_CAPACITY: usize = 64;

/// The agent-dir files that can carry a `!command` value. A change to any of
/// them invalidates every cached command result (see [`command_cache`]).
const COMMAND_CONFIG_FILES: [&str; 3] = ["models.json", "mcp.json", "auth.json"];

/// One cached `!command` result: the source it came from (resolved agent dir +
/// the command text after the leading `!`) and the config stamps observed when
/// it ran.
struct CommandCacheEntry {
    agent_dir: PathBuf,
    command: String,
    config_stamps: [Option<FileStamp>; COMMAND_CONFIG_FILES.len()],
    value: Option<String>,
}

/// Cache of `!command` results, keyed by `(resolved agent dir, command)`.
///
/// **Lifetime / invalidation.** An entry is served only while both hold:
/// 1. the resolved agent dir is the one the entry was computed for, and
/// 2. the agent dir's config sources ([`COMMAND_CONFIG_FILES`]) still carry
///    the same length+mtime stamps (`FileStamp`).
///
/// So (a) a `PIR_AGENT_DIR` change re-runs the command against the new dir
/// instead of replaying the old dir's output (the audit's D7 defect), and
/// (b) editing/creating/removing `models.json` / `mcp.json` / `auth.json`
/// re-runs the command. Validation costs one `metadata` call per config file
/// — never a read or parse — so the fast path stays a stamp comparison.
/// A command whose *output* depends on a file outside the agent dir (or on
/// state mutated in place without changing any of those files) is not
/// detected; hosts that know better call [`clear_config_value_cache`].
///
/// **Bound.** [`COMMAND_CACHE_CAPACITY`] entries, least-recently-used evicted
/// first.
fn command_cache() -> &'static Mutex<Vec<CommandCacheEntry>> {
    static CACHE: std::sync::OnceLock<Mutex<Vec<CommandCacheEntry>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Clear the `!command` result cache (upstream `clearConfigValueCache`):
/// forces every command value to re-run on its next resolution.
pub fn clear_config_value_cache() {
    let mut guard = command_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.clear();
}

/// Number of cached `!command` results (diagnostic/test seam; never exceeds
/// [`COMMAND_CACHE_CAPACITY`]).
pub fn command_cache_len() -> usize {
    command_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len()
}

/// The current stamps of the config sources that can declare a `!command`
/// value under `agent_dir`.
fn command_config_stamps(agent_dir: &Path) -> [Option<FileStamp>; COMMAND_CONFIG_FILES.len()] {
    COMMAND_CONFIG_FILES.map(|name| crate::config::settings::file_stamp(&agent_dir.join(name)))
}

/// One parsed template part.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplatePart {
    Literal(String),
    Env(String),
}

/// One parsed config value reference.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigValueReference {
    Command(String),
    Template(Vec<TemplatePart>),
}

fn is_env_var_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_env_var_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn append_literal(parts: &mut Vec<TemplatePart>, value: &str) {
    if value.is_empty() {
        return;
    }
    if let Some(TemplatePart::Literal(previous)) = parts.last_mut() {
        previous.push_str(value);
        return;
    }
    parts.push(TemplatePart::Literal(value.to_string()));
}

/// Parse `$ENV` / `${ENV}` template parts (upstream
/// `parseConfigValueTemplate`): `$$` and `$!` escape a literal `$`/`!`;
/// `${...}` with a non-identifier name is kept verbatim.
fn parse_template(config: &str) -> Vec<TemplatePart> {
    let chars: Vec<char> = config.chars().collect();
    let mut parts: Vec<TemplatePart> = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let Some(dollar_index) = chars[index..].iter().position(|ch| *ch == '$') else {
            let rest: String = chars[index..].iter().collect();
            append_literal(&mut parts, &rest);
            break;
        };
        let dollar_index = index + dollar_index;
        let before: String = chars[index..dollar_index].iter().collect();
        append_literal(&mut parts, &before);
        let next = chars.get(dollar_index + 1).copied();
        match next {
            // `$$` / `$!` escape a literal `$` / `!`.
            Some('$') | Some('!') => {
                let escaped = next.unwrap();
                append_literal(&mut parts, &escaped.to_string());
                index = dollar_index + 2;
            }
            Some('{') => {
                let close = chars[dollar_index + 2..]
                    .iter()
                    .position(|ch| *ch == '}')
                    .map(|offset| dollar_index + 2 + offset);
                match close {
                    Some(end) => {
                        let name: String = chars[dollar_index + 2..end].iter().collect();
                        if !name.is_empty()
                            && is_env_var_start(name.chars().next().unwrap_or('_'))
                            && name.chars().all(is_env_var_char)
                        {
                            parts.push(TemplatePart::Env(name));
                        } else {
                            let raw: String = chars[dollar_index..=end].iter().collect();
                            append_literal(&mut parts, &raw);
                        }
                        index = end + 1;
                    }
                    None => {
                        append_literal(&mut parts, "$");
                        index = dollar_index + 1;
                    }
                }
            }
            Some(ch) if is_env_var_start(ch) => {
                let name: String = chars[dollar_index + 1..]
                    .iter()
                    .copied()
                    .take_while(|ch| is_env_var_char(*ch))
                    .collect();
                index = dollar_index + 1 + name.chars().count();
                parts.push(TemplatePart::Env(name));
            }
            _ => {
                append_literal(&mut parts, "$");
                index = dollar_index + 1;
            }
        }
    }
    parts
}

fn parse_reference(config: &str) -> ConfigValueReference {
    if let Some(command) = config.strip_prefix('!') {
        return ConfigValueReference::Command(command.to_string());
    }
    ConfigValueReference::Template(parse_template(config))
}

fn env_lookup(name: &str, env: Option<&BTreeMap<String, String>>) -> Option<String> {
    if let Some(env) = env {
        if let Some(value) = env.get(name) {
            return Some(value.clone());
        }
    }
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn resolve_template(
    parts: &[TemplatePart],
    env: Option<&BTreeMap<String, String>>,
) -> Option<String> {
    let mut resolved = String::new();
    for part in parts {
        match part {
            TemplatePart::Literal(value) => resolved.push_str(value),
            TemplatePart::Env(name) => resolved.push_str(&env_lookup(name, env)?),
        }
    }
    Some(resolved)
}

/// The single environment variable a config value references, when the value
/// is exactly `$NAME` (upstream `getConfigValueEnvVarName`).
pub fn get_config_value_env_var_name(config: &str) -> Option<String> {
    let reference = parse_reference(config);
    match reference {
        ConfigValueReference::Template(parts) => match parts.as_slice() {
            [TemplatePart::Env(name)] => Some(name.clone()),
            _ => None,
        },
        ConfigValueReference::Command(_) => None,
    }
}

/// All environment variable names referenced by a template value (upstream
/// `getConfigValueEnvVarNames`).
pub fn get_config_value_env_var_names(config: &str) -> Vec<String> {
    let reference = parse_reference(config);
    match reference {
        ConfigValueReference::Template(parts) => {
            let mut names = Vec::new();
            for part in parts {
                if let TemplatePart::Env(name) = part {
                    if !names.contains(&name) {
                        names.push(name);
                    }
                }
            }
            names
        }
        ConfigValueReference::Command(_) => Vec::new(),
    }
}

/// The missing environment variables for a template value (upstream
/// `getMissingConfigValueEnvVarNames`).
fn get_missing_config_value_env_var_names(
    config: &str,
    env: Option<&BTreeMap<String, String>>,
) -> Vec<String> {
    get_config_value_env_var_names(config)
        .into_iter()
        .filter(|name| env_lookup(name, env).is_none())
        .collect()
}

/// Whether a config value is a `!command` reference (upstream
/// `isCommandConfigValue`).
pub fn is_command_config_value(config: &str) -> bool {
    matches!(parse_reference(config), ConfigValueReference::Command(_))
}

/// Whether a config value can resolve (command values count as configured;
/// templates need every referenced variable present).
pub fn is_config_value_configured(config: &str, env: Option<&BTreeMap<String, String>>) -> bool {
    get_missing_config_value_env_var_names(config, env).is_empty()
}

/// Resolve a `!command` value by executing the rest of the string through the
/// system shell, trimmed (upstream `executeCommand`); bounded to a 10s wait
/// and cached per (agent dir, command), validated against the agent dir's
/// config files on every lookup (see [`command_cache`]).
fn execute_command(command: &str) -> Option<String> {
    execute_command_in(&crate::ai::providers::agent_dir(), command)
}

/// [`execute_command`] against an EXPLICIT config source (the agent dir whose
/// `models.json` / `mcp.json` / `auth.json` declare the value).
fn execute_command_in(agent_dir: &Path, command: &str) -> Option<String> {
    let stamps = command_config_stamps(agent_dir);
    let mut guard = command_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(index) = guard
        .iter()
        .position(|entry| entry.agent_dir.as_path() == agent_dir && entry.command == command)
    {
        if guard[index].config_stamps == stamps {
            // LRU refresh: move the hit to the back (eviction order).
            let entry = guard.remove(index);
            let value = entry.value.clone();
            guard.push(entry);
            return value;
        }
        // The config under this agent dir changed: this entry is stale.
        guard.remove(index);
    }
    // The command runs OUTSIDE the lock: the cache lock must not be held for
    // the command's full 10s timeout, or one slow command value would stall
    // every other config-value resolution.
    drop(guard);
    let result = execute_command_with_timeout(command, COMMAND_TIMEOUT);
    // Cache only when no config edit landed while the command ran: a value
    // produced against the pre-edit config must not be stamped as current.
    if command_config_stamps(agent_dir) != stamps {
        return result;
    }
    let mut guard = command_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !guard
        .iter()
        .any(|entry| entry.agent_dir.as_path() == agent_dir && entry.command == command)
    {
        if guard.len() >= COMMAND_CACHE_CAPACITY {
            guard.remove(0);
        }
        guard.push(CommandCacheEntry {
            agent_dir: agent_dir.to_path_buf(),
            command: command.to_string(),
            config_stamps: stamps,
            value: result.clone(),
        });
    }
    result
}

/// Execute one command with an explicit timeout (the testable core of
/// [`execute_command`]). The child is killed when the deadline passes; a
/// killed or failed command resolves to `None`.
///
/// Shell selection follows upstream `executeCommandUncached`
/// (`resolve-config-value.ts:198-206`): on Windows the command runs through
/// `getShellConfig()`'s platform default (the embedded shell when this build
/// carries one, else Git Bash, else `bash.exe` on PATH; upstream calls it with
/// no custom path, so the `shellPath` SETTING does not apply to `!command`) and
/// falls back to `execSync`'s default shell only when that shell could not be
/// STARTED at all. A configured shell that started and then failed the command
/// yields `None` with no second attempt — retrying under a different shell
/// would make a config value's meaning depend on which shell happened to work.
///
/// Why not `cmd /C` unconditionally: `cmd` treats `\"` inside an argument as
/// an escape, so a quoted path that arrives through `%VAR%` expansion — the
/// documented `!command` idiom — fails with "The filename, directory name, or
/// volume label syntax is incorrect" (measured on Windows 11). The bash
/// transport keeps the command as one `-c` argument, which is why upstream
/// prefers it and why the rest of pir already resolves shells that way.
pub fn execute_command_with_timeout(command: &str, timeout: Duration) -> Option<String> {
    // Upstream `executeWithConfiguredShell`: an explicit `shellPath` setting,
    // else the platform default (Git Bash on Windows, `/bin/bash` or `sh`
    // elsewhere). `Err` means "no configured shell could be resolved", which
    // upstream models as `executed: false`.
    // Upstream's `executed` distinction: a spawn FAILURE (the shell binary is
    // missing) falls back to the default shell; a spawned shell that exits
    // non-zero does not.
    if let Ok(config) = crate::coding_agent::tools::resolve_shell_config(None) {
        if let Some(result) = run_shell_command(&config, command, timeout) {
            return result;
        }
    }
    // `execSync`'s default shell: `cmd /C` on Windows, `sh -c` elsewhere.
    let fallback = fallback_shell_config();
    run_shell_command(&fallback, command, timeout).unwrap_or(None)
}

/// The shell `execSync` uses when no shell was configured: `cmd /C` on
/// Windows, `sh -c` elsewhere.
fn fallback_shell_config() -> crate::coding_agent::tools::ShellConfig {
    #[cfg(windows)]
    {
        crate::coding_agent::tools::ShellConfig::explicit("cmd", vec!["/C"], false)
    }
    #[cfg(not(windows))]
    {
        crate::coding_agent::tools::ShellConfig::explicit("sh", vec!["-c"], false)
    }
}

/// Run `command` through `config`, returning `None` when the shell could not
/// be STARTED (upstream `executed: false`, which is what triggers the
/// fallback) and `Some(value)` — possibly `Some(None)` — once it ran. A
/// command whose stdout exceeds [`COMMAND_OUTPUT_CAP_BYTES`] resolves
/// `Some(None)`, matching upstream `execSync`'s `maxBuffer` overflow; a
/// truncated value is never returned.
fn run_shell_command(
    config: &crate::coding_agent::tools::ShellConfig,
    command: &str,
    timeout: Duration,
) -> Option<Option<String>> {
    let mut child = config.spawn(command).ok()?;
    #[cfg(windows)]
    let child_pid = child.id();
    // A stdin-transport shell (legacy WSL bash) takes the command on stdin;
    // close it so the shell sees EOF and starts.
    if config.stdin_transport() {
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(command.as_bytes());
            drop(stdin);
        }
    }
    // Drain stdout on a thread so a chatty command cannot fill the pipe
    // buffer and deadlock; the output is capped to keep the read bounded.
    let stdout = child.stdout.take();
    let (output_tx, output_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut overflowed = false;
        if let Some(mut stdout) = stdout {
            let mut chunk = [0u8; 4096];
            loop {
                match std::io::Read::read(&mut stdout, &mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        out.extend_from_slice(&chunk[..read]);
                        if out.len() > COMMAND_OUTPUT_CAP_BYTES {
                            overflowed = true;
                            break;
                        }
                    }
                }
            }
        }
        let _ = output_tx.send((out, overflowed));
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                // On Windows the shell outlives its grandchildren; kill the
                // tree so a spawned grandchild cannot hold the stdout pipe
                // open past the deadline.
                #[cfg(windows)]
                {
                    let _ = std::process::Command::new("taskkill")
                        .args(["/PID", &child_pid.to_string(), "/T", "/F"])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => break None,
        }
    };
    // A killed tree closes the pipe promptly; keep a small grace window so a
    // stuck reader cannot hang the caller past the deadline.
    let (output, overflowed) = if status.is_some() {
        match output_rx.recv() {
            Ok(output) => output,
            Err(_) => return Some(None),
        }
    } else {
        match output_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(output) => output,
            Err(_) => return Some(None),
        }
    };
    // Upstream `execSync` throws when `maxBuffer` is exceeded and the caller
    // maps that to `undefined`; never hand a truncated value to a credential
    // or header consumer.
    if overflowed {
        return Some(None);
    }
    match status {
        Some(status) if status.success() => {
            let text = String::from_utf8_lossy(&output).trim().to_string();
            Some(Some(text).filter(|value| !value.is_empty()))
        }
        _ => Some(None),
    }
}

/// Resolve a config value (API key, header value, …) to its actual value
/// (upstream `resolveConfigValue`).
///
/// Ambient boundary: a `!command` value resolves against the process's
/// resolved agent dir (its config sources). Hosts that own their config source
/// call [`resolve_config_value_in`] directly.
pub fn resolve_config_value(
    config: &str,
    env: Option<&BTreeMap<String, String>>,
) -> Option<String> {
    match parse_reference(config) {
        ConfigValueReference::Command(command) => execute_command(&command),
        ConfigValueReference::Template(parts) => resolve_template(&parts, env),
    }
}

/// [`resolve_config_value`] with an EXPLICIT config source: a `!command` value
/// is cached under (and invalidated by) `agent_dir`'s config files, so a host
/// bound to one agent dir never serves another dir's command output.
pub fn resolve_config_value_in(
    agent_dir: &Path,
    config: &str,
    env: Option<&BTreeMap<String, String>>,
) -> Option<String> {
    match parse_reference(config) {
        ConfigValueReference::Command(command) => execute_command_in(agent_dir, &command),
        ConfigValueReference::Template(parts) => resolve_template(&parts, env),
    }
}

/// Resolve every header value with the same logic as API keys, dropping
/// headers whose values do not resolve (upstream `resolveHeaders`).
pub fn resolve_headers(
    headers: Option<&BTreeMap<String, String>>,
    env: Option<&BTreeMap<String, String>>,
) -> Option<BTreeMap<String, String>> {
    let headers = headers?;
    resolve_headers_in(&crate::ai::providers::agent_dir(), Some(headers), env)
}

/// [`resolve_headers`] with an EXPLICIT config source: `!command` header
/// values are cached under, and invalidated by, `agent_dir`'s config files,
/// so an SDK agent bound to one agent dir never sends another dir's cached
/// header value.
pub fn resolve_headers_in(
    agent_dir: &Path,
    headers: Option<&BTreeMap<String, String>>,
    env: Option<&BTreeMap<String, String>>,
) -> Option<BTreeMap<String, String>> {
    let headers = headers?;
    let mut resolved = BTreeMap::new();
    for (key, value) in headers {
        // An empty value is the null-suppression marker (upstream
        // `ProviderHeaders` null): keep it so it can shadow a default header.
        if value.is_empty() {
            resolved.insert(key.clone(), String::new());
            continue;
        }
        if let Some(value) = resolve_config_value_in(agent_dir, value, env) {
            resolved.insert(key.clone(), value);
        }
    }
    if resolved.is_empty() {
        None
    } else {
        Some(resolved)
    }
}
