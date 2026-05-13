use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use agent_gateway::agent::AgentRegistry;
use agent_gateway::bot::matrix::{MatrixBot, Session};

fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/agent-gateway/config.toml")
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

fn user_service_dir() -> PathBuf {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".config")
    };
    base.join("systemd").join("user")
}

fn install_user_service(config_path: &Path) -> anyhow::Result<()> {
    let binary = std::env::current_exe().context("failed to get current executable path")?;
    let service_dir = user_service_dir();
    std::fs::create_dir_all(&service_dir).context("failed to create systemd user service dir")?;

    let unit_path = service_dir.join("agent-gateway.service");
    let template = include_str!("../utils/agent-gateway.service");
    let unit_content = template
        .replace("{{binary}}", &binary.display().to_string())
        .replace("{{config_path}}", &config_path.display().to_string());

    std::fs::write(&unit_path, &unit_content).context("failed to write systemd unit file")?;

    let status = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
        .context("failed to run systemctl --user daemon-reload")?;

    if !status.success() {
        anyhow::bail!("systemctl --user daemon-reload failed");
    }

    println!("Systemd user service installed: {}", unit_path.display());
    println!("Start it with:  systemctl --user enable --now agent-gateway");
    Ok(())
}

#[derive(Parser)]
#[command(name = "agent-gateway", about = "Matrix gateway for Claude Code")]
struct Cli {
    /// Path to the gateway config file (default: ~/.config/agent-gateway/config.toml)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Enable debug-level logging instead of info-level
    #[arg(long)]
    debug: bool,

    /// Install systemd user service and exit
    #[arg(long)]
    install_user_service: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(default_config_path);

    if cli.install_user_service {
        install_user_service(&config_path)?;
        return Ok(());
    }

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
    let sessions: HashMap<String, Session> = std::fs::read_to_string(&sessions_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    tracing::info!("Loaded {} room sessions", sessions.len());

    let bot = MatrixBot::new(AgentRegistry::new(), state_dir, config_path, sessions, sessions_path);
    bot.run().await
}
