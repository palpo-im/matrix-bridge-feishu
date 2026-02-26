mod bridge;

use anyhow::Result;
pub use bridge::*;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct HomeserverConfig {
    pub address: String,
    pub domain: String,
    #[serde(default = "default_software")]
    pub software: String,
    pub status_endpoint: Option<String>,
    pub message_send_checkpoint_endpoint: Option<String>,
    #[serde(default)]
    pub async_media: bool,
    #[serde(default)]
    pub websocket: bool,
    #[serde(default)]
    pub ping_interval_seconds: u64,
}

fn default_software() -> String {
    "standard".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default = "default_db_type")]
    pub r#type: String,
    pub uri: String,
    #[serde(default = "default_max_open_conns")]
    pub max_open_conns: u32,
    #[serde(default = "default_max_idle_conns")]
    pub max_idle_conns: u32,
    pub max_conn_idle_time: Option<String>,
    pub max_conn_lifetime: Option<String>,
}

fn default_db_type() -> String {
    "sqlite".to_string()
}

fn default_max_open_conns() -> u32 {
    20
}

fn default_max_idle_conns() -> u32 {
    2
}

#[derive(Debug, Clone, Deserialize)]
pub struct BotConfig {
    pub username: String,
    pub displayname: String,
    pub avatar: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppServiceConfig {
    pub address: String,
    pub hostname: String,
    pub port: u16,
    pub database: DatabaseConfig,
    pub id: String,
    pub bot: BotConfig,
    #[serde(default)]
    pub ephemeral_events: bool,
    #[serde(default)]
    pub async_transactions: bool,
    pub as_token: String,
    pub hs_token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingWriterConfig {
    pub r#type: String,
    pub format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_backups: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compress: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingConfig {
    pub min_level: String,
    pub writers: Vec<LoggingWriterConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub homeserver: HomeserverConfig,
    pub appservice: AppServiceConfig,
    pub bridge: BridgeConfig,
    pub logging: LoggingConfig,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = serde_yaml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    pub fn load_from_bytes(bytes: &[u8]) -> Result<Self> {
        let config: Config = serde_yaml::from_slice(bytes)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        let has_wildcard = self.bridge.permissions.contains_key("*");
        let has_example_domain = self.bridge.permissions.contains_key("example.com");
        let has_example_user = self.bridge.permissions.contains_key("@admin:example.com");

        let example_count =
            has_wildcard as usize + has_example_domain as usize + has_example_user as usize;

        if self.bridge.permissions.len() <= example_count {
            anyhow::bail!("bridge.permissions not configured");
        }

        if !self.bridge.username_template.contains("{{.}}") {
            anyhow::bail!("username template is missing user ID placeholder");
        }

        Ok(())
    }

    pub fn format_username(&self, username: &str) -> String {
        self.bridge.username_template.replace("{{.}}", username)
    }
}
