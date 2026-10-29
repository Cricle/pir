//! Configurable QuickJS resource limits for the extension runtime.
//!
//! Engine-free and always compiled (same discipline as
//! [`crate::extensions::types`]): the shape and the resolution rules are
//! shared by SDK builds without the `extensions` feature, the host runtime
//! (which applies them to the QuickJS `AsyncRuntime`) and tests.
//!
//! Resolution order, first wins:
//!
//! 1. an explicitly injected [`ExtensionLimits`] (`ExtensionConfig::limits`,
//!    the SDK/embedder/test seam) — used verbatim, env and settings are NOT
//!    overlaid;
//! 2. `PI_EXT_*` environment variables (operator/session override);
//! 3. the `extensionLimits` object of the **global** settings file
//!    (`~/.pi/agent/settings.json`, i.e. `$PIR_AGENT_DIR/settings.json`);
//! 4. the built-in defaults: a 128 MiB JS heap cap (low-resource default: a
//!    runaway extension must fail its own call instead of OOM-killing the
//!    host process), 16 MiB JS stack, 30 s call timeout, 30 min hook timeout,
//!    no GC threshold. `memoryMb`/`memory_bytes: None` (the explicit `0`
//!    spelling) restores QuickJS's unlimited heap.
//!
//! Project-scope `.pi/settings.json` is deliberately NOT consulted, not even
//! when the project is trusted: a repository must never be able to loosen the
//! host's own engine safety net (an extension loaded from that project could
//! otherwise opt out of the memory cap that keeps the host process alive).
//! This is the security boundary of the feature.
//!
//! `0` semantics: `PI_EXT_MEMORY_MB` / `PI_EXT_GC_THRESHOLD_MB` / `memoryMb`
//! / `gcThresholdMb` = `0` means "no limit" (QuickJS unlimited heap / default
//! GC pacing); the option fields become `None`. `stackMb` has no "unlimited"
//! (rquickjs interprets a value above 16 MiB as *no stack check*, which can
//! segfault the process on deep recursion), so `0` keeps the default and
//! values above 16 MiB are clamped to 16 MiB. Timeouts of `0` keep their
//! defaults (the interrupt-deadline machinery always needs a positive
//! duration). Invalid values are ignored and reported as diagnostics.

use std::path::Path;
use std::time::Duration;

use crate::config::settings::Settings;

/// Default JS stack for a runtime (the value the engine has always used).
pub const DEFAULT_STACK_BYTES: usize = 16 * 1024 * 1024;
/// Upper bound accepted for the JS stack. rquickjs maps anything above this
/// to QuickJS's "no check" (0), so a larger request is clamped instead of
/// silently disabling the guard that keeps deep recursion from crashing the
/// process.
pub const MAX_STACK_BYTES: usize = 16 * 1024 * 1024;

/// Default QuickJS heap cap (128 MiB): the low-resource safety net.
///
/// Without a cap, a runaway extension can grow the QuickJS heap until the
/// whole host process is OOM-killed (the mini-device deployment target has no
/// swap headroom). 128 MiB matches the codemode sandbox default
/// (`codemode.sandboxMemoryMb`) and is far above any extension's boot
/// footprint, while still containing an allocation loop to a failed call the
/// host survives. An operator can raise it (`extensionLimits.memoryMb` /
/// `PI_EXT_MEMORY_MB`) or disable it explicitly with `0`.
pub const DEFAULT_MEMORY_BYTES: usize = 128 * 1024 * 1024;

/// Native headroom reserved above the JS stack budget on the extension actor
/// thread: QuickJS's C frames, the Rust dispatch machinery and the shim code
/// all run on that same OS stack.
pub const ACTOR_STACK_HEADROOM_BYTES: usize = 8 * 1024 * 1024;

/// `PI_EXT_MEMORY_MB`: QuickJS heap cap in MiB (`0` = no cap).
pub const ENV_MEMORY_MB: &str = "PI_EXT_MEMORY_MB";
/// `PI_EXT_STACK_MB`: JS stack in MiB (clamped to 16; `0` keeps the default).
pub const ENV_STACK_MB: &str = "PI_EXT_STACK_MB";
/// `PI_EXT_GC_THRESHOLD_MB`: GC threshold in MiB (`0` = no threshold).
pub const ENV_GC_THRESHOLD_MB: &str = "PI_EXT_GC_THRESHOLD_MB";
/// `PI_EXT_CALL_TIMEOUT_MS`: per-call timeout in milliseconds (`0` keeps the
/// default).
pub const ENV_CALL_TIMEOUT_MS: &str = "PI_EXT_CALL_TIMEOUT_MS";
/// `PI_EXT_HOOK_TIMEOUT_MS`: reserved-prefix hook timeout in milliseconds
/// (`0` keeps the default).
pub const ENV_HOOK_TIMEOUT_MS: &str = "PI_EXT_HOOK_TIMEOUT_MS";
/// `PI_EXT_BEFORE_AGENT_START_TIMEOUT_MS`: how long the host waits for the
/// `before_agent_start` lifecycle dispatch before letting the run proceed
/// (`0` keeps the default).
pub const ENV_BEFORE_AGENT_START_TIMEOUT_MS: &str = "PI_EXT_BEFORE_AGENT_START_TIMEOUT_MS";

/// The six names [`ExtensionLimits::apply_process_env`] reads, in report
/// order (also the set tests use to build synthetic environments).
pub const ENV_NAMES: [&str; 6] = [
    ENV_MEMORY_MB,
    ENV_STACK_MB,
    ENV_GC_THRESHOLD_MB,
    ENV_CALL_TIMEOUT_MS,
    ENV_HOOK_TIMEOUT_MS,
    ENV_BEFORE_AGENT_START_TIMEOUT_MS,
];

/// Default budget for the `before_agent_start` lifecycle dispatch.
///
/// Deliberately shorter than [`crate::extensions::DEFAULT_CALL_TIMEOUT`]: the
/// first prompt must not wait for a lifecycle hook that is stuck behind
/// extension-owned background work (the pi-subagents advertisement scan is the
/// measured case). A handler that needs longer can raise
/// `extensionLimits.beforeAgentStartTimeoutMs`; a timed-out dispatch is
/// abandoned and the run proceeds with a warning.
pub const DEFAULT_BEFORE_AGENT_START_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolved QuickJS resource limits of one extension runtime.
///
/// The timeout fields are the INITIAL values handed to the runtime state;
/// `ExtensionHandle::set_call_timeout` still overrides `call_timeout` at
/// runtime (a host running a one-off long call uses that seam, not settings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionLimits {
    /// QuickJS heap cap; `None` = no cap (QuickJS default/unlimited). A
    /// positive value is applied with `AsyncRuntime::set_memory_limit`, which
    /// makes an over-limit allocation throw QuickJS's out-of-memory
    /// `InternalError` instead of growing the host process.
    pub memory_bytes: Option<usize>,
    /// JS stack size (`AsyncRuntime::set_max_stack_size`). Never 0 here: the
    /// resolver clamps to `1..=`[`MAX_STACK_BYTES`].
    pub stack_bytes: usize,
    /// GC threshold (`AsyncRuntime::set_gc_threshold`); `None` = leave
    /// QuickJS's default pacing.
    pub gc_threshold_bytes: Option<usize>,
    /// Per-call dispatch/tool timeout (the tokio deadline AND the QuickJS
    /// interrupt deadline both use it).
    pub call_timeout: Duration,
    /// Timeout of reserved-prefix hook calls (`__pir_stream:` /
    /// `__pir_route:`): a real model backend may legitimately stream for
    /// many minutes.
    pub hook_timeout: Duration,
    /// Budget for the `before_agent_start` lifecycle dispatch. Shorter than
    /// [`Self::call_timeout`] on purpose: the first prompt proceeds with a
    /// warning when a handler is stuck behind extension background work, and
    /// a late completion is discarded (`DEFAULT_BEFORE_AGENT_START_TIMEOUT`).
    pub before_agent_start_timeout: Duration,
}

impl Default for ExtensionLimits {
    /// Low-resource defaults: a 128 MiB heap cap (explicit `0`/`memoryMb: 0`
    /// restores the unlimited heap), 16 MiB JS stack, 30 s call timeout,
    /// 30 min hook timeout, QuickJS-default GC pacing.
    fn default() -> Self {
        Self {
            memory_bytes: Some(DEFAULT_MEMORY_BYTES),
            stack_bytes: DEFAULT_STACK_BYTES,
            gc_threshold_bytes: None,
            call_timeout: crate::extensions::types::DEFAULT_CALL_TIMEOUT,
            hook_timeout: crate::extensions::types::HOOK_CALL_TIMEOUT,
            before_agent_start_timeout: DEFAULT_BEFORE_AGENT_START_TIMEOUT,
        }
    }
}

impl ExtensionLimits {
    /// The host's resolution entry: an explicitly injected value wins
    /// verbatim; `None` loads the global settings file plus the process
    /// environment ([`Self::load`]).
    pub fn resolve(explicit: Option<ExtensionLimits>) -> Self {
        explicit.unwrap_or_else(Self::load)
    }

    /// Load the limits the way a host does: global settings file, then the
    /// process environment, then defaults. Diagnostics for ignored or
    /// clamped values are routed through `ext_diag!`.
    pub fn load() -> Self {
        let (limits, settings_diags) = Self::from_settings(&load_global_settings());
        let (limits, env_diags) = limits.apply_process_env();
        for diagnostic in settings_diags.into_iter().chain(env_diags) {
            crate::ext_diag!("pir-ext: {diagnostic}");
        }
        limits
    }

    /// Parse the `extensionLimits` object of a settings snapshot.
    ///
    /// The snapshot must be GLOBAL scope (see the module docs); the function
    /// itself is scope-agnostic and simply reads the typed field. Returns the
    /// limits plus one diagnostic per clamped/ignored value.
    pub fn from_settings(settings: &Settings) -> (Self, Vec<String>) {
        let mut limits = Self::default();
        let mut diagnostics = Vec::new();
        let Some(section) = settings.extension_limits.as_ref() else {
            return (limits, diagnostics);
        };
        if let Some(mb) = section.memory_mb {
            limits.memory_bytes = mb_to_bytes(mb);
        }
        if let Some(mb) = section.stack_mb {
            limits.stack_bytes = clamp_stack_mb(mb, &mut diagnostics);
        }
        if let Some(mb) = section.gc_threshold_mb {
            limits.gc_threshold_bytes = mb_to_bytes(mb);
        }
        if let Some(ms) = section.call_timeout_ms {
            match positive_ms(ms) {
                Some(timeout) => limits.call_timeout = timeout,
                None => diagnostics.push(format!(
                    "extensionLimits.callTimeoutMs = 0 ignored: the default {} s call timeout stands",
                    limits.call_timeout.as_secs()
                )),
            }
        }
        if let Some(ms) = section.hook_timeout_ms {
            match positive_ms(ms) {
                Some(timeout) => limits.hook_timeout = timeout,
                None => diagnostics.push(format!(
                    "extensionLimits.hookTimeoutMs = 0 ignored: the default {} min hook timeout stands",
                    limits.hook_timeout.as_secs() / 60
                )),
            }
        }
        if let Some(ms) = section.before_agent_start_timeout_ms {
            match positive_ms(ms) {
                Some(timeout) => limits.before_agent_start_timeout = timeout,
                None => diagnostics.push(format!(
                    "extensionLimits.beforeAgentStartTimeoutMs = 0 ignored: the default {} s before_agent_start budget stands",
                    limits.before_agent_start_timeout.as_secs()
                )),
            }
        }
        (limits, diagnostics)
    }

    /// Apply `PI_EXT_*` overrides over this value, read from `vars` (a
    /// synthetic `(name, value)` environment keeps the parse pure and
    /// testable). Returns the effective limits plus one diagnostic per
    /// ignored/clamped value; absent names leave the field untouched.
    pub fn apply_env(self, vars: &[(&str, &str)]) -> (Self, Vec<String>) {
        let mut limits = self;
        let mut diagnostics = Vec::new();
        if let Some(value) = lookup(vars, ENV_MEMORY_MB) {
            match parse_mb(ENV_MEMORY_MB, value) {
                Ok(bytes) => limits.memory_bytes = bytes,
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
        }
        if let Some(value) = lookup(vars, ENV_STACK_MB) {
            match parse_mb(ENV_STACK_MB, value) {
                Ok(Some(bytes)) => {
                    let clamped = bytes.min(MAX_STACK_BYTES);
                    if clamped != bytes {
                        diagnostics.push(format!(
                            "{ENV_STACK_MB}={value} clamped to {} MB: rquickjs treats larger stacks as \"no stack check\"",
                            MAX_STACK_BYTES / (1024 * 1024)
                        ));
                    }
                    limits.stack_bytes = clamped;
                }
                Ok(None) => diagnostics.push(format!(
                    "{ENV_STACK_MB}=0 ignored: an unchecked JS stack can crash the process; keeping {} MB",
                    DEFAULT_STACK_BYTES / (1024 * 1024)
                )),
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
        }
        if let Some(value) = lookup(vars, ENV_GC_THRESHOLD_MB) {
            match parse_mb(ENV_GC_THRESHOLD_MB, value) {
                Ok(bytes) => limits.gc_threshold_bytes = bytes,
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
        }
        if let Some(value) = lookup(vars, ENV_CALL_TIMEOUT_MS) {
            match parse_ms(ENV_CALL_TIMEOUT_MS, value) {
                Ok(Some(ms)) => limits.call_timeout = ms,
                Ok(None) => diagnostics.push(format!(
                    "{ENV_CALL_TIMEOUT_MS}=0 ignored: the default {} s call timeout stands",
                    limits.call_timeout.as_secs()
                )),
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
        }
        if let Some(value) = lookup(vars, ENV_HOOK_TIMEOUT_MS) {
            match parse_ms(ENV_HOOK_TIMEOUT_MS, value) {
                Ok(Some(ms)) => limits.hook_timeout = ms,
                Ok(None) => diagnostics.push(format!(
                    "{ENV_HOOK_TIMEOUT_MS}=0 ignored: the default {} min hook timeout stands",
                    limits.hook_timeout.as_secs() / 60
                )),
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
        }
        if let Some(value) = lookup(vars, ENV_BEFORE_AGENT_START_TIMEOUT_MS) {
            match parse_ms(ENV_BEFORE_AGENT_START_TIMEOUT_MS, value) {
                Ok(Some(ms)) => limits.before_agent_start_timeout = ms,
                Ok(None) => diagnostics.push(format!(
                    "{ENV_BEFORE_AGENT_START_TIMEOUT_MS}=0 ignored: the default {} s before_agent_start budget stands",
                    limits.before_agent_start_timeout.as_secs()
                )),
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
        }
        (limits, diagnostics)
    }

    /// [`Self::apply_env`] against the real process environment (exactly the
    /// [`ENV_NAMES`] set; every other variable is out of scope).
    pub fn apply_process_env(self) -> (Self, Vec<String>) {
        let vars: Vec<(String, String)> = ENV_NAMES
            .iter()
            .filter_map(|name| {
                std::env::var(name)
                    .ok()
                    .map(|value| ((*name).to_string(), value))
            })
            .collect();
        let borrowed: Vec<(&str, &str)> = vars
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        self.apply_env(&borrowed)
    }

    /// The configured heap cap in MiB, for error/diagnostic text.
    pub fn memory_mb(&self) -> Option<u64> {
        self.memory_bytes
            .map(|bytes| (bytes / (1024 * 1024)) as u64)
    }

    /// The configured stack in MiB.
    pub fn stack_mb(&self) -> u64 {
        (self.stack_bytes / (1024 * 1024)) as u64
    }

    /// OS thread stack for the extension actor.
    ///
    /// QuickJS's JS stack budget and the host's native frames share the actor
    /// thread's stack, so reserve twice the JS stack plus
    /// [`ACTOR_STACK_HEADROOM_BYTES`] instead of a flat 64 MiB. The default
    /// 16 MiB JS stack therefore gets 40 MiB (was 64), and a low-resource
    /// operator who sets `stackMb: 8` gets 24 MiB. The result is always at
    /// least the JS stack budget itself, so the QuickJS recursion guard can
    /// never trip before the OS stack does.
    pub fn actor_stack_bytes(&self) -> usize {
        self.stack_bytes
            .saturating_mul(2)
            .saturating_add(ACTOR_STACK_HEADROOM_BYTES)
            .max(self.stack_bytes)
    }

    /// One-line rendering for the `PI_TIMING=1` load diagnostic.
    pub fn summary(&self) -> String {
        let memory = match self.memory_mb() {
            Some(mb) => format!("{mb} MB"),
            None => "unlimited".to_owned(),
        };
        let gc = match self.gc_threshold_bytes {
            Some(bytes) => format!("{} MB", bytes / (1024 * 1024)),
            None => "default".to_owned(),
        };
        format!(
            "memory={memory} stack={} MB gc_threshold={gc} call_timeout={} ms hook_timeout={} ms before_agent_start_timeout={} ms",
            self.stack_mb(),
            self.call_timeout.as_millis(),
            self.hook_timeout.as_millis(),
            self.before_agent_start_timeout.as_millis(),
        )
    }

    /// The actionable text for a heap-limit hit: names the setting and the
    /// environment override (and the `0` = no-limit escape hatch).
    pub fn memory_limit_error_message(&self) -> String {
        match self.memory_mb() {
            Some(mb) => format!(
                "extension exceeded its JS memory limit ({mb} MB); raise `extensionLimits.memoryMb` in ~/.pi/agent/settings.json or set PI_EXT_MEMORY_MB (0 = no limit)"
            ),
            None => "extension exhausted the JS heap".to_owned(),
        }
    }

    /// Whether `text` (a QuickJS exception / error message) reports heap
    /// exhaustion. QuickJS throws `InternalError: out of memory`.
    pub fn looks_like_memory_exhaustion(text: &str) -> bool {
        text.to_ascii_lowercase().contains("out of memory")
    }
}

/// Read `~/.pi/agent/settings.json` (`$PIR_AGENT_DIR/settings.json`) ONLY.
///
/// The project scope is passed untrusted on purpose: `SettingsManager`
/// leaves it out of the merge, so the returned snapshot is global-only —
/// the code-level half of the "project settings cannot loosen engine
/// limits" boundary. Missing files yield `Settings::default()`.
fn load_global_settings() -> Settings {
    let global_dir = crate::coding_agent::context::agent_dir();
    global_settings_from_dir(&global_dir)
}

/// [`load_global_settings`] against an explicit global directory (the test
/// seam; keeps the scope decision in one place). Per-field parse diagnostics
/// of the settings file are routed through `ext_diag!` (an invalid
/// `extensionLimits` value is ignored, never fatal).
pub fn global_settings_from_dir(global_dir: &Path) -> Settings {
    let manager = crate::config::settings::SettingsManager::load(
        global_dir, global_dir, global_dir,
        // Untrusted: the project scope must not contribute to engine limits.
        false,
    );
    for message in manager
        .error_messages()
        .into_iter()
        .chain(manager.warning_messages())
    {
        crate::ext_diag!("pir-ext: global settings: {message}");
    }
    manager.settings
}

fn lookup<'a>(vars: &'a [(&'a str, &'a str)], name: &str) -> Option<&'a str> {
    vars.iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| *value)
}

/// Parse a `*_MB` value: `Some(bytes)` for a positive amount, `None` for the
/// `0` = "no limit" spelling. A blank/non-numeric/oversized value is an
/// error the caller turns into a diagnostic.
fn parse_mb(name: &str, raw: &str) -> Result<Option<usize>, String> {
    let trimmed = raw.trim();
    let mb: u64 = trimmed
        .parse()
        .map_err(|_| format!("{name}=\"{raw}\" ignored: expected a non-negative integer (MiB)"))?;
    if mb == 0 {
        return Ok(None);
    }
    Ok(Some(mb_to_bytes(mb).unwrap_or(usize::MAX)))
}

/// Milliseconds → `Duration`, `0` meaning "keep the default" (`None`).
fn positive_ms(ms: u64) -> Option<Duration> {
    (ms > 0).then(|| Duration::from_millis(ms))
}

/// Parse a `*_MS` value: `Some(duration)` for a positive amount, `None` for
/// `0` (keeps the default). Non-numeric values are errors.
fn parse_ms(name: &str, raw: &str) -> Result<Option<Duration>, String> {
    let trimmed = raw.trim();
    let ms: u64 = trimmed
        .parse()
        .map_err(|_| format!("{name}=\"{raw}\" ignored: expected milliseconds"))?;
    Ok(positive_ms(ms))
}

/// MiB → bytes; `0` (the "no limit" spelling) becomes `None`, and a value
/// that would overflow `usize` saturates at `usize::MAX` (QuickJS treats the
/// resulting limit as unreachable, which is the closest honest behavior).
fn mb_to_bytes(mb: u64) -> Option<usize> {
    if mb == 0 {
        return None;
    }
    Some(usize::try_from(mb.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX))
}

/// Clamp a requested stack (MiB) for [`ExtensionLimits::stack_bytes`]:
/// `0` keeps the default, above [`MAX_STACK_BYTES`] clamps down. Both
/// adjustments push a diagnostic.
fn clamp_stack_mb(mb: u64, diagnostics: &mut Vec<String>) -> usize {
    if mb == 0 {
        diagnostics.push(format!(
            "extensionLimits.stackMb = 0 ignored: an unchecked JS stack can crash the process; keeping {} MB",
            DEFAULT_STACK_BYTES / (1024 * 1024)
        ));
        return DEFAULT_STACK_BYTES;
    }
    let bytes = usize::try_from(mb.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX);
    if bytes > MAX_STACK_BYTES {
        diagnostics.push(format!(
            "extensionLimits.stackMb = {mb} clamped to {} MB: rquickjs treats larger stacks as \"no stack check\"",
            MAX_STACK_BYTES / (1024 * 1024)
        ));
        return MAX_STACK_BYTES;
    }
    bytes
}
