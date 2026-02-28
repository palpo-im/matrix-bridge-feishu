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
        let resolved_path = std::env::var("CONFIG_PATH").unwrap_or_else(|_| path.to_string());
        let content = std::fs::read_to_string(&resolved_path)?;
        let mut config: Config = serde_yaml::from_str(&content)?;
        config.apply_env_overrides();
        config.validate()?;
        Ok(config)
    }

    pub fn load_from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut config: Config = serde_yaml::from_slice(bytes)?;
        config.apply_env_overrides();
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        let db_type = self.appservice.database.r#type.trim().to_ascii_lowercase();
        if db_type != "sqlite" {
            anyhow::bail!(
                "appservice.database.type='{}' is not supported in current bridge build; use 'sqlite'",
                self.appservice.database.r#type
            );
        }

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

        validate_not_placeholder("appservice.as_token", &self.appservice.as_token)?;
        validate_not_placeholder("appservice.hs_token", &self.appservice.hs_token)?;
        validate_not_placeholder("bridge.app_id", &self.bridge.app_id)?;
        validate_not_placeholder("bridge.app_secret", &self.bridge.app_secret)?;
        validate_not_placeholder("bridge.listen_secret", &self.bridge.listen_secret)?;

        if self.bridge.enable_rich_text == false && self.bridge.allow_plain_text == false {
            anyhow::bail!(
                "bridge.enable_rich_text and bridge.allow_plain_text cannot both be false"
            );
        }

        if self.bridge.message_limit > 0 && self.bridge.message_cooldown == 0 {
            anyhow::bail!(
                "bridge.message_cooldown must be > 0 when bridge.message_limit is configured"
            );
        }

        let has_signature = !self.bridge.listen_secret.trim().is_empty();
        let has_token = self
            .bridge
            .verification_token
            .as_deref()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        let has_encrypt_key = self
            .bridge
            .encrypt_key
            .as_deref()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        if !has_signature && !has_token && !has_encrypt_key {
            anyhow::bail!(
                "at least one webhook verification option must be configured: listen_secret/encrypt_key/verification_token"
            );
        }

        if has_encrypt_key && !has_token {
            anyhow::bail!(
                "bridge.verification_token is required when bridge.encrypt_key is configured"
            );
        }

        Ok(())
    }

    fn apply_env_overrides(&mut self) {
        override_from_env(&mut self.appservice.database.r#type, "DB_TYPE");
        override_from_env(&mut self.appservice.database.uri, "DB_URI");
        override_from_env(&mut self.appservice.as_token, "AS_TOKEN");
        override_from_env(&mut self.appservice.hs_token, "HS_TOKEN");
        override_from_env(&mut self.bridge.app_id, "BRIDGE_APP_ID");
        override_from_env(&mut self.bridge.app_secret, "BRIDGE_APP_SECRET");
        override_from_env(&mut self.bridge.listen_address, "BRIDGE_LISTEN_ADDRESS");
        override_from_env(&mut self.bridge.listen_secret, "BRIDGE_LISTEN_SECRET");

        if let Ok(value) = env_var("BRIDGE_ENCRYPT_KEY") {
            self.bridge.encrypt_key = if value.trim().is_empty() {
                None
            } else {
                Some(value)
            };
        }
        if let Ok(value) = env_var("BRIDGE_VERIFICATION_TOKEN") {
            self.bridge.verification_token = if value.trim().is_empty() {
                None
            } else {
                Some(value)
            };
        }
    }

    pub fn format_username(&self, username: &str) -> String {
        self.bridge.username_template.replace("{{.}}", username)
    }
}

fn env_var(suffix: &str) -> Result<String, std::env::VarError> {
    let key = format!("MATRIX_BRIDGE_FEISHU_{}", suffix);
    std::env::var(key)
}

fn override_from_env(target: &mut String, suffix: &str) {
    if let Ok(value) = env_var(suffix) {
        if !value.trim().is_empty() {
            *target = value;
        }
    }
}

fn validate_not_placeholder(field: &str, value: &str) -> Result<()> {
    let lowered = value.trim().to_ascii_lowercase();
    let is_placeholder = lowered.is_empty()
        || lowered.contains("your_")
        || lowered.contains("changeme")
        || lowered.contains("replace_me")
        || lowered.contains("example")
        || lowered.ends_with("_here");
    if is_placeholder {
        anyhow::bail!(
            "configuration field '{}' still uses placeholder value: '{}'",
            field,
            value
        );
    }
    Ok(())
}
