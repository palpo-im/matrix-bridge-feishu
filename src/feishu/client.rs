use anyhow::Result;
use base64::Engine as _;
use base64::engine::general_purpose;
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde_json::Value;
use sha2::Sha256;
use tracing::debug;

use super::types::*;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct FeishuClient {
    pub(crate) app_id: String,
    pub(crate) app_secret: String,
    pub(crate) encrypt_key: Option<String>,
    pub(crate) verification_token: Option<String>,
    client: Client,
    access_token: Option<String>,
    token_expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl FeishuClient {
    pub fn new(
        app_id: String,
        app_secret: String,
        encrypt_key: Option<String>,
        verification_token: Option<String>,
    ) -> Self {
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

        let response = self.client.post(url).json(&payload).send().await?;

        let json: Value = response.json().await?;

        if json.get("code").and_then(|v| v.as_i64()) != Some(0) {
            return Err(anyhow::anyhow!(
                "Failed to get tenant access token: {:?}",
                json
            ));
        }

        let access_token = json
            .get("tenant_access_token")
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
        let url = format!(
            "https://open.feishu.cn/open-apis/contact/v3/users/{}",
            user_id
        );

        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", access_token))
            .send()
            .await?;

        let json: Value = response.json().await?;

        if json.get("code").and_then(|v| v.as_i64()) != Some(0) {
            return Err(anyhow::anyhow!("Failed to get user: {:?}", json));
        }

        let data = json
            .get("data")
            .ok_or_else(|| anyhow::anyhow!("No data in response"))?;
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

        let response = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", access_token))
            .json(&payload)
            .send()
            .await?;

        let json: Value = response.json().await?;

        if json.get("code").and_then(|v| v.as_i64()) != Some(0) {
            return Err(anyhow::anyhow!("Failed to send message: {:?}", json));
        }

        let message_id = json
            .get("data")
            .and_then(|d| d.get("message_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("No message_id in response"))?
            .to_string();

        Ok(message_id)
    }

    pub async fn send_rich_text_message(
        &mut self,
        chat_id: &str,
        rich_text: &FeishuRichText,
    ) -> Result<String> {
        let access_token = self.get_tenant_access_token().await?;
        let url = "https://open.feishu.cn/open-apis/im/v1/messages";

        let payload = serde_json::json!({
            "receive_id_type": "chat_id",
            "receive_id": chat_id,
            "content_type": "post",
            "content": rich_text
        });

        let response = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", access_token))
            .json(&payload)
            .send()
            .await?;

        let json: Value = response.json().await?;

        if json.get("code").and_then(|v| v.as_i64()) != Some(0) {
            return Err(anyhow::anyhow!("Failed to send rich text: {:?}", json));
        }

        let message_id = json
            .get("data")
            .and_then(|d| d.get("message_id"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("No message_id in response"))?
            .to_string();

        Ok(message_id)
    }

    pub async fn upload_image(&mut self, image_data: Vec<u8>, image_type: &str) -> Result<String> {
        let access_token = self.get_tenant_access_token().await?;
        let url = "https://open.feishu.cn/open-apis/im/v1/images";

        let form = reqwest::multipart::Form::new().part(
            "image",
            reqwest::multipart::Part::bytes(image_data)
                .file_name("image")
                .mime_str(image_type)?,
        );

        let response = self
            .client
            .post(url)
            .header("Authorization", format!("Bearer {}", access_token))
            .multipart(form)
            .send()
            .await?;

        let json: Value = response.json().await?;

        if json.get("code").and_then(|v| v.as_i64()) != Some(0) {
            return Err(anyhow::anyhow!("Failed to upload image: {:?}", json));
        }

        let image_key = json
            .get("data")
            .and_then(|d| d.get("image_key"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("No image_key in response"))?
            .to_string();

        Ok(image_key)
    }

    pub fn verify_webhook_signature(
        &self,
        signing_secret: &str,
        timestamp: &str,
        nonce: &str,
        body: &str,
        provided_signature: &str,
    ) -> Result<bool> {
        if signing_secret.is_empty() {
            return Ok(true);
        }

        let signature = provided_signature.trim().trim_start_matches("sha256=");

        let payload_compact = format!("{}{}{}", timestamp, nonce, body);
        let payload_lines = format!("{}\n{}\n{}", timestamp, nonce, body);

        let compact_b64 = self.sign_hmac_base64(signing_secret, payload_compact.as_bytes())?;
        let compact_hex = self.sign_hmac_hex(signing_secret, payload_compact.as_bytes())?;
        let lines_b64 = self.sign_hmac_base64(signing_secret, payload_lines.as_bytes())?;
        let lines_hex = self.sign_hmac_hex(signing_secret, payload_lines.as_bytes())?;

        Ok(signature == compact_b64
            || signature == compact_hex
            || signature == lines_b64
            || signature == lines_hex)
    }

    pub fn verify_verification_token(&self, token: Option<&str>) -> bool {
        match (&self.verification_token, token) {
            (Some(expected), Some(actual)) => expected == actual,
            (Some(_), None) => false,
            (None, _) => true,
        }
    }

    fn sign_hmac_base64(&self, signing_secret: &str, payload: &[u8]) -> Result<String> {
        let mut mac = HmacSha256::new_from_slice(signing_secret.as_bytes())?;
        mac.update(payload);
        let sig = mac.finalize().into_bytes();
        Ok(general_purpose::STANDARD.encode(sig))
    }

    fn sign_hmac_hex(&self, signing_secret: &str, payload: &[u8]) -> Result<String> {
        let mut mac = HmacSha256::new_from_slice(signing_secret.as_bytes())?;
        mac.update(payload);
        let sig = mac.finalize().into_bytes();
        Ok(hex::encode(sig))
    }

    pub fn decrypt_webhook_content(&self, encrypt: &str) -> Result<String> {
        if self.encrypt_key.is_none() {
            return Ok(encrypt.to_string());
        }

        let decoded = general_purpose::STANDARD.decode(encrypt)?;
        if let Ok(text) = String::from_utf8(decoded) {
            debug!("Webhook payload parsed from base64 content");
            return Ok(text);
        }

        anyhow::bail!("encrypted webhook payload cannot be decrypted with current configuration")
    }
}
