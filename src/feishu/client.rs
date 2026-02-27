use aes::Aes256;
use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use reqwest::Client;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::debug;
use uuid::Uuid;

use super::types::*;

type Aes256CbcDec = cbc::Decryptor<Aes256>;

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
        let url = "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id";

        let payload = Self::build_message_payload(
            chat_id,
            "text",
            serde_json::json!({ "text": content }),
            Some(Uuid::new_v4().to_string()),
        )?;

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
        let url = "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id";
        let payload = Self::build_message_payload(
            chat_id,
            "post",
            serde_json::to_value(rich_text).context("failed to serialize rich text payload")?,
            Some(Uuid::new_v4().to_string()),
        )?;

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

        // Feishu signature uses SHA256(timestamp + nonce + encrypt_key + raw_body) in hex.
        let signature = provided_signature
            .trim()
            .trim_start_matches("sha256=")
            .to_ascii_lowercase();
        let payload = format!("{}{}{}{}", timestamp, nonce, signing_secret, body);
        let expected = hex::encode(Sha256::digest(payload.as_bytes()));
        Ok(signature == expected)
    }

    pub fn verify_verification_token(&self, token: Option<&str>) -> bool {
        match (&self.verification_token, token) {
            (Some(expected), Some(actual)) => expected == actual,
            (Some(_), None) => false,
            (None, _) => true,
        }
    }

    pub fn decrypt_webhook_content(&self, encrypt: &str) -> Result<String> {
        let Some(encrypt_key) = self.encrypt_key.as_ref() else {
            return Ok(encrypt.to_string());
        };

        let decoded = general_purpose::STANDARD
            .decode(encrypt)
            .context("encrypted webhook payload is not valid base64")?;
        if decoded.len() < 16 {
            anyhow::bail!("encrypted webhook payload is too short");
        }

        let iv = &decoded[..16];
        let mut encrypted = decoded[16..].to_vec();
        let key = Sha256::digest(encrypt_key.as_bytes());

        let decrypted = Aes256CbcDec::new_from_slices(key.as_slice(), iv)
            .context("invalid encrypt key or IV for webhook decryption")?
            .decrypt_padded_mut::<Pkcs7>(&mut encrypted)
            .map_err(|_| anyhow::anyhow!("failed to decrypt webhook payload with AES-256-CBC"))?;

        let text = String::from_utf8(decrypted.to_vec())
            .context("decrypted webhook payload is not valid UTF-8")?;
        debug!("Webhook payload decrypted successfully");
        Ok(text)
    }

    pub fn callback_signature_key<'a>(&'a self, fallback_secret: &'a str) -> Option<&'a str> {
        self.encrypt_key
            .as_deref()
            .filter(|value| !value.is_empty())
            .or_else(|| (!fallback_secret.is_empty()).then_some(fallback_secret))
    }

    fn build_message_payload(
        receive_id: &str,
        msg_type: &str,
        content: Value,
        uuid: Option<String>,
    ) -> Result<Value> {
        let mut payload = serde_json::json!({
            "receive_id": receive_id,
            "msg_type": msg_type,
            "content": serde_json::to_string(&content)
                .context("failed to serialize Feishu message content")?,
        });

        if let Some(uuid) = uuid {
            payload["uuid"] = Value::String(uuid);
        }

        Ok(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::FeishuClient;
    use sha2::Digest;

    #[test]
    fn verify_webhook_signature_uses_official_sha256_formula() {
        let client = FeishuClient::new(
            "app".to_string(),
            "secret".to_string(),
            Some("encrypt_key".to_string()),
            None,
        );
        let timestamp = "1700000000";
        let nonce = "abc123";
        let body = r#"{"encrypt":"xxx"}"#;
        let expected = hex::encode(sha2::Sha256::digest(
            format!("{}{}{}{}", timestamp, nonce, "encrypt_key", body).as_bytes(),
        ));

        let valid = client
            .verify_webhook_signature("encrypt_key", timestamp, nonce, body, &expected)
            .expect("signature validation should not fail");

        assert!(valid);
    }

    #[test]
    fn decrypt_webhook_content_uses_aes_256_cbc() {
        let client = FeishuClient::new(
            "app".to_string(),
            "secret".to_string(),
            Some("test key".to_string()),
            None,
        );

        let decrypted = client
            .decrypt_webhook_content("P37w+VZImNgPEO1RBhJ6RtKl7n6zymIbEG1pReEzghk=")
            .expect("decryption should succeed");

        assert_eq!(decrypted, "hello world");
    }
}
