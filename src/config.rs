use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct GatewayConfig {
    pub matrix: MatrixConfig,
}

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
