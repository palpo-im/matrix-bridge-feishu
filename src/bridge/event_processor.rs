use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::bridge::command_handler::{MatrixCommandHandler, MatrixCommandOutcome};
use crate::bridge::matrix_event_parser::{
    outbound_content_hash, outbound_delivery_uuid, parse_matrix_inbound,
};
use crate::bridge::matrix_to_feishu_dispatcher::MatrixToFeishuDispatcher;
use crate::bridge::message_flow::{MessageFlow, OutboundFeishuMessage};
use crate::config::Config;
use crate::database::{
    EventStore, MediaStore, MessageMapping, MessageStore, ProcessedEvent, RoomMapping, RoomStore,
    UserStore,
};
use crate::feishu::{FeishuMessageSendData, FeishuService};
use crate::util::build_trace_id;
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
    user_store: Arc<dyn UserStore>,
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
        user_store: Arc<dyn UserStore>,
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
            user_store,
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
        let trace_id = build_trace_id("matrix_in", event.event_id.as_deref(), None);
        global_metrics().record_inbound_event(&format!("matrix:{}", event.event_type));
        global_metrics().record_trace_event("matrix_in", "received");
        debug!(
            trace_id = %trace_id,
            matrix_event_id = %matrix_event_id,
            event_type = %event.event_type,
            chat_id = %event.room_id,
            "Processing Matrix event"
        );

        if let Some(ref event_id) = event.event_id {
            if self.event_store.is_event_processed(event_id).await? {
                global_metrics().record_trace_event("matrix_in", "duplicate");
                debug!(
                    trace_id = %trace_id,
                    matrix_event_id = %event_id,
                    "Skipping already processed Matrix event"
                );
                return Ok(());
            }
        }

        match event.event_type.as_str() {
            "m.room.message" | "m.sticker" => {
                println!("[Matrix Event] 📨 Type: {}", event.event_type);
                println!("[Matrix Event]   Event ID: {:?}", event.event_id);
                println!("[Matrix Event]   Room ID: {:?}", event.room_id);
                println!("[Matrix Event]   Sender: {:?}", event.sender);
                if let Some(content) = &event.content {
                    if let Some(msgtype) = content.get("msgtype").and_then(|v| v.as_str()) {
                        println!("[Matrix Event]   Message Type: {}", msgtype);
                    }
                    if let Some(body) = content.get("body").and_then(|v| v.as_str()) {
                        let preview = if body.len() > 50 { &body[..50] } else { body };
                        println!("[Matrix Event]   Body: {}...", preview);
                    }
                }
                self.handle_message_event(&event).await?;
            }
            "m.room.member" => {
                println!("[Matrix Event] 👥 Type: m.room.member");
                println!("[Matrix Event]   Event ID: {:?}", event.event_id);
                println!("[Matrix Event]   Room ID: {:?}", event.room_id);
                println!("[Matrix Event]   Sender: {:?}", event.sender);
                if let Some(content) = &event.content {
                    if let Some(membership) = content.get("membership").and_then(|v| v.as_str()) {
                        println!("[Matrix Event]   Membership: {}", membership);
                    }
                    if let Some(displayname) = content.get("displayname").and_then(|v| v.as_str()) {
                        println!("[Matrix Event]   Display Name: {}", displayname);
                    }
                }
                self.handle_member_event(&event).await?;
            }
            "m.room.redaction" => {
                println!("[Matrix Event] 🗑️  Type: m.room.redaction");
                println!("[Matrix Event]   Event ID: {:?}", event.event_id);
                println!("[Matrix Event]   Room ID: {:?}", event.room_id);
                println!("[Matrix Event]   Sender: {:?}", event.sender);
                if let Some(content) = &event.content {
                    if let Some(redacts) = content.get("redacts") {
                        println!("[Matrix Event]   Redacts: {}", redacts);
                    }
                }
                self.handle_redaction_event(&event).await?;
            }
            "m.reaction" => {
                println!("[Matrix Event] 👍 Type: m.reaction");
                println!("[Matrix Event]   Event ID: {:?}", event.event_id);
                println!("[Matrix Event]   Room ID: {:?}", event.room_id);
                println!("[Matrix Event]   Sender: {:?}", event.sender);
                if let Some(content) = &event.content {
                    if let Some(relates_to) = content.get("m.relates_to") {
                        println!("[Matrix Event]   Relates To: {}", relates_to);
                    }
                }
                self.handle_reaction_event(&event).await?;
            }
            _ => {
                println!("[Matrix Event] ❓ Type: {} (ignored)", event.event_type);
                println!("[Matrix Event]   Event ID: {:?}", event.event_id);
                println!("[Matrix Event]   Room ID: {:?}", event.room_id);
                println!("[Matrix Event]   Sender: {:?}", event.sender);
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

        global_metrics().record_trace_event("matrix_in", "processed");

        Ok(())
    }

    async fn handle_message_event(&self, event: &MatrixEvent) -> anyhow::Result<()> {
        let trace_id = build_trace_id("mx_to_feishu", event.event_id.as_deref(), None);
        if self.is_bridge_bot_sender(&event.sender) {
            global_metrics().record_trace_event("mx_to_feishu", "ignored_bridge_bot_sender");
            debug!(
                trace_id = %trace_id,
                matrix_event_id = %event.event_id.as_deref().unwrap_or("unknown"),
                sender = %event.sender,
                "Ignoring Matrix message sent by bridge bot to prevent self-loop"
            );
            return Ok(());
        }

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
            global_metrics().record_trace_event("mx_to_feishu", "blocked_msgtype");
            warn!(
                trace_id = %trace_id,
                matrix_event_id = %event.event_id.as_deref().unwrap_or("unknown"),
                chat_id = %event.room_id,
                msgtype = %matrix_msgtype,
                "Blocking Matrix event due to blocked msgtype policy"
            );
            return Ok(());
        }
        if !self.rate_limiter.allow(&event.room_id) {
            global_metrics().record_policy_block("rate_limited");
            global_metrics().record_trace_event("mx_to_feishu", "blocked_rate_limit");
            warn!(
                trace_id = %trace_id,
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
            global_metrics().record_trace_event("mx_to_feishu", "unmapped_room");
            return Ok(());
        };

        let Some(inbound) = parse_matrix_inbound(event) else {
            debug!("Failed to parse message event");
            global_metrics().record_trace_event("mx_to_feishu", "parse_skipped");
            return Ok(());
        };

        let mut outbound = self.message_flow.matrix_to_feishu(&inbound);
        self.prepend_matrix_sender_marker(event, &mut outbound).await;
        if self.config.bridge.max_text_length > 0 {
            let (truncated, changed) =
                truncate_text(&outbound.content, self.config.bridge.max_text_length);
            if changed {
                global_metrics().record_degraded_event("text_truncated");
                global_metrics().record_trace_event("mx_to_feishu", "text_truncated");
                warn!(
                    trace_id = %trace_id,
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
            global_metrics().record_trace_event("mx_to_feishu", "deduped");
            debug!(
                trace_id = %trace_id,
                matrix_event_id = %event.event_id.as_deref().unwrap_or("unknown"),
                dedupe_hash = %content_hash,
                feishu_message_id = %existing.feishu_message_id,
                "Skipping duplicate outbound Matrix message based on content hash"
            );
            return Ok(());
        }

        if let Some(event_id) = &outbound.edit_of {
            global_metrics().record_trace_event("mx_to_feishu", "edit");
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
                    global_metrics().record_trace_event("mx_to_feishu", "failed");
                    return Err(err);
                }

                global_metrics().record_degraded_event("delivery_failure");
                global_metrics().record_trace_event("mx_to_feishu", "degraded");
                warn!(
                    trace_id = %trace_id,
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
            let link_trace_id = build_trace_id(
                "mx_to_feishu",
                Some(event_id.as_str()),
                Some(&feishu_message.message_id),
            );
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
                    trace_id = %link_trace_id,
                    matrix_event_id = %event_id,
                    feishu_message_id = %link.feishu_message_id,
                    chat_id = %event.room_id,
                    error = %err,
                    "Failed to persist Matrix->Feishu message mapping"
                );
            }

            global_metrics().record_trace_event("mx_to_feishu", "success");
            info!(
                trace_id = %link_trace_id,
                matrix_event_id = %event_id,
                feishu_message_id = %link.feishu_message_id,
                chat_id = %event.room_id,
                feishu_chat_id = %mapping.feishu_chat_id,
                "Bridged Matrix message to Feishu"
            );
            return Ok(());
        }
        global_metrics().record_trace_event("mx_to_feishu", "attachments_only_or_empty");

        Ok(())
    }

    async fn prepend_matrix_sender_marker(
        &self,
        event: &MatrixEvent,
        outbound: &mut OutboundFeishuMessage,
    ) {
        if outbound.content.trim().is_empty() {
            return;
        }

        let (marker, has_feishu_at) = self.build_matrix_sender_marker(&event.sender).await;
        outbound.content = format!("{}{}", marker, outbound.content);

        // Feishu text supports <at user_id="...">...</at> directly.
        // Keep message type as text when marker contains explicit Feishu at-tag.
        if has_feishu_at && outbound.msg_type == "post" {
            outbound.msg_type = "text".to_string();
        }
    }

    async fn build_matrix_sender_marker(&self, matrix_sender: &str) -> (String, bool) {
        let mut display_name = extract_matrix_sender_name(matrix_sender);
        let mut feishu_user_id = None::<String>;

        match self.user_store.get_user_by_matrix_id(matrix_sender).await {
            Ok(Some(mapping)) => {
                if let Some(name) = mapping
                    .feishu_username
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    display_name = name.to_string();
                }
                if !mapping.feishu_user_id.trim().is_empty() {
                    feishu_user_id = Some(mapping.feishu_user_id);
                }
            }
            Ok(None) => {}
            Err(err) => warn!(
                sender = %matrix_sender,
                error = %err,
                "Failed to resolve Matrix sender mapping for Feishu marker"
            ),
        }

        let safe_display_name = sanitize_marker_fragment(&display_name);
        if let Some(feishu_user_id) = feishu_user_id {
            let safe_feishu_user_id = sanitize_marker_fragment(&feishu_user_id);
            let marker = format!(
                "[Matrix: {}] <at user_id=\"{}\">{}</at> ",
                safe_display_name, safe_feishu_user_id, safe_display_name
            );
            return (marker, true);
        }

        (format!("[Matrix: {}] ", safe_display_name), false)
    }

    async fn send_failure_degrade_notice(
        &self,
        mapping: &RoomMapping,
        event: &MatrixEvent,
        err: &anyhow::Error,
    ) {
        let trace_id = build_trace_id("mx_to_feishu", event.event_id.as_deref(), None);
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
            let error_chain = format!("{:#}", notice_err);
            warn!(
                trace_id = %trace_id,
                matrix_event_id = %event.event_id.as_deref().unwrap_or("unknown"),
                chat_id = %event.room_id,
                feishu_chat_id = %mapping.feishu_chat_id,
                error = %notice_err,
                error_chain = %error_chain,
                "Failed to send failure degrade notice to Feishu"
            );
        }
    }

    async fn handle_command(&self, event: &MatrixEvent, body: &str) -> anyhow::Result<()> {
        println!("[Matrix Command] 🎹 Command detected");
        println!("[Matrix Command]   Event ID: {:?}", event.event_id);
        println!("[Matrix Command]   Room ID: {:?}", event.room_id);
        println!("[Matrix Command]   Sender: {:?}", event.sender);
        println!("[Matrix Command]   Command: {}", body);

        let room_mapping = self
            .room_store
            .get_room_by_matrix_id(&event.room_id)
            .await?;

        let is_bridged = room_mapping.is_some();
        println!("[Matrix Command]   Is Bridged: {}", is_bridged);

        let outcome = self.command_handler.handle(body, is_bridged, |_| true);

        match outcome {
            MatrixCommandOutcome::Ignored => {
                println!("[Matrix Command]   Outcome: Ignored");
            }
            MatrixCommandOutcome::Reply(reply) => {
                println!("[Matrix Command]   Outcome: Reply - {}", reply);
                if let Err(err) = self.send_matrix_command_reply(&event.room_id, &reply).await {
                    warn!(
                        room_id = %event.room_id,
                        error = %err,
                        "Failed to send Matrix command reply"
                    );
                }
            }
            MatrixCommandOutcome::BridgeRequested { feishu_chat_id } => {
                println!(
                    "[Matrix Command]   Outcome: Bridge Request to {}",
                    feishu_chat_id
                );
                let reply = self
                    .handle_bridge_request(&event.room_id, &event.sender, &feishu_chat_id)
                    .await?;
                if let Err(err) = self.send_matrix_command_reply(&event.room_id, &reply).await {
                    warn!(
                        room_id = %event.room_id,
                        error = %err,
                        "Failed to send Matrix command reply"
                    );
                }
            }
            MatrixCommandOutcome::UnbridgeRequested => {
                println!("[Matrix Command]   Outcome: Unbridge Request");
                let reply = self.handle_unbridge_request(&event.room_id).await?;
                if let Err(err) = self.send_matrix_command_reply(&event.room_id, &reply).await {
                    warn!(
                        room_id = %event.room_id,
                        error = %err,
                        "Failed to send Matrix command reply"
                    );
                }
            }
        }

        Ok(())
    }

    async fn send_matrix_command_reply(&self, room_id: &str, reply: &str) -> anyhow::Result<()> {
        let homeserver_url = self.config.bridge.homeserver_url.trim_end_matches('/');
        let access_token = &self.config.registration.as_token;
        let bot_mxid = format!(
            "@{}:{}",
            self.config.bridge.bot_username, self.config.bridge.domain
        );
        let txn_id = format!("cmd_{}", Uuid::new_v4());
        let send_url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}?user_id={}",
            homeserver_url,
            urlencoding::encode(room_id),
            txn_id,
            urlencoding::encode(&bot_mxid),
        );
        let payload = serde_json::json!({
            "msgtype": "m.notice",
            "body": reply,
        });

        let response = self
            .dispatcher
            .http_client()
            .put(&send_url)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|err| format!("Could not read error response: {}", err));
            anyhow::bail!(
                "failed to send matrix command reply to room {}: {} - {}",
                room_id,
                status,
                error_body
            );
        }

        Ok(())
    }

    async fn handle_bridge_request(
        &self,
        room_id: &str,
        sender: &str,
        feishu_chat_id: &str,
    ) -> anyhow::Result<String> {
        println!("[Bridge Action] 🔗 Bridge Request");
        println!("[Bridge Action]   Sender: {}", sender);
        println!("[Bridge Action]   Matrix Room: {}", room_id);
        println!("[Bridge Action]   Feishu Chat: {}", feishu_chat_id);

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
            println!("[Bridge Action]   ❌ Already bridged");
            warn!("Feishu chat {} is already bridged", feishu_chat_id);
            return Ok(format!(
                "Feishu chat {} is already bridged to another Matrix room.",
                feishu_chat_id
            ));
        }

        let mapping = RoomMapping::new(
            room_id.to_string(),
            feishu_chat_id.to_string(),
            Some("Bridged Room".to_string()),
        );

        self.room_store.create_room_mapping(&mapping).await?;
        info!("Created bridge mapping: {} <-> {}", room_id, feishu_chat_id);

        Ok(format!("Bridged to Feishu chat: {}", feishu_chat_id))
    }

    async fn handle_unbridge_request(&self, room_id: &str) -> anyhow::Result<String> {
        println!("[Bridge Action] 🔌 Unbridge Request");
        println!("[Bridge Action]   Matrix Room: {}", room_id);

        let Some(mapping) = self.room_store.get_room_by_matrix_id(room_id).await? else {
            println!("[Bridge Action]   ❌ Room not bridged");
            warn!("Room {} is not bridged", room_id);
            return Ok("This room is not bridged to any Feishu chat.".to_string());
        };

        println!("[Bridge Action]   Feishu Chat: {}", mapping.feishu_chat_id);
        self.room_store.delete_room_mapping(mapping.id).await?;
        println!("[Bridge Action]   ✅ Bridge removed");
        info!("Removed bridge mapping for room {}", room_id);

        Ok("Bridge removed".to_string())
    }

    async fn handle_member_event(&self, event: &MatrixEvent) -> anyhow::Result<()> {
        let Some(content) = &event.content else {
            println!("[Member Event] ⚠️  No content in event");
            return Ok(());
        };

        let membership = content
            .get("membership")
            .and_then(Value::as_str)
            .unwrap_or("leave");

        let state_key = event.state_key.as_deref().unwrap_or("");
        println!("[Member Event] 👤 Membership Change");
        println!("[Member Event]   Event ID: {:?}", event.event_id);
        println!("[Member Event]   Room ID: {:?}", event.room_id);
        println!("[Member Event]   Sender: {:?}", event.sender);
        println!("[Member Event]   Target (state_key): '{}'", state_key);
        println!(
            "[Member Event]   Target (state_key) bytes: {:?}",
            state_key.as_bytes()
        );
        println!(
            "[Member Event]   Target (state_key) len: {}",
            state_key.len()
        );
        println!("[Member Event]   Membership: {}", membership);

        debug!(
            "Member event: sender={} state_key={} membership={}",
            event.sender, state_key, membership
        );

        // Check if this is an invite for the bot
        if membership == "invite" {
            // Construct the bot's MXID
            let bot_username = &self.config.bridge.bot_username;
            let bot_domain = &self.config.bridge.domain;
            let bot_mxid = format!("@{}:{}", bot_username, bot_domain);

            println!("[Member Event] 🔍 Invite detected!");
            println!(
                "[Member Event]   Bot Username from config: '{}'",
                bot_username
            );
            println!("[Member Event]   Bot Domain from config: '{}'", bot_domain);
            println!("[Member Event]   Bot MXID constructed: '{}'", bot_mxid);
            println!("[Member Event]   Bot MXID len: {}", bot_mxid.len());
            println!("[Member Event]   State Key (invitee): '{}'", state_key);
            println!("[Member Event]   State Key len: {}", state_key.len());
            println!("[Member Event]   Exact match: {}", state_key == bot_mxid);
            println!(
                "[Member Event]   Case-insensitive match: {}",
                state_key.eq_ignore_ascii_case(&bot_mxid)
            );

            // Try both exact and case-insensitive comparison
            let is_bot_invited = state_key == bot_mxid || state_key.eq_ignore_ascii_case(&bot_mxid);
            println!(
                "[Member Event]   Is bot invited (final): {}",
                is_bot_invited
            );

            // If the bot is invited, join the room
            if is_bot_invited {
                println!("[Member Event] 🚪 Bot invited to room, attempting to join...");
                println!("[Member Event]   Room ID: {}", event.room_id);

                match self.join_matrix_room(&event.room_id).await {
                    Ok(_) => {
                        println!("[Member Event]   ✅ Successfully joined room!");
                        info!("Bot joined room {}", event.room_id);
                    }
                    Err(e) => {
                        println!("[Member Event]   ❌ Failed to join room: {}", e);
                        println!("[Member Event]   Error details: {:?}", e);
                        error!("Failed to join room {}: {:?}", event.room_id, e);
                    }
                }
            } else {
                println!("[Member Event] ⏭️  Not invited to this room, skipping auto-join");
            }
        } else if membership == "join" {
            println!("[Member Event] ✅ User joined room (no action needed)");
        } else if membership == "leave" {
            println!("[Member Event] 👋 User left room");
        } else {
            println!("[Member Event] ❓ Other membership change: {}", membership);
        }

        Ok(())
    }

    async fn handle_redaction_event(&self, event: &MatrixEvent) -> anyhow::Result<()> {
        if !self.config.bridge.bridge_matrix_redactions {
            println!("[Redaction] ⚠️  Redactions disabled in config, skipping");
            return Ok(());
        }

        let redacts_event_id = event
            .content
            .as_ref()
            .and_then(|c| c.get("redacts"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        let Some(redacts_event_id) = redacts_event_id else {
            println!("[Redaction] ⚠️  No redacts event ID found");
            return Ok(());
        };

        println!("[Redaction] 🗑️  Processing Redaction");
        println!("[Redaction]   Event ID: {:?}", event.event_id);
        println!("[Redaction]   Room ID: {:?}", event.room_id);
        println!("[Redaction]   Sender: {:?}", event.sender);
        println!("[Redaction]   Redacts Event ID: {}", redacts_event_id);

        let mapping = self
            .message_store
            .get_message_by_matrix_id(&redacts_event_id)
            .await?;

        let Some(mapping) = mapping else {
            println!("[Redaction]   ❌ No mapping found for event");
            debug!(
                "No Matrix->Feishu mapping found for redacted event {}",
                redacts_event_id
            );
            return Ok(());
        };

        println!(
            "[Redaction]   Feishu Message ID: {}",
            mapping.feishu_message_id
        );
        self.feishu_service
            .recall_message(&mapping.feishu_message_id)
            .await?;
        println!("[Redaction]   ✅ Message recalled on Feishu");

        if let Err(err) = self.message_store.delete_message_mapping(mapping.id).await {
            println!("[Redaction]   ⚠️  Failed to delete mapping: {}", err);
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
            println!("[Reaction] ⚠️  Reactions disabled in config, skipping");
            return Ok(());
        }

        let Some(content) = &event.content else {
            println!("[Reaction] ⚠️  No content in reaction event");
            return Ok(());
        };

        let relates_to = content.get("m.relates_to");
        println!("[Reaction] 👍 Processing Reaction");
        println!("[Reaction]   Event ID: {:?}", event.event_id);
        println!("[Reaction]   Room ID: {:?}", event.room_id);
        println!("[Reaction]   Sender: {:?}", event.sender);
        println!("[Reaction]   Relates To: {:?}", relates_to);
        debug!("Reaction event: {:?}", relates_to);
        Ok(())
    }

    async fn join_matrix_room(&self, room_id: &str) -> anyhow::Result<()> {
        let homeserver_url = &self.config.bridge.homeserver_url;
        let access_token = &self.config.registration.as_token;
        let bot_mxid = format!(
            "@{}:{}",
            self.config.bridge.bot_username, self.config.bridge.domain
        );

        println!("[JOIN] 🚪 Starting join room process");
        println!("[JOIN]   Homeserver URL: '{}'", homeserver_url);
        println!("[JOIN]   Room ID: '{}'", room_id);
        println!("[JOIN]   Bot MXID (as user_id): '{}'", bot_mxid);
        println!(
            "[JOIN]   Access Token (first 20 chars): '{}...'",
            &access_token[..access_token.len().min(20)]
        );

        let join_url = format!(
            "{}/_matrix/client/v3/join/{}?user_id={}",
            homeserver_url.trim_end_matches('/'),
            urlencoding::encode(room_id),
            urlencoding::encode(&bot_mxid),
        );

        println!("[JOIN]   Full Join URL: {}", join_url);

        let request_body = serde_json::json!({});
        println!("[JOIN]   Request body: {}", request_body);

        println!("[JOIN]   Sending POST request...");
        let response_result = self
            .dispatcher
            .http_client()
            .post(&join_url)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await;

        match response_result {
            Ok(response) => {
                let status = response.status();
                println!("[JOIN]   Response status: {}", status);
                println!(
                    "[JOIN]   Response status text: {}",
                    status.canonical_reason().unwrap_or("unknown")
                );

                if status.is_success() {
                    // Try to read response body
                    match response.text().await {
                        Ok(body) => {
                            println!("[JOIN]   Response body: {}", body);
                        }
                        Err(e) => {
                            println!("[JOIN]   Could not read response body: {}", e);
                        }
                    }
                    println!("[JOIN]   ✅ Join API call successful");
                    Ok(())
                } else {
                    let error_text = response
                        .text()
                        .await
                        .unwrap_or_else(|e| format!("Could not read error: {}", e));
                    if is_non_public_join_forbidden(status, &error_text) {
                        println!(
                            "[JOIN]   ⚠️  Join API returned non-public forbidden for invite flow; treating as non-fatal"
                        );
                        println!("[JOIN]   Response: {}", error_text);
                        Ok(())
                    } else {
                        println!("[JOIN]   ❌ Join API call failed");
                        println!("[JOIN]   Error response: {}", error_text);
                        Err(anyhow::anyhow!(
                            "Failed to join room: {} - {}",
                            status,
                            error_text
                        ))
                    }
                }
            }
            Err(e) => {
                println!("[JOIN]   ❌ HTTP request failed: {:?}", e);
                Err(anyhow::anyhow!("HTTP request failed: {:?}", e))
            }
        }
    }

    fn is_bridge_bot_sender(&self, sender: &str) -> bool {
        sender_matches_bridge_bot(
            sender,
            &self.config.bridge.bot_username,
            &self.config.registration.sender_localpart,
            &self.config.bridge.username_template,
            &self.config.bridge.domain,
        )
    }
}

fn sender_matches_bridge_bot(
    sender: &str,
    bot_username: &str,
    sender_localpart: &str,
    username_template: &str,
    domain: &str,
) -> bool {
    let configured_bot = format!("@{}:{}", bot_username, domain);
    if sender.eq_ignore_ascii_case(&configured_bot) {
        return true;
    }

    let sender_localpart = sender_localpart.trim();
    if sender_localpart.is_empty() {
        return false;
    }

    let registration_bot = format!("@{}:{}", sender_localpart, domain);
    if sender.eq_ignore_ascii_case(&registration_bot) {
        return true;
    }

    sender_matches_bridge_puppet(sender, username_template, domain)
}

fn sender_matches_bridge_puppet(sender: &str, username_template: &str, domain: &str) -> bool {
    let sender = sender.trim();
    let Some(stripped) = sender.strip_prefix('@') else {
        return false;
    };
    let Some((localpart, sender_domain)) = stripped.rsplit_once(':') else {
        return false;
    };
    if !sender_domain.eq_ignore_ascii_case(domain) {
        return false;
    }

    let Some((prefix, suffix)) = username_template.split_once("{{.}}") else {
        return false;
    };
    if localpart.len() < prefix.len() + suffix.len() {
        return false;
    }
    if !localpart.starts_with(prefix) || !localpart.ends_with(suffix) {
        return false;
    }

    let dynamic_start = prefix.len();
    let dynamic_end = localpart.len() - suffix.len();
    dynamic_end > dynamic_start
}

fn is_non_public_join_forbidden(status: reqwest::StatusCode, error_text: &str) -> bool {
    if status != reqwest::StatusCode::FORBIDDEN {
        return false;
    }

    if let Ok(body) = serde_json::from_str::<Value>(error_text) {
        let errcode = body
            .get("errcode")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let message = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if errcode == "M_FORBIDDEN"
            && (message.contains("not `public`") || message.contains("not public"))
        {
            return true;
        }
    }

    let lowered = error_text.to_ascii_lowercase();
    lowered.contains("m_forbidden")
        && (lowered.contains("not `public`") || lowered.contains("not public"))
}

fn extract_matrix_sender_name(matrix_sender: &str) -> String {
    let sender = matrix_sender.trim().trim_start_matches('@');
    let localpart = sender.split(':').next().unwrap_or(sender).trim();
    if localpart.is_empty() {
        "unknown".to_string()
    } else {
        localpart.to_string()
    }
}

fn sanitize_marker_fragment(value: &str) -> String {
    let compact = value
        .chars()
        .map(|ch| match ch {
            '\n' | '\r' | '\t' => ' ',
            '<' | '>' | '"' => ' ',
            _ => ch,
        })
        .collect::<String>();
    let compact = compact.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        "unknown".to_string()
    } else {
        compact
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

#[cfg(test)]
mod tests {
    use super::sender_matches_bridge_bot;

    #[test]
    fn sender_matches_configured_bot_username() {
        assert!(sender_matches_bridge_bot(
            "@_feishu_bot:localhost",
            "_feishu_bot",
            "_unused_registration_bot",
            "feishu_{{.}}",
            "localhost"
        ));
    }

    #[test]
    fn sender_matches_registration_sender_localpart() {
        assert!(sender_matches_bridge_bot(
            "@registration_bot:localhost",
            "_feishu_bot",
            "registration_bot",
            "feishu_{{.}}",
            "localhost"
        ));
    }

    #[test]
    fn sender_does_not_match_other_user() {
        assert!(!sender_matches_bridge_bot(
            "@alice:localhost",
            "_feishu_bot",
            "registration_bot",
            "feishu_{{.}}",
            "localhost"
        ));
    }

    #[test]
    fn sender_matches_namespaced_puppet_user() {
        assert!(sender_matches_bridge_bot(
            "@feishu_ou_12345:localhost",
            "_feishu_bot",
            "registration_bot",
            "feishu_{{.}}",
            "localhost"
        ));
    }

    #[test]
    fn sender_does_not_match_puppet_template_with_empty_placeholder() {
        assert!(!sender_matches_bridge_bot(
            "@feishu_:localhost",
            "_feishu_bot",
            "registration_bot",
            "feishu_{{.}}",
            "localhost"
        ));
    }
}
