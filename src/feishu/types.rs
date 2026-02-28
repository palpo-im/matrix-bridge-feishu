use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuMessage {
    pub message_id: String,
    pub chat_id: String,
    pub chat_type: String,
    pub sender_id: String,
    pub sender_type: String,
    pub create_time: DateTime<Utc>,
    pub update_time: Option<DateTime<Utc>>,
    pub delete_time: Option<DateTime<Utc>>,
    pub msg_type: String,
    pub parent_id: Option<String>,
    pub thread_id: Option<String>,
    pub root_id: Option<String>,
    pub mentioned_sender: Option<String>,
    pub mentioned_users: Vec<String>,
    pub mentioned_chats: Vec<String>,
    pub content: FeishuMessageContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuMessageContent {
    pub text: Option<String>,
    pub rich_text: Option<FeishuRichText>,
    pub image_key: Option<String>,
    pub file_key: Option<String>,
    pub audio_key: Option<String>,
    pub video_key: Option<String>,
    pub sticker_id: Option<String>,
    pub card: Option<FeishuCard>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuRichText {
    pub title: Option<String>,
    pub content: Vec<FeishuRichTextElement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuRichTextElement {
    pub segment_type: String,
    pub content: FeishuRichTextContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuRichTextContent {
    pub text: Option<String>,
    pub link: Option<String>,
    pub mention: Option<FeishuMention>,
    pub image: Option<FeishuImage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuMention {
    pub user_id: Option<String>,
    pub chat_id: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuImage {
    pub image_key: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuCard {
    pub card_type: String,
    pub header: Option<FeishuCardHeader>,
    pub elements: Vec<FeishuCardElement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuCardHeader {
    pub title: String,
    pub subtitle: Option<String>,
    pub ud_link: Option<String>,
    pub avatar: Option<FeishuCardAvatar>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuCardAvatar {
    pub token: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuCardElement {
    pub tag: String,
    pub text: Option<FeishuCardText>,
    pub button: Option<FeishuCardButton>,
    pub image: Option<FeishuCardImage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuCardText {
    pub content: String,
    pub tag: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuCardButton {
    pub text: FeishuCardText,
    pub url: String,
    pub type_field: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuCardImage {
    pub img_key: String,
    pub alt: Option<String>,
    pub preview: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuUser {
    pub user_id: String,
    pub name: String,
    pub en_name: Option<String>,
    pub email: Option<String>,
    pub mobile: Option<String>,
    pub avatar: Option<FeishuAvatar>,
    pub status: FeishuUserStatus,
    pub department_ids: Vec<String>,
    pub leader_user_id: Option<String>,
    pub position: Option<String>,
    pub employee_no: Option<String>,
    pub employee_type: u32,
    pub join_time: i64,
    pub custom_attrs: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuAvatar {
    pub avatar_72: String,
    pub avatar_240: String,
    pub avatar_640: String,
    pub avatar_origin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuUserStatus {
    pub is_activated: bool,
    pub is_exited: bool,
    pub is_resigned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuChat {
    pub chat_id: String,
    pub name: Option<String>,
    pub avatar: Option<String>,
    pub description: Option<String>,
    pub chat_mode: String,
    pub chat_type: String,
    pub external: bool,
    pub tenant_key: String,
    pub chat_biz_type: String,
    pub chat_status: String,
    pub add_member_permission: String,
    pub share_group_card_permission: String,
    pub at_all_permission: String,
    pub edit_group_announcement_permission: String,
    pub edit_group_name_permission: String,
    pub owner_id: String,
    pub chat_info: FeishuChatInfo,
    pub member_count: u32,
    #[serde(rename = "public")]
    pub is_public: bool,
    pub public_extra: FeishuPublicExtra,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuChatInfo {
    pub create_time: Option<i64>,
    pub creator: Option<String>,
    pub update_time: Option<i64>,
    pub updater: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuPublicExtra {
    pub joinable: bool,
    pub need_approval: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuWebhookEvent {
    pub schema: String,
    pub header: FeishuWebhookHeader,
    pub event: FeishuWebhookEventContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuWebhookHeader {
    pub event_id: String,
    pub event_type: String,
    pub create_time: String,
    pub token: String,
    pub app_id: String,
    pub tenant_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuWebhookEventContent {
    pub sender: FeishuWebhookSender,
    pub message: Option<FeishuWebhookMessage>,
    pub chat: Option<FeishuWebhookChat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuWebhookSender {
    pub sender_id: FeishuWebhookSenderId,
    pub sender_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuWebhookSenderId {
    pub user_id: String,
    pub union_id: Option<String>,
    pub open_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuWebhookMessage {
    pub message_id: String,
    pub chat_id: String,
    pub chat_type: String,
    pub msg_type: String,
    pub parent_id: Option<String>,
    pub thread_id: Option<String>,
    pub root_id: Option<String>,
    pub content: serde_json::Value,
    pub create_time: String,
    pub update_time: Option<String>,
    pub delete_time: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuWebhookChat {
    pub chat_id: String,
    pub chat_type: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuApiEnvelope<T> {
    pub code: i64,
    pub msg: String,
    pub data: Option<T>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuMessageSendData {
    pub message_id: String,
    pub root_id: Option<String>,
    pub parent_id: Option<String>,
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuMessageListData {
    #[serde(default)]
    pub items: Vec<FeishuMessageData>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuMessageData {
    pub message_id: String,
    pub root_id: Option<String>,
    pub parent_id: Option<String>,
    pub thread_id: Option<String>,
    pub msg_type: Option<String>,
    pub chat_id: Option<String>,
    pub deleted: Option<bool>,
    pub updated: Option<bool>,
    pub create_time: Option<String>,
    pub update_time: Option<String>,
    pub body: Option<FeishuMessageBody>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuMessageBody {
    pub content: Option<String>,
    pub mentions: Option<Vec<FeishuMessageMention>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuMessageMention {
    pub key: Option<String>,
    pub name: Option<String>,
    pub id: Option<FeishuMessageMentionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuMessageMentionId {
    pub id: Option<String>,
    pub id_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuImageUploadData {
    pub image_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuFileUploadData {
    pub file_key: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CapabilityStatus {
    Supported,
    Partial,
    Unsupported,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FeishuCapabilityMatrixRow {
    pub capability: &'static str,
    pub status: CapabilityStatus,
    pub degrade_strategy: &'static str,
    pub code_entry: &'static str,
}

pub const FEISHU_MESSAGE_CAPABILITIES: &[FeishuCapabilityMatrixRow] = &[
    FeishuCapabilityMatrixRow {
        capability: "text",
        status: CapabilityStatus::Supported,
        degrade_strategy: "fallback to plain text",
        code_entry: "src/feishu/service.rs:webhook_event_to_bridge_message",
    },
    FeishuCapabilityMatrixRow {
        capability: "post",
        status: CapabilityStatus::Supported,
        degrade_strategy: "flatten post blocks to readable text",
        code_entry: "src/feishu/service.rs:extract_text_from_post_content",
    },
    FeishuCapabilityMatrixRow {
        capability: "interactive/card",
        status: CapabilityStatus::Partial,
        degrade_strategy: "extract key header/actions text when unsupported",
        code_entry: "src/feishu/service.rs:extract_text_from_card_content",
    },
    FeishuCapabilityMatrixRow {
        capability: "image/file/audio/media/sticker",
        status: CapabilityStatus::Supported,
        degrade_strategy: "bridge as Matrix attachment or placeholder text",
        code_entry: "src/feishu/service.rs:webhook_event_to_bridge_message",
    },
];

pub const FEISHU_EVENT_CAPABILITIES: &[FeishuCapabilityMatrixRow] = &[
    FeishuCapabilityMatrixRow {
        capability: "im.message.receive_v1",
        status: CapabilityStatus::Supported,
        degrade_strategy: "unknown msg_type falls back to text",
        code_entry: "src/feishu/service.rs:handle_webhook",
    },
    FeishuCapabilityMatrixRow {
        capability: "im.message.recalled_v1",
        status: CapabilityStatus::Supported,
        degrade_strategy: "missing mapping logs and skips",
        code_entry: "src/bridge/feishu_bridge.rs:handle_feishu_message_recalled",
    },
    FeishuCapabilityMatrixRow {
        capability: "im.chat.member.user.added_v1 / deleted_v1",
        status: CapabilityStatus::Supported,
        degrade_strategy: "no room mapping then skip with debug log",
        code_entry: "src/bridge/feishu_bridge.rs:handle_feishu_chat_member_added",
    },
    FeishuCapabilityMatrixRow {
        capability: "im.chat.updated_v1",
        status: CapabilityStatus::Supported,
        degrade_strategy: "partial fields are merged into existing mapping",
        code_entry: "src/bridge/feishu_bridge.rs:handle_feishu_chat_updated",
    },
    FeishuCapabilityMatrixRow {
        capability: "im.chat.disbanded_v1",
        status: CapabilityStatus::Supported,
        degrade_strategy: "missing mapping clears in-memory cache only",
        code_entry: "src/bridge/feishu_bridge.rs:handle_feishu_chat_disbanded",
    },
];
