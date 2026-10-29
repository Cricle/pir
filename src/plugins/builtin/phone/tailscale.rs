//! Tailscale Serve integration (port of the reference `phone-tailscale.ts`).
//!
//! Each phone server claims one path-scoped route, `/phone-<port>`, and maps
//! it to its own loopback origin `http://127.0.0.1:<port>`. Because the route
//! is scoped by `--set-path`, concurrent pi sessions share a single Tailscale
//! Serve configuration without replacing (or tearing down) each other's
//! routes.
//!
//! Every invocation spawns `tailscale` directly (never through a shell) with
//! a five-second timeout. A missing binary is reported as a soft error string:
//! Tailscale is optional, so it can never fail a local phone server and never
//! panics.
//!
//! [`TailscaleRunner`] is the hermetic test seam: tests inject a runner that
//! records the exact argv and returns canned JSON, so they never touch a real
//! `tailscale` binary.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as JsonValue;

/// Timeout for one `tailscale` invocation (the reference `pi.exec` timeout).
pub const TAILSCALE_TIMEOUT: Duration = Duration::from_secs(5);

/// The per-instance URL path for this port (the reference `phoneServePath`).
pub fn serve_path(port: u16) -> String {
    format!("/phone-{port}")
}

/// One finished `tailscale` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Future returned by [`TailscaleRunner::run`].
pub type CommandFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CommandOutput, String>> + Send + 'a>>;

/// Spawns `tailscale` (or a test double) and returns its output.
///
/// Implementations must not block the async executor: the production runner
/// uses `tokio::process`. The runner receives the full argument list (the
/// program name itself is fixed by the runner), which is what the test seam
/// records and asserts.
pub trait TailscaleRunner: Send + Sync {
    fn run<'a>(&'a self, args: &'a [String], timeout: Duration) -> CommandFuture<'a>;

    /// Identifies the runner kind (`"process"`, `"noop"`, or `"custom"`).
    ///
    /// Used by tests to prove that a plugin instance can never spawn the CLI
    /// without actually running it.
    fn kind(&self) -> &'static str {
        "custom"
    }
}

/// A runner that never spawns anything.
///
/// `PhonePlugin::new()` (the default/test constructor) installs this so a
/// plugin built outside the builtin assembly cannot reconfigure a real
/// Tailscale Serve setup. Every invocation is a soft error, which `/phone-start`
/// reports as "Could not configure Tailscale Serve automatically" without
/// failing the local server. The production factory
/// ([`super::new_phone_plugin`]) installs [`ProcessTailscaleRunner`] instead.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopTailscaleRunner;

impl TailscaleRunner for NoopTailscaleRunner {
    fn run<'a>(&'a self, _args: &'a [String], _timeout: Duration) -> CommandFuture<'a> {
        Box::pin(async { Err(Tailscale::DISABLED_ERROR.to_owned()) })
    }

    fn kind(&self) -> &'static str {
        "noop"
    }
}

/// The production runner: `tokio::process::Command` with no shell, piped
/// stdout/stderr, `kill_on_drop`, and a hidden console window on Windows.
#[derive(Debug, Clone)]
pub struct ProcessTailscaleRunner {
    program: PathBuf,
}

impl ProcessTailscaleRunner {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl TailscaleRunner for ProcessTailscaleRunner {
    fn run<'a>(&'a self, args: &'a [String], timeout: Duration) -> CommandFuture<'a> {
        Box::pin(async move {
            let mut command = tokio::process::Command::new(&self.program);
            command
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            #[cfg(windows)]
            {
                // CREATE_NO_WINDOW: this may run from the TUI, which must not
                // flash a console for every status probe.
                command.creation_flags(0x0800_0000);
            }
            let program = self.program.display().to_string();
            let child = command
                .spawn()
                .map_err(|error| format!("failed to run {program}: {error}"))?;
            match tokio::time::timeout(timeout, child.wait_with_output()).await {
                Ok(Ok(output)) => Ok(CommandOutput {
                    code: output.status.code().unwrap_or(-1),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                }),
                Ok(Err(error)) => Err(format!("failed to run {program}: {error}")),
                Err(_) => Err(format!(
                    "{program} timed out after {} ms",
                    timeout.as_millis()
                )),
            }
        })
    }

    fn kind(&self) -> &'static str {
        "process"
    }
}

/// The Serve state of one phone port (the reference `getTailscaleServeInfo`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServeInfo {
    /// A Serve handler for `/phone-<port>` proxies to this port.
    pub active: bool,
    /// `https://<dns>/phone-<port>/`, or empty when Tailscale is unavailable.
    pub url: String,
    /// Any web handler existed before this instance touched the config.
    pub had_any_web_config: bool,
    /// Soft error (missing binary, failed command, unparseable output).
    pub error: String,
}

/// The result of enabling one phone route (the reference `enableTailscaleServe`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnableResult {
    pub enabled: bool,
    pub changed: bool,
    pub replaced_existing: bool,
    pub url: String,
    pub error: String,
}

/// The result of disabling one phone route (the reference
/// `disableMatchingTailscaleServe`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DisableResult {
    pub disabled: bool,
    pub error: String,
}

/// The tailscale argv for one `serve status --json` probe.
pub fn serve_status_args() -> Vec<String> {
    ["serve", "status", "--json"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// The tailscale argv enabling `/phone-<port>` (the exact reference argv).
///
/// The `--set-path` value deliberately has no leading slash: Tailscale rejects
/// `--https=443/phone-x` and a leading slash is rewritten by MSYS shells. The
/// proxy target is a bare origin because Serve strips the mounted prefix.
pub fn enable_args(port: u16) -> Vec<String> {
    [
        "serve".to_owned(),
        "--bg".to_owned(),
        "--yes".to_owned(),
        "--https=443".to_owned(),
        format!("--set-path=phone-{port}"),
        format!("http://127.0.0.1:{port}"),
    ]
    .to_vec()
}

/// The tailscale argv disabling only `/phone-<port>`.
pub fn disable_args(port: u16) -> Vec<String> {
    [
        "serve".to_owned(),
        "--yes".to_owned(),
        "--https=443".to_owned(),
        format!("--set-path=phone-{port}"),
        "off".to_owned(),
    ]
    .to_vec()
}

/// A Tailscale client. The default uses the `tailscale` binary from `PATH`;
/// [`Tailscale::with_runner`] makes the whole surface hermetic in tests.
#[derive(Clone)]
pub struct Tailscale {
    runner: Arc<dyn TailscaleRunner>,
}

impl Default for Tailscale {
    /// Safe default: no CLI is ever spawned. Production code asks for
    /// [`Tailscale::new`] explicitly.
    fn default() -> Self {
        Self::disabled()
    }
}

impl Tailscale {
    /// The soft error reported by [`NoopTailscaleRunner`].
    pub const DISABLED_ERROR: &'static str =
        "Tailscale Serve integration is disabled for this plugin instance.";

    /// The production client: resolves `tailscale` on `PATH` and spawns it.
    pub fn new() -> Self {
        Self::with_program("tailscale")
    }

    /// A client that never spawns the CLI (the `PhonePlugin::new()` default).
    pub fn disabled() -> Self {
        Self::with_runner(Arc::new(NoopTailscaleRunner))
    }

    /// Use a specific program path instead of resolving `tailscale` on `PATH`.
    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Self {
            runner: Arc::new(ProcessTailscaleRunner::new(program)),
        }
    }

    /// Inject a custom runner (test seam).
    pub fn with_runner(runner: Arc<dyn TailscaleRunner>) -> Self {
        Self { runner }
    }

    /// The injected runner's kind (`"process"`, `"noop"`, or `"custom"`).
    #[doc(hidden)]
    pub fn runner_kind(&self) -> &'static str {
        self.runner.kind()
    }

    async fn run(&self, args: &[String]) -> Result<CommandOutput, String> {
        self.runner.run(args, TAILSCALE_TIMEOUT).await
    }

    /// `https://<dns>/phone-<port>/`; empty when `tailscale status` fails.
    async fn tailscale_url(&self, port: u16) -> String {
        let Ok(output) = self.run(&["status".to_owned(), "--json".to_owned()]).await else {
            return String::new();
        };
        if output.code != 0 {
            return String::new();
        }
        let Some(payload) = parse_json(&output.stdout) else {
            return String::new();
        };
        let dns = payload
            .get("Self")
            .and_then(|value| value.get("DNSName"))
            .and_then(JsonValue::as_str)
            .unwrap_or("");
        let dns = dns.strip_suffix('.').unwrap_or(dns);
        if dns.is_empty() {
            String::new()
        } else {
            format!("https://{dns}{}/", serve_path(port))
        }
    }

    /// Read the Serve state for `port` (the reference `getTailscaleServeInfo`).
    pub async fn serve_info(&self, port: u16) -> ServeInfo {
        let url = self.tailscale_url(port).await;
        let output = match self.run(&serve_status_args()).await {
            Ok(output) => output,
            Err(error) => {
                return ServeInfo {
                    active: false,
                    url,
                    had_any_web_config: false,
                    error,
                };
            }
        };
        if output.code != 0 {
            return ServeInfo {
                active: false,
                url,
                had_any_web_config: false,
                error: command_error(
                    &output,
                    format!("tailscale serve status exited {}", output.code),
                ),
            };
        }

        let Some(payload) = parse_json(&output.stdout) else {
            return ServeInfo {
                active: false,
                url,
                had_any_web_config: false,
                error: "Failed to parse tailscale serve status output.".to_owned(),
            };
        };

        let services = payload.get("Web").and_then(JsonValue::as_object);
        let mut active = false;
        let mut had_any_web_config = false;
        let this_path = serve_path(port);
        let this_path_with_slash = format!("{this_path}/");
        if let Some(services) = services {
            had_any_web_config = !services.is_empty();
            'services: for service in services.values() {
                let Some(handlers) = service.get("Handlers").and_then(JsonValue::as_object) else {
                    continue;
                };
                for (path, handler) in handlers {
                    let normalized = normalize_handler_path(path);
                    let proxy = handler.get("Proxy").and_then(JsonValue::as_str);
                    let Some(proxy) = proxy else {
                        continue;
                    };
                    if proxy_targets_port(proxy, port)
                        && (normalized == this_path || normalized == this_path_with_slash)
                    {
                        active = true;
                        break 'services;
                    }
                }
            }
        }

        ServeInfo {
            active,
            url,
            had_any_web_config,
            error: String::new(),
        }
    }

    /// Enable (or confirm) this port's route (the reference
    /// `enableTailscaleServe`).
    pub async fn enable(&self, port: u16) -> EnableResult {
        let before = self.serve_info(port).await;
        if before.active {
            return EnableResult {
                enabled: true,
                changed: false,
                replaced_existing: false,
                url: before.url,
                error: String::new(),
            };
        }

        let output = match self.run(&enable_args(port)).await {
            Ok(output) => output,
            Err(error) => {
                return EnableResult {
                    enabled: false,
                    changed: false,
                    replaced_existing: before.had_any_web_config,
                    url: before.url,
                    error,
                };
            }
        };
        if output.code != 0 {
            return EnableResult {
                enabled: false,
                changed: false,
                replaced_existing: before.had_any_web_config,
                url: before.url,
                error: command_error(&output, format!("tailscale serve exited {}", output.code)),
            };
        }

        let after = self.serve_info(port).await;
        EnableResult {
            enabled: after.active || after.error.is_empty(),
            changed: true,
            replaced_existing: before.had_any_web_config,
            url: if after.url.is_empty() {
                before.url
            } else {
                after.url
            },
            error: if after.active {
                String::new()
            } else {
                after.error
            },
        }
    }

    /// Disable only this port's route (the reference
    /// `disableMatchingTailscaleServe`).
    ///
    /// The probe runs first so a stale or unrelated Serve config is never
    /// touched: `serve ... off` with `--set-path` removes exactly one mount.
    pub async fn disable(&self, port: u16) -> DisableResult {
        let info = self.serve_info(port).await;
        if !info.active {
            return DisableResult {
                disabled: false,
                error: info.error,
            };
        }

        match self.run(&disable_args(port)).await {
            Ok(output) if output.code == 0 => DisableResult {
                disabled: true,
                error: String::new(),
            },
            Ok(output) => DisableResult {
                disabled: false,
                error: command_error(
                    &output,
                    format!("tailscale serve --https=443 off exited {}", output.code),
                ),
            },
            Err(error) => DisableResult {
                disabled: false,
                error,
            },
        }
    }
}

/// Read the Serve state for `port` with the default `tailscale` client.
pub async fn serve_info(port: u16) -> ServeInfo {
    Tailscale::new().serve_info(port).await
}

/// Enable `port`'s route with the default `tailscale` client.
pub async fn enable(port: u16) -> EnableResult {
    Tailscale::new().enable(port).await
}

/// Disable `port`'s route with the default `tailscale` client.
pub async fn disable(port: u16) -> DisableResult {
    Tailscale::new().disable(port).await
}

fn parse_json(stdout: &str) -> Option<JsonValue> {
    let text = stdout.trim();
    let text = if text.is_empty() { "{}" } else { text };
    serde_json::from_str(text).ok()
}

fn command_error(output: &CommandOutput, fallback: String) -> String {
    let stderr = output.stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_owned();
    }
    let stdout = output.stdout.trim();
    if !stdout.is_empty() {
        return stdout.to_owned();
    }
    fallback
}

fn normalize_handler_path(path: &str) -> String {
    format!("/{}", path.trim_start_matches('/'))
}

/// Whether a Serve handler `Proxy` target is an HTTP origin on `port`'s
/// loopback interface (`127.0.0.1`, `localhost`, `::1`/`[::1]`).
fn proxy_targets_port(proxy: &str, port: u16) -> bool {
    let Some((scheme, rest)) = proxy.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.is_empty() {
        return false;
    }

    let (host, port_text) = if let Some(after_bracket) = authority.strip_prefix('[') {
        let Some((host, tail)) = after_bracket.split_once(']') else {
            return false;
        };
        let Some(port_text) = tail.strip_prefix(':') else {
            return false;
        };
        (format!("[{host}]"), port_text)
    } else {
        let Some((host, port_text)) = authority.rsplit_once(':') else {
            return false;
        };
        (host.to_owned(), port_text)
    };

    if port_text.is_empty() || !port_text.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    if port_text.parse::<u16>().ok() != Some(port) {
        return false;
    }
    matches!(
        host.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1" | "[::1]"
    )
}
