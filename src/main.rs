use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use anyhow::Context;
use tokio::sync::Mutex;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use agent_gateway::agent::AgentRegistry;
use agent_gateway::bot::matrix::{MatrixBot, Session};

fn install_systemd_user() -> anyhow::Result<()> {
    let home = dirs::home_dir().context("HOME not set")?;
    let config_dir = home.join(".config/agent-gateway");
    let unit_dir = home.join(".config/systemd/user");
    std::fs::create_dir_all(&config_dir)?;
    std::fs::create_dir_all(&unit_dir)?;

    let service_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("service");

    let config_dest = config_dir.join("config.toml");
    if config_dest.exists() {
        println!("Already exists, skipping: {}", config_dest.display());
    } else {
        std::fs::copy(service_dir.join("config.toml"), &config_dest)?;
        println!("Created: {}", config_dest.display());
    }

    let service_dest = config_dir.join("agent-gateway.service");
    if service_dest.exists() {
        println!("Already exists, skipping: {}", service_dest.display());
    } else {
        std::fs::copy(service_dir.join("agent-gateway.service"), &service_dest)?;
        println!("Created: {}", service_dest.display());
    }

    let exe = std::env::current_exe()?;
    let exec_start = format!("{} --config {}", exe.display(), config_dest.display());
    let unit_name = "agent-gateway.service";
    let template = std::fs::read_to_string(service_dir.join("agent-gateway.service"))?;
    let unit = template.replace("AGENT_GATEWAY_BIN", &exec_start);

    let unit_dest = unit_dir.join(unit_name);
    std::fs::write(&unit_dest, unit)?;
    println!("Installed: {}", unit_dest.display());

    let status = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()?;
    if !status.success() {
        anyhow::bail!("systemctl --user daemon-reload failed");
    }

    println!("Done. Run 'systemctl --user start {unit_name}' to start.");
    Ok(())
}

fn resolve_dir(env_var: &str, xdg_var: &str, home_segment: &str) -> PathBuf {
    if let Ok(dir) = std::env::var(env_var) {
        return PathBuf::from(dir);
    }
    if let Ok(xdg) = std::env::var(xdg_var) {
        return PathBuf::from(xdg).join("agent-gateway");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(home_segment).join("agent-gateway")
}

#[derive(Parser)]
#[command(name = "agent-gateway", about = "Matrix gateway for Claude Code")]
struct Cli {
    /// Path to the gateway config file
    #[arg(long)]
    config: Option<PathBuf>,

    /// Enable debug-level logging instead of info-level
    #[arg(long)]
    debug: bool,

    /// Install systemd user unit and exit
    #[arg(long)]
    install_systemd_user: bool,

    /// Start the systemd user service
    #[arg(long)]
    run_systemd: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.install_systemd_user {
        return install_systemd_user();
    }

    if cli.run_systemd {
        let status = std::process::Command::new("systemctl")
            .args(["--user", "start", "agent-gateway.service"])
            .status()?;
        if !status.success() {
            anyhow::bail!("systemctl --user start agent-gateway.service failed");
        }
        return Ok(());
    }

    let config_path = cli.config.context("--config is required")?;

    let state_dir = resolve_dir("STATE_DIRECTORY", "XDG_STATE_HOME", ".local/state");
    let cache_dir = resolve_dir("CACHE_DIRECTORY", "XDG_CACHE_HOME", ".cache");

    std::fs::create_dir_all(&state_dir)?;
    std::fs::create_dir_all(&cache_dir)?;

    let log_level = if cli.debug { "debug" } else { "info" };
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| log_level.into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!("State directory: {}", state_dir.display());
    tracing::info!("Cache directory: {}", cache_dir.display());
    tracing::info!("Config: {}", config_path.display());

    let sessions_path = state_dir.join("room_sessions.json");
    let mut sessions: HashMap<String, Session> =
        std::fs::read_to_string(&sessions_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
    // Clear stale ACP session IDs — child process is fresh on each start
    for s in sessions.values_mut() {
        s.clear_agent_session_id();
    }
    tracing::info!("Loaded {} room sessions", sessions.len());

    tracing::info!("Starting Claude Code via ACP...");
    let mut registry = AgentRegistry::new(cache_dir);
    registry.start().await?;
    let registry = Arc::new(Mutex::new(registry));
    tracing::info!("Claude Code ready");

    let bot = MatrixBot::new(registry, state_dir, config_path, sessions, sessions_path);
    let result = bot.start().await;
    bot.shutdown().await?;

    result
}
