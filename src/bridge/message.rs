use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeMessage {
    pub id: String,
    pub sender: String,
    pub room_id: String,
    pub content: String,
    pub msg_type: MessageType,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageType {
    Text,
    Image,
    Video,
    Audio,
    File,
    Markdown,
    RichText,
    Card,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub url: String,
    pub size: u64,
    pub mime_type: String,
}

impl BridgeMessage {
    pub fn new_text(sender: String, room_id: String, content: String) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            sender,
            room_id,
            content,
            msg_type: MessageType::Text,
            timestamp: chrono::Utc::now(),
            attachments: vec![],
        }
    }

    pub fn new_rich_text(sender: String, room_id: String, content: String) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            sender,
            room_id,
            content,
            msg_type: MessageType::RichText,
            timestamp: chrono::Utc::now(),
            attachments: vec![],
        }
    }

    pub fn new_card(sender: String, room_id: String, content: String) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            sender,
            room_id,
            content,
            msg_type: MessageType::Card,
            timestamp: chrono::Utc::now(),
            attachments: vec![],
        }
    }
}
