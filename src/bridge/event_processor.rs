use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::{debug, info, warn};

use crate::bridge::command_handler::{MatrixCommandHandler, MatrixCommandOutcome};
use crate::bridge::matrix_event_parser::{
    outbound_content_hash, outbound_delivery_uuid, parse_matrix_inbound,
};
use crate::bridge::matrix_to_feishu_dispatcher::MatrixToFeishuDispatcher;
use crate::bridge::message_flow::MessageFlow;
use crate::config::Config;
use crate::database::{
    EventStore, MediaStore, MessageMapping, MessageStore, ProcessedEvent, RoomMapping, RoomStore,
};
use crate::feishu::{FeishuMessageSendData, FeishuService};
use crate::web::{ScopedTimer, global_metrics};

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
    dispatcher: MatrixToFeishuDispatcher,
    rate_limiter: RoomRateLimiter,
    blocked_msgtypes: HashSet<String>,
}

impl MatrixEventProcessor {
    pub fn new(
        config: Arc<Config>,
        feishu_service: Arc<FeishuService>,
        room_store: Arc<dyn RoomStore>,
        message_store: Arc<dyn MessageStore>,
        event_store: Arc<dyn EventStore>,
        media_store: Arc<dyn MediaStore>,
        message_flow: Arc<MessageFlow>,
    ) -> Self {
        let self_service = true;
        let dispatcher = MatrixToFeishuDispatcher::new(
            config.clone(),
            feishu_service.clone(),
            message_store.clone(),
            media_store,
        );
        let rate_limiter =
            RoomRateLimiter::new(config.bridge.message_limit, config.bridge.message_cooldown);
        let blocked_msgtypes = config
            .bridge
            .blocked_matrix_msgtypes
            .iter()
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
            .collect();

        Self {
            config,
            feishu_service,
            room_store,
            message_store,
            event_store,
            message_flow,
            command_handler: MatrixCommandHandler::new(self_service),
            dispatcher,
            rate_limiter,
            blocked_msgtypes,
        }
    }

    pub async fn process_event(&self, event: MatrixEvent) -> anyhow::Result<()> {
        let _timer = ScopedTimer::new("matrix_event_process");
        let matrix_event_id = event.event_id.as_deref().unwrap_or("unknown");
        global_metrics().record_inbound_event(&format!("matrix:{}", event.event_type));
        debug!(
            matrix_event_id = %matrix_event_id,
            event_type = %event.event_type,
            chat_id = %event.room_id,
            "Processing Matrix event"
        );

        if let Some(ref event_id) = event.event_id {
            if self.event_store.is_event_processed(event_id).await? {
                debug!(matrix_event_id = %event_id, "Skipping already processed Matrix event");
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
                debug!(event_type = %event.event_type, "Ignoring Matrix event type");
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
        let content_for_policy = content
            .get("m.new_content")
            .filter(|value| value.is_object())
            .unwrap_or(content);
        let matrix_msgtype = content_for_policy
            .get("msgtype")
            .and_then(Value::as_str)
            .unwrap_or("m.text");

        let body = content_for_policy
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default();

        if self.command_handler.is_command(body) {
            return self.handle_command(event, body).await;
        }

        if self
            .blocked_msgtypes
            .contains(&matrix_msgtype.to_ascii_lowercase())
        {
            global_metrics().record_policy_block("msgtype_blocked");
            warn!(
                matrix_event_id = %event.event_id.as_deref().unwrap_or("unknown"),
                chat_id = %event.room_id,
                msgtype = %matrix_msgtype,
                "Blocking Matrix event due to blocked msgtype policy"
            );
            return Ok(());
        }
        if !self.rate_limiter.allow(&event.room_id) {
            global_metrics().record_policy_block("rate_limited");
            warn!(
                matrix_event_id = %event.event_id.as_deref().unwrap_or("unknown"),
                chat_id = %event.room_id,
                "Dropping Matrix event due to per-room rate limit"
            );
            return Ok(());
        }

        let room_mapping = self
            .room_store
            .get_room_by_matrix_id(&event.room_id)
            .await?;

        let Some(mapping) = room_mapping else {
            debug!("No room mapping found for room {}", event.room_id);
            return Ok(());
        };

        let Some(inbound) = parse_matrix_inbound(event) else {
            debug!("Failed to parse message event");
            return Ok(());
        };

        let mut outbound = self.message_flow.matrix_to_feishu(&inbound);
        if self.config.bridge.max_text_length > 0 {
            let (truncated, changed) =
                truncate_text(&outbound.content, self.config.bridge.max_text_length);
            if changed {
                global_metrics().record_degraded_event("text_truncated");
                warn!(
                    matrix_event_id = %event.event_id.as_deref().unwrap_or("unknown"),
                    chat_id = %event.room_id,
                    max_text_length = self.config.bridge.max_text_length,
                    "Truncated outbound Matrix message due to max_text_length policy"
                );
                outbound.content = truncated;
            }
        }
        let content_hash = outbound_content_hash(event, &outbound);

        if let Some(existing) = self
            .message_store
            .get_message_by_content_hash(&content_hash)
            .await?
        {
            debug!(
                matrix_event_id = %event.event_id.as_deref().unwrap_or("unknown"),
                dedupe_hash = %content_hash,
                feishu_message_id = %existing.feishu_message_id,
                "Skipping duplicate outbound Matrix message based on content hash"
            );
            return Ok(());
        }

        if let Some(event_id) = &outbound.edit_of {
            self.dispatcher
                .handle_edit_message(event_id, &outbound)
                .await?;
            return Ok(());
        }

        let delivery_uuid = Some(outbound_delivery_uuid(
            event.event_id.as_deref(),
            &content_hash,
        ));
        let send_result = async {
            let mut primary_feishu_message = self
                .dispatcher
                .send_outbound_message(&mapping, &outbound, delivery_uuid)
                .await?;

            let attachment_message_ids = self
                .dispatcher
                .forward_attachments_to_feishu(&mapping, &outbound.attachments)
                .await?;

            if primary_feishu_message.is_none() {
                primary_feishu_message =
                    attachment_message_ids.first().cloned().map(|message_id| {
                        FeishuMessageSendData {
                            message_id,
                            root_id: None,
                            parent_id: None,
                            thread_id: None,
                        }
                    });
            }

            Ok::<Option<FeishuMessageSendData>, anyhow::Error>(primary_feishu_message)
        }
        .await;

        let primary_feishu_message = match send_result {
            Ok(message) => message,
            Err(err) => {
                if !self.config.bridge.enable_failure_degrade {
                    return Err(err);
                }

                global_metrics().record_degraded_event("delivery_failure");
                warn!(
                    matrix_event_id = %event.event_id.as_deref().unwrap_or("unknown"),
                    chat_id = %event.room_id,
                    feishu_chat_id = %mapping.feishu_chat_id,
                    error = %err,
                    "Outbound Matrix message failed; applying degrade template"
                );
                self.send_failure_degrade_notice(&mapping, event, &err)
                    .await;
                return Ok(());
            }
        };

        if let (Some(event_id), Some(feishu_message)) = (&event.event_id, primary_feishu_message) {
            let link = MessageMapping::new(
                event_id.clone(),
                feishu_message.message_id,
                event.room_id.clone(),
                event.sender.clone(),
                "matrix".to_string(),
            )
            .with_threading(
                feishu_message.thread_id,
                feishu_message.root_id,
                feishu_message.parent_id,
            )
            .with_content_hash(Some(content_hash));

            if let Err(err) = self.message_store.create_message_mapping(&link).await {
                warn!(
                    matrix_event_id = %event_id,
                    feishu_message_id = %link.feishu_message_id,
                    chat_id = %event.room_id,
                    error = %err,
                    "Failed to persist Matrix->Feishu message mapping"
                );
            }
        }

        info!(
            matrix_event_id = %event.event_id.as_deref().unwrap_or("unknown"),
            chat_id = %event.room_id,
            feishu_chat_id = %mapping.feishu_chat_id,
            "Bridged Matrix message to Feishu"
        );

        Ok(())
    }

    async fn send_failure_degrade_notice(
        &self,
        mapping: &RoomMapping,
        event: &MatrixEvent,
        err: &anyhow::Error,
    ) {
        let template = self.config.bridge.failure_notice_template.trim();
        if template.is_empty() {
            return;
        }

        let message = render_failure_notice(template, event, err);
        if let Err(notice_err) = self
            .feishu_service
            .send_text_message(&mapping.feishu_chat_id, &message)
            .await
        {
            warn!(
                matrix_event_id = %event.event_id.as_deref().unwrap_or("unknown"),
                chat_id = %event.room_id,
                feishu_chat_id = %mapping.feishu_chat_id,
                error = %notice_err,
                "Failed to send failure degrade notice to Feishu"
            );
        }
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

        let redacts_event_id = event
            .content
            .as_ref()
            .and_then(|c| c.get("redacts"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        let Some(redacts_event_id) = redacts_event_id else {
            return Ok(());
        };

        let mapping = self
            .message_store
            .get_message_by_matrix_id(&redacts_event_id)
            .await?;

        let Some(mapping) = mapping else {
            debug!(
                "No Matrix->Feishu mapping found for redacted event {}",
                redacts_event_id
            );
            return Ok(());
        };

        self.feishu_service
            .recall_message(&mapping.feishu_message_id)
            .await?;

        if let Err(err) = self.message_store.delete_message_mapping(mapping.id).await {
            warn!(
                matrix_event_id = %redacts_event_id,
                feishu_message_id = %mapping.feishu_message_id,
                chat_id = %mapping.room_id,
                error = %err,
                "Failed to delete mapping after redaction"
            );
        }

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

struct RoomRateLimiter {
    limit: usize,
    window: Duration,
    events_by_room: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl RoomRateLimiter {
    fn new(limit: u32, window_millis: u64) -> Self {
        Self {
            limit: limit as usize,
            window: Duration::from_millis(window_millis),
            events_by_room: Mutex::new(HashMap::new()),
        }
    }

    fn allow(&self, room_id: &str) -> bool {
        if self.limit == 0 || self.window.is_zero() {
            return true;
        }

        let now = Instant::now();
        let mut guard = self
            .events_by_room
            .lock()
            .expect("rate limiter mutex poisoned");
        let queue = guard.entry(room_id.to_string()).or_default();

        while let Some(timestamp) = queue.front() {
            if now.duration_since(*timestamp) > self.window {
                queue.pop_front();
            } else {
                break;
            }
        }

        if queue.len() >= self.limit {
            return false;
        }

        queue.push_back(now);
        true
    }
}

fn truncate_text(text: &str, max_chars: usize) -> (String, bool) {
    if max_chars == 0 {
        return (text.to_string(), false);
    }

    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return (text.to_string(), false);
    }

    let mut truncated: String = text.chars().take(max_chars).collect();
    truncated.push_str(" …");
    (truncated, true)
}

fn render_failure_notice(template: &str, event: &MatrixEvent, err: &anyhow::Error) -> String {
    template
        .replace(
            "{matrix_event_id}",
            event.event_id.as_deref().unwrap_or("unknown"),
        )
        .replace("{matrix_room_id}", &event.room_id)
        .replace("{error}", &err.to_string())
}
