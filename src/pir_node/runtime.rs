//! Context contract between pir and `pir-node`.
//!
//! `pir-node` is deliberately independent of the pir crate and of rquickjs:
//! it never holds `RuntimeState`, `ExtensionHandle`, or `Value<'js>`. Instead
//! the host supplies the small amount of process state the Node surface needs
//! through this trait and through owned constructor values. That keeps the
//! Node implementations pure Rust over `serde_json::Value` and makes them
//! callable directly from the C-ABI layer without any JSON envelope.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

/// Snapshot of the host process facts the Node surface reads. Immutable for
/// the lifetime of one runtime (mode/has_ui are fixed at construction, see
/// `host_functions.rs`'s `normalize_mode` and `ui_fail_fast`).
#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    pub cwd: PathBuf,
    pub mode: String,
    pub has_ui: bool,
    pub host_env_boot: Option<String>,
}

/// The small host-facing context `pir-node` ops consult.
///
/// Implemented by pir by adapting `Arc<StdMutex<RuntimeState>>` plus the
/// extension loader's `allowed_roots`; a no-op impl (`()`-like) is provided for
/// embedders that only use the pure modules.
pub trait NodeContext: Send + Sync {
    fn cwd(&self) -> PathBuf;
    fn allowed_roots(&self) -> Vec<PathBuf>;
    fn is_aborted(&self) -> bool;
    fn is_alive(&self) -> bool {
        true
    }
    fn generation(&self) -> u64 {
        0
    }
    fn record_host_op(&self, _label: &'static str, _elapsed: Duration) {}
    fn diag(&self, _message: &str) {}
}

/// Optional hooks into pir-owned subsystems that `pir-node` must not depend on
/// directly (process execution, codemode, stdio routing, npm loading). The
/// default implementation reports the capability as unavailable so a pure
/// `pir-node` embedder still compiles and runs.
pub trait NodeHooks: Send + Sync {
    fn exec(
        &self,
        _argv: &[String],
        _cwd: Option<&Path>,
    ) -> Result<ExecOutcome, crate::pir_node::error::NodeError> {
        Err(crate::pir_node::error::NodeError::new(
            "ENOSYS",
            "exec is not available in this pir-node host",
        ))
    }

    fn resolve_npm(
        &self,
        _specifier: &str,
        _from: &Path,
    ) -> Result<PathBuf, crate::pir_node::error::NodeError> {
        Err(crate::pir_node::error::NodeError::new(
            "ENOSYS",
            "npm resolution is not available in this pir-node host",
        ))
    }

    fn write_stdio(&self, _stream: &str, _text: &str) {}
    fn exit_process(&self, _code: i32) {}

    /// Spawn the OS thread + QuickJS runtime for one worker and drive it with
    /// `handle`. pir owns the engine boot, so the transport (registry,
    /// mailboxes, op table) stays here while the runtime lives on the host.
    fn spawn_worker(
        &self,
        _launch: crate::pir_node::modules::worker::WorkerLaunch,
        _handle: crate::pir_node::modules::worker::WorkerHandle,
    ) -> Result<(), crate::pir_node::error::NodeError> {
        Err(crate::pir_node::error::NodeError::new(
            "ENOSYS",
            "worker_threads is not available in this pir-node host",
        ))
    }

    /// Extra loader roots a worker inherits (the host's agent npm root).
    fn worker_roots(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    /// npm specifier lookup (`require`/`import` resolution + builtin
    /// classification). pir owns the resolver and the transpile stack, so the
    /// classification is supplied by the host; this crate keeps the op table,
    /// the wire shape and the error mapping.
    fn npm_lookup(&self, _spec: &str, _dir: &Path, _require: bool) -> NpmLookup {
        NpmLookup::Error {
            code: "ENOSYS".to_owned(),
            message: "npm resolution is not available in this pir-node host".to_owned(),
        }
    }

    /// Read + transpile one on-disk module source for the CJS runtime. The
    /// caller has already sandbox-checked `path`.
    fn npm_source(&self, _path: &Path, _as_cjs: bool) -> Result<NpmSource, crate::pir_node::error::NodeError> {
        Err(crate::pir_node::error::NodeError::new(
            "ENOSYS",
            "module source reading is not available in this pir-node host",
        ))
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExecOutcome {
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
    pub signal: Option<String>,
}

/// Result of one npm specifier lookup, mirroring the `{kind}` value the JS
/// CJS/ESM runtime consumes.
#[derive(Debug, Clone)]
pub enum NpmLookup {
    /// Shipped builtin / virtual package shim: the JS layer reads the module
    /// out of `__pirNodeModules` by this name.
    Builtin { name: String },
    /// A file on disk (ESM/CJS/JSON), with the kind label the loader expects.
    File { path: PathBuf, module_kind: String },
    /// Not found: the JS layer turns this into Node's `MODULE_NOT_FOUND`.
    NotFound { code: String, message: String },
    /// A coded resolution failure (unsupported scheme, native addon, …).
    Error { code: String, message: String },
}

/// Source text for one on-disk module, already transpiled as requested.
#[derive(Debug, Clone)]
pub struct NpmSource {
    /// `"cjs"`, `"esm"` or `"json"`.
    pub kind: String,
    pub source: String,
}

/// Concrete runtime handed to the C-ABI glue. It owns the shared allow-list
/// (mutated by the loader at runtime, so an `Arc` is required) and holds host
/// callbacks behind traits, never JS values.
pub struct NodeRuntime {
    pub allowed_roots: Arc<StdMutex<Vec<PathBuf>>>,
    pub config: SessionConfig,
    pub context: Arc<dyn NodeContext>,
    pub hooks: Arc<dyn NodeHooks>,
    /// Open `fs.open` descriptors for this runtime generation.
    pub fd_table: Arc<StdMutex<crate::pir_node::fd::FdTable>>,
    /// `process.chdir` target. Kept separate from `config.cwd` (which is the
    /// session's immutable base) so relative fs paths follow Node's cwd
    /// without mutating the shared session config.
    pub cwd_override: Arc<StdMutex<Option<PathBuf>>>,
}

impl NodeRuntime {
    pub fn new(
        allowed_roots: Arc<StdMutex<Vec<PathBuf>>>,
        config: SessionConfig,
        context: Arc<dyn NodeContext>,
        hooks: Arc<dyn NodeHooks>,
    ) -> Self {
        Self {
            allowed_roots,
            config,
            context,
            hooks,
            fd_table: Arc::new(StdMutex::new(crate::pir_node::fd::FdTable::new())),
            cwd_override: Arc::new(StdMutex::new(None)),
        }
    }

    /// Current working directory: the `process.chdir` override when set,
    /// otherwise the session's base cwd.
    pub fn cwd(&self) -> PathBuf {
        self.cwd_override
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
            .unwrap_or_else(|| self.config.cwd.clone())
    }

    /// Effective sandbox roots: the allow-list plus cwd (the loader always
    /// includes cwd, mirroring `node_fns.rs`'s `effective_roots`).
    pub fn effective_roots(&self) -> Vec<PathBuf> {
        let mut roots = self
            .allowed_roots
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let cwd = self.config.cwd.clone();
        if !roots.iter().any(|root| root == &cwd) {
            roots.push(cwd);
        }
        roots
    }
}

/// Callback installed by the host for `PIR_EXT_DIAG`-gated transport
/// diagnostics.
type DiagSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Process-wide diagnostics sink. Some deep transport helpers (parked body
/// readers, connection threads) have no runtime handle in scope but still need
/// the host's `PIR_EXT_DIAG`-gated channel; the host installs one sink when it
/// builds its first runtime.
static DIAG_SINK: std::sync::OnceLock<DiagSink> = std::sync::OnceLock::new();

/// Install the diagnostics sink (idempotent; the first install wins).
pub fn install_diag_sink(sink: DiagSink) {
    let _ = DIAG_SINK.set(sink);
}

/// Emit one host diagnostic line. A no-op until a host installs a sink.
pub fn diag(message: &str) {
    if let Some(sink) = DIAG_SINK.get() {
        sink(message);
    }
}

/// `diag!` in the host's `ext_diag!` style.
#[macro_export]
macro_rules! node_diag {
    ($($arg:tt)*) => {
        $crate::pir_node::runtime::diag(&format!($($arg)*))
    };
}

/// No-op host context for embedders that do not need pir telemetry/state.
pub struct NullContext;

impl NodeContext for NullContext {
    fn cwd(&self) -> PathBuf {
        std::env::current_dir().unwrap_or_default()
    }
    fn allowed_roots(&self) -> Vec<PathBuf> {
        vec![self.cwd()]
    }
    fn is_aborted(&self) -> bool {
        false
    }
}

/// No-op hooks for pure-embedder use.
pub struct NullHooks;

impl NodeHooks for NullHooks {}
