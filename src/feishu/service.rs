use std::sync::Arc;
use anyhow::Result;
use tracing::{info, error, debug};
use salvo::prelude::*;
use serde_json::Value;

use super::{FeishuClient, FeishuWebhookEvent};
use crate::bridge::FeishuBridge;
use crate::bridge::message::BridgeMessage;

pub struct FeishuService {
    client: FeishuClient,
    listen_address: String,
    listen_secret: String,
}

impl FeishuService {
    pub fn new(app_id: String, app_secret: String, listen_address: String, listen_secret: String, encrypt_key: Option<String>, verification_token: Option<String>) -> Self {
        Self {
            client: FeishuClient::new(app_id, app_secret, encrypt_key, verification_token),
            listen_address,
            listen_secret,
        }
    }

    pub async fn start(&self, bridge: FeishuBridge) -> Result<()> {
        info!("Starting Feishu service on {}", self.listen_address);
        
        // Create webhook router
        let webhook_router = self.create_webhook_router(bridge);
        
        let (hostname, port) = self.parse_listen_address();
        let acceptor = TcpListener::new(&format!("{}:{}", hostname, port)).bind().await;
        
        info!("Feishu webhook listening on {}:{}", hostname, port);
        
        Server::new(acceptor).serve(webhook_router).await;
        
        Ok(())
    }

    fn parse_listen_address(&self) -> (String, u16) {
        let url = url::Url::parse(&self.listen_address).unwrap_or_else(|_| {
            url::Url::parse(&format!("http://{}", self.listen_address)).unwrap()
        });
        
        let hostname = url.host_str().unwrap_or("0.0.0.0").to_string();
        let port = url.port().unwrap_or(8081);
        
        (hostname, port)
    }

    fn create_webhook_router(&self, bridge: FeishuBridge) -> Router {
        let client = self.client.clone();
        let secret = self.listen_secret.clone();
        
        Router::new()
            .hoop(Logger::new())
            .hoop(Cors::new())
            .push(
                Router::with_path("/webhook")
                    .method(Method::Post)
                    .handle(move |req: &mut Request, res: &mut Response| {
                        let bridge = bridge.clone();
                        let client = client.clone();
                        let secret = secret.clone();
                        async move {
                            Self::handle_webhook(req, res, bridge, client, secret).await
                        }
                    })
            )
            .push(
                Router::with_path("/health")
                    .handle(|_res: &mut Response| async {
                        "OK"
                    })
            )
    }

    async fn handle_webhook(
        req: &mut Request,
        res: &mut Response,
        bridge: FeishuBridge,
        _client: FeishuClient,
        secret: String,
    ) {
        debug!("Received Feishu webhook request");
        
        // Get headers
        let timestamp = req.header::<String>("X-Lark-Request-Timestamp");
        let nonce = req.header::<String>("X-Lark-Request-Nonce");
        let signature = req.header::<String>("X-Lark-Signature");
        
        // Get request body
        let body_bytes = req.payload().await.unwrap_or_default();
        let body_str = String::from_utf8_lossy(&body_bytes);
        
        // Verify signature
        if let (Some(timestamp), Some(nonce), Some(signature)) = (timestamp, nonce, signature) {
            // TODO: Implement signature verification
            // if !_client.verify_webhook_signature(&timestamp, &nonce, &body_str)? {
            //     res.status_code(StatusCode::UNAUTHORIZED);
            //     return;
            // }
        }
        
        // Parse webhook event
        match serde_json::from_str::<FeishuWebhookEvent>(&body_str) {
            Ok(event) => {
                info!("Received Feishu event: {} from {}", event.header.event_type, event.event.sender.sender_id.user_id);
                
                // Convert to bridge message
                let bridge_message = self.webhook_event_to_bridge_message(event).await;
                
                if let Ok(message) = bridge_message {
                    // Handle the message through the bridge
                    if let Err(e) = bridge.handle_feishu_message(message).await {
                        error!("Failed to handle Feishu message: {}", e);
                        res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                        return;
                    }
                }
            }
            Err(e) => {
                error!("Failed to parse Feishu webhook event: {}", e);
                res.status_code(StatusCode::BAD_REQUEST);
                return;
            }
        }
        
        res.status_code(StatusCode::OK);
        res.render("success");
    }

    async fn webhook_event_to_bridge_message(&self, event: FeishuWebhookEvent) -> Result<BridgeMessage> {
        match event.header.event_type.as_str() {
            "message.receive_v1" => {
                if let Some(message) = event.event.message {
                    let content = match message.msg_type.as_str() {
                        "text" => {
                            let content_json: Value = serde_json::from_str(&message.content.to_string())?;
                            content_json.get("text")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string()
                        }
                        "rich_text" => {
                            // Extract text from rich text content
                            "[Rich Text]".to_string()
                        }
                        "image" => {
                            "[Image]".to_string()
                        }
                        "file" => {
                            "[File]".to_string()
                        }
                        "audio" => {
                            "[Audio]".to_string()
                        }
                        "video" => {
                            "[Video]".to_string()
                        }
                        "card" => {
                            "[Card]".to_string()
                        }
                        _ => {
                            format!("[Unsupported: {}]", message.msg_type)
                        }
                    };
                    
                    let message = BridgeMessage::new_text(
                        message.chat_id,
                        event.event.sender.sender_id.user_id,
                        content,
                    );
                    
                    Ok(message)
                } else {
                    Err(anyhow::anyhow!("No message in event"))
                }
            }
            _ => {
                Err(anyhow::anyhow!("Unsupported event type: {}", event.header.event_type))
            }
        }
    }

    pub async fn get_user(&mut self, user_id: &str) -> Result<super::FeishuUser> {
        self.client.get_user(user_id).await
    }

    pub async fn send_text_message(&mut self, chat_id: &str, content: &str) -> Result<String> {
        self.client.send_text_message(chat_id, content).await
    }

    pub async fn send_rich_text_message(&mut self, chat_id: &str, rich_text: &super::FeishuRichText) -> Result<String> {
        self.client.send_rich_text_message(chat_id, rich_text).await
    }

    pub async fn upload_image(&mut self, image_data: Vec<u8>, image_type: &str) -> Result<String> {
        self.client.upload_image(image_data, image_type).await
    }
}

impl Clone for FeishuService {
    fn clone(&self) -> Self {
        Self {
            client: FeishuClient::new(
                self.client.app_id.clone(),
                self.client.app_secret.clone(),
                self.client.encrypt_key.clone(),
                self.client.verification_token.clone(),
            ),
            listen_address: self.listen_address.clone(),
            listen_secret: self.listen_secret.clone(),
        }
    }
}