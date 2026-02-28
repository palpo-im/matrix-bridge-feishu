use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use salvo::prelude::*;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::{
    FeishuClient, FeishuMessageData, FeishuMessageSendData, FeishuRichText, FeishuUser,
};
use crate::bridge::FeishuBridge;
use crate::bridge::message::{Attachment, BridgeMessage, MessageType};
use crate::web::{ScopedTimer, global_metrics};

#[derive(Clone)]
pub struct FeishuService {
    client: Arc<Mutex<FeishuClient>>,
    listen_address: String,
    listen_secret: String,
    chat_queues: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

#[derive(Debug, Clone)]
struct RecalledMessage {
    message_id: String,
    chat_id: String,
}

#[derive(Debug, Clone)]
struct ChatMemberChangedEvent {
    chat_id: String,
    user_ids: Vec<String>,
}

#[derive(Debug, Clone)]
struct ChatUpdatedEvent {
    chat_id: String,
    chat_name: Option<String>,
    chat_mode: Option<String>,
    chat_type: Option<String>,
}

impl FeishuService {
    pub fn new(
        app_id: String,
        app_secret: String,
        listen_address: String,
        listen_secret: String,
        encrypt_key: Option<String>,
        verification_token: Option<String>,
    ) -> Self {
        Self {
            client: Arc::new(Mutex::new(FeishuClient::new(
                app_id,
                app_secret,
                encrypt_key,
                verification_token,
            ))),
            listen_address,
            listen_secret,
            chat_queues: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn start(&self, bridge: FeishuBridge) -> Result<()> {
        info!("Starting Feishu service on {}", self.listen_address);

        let webhook_router = self.create_webhook_router(bridge);
        let (hostname, port) = self.parse_listen_address();
        let acceptor = TcpListener::new(format!("{}:{}", hostname, port))
            .bind()
            .await;

        info!("Feishu webhook listening on {}:{}", hostname, port);
        Server::new(acceptor).serve(webhook_router).await;

        Ok(())
    }

    fn parse_listen_address(&self) -> (String, u16) {
        let candidate = if self.listen_address.contains("://") {
            self.listen_address.clone()
        } else {
            format!("http://{}", self.listen_address)
        };

        match url::Url::parse(&candidate) {
            Ok(url) => {
                let hostname = url.host_str().unwrap_or("0.0.0.0").to_string();
                let port = url.port().unwrap_or(8081);
                (hostname, port)
            }
            Err(_) => ("0.0.0.0".to_string(), 8081),
        }
    }

    fn create_webhook_router(&self, bridge: FeishuBridge) -> Router {
        let handler = FeishuWebhookHandler {
            service: self.clone(),
            bridge,
        };

        Router::new()
            .hoop(Logger::new())
            .push(Router::with_path("/webhook").post(handler))
            .push(Router::with_path("/health").get(feishu_health))
    }

    async fn queue_chat_task<F>(&self, chat_id: String, task: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let queue_lock = {
            let mut queues = self.chat_queues.lock().await;
            queues
                .entry(chat_id)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let queue_guard = global_metrics().begin_queue_task();

        tokio::spawn(async move {
            let _queue_guard = queue_guard;
            let _guard = queue_lock.lock().await;
            task.await;
        });
    }

    async fn handle_webhook(
        &self,
        req: &mut Request,
        res: &mut Response,
        bridge: FeishuBridge,
    ) -> Result<()> {
        let _timer = ScopedTimer::new("feishu_webhook_ack");
        debug!("Received Feishu webhook request");

        let body_bytes = req
            .payload()
            .await
            .context("failed to read webhook request body")?;
        let body_str =
            String::from_utf8(body_bytes.to_vec()).context("webhook payload is not valid UTF-8")?;

        self.verify_request_signature(req, &body_str, res).await?;
        let mut payload: Value =
            serde_json::from_str(&body_str).context("failed to parse webhook payload as JSON")?;

        if let Some(encrypt) = payload.get("encrypt").and_then(Value::as_str) {
            let decrypted = {
                let client = self.client.lock().await;
                client.decrypt_webhook_content(encrypt)?
            };
            payload = serde_json::from_str(&decrypted)
                .context("failed to parse decrypted webhook payload as JSON")?;
        }

        if payload.get("type").and_then(Value::as_str) == Some("url_verification") {
            let token = payload.get("token").and_then(Value::as_str);
            let challenge = payload
                .get("challenge")
                .and_then(Value::as_str)
                .unwrap_or_default();

            let verified = {
                let client = self.client.lock().await;
                client.verify_verification_token(token)
            };

            if !verified {
                res.status_code(StatusCode::UNAUTHORIZED);
                res.render("invalid verification token");
                return Ok(());
            }

            res.status_code(StatusCode::OK);
            res.render(json!({ "challenge": challenge }).to_string());
            return Ok(());
        }

        if res.status_code == Some(StatusCode::UNAUTHORIZED) {
            return Ok(());
        }

        self.verify_payload_token(&payload, res).await?;
        if res.status_code == Some(StatusCode::UNAUTHORIZED) {
            return Ok(());
        }

        let event_type = payload
            .pointer("/header/event_type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        if event_type.is_empty() {
            warn!("Webhook payload missing header.event_type: {}", payload);
            res.status_code(StatusCode::BAD_REQUEST);
            res.render("missing event type");
            return Ok(());
        }

        info!(event_type = %event_type, "Received Feishu event");
        global_metrics().record_inbound_event(&event_type);

        match event_type.as_str() {
            "im.message.receive_v1" | "message.receive_v1" => {
                let bridge_message = self
                    .webhook_event_to_bridge_message(&payload)
                    .context("failed to parse receive event")?;
                let chat_id = bridge_message.room_id.clone();
                let feishu_message_id = bridge_message.id.clone();
                self.queue_chat_task(chat_id.clone(), async move {
                    if let Err(err) = bridge.handle_feishu_message(bridge_message).await {
                        error!(
                            event_type = "im.message.receive_v1",
                            chat_id = %chat_id,
                            feishu_message_id = %feishu_message_id,
                            error = %err,
                            "Failed to process Feishu receive event"
                        );
                    }
                })
                .await;
                res.status_code(StatusCode::OK);
                res.render("accepted");
            }
            "im.message.recalled_v1" => {
                let recalled = self
                    .webhook_event_to_recalled_message(&payload)
                    .context("failed to parse recalled event")?;
                let message_id = recalled.message_id.clone();
                let chat_id = recalled.chat_id.clone();
                self.queue_chat_task(chat_id.clone(), async move {
                    if let Err(err) = bridge
                        .handle_feishu_message_recalled(&chat_id, &message_id)
                        .await
                    {
                        error!(
                            event_type = "im.message.recalled_v1",
                            chat_id = %chat_id,
                            feishu_message_id = %message_id,
                            error = %err,
                            "Failed to process Feishu recalled event"
                        );
                    }
                })
                .await;
                res.status_code(StatusCode::OK);
                res.render("accepted");
            }
            "im.chat.member.user.added_v1" => {
                let event = self
                    .webhook_event_to_chat_member_change(&payload)
                    .context("failed to parse member added event")?;
                let chat_id = event.chat_id.clone();
                self.queue_chat_task(chat_id.clone(), async move {
                    if let Err(err) = bridge
                        .handle_feishu_chat_member_added(&chat_id, &event.user_ids)
                        .await
                    {
                        error!(
                            event_type = "im.chat.member.user.added_v1",
                            chat_id = %chat_id,
                            error = %err,
                            "Failed to process Feishu member added event"
                        );
                    }
                })
                .await;
                res.status_code(StatusCode::OK);
                res.render("accepted");
            }
            "im.chat.member.user.deleted_v1" => {
                let event = self
                    .webhook_event_to_chat_member_change(&payload)
                    .context("failed to parse member deleted event")?;
                let chat_id = event.chat_id.clone();
                self.queue_chat_task(chat_id.clone(), async move {
                    if let Err(err) = bridge
                        .handle_feishu_chat_member_deleted(&chat_id, &event.user_ids)
                        .await
                    {
                        error!(
                            event_type = "im.chat.member.user.deleted_v1",
                            chat_id = %chat_id,
                            error = %err,
                            "Failed to process Feishu member deleted event"
                        );
                    }
                })
                .await;
                res.status_code(StatusCode::OK);
                res.render("accepted");
            }
            "im.chat.updated_v1" => {
                let event = self
                    .webhook_event_to_chat_updated(&payload)
                    .context("failed to parse chat updated event")?;
                let chat_id = event.chat_id.clone();
                self.queue_chat_task(chat_id.clone(), async move {
                    if let Err(err) = bridge
                        .handle_feishu_chat_updated(
                            &chat_id,
                            event.chat_name,
                            event.chat_mode,
                            event.chat_type,
                        )
                        .await
                    {
                        error!(
                            event_type = "im.chat.updated_v1",
                            chat_id = %chat_id,
                            error = %err,
                            "Failed to process Feishu chat updated event"
                        );
                    }
                })
                .await;
                res.status_code(StatusCode::OK);
                res.render("accepted");
            }
            "im.chat.disbanded_v1" => {
                let chat_id = self
                    .webhook_event_to_chat_disbanded(&payload)
                    .context("failed to parse chat disbanded event")?;
                self.queue_chat_task(chat_id.clone(), async move {
                    if let Err(err) = bridge.handle_feishu_chat_disbanded(&chat_id).await {
                        error!(
                            event_type = "im.chat.disbanded_v1",
                            chat_id = %chat_id,
                            error = %err,
                            "Failed to process Feishu chat disbanded event"
                        );
                    }
                })
                .await;
                res.status_code(StatusCode::OK);
                res.render("accepted");
            }
            _ => {
                debug!(event_type = %event_type, "Ignoring unsupported Feishu event type");
                res.status_code(StatusCode::OK);
                res.render("ignored");
            }
        }

        Ok(())
    }

    async fn verify_request_signature(
        &self,
        req: &Request,
        body: &str,
        res: &mut Response,
    ) -> Result<()> {
        let signature_key = {
            let client = self.client.lock().await;
            client
                .callback_signature_key(&self.listen_secret)
                .map(ToOwned::to_owned)
        };

        let Some(signature_key) = signature_key else {
            return Ok(());
        };

        let timestamp = req.header::<String>("X-Lark-Request-Timestamp");
        let nonce = req.header::<String>("X-Lark-Request-Nonce");
        let signature = req.header::<String>("X-Lark-Signature");

        let (timestamp, nonce, signature) = match (timestamp, nonce, signature) {
            (Some(timestamp), Some(nonce), Some(signature)) => (timestamp, nonce, signature),
            _ => {
                warn!("Webhook signature headers missing while listen_secret is configured");
                res.status_code(StatusCode::UNAUTHORIZED);
                res.render("missing signature");
                return Ok(());
            }
        };

        let valid_signature = {
            let client = self.client.lock().await;
            client.verify_webhook_signature(&signature_key, &timestamp, &nonce, body, &signature)?
        };

        if !valid_signature {
            warn!("Webhook signature verification failed");
            res.status_code(StatusCode::UNAUTHORIZED);
            res.render("invalid signature");
        }

        Ok(())
    }

    async fn verify_payload_token(&self, payload: &Value, res: &mut Response) -> Result<()> {
        let token = payload
            .pointer("/header/token")
            .and_then(Value::as_str)
            .or_else(|| payload.get("token").and_then(Value::as_str));

        let verified = {
            let client = self.client.lock().await;
            client.verify_verification_token(token)
        };

        if !verified {
            warn!("Webhook verification token check failed");
            res.status_code(StatusCode::UNAUTHORIZED);
            res.render("invalid verification token");
        }

        Ok(())
    }

    fn webhook_event_to_bridge_message(&self, payload: &Value) -> Result<BridgeMessage> {
        let event = payload
            .get("event")
            .ok_or_else(|| anyhow::anyhow!("missing event object"))?;
        let message = event
            .get("message")
            .ok_or_else(|| anyhow::anyhow!("missing event.message object"))?;

        let message_id = message
            .get("message_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing message.message_id"))?
            .to_string();
        let chat_id = message
            .get("chat_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing message.chat_id"))?
            .to_string();
        let msg_type = message
            .get("msg_type")
            .and_then(Value::as_str)
            .unwrap_or("text")
            .to_string();
        let create_time = message
            .get("create_time")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let parent_id = message
            .get("parent_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let thread_id = message
            .get("thread_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let root_id = message
            .get("root_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);

        let sender = event
            .pointer("/sender/sender_id/user_id")
            .and_then(Value::as_str)
            .or_else(|| event.pointer("/sender/sender_id/open_id").and_then(Value::as_str))
            .unwrap_or("unknown")
            .to_string();

        let parsed_content = parse_feishu_message_content(
            &message
                .get("content")
                .cloned()
                .unwrap_or_else(|| Value::String(String::new())),
        );

        let mut attachments = Vec::new();
        let (content, message_type) = match msg_type.as_str() {
            "text" => (
                parsed_content
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                MessageType::Text,
            ),
            "post" | "rich_text" => (
                extract_text_from_post_content(&parsed_content),
                MessageType::RichText,
            ),
            "image" => {
                if let Some(image_key) = parsed_content.get("image_key").and_then(Value::as_str) {
                    attachments.push(build_attachment("image", image_key, "image/*"));
                }
                (String::new(), MessageType::Image)
            }
            "file" => {
                if let Some(file_key) = parsed_content.get("file_key").and_then(Value::as_str) {
                    attachments.push(build_attachment(
                        "file",
                        file_key,
                        "application/octet-stream",
                    ));
                }
                (
                    parsed_content
                        .get("file_name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    MessageType::File,
                )
            }
            "audio" => {
                if let Some(file_key) = parsed_content.get("file_key").and_then(Value::as_str) {
                    attachments.push(build_attachment("audio", file_key, "audio/*"));
                }
                (String::new(), MessageType::Audio)
            }
            "media" | "video" => {
                if let Some(file_key) = parsed_content.get("file_key").and_then(Value::as_str) {
                    attachments.push(build_attachment("video", file_key, "video/*"));
                }
                (
                    parsed_content
                        .get("file_name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    MessageType::Video,
                )
            }
            "sticker" => {
                if let Some(file_key) = parsed_content.get("file_key").and_then(Value::as_str) {
                    attachments.push(build_attachment("sticker", file_key, "image/*"));
                }
                (String::new(), MessageType::Image)
            }
            "interactive" | "card" => (
                extract_text_from_card_content(&parsed_content),
                MessageType::Card,
            ),
            _ => (
                parsed_content
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                MessageType::Text,
            ),
        };

        Ok(BridgeMessage {
            id: message_id,
            sender,
            room_id: chat_id,
            content,
            msg_type: message_type,
            timestamp: parse_event_timestamp(create_time),
            attachments,
            thread_id,
            root_id,
            parent_id,
        })
    }

    fn webhook_event_to_recalled_message(&self, payload: &Value) -> Result<RecalledMessage> {
        let event = payload
            .get("event")
            .ok_or_else(|| anyhow::anyhow!("missing event object"))?;

        let message_id = event
            .get("message_id")
            .and_then(Value::as_str)
            .or_else(|| {
                event
                    .get("message")
                    .and_then(|m| m.get("message_id"))
                    .and_then(Value::as_str)
            })
            .ok_or_else(|| anyhow::anyhow!("missing recalled message_id"))?
            .to_string();

        let chat_id = event
            .get("chat_id")
            .and_then(Value::as_str)
            .or_else(|| {
                event
                    .get("message")
                    .and_then(|m| m.get("chat_id"))
                    .and_then(Value::as_str)
            })
            .ok_or_else(|| anyhow::anyhow!("missing recalled chat_id"))?
            .to_string();

        Ok(RecalledMessage {
            message_id,
            chat_id,
        })
    }

    fn webhook_event_to_chat_member_change(&self, payload: &Value) -> Result<ChatMemberChangedEvent> {
        let event = payload
            .get("event")
            .ok_or_else(|| anyhow::anyhow!("missing event object"))?;

        let chat_id = pick_first_string(
            event,
            &[
                "/chat_id",
                "/chat/chat_id",
                "/open_chat_id",
                "/chat/open_chat_id",
            ],
        )
        .ok_or_else(|| anyhow::anyhow!("missing chat_id in member change event"))?;

        let mut user_ids = Vec::new();
        collect_user_ids(event.pointer("/users"), &mut user_ids);
        collect_user_ids(event.pointer("/added_users"), &mut user_ids);
        collect_user_ids(event.pointer("/deleted_users"), &mut user_ids);
        if let Some(single_user) = pick_first_string(
            event,
            &[
                "/operator_id/user_id",
                "/operator_id/open_id",
                "/user_id/user_id",
                "/user_id/open_id",
            ],
        ) {
            user_ids.push(single_user);
        }
        user_ids.sort();
        user_ids.dedup();

        Ok(ChatMemberChangedEvent { chat_id, user_ids })
    }

    fn webhook_event_to_chat_updated(&self, payload: &Value) -> Result<ChatUpdatedEvent> {
        let event = payload
            .get("event")
            .ok_or_else(|| anyhow::anyhow!("missing event object"))?;

        let chat_id = pick_first_string(
            event,
            &[
                "/chat_id",
                "/chat/chat_id",
                "/open_chat_id",
                "/chat/open_chat_id",
            ],
        )
        .ok_or_else(|| anyhow::anyhow!("missing chat_id in chat updated event"))?;

        let chat_name = pick_first_string(event, &["/name", "/chat/name", "/chat_info/name"]);
        let chat_mode = pick_first_string(event, &["/chat_mode", "/chat/chat_mode"]);
        let chat_type = pick_first_string(event, &["/chat_type", "/chat/chat_type"]);

        Ok(ChatUpdatedEvent {
            chat_id,
            chat_name,
            chat_mode,
            chat_type,
        })
    }

    fn webhook_event_to_chat_disbanded(&self, payload: &Value) -> Result<String> {
        let event = payload
            .get("event")
            .ok_or_else(|| anyhow::anyhow!("missing event object"))?;

        pick_first_string(
            event,
            &[
                "/chat_id",
                "/chat/chat_id",
                "/open_chat_id",
                "/chat/open_chat_id",
            ],
        )
        .ok_or_else(|| anyhow::anyhow!("missing chat_id in chat disbanded event"))
    }

    pub async fn get_user(&self, user_id: &str) -> Result<FeishuUser> {
        let api = "contact.v3.users.get";
        global_metrics().record_outbound_call(api);
        let mut client = self.client.lock().await;
        let result = client.get_user(user_id).await;
        if let Err(err) = &result {
            global_metrics().record_outbound_failure(api, &extract_error_code(err));
        }
        result
    }

    pub async fn send_message(
        &self,
        receive_id_type: &str,
        receive_id: &str,
        msg_type: &str,
        content: Value,
        uuid: Option<String>,
    ) -> Result<FeishuMessageSendData> {
        let api = "im.v1.messages.create";
        global_metrics().record_outbound_call(api);
        let mut client = self.client.lock().await;
        let result = client
            .send_message(receive_id_type, receive_id, msg_type, content, uuid)
            .await;
        if let Err(err) = &result {
            global_metrics().record_outbound_failure(api, &extract_error_code(err));
        }
        result
    }

    pub async fn send_text_message(&self, chat_id: &str, content: &str) -> Result<String> {
        let api = "im.v1.messages.create";
        global_metrics().record_outbound_call(api);
        let mut client = self.client.lock().await;
        let result = client.send_text_message(chat_id, content).await;
        if let Err(err) = &result {
            global_metrics().record_outbound_failure(api, &extract_error_code(err));
        }
        result
    }

    pub async fn send_rich_text_message(
        &self,
        chat_id: &str,
        rich_text: &FeishuRichText,
    ) -> Result<String> {
        let api = "im.v1.messages.create";
        global_metrics().record_outbound_call(api);
        let mut client = self.client.lock().await;
        let result = client.send_rich_text_message(chat_id, rich_text).await;
        if let Err(err) = &result {
            global_metrics().record_outbound_failure(api, &extract_error_code(err));
        }
        result
    }

    pub async fn reply_message(
        &self,
        message_id: &str,
        msg_type: &str,
        content: Value,
        reply_in_thread: bool,
        uuid: Option<String>,
    ) -> Result<FeishuMessageSendData> {
        let api = "im.v1.messages.reply";
        global_metrics().record_outbound_call(api);
        let mut client = self.client.lock().await;
        let result = client
            .reply_message(message_id, msg_type, content, reply_in_thread, uuid)
            .await;
        if let Err(err) = &result {
            global_metrics().record_outbound_failure(api, &extract_error_code(err));
        }
        result
    }

    pub async fn update_message(
        &self,
        message_id: &str,
        msg_type: &str,
        content: Value,
    ) -> Result<FeishuMessageSendData> {
        let api = "im.v1.messages.update";
        global_metrics().record_outbound_call(api);
        let mut client = self.client.lock().await;
        let result = client.update_message(message_id, msg_type, content).await;
        if let Err(err) = &result {
            global_metrics().record_outbound_failure(api, &extract_error_code(err));
        }
        result
    }

    pub async fn recall_message(&self, message_id: &str) -> Result<()> {
        let api = "im.v1.messages.delete";
        global_metrics().record_outbound_call(api);
        let mut client = self.client.lock().await;
        let result = client.recall_message(message_id).await;
        if let Err(err) = &result {
            global_metrics().record_outbound_failure(api, &extract_error_code(err));
        }
        result
    }

    pub async fn get_message(&self, message_id: &str) -> Result<Option<FeishuMessageData>> {
        let api = "im.v1.messages.get";
        global_metrics().record_outbound_call(api);
        let mut client = self.client.lock().await;
        let result = client.get_message(message_id).await;
        if let Err(err) = &result {
            global_metrics().record_outbound_failure(api, &extract_error_code(err));
        }
        result
    }

    pub async fn get_message_resource(
        &self,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
    ) -> Result<Vec<u8>> {
        let api = "im.v1.messages.resources.get";
        global_metrics().record_outbound_call(api);
        let mut client = self.client.lock().await;
        let result = client
            .get_message_resource(message_id, file_key, resource_type)
            .await;
        if let Err(err) = &result {
            global_metrics().record_outbound_failure(api, &extract_error_code(err));
        }
        result
    }

    pub async fn upload_image(&self, image_data: Vec<u8>, image_type: &str) -> Result<String> {
        let api = "im.v1.images.create";
        global_metrics().record_outbound_call(api);
        let mut client = self.client.lock().await;
        let result = client.upload_image(image_data, image_type, "message").await;
        if let Err(err) = &result {
            global_metrics().record_outbound_failure(api, &extract_error_code(err));
        }
        result
    }

    pub async fn upload_file(
        &self,
        file_name: &str,
        file_data: Vec<u8>,
        file_type: &str,
    ) -> Result<String> {
        let api = "im.v1.files.create";
        global_metrics().record_outbound_call(api);
        let mut client = self.client.lock().await;
        let result = client.upload_file(file_name, file_data, file_type).await;
        if let Err(err) = &result {
            global_metrics().record_outbound_failure(api, &extract_error_code(err));
        }
        result
    }
}

fn parse_event_timestamp(value: &str) -> chrono::DateTime<Utc> {
    if let Ok(epoch) = value.parse::<i64>() {
        if epoch > 10_000_000_000 {
            return Utc
                .timestamp_millis_opt(epoch)
                .single()
                .unwrap_or_else(Utc::now);
        }
        return Utc
            .timestamp_opt(epoch, 0)
            .single()
            .unwrap_or_else(Utc::now);
    }

    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(value) {
        return parsed.with_timezone(&Utc);
    }

    Utc::now()
}

fn parse_feishu_message_content(raw_content: &Value) -> Value {
    match raw_content {
        Value::Object(_) => raw_content.clone(),
        Value::String(content) => {
            serde_json::from_str::<Value>(content).unwrap_or_else(|_| json!({ "text": content }))
        }
        _ => Value::Null,
    }
}

fn extract_text_from_post_content(content: &Value) -> String {
    let mut parts = Vec::new();

    if let Some(locale_obj) = content.as_object() {
        for value in locale_obj.values() {
            if let Some(rows) = value.get("content").and_then(Value::as_array) {
                for row in rows {
                    if let Some(items) = row.as_array() {
                        for item in items {
                            let tag = item.get("tag").and_then(Value::as_str).unwrap_or("");
                            match tag {
                                "text" => {
                                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                                        parts.push(text.to_string());
                                    }
                                }
                                "a" => {
                                    let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                                    let href = item.get("href").and_then(Value::as_str).unwrap_or("");
                                    if text.is_empty() {
                                        parts.push(href.to_string());
                                    } else {
                                        parts.push(format!("{} ({})", text, href));
                                    }
                                }
                                "at" => {
                                    let text = item
                                        .get("user_name")
                                        .and_then(Value::as_str)
                                        .or_else(|| item.get("user_id").and_then(Value::as_str))
                                        .unwrap_or("user");
                                    parts.push(format!("@{}", text));
                                }
                                "img" => {
                                    parts.push("[Image]".to_string());
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }

    if parts.is_empty() {
        content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    } else {
        parts.join("")
    }
}

fn extract_text_from_card_content(content: &Value) -> String {
    let header = content
        .pointer("/header/title/content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let body = content.to_string();
    if header.is_empty() {
        body
    } else {
        format!("{}\n{}", header, body)
    }
}

fn build_attachment(kind: &str, key: &str, mime_type: &str) -> Attachment {
    Attachment {
        id: Uuid::new_v4().to_string(),
        name: format!("{}_{}", kind, key),
        url: format!("feishu://{}/{}", kind, key),
        size: 0,
        mime_type: mime_type.to_string(),
    }
}

fn extract_error_code(err: &anyhow::Error) -> String {
    let message = err.to_string();
    if let Some(idx) = message.find("code=") {
        let suffix = &message[idx + 5..];
        let code: String = suffix.chars().take_while(|ch| ch.is_ascii_digit()).collect();
        if !code.is_empty() {
            return code;
        }
    }
    "unknown".to_string()
}

fn pick_first_string(root: &Value, pointers: &[&str]) -> Option<String> {
    pointers.iter().find_map(|pointer| {
        root.pointer(pointer)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn collect_user_ids(source: Option<&Value>, output: &mut Vec<String>) {
    let Some(value) = source else {
        return;
    };

    if let Some(entries) = value.as_array() {
        for entry in entries {
            if let Some(id) = pick_first_string(
                entry,
                &["/user_id", "/open_id", "/id/user_id", "/id/open_id"],
            ) {
                output.push(id);
            }
        }
        return;
    }

    if let Some(id) = pick_first_string(value, &["/user_id", "/open_id"]) {
        output.push(id);
    }
}

struct FeishuWebhookHandler {
    service: FeishuService,
    bridge: FeishuBridge,
}

#[async_trait]
impl Handler for FeishuWebhookHandler {
    async fn handle(
        &self,
        req: &mut Request,
        _depot: &mut Depot,
        res: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        if let Err(err) = self
            .service
            .handle_webhook(req, res, self.bridge.clone())
            .await
        {
            error!("Feishu webhook processing failed: {}", err);
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render("internal error");
        }
    }
}

#[handler]
async fn feishu_health(res: &mut Response) {
    res.status_code(StatusCode::OK);
    res.render("OK");
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{FeishuService, parse_feishu_message_content};

    fn build_service() -> FeishuService {
        FeishuService::new(
            "app_id".to_string(),
            "app_secret".to_string(),
            "127.0.0.1:8081".to_string(),
            "listen_secret".to_string(),
            Some("encrypt_key".to_string()),
            Some("verification_token".to_string()),
        )
    }

    #[test]
    fn parse_feishu_message_content_accepts_json_string() {
        let parsed = parse_feishu_message_content(&json!("{\"text\":\"hello\"}"));
        assert_eq!(
            parsed.get("text").and_then(|value| value.as_str()),
            Some("hello")
        );
    }

    #[test]
    fn parse_feishu_message_content_falls_back_to_plain_text() {
        let parsed = parse_feishu_message_content(&json!("hello"));
        assert_eq!(
            parsed.get("text").and_then(|value| value.as_str()),
            Some("hello")
        );
    }

    #[test]
    fn parse_chat_member_change_event_extracts_chat_and_users() {
        let service = build_service();
        let payload = json!({
            "event": {
                "chat_id": "oc_chat",
                "users": [
                    {"user_id": "u_1"},
                    {"open_id": "ou_2"}
                ]
            }
        });

        let parsed = service
            .webhook_event_to_chat_member_change(&payload)
            .expect("member change should parse");
        assert_eq!(parsed.chat_id, "oc_chat");
        assert_eq!(parsed.user_ids, vec!["ou_2".to_string(), "u_1".to_string()]);
    }

    #[test]
    fn parse_chat_updated_event_extracts_mode() {
        let service = build_service();
        let payload = json!({
            "event": {
                "chat": {
                    "chat_id": "oc_chat",
                    "name": "Bridge Chat",
                    "chat_mode": "thread",
                    "chat_type": "group"
                }
            }
        });

        let parsed = service
            .webhook_event_to_chat_updated(&payload)
            .expect("chat updated should parse");
        assert_eq!(parsed.chat_id, "oc_chat");
        assert_eq!(parsed.chat_name.as_deref(), Some("Bridge Chat"));
        assert_eq!(parsed.chat_mode.as_deref(), Some("thread"));
        assert_eq!(parsed.chat_type.as_deref(), Some("group"));
    }

    #[test]
    fn parse_receive_event_image_extracts_attachment_and_thread_fields() {
        let service = build_service();
        let payload = json!({
            "event": {
                "sender": {
                    "sender_id": {
                        "user_id": "ou_sender"
                    }
                },
                "message": {
                    "message_id": "om_1",
                    "chat_id": "oc_chat",
                    "msg_type": "image",
                    "parent_id": "om_parent",
                    "thread_id": "omt_thread",
                    "root_id": "om_root",
                    "create_time": "1700000000",
                    "content": "{\"image_key\":\"img_key_1\"}"
                }
            }
        });

        let parsed = service
            .webhook_event_to_bridge_message(&payload)
            .expect("receive event should parse");
        assert_eq!(parsed.id, "om_1");
        assert_eq!(parsed.room_id, "oc_chat");
        assert_eq!(parsed.attachments.len(), 1);
        assert_eq!(parsed.attachments[0].url, "feishu://image/img_key_1");
        assert_eq!(parsed.thread_id.as_deref(), Some("omt_thread"));
        assert_eq!(parsed.root_id.as_deref(), Some("om_root"));
        assert_eq!(parsed.parent_id.as_deref(), Some("om_parent"));
    }

    #[test]
    fn parse_receive_event_post_extracts_text_content() {
        let service = build_service();
        let payload = json!({
            "event": {
                "sender": {
                    "sender_id": {
                        "open_id": "ou_sender"
                    }
                },
                "message": {
                    "message_id": "om_2",
                    "chat_id": "oc_chat",
                    "msg_type": "post",
                    "create_time": "1700000000",
                    "content": "{\"zh_cn\":{\"content\":[[{\"tag\":\"text\",\"text\":\"hello\"},{\"tag\":\"text\",\"text\":\" world\"}]]}}"
                }
            }
        });

        let parsed = service
            .webhook_event_to_bridge_message(&payload)
            .expect("post receive event should parse");
        assert_eq!(parsed.content, "hello world");
    }
}
