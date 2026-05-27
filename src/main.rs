mod agent;
mod bot;
mod command;
mod config;
mod request;

use clap::Parser;
use tokio::signal::ctrl_c;

use bot::Bot;
use bot::xmpp::XmppBot;
use config::XmppConfig;
use request::handle_request;

#[derive(serde::Deserialize)]
struct AppConfig {
    xmpp: XmppConfig,
}

#[derive(Parser)]
#[command(version, about = "XMPP bot that forwards messages to Claude Code via ACP")]
struct Cli {
    /// Path to config file
    #[arg(short = 'c', long = "config")]
    config: Option<String>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(|| "config.toml".into());

    let config: AppConfig = std::fs::read_to_string(&config_path)
        .map_err(|e| format!("Failed to read {config_path}: {e}"))
        .and_then(|s| toml::from_str(&s).map_err(|e| format!("Failed to parse {config_path}: {e}")))
        .unwrap_or_else(|e| {
            tracing::error!("{e}");
            std::process::exit(1);
        });

    let mut bot = XmppBot::builder()
        .jid(config.xmpp.jid)
        .password(config.xmpp.password)
        .nick(config.xmpp.nick)
        .rooms(config.xmpp.rooms)
        .build();

    loop {
        tokio::select! {
            (req, maybe_session) = bot.listen_msg() => {
                if !bot.handle_command(&req).await {
                    tokio::spawn(handle_request(req, maybe_session));
                }
            }
            _ = ctrl_c() => {
                bot.shutdown().await;
                break;
            }
        }
    }
}
