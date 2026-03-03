use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct BridgeConfig {
    // Appservice / Bridge network configuration
    /// Domain part of the bridge (e.g., matrix.org)
    pub domain: String,
    /// Homeserver URL
    pub homeserver_url: String,
    /// The TCP port on which the appservice runs
    pub port: u16,
    /// Bind address for the appservice
    #[serde(default = "default_bind_address")]
    pub bind_address: String,

    // Bot configuration
    /// Bot username
    #[serde(default = "default_bot_username")]
    pub bot_username: String,
    /// Bot display name
    #[serde(default = "default_bot_displayname")]
    pub bot_displayname: String,
    /// Bot avatar URL
    #[serde(default)]
    pub bot_avatar: String,

    // Event mode configuration
    /// Event subscription mode (long_connection or webhook)
    #[serde(default = "default_event_mode")]
    pub event_mode: String,

    /// The address for Feishu webhook callbacks
    #[serde(default = "default_listen_address")]
    pub listen_address: String,

    /// The secret for Feishu webhook validation
    #[serde(default)]
    pub listen_secret: String,

    /// Feishu long-connection endpoint domain
    #[serde(default = "default_long_connection_domain")]
    pub long_connection_domain: String,

    /// Reconnect interval when long connection disconnects (seconds)
    #[serde(default = "default_long_connection_reconnect_interval_secs")]
    pub long_connection_reconnect_interval_secs: u64,

    /// Feishu app credentials
    pub app_id: String,
    pub app_secret: String,

    /// Feishu encrypt key (optional)
    pub encrypt_key: Option<String>,

    /// Feishu verification token (optional)
    pub verification_token: Option<String>,

    /// The template for Matrix puppet usernames
    #[serde(default = "default_username_template")]
    pub username_template: String,

    /// Permissions for bridging users/rooms
    pub permissions: HashMap<String, String>,

    /// Displayname template for bridged users
    #[serde(default = "default_displayname_template")]
    pub displayname_template: String,

    /// Avatar URL template for bridged users
    #[serde(default = "default_avatar_template")]
    pub avatar_template: String,

    /// Bridge configuration
    #[serde(default)]
    pub bridge_matrix_reply: bool,
    #[serde(default)]
    pub bridge_matrix_edit: bool,
    #[serde(default)]
    pub bridge_matrix_reactions: bool,
    #[serde(default)]
    pub bridge_matrix_redactions: bool,
    #[serde(default)]
    pub bridge_matrix_leave: bool,
    #[serde(default)]
    pub bridge_feishu_join: bool,
    #[serde(default)]
    pub bridge_feishu_leave: bool,

    /// Message formatting
    #[serde(default)]
    pub allow_plain_text: bool,
    #[serde(default = "default_true")]
    pub allow_markdown: bool,
    #[serde(default)]
    pub allow_html: bool,

    /// Media handling
    #[serde(default = "default_true")]
    pub allow_images: bool,
    #[serde(default)]
    pub allow_videos: bool,
    #[serde(default)]
    pub allow_audio: bool,
    #[serde(default)]
    pub allow_files: bool,
    #[serde(default)]
    pub max_media_size: usize,

    /// Rate limiting
    #[serde(default)]
    pub message_limit: u32,
    /// Rate limit window in milliseconds
    #[serde(default)]
    pub message_cooldown: u64,

    /// Outbound Matrix message policy
    #[serde(default)]
    pub blocked_matrix_msgtypes: Vec<String>,
    #[serde(default)]
    pub max_text_length: usize,

    /// Degrade behavior when outbound delivery fails
    #[serde(default = "default_true")]
    pub enable_failure_degrade: bool,
    #[serde(default = "default_failure_notice_template")]
    pub failure_notice_template: String,

    /// User profile synchronization controls
    #[serde(default = "default_user_sync_interval_secs")]
    pub user_sync_interval_secs: u64,
    #[serde(default = "default_user_mapping_stale_ttl_hours")]
    pub user_mapping_stale_ttl_hours: u64,

    /// Webhook timeout in seconds
    #[serde(default = "default_webhook_timeout")]
    pub webhook_timeout: u64,

    /// Feishu API timeout in seconds
    #[serde(default = "default_api_timeout")]
    pub api_timeout: u64,

    /// Enable Feishu rich text formatting
    #[serde(default = "default_true")]
    pub enable_rich_text: bool,

    /// Convert Feishu cards to Matrix
    #[serde(default = "default_true")]
    pub convert_cards: bool,
}

fn default_username_template() -> String {
    "feishu_{{.}}".to_string()
}

fn default_event_mode() -> String {
    "long_connection".to_string()
}

fn default_listen_address() -> String {
    "http://0.0.0.0:8081".to_string()
}

fn default_long_connection_domain() -> String {
    "https://open.feishu.cn".to_string()
}

fn default_long_connection_reconnect_interval_secs() -> u64 {
    5
}

fn default_displayname_template() -> String {
    "{{.}} (Feishu)".to_string()
}

fn default_avatar_template() -> String {
    "".to_string()
}

fn default_webhook_timeout() -> u64 {
    30
}

fn default_api_timeout() -> u64 {
    60
}

fn default_true() -> bool {
    true
}

fn default_failure_notice_template() -> String {
    "[bridge degraded] failed to deliver message from Matrix event {matrix_event_id}: {error}"
        .to_string()
}

fn default_user_sync_interval_secs() -> u64 {
    300
}

fn default_user_mapping_stale_ttl_hours() -> u64 {
    24 * 30
}

fn default_bind_address() -> String {
    "0.0.0.0".to_string()
}

fn default_bot_username() -> String {
    "feishubot".to_string()
}

fn default_bot_displayname() -> String {
    "Feishu Bridge".to_string()
}
