use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeUser {
    pub id: Option<i64>,
    pub mxid: String,
    pub feishu_user_id: Option<String>,
    pub feishu_token: Option<String>,
    pub is_whitelisted: bool,
    pub is_admin: bool,
    pub relay_bot: Option<String>,
    pub management_room: Option<String>,
    pub space_room: Option<String>,
    pub timezone: Option<String>,
    pub connection_state: ConnectionState,
    pub next_batch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Error,
    Unknown,
}

impl BridgeUser {
    pub fn new(mxid: String) -> Self {
        Self {
            id: None,
            mxid,
            feishu_user_id: None,
            feishu_token: None,
            is_whitelisted: false,
            is_admin: false,
            relay_bot: None,
            management_room: None,
            space_room: None,
            timezone: None,
            connection_state: ConnectionState::Disconnected,
            next_batch: None,
        }
    }

    pub async fn login_feishu(&mut self, user_id: String, token: String) -> Result<()> {
        if user_id.trim().is_empty() {
            self.connection_state = ConnectionState::Error;
            anyhow::bail!("feishu user_id cannot be empty");
        }
        if token.trim().len() < 8 {
            self.connection_state = ConnectionState::Error;
            anyhow::bail!("feishu token looks invalid");
        }

        self.feishu_user_id = Some(user_id);
        self.feishu_token = Some(token);
        self.connection_state = ConnectionState::Connecting;

        self.connection_state = ConnectionState::Connected;
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.connection_state, ConnectionState::Connected)
    }
}
