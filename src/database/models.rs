use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomMapping {
    pub id: i64,
    pub matrix_room_id: String,
    pub feishu_chat_id: String,
    pub feishu_chat_name: Option<String>,
    pub feishu_chat_type: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMapping {
    pub id: i64,
    pub matrix_user_id: String,
    pub feishu_user_id: String,
    pub feishu_username: Option<String>,
    pub feishu_avatar: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageMapping {
    pub id: i64,
    pub matrix_event_id: String,
    pub feishu_message_id: String,
    pub room_id: String,
    pub sender_mxid: String,
    pub sender_feishu_id: String,
    pub content_hash: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessedEvent {
    pub id: i64,
    pub event_id: String,
    pub event_type: String,
    pub source: String,
    pub processed_at: DateTime<Utc>,
}

impl RoomMapping {
    pub fn new(
        matrix_room_id: String,
        feishu_chat_id: String,
        feishu_chat_name: Option<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: 0,
            matrix_room_id,
            feishu_chat_id,
            feishu_chat_name,
            feishu_chat_type: "group".to_string(),
            created_at: now,
            updated_at: now,
        }
    }
}

impl UserMapping {
    pub fn new(
        matrix_user_id: String,
        feishu_user_id: String,
        feishu_username: Option<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: 0,
            matrix_user_id,
            feishu_user_id,
            feishu_username,
            feishu_avatar: None,
            created_at: now,
            updated_at: now,
        }
    }
}

impl MessageMapping {
    pub fn new(
        matrix_event_id: String,
        feishu_message_id: String,
        room_id: String,
        sender_mxid: String,
        sender_feishu_id: String,
    ) -> Self {
        Self {
            id: 0,
            matrix_event_id,
            feishu_message_id,
            room_id,
            sender_mxid,
            sender_feishu_id,
            content_hash: None,
            created_at: Utc::now(),
        }
    }
}
