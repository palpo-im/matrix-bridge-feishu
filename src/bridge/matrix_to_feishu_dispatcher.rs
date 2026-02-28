use std::sync::Arc;

use reqwest::Client;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::warn;
use uuid::Uuid;

use crate::bridge::message_flow::{MessageAttachment, OutboundFeishuMessage};
use crate::config::Config;
use crate::database::{MediaCacheEntry, MediaStore, MessageStore, RoomMapping};
use crate::feishu::{FeishuMessageSendData, FeishuService};

pub struct MatrixToFeishuDispatcher {
    config: Arc<Config>,
    feishu_service: Arc<FeishuService>,
    message_store: Arc<dyn MessageStore>,
    media_store: Arc<dyn MediaStore>,
    http_client: Client,
}

impl MatrixToFeishuDispatcher {
    pub fn new(
        config: Arc<Config>,
        feishu_service: Arc<FeishuService>,
        message_store: Arc<dyn MessageStore>,
        media_store: Arc<dyn MediaStore>,
    ) -> Self {
        Self {
            config,
            feishu_service,
            message_store,
            media_store,
            http_client: Client::new(),
        }
    }

    pub async fn send_outbound_message(
        &self,
        mapping: &RoomMapping,
        outbound: &OutboundFeishuMessage,
        delivery_uuid: Option<String>,
    ) -> anyhow::Result<Option<FeishuMessageSendData>> {
        if outbound.content.trim().is_empty() {
            return Ok(None);
        }

        let (msg_type, content) =
            build_feishu_content_payload(&outbound.msg_type, &outbound.content)?;
        let reply_in_thread = mapping.feishu_chat_type.eq_ignore_ascii_case("thread");

        if self.config.bridge.bridge_matrix_reply {
            if let Some(reply_to) = &outbound.reply_to {
                if let Some(reply_mapping) = self
                    .message_store
                    .get_message_by_matrix_id(reply_to)
                    .await?
                {
                    let response = self
                        .feishu_service
                        .reply_message(
                            &reply_mapping.feishu_message_id,
                            &msg_type,
                            content,
                            reply_in_thread,
                            delivery_uuid,
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
                delivery_uuid,
            )
            .await?;

        Ok(Some(response))
    }

    pub async fn handle_edit_message(
        &self,
        matrix_target_event_id: &str,
        outbound: &OutboundFeishuMessage,
    ) -> anyhow::Result<()> {
        if !self.config.bridge.bridge_matrix_edit {
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

        if msg_type != "text" && msg_type != "post" {
            msg_type = "text".to_string();
        }

        self.feishu_service
            .update_message(&target.feishu_message_id, &msg_type, content)
            .await?;

        Ok(())
    }

    pub async fn forward_attachments_to_feishu(
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
        let media_hash = sha256_hex(&bytes);

        match attachment.kind.as_str() {
            "m.image" | "m.sticker" => {
                if !self.config.bridge.allow_images {
                    anyhow::bail!("image bridging disabled");
                }
                if let Some(cached) = self
                    .media_store
                    .get_media_cache(&media_hash, "image")
                    .await?
                {
                    return self
                        .send_cached_resource(
                            feishu_chat_id,
                            "image",
                            "image_key",
                            &cached.resource_key,
                        )
                        .await;
                }
                let mime = guess_image_mime(&attachment.name);
                let image_key = self.feishu_service.upload_image(bytes, mime).await?;
                self.upsert_media_cache(&media_hash, "image", &image_key)
                    .await?;
                self.send_cached_resource(feishu_chat_id, "image", "image_key", &image_key)
                    .await
            }
            "m.audio" => {
                if !self.config.bridge.allow_audio {
                    anyhow::bail!("audio bridging disabled");
                }
                if let Some(cached) = self
                    .media_store
                    .get_media_cache(&media_hash, "audio")
                    .await?
                {
                    return self
                        .send_cached_resource(
                            feishu_chat_id,
                            "audio",
                            "file_key",
                            &cached.resource_key,
                        )
                        .await;
                }
                let file_type = guess_file_type(&attachment.name, "m.audio");
                let file_key = self
                    .feishu_service
                    .upload_file(&attachment.name, bytes, file_type)
                    .await?;
                self.upsert_media_cache(&media_hash, "audio", &file_key)
                    .await?;
                self.send_cached_resource(feishu_chat_id, "audio", "file_key", &file_key)
                    .await
            }
            "m.video" => {
                if !self.config.bridge.allow_videos {
                    anyhow::bail!("video bridging disabled");
                }
                if let Some(cached) = self
                    .media_store
                    .get_media_cache(&media_hash, "media")
                    .await?
                {
                    return self
                        .send_cached_resource(
                            feishu_chat_id,
                            "media",
                            "file_key",
                            &cached.resource_key,
                        )
                        .await;
                }
                let file_type = guess_file_type(&attachment.name, "m.video");
                let file_key = self
                    .feishu_service
                    .upload_file(&attachment.name, bytes, file_type)
                    .await?;
                self.upsert_media_cache(&media_hash, "media", &file_key)
                    .await?;
                self.send_cached_resource(feishu_chat_id, "media", "file_key", &file_key)
                    .await
            }
            _ => {
                if !self.config.bridge.allow_files {
                    anyhow::bail!("file bridging disabled");
                }
                if let Some(cached) = self
                    .media_store
                    .get_media_cache(&media_hash, "file")
                    .await?
                {
                    return self
                        .send_cached_resource(
                            feishu_chat_id,
                            "file",
                            "file_key",
                            &cached.resource_key,
                        )
                        .await;
                }
                let file_type = guess_file_type(&attachment.name, "m.file");
                let file_key = self
                    .feishu_service
                    .upload_file(&attachment.name, bytes, file_type)
                    .await?;
                self.upsert_media_cache(&media_hash, "file", &file_key)
                    .await?;
                self.send_cached_resource(feishu_chat_id, "file", "file_key", &file_key)
                    .await
            }
        }
    }

    async fn send_cached_resource(
        &self,
        feishu_chat_id: &str,
        msg_type: &str,
        key_field: &str,
        resource_key: &str,
    ) -> anyhow::Result<String> {
        let payload = json!({ key_field: resource_key });
        let response = self
            .feishu_service
            .send_message(
                "chat_id",
                feishu_chat_id,
                msg_type,
                payload,
                Some(Uuid::new_v4().to_string()),
            )
            .await?;
        Ok(response.message_id)
    }

    async fn upsert_media_cache(
        &self,
        content_hash: &str,
        media_kind: &str,
        resource_key: &str,
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now();
        let entry = MediaCacheEntry {
            id: 0,
            content_hash: content_hash.to_string(),
            media_kind: media_kind.to_string(),
            resource_key: resource_key.to_string(),
            created_at: now,
            updated_at: now,
        };
        self.media_store.upsert_media_cache(&entry).await?;
        Ok(())
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

        if self.config.bridge.max_media_size > 0 && bytes.len() > self.config.bridge.max_media_size
        {
            anyhow::bail!(
                "matrix media exceeds configured max_media_size: {} > {}",
                bytes.len(),
                self.config.bridge.max_media_size
            );
        }

        Ok(bytes)
    }
}

fn build_feishu_content_payload(msg_type: &str, body: &str) -> anyhow::Result<(String, Value)> {
    if msg_type == "post" {
        let post = crate::formatter::create_feishu_rich_text(body);
        let post_json = serde_json::from_str::<Value>(&post).unwrap_or_else(
            |_| json!({ "zh_cn": { "title": "", "content": [[{"tag": "text", "text": body}]] } }),
        );
        return Ok(("post".to_string(), post_json));
    }

    Ok(("text".to_string(), json!({ "text": body })))
}

fn guess_image_mime(name: &str) -> &'static str {
    let ext = name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
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

    let ext = name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => "pdf",
        "doc" | "docx" => "doc",
        "xls" | "xlsx" => "xls",
        "ppt" | "pptx" => "ppt",
        "mp4" => "mp4",
        _ => "stream",
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}
