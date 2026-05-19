use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct GatewayConfig {
    pub matrix: MatrixConfig,
    pub irc: Option<IrcConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IrcConfig {
    pub server: String,
    #[serde(default = "default_irc_port")]
    pub port: u16,
    #[serde(default = "default_true")]
    pub tls: bool,
    pub nick: String,
    #[serde(default = "default_irc_user")]
    pub user: String,
    #[serde(default = "default_irc_realname")]
    pub realname: String,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default = "default_command_prefix")]
    pub command_prefix: String,
    #[serde(default)]
    pub allowed_users: Vec<String>,
    #[serde(default)]
    pub password: Option<String>,
}

fn default_irc_port() -> u16 { 6697 }
fn default_true() -> bool { true }
fn default_irc_user() -> String { "agent-gateway".into() }
fn default_irc_realname() -> String { "Agent Gateway IRC Bot".into() }
fn default_command_prefix() -> String { "/".into() }

#[derive(Debug, Deserialize)]
pub struct MatrixConfig {
    pub id: String,
    pub password: String,
    #[serde(rename = "allowed-user", default)]
    pub allowed_user: Vec<String>,
}

impl GatewayConfig {
    pub fn from_file(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: GatewayConfig = toml::from_str(&content)?;
        Ok(config)
    }
}
