use std::sync::Arc;
use std::time::Instant;

use tokio::sync::RwLock;

use crate::auth::TicketStore;
use crate::config::Config;
use crate::metrics::Metrics;
use crate::registry::Registry;
use crate::store::Store;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub store: Store,
    pub registry: Arc<RwLock<Registry>>,
    pub tickets: Arc<TicketStore>,
    pub metrics: Arc<Metrics>,
    pub started_at: Instant,
}

impl AppState {
    pub fn new(config: Config, store: Store) -> Self {
        let mut registry = Registry::new();
        registry.set_max_resident_sessions(config.max_resident_sessions);
        Self {
            config: Arc::new(config),
            store,
            registry: Arc::new(RwLock::new(registry)),
            tickets: Arc::new(TicketStore::default()),
            metrics: Arc::new(Metrics::default()),
            started_at: Instant::now(),
        }
    }
}
