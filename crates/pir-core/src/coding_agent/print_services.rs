//! Minimal [`SessionService`] for the non-interactive print host.
//!
//! Print already pushes a full [`SessionContextSnapshot`](crate::plugins::services::SessionContextSnapshot)
//! through `PluginHost::update_session_context`, but it never bound a
//! [`PluginContext`](crate::plugins::context::PluginContext). Native builtins
//! that keep durable per-session sidecars (the `mode` builtin's
//! `<session>.foreman.jsonl` journal and `.mode.json` sidecar) read the
//! session path from `ctx.session()`, so without a bound service `/mode plan`
//! changed in-memory state and then forgot it: `submit_plan` returned a
//! pending approval that no later process could read (`pir foreman approve`
//! opens the journal by session path).
//!
//! The print host has no live UI, message pump or tool service to bind; this
//! service exposes exactly the durable identity the builtins need. Every
//! other capability keeps the trait default (absent / headless).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::plugins::services::SessionService;

/// Session identity available to native plugins in `pir print`.
#[derive(Debug)]
pub struct PrintSessionService {
    id: Option<Arc<str>>,
    path: Option<Arc<Path>>,
    cwd: Arc<Path>,
}

impl PrintSessionService {
    pub fn new(id: Option<String>, path: Option<PathBuf>, cwd: PathBuf) -> Self {
        Self {
            id: id.map(Arc::from),
            path: path.map(|path| Arc::from(path.as_path())),
            cwd: Arc::from(cwd.as_path()),
        }
    }
}

impl SessionService for PrintSessionService {
    fn session_id(&self) -> Option<Arc<str>> {
        self.id.clone()
    }

    fn session_path(&self) -> Option<Arc<Path>> {
        self.path.clone()
    }

    fn cwd(&self) -> Option<Arc<Path>> {
        Some(Arc::clone(&self.cwd))
    }
}
