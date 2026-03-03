use std::time::Duration;

use aes::Aes256;
use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use feishu_sdk::Client as SdkClient;
use feishu_sdk::api::SelfBuiltTenantAccessTokenReq;
use feishu_sdk::core::{
    ApiResponse as SdkApiResponse, Config as SdkConfig, Error as SdkError,
    RequestOptions as SdkRequestOptions,
};
use reqwest::{Client as HttpClient, RequestBuilder};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};
use uuid::Uuid;

use super::types::*;

type Aes256CbcDec = cbc::Decryptor<Aes256>;

const DEFAULT_FEISHU_API_BASE: &str = "https://open.feishu.cn/open-apis";
const DEFAULT_FEISHU_SDK_BASE: &str = "https://open.feishu.cn";
const IMAGE_SIZE_LIMIT: usize = 10 * 1024 * 1024;
const FILE_SIZE_LIMIT: usize = 30 * 1024 * 1024;
const RESOURCE_DOWNLOAD_LIMIT: usize = 100 * 1024 * 1024;
const DEFAULT_MAX_RETRIES: u32 = 3;
const DEFAULT_RETRY_BASE_MS: u64 = 250;

#[cfg(test)]
#[derive(Debug, serde::Deserialize)]
struct TenantTokenData {
    tenant_access_token: String,
    expire: i64,
}

#[cfg(test)]
#[derive(Debug, serde::Deserialize)]
struct TenantTokenEnvelopeCompat {
    code: i64,
    #[serde(default)]
    msg: String,
    #[serde(default)]
    data: Option<TenantTokenData>,
    #[serde(default)]
    tenant_access_token: Option<String>,
    #[serde(default)]
    expire: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
enum FeishuErrorClass {
    AuthFailed,
    PermissionDenied,
    RateLimited,
    InvalidRequest,
    ServerTransient,
    Unknown,
}

impl FeishuErrorClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::AuthFailed => "auth_failed",
            Self::PermissionDenied => "permission_denied",
            Self::RateLimited => "rate_limited",
            Self::InvalidRequest => "invalid_request",
            Self::ServerTransient => "server_transient",
            Self::Unknown => "unknown",
        }
    }

    fn retryable(self) -> bool {
        matches!(self, Self::RateLimited | Self::ServerTransient)
    }
}

#[derive(Clone)]
pub struct FeishuClient {
    pub(crate) app_id: String,
    pub(crate) app_secret: String,
    pub(crate) encrypt_key: Option<String>,
    pub(crate) verification_token: Option<String>,
    client: HttpClient,
    sdk_client: Option<SdkClient>,
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
        let sdk_config = SdkConfig::builder(&app_id, &app_secret)
            .base_url(Self::sdk_base())
            .build();
        let sdk_client = SdkClient::new(sdk_config).ok();

        Self {
            app_id,
            app_secret,
            encrypt_key,
            verification_token,
            client: HttpClient::new(),
            sdk_client,
            access_token: None,
            token_expires_at: None,
        }
    }

    fn api_base() -> String {
        std::env::var("FEISHU_API_BASE_URL")
            .ok()
            .map(|value| value.trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_FEISHU_API_BASE.to_string())
    }

    fn sdk_base() -> String {
        let base = Self::api_base();
        base.strip_suffix("/open-apis")
            .map(ToOwned::to_owned)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_FEISHU_SDK_BASE.to_string())
    }

    fn sdk_client(&self) -> Result<&SdkClient> {
        self.sdk_client.as_ref().ok_or_else(|| {
            anyhow::anyhow!("failed to initialize feishu-sdk client due to invalid app config")
        })
    }

    fn sdk_request_options(&self) -> SdkRequestOptions {
        SdkRequestOptions::new().retry(
            self.max_retries(),
            Duration::from_millis(self.retry_base_delay_ms()),
        )
    }

    fn ensure_sdk_http_success(api_name: &str, response: &SdkApiResponse) -> Result<()> {
        if (200..300).contains(&response.status) {
            return Ok(());
        }

        let class = classify_http_error(response.status);
        let body = String::from_utf8_lossy(&response.body);
        anyhow::bail!(
            "Feishu {} failed: class={} retryable={} status={} body={}",
            api_name,
            class.as_str(),
            class.retryable(),
            response.status,
            body
        );
    }

    fn map_sdk_error(api_name: &str, err: SdkError) -> anyhow::Error {
        match err {
            SdkError::Api(api_err) => {
                let class = classify_api_error(api_err.code, &api_err.msg);
                anyhow::anyhow!(
                    "Feishu {} failed: class={} retryable={} code={} msg={} status={} request_id={} body={}",
                    api_name,
                    class.as_str(),
                    class.retryable(),
                    api_err.code,
                    api_err.msg,
                    api_err.http_status.unwrap_or_default(),
                    api_err.request_id.unwrap_or_default(),
                    api_err.raw_body
                )
            }
            other => anyhow::anyhow!(
                "Feishu {} failed: class={} retryable={} error={}",
                api_name,
                FeishuErrorClass::Unknown.as_str(),
                other.is_retryable(),
                other
            ),
        }
    }

    pub async fn get_tenant_access_token(&mut self) -> Result<String> {
        if let Some(expires_at) = self.token_expires_at {
            if chrono::Utc::now() < expires_at {
                return Ok(self.access_token.clone().unwrap_or_default());
            }
        }

        let response = self
            .sdk_client()?
            .auth_v3()
            .tenant_access_token_internal(&SelfBuiltTenantAccessTokenReq {
                app_id: self.app_id.clone(),
                app_secret: self.app_secret.clone(),
            })
            .await
            .map_err(|err| Self::map_sdk_error("auth/v3/tenant_access_token/internal", err))
            .context("failed to call tenant_access_token/internal")?;
        if response.tenant_access_token.trim().is_empty() {
            anyhow::bail!("Feishu auth/v3/tenant_access_token/internal returned empty token");
        }

        // Refresh 5 minutes earlier to avoid using near-expiry token.
        let valid_for_secs = (response.expire - 300).max(60);
        self.token_expires_at =
            Some(chrono::Utc::now() + chrono::Duration::seconds(valid_for_secs));
        self.access_token = Some(response.tenant_access_token.clone());

        Ok(response.tenant_access_token)
    }

    pub async fn get_user(&mut self, user_id: &str) -> Result<FeishuUser> {
        let response = self
            .sdk_client()?
            .contact_v3_user()
            .get(user_id.to_string(), Vec::new(), self.sdk_request_options())
            .await
            .map_err(|err| Self::map_sdk_error("contact/v3/users/get", err))
            .context("failed to call contact/v3/users/get")?;
        Self::ensure_sdk_http_success("contact/v3/users/get", &response)?;
        let json = response
            .json_value()
            .map_err(|err| Self::map_sdk_error("contact/v3/users/get", err))
            .context("failed to parse contact/v3/users/get response as JSON")?;

        #[derive(serde::Deserialize)]
        struct UserWrapper {
            user: FeishuUser,
        }

        let data: UserWrapper = Self::parse_data("contact/v3/users/get", json)?;
        Ok(data.user)
    }

    pub async fn get_chat(&mut self, chat_id: &str) -> Result<FeishuChatProfile> {
        let response = self
            .sdk_client()?
            .im_v1_chat()
            .get(chat_id.to_string(), Vec::new(), self.sdk_request_options())
            .await
            .map_err(|err| Self::map_sdk_error("im/v1/chats/get", err))
            .context("failed to call im/v1/chats/get")?;
        Self::ensure_sdk_http_success("im/v1/chats/get", &response)?;
        let json = response
            .json_value()
            .map_err(|err| Self::map_sdk_error("im/v1/chats/get", err))
            .context("failed to parse im/v1/chats/get response as JSON")?;

        #[derive(serde::Deserialize)]
        struct ChatWrapper {
            chat: FeishuChatProfile,
        }

        let data: ChatWrapper = Self::parse_data("im/v1/chats/get", json)?;
        Ok(data.chat)
    }

    pub async fn send_message(
        &mut self,
        receive_id_type: &str,
        receive_id: &str,
        msg_type: &str,
        content: Value,
        uuid: Option<String>,
    ) -> Result<FeishuMessageSendData> {
        self.create_message(receive_id_type, receive_id, msg_type, content, uuid)
            .await
    }

    pub async fn create_message(
        &mut self,
        receive_id_type: &str,
        receive_id: &str,
        msg_type: &str,
        content: Value,
        uuid: Option<String>,
    ) -> Result<FeishuMessageSendData> {
        let payload = Self::build_message_payload(receive_id, msg_type, content, uuid)?;
        let response = self
            .sdk_client()?
            .im_v1_message()
            .send(
                vec![("receive_id_type".to_string(), receive_id_type.to_string())],
                payload,
                self.sdk_request_options(),
            )
            .await
            .map_err(|err| Self::map_sdk_error("im/v1/message/create", err))
            .context("failed to call im/v1/message/create")?;
        Self::ensure_sdk_http_success("im/v1/message/create", &response)?;
        let json = response
            .json_value()
            .map_err(|err| Self::map_sdk_error("im/v1/message/create", err))
            .context("failed to parse im/v1/message/create response as JSON")?;

        Self::parse_data("im/v1/message/create", json)
    }

    pub async fn reply_message(
        &mut self,
        message_id: &str,
        msg_type: &str,
        content: Value,
        reply_in_thread: bool,
        uuid: Option<String>,
    ) -> Result<FeishuMessageSendData> {
        let mut payload = json!({
            "msg_type": msg_type,
            "content": serde_json::to_string(&content)
                .context("failed to serialize reply message content")?,
        });

        if reply_in_thread {
            payload["reply_in_thread"] = Value::Bool(true);
        }
        if let Some(uuid) = uuid {
            payload["uuid"] = Value::String(uuid);
        }

        let response = self
            .sdk_client()?
            .im_v1_message()
            .reply(
                message_id.to_string(),
                vec![("msg_type".to_string(), msg_type.to_string())],
                payload,
                self.sdk_request_options(),
            )
            .await
            .map_err(|err| Self::map_sdk_error("im/v1/message/reply", err))
            .context("failed to call im/v1/message/reply")?;
        Self::ensure_sdk_http_success("im/v1/message/reply", &response)?;
        let json = response
            .json_value()
            .map_err(|err| Self::map_sdk_error("im/v1/message/reply", err))
            .context("failed to parse im/v1/message/reply response as JSON")?;

        Self::parse_data("im/v1/message/reply", json)
    }

    pub async fn update_message(
        &mut self,
        message_id: &str,
        msg_type: &str,
        content: Value,
    ) -> Result<FeishuMessageSendData> {
        let payload = json!({
            "msg_type": msg_type,
            "content": serde_json::to_string(&content)
                .context("failed to serialize update message content")?,
        });

        let response = self
            .sdk_client()?
            .operation("im.v1.message.update")
            .path_param("message_id", message_id)
            .body_json(&payload)
            .map_err(|err| Self::map_sdk_error("im/v1/message/update", err))
            .context("failed to build im/v1/message/update request")?
            .options(self.sdk_request_options())
            .send()
            .await
            .map_err(|err| Self::map_sdk_error("im/v1/message/update", err))
            .context("failed to call im/v1/message/update")?;
        Self::ensure_sdk_http_success("im/v1/message/update", &response)?;
        let json = response
            .json_value()
            .map_err(|err| Self::map_sdk_error("im/v1/message/update", err))
            .context("failed to parse im/v1/message/update response as JSON")?;

        Self::parse_data("im/v1/message/update", json)
    }

    pub async fn recall_message(&mut self, message_id: &str) -> Result<()> {
        let response = self
            .sdk_client()?
            .im_v1_message()
            .delete(message_id.to_string(), self.sdk_request_options())
            .await
            .map_err(|err| Self::map_sdk_error("im/v1/message/delete", err))
            .context("failed to call im/v1/message/delete")?;
        Self::ensure_sdk_http_success("im/v1/message/delete", &response)?;
        if response.body.is_empty() {
            return Ok(());
        }
        if let Ok(json) = response.json_value() {
            Self::ensure_ok("im/v1/message/delete", json)?;
        }
        Ok(())
    }

    pub async fn get_message(&mut self, message_id: &str) -> Result<Option<FeishuMessageData>> {
        let response = self
            .sdk_client()?
            .im_v1_message()
            .get(
                message_id.to_string(),
                Vec::new(),
                self.sdk_request_options(),
            )
            .await
            .map_err(|err| Self::map_sdk_error("im/v1/message/get", err))
            .context("failed to call im/v1/message/get")?;
        Self::ensure_sdk_http_success("im/v1/message/get", &response)?;
        let json = response
            .json_value()
            .map_err(|err| Self::map_sdk_error("im/v1/message/get", err))
            .context("failed to parse im/v1/message/get response as JSON")?;

        let data: FeishuMessageListData = Self::parse_data("im/v1/message/get", json)?;
        Ok(data.items.into_iter().next())
    }

    pub async fn get_message_resource(
        &mut self,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
    ) -> Result<Vec<u8>> {
        let max_retries = self.max_retries();
        let mut attempts = 0_u32;
        let mut delay = Duration::from_millis(self.retry_base_delay_ms());

        loop {
            attempts += 1;
            let response = self
                .sdk_client()?
                .operation("im.v1.message_resource.get")
                .path_param("message_id", message_id)
                .path_param("file_key", file_key)
                .query_param("type", resource_type)
                .options(self.sdk_request_options())
                .send()
                .await
                .map_err(|err| Self::map_sdk_error("im/v1/message-resource/get", err))
                .context("failed to call im/v1/message-resource/get")?;
            let body = response.body;

            if !(200..300).contains(&response.status) {
                let class = classify_http_error(response.status);
                if class.retryable() && attempts <= max_retries {
                    warn!(
                        "Retrying Feishu message resource request status={} class={} attempt={}/{}",
                        response.status,
                        class.as_str(),
                        attempts,
                        max_retries + 1
                    );
                    tokio::time::sleep(delay).await;
                    delay = next_backoff(delay);
                    continue;
                }

                let detail = String::from_utf8_lossy(&body);
                anyhow::bail!(
                    "Feishu im/v1/message-resource/get failed: class={} retryable={} status={} body={}",
                    class.as_str(),
                    class.retryable(),
                    response.status,
                    detail
                );
            }

            if body.len() > RESOURCE_DOWNLOAD_LIMIT {
                anyhow::bail!(
                    "Feishu message resource exceeds {} bytes limit",
                    RESOURCE_DOWNLOAD_LIMIT
                );
            }

            return Ok(body);
        }
    }

    pub async fn send_text_message(&mut self, chat_id: &str, content: &str) -> Result<String> {
        let data = self
            .create_message(
                "chat_id",
                chat_id,
                "text",
                json!({ "text": content }),
                Some(Uuid::new_v4().to_string()),
            )
            .await?;
        Ok(data.message_id)
    }

    pub async fn send_rich_text_message(
        &mut self,
        chat_id: &str,
        rich_text: &FeishuRichText,
    ) -> Result<String> {
        let content = serde_json::to_value(rich_text).context("failed to serialize rich text")?;
        let data = self
            .create_message(
                "chat_id",
                chat_id,
                "post",
                content,
                Some(Uuid::new_v4().to_string()),
            )
            .await?;
        Ok(data.message_id)
    }

    pub async fn upload_image(
        &mut self,
        image_data: Vec<u8>,
        image_type: &str,
        image_use: &str,
    ) -> Result<String> {
        if image_data.is_empty() {
            anyhow::bail!("image payload cannot be empty");
        }
        if image_data.len() > IMAGE_SIZE_LIMIT {
            anyhow::bail!("image payload exceeds {} bytes", IMAGE_SIZE_LIMIT);
        }

        let access_token = self.get_tenant_access_token().await?;
        let url = format!("{}/im/v1/images", Self::api_base());

        let form = reqwest::multipart::Form::new()
            .text("image_type", image_use.to_string())
            .part(
                "image",
                reqwest::multipart::Part::bytes(image_data)
                    .file_name("image")
                    .mime_str(image_type)
                    .context("invalid image mime type")?,
            );

        let response = self
            .execute_json(
                self.client
                    .post(url)
                    .header("Authorization", format!("Bearer {}", access_token))
                    .multipart(form),
            )
            .await
            .context("failed to call im/v1/image/create")?;

        let data: FeishuImageUploadData = Self::parse_data("im/v1/image/create", response)?;
        Ok(data.image_key)
    }

    pub async fn upload_file(
        &mut self,
        file_name: &str,
        file_data: Vec<u8>,
        file_type: &str,
    ) -> Result<String> {
        if file_data.is_empty() {
            anyhow::bail!("file payload cannot be empty");
        }
        if file_data.len() > FILE_SIZE_LIMIT {
            anyhow::bail!("file payload exceeds {} bytes", FILE_SIZE_LIMIT);
        }

        let access_token = self.get_tenant_access_token().await?;
        let url = format!("{}/im/v1/files", Self::api_base());

        let part = reqwest::multipart::Part::bytes(file_data)
            .file_name(file_name.to_string())
            .mime_str("application/octet-stream")
            .context("invalid file mime type")?;

        let form = reqwest::multipart::Form::new()
            .text("file_type", file_type.to_string())
            .text("file_name", file_name.to_string())
            .part("file", part);

        let response = self
            .execute_json(
                self.client
                    .post(url)
                    .header("Authorization", format!("Bearer {}", access_token))
                    .multipart(form),
            )
            .await
            .context("failed to call im/v1/file/create")?;

        let data: FeishuFileUploadData = Self::parse_data("im/v1/file/create", response)?;
        Ok(data.file_key)
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

    async fn execute_json(&self, request: RequestBuilder) -> Result<Value> {
        let mut attempts = 0_u32;
        let max_retries = self.max_retries();
        let mut delay = Duration::from_millis(self.retry_base_delay_ms());
        let mut current_request = request;

        loop {
            attempts += 1;
            let next_request = current_request.try_clone();
            let response = current_request.send().await.context("request failed")?;
            let status = response.status();
            let body = response
                .bytes()
                .await
                .context("failed to read response body")?;

            let json: Value = serde_json::from_slice(&body).with_context(|| {
                format!(
                    "response is not valid JSON: status={} body={}",
                    status,
                    String::from_utf8_lossy(&body)
                )
            })?;

            if !status.is_success() {
                let class = classify_http_error(status.as_u16());
                if class.retryable() && attempts <= max_retries {
                    if let Some(retry_request) = next_request {
                        warn!(
                            "Retrying Feishu HTTP request after status={} class={} attempt={}/{}",
                            status,
                            class.as_str(),
                            attempts,
                            max_retries + 1
                        );
                        tokio::time::sleep(delay).await;
                        delay = next_backoff(delay);
                        current_request = retry_request;
                        continue;
                    }
                }

                anyhow::bail!(
                    "Feishu HTTP request failed: class={} retryable={} status={} body={}",
                    class.as_str(),
                    class.retryable(),
                    status,
                    json
                );
            }

            let envelope: FeishuApiEnvelope<Value> = serde_json::from_value(json.clone())
                .context("failed to parse Feishu API response envelope")?;
            if envelope.code != 0 {
                let class = classify_api_error(envelope.code, &envelope.msg);
                if class.retryable() && attempts <= max_retries {
                    if let Some(retry_request) = next_request {
                        warn!(
                            "Retrying Feishu API request after code={} class={} attempt={}/{} msg={}",
                            envelope.code,
                            class.as_str(),
                            attempts,
                            max_retries + 1,
                            envelope.msg
                        );
                        tokio::time::sleep(delay).await;
                        delay = next_backoff(delay);
                        current_request = retry_request;
                        continue;
                    }
                }

                anyhow::bail!(
                    "Feishu API failed: class={} retryable={} code={} msg={} body={}",
                    class.as_str(),
                    class.retryable(),
                    envelope.code,
                    envelope.msg,
                    json
                );
            }

            return Ok(json);
        }
    }

    fn max_retries(&self) -> u32 {
        std::env::var("FEISHU_API_MAX_RETRIES")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(DEFAULT_MAX_RETRIES)
    }

    fn retry_base_delay_ms(&self) -> u64 {
        std::env::var("FEISHU_API_RETRY_BASE_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_RETRY_BASE_MS)
    }

    fn ensure_ok(api_name: &str, json: Value) -> Result<()> {
        let envelope: FeishuApiEnvelope<Value> = serde_json::from_value(json.clone())
            .with_context(|| format!("{}: failed to parse envelope body={}", api_name, json))?;
        if envelope.code != 0 {
            anyhow::bail!(
                "Feishu {} failed: code={} msg={} body={}",
                api_name,
                envelope.code,
                envelope.msg,
                json
            );
        }
        Ok(())
    }

    fn parse_data<T: DeserializeOwned>(api_name: &str, json: Value) -> Result<T> {
        let envelope: FeishuApiEnvelope<T> = serde_json::from_value(json.clone())
            .with_context(|| format!("{}: failed to parse envelope body={}", api_name, json))?;

        if envelope.code != 0 {
            anyhow::bail!(
                "Feishu {} failed: code={} msg={} body={}",
                api_name,
                envelope.code,
                envelope.msg,
                json
            );
        }

        envelope
            .data
            .ok_or_else(|| anyhow::anyhow!("Feishu {} response data is missing", api_name))
    }

    #[cfg(test)]
    fn parse_tenant_access_token_response(json: Value) -> Result<TenantTokenData> {
        let envelope: TenantTokenEnvelopeCompat = serde_json::from_value(json.clone())
            .with_context(|| {
                format!(
                    "auth/v3/tenant_access_token/internal: failed to parse envelope body={}",
                    json
                )
            })?;

        if envelope.code != 0 {
            anyhow::bail!(
                "Feishu auth/v3/tenant_access_token/internal failed: code={} msg={} body={}",
                envelope.code,
                envelope.msg,
                json
            );
        }

        let data = if let Some(data) = envelope.data {
            data
        } else {
            let tenant_access_token = envelope.tenant_access_token.ok_or_else(|| {
                anyhow::anyhow!(
                    "Feishu auth/v3/tenant_access_token/internal response token is missing"
                )
            })?;
            let expire = envelope.expire.ok_or_else(|| {
                anyhow::anyhow!(
                    "Feishu auth/v3/tenant_access_token/internal response expire is missing"
                )
            })?;
            TenantTokenData {
                tenant_access_token,
                expire,
            }
        };

        if data.tenant_access_token.trim().is_empty() {
            anyhow::bail!(
                "Feishu auth/v3/tenant_access_token/internal returned empty tenant_access_token"
            );
        }

        Ok(data)
    }

    fn build_message_payload(
        receive_id: &str,
        msg_type: &str,
        content: Value,
        uuid: Option<String>,
    ) -> Result<Value> {
        let mut payload = json!({
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

fn classify_http_error(status: u16) -> FeishuErrorClass {
    match status {
        401 => FeishuErrorClass::AuthFailed,
        403 => FeishuErrorClass::PermissionDenied,
        429 => FeishuErrorClass::RateLimited,
        500..=599 => FeishuErrorClass::ServerTransient,
        400..=499 => FeishuErrorClass::InvalidRequest,
        _ => FeishuErrorClass::Unknown,
    }
}

fn classify_api_error(code: i64, msg: &str) -> FeishuErrorClass {
    let normalized = msg.to_ascii_lowercase();

    if matches!(code, 99991663 | 90013)
        || normalized.contains("rate")
        || normalized.contains("frequency")
    {
        return FeishuErrorClass::RateLimited;
    }

    if normalized.contains("token")
        || normalized.contains("unauthorized")
        || normalized.contains("tenant_access_token")
    {
        return FeishuErrorClass::AuthFailed;
    }

    if normalized.contains("permission") || normalized.contains("forbidden") {
        return FeishuErrorClass::PermissionDenied;
    }

    if normalized.contains("invalid")
        || normalized.contains("param")
        || normalized.contains("bad request")
    {
        return FeishuErrorClass::InvalidRequest;
    }

    FeishuErrorClass::Unknown
}

fn next_backoff(current: Duration) -> Duration {
    let next = current.as_millis().saturating_mul(2);
    Duration::from_millis(next.min(8_000) as u64)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sha2::Digest;

    use super::{FeishuClient, classify_api_error, classify_http_error};

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
    fn verify_webhook_signature_rejects_invalid_signature() {
        let client = FeishuClient::new(
            "app".to_string(),
            "secret".to_string(),
            Some("encrypt_key".to_string()),
            None,
        );
        let valid = client
            .verify_webhook_signature(
                "encrypt_key",
                "1700000000",
                "abc123",
                r#"{"encrypt":"xxx"}"#,
                "deadbeef",
            )
            .expect("signature validation should not fail");
        assert!(!valid);
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

    #[test]
    fn classify_http_error_marks_retryable_statuses() {
        assert_eq!(classify_http_error(429).as_str(), "rate_limited");
        assert_eq!(classify_http_error(503).as_str(), "server_transient");
        assert!(!classify_http_error(400).retryable());
    }

    #[test]
    fn classify_api_error_detects_common_categories() {
        assert_eq!(
            classify_api_error(99991663, "rate limited").as_str(),
            "rate_limited"
        );
        assert_eq!(
            classify_api_error(1, "permission denied").as_str(),
            "permission_denied"
        );
        assert_eq!(
            classify_api_error(1, "invalid param").as_str(),
            "invalid_request"
        );
    }

    #[test]
    fn classify_http_error_handles_auth_and_timeout() {
        assert_eq!(classify_http_error(401).as_str(), "auth_failed");
        assert_eq!(classify_http_error(408).as_str(), "invalid_request");
    }

    #[test]
    fn classify_api_error_detects_auth_and_permission_signals() {
        assert_eq!(
            classify_api_error(99991664, "tenant_access_token invalid").as_str(),
            "auth_failed"
        );
        assert_eq!(
            classify_api_error(42, "forbidden by scope").as_str(),
            "permission_denied"
        );
    }

    #[test]
    fn parse_tenant_access_token_response_accepts_data_wrapper() {
        let payload = json!({
            "code": 0,
            "msg": "ok",
            "data": {
                "tenant_access_token": "token_data_wrapped",
                "expire": 7200
            }
        });

        let parsed = FeishuClient::parse_tenant_access_token_response(payload)
            .expect("token response with data wrapper should parse");
        assert_eq!(parsed.tenant_access_token, "token_data_wrapped");
        assert_eq!(parsed.expire, 7200);
    }

    #[test]
    fn parse_tenant_access_token_response_accepts_flat_wrapper() {
        let payload = json!({
            "code": 0,
            "msg": "ok",
            "tenant_access_token": "token_flat",
            "expire": 7200
        });

        let parsed = FeishuClient::parse_tenant_access_token_response(payload)
            .expect("flat token response should parse");
        assert_eq!(parsed.tenant_access_token, "token_flat");
        assert_eq!(parsed.expire, 7200);
    }
}
