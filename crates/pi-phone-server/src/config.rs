use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub listen: SocketAddr,
    pub public_url: Option<String>,
    pub allowed_origins: Vec<String>,
    pub pepper: String,
    pub metrics_token: Option<String>,
    pub max_connections: usize,
    pub max_devices_per_user: usize,
    pub max_sessions_per_user: usize,
    pub max_clients_per_session: usize,
    pub max_ws_message_bytes: usize,
    pub max_client_queue: usize,
    pub max_agent_queue: usize,
    pub idle_timeout_secs: u64,
    /// Keep at most this many session runtimes resident in memory. Online
    /// sessions and sessions with subscribers are never evicted; offline
    /// metadata remains in redb and catalog pages hydrate from the store.
    /// 0 disables the high-water eviction (historical behavior).
    pub max_resident_sessions: usize,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Config")
            .field("data_dir", &self.data_dir)
            .field("listen", &self.listen)
            .field("public_url", &self.public_url)
            .field("allowed_origins", &self.allowed_origins)
            .field("pepper", &"<redacted>")
            .field(
                "metrics_token",
                &self.metrics_token.as_ref().map(|_| "<redacted>"),
            )
            .field("max_connections", &self.max_connections)
            .field("max_devices_per_user", &self.max_devices_per_user)
            .field("max_sessions_per_user", &self.max_sessions_per_user)
            .field("max_clients_per_session", &self.max_clients_per_session)
            .field("max_ws_message_bytes", &self.max_ws_message_bytes)
            .field("max_client_queue", &self.max_client_queue)
            .field("max_agent_queue", &self.max_agent_queue)
            .field("idle_timeout_secs", &self.idle_timeout_secs)
            .field("max_resident_sessions", &self.max_resident_sessions)
            .finish()
    }
}

impl Config {
    pub fn load(
        data_dir: PathBuf,
        listen: SocketAddr,
        public_url: Option<String>,
        allowed_origins: Vec<String>,
    ) -> anyhow::Result<Self> {
        let secret_path = data_dir.join("secret");
        let pepper = std::env::var("PIR_PHONE_SERVER_PEPPER")
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                std::fs::read_to_string(&secret_path)
                    .ok()
                    .map(|value| value.trim().to_owned())
                    .filter(|value| !value.is_empty())
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "cannot read {}; run `pir-phone-server init --data-dir {}` first",
                    secret_path.display(),
                    data_dir.display()
                )
            })?;
        if pepper.len() < 32 {
            anyhow::bail!("server secret is too short: {}", secret_path.display());
        }
        Ok(Self {
            data_dir,
            listen,
            public_url,
            allowed_origins,
            pepper,
            metrics_token: std::env::var("PIR_PHONE_SERVER_METRICS_TOKEN")
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty()),
            max_connections: 20_000,
            max_devices_per_user: 10,
            max_sessions_per_user: 500,
            max_clients_per_session: 256,
            max_ws_message_bytes: 8 * 1024 * 1024,
            max_client_queue: 256,
            max_agent_queue: 512,
            idle_timeout_secs: 90,
            max_resident_sessions: std::env::var("PIR_PHONE_SERVER_MAX_RESIDENT_SESSIONS")
                .ok()
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(100_000),
        })
    }
}
