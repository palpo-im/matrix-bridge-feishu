use std::sync::Arc;

use serde_json::Value;

use crate::bridge::message::{BridgeMessage, MessageType};
use crate::config::Config;
use crate::feishu::FeishuService;

const ATTACHMENT_TYPES: &[&str] = &["m.image", "m.audio", "m.video", "m.file", "m.sticker"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageRelation {
    Reply { event_id: String },
    Replace { event_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageAttachment {
    pub name: String,
    pub url: String,
    pub kind: String,
}

#[derive(Debug, Clone)]
pub struct MatrixInboundMessage {
    pub event_id: Option<String>,
    pub room_id: String,
    pub sender: String,
    pub body: String,
    pub relation: Option<MessageRelation>,
    pub attachments: Vec<MessageAttachment>,
}

#[derive(Debug, Clone)]
pub struct FeishuInboundMessage {
    pub chat_id: String,
    pub sender_id: String,
    pub content: String,
    pub msg_type: String,
    pub attachments: Vec<String>,
    pub reply_to: Option<String>,
    pub edit_of: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OutboundFeishuMessage {
    pub content: String,
    pub msg_type: String,
    pub reply_to: Option<String>,
    pub edit_of: Option<String>,
    pub attachments: Vec<String>,
}

impl OutboundFeishuMessage {
    pub fn render_content(&self) -> String {
        let mut parts = Vec::new();
        if let Some(reply_to) = &self.reply_to {
            parts.push(format!("> reply to {}\n", reply_to));
        }
        if let Some(edit_of) = &self.edit_of {
            parts.push(format!("* (edit of {})\n", edit_of));
        }
        if !self.content.is_empty() {
            parts.push(self.content.clone());
        }
        if !self.attachments.is_empty() {
            parts.push(self.attachments.join("\n"));
        }
        parts.join("")
    }
}

#[derive(Debug, Clone)]
pub struct OutboundMatrixMessage {
    pub body: String,
    pub formatted_body: Option<String>,
    pub msg_type: String,
    pub reply_to: Option<String>,
    pub edit_of: Option<String>,
    pub attachments: Vec<String>,
}

impl OutboundMatrixMessage {
    pub fn render_body(&self) -> String {
        let mut body = self.body.clone();
        if let Some(reply_to) = &self.reply_to {
            body = format!("> reply to {}\n{}", reply_to, body);
        }
        if let Some(edit_of) = &self.edit_of {
            body = format!("* {}\n(edit:{})", body, edit_of);
        }
        if !self.attachments.is_empty() {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&self.attachments.join("\n"));
        }
        body
    }
}

#[derive(Clone)]
pub struct MessageFlow {
    config: Arc<Config>,
    #[allow(dead_code)]
    feishu_service: Arc<FeishuService>,
}

impl MessageFlow {
    pub fn new(config: Arc<Config>, feishu_service: Arc<FeishuService>) -> Self {
        Self {
            config,
            feishu_service,
        }
    }

    pub fn parse_matrix_event(event_type: &str, content: &Value) -> Option<MatrixInboundMessage> {
        if !matches!(event_type, "m.room.message" | "m.sticker") {
            return None;
        }

        let content_for_body = content
            .get("m.new_content")
            .filter(|value| value.is_object())
            .unwrap_or(content);

        let body = content_for_body
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let msgtype = content_for_body
            .get("msgtype")
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                if event_type == "m.sticker" {
                    "m.sticker"
                } else {
                    "m.text"
                }
            })
            .to_string();

        let relation = parse_relation(content);
        let attachments = parse_attachments(content_for_body, &msgtype);

        if body.is_empty() && attachments.is_empty() {
            return None;
        }

        Some(MatrixInboundMessage {
            event_id: None,
            room_id: String::new(),
            sender: String::new(),
            body,
            relation,
            attachments,
        })
    }

    pub fn matrix_to_feishu(&self, message: &MatrixInboundMessage) -> OutboundFeishuMessage {
        let reply_to = match &message.relation {
            Some(MessageRelation::Reply { event_id }) => Some(event_id.clone()),
            _ => None,
        };
        let edit_of = match &message.relation {
            Some(MessageRelation::Replace { event_id }) => Some(event_id.clone()),
            _ => None,
        };
        let attachments = message
            .attachments
            .iter()
            .map(|attachment| attachment.url.clone())
            .collect();

        let content = self.format_for_feishu(&message.body);
        let msg_type = if self.config.bridge.enable_rich_text {
            "post".to_string()
        } else {
            "text".to_string()
        };

        OutboundFeishuMessage {
            content,
            msg_type,
            reply_to,
            edit_of,
            attachments,
        }
    }

    pub fn feishu_to_matrix(&self, message: &FeishuInboundMessage) -> OutboundMatrixMessage {
        let body = self.format_for_matrix(&message.content);
        let formatted_body = if self.config.bridge.allow_html {
            Some(crate::formatter::convert_feishu_content_to_matrix_html(
                &message.content,
            ))
        } else {
            None
        };

        OutboundMatrixMessage {
            body,
            formatted_body,
            msg_type: "m.text".to_string(),
            reply_to: message.reply_to.clone(),
            edit_of: message.edit_of.clone(),
            attachments: message.attachments.clone(),
        }
    }

    fn format_for_feishu(&self, content: &str) -> String {
        let mut result = content.to_string();

        if self.config.bridge.allow_html {
            result = crate::formatter::convert_matrix_html_to_feishu(&result);
        }

        if self.config.bridge.allow_markdown {
            result = crate::formatter::convert_matrix_markdown_to_feishu(&result);
        } else {
            result = crate::formatter::convert_matrix_text_to_feishu(&result);
        }

        result
    }

    fn format_for_matrix(&self, content: &str) -> String {
        let mut result = content.to_string();

        if self.config.bridge.convert_cards {
            result = crate::formatter::convert_feishu_emoticons(&result);
        }

        result
    }

    pub fn convert_bridge_message_to_feishu(message: &BridgeMessage) -> FeishuInboundMessage {
        let msg_type = match message.msg_type {
            MessageType::Text => "text",
            MessageType::Image => "image",
            MessageType::Video => "video",
            MessageType::Audio => "audio",
            MessageType::File => "file",
            MessageType::Markdown => "text",
            MessageType::RichText => "post",
            MessageType::Card => "interactive",
        };

        FeishuInboundMessage {
            chat_id: message.room_id.clone(),
            sender_id: message.sender.clone(),
            content: message.content.clone(),
            msg_type: msg_type.to_string(),
            attachments: message.attachments.iter().map(|a| a.url.clone()).collect(),
            reply_to: None,
            edit_of: None,
        }
    }
}

fn parse_relation(content: &Value) -> Option<MessageRelation> {
    let relates_to = content.get("m.relates_to")?;
    if let Some(reply_event_id) = relates_to
        .get("m.in_reply_to")
        .and_then(|inner| inner.get("event_id"))
        .and_then(Value::as_str)
    {
        return Some(MessageRelation::Reply {
            event_id: reply_event_id.to_string(),
        });
    }
    let rel_type = relates_to.get("rel_type").and_then(Value::as_str);
    if rel_type == Some("m.replace") {
        if let Some(edit_event_id) = relates_to.get("event_id").and_then(Value::as_str) {
            return Some(MessageRelation::Replace {
                event_id: edit_event_id.to_string(),
            });
        }
    }
    None
}

fn parse_attachments(content: &Value, msgtype: &str) -> Vec<MessageAttachment> {
    if !ATTACHMENT_TYPES.contains(&msgtype) {
        return Vec::new();
    }
    let Some(url) = content.get("url").and_then(Value::as_str) else {
        return Vec::new();
    };
    let name = content
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("matrix-media")
        .to_string();

    vec![MessageAttachment {
        name,
        url: url.to_string(),
        kind: msgtype.to_string(),
    }]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{MessageFlow, MessageRelation};

    #[test]
    fn parse_matrix_event_extracts_reply_and_attachment() {
        let content = json!({
            "msgtype": "m.image",
            "body": "cat.png",
            "url": "mxc://example.org/cat",
            "m.relates_to": {
                "m.in_reply_to": {
                    "event_id": "$source"
                }
            }
        });

        let parsed = MessageFlow::parse_matrix_event("m.room.message", &content)
            .expect("matrix message should parse");
        assert_eq!(
            parsed.relation,
            Some(MessageRelation::Reply {
                event_id: "$source".to_string()
            })
        );
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].url, "mxc://example.org/cat");
    }

    #[test]
    fn parse_matrix_event_extracts_edit() {
        let content = json!({
            "msgtype": "m.text",
            "body": "* new body",
            "m.relates_to": {
                "rel_type": "m.replace",
                "event_id": "$old"
            },
            "m.new_content": {
                "msgtype": "m.text",
                "body": "new body"
            }
        });

        let parsed = MessageFlow::parse_matrix_event("m.room.message", &content)
            .expect("matrix message should parse");
        assert_eq!(
            parsed.relation,
            Some(MessageRelation::Replace {
                event_id: "$old".to_string()
            })
        );
    }
}
