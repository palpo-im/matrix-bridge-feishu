use std::sync::Arc;

use reqwest::Client;
use serde_json::{Value, json};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::bridge::command_handler::{MatrixCommandHandler, MatrixCommandOutcome};
use crate::bridge::message_flow::{
    MatrixInboundMessage, MessageAttachment, MessageFlow, OutboundFeishuMessage,
};
use crate::config::Config;
use crate::database::{
    EventStore, MessageMapping, MessageStore, ProcessedEvent, RoomMapping, RoomStore,
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
    http_client: Client,
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
            http_client: Client::new(),
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

        if let Some(event_id) = &outbound.edit_of {
            self.handle_edit_message(&mapping, event_id, &outbound).await?;
            return Ok(());
        }

        let mut primary_feishu_message = self.send_outbound_message(&mapping, &outbound).await?;

        let attachment_message_ids = self
            .forward_attachments_to_feishu(&mapping, &outbound.attachments)
            .await?;
        if primary_feishu_message.is_none() {
            primary_feishu_message = attachment_message_ids.first().cloned().map(|message_id| {
                FeishuMessageSendData {
                    message_id,
                    root_id: None,
                    parent_id: None,
                    thread_id: None,
                }
            });
        }

        if let (Some(event_id), Some(feishu_message)) = (&event.event_id, primary_feishu_message)
        {
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
            );
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

    async fn send_outbound_message(
        &self,
        mapping: &RoomMapping,
        outbound: &OutboundFeishuMessage,
    ) -> anyhow::Result<Option<FeishuMessageSendData>> {
        if outbound.content.trim().is_empty() {
            return Ok(None);
        }

        let (msg_type, content) = build_feishu_content_payload(&outbound.msg_type, &outbound.content)?;
        let uuid = Some(Uuid::new_v4().to_string());
        let reply_in_thread = mapping.feishu_chat_type.eq_ignore_ascii_case("thread");

        if self.config.bridge.bridge_matrix_reply {
            if let Some(reply_to) = &outbound.reply_to {
                if let Some(reply_mapping) = self.message_store.get_message_by_matrix_id(reply_to).await? {
                    let response = self
                        .feishu_service
                        .reply_message(
                            &reply_mapping.feishu_message_id,
                            &msg_type,
                            content,
                            reply_in_thread,
                            uuid,
                        )
                        .await?;
                    return Ok(Some(response));
                }
            }
        }

        let response = self
            .feishu_service
            .send_message(
                "chat_id",
                &mapping.feishu_chat_id,
                &msg_type,
                content,
                uuid,
            )
            .await?;

        Ok(Some(response))
    }

    async fn handle_edit_message(
        &self,
        _mapping: &RoomMapping,
        matrix_target_event_id: &str,
        outbound: &OutboundFeishuMessage,
    ) -> anyhow::Result<()> {
        if !self.config.bridge.bridge_matrix_edit {
            debug!("Skipping Matrix edit forwarding because bridge_matrix_edit=false");
            return Ok(());
        }

        let target = self
            .message_store
            .get_message_by_matrix_id(matrix_target_event_id)
            .await?;

        let Some(target) = target else {
            warn!(
                matrix_event_id = %matrix_target_event_id,
                "Matrix edit target has no Feishu mapping"
            );
            return Ok(());
        };

        let (mut msg_type, content) =
            build_feishu_content_payload(&outbound.msg_type, &outbound.content)?;

        // Feishu update currently supports text/post only.
        if msg_type != "text" && msg_type != "post" {
            msg_type = "text".to_string();
        }

        self.feishu_service
            .update_message(&target.feishu_message_id, &msg_type, content)
            .await?;

        Ok(())
    }

    async fn forward_attachments_to_feishu(
        &self,
        mapping: &RoomMapping,
        attachments: &[MessageAttachment],
    ) -> anyhow::Result<Vec<String>> {
        let mut message_ids = Vec::new();

        for attachment in attachments {
            match self
                .forward_single_attachment(&mapping.feishu_chat_id, attachment)
                .await
            {
                Ok(message_id) => message_ids.push(message_id),
                Err(err) => warn!(
                    "Failed to forward Matrix attachment {} to Feishu chat {}: {}",
                    attachment.url, mapping.feishu_chat_id, err
                ),
            }
        }

        Ok(message_ids)
    }

    async fn forward_single_attachment(
        &self,
        feishu_chat_id: &str,
        attachment: &MessageAttachment,
    ) -> anyhow::Result<String> {
        let bytes = self.download_matrix_media(&attachment.url).await?;

        match attachment.kind.as_str() {
            "m.image" | "m.sticker" => {
                if !self.config.bridge.allow_images {
                    anyhow::bail!("image bridging disabled");
                }
                let mime = guess_image_mime(&attachment.name);
                let image_key = self.feishu_service.upload_image(bytes, mime).await?;
                let response = self
                    .feishu_service
                    .send_message(
                        "chat_id",
                        feishu_chat_id,
                        "image",
                        json!({ "image_key": image_key }),
                        Some(Uuid::new_v4().to_string()),
                    )
                    .await?;
                Ok(response.message_id)
            }
            "m.audio" => {
                if !self.config.bridge.allow_audio {
                    anyhow::bail!("audio bridging disabled");
                }
                let file_type = guess_file_type(&attachment.name, "m.audio");
                let file_key = self
                    .feishu_service
                    .upload_file(&attachment.name, bytes, file_type)
                    .await?;
                let response = self
                    .feishu_service
                    .send_message(
                        "chat_id",
                        feishu_chat_id,
                        "audio",
                        json!({ "file_key": file_key }),
                        Some(Uuid::new_v4().to_string()),
                    )
                    .await?;
                Ok(response.message_id)
            }
            "m.video" => {
                if !self.config.bridge.allow_videos {
                    anyhow::bail!("video bridging disabled");
                }
                let file_type = guess_file_type(&attachment.name, "m.video");
                let file_key = self
                    .feishu_service
                    .upload_file(&attachment.name, bytes, file_type)
                    .await?;
                let response = self
                    .feishu_service
                    .send_message(
                        "chat_id",
                        feishu_chat_id,
                        "media",
                        json!({ "file_key": file_key }),
                        Some(Uuid::new_v4().to_string()),
                    )
                    .await?;
                Ok(response.message_id)
            }
            _ => {
                if !self.config.bridge.allow_files {
                    anyhow::bail!("file bridging disabled");
                }
                let file_type = guess_file_type(&attachment.name, "m.file");
                let file_key = self
                    .feishu_service
                    .upload_file(&attachment.name, bytes, file_type)
                    .await?;
                let response = self
                    .feishu_service
                    .send_message(
                        "chat_id",
                        feishu_chat_id,
                        "file",
                        json!({ "file_key": file_key }),
                        Some(Uuid::new_v4().to_string()),
                    )
                    .await?;
                Ok(response.message_id)
            }
        }
    }

    async fn download_matrix_media(&self, source_url: &str) -> anyhow::Result<Vec<u8>> {
        let url = if let Some(path) = source_url.strip_prefix("mxc://") {
            format!(
                "{}/_matrix/media/v3/download/{}",
                self.config.homeserver.address.trim_end_matches('/'),
                path
            )
        } else {
            source_url.to_string()
        };

        let response = self
            .http_client
            .get(url)
            .header(
                "Authorization",
                format!("Bearer {}", self.config.appservice.as_token),
            )
            .send()
            .await?;

        let status = response.status();
        let bytes = response.bytes().await?.to_vec();
        if !status.is_success() {
            anyhow::bail!(
                "failed to download matrix media: status={} body={}",
                status,
                String::from_utf8_lossy(&bytes)
            );
        }

        if self.config.bridge.max_media_size > 0 && bytes.len() > self.config.bridge.max_media_size {
            anyhow::bail!(
                "matrix media exceeds configured max_media_size: {} > {}",
                bytes.len(),
                self.config.bridge.max_media_size
            );
        }

        Ok(bytes)
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

fn build_feishu_content_payload(msg_type: &str, body: &str) -> anyhow::Result<(String, Value)> {
    if msg_type == "post" {
        let post = crate::formatter::create_feishu_rich_text(body);
        let post_json = serde_json::from_str::<Value>(&post)
            .unwrap_or_else(|_| json!({ "zh_cn": { "title": "", "content": [[{"tag": "text", "text": body}]] } }));
        return Ok(("post".to_string(), post_json));
    }

    Ok(("text".to_string(), json!({ "text": body })))
}

fn guess_image_mime(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        _ => "image/png",
    }
}

fn guess_file_type(name: &str, kind: &str) -> &'static str {
    if kind == "m.audio" {
        return "opus";
    }
    if kind == "m.video" {
        return "mp4";
    }

    let ext = name.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => "pdf",
        "doc" | "docx" => "doc",
        "xls" | "xlsx" => "xls",
        "ppt" | "pptx" => "ppt",
        "mp4" => "mp4",
        _ => "stream",
    }
}
