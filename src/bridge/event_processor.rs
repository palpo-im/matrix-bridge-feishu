use std::sync::Arc;

use serde_json::Value;
use tracing::{debug, info, warn};

use crate::bridge::command_handler::{MatrixCommandHandler, MatrixCommandOutcome};
use crate::bridge::message_flow::{MatrixInboundMessage, MessageFlow};
use crate::config::Config;
use crate::database::{
    EventStore, MessageMapping, MessageStore, ProcessedEvent, RoomMapping, RoomStore,
};
use crate::feishu::FeishuService;

#[derive(Debug, Clone)]
pub struct MatrixEvent {
    pub event_id: Option<String>,
    pub event_type: String,
    pub room_id: String,
    pub sender: String,
    pub state_key: Option<String>,
    pub content: Option<Value>,
    pub timestamp: Option<String>,
}

pub struct MatrixEventProcessor {
    config: Arc<Config>,
    feishu_service: Arc<FeishuService>,
    room_store: Arc<dyn RoomStore>,
    message_store: Arc<dyn MessageStore>,
    event_store: Arc<dyn EventStore>,
    message_flow: Arc<MessageFlow>,
    command_handler: MatrixCommandHandler,
}

impl MatrixEventProcessor {
    pub fn new(
        config: Arc<Config>,
        feishu_service: Arc<FeishuService>,
        room_store: Arc<dyn RoomStore>,
        message_store: Arc<dyn MessageStore>,
        event_store: Arc<dyn EventStore>,
        message_flow: Arc<MessageFlow>,
    ) -> Self {
        let self_service = true;
        Self {
            config,
            feishu_service,
            room_store,
            message_store,
            event_store,
            message_flow,
            command_handler: MatrixCommandHandler::new(self_service),
        }
    }

    pub async fn process_event(&self, event: MatrixEvent) -> anyhow::Result<()> {
        debug!(
            "Processing Matrix event: {:?} type={} room={}",
            event.event_id, event.event_type, event.room_id
        );

        if let Some(ref event_id) = event.event_id {
            if self.event_store.is_event_processed(event_id).await? {
                debug!("Skipping already processed event: {}", event_id);
                return Ok(());
            }
        }

        match event.event_type.as_str() {
            "m.room.message" | "m.sticker" => {
                self.handle_message_event(&event).await?;
            }
            "m.room.member" => {
                self.handle_member_event(&event).await?;
            }
            "m.room.redaction" => {
                self.handle_redaction_event(&event).await?;
            }
            "m.reaction" => {
                self.handle_reaction_event(&event).await?;
            }
            _ => {
                debug!("Ignoring event type: {}", event.event_type);
            }
        }

        if let Some(event_id) = &event.event_id {
            let processed = ProcessedEvent {
                id: 0,
                event_id: event_id.clone(),
                event_type: event.event_type.clone(),
                source: "matrix".to_string(),
                processed_at: chrono::Utc::now(),
            };
            self.event_store.mark_event_processed(&processed).await?;
        }

        Ok(())
    }

    async fn handle_message_event(&self, event: &MatrixEvent) -> anyhow::Result<()> {
        let Some(content) = &event.content else {
            debug!("Message event has no content");
            return Ok(());
        };

        let body = content
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default();

        if self.command_handler.is_command(body) {
            return self.handle_command(event, body).await;
        }

        let room_mapping = self
            .room_store
            .get_room_by_matrix_id(&event.room_id)
            .await?;

        let Some(mapping) = room_mapping else {
            debug!("No room mapping found for room {}", event.room_id);
            return Ok(());
        };

        let Some(inbound) = MessageFlow::parse_matrix_event(&event.event_type, content) else {
            debug!("Failed to parse message event");
            return Ok(());
        };

        let outbound = self.message_flow.matrix_to_feishu(&MatrixInboundMessage {
            event_id: event.event_id.clone(),
            room_id: event.room_id.clone(),
            sender: event.sender.clone(),
            ..inbound
        });

        let content = outbound.render_content();
        debug!(
            "Sending message to Feishu chat {}: {}",
            mapping.feishu_chat_id,
            preview_text(&content, 100)
        );

        let feishu_message_id = self
            .feishu_service
            .send_text_message(&mapping.feishu_chat_id, &content)
            .await?;

        if let Some(event_id) = &event.event_id {
            let link = MessageMapping::new(
                event_id.clone(),
                feishu_message_id,
                event.room_id.clone(),
                event.sender.clone(),
                "matrix".to_string(),
            );
            if let Err(err) = self.message_store.create_message_mapping(&link).await {
                warn!(
                    "Failed to persist Matrix->Feishu message mapping for event {}: {}",
                    event_id, err
                );
            }
        }

        info!(
            "Bridged Matrix message to Feishu: room={} chat={}",
            event.room_id, mapping.feishu_chat_id
        );

        Ok(())
    }

    async fn handle_command(&self, event: &MatrixEvent, body: &str) -> anyhow::Result<()> {
        let room_mapping = self
            .room_store
            .get_room_by_matrix_id(&event.room_id)
            .await?;

        let is_bridged = room_mapping.is_some();

        let outcome = self.command_handler.handle(body, is_bridged, |_| true);

        match outcome {
            MatrixCommandOutcome::Ignored => {}
            MatrixCommandOutcome::Reply(reply) => {
                warn!("Command reply not implemented: {}", reply);
            }
            MatrixCommandOutcome::BridgeRequested { feishu_chat_id } => {
                self.handle_bridge_request(&event.room_id, &event.sender, &feishu_chat_id)
                    .await?;
            }
            MatrixCommandOutcome::UnbridgeRequested => {
                self.handle_unbridge_request(&event.room_id).await?;
            }
        }

        Ok(())
    }

    async fn handle_bridge_request(
        &self,
        room_id: &str,
        sender: &str,
        feishu_chat_id: &str,
    ) -> anyhow::Result<()> {
        info!(
            "Bridge request from {} for room {} to chat {}",
            sender, room_id, feishu_chat_id
        );

        if self
            .room_store
            .get_room_by_feishu_id(feishu_chat_id)
            .await?
            .is_some()
        {
            warn!("Feishu chat {} is already bridged", feishu_chat_id);
            return Ok(());
        }

        let mapping = RoomMapping::new(
            room_id.to_string(),
            feishu_chat_id.to_string(),
            Some("Bridged Room".to_string()),
        );

        self.room_store.create_room_mapping(&mapping).await?;
        info!("Created bridge mapping: {} <-> {}", room_id, feishu_chat_id);

        Ok(())
    }

    async fn handle_unbridge_request(&self, room_id: &str) -> anyhow::Result<()> {
        let Some(mapping) = self.room_store.get_room_by_matrix_id(room_id).await? else {
            warn!("Room {} is not bridged", room_id);
            return Ok(());
        };

        self.room_store.delete_room_mapping(mapping.id).await?;
        info!("Removed bridge mapping for room {}", room_id);

        Ok(())
    }

    async fn handle_member_event(&self, event: &MatrixEvent) -> anyhow::Result<()> {
        let Some(content) = &event.content else {
            return Ok(());
        };

        let membership = content
            .get("membership")
            .and_then(Value::as_str)
            .unwrap_or("leave");

        debug!(
            "Member event: sender={} state_key={} membership={}",
            event.sender,
            event.state_key.as_deref().unwrap_or(""),
            membership
        );

        Ok(())
    }

    async fn handle_redaction_event(&self, event: &MatrixEvent) -> anyhow::Result<()> {
        if !self.config.bridge.bridge_matrix_redactions {
            return Ok(());
        }

        let Some(redacts) = event.content.as_ref().and_then(|c| c.get("redacts")) else {
            return Ok(());
        };

        debug!("Redaction event for: {:?}", redacts);
        Ok(())
    }

    async fn handle_reaction_event(&self, event: &MatrixEvent) -> anyhow::Result<()> {
        if !self.config.bridge.bridge_matrix_reactions {
            return Ok(());
        }

        let Some(content) = &event.content else {
            return Ok(());
        };

        let relates_to = content.get("m.relates_to");
        debug!("Reaction event: {:?}", relates_to);
        Ok(())
    }
}

fn preview_text(text: &str, max_len: usize) -> String {
    let chars: String = text.chars().take(max_len).collect();
    if text.len() > max_len {
        format!("{}...", chars)
    } else {
        chars
    }
}
