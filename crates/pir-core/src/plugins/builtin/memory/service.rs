//! State and path resolution for the `memory` builtin.
//!
//! One `MemoryService` lives per plugin instance (one per session host). It
//! caches the resolved settings and the cheap `cm_memory::Cm` handle, resolves
//! the store path from the live session transcript location, and keeps every
//! synchronous cm call on the blocking pool.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::RwLock;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use cm_memory::{BriefResponse, Cm, SourcePolicy, StoreOptions};
use serde_json::{json, Value};

use crate::agent::tool_execution::ToolSessionEnv;
use crate::config::settings::{MemoryAutoRecallPolicy, MemorySettings, SettingsManager};
pub use crate::config::settings::{MemoryStoreMode, MemoryWriteMode};

use super::prompt;

/// Effective memory settings (defaults applied).
#[derive(Clone, Debug)]
pub struct MemoryConfig {
    pub store: MemoryStoreMode,
    pub path: Option<PathBuf>,
    pub auto_recall: bool,
    pub auto_recall_policy: MemoryAutoRecallPolicy,
    pub auto_recall_min_score: f64,
    pub auto_recall_min_prompt_chars: usize,
    pub auto_brief: bool,
    pub recall_budget_tokens: usize,
    pub recall_max_items: usize,
    pub write: MemoryWriteMode,
    pub checkpoint_on_settle: bool,
    pub strict_sources: bool,
    pub max_store_bytes: u64,
    pub disk_cache: bool,
    pub allow_external_commands: bool,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            store: MemoryStoreMode::Session,
            path: None,
            auto_recall: true,
            auto_recall_policy: MemoryAutoRecallPolicy::Auto,
            auto_recall_min_score: 0.3,
            auto_recall_min_prompt_chars: 40,
            auto_brief: false,
            recall_budget_tokens: 600,
            recall_max_items: 3,
            write: MemoryWriteMode::Explicit,
            checkpoint_on_settle: true,
            strict_sources: true,
            max_store_bytes: 256 * 1024 * 1024,
            disk_cache: true,
            allow_external_commands: false,
        }
    }
}

impl MemoryConfig {
    fn from_settings(settings: &MemorySettings) -> Self {
        let defaults = Self::default();
        Self {
            store: settings.store.unwrap_or(defaults.store),
            path: settings.path.as_ref().map(PathBuf::from),
            auto_recall: settings.auto_recall.unwrap_or(defaults.auto_recall),
            auto_recall_policy: settings
                .auto_recall_policy
                .unwrap_or(defaults.auto_recall_policy),
            auto_recall_min_score: settings
                .auto_recall_min_score
                .unwrap_or(defaults.auto_recall_min_score)
                .max(0.0),
            auto_recall_min_prompt_chars: settings
                .auto_recall_min_prompt_chars
                .map(|value| value as usize)
                .unwrap_or(defaults.auto_recall_min_prompt_chars)
                .clamp(0, 10_000),
            auto_brief: settings.auto_brief.unwrap_or(defaults.auto_brief),
            recall_budget_tokens: settings
                .recall_budget_tokens
                .map(|value| value as usize)
                .unwrap_or(defaults.recall_budget_tokens)
                .clamp(50, 8000),
            recall_max_items: settings
                .recall_max_items
                .map(|value| value as usize)
                .unwrap_or(defaults.recall_max_items)
                .clamp(1, 20),
            write: settings.write.unwrap_or(defaults.write),
            checkpoint_on_settle: settings
                .checkpoint_on_settle
                .unwrap_or(defaults.checkpoint_on_settle),
            strict_sources: settings.strict_sources.unwrap_or(defaults.strict_sources),
            max_store_bytes: settings.max_store_bytes.unwrap_or(defaults.max_store_bytes),
            disk_cache: settings.disk_cache.unwrap_or(defaults.disk_cache),
            allow_external_commands: settings
                .allow_external_commands
                .unwrap_or(defaults.allow_external_commands),
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.write == MemoryWriteMode::ReadOnly
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BriefCacheEntry {
    signature: (u64, u64),
    block: String,
}

/// Per-session memory service shared by the tool, the `/memory` command and
/// the automatic recall/brief hooks.
pub struct MemoryService {
    agent_dir: RwLock<PathBuf>,
    cwd: RwLock<PathBuf>,
    session_file: RwLock<Option<PathBuf>>,
    session_id: RwLock<Option<String>>,
    trusted: AtomicBool,
    config: RwLock<Option<MemoryConfig>>,
    store_override: Option<PathBuf>,
    cm: RwLock<Option<(PathBuf, Cm)>>,
    dirty: AtomicBool,
    brief_cache: RwLock<Option<BriefCacheEntry>>,
    /// Bumped by every local invalidation so an in-flight generation cannot
    /// write a pre-write block back into the cache.
    brief_generation: AtomicU64,
}

impl MemoryService {
    pub fn new(agent_dir: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            agent_dir: RwLock::new(agent_dir.into()),
            cwd: RwLock::new(cwd.into()),
            session_file: RwLock::new(None),
            session_id: RwLock::new(None),
            trusted: AtomicBool::new(false),
            config: RwLock::new(None),
            store_override: None,
            cm: RwLock::new(None),
            dirty: AtomicBool::new(false),
            brief_cache: RwLock::new(None),
            brief_generation: AtomicU64::new(0),
        }
    }

    pub fn with_store_path(path: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        let mut service = Self::new(crate::coding_agent::context::agent_dir(), cwd);
        service.store_override = Some(path.into());
        service
    }

    /// Push the live session snapshot (cwd / transcript path / trust). Called
    /// from `on_session_context`; cheap and non-blocking.
    pub fn observe_session(
        &self,
        cwd: &Path,
        session_file: Option<PathBuf>,
        session_id: Option<String>,
        trusted: bool,
    ) {
        let cwd_changed = {
            let current = self.cwd.read().unwrap_or_else(|poison| poison.into_inner());
            *current != cwd
        };
        if cwd_changed {
            *self
                .cwd
                .write()
                .unwrap_or_else(|poison| poison.into_inner()) = cwd.to_path_buf();
        }
        let trust_changed = self.trusted.swap(trusted, Ordering::Relaxed) != trusted;
        // Settings resolution depends on cwd and trust: a cached config from
        // an untrusted cwd must never keep applying after a trust/cwd change.
        if cwd_changed || trust_changed {
            self.reload();
        }
        let session_changed = {
            let current = self
                .session_file
                .read()
                .unwrap_or_else(|poison| poison.into_inner());
            *current != session_file
        };
        if session_changed {
            *self
                .session_file
                .write()
                .unwrap_or_else(|poison| poison.into_inner()) = session_file;
            *self.cm.write().unwrap_or_else(|poison| poison.into_inner()) = None;
            self.invalidate_brief();
        }
        *self
            .session_id
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = session_id;
    }

    /// Pull the authoritative cwd/session/agent-dir fields out of a tool call.
    pub fn observe_tool_context(&self, cwd: &Path, env: Option<&ToolSessionEnv>) {
        let session_file = env.and_then(|env| env.session_file.clone());
        let session_id = env.and_then(|env| env.session_id.clone());
        // A missing ToolSessionEnv means "this transport did not carry the
        // snapshot", not "the project is untrusted": keep the last known
        // trust value instead of downgrading it.
        let trusted = env
            .map(|env| env.project_trusted)
            .unwrap_or_else(|| self.trusted.load(Ordering::Relaxed));
        self.observe_session(cwd, session_file, session_id, trusted);
        if let Some(agent_dir) = env.and_then(|env| env.agent_dir.clone()) {
            let mut current = self
                .agent_dir
                .write()
                .unwrap_or_else(|poison| poison.into_inner());
            if *current != agent_dir {
                *current = agent_dir;
                drop(current);
                self.reload();
            }
        }
    }

    pub fn session_id(&self) -> Option<String> {
        self.session_id
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub fn cwd(&self) -> PathBuf {
        self.cwd
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub fn agent_dir(&self) -> PathBuf {
        self.agent_dir
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub fn config(&self) -> Result<MemoryConfig> {
        if let Some(config) = self
            .config
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
        {
            return Ok(config);
        }
        let agent_dir = self.agent_dir();
        let cwd = self.cwd();
        let trusted = self.trusted.load(Ordering::Relaxed);
        let mut manager = SettingsManager::load(&agent_dir, &cwd.join(".pi"), &cwd, trusted);
        // Log-only: the CLI/host startup printer already reported each
        // distinct settings load failure once; echoing it here per memory
        // service construction duplicated the same problem on stderr.
        for error in manager.drain_errors() {
            crate::ext_diag_debug!("[memory] settings: {error:?}");
        }
        let settings = manager.settings.memory.clone().unwrap_or_default();
        let config = MemoryConfig::from_settings(&settings);
        *self
            .config
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(config.clone());
        Ok(config)
    }

    pub fn reload(&self) {
        *self
            .config
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
        *self.cm.write().unwrap_or_else(|poison| poison.into_inner()) = None;
        self.invalidate_brief();
    }

    /// The cm file for this session. `store = "path"` wins; otherwise the
    /// configured policy resolves against the live session/cwd.
    pub fn store_path(&self) -> Result<PathBuf> {
        if let Some(path) = &self.store_override {
            return Ok(path.clone());
        }
        let config = self.config()?;
        let cwd = self.cwd();
        Ok(match config.store {
            MemoryStoreMode::Session => {
                let session_file = self
                    .session_file
                    .read()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .clone();
                match session_file {
                    Some(path) => path
                        .parent()
                        .map(|parent| parent.join("cm"))
                        .unwrap_or_else(|| cwd.join("cm")),
                    None => {
                        // `resolve_path_str` gives the same lexically
                        // normalized absolute path the session manager uses;
                        // `canonicalize` on Windows yields a `\\?\` verbatim
                        // prefix whose `?` is not a legal directory-name
                        // character in the encoded session dir.
                        let resolved =
                            crate::session_manager::resolve_path_str(&cwd.to_string_lossy());
                        crate::session_manager::sessions_dir_in(&self.agent_dir())
                            .join(crate::session_manager::encode_cwd_dir_name(&resolved))
                            .join("cm")
                    }
                }
            }
            MemoryStoreMode::Workspace => project_root(&cwd).join("cm"),
            MemoryStoreMode::AgentDir => self
                .agent_dir()
                .join("memory")
                .join(project_hash(&project_root(&cwd)))
                .join("cm"),
            MemoryStoreMode::Path => {
                let path = config
                    .path
                    .ok_or_else(|| anyhow!("memory.store is \"path\" but memory.path is unset"))?;
                if path.is_absolute() {
                    path
                } else {
                    cwd.join(path)
                }
            }
        })
    }

    pub fn path_exists(&self) -> bool {
        self.store_path().map(|path| path.exists()).unwrap_or(false)
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::SeqCst);
        self.invalidate_brief();
    }

    fn store_signature(&self) -> Option<(u64, u64)> {
        let path = self.store_path().ok()?;
        let metadata = std::fs::metadata(path).ok()?;
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        Some((metadata.len(), mtime))
    }

    pub fn dirty(&self) -> bool {
        self.dirty.load(Ordering::SeqCst)
    }

    pub fn invalidate_brief(&self) {
        self.brief_generation.fetch_add(1, Ordering::SeqCst);
        *self
            .brief_cache
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
    }

    /// Run one synchronous cm operation on the blocking pool.
    ///
    /// `create` initializes an empty store when the file does not exist; read
    /// actions pass `false` and return `None` for a missing store.
    pub async fn with_cm<T, F>(&self, create: bool, f: F) -> Result<Option<T>>
    where
        T: Send + 'static,
        F: FnOnce(&Cm) -> Result<T> + Send + 'static,
    {
        // Writes enforce the size cap; reads never create the store.
        self.with_cm_inner(create, create, f).await
    }

    /// Open the store for a recovery/maintenance write (forget, purge,
    /// checkpoint, reindex). These actions shrink or repair the file, so a
    /// store already above `maxStoreBytes` must still be recoverable.
    pub async fn with_cm_recovery<T, F>(&self, f: F) -> Result<Option<T>>
    where
        T: Send + 'static,
        F: FnOnce(&Cm) -> Result<T> + Send + 'static,
    {
        self.with_cm_inner(true, false, f).await
    }

    async fn with_cm_inner<T, F>(&self, create: bool, enforce_cap: bool, f: F) -> Result<Option<T>>
    where
        T: Send + 'static,
        F: FnOnce(&Cm) -> Result<T> + Send + 'static,
    {
        let config = self.config()?;
        let path = self.store_path()?;
        // Relative `source` anchors are written by tools (write/edit) relative
        // to the live session cwd, not to the nearest `.git`/project root.
        // Resolve and confine them against the same cwd so a nested project
        // directory can cite its own files without weakening confinement.
        let policy = SourcePolicy {
            root: self.cwd(),
            strict: config.strict_sources,
        };
        tokio::task::spawn_blocking(move || {
            let Some(cm) = open_cm(&path, &config, &policy, create, enforce_cap)? else {
                return Ok(None);
            };
            f(&cm).map(Some)
        })
        .await
        .map_err(|error| anyhow!("memory blocking task failed: {error}"))?
    }

    /// Store status plus the resolved path/mode, for `/memory status` and the
    /// command surface. A missing store reports `exists: false` rather than an
    /// error.
    pub async fn status_value(&self) -> Result<Value> {
        let config = self.config()?;
        let path = self.store_path()?;
        let mode = match config.store {
            MemoryStoreMode::Session => "session",
            MemoryStoreMode::Workspace => "workspace",
            MemoryStoreMode::AgentDir => "agentDir",
            MemoryStoreMode::Path => "path",
        };
        if !path.exists() {
            return Ok(json!({
                "ok": true,
                "exists": false,
                "path": path.display().to_string(),
                "mode": mode,
                "write": if config.is_read_only() { "readOnly" } else { "explicit" }
            }));
        }
        let status = self
            .with_cm(false, |cm| cm.status())
            .await?
            .ok_or_else(|| anyhow!("memory store disappeared: {}", path.display()))?;
        Ok(json!({
            "ok": true,
            "exists": true,
            "path": path.display().to_string(),
            "mode": mode,
            "write": if config.is_read_only() { "readOnly" } else { "explicit" },
            "doc_count": status.doc_count,
            "active_count": status.active_count,
            "tail_count": status.tail_count,
            "generation": status.generation,
            "bytes": status.bytes,
            "cache_bytes": status.cache_bytes,
            "cache_capacity": status.cache_capacity,
            "cache_entries": status.cache_entries,
            "cache_hits": status.cache_hits,
            "cache_misses": status.cache_misses,
            "dirty": self.dirty()
        }))
    }

    /// Automatic recall for the `input` event. Returns `None` when disabled,
    /// the store is missing, or the query has no frame/memories.
    pub async fn auto_recall(&self, text: &str) -> Option<String> {
        let config = self.config().ok()?;
        let trimmed = text.trim();
        if !config.auto_recall
            || config.auto_recall_policy == MemoryAutoRecallPolicy::Off
            || trimmed.is_empty()
            || trimmed.starts_with('/')
        {
            return None;
        }
        if !self.path_exists() {
            return None;
        }
        let chars = trimmed.chars().count();
        if config.auto_recall_policy == MemoryAutoRecallPolicy::Auto
            && chars < config.auto_recall_min_prompt_chars
        {
            return None;
        }
        let lower = trimmed.to_lowercase();
        let is_question = [
            "explain",
            "how ",
            "why ",
            "what ",
            "where ",
            "which ",
            "describe",
            "analyze",
            "analyse",
            "architecture",
            "design",
            "summarize",
            "summarise",
            "compare",
            "investigate",
            "tell me",
            "walk me through",
            "review",
        ]
        .iter()
        .any(|needle| lower.contains(needle));
        let is_edit = [
            "fix ",
            "implement",
            "rename",
            "add ",
            "update ",
            "refactor",
            "change ",
            "remove",
            "delete",
            "write ",
            "create ",
            "patch",
            "migrate",
            "replace",
        ]
        .iter()
        .any(|needle| lower.contains(needle));
        let continuation = [
            "session 1",
            "session 2",
            "session1",
            "session2",
            "continue",
            "resume",
            "previous",
            "earlier",
            "as discussed",
            "from the plan",
            "apply the plan",
        ]
        .iter()
        .any(|needle| lower.contains(needle));
        let short_edit = config.auto_recall_policy == MemoryAutoRecallPolicy::Auto
            && is_edit
            && !is_question
            && chars < 400;
        if short_edit {
            // Autonomous routing: a plain one-shot edit gets no automatic
            // injection (the measured BM25-record overhead exceeded the
            // benefit). A continuation prompt ("Session 2", "continue",
            // "apply the plan") gets only the active task frame — tiny,
            // actionable goal/next/constraint context, never memory records.
            // The model can always call the memory tool explicitly, and
            // `always` keeps the old full-brief behavior.
            if !continuation {
                return None;
            }
            let response = self.with_cm(false, |cm| cm.brief(None, 0)).await.ok()??;
            if response.frame.is_none() || response.prompt.trim().is_empty() {
                return None;
            }
            return Some(prompt::render_memory_context(&response.prompt));
        }
        let query: String = trimmed.chars().take(2000).collect();
        let options = cm_memory::BriefOptions {
            task: None,
            query: Some(query),
            budget: config.recall_budget_tokens,
            top: 16,
            max_items: config.recall_max_items,
            scope: None,
            min_trust: Some("imported".to_string()),
            min_score: config.auto_recall_min_score,
            snippet_lines: 0,
            root: self.cwd(),
        };
        let response = self
            .with_cm(false, move |cm| cm.brief_with(&options))
            .await
            .ok()??;
        // `select_brief` applies `min_score` to recalled memories in every
        // policy; the frame itself is explicit task context and survives.
        if response.memories.is_empty() && response.frame.is_none() {
            return None;
        }
        Some(prompt::render_memory_context(&response.prompt))
    }

    /// Stable automatic brief for `before_agent_start` (cached until a write or
    /// reload). Returns `None` when disabled or no active frame exists.
    pub async fn auto_brief(&self) -> Option<String> {
        let config = self.config().ok()?;
        if !config.auto_brief || !self.path_exists() {
            return None;
        }
        let signature = self.store_signature()?;
        let generation = self.brief_generation.load(Ordering::SeqCst);
        if let Some(cached) = self
            .brief_cache
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
        {
            if cached.signature == signature {
                return Some(cached.block.clone());
            }
        }
        let options = cm_memory::BriefOptions {
            task: None,
            query: None,
            budget: config.recall_budget_tokens,
            top: 16,
            max_items: config.recall_max_items,
            scope: None,
            min_trust: Some("imported".to_string()),
            min_score: config.auto_recall_min_score,
            snippet_lines: 0,
            root: self.cwd(),
        };
        let response = self
            .with_cm(false, move |cm| cm.brief_with(&options))
            .await
            .ok()??;
        if response.prompt.trim().is_empty() {
            return None;
        }
        let block = prompt::render_memory_brief(&response.prompt);
        // Only cache if no local write and no external store change happened
        // while this block was being generated.
        if self.brief_generation.load(Ordering::SeqCst) == generation
            && self.store_signature() == Some(signature)
        {
            *self
                .brief_cache
                .write()
                .unwrap_or_else(|poison| poison.into_inner()) = Some(BriefCacheEntry {
                signature,
                block: block.clone(),
            });
        }
        Some(block)
    }

    /// Checkpoint after writes, if configured and dirty.
    pub async fn checkpoint_if_dirty(&self) -> Result<()> {
        let config = self.config()?;
        if !config.checkpoint_on_settle || !self.dirty.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        let result = self.with_cm_recovery(|cm| cm.checkpoint_op(true)).await;
        if result.is_err() {
            self.dirty.store(true, Ordering::SeqCst);
        }
        result.map(|_| ())
    }

    /// Checkpoint regardless of the setting, for shutdown flush.
    pub async fn flush(&self) -> Result<()> {
        if !self.dirty.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        let result = timeout_checkpoint(self).await;
        if result.is_err() {
            self.dirty.store(true, Ordering::SeqCst);
        }
        result
    }

    pub async fn brief_response(&self, query: &str) -> Result<BriefResponse> {
        let config = self.config()?;
        let options = cm_memory::BriefOptions {
            task: None,
            query: Some(query.to_string()),
            budget: config.recall_budget_tokens,
            top: 16,
            max_items: config.recall_max_items,
            scope: None,
            min_trust: Some("imported".to_string()),
            min_score: 0.0,
            snippet_lines: 0,
            root: self.cwd(),
        };
        self.with_cm(false, move |cm| cm.brief_with(&options))
            .await?
            .ok_or_else(|| anyhow!("memory store not initialized"))
    }
}

async fn timeout_checkpoint(service: &MemoryService) -> Result<()> {
    match tokio::time::timeout(
        Duration::from_secs(5),
        service.with_cm_recovery(|cm| cm.checkpoint_op(true)),
    )
    .await
    {
        Ok(result) => result.map(|_| ()),
        Err(_) => bail!("memory checkpoint timed out"),
    }
}

fn open_cm(
    path: &Path,
    config: &MemoryConfig,
    policy: &SourcePolicy,
    create: bool,
    enforce_cap: bool,
) -> Result<Option<Cm>> {
    if !path.exists() {
        if !create {
            return Ok(None);
        }
        match Cm::init(path, true) {
            Ok(_) => {}
            Err(error) => {
                // Another process may have won the `create_new` race.
                if !path.exists() {
                    return Err(error);
                }
            }
        }
    }
    if enforce_cap {
        let bytes = std::fs::metadata(path)?.len();
        if bytes > config.max_store_bytes {
            bail!(
                "memory store is {} bytes, above the {} byte cap",
                bytes,
                config.max_store_bytes
            );
        }
    }
    let options = StoreOptions {
        read_only: config.is_read_only(),
        lock_timeout: Some(Duration::from_secs(5)),
        disk_cache: config.disk_cache,
        source_policy: policy.clone(),
    };
    Ok(Some(Cm::open_with(path, options)?))
}

/// Nearest ancestor holding `.git`, else the nearest `Cargo.toml`, else cwd.
pub fn project_root(cwd: &Path) -> PathBuf {
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut current = canonical.clone();
    loop {
        if current.join(".git").exists() {
            return current;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    let mut current = canonical.clone();
    loop {
        if current.join("Cargo.toml").exists() {
            return current;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    canonical
}

/// Stable FNV-1a 64-bit hash of the canonical project path (hex, 16 chars).
pub fn project_hash(path: &Path) -> String {
    let mut text = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        text.make_ascii_lowercase();
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}
