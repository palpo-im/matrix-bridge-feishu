use sha2::{Digest, Sha256};

use crate::bridge::message_flow::{MatrixInboundMessage, OutboundFeishuMessage};
use crate::bridge::{MatrixEvent, MessageFlow};

pub fn parse_matrix_inbound(event: &MatrixEvent) -> Option<MatrixInboundMessage> {
    let content = event.content.as_ref()?;
    let inbound = MessageFlow::parse_matrix_event(&event.event_type, content)?;
    Some(MatrixInboundMessage {
        event_id: event.event_id.clone(),
        room_id: event.room_id.clone(),
        sender: event.sender.clone(),
        ..inbound
    })
}

pub fn outbound_content_hash(event: &MatrixEvent, outbound: &OutboundFeishuMessage) -> String {
    let mut hasher = Sha256::new();
    if let Some(event_id) = &event.event_id {
        hasher.update(event_id.as_bytes());
    }
    hasher.update(event.room_id.as_bytes());
    hasher.update(event.sender.as_bytes());
    hasher.update(outbound.msg_type.as_bytes());
    hasher.update(outbound.content.as_bytes());
    if let Some(reply_to) = &outbound.reply_to {
        hasher.update(reply_to.as_bytes());
    }
    if let Some(edit_of) = &outbound.edit_of {
        hasher.update(edit_of.as_bytes());
    }
    for attachment in &outbound.attachments {
        hasher.update(attachment.kind.as_bytes());
        hasher.update(attachment.url.as_bytes());
    }
    hex::encode(hasher.finalize())
}

pub fn outbound_delivery_uuid(event_id: Option<&str>, content_hash: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(event_id.unwrap_or("unknown").as_bytes());
    hasher.update(content_hash.as_bytes());
    let digest = hasher.finalize();
    hex::encode(digest)[0..32].to_string()
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use super::{outbound_content_hash, outbound_delivery_uuid, parse_matrix_inbound};
    use crate::bridge::MatrixEvent;
    use crate::bridge::message_flow::{MessageAttachment, OutboundFeishuMessage};

    #[test]
    fn parse_matrix_inbound_keeps_event_context() {
        let event = MatrixEvent {
            event_id: Some("$event123".to_string()),
            event_type: "m.room.message".to_string(),
            room_id: "!room:example.com".to_string(),
            sender: "@alice:example.com".to_string(),
            state_key: None,
            content: Some(json!({
                "msgtype": "m.text",
                "body": "hello"
            })),
            timestamp: Some(Utc::now().timestamp_millis().to_string()),
        };

        let inbound = parse_matrix_inbound(&event).expect("event should parse");
        assert_eq!(inbound.event_id.as_deref(), Some("$event123"));
        assert_eq!(inbound.room_id, "!room:example.com");
        assert_eq!(inbound.sender, "@alice:example.com");
        assert_eq!(inbound.body, "hello");
    }

    #[test]
    fn outbound_content_hash_changes_when_attachment_changes() {
        let event = MatrixEvent {
            event_id: Some("$event".to_string()),
            event_type: "m.room.message".to_string(),
            room_id: "!room:example.com".to_string(),
            sender: "@alice:example.com".to_string(),
            state_key: None,
            content: None,
            timestamp: None,
        };
        let outbound_a = OutboundFeishuMessage {
            content: "hello".to_string(),
            msg_type: "text".to_string(),
            reply_to: None,
            edit_of: None,
            attachments: vec![MessageAttachment {
                name: "a.txt".to_string(),
                url: "mxc://example/a".to_string(),
                kind: "m.file".to_string(),
            }],
        };
        let outbound_b = OutboundFeishuMessage {
            attachments: vec![MessageAttachment {
                name: "b.txt".to_string(),
                url: "mxc://example/b".to_string(),
                kind: "m.file".to_string(),
            }],
            ..outbound_a.clone()
        };

        let hash_a = outbound_content_hash(&event, &outbound_a);
        let hash_b = outbound_content_hash(&event, &outbound_b);
        assert_ne!(hash_a, hash_b);
    }

    #[test]
    fn outbound_delivery_uuid_is_deterministic() {
        let content_hash = "abc123";
        let first = outbound_delivery_uuid(Some("$event"), content_hash);
        let second = outbound_delivery_uuid(Some("$event"), content_hash);
        let different = outbound_delivery_uuid(Some("$event2"), content_hash);

        assert_eq!(first, second);
        assert_ne!(first, different);
        assert_eq!(first.len(), 32);
    }
}
