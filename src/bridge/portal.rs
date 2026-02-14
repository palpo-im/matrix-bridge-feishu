use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

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
                creator: creator_mxid.clone(),
                protocol: "feishu".to_string(),
                channel: HashMap::new(),
            },
            creation_content: None,
            last_event: None,
        }
    }

    pub async fn handle_feishu_message(&self, message: super::message::BridgeMessage) -> Result<()> {
        // TODO: Convert and send message to Matrix
        Ok(())
    }

    pub async fn handle_matrix_message(&self, message: super::message::BridgeMessage) -> Result<()> {
        // TODO: Convert and send message to Feishu
        Ok(())
    }
}