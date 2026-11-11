use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use pir_phone_server::config::Config;
use pir_phone_server::http;
use pir_phone_server::state::AppState;
use pir_phone_server::store::Store;

#[derive(Parser)]
#[command(
    name = "pir-phone-server",
    version,
    about = "Central information proxy for pi-phone sessions"
)]
struct Cli {
    /// Data directory shared by every subcommand.
    #[arg(long, global = true, default_value = "./pir-phone-data")]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the data directory, database, secret, and first admin token.
    Init,
    /// Manage users.
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
    /// Manage tokens.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Run the HTTP/WebSocket server.
    Serve {
        #[arg(long, default_value = "127.0.0.1:8787")]
        listen: SocketAddr,
        #[arg(long)]
        public_url: Option<String>,
        #[arg(long = "allowed-origin")]
        allowed_origins: Vec<String>,
    },
}

#[derive(Subcommand)]
enum UserCommand {
    Create {
        username: String,
        #[arg(long)]
        display_name: Option<String>,
    },
    List,
}

#[derive(Subcommand)]
enum TokenCommand {
    Create {
        #[arg(long)]
        user: String,
        #[arg(long, default_value = "user")]
        kind: String,
        #[arg(long)]
        name: Option<String>,
    },
    List {
        #[arg(long)]
        user: String,
    },
    Revoke {
        #[arg(long)]
        user: String,
        #[arg(long)]
        id: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let data_dir = cli.data_dir;
    match cli.command {
        Command::Init => init(&data_dir),
        Command::User { command } => user_command(&data_dir, command),
        Command::Token { command } => token_command(&data_dir, command),
        Command::Serve {
            listen,
            public_url,
            allowed_origins,
        } => serve(data_dir, listen, public_url, allowed_origins).await,
    }
}

fn init(data_dir: &std::path::Path) -> anyhow::Result<()> {
    let store = Store::open(data_dir)?;
    if let Some(existing) = store.find_user_by_username("admin")? {
        println!(
            "data directory already initialized; admin user id: {}",
            existing.id
        );
        return Ok(());
    }
    let admin = store.create_user("admin", Some("Administrator"))?;
    let (token, token_row) =
        store.create_token(&admin.id, "user", Some("admin bootstrap"), None)?;
    println!("initialized {}", data_dir.display());
    println!("admin user id: {}", admin.id);
    println!("admin token id: {}", token_row.id);
    println!("admin token (save it now; it cannot be shown again): {token}");
    Ok(())
}

fn user_command(data_dir: &std::path::Path, command: UserCommand) -> anyhow::Result<()> {
    let store = Store::open(data_dir)?;
    match command {
        UserCommand::Create {
            username,
            display_name,
        } => {
            let user = store.create_user(&username, display_name.as_deref())?;
            println!("user created: {} ({})", user.username, user.id);
        }
        UserCommand::List => {
            // No dedicated list API yet; the admin CLI is intentionally small.
            println!("user listing is available through /api/me for authenticated users");
        }
    }
    Ok(())
}

fn token_command(data_dir: &std::path::Path, command: TokenCommand) -> anyhow::Result<()> {
    let store = Store::open(data_dir)?;
    match command {
        TokenCommand::Create { user, kind, name } => {
            if kind != "user" && kind != "device" {
                anyhow::bail!("--kind must be 'user' or 'device'");
            }
            let Some(user_row) = store.find_user_by_username(&user)? else {
                anyhow::bail!("unknown user: {user}");
            };
            let device_id = if kind == "device" {
                let device_name = name.clone().unwrap_or_else(|| "device".to_owned());
                Some(store.create_device(&user_row.id, &device_name, None)?.id)
            } else {
                None
            };
            let (token, token_row) =
                store.create_token(&user_row.id, &kind, name.as_deref(), device_id.as_deref())?;
            println!("token id: {}", token_row.id);
            if let Some(device_id) = device_id {
                println!("device id: {device_id}");
            }
            println!("token (save it now; it cannot be shown again): {token}");
        }
        TokenCommand::List { user } => {
            let Some(user_row) = store.find_user_by_username(&user)? else {
                anyhow::bail!("unknown user: {user}");
            };
            for token in store.list_tokens(&user_row.id)? {
                println!(
                    "{}  kind={}  name={}  revoked={}",
                    token.id,
                    token.kind,
                    token.name.unwrap_or_default(),
                    token.revoked_at.is_some()
                );
            }
        }
        TokenCommand::Revoke { user, id } => {
            let Some(user_row) = store.find_user_by_username(&user)? else {
                anyhow::bail!("unknown user: {user}");
            };
            if store.revoke_token(&user_row.id, &id)? {
                println!("token revoked: {id}");
            } else {
                anyhow::bail!("token not found or already revoked: {id}");
            }
        }
    }
    Ok(())
}

async fn serve(
    data_dir: PathBuf,
    listen: SocketAddr,
    public_url: Option<String>,
    allowed_origins: Vec<String>,
) -> anyhow::Result<()> {
    let store = Store::open(&data_dir)?;
    // Presence is process-local: a previous crash/restart must not leave
    // sessions marked online in the catalog.
    store.mark_all_sessions_offline()?;
    let config = Config::load(data_dir, listen, public_url, allowed_origins)?;
    let state = AppState::new(config, store);
    let listener = tokio::net::TcpListener::bind(state.config.listen).await?;
    tracing::info!(
        "pir-phone-server listening on http://{}",
        listener.local_addr()?
    );
    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
