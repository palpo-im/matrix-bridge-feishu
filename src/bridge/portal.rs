use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgePortal {
    pub id: Option<i64>,
    pub feishu_room_id: String,
    pub mxid: String,
    pub name: String,
    pub topic: Option<String>,
    pub avatar_url: Option<String>,
    pub encrypted: bool,
    pub room_type: RoomType,
    pub relay_user_id: Option<String>,
    pub creator_mxid: String,
    pub bridge_info: BridgeInfo,
    pub creation_content: Option<serde_json::Value>,
    pub last_event: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RoomType {
    Group,
    Direct,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeInfo {
    pub bridgebot: String,
    pub creator: String,
    pub protocol: String,
    pub channel: HashMap<String, serde_json::Value>,
}

impl BridgePortal {
    pub fn new(feishu_room_id: String, mxid: String, name: String, creator_mxid: String) -> Self {
        let creator = creator_mxid.clone();
        Self {
            id: None,
            feishu_room_id,
            mxid,
            name,
            topic: None,
            avatar_url: None,
            encrypted: false,
            room_type: RoomType::Group,
            relay_user_id: None,
            creator_mxid,
            bridge_info: BridgeInfo {
                bridgebot: "@feishubot:localhost".to_string(),
                creator,
                protocol: "feishu".to_string(),
                channel: HashMap::new(),
            },
            creation_content: None,
            last_event: None,
        }
    }

    pub fn handle_feishu_message(&self, message: super::message::BridgeMessage) -> Result<()> {
        let matrix_text = crate::formatter::convert_feishu_content_to_matrix_html(&message.content);
        info!(
            "Portal {} handled Feishu->Matrix message {}: {}",
            self.mxid, message.id, matrix_text
        );
        Ok(())
    }

    pub fn handle_matrix_message(&self, message: super::message::BridgeMessage) -> Result<()> {
        let feishu_text = crate::formatter::format_matrix_to_feishu(message.clone())?;
        info!(
            "Portal {} handled Matrix->Feishu message {}: {}",
            self.mxid, message.id, feishu_text
        );
        Ok(())
    }
}
