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
