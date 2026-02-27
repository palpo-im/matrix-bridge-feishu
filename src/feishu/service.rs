use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use salvo::prelude::*;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use super::{FeishuClient, FeishuWebhookEvent};
use crate::bridge::FeishuBridge;
use crate::bridge::message::{BridgeMessage, MessageType};

#[derive(Clone)]
pub struct FeishuService {
    client: Arc<Mutex<FeishuClient>>,
    listen_address: String,
    listen_secret: String,
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

    async fn handle_webhook(
        &self,
        req: &mut Request,
        res: &mut Response,
        bridge: FeishuBridge,
    ) -> Result<()> {
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

        let event: FeishuWebhookEvent =
            serde_json::from_value(payload).context("failed to parse webhook event body")?;
        info!("Received Feishu event: {}", event.header.event_type);

        let supported_event = matches!(
            event.header.event_type.as_str(),
            "im.message.receive_v1" | "message.receive_v1"
        );

        if !supported_event {
            debug!(
                "Ignoring unsupported Feishu event type: {}",
                event.header.event_type
            );
            res.status_code(StatusCode::OK);
            res.render("ignored");
            return Ok(());
        }

        let bridge_message = self.webhook_event_to_bridge_message(event)?;
        bridge.handle_feishu_message(bridge_message).await?;

        res.status_code(StatusCode::OK);
        res.render("success");
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

    fn webhook_event_to_bridge_message(&self, event: FeishuWebhookEvent) -> Result<BridgeMessage> {
        let message = event
            .event
            .message
            .ok_or_else(|| anyhow::anyhow!("No message content in webhook event"))?;
        let parsed_content = parse_feishu_message_content(&message.content);

        let (content, msg_type) = match message.msg_type.as_str() {
            "text" => (
                parsed_content
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                MessageType::Text,
            ),
            "post" | "rich_text" => ("[Rich Text]".to_string(), MessageType::RichText),
            "image" => ("[Image]".to_string(), MessageType::Image),
            "file" => ("[File]".to_string(), MessageType::File),
            "audio" => ("[Audio]".to_string(), MessageType::Audio),
            "video" => ("[Video]".to_string(), MessageType::Video),
            "interactive" | "card" => ("[Card]".to_string(), MessageType::Card),
            other => (format!("[Unsupported: {}]", other), MessageType::Text),
        };

        let timestamp = parse_event_timestamp(&message.create_time);
        Ok(BridgeMessage {
            id: message.message_id,
            sender: event.event.sender.sender_id.user_id,
            room_id: message.chat_id,
            content,
            msg_type,
            timestamp,
            attachments: vec![],
        })
    }

    pub async fn get_user(&self, user_id: &str) -> Result<super::FeishuUser> {
        let mut client = self.client.lock().await;
        client.get_user(user_id).await
    }

    pub async fn send_text_message(&self, chat_id: &str, content: &str) -> Result<String> {
        let mut client = self.client.lock().await;
        client.send_text_message(chat_id, content).await
    }

    pub async fn send_rich_text_message(
        &self,
        chat_id: &str,
        rich_text: &super::FeishuRichText,
    ) -> Result<String> {
        let mut client = self.client.lock().await;
        client.send_rich_text_message(chat_id, rich_text).await
    }

    pub async fn upload_image(&self, image_data: Vec<u8>, image_type: &str) -> Result<String> {
        let mut client = self.client.lock().await;
        client.upload_image(image_data, image_type).await
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

    use super::parse_feishu_message_content;

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
}
