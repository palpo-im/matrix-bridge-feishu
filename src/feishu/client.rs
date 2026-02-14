use anyhow::Result;
use base64::{Engine as _, engine::general_purpose};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use serde_json::Value;
use tracing::{info, error, debug};
use reqwest::Client;

use super::types::*;

type HmacSha256 = Hmac<Sha256>;

pub struct FeishuClient {
    app_id: String,
    app_secret: String,
    encrypt_key: Option<String>,
    verification_token: Option<String>,
    client: Client,
    access_token: Option<String>,
    token_expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl FeishuClient {
    pub fn new(app_id: String, app_secret: String, encrypt_key: Option<String>, verification_token: Option<String>) -> Self {
        Self {
            app_id,
            app_secret,
            encrypt_key,
            verification_token,
            client: Client::new(),
            access_token: None,
            token_expires_at: None,
        }
    }

    pub async fn get_tenant_access_token(&mut self) -> Result<String> {
        if let Some(expires_at) = self.token_expires_at {
            if chrono::Utc::now() < expires_at {
                return Ok(self.access_token.clone().unwrap());
            }
        }

        let url = "https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal";

        let payload = serde_json::json!({
            "app_id": self.app_id,
            "app_secret": self.app_secret
        });

        let response = self.client
            .post(url)
            .json(&payload)
            .send()
            .await?;

        let json: Value = response.json().await?;
        
        if json.get("code").and_then(|v| v.as_i64()) != Some(0) {
            return Err(anyhow::anyhow!("Failed to get tenant access token: {:?}", json));
        }

        let access_token = json.get("tenant_access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("No tenant_access_token in response"))?
            .to_string();

        // Set token to expire in 1 hour (Feishu tokens are valid for 2 hours)
        self.token_expires_at = Some(chrono::Utc::now() + chrono::Duration::hours(1));
        self.access_token = Some(access_token.clone());

        Ok(access_token)
    }

    pub async fn get_user(&mut self, user_id: &str) -> Result<FeishuUser> {
        let access_token = self.get_tenant_access_token().await?;
        let url = format!("https://open.feishu.cn/open-apis/contact/v3/users/{}", user_id);

        let response = self.client
            .get(&url)
            .header("Authorization", format!("Bearer {}", access_token))
            .send()
            .await?;

        let json: Value = response.json().await?;
        
        if json.get("code").and_then(|v| v.as_i64()) != Some(0) {
            return Err(anyhow::anyhow!("Failed to get user: {:?}", json));
        }

        let data = json.get("data").ok_or_else(|| anyhow::anyhow!("No data in response"))?;
        let user: FeishuUser = serde_json::from_value(data.clone())?;

        Ok(user)
    }

    pub async fn send_text_message(&mut self, chat_id: &str, content: &str) -> Result<String> {
        let access_token = self.get_tenant_access_token().await?;
        let url = "https://open.feishu.cn/open-apis/im/v1/messages";

        let payload = serde_json::json!({
            "receive_id_type": "chat_id",
            "receive_id": chat_id,
            "content_type": "text",
            "content": serde_json::json!({
                "text": content
            })
        });

        let response = self.client
            .post(url)
            .header("Authorization", format!("Bearer {}", access_token))
            .json(&payload)
            .send()
            .await?;

        let json: Value = response.json().await?;
        
        if json.get("code").and_then(|v| v.as_i64()) != Some(0) {
            return Err(anyhow::anyhow!("Failed to send message: {:?}", json));
        }

        let message_id = json.get("data")
            .and_then(|d| d.get("message_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("No message_id in response"))?
            .to_string();

        Ok(message_id)
    }

    pub async fn send_rich_text_message(&mut self, chat_id: &str, rich_text: &FeishuRichText) -> Result<String> {
        let access_token = self.get_tenant_access_token().await?;
        let url = "https://open.feishu.cn/open-apis/im/v1/messages";

        let payload = serde_json::json!({
            "receive_id_type": "chat_id",
            "receive_id": chat_id,
            "content_type": "post",
            "content": rich_text
        });

        let response = self.client
            .post(url)
            .header("Authorization", format!("Bearer {}", access_token))
            .json(&payload)
            .send()
            .await?;

        let json: Value = response.json().await?;
        
        if json.get("code").and_then(|v| v.as_i64()) != Some(0) {
            return Err(anyhow::anyhow!("Failed to send rich text: {:?}", json));
        }

        let message_id = json.get("data")
            .and_then(|d| d.get("message_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("No message_id in response"))?
            .to_string();

        Ok(message_id)
    }

    pub async fn upload_image(&mut self, image_data: Vec<u8>, image_type: &str) -> Result<String> {
        let access_token = self.get_tenant_access_token().await?;
        let url = "https://open.feishu.cn/open-apis/im/v1/images";

        let form = reqwest::multipart::Form::new()
            .part("image", reqwest::multipart::Part::bytes(image_data)
                .file_name("image")
                .mime_str(image_type)?);

        let response = self.client
            .post(url)
            .header("Authorization", format!("Bearer {}", access_token))
            .multipart(form)
            .send()
            .await?;

        let json: Value = response.json().await?;
        
        if json.get("code").and_then(|v| v.as_i64()) != Some(0) {
            return Err(anyhow::anyhow!("Failed to upload image: {:?}", json));
        }

        let image_key = json.get("data")
            .and_then(|d| d.get("image_key"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("No image_key in response"))?
            .to_string();

        Ok(image_key)
    }

    pub fn verify_webhook_signature(&self, timestamp: &str, nonce: &str, body: &str) -> Result<bool> {
        if let Some(verification_token) = &self.verification_token {
            // Simple verification with token (for Feishu)
            // In practice, you might need more sophisticated verification
            Ok(verification_token == "your_verification_token")
        } else {
            Ok(true) // Skip verification if no token provided
        }
    }

    pub fn decrypt_webhook_content(&self, encrypt: &str) -> Result<String> {
        if let Some(encrypt_key) = &self.encrypt_key {
            // TODO: Implement AES decryption for webhook content
            // This requires implementing AES/CBC/PKCS7Padding decryption
            // For now, return the encrypted content as-is
            Ok(encrypt.to_string())
        } else {
            Ok(encrypt.to_string())
        }
    }
}