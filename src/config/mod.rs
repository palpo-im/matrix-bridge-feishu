mod bridge;

use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

pub use bridge::*;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("YAML parsing error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Environment variable error: {0}")]
    EnvVar(#[from] std::env::VarError),
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RegistrationConfig {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub as_token: String,
    #[serde(default)]
    pub hs_token: String,
    #[serde(default)]
    pub sender_localpart: String,
    #[serde(default)]
    pub namespaces: RegistrationNamespaces,
    #[serde(default)]
    pub rate_limited: bool,
    #[serde(default)]
    pub protocols: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct RegistrationNamespaces {
    #[serde(default)]
    pub users: Vec<NamespaceEntry>,
    #[serde(default)]
    pub aliases: Vec<NamespaceEntry>,
    #[serde(default)]
    pub rooms: Vec<NamespaceEntry>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct NamespaceEntry {
    #[serde(default)]
    pub exclusive: bool,
    #[serde(default)]
    pub regex: String,
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
    pub bridge: BridgeConfig,
    pub logging: LoggingConfig,
    pub database: DatabaseConfig,
    #[serde(skip)]
    pub registration: RegistrationConfig,
}

impl Config {
    pub fn load() -> Result<Self, ConfigError> {
        let config_path = std::env::var("CONFIG_PATH")
            .ok()
            .or_else(|| Some("config.yaml".to_string()))
            .unwrap();

        println!("Loading configuration from: {}", config_path);
        Self::load_from_file(&config_path)
    }

    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(&path)?;
        let mut config: Config = serde_yaml::from_str(&content)?;

        let config_dir = path.as_ref().parent().unwrap_or(Path::new("."));
        let registration_path = config_dir
            .join("appservices")
            .join("feishu-registration.yaml");
        let registration_content = std::fs::read_to_string(&registration_path).map_err(|e| {
            ConfigError::InvalidConfig(format!(
                "failed to load registration file {}: {}",
                registration_path.display(),
                e
            ))
        })?;
        config.registration = serde_yaml::from_str(&registration_content)?;

        config.apply_env_overrides();
        config.normalize();
        config.validate()?;
        Ok(config)
    }

    pub fn load_from_bytes(bytes: &[u8]) -> Result<Self, ConfigError> {
        let mut config: Config = serde_yaml::from_slice(bytes)?;
        config.registration = RegistrationConfig::default();
        config.apply_env_overrides();
        config.normalize();
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let db_type = self.database.r#type.trim().to_ascii_lowercase();
        if db_type != "sqlite" {
            return Err(ConfigError::InvalidConfig(format!(
                "database.type='{}' is not supported in current bridge build; use 'sqlite'",
                self.database.r#type
            )));
        }

        let has_wildcard = self.bridge.permissions.contains_key("*");
        let has_example_domain = self.bridge.permissions.contains_key("example.com");
        let has_example_user = self.bridge.permissions.contains_key("@admin:example.com");

        let example_count =
            has_wildcard as usize + has_example_domain as usize + has_example_user as usize;

        if self.bridge.permissions.len() <= example_count {
            return Err(ConfigError::InvalidConfig(
                "bridge.permissions not configured".to_string(),
            ));
        }

        if !self.bridge.username_template.contains("{{.}}") {
            return Err(ConfigError::InvalidConfig(
                "username template is missing user ID placeholder".to_string(),
            ));
        }

        validate_not_placeholder("registration.as_token", &self.registration.as_token)?;
        validate_not_placeholder("registration.hs_token", &self.registration.hs_token)?;
        validate_not_placeholder("bridge.app_id", &self.bridge.app_id)?;
        validate_not_placeholder("bridge.app_secret", &self.bridge.app_secret)?;

        let event_mode = self.bridge.event_mode.trim().to_ascii_lowercase();
        if event_mode != "long_connection" && event_mode != "webhook" {
            return Err(ConfigError::InvalidConfig(format!(
                "bridge.event_mode='{}' is invalid; expected 'long_connection' or 'webhook'",
                self.bridge.event_mode
            )));
        }

        if event_mode == "webhook" {
            validate_not_placeholder("bridge.listen_secret", &self.bridge.listen_secret)?;
        }

        if self.bridge.enable_rich_text == false && self.bridge.allow_plain_text == false {
            return Err(ConfigError::InvalidConfig(
                "bridge.enable_rich_text and bridge.allow_plain_text cannot both be false"
                    .to_string(),
            ));
        }

        if self.bridge.message_limit > 0 && self.bridge.message_cooldown == 0 {
            return Err(ConfigError::InvalidConfig(
                "bridge.message_cooldown must be > 0 when bridge.message_limit is configured"
                    .to_string(),
            ));
        }
        if self.bridge.user_sync_interval_secs == 0 {
            return Err(ConfigError::InvalidConfig(
                "bridge.user_sync_interval_secs must be > 0".to_string(),
            ));
        }
        if self.bridge.user_mapping_stale_ttl_hours == 0 {
            return Err(ConfigError::InvalidConfig(
                "bridge.user_mapping_stale_ttl_hours must be > 0".to_string(),
            ));
        }

        if event_mode == "webhook" {
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
                return Err(ConfigError::InvalidConfig(
                    "at least one webhook verification option must be configured: listen_secret/encrypt_key/verification_token".to_string()
                ));
            }

            if has_encrypt_key && !has_token {
                return Err(ConfigError::InvalidConfig(
                    "bridge.verification_token is required when bridge.encrypt_key is configured"
                        .to_string(),
                ));
            }
        }

        if self.bridge.long_connection_domain.trim().is_empty() {
            return Err(ConfigError::InvalidConfig(
                "bridge.long_connection_domain cannot be empty".to_string(),
            ));
        }
        if self.bridge.long_connection_reconnect_interval_secs == 0 {
            return Err(ConfigError::InvalidConfig(
                "bridge.long_connection_reconnect_interval_secs must be > 0".to_string(),
            ));
        }

        Ok(())
    }

    fn apply_env_overrides(&mut self) {
        override_from_env(&mut self.database.r#type, "DB_TYPE");
        override_from_env(&mut self.database.uri, "DB_URI");
        override_from_env(&mut self.registration.as_token, "AS_TOKEN");
        override_from_env(&mut self.registration.hs_token, "HS_TOKEN");
        override_from_env(&mut self.bridge.app_id, "BRIDGE_APP_ID");
        override_from_env(&mut self.bridge.app_secret, "BRIDGE_APP_SECRET");
        override_from_env(&mut self.bridge.event_mode, "BRIDGE_EVENT_MODE");
        override_from_env(&mut self.bridge.listen_address, "BRIDGE_LISTEN_ADDRESS");
        override_from_env(&mut self.bridge.listen_secret, "BRIDGE_LISTEN_SECRET");
        override_from_env(
            &mut self.bridge.long_connection_domain,
            "BRIDGE_LONG_CONNECTION_DOMAIN",
        );

        if let Ok(value) = env_var("BRIDGE_LONG_CONNECTION_RECONNECT_INTERVAL_SECS") {
            if let Ok(parsed) = value.parse::<u64>() {
                self.bridge.long_connection_reconnect_interval_secs = parsed.max(1);
            }
        }

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

    fn normalize(&mut self) {
        self.registration.as_token = self.registration.as_token.trim().to_string();
        self.registration.hs_token = self.registration.hs_token.trim().to_string();
        self.bridge.app_id = self.bridge.app_id.trim().to_string();
        self.bridge.app_secret = self.bridge.app_secret.trim().to_string();
        self.bridge.event_mode = self.bridge.event_mode.trim().to_ascii_lowercase();
        self.bridge.listen_address = self.bridge.listen_address.trim().to_string();
        self.bridge.listen_secret = self.bridge.listen_secret.trim().to_string();
        self.bridge.long_connection_domain = self
            .bridge
            .long_connection_domain
            .trim_end_matches('/')
            .trim()
            .to_string();

        // Normalize optional fields
        if let Some(ref mut key) = self.bridge.encrypt_key {
            *key = key.trim().to_string();
        }
        if let Some(ref mut token) = self.bridge.verification_token {
            *token = token.trim().to_string();
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

fn validate_not_placeholder(field: &str, value: &str) -> Result<(), ConfigError> {
    let lowered = value.trim().to_ascii_lowercase();
    let is_placeholder = lowered.is_empty()
        || lowered.contains("your_")
        || lowered.contains("changeme")
        || lowered.contains("replace_me")
        || lowered.contains("example")
        || lowered.ends_with("_here");
    if is_placeholder {
        return Err(ConfigError::InvalidConfig(format!(
            "configuration field '{}' still uses placeholder value: '{}'",
            field, value
        )));
    }
    Ok(())
}
