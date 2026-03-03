use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use salvo::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::{
    FeishuChatProfile, FeishuClient, FeishuMessageData, FeishuMessageSendData, FeishuRichText,
    FeishuUser,
};
use crate::bridge::FeishuBridge;
use crate::bridge::message::{Attachment, BridgeMessage, MessageType};
use crate::util::{TtlCache, build_trace_id, parse_feishu_api_error};
use crate::web::{ScopedTimer, global_metrics};

#[derive(Clone)]
pub struct FeishuService {
    client: Arc<Mutex<FeishuClient>>,
    event_mode: FeishuEventMode,
    listen_address: String,
    listen_secret: String,
    long_connection_domain: String,
    long_connection_reconnect_interval: Duration,
    chat_queues: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    user_cache: Arc<Mutex<TtlCache<String, FeishuUser>>>,
    chat_cache: Arc<Mutex<TtlCache<String, FeishuChatProfile>>>,
}

const DEFAULT_USER_CACHE_TTL_SECS: u64 = 300;
const DEFAULT_CHAT_CACHE_TTL_SECS: u64 = 300;
const DEFAULT_METADATA_CACHE_CAPACITY: usize = 1024;
const FEISHU_LONG_CONNECTION_ENDPOINT_PATH: &str = "/callback/ws/endpoint";
const FEISHU_LONG_CONNECTION_FRAGMENT_TTL_SECS: u64 = 5;
const FEISHU_DEFAULT_PING_INTERVAL_SECS: u64 = 120;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeishuEventMode {
    Webhook,
    LongConnection,
}

impl FeishuEventMode {
    fn from_config(value: &str) -> Self {
        if value.eq_ignore_ascii_case("webhook") {
            Self::Webhook
        } else {
            Self::LongConnection
        }
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventDispatchResult {
    Accepted,
    Duplicate,
    Ignored,
    BadRequest,
}

#[derive(Debug)]
enum LongConnectionError {
    Retryable(anyhow::Error),
    Fatal(anyhow::Error),
}

impl LongConnectionError {
    fn retryable(err: impl Into<anyhow::Error>) -> Self {
        Self::Retryable(err.into())
    }

    fn fatal(err: impl Into<anyhow::Error>) -> Self {
        Self::Fatal(err.into())
    }
}

impl FeishuService {
    pub fn new(
        app_id: String,
        app_secret: String,
        event_mode: String,
        listen_address: String,
        listen_secret: String,
        long_connection_domain: String,
        long_connection_reconnect_interval_secs: u64,
        encrypt_key: Option<String>,
        verification_token: Option<String>,
    ) -> Self {
        let user_ttl = Duration::from_secs(read_env_u64(
            "FEISHU_USER_CACHE_TTL_SECS",
            DEFAULT_USER_CACHE_TTL_SECS,
        ));
        let chat_ttl = Duration::from_secs(read_env_u64(
            "FEISHU_CHAT_CACHE_TTL_SECS",
            DEFAULT_CHAT_CACHE_TTL_SECS,
        ));
        let cache_capacity = read_env_usize(
            "FEISHU_METADATA_CACHE_CAPACITY",
            DEFAULT_METADATA_CACHE_CAPACITY,
        );

        Self {
            client: Arc::new(Mutex::new(FeishuClient::new(
                app_id,
                app_secret,
                encrypt_key,
                verification_token,
            ))),
            event_mode: FeishuEventMode::from_config(&event_mode),
            listen_address,
            listen_secret,
            long_connection_domain,
            long_connection_reconnect_interval: Duration::from_secs(
                long_connection_reconnect_interval_secs.max(1),
            ),
            chat_queues: Arc::new(Mutex::new(HashMap::new())),
            user_cache: Arc::new(Mutex::new(TtlCache::new(user_ttl, cache_capacity))),
            chat_cache: Arc::new(Mutex::new(TtlCache::new(chat_ttl, cache_capacity))),
        }
    }

    pub async fn start(&self, bridge: FeishuBridge) -> Result<()> {
        match self.event_mode {
            FeishuEventMode::Webhook => self.start_webhook_server(bridge).await,
            FeishuEventMode::LongConnection => self.start_long_connection(bridge).await,
        }
    }

    async fn start_webhook_server(&self, bridge: FeishuBridge) -> Result<()> {
        info!("Starting Feishu webhook service on {}", self.listen_address);

        let webhook_router = self.create_webhook_router(bridge);
        let (hostname, port) = self.parse_listen_address();
        let acceptor = TcpListener::new(format!("{}:{}", hostname, port))
            .bind()
            .await;

        info!("Feishu webhook listening on {}:{}", hostname, port);
        Server::new(acceptor).serve(webhook_router).await;

        Ok(())
    }

    async fn start_long_connection(&self, bridge: FeishuBridge) -> Result<()> {
        info!(
            domain = %self.long_connection_domain,
            reconnect_interval_secs = self.long_connection_reconnect_interval.as_secs(),
            "Starting Feishu long-connection service"
        );

        loop {
            match self.run_long_connection_session(bridge.clone()).await {
                Ok(()) => warn!("Feishu long-connection session exited unexpectedly"),
                Err(LongConnectionError::Retryable(err)) => warn!(
                    error = %err,
                    "Feishu long-connection session failed; reconnecting"
                ),
                Err(LongConnectionError::Fatal(err)) => {
                    error!(error = %err, "Feishu long-connection stopped with fatal error");
                    return Err(err);
                }
            }

            tokio::time::sleep(self.long_connection_reconnect_interval).await;
        }
    }

    async fn run_long_connection_session(
        &self,
        bridge: FeishuBridge,
    ) -> std::result::Result<(), LongConnectionError> {
        let endpoint = self.fetch_long_connection_endpoint().await?;
        let parsed_url = url::Url::parse(&endpoint.url).map_err(|err| {
            LongConnectionError::retryable(anyhow::anyhow!(
                "invalid long-connection URL from Feishu: {err}"
            ))
        })?;
        let service_id = parsed_url
            .query_pairs()
            .find_map(|(key, value)| (key == "service_id").then_some(value))
            .and_then(|value| value.parse::<i32>().ok())
            .unwrap_or_default();

        let (stream, _response) = connect_async(endpoint.url.clone()).await.map_err(|err| {
            LongConnectionError::retryable(anyhow::anyhow!(
                "failed to establish Feishu long-connection websocket: {err}"
            ))
        })?;
        let (mut ws_writer, mut ws_reader) = stream.split();
        let mut ping_interval_secs = endpoint.client_config.ping_interval_secs();
        let mut ping_timer = tokio::time::interval(Duration::from_secs(ping_interval_secs));
        ping_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut payload_reassembler = LongConnectionPayloadReassembler::new(Duration::from_secs(
            FEISHU_LONG_CONNECTION_FRAGMENT_TTL_SECS,
        ));

        info!(
            service_id = service_id,
            ping_interval_secs = ping_interval_secs,
            "Feishu long-connection established"
        );

        loop {
            tokio::select! {
                _ = ping_timer.tick() => {
                    let ping = build_long_connection_ping_frame(service_id).encode_to_vec();
                    ws_writer
                        .send(Message::Binary(ping.into()))
                        .await
                        .map_err(|err| {
                            LongConnectionError::retryable(anyhow::anyhow!(
                                "failed to send long-connection ping: {err}"
                            ))
                        })?;
                }
                incoming = ws_reader.next() => {
                    let Some(incoming) = incoming else {
                        return Err(LongConnectionError::retryable(anyhow::anyhow!(
                            "Feishu long-connection socket closed by peer"
                        )));
                    };

                    let message = incoming.map_err(|err| {
                        LongConnectionError::retryable(anyhow::anyhow!(
                            "failed to read long-connection message: {err}"
                        ))
                    })?;

                    match message {
                        Message::Binary(bytes) => {
                            let frame = LongConnectionFrame::decode(bytes.as_ref()).map_err(|err| {
                                LongConnectionError::retryable(anyhow::anyhow!(
                                    "failed to decode long-connection protobuf frame: {err}"
                                ))
                            })?;

                            if frame.method == 0 {
                                if let Some(updated_ping_secs) = self.handle_long_connection_control_frame(&frame) {
                                    if updated_ping_secs != ping_interval_secs {
                                        ping_interval_secs = updated_ping_secs;
                                        ping_timer = tokio::time::interval(Duration::from_secs(ping_interval_secs));
                                        ping_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                                        info!(ping_interval_secs = ping_interval_secs, "Updated long-connection ping interval");
                                    }
                                }
                                continue;
                            }

                            if frame.method != 1 {
                                warn!(
                                    method = frame.method,
                                    headers = %format_long_connection_headers(&frame.headers),
                                    payload_encoding = %frame.payload_encoding,
                                    payload_type = %frame.payload_type,
                                    "Ignoring unsupported long-connection frame method"
                                );
                                continue;
                            }

                            if let Some(response_frame) = self
                                .handle_long_connection_data_frame(frame, &bridge, &mut payload_reassembler)
                                .await?
                            {
                                let encoded = response_frame.encode_to_vec();
                                ws_writer
                                    .send(Message::Binary(encoded.into()))
                                    .await
                                    .map_err(|err| {
                                        LongConnectionError::retryable(anyhow::anyhow!(
                                            "failed to send long-connection response frame: {err}"
                                        ))
                                    })?;
                            }
                        }
                        Message::Ping(payload) => {
                            ws_writer
                                .send(Message::Pong(payload))
                                .await
                                .map_err(|err| {
                                    LongConnectionError::retryable(anyhow::anyhow!(
                                        "failed to respond websocket ping with pong: {err}"
                                    ))
                                })?;
                        }
                        Message::Pong(_) => {
                            debug!("Received websocket pong frame from Feishu");
                        }
                        Message::Close(frame) => {
                            return Err(LongConnectionError::retryable(anyhow::anyhow!(
                                "Feishu long-connection closed: {:?}",
                                frame
                            )));
                        }
                        Message::Text(text) => {
                            debug!(text = %text, "Ignoring unexpected text frame from Feishu long-connection");
                        }
                        Message::Frame(_) => {}
                    }
                }
            }
        }
    }

    async fn fetch_long_connection_endpoint(
        &self,
    ) -> std::result::Result<LongConnectionEndpoint, LongConnectionError> {
        let (app_id, app_secret) = {
            let client = self.client.lock().await;
            (client.app_id.clone(), client.app_secret.clone())
        };
        let url = format!(
            "{}{}",
            self.long_connection_domain.trim_end_matches('/'),
            FEISHU_LONG_CONNECTION_ENDPOINT_PATH
        );

        let response = reqwest::Client::new()
            .post(&url)
            .json(&json!({
                "AppID": app_id,
                "AppSecret": app_secret,
            }))
            .send()
            .await
            .map_err(|err| {
                LongConnectionError::retryable(anyhow::anyhow!(
                    "failed to request Feishu long-connection endpoint: {err}"
                ))
            })?;
        let status = response.status();
        let body = response.text().await.map_err(|err| {
            LongConnectionError::retryable(anyhow::anyhow!(
                "failed to read Feishu long-connection endpoint response body: {err}"
            ))
        })?;

        if !status.is_success() {
            let err = anyhow::anyhow!(
                "Feishu long-connection endpoint HTTP error: status={} body={}",
                status,
                body
            );
            return if status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status == reqwest::StatusCode::REQUEST_TIMEOUT
            {
                Err(LongConnectionError::retryable(err))
            } else if status.is_client_error() {
                Err(LongConnectionError::fatal(err))
            } else {
                Err(LongConnectionError::retryable(err))
            };
        }

        let endpoint_response: LongConnectionEndpointResponse =
            serde_json::from_str(&body).map_err(|err| {
                LongConnectionError::retryable(anyhow::anyhow!(
                    "failed to parse Feishu long-connection endpoint response JSON: {err}; body={body}"
                ))
            })?;

        match endpoint_response.code {
            0 => {
                let data = endpoint_response.data.ok_or_else(|| {
                    LongConnectionError::retryable(anyhow::anyhow!(
                        "Feishu long-connection endpoint response missing data"
                    ))
                })?;
                if data.url.trim().is_empty() {
                    return Err(LongConnectionError::retryable(anyhow::anyhow!(
                        "Feishu long-connection endpoint URL is empty"
                    )));
                }
                Ok(LongConnectionEndpoint {
                    url: data.url,
                    client_config: data.client_config.unwrap_or_default(),
                })
            }
            1 | 1000040343 => Err(LongConnectionError::retryable(anyhow::anyhow!(
                "Feishu long-connection endpoint unavailable: code={} msg={}",
                endpoint_response.code,
                endpoint_response.msg
            ))),
            _ => Err(LongConnectionError::fatal(anyhow::anyhow!(
                "Feishu long-connection endpoint rejected request: code={} msg={}",
                endpoint_response.code,
                endpoint_response.msg
            ))),
        }
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

    fn handle_long_connection_control_frame(&self, frame: &LongConnectionFrame) -> Option<u64> {
        let message_type = header_value(&frame.headers, "type");
        if message_type.as_deref() != Some("pong") || frame.payload.is_empty() {
            return None;
        }

        let config: LongConnectionClientConfig = serde_json::from_slice(&frame.payload).ok()?;
        Some(config.ping_interval_secs())
    }

    async fn handle_long_connection_data_frame(
        &self,
        mut frame: LongConnectionFrame,
        bridge: &FeishuBridge,
        payload_reassembler: &mut LongConnectionPayloadReassembler,
    ) -> std::result::Result<Option<LongConnectionFrame>, LongConnectionError> {
        let sum = header_value(&frame.headers, "sum")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(1);
        let seq = header_value(&frame.headers, "seq")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let message_id = header_value(&frame.headers, "message_id")
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let message_type = header_value(&frame.headers, "type").unwrap_or_default();
        info!(
            message_id = %message_id,
            message_type = %message_type,
            sum = sum,
            seq = seq,
            payload_encoding = %frame.payload_encoding,
            payload_type = %frame.payload_type,
            headers = %format_long_connection_headers(&frame.headers),
            "Received Feishu long-connection data frame"
        );

        let raw_payload = std::mem::take(&mut frame.payload);
        let payload = if sum > 1 {
            match payload_reassembler.push(&message_id, sum, seq, raw_payload) {
                Some(merged) => merged,
                None => return Ok(None),
            }
        } else {
            raw_payload
        };

        let started_at = Instant::now();
        let mut response_code = 200;
        if message_type == "event" {
            match serde_json::from_slice::<Value>(&payload) {
                Ok(payload_json) => match self
                    .dispatch_event_payload(payload_json, bridge.clone(), "feishu_long_connection")
                    .await
                {
                    Ok(EventDispatchResult::BadRequest) => response_code = 400,
                    Ok(_) => {}
                    Err(err) => {
                        response_code = 500;
                        error!(
                            error = %err,
                            "Failed to process long-connection Feishu event payload"
                        );
                    }
                },
                Err(err) => {
                    response_code = 500;
                    error!(
                        error = %err,
                        message_id = %message_id,
                        "Failed to parse long-connection payload JSON"
                    );
                }
            }
        } else {
            warn!(
                message_type = %message_type,
                message_id = %message_id,
                "Ignoring unsupported long-connection data frame type"
            );
        }

        let biz_rt = started_at.elapsed().as_millis().to_string();
        set_header_value(&mut frame.headers, "biz_rt", biz_rt);
        frame.payload = serde_json::to_vec(&LongConnectionResponse {
            code: response_code,
            headers: HashMap::new(),
            data: Vec::new(),
        })
        .map_err(|err| {
            LongConnectionError::retryable(anyhow::anyhow!(
                "failed to encode long-connection response payload: {err}"
            ))
        })?;
        Ok(Some(frame))
    }

    async fn dispatch_event_payload(
        &self,
        payload: Value,
        bridge: FeishuBridge,
        flow: &'static str,
    ) -> Result<EventDispatchResult> {
        let event_type = extract_event_type(&payload);

        if event_type.is_none() {
            warn!("Feishu payload missing header.event_type: {}", payload);
            return Ok(EventDispatchResult::BadRequest);
        }
        let event_type = event_type.unwrap_or_default();

        let header_event_id = extract_event_id(&payload);
        let trace_id = build_trace_id(flow, header_event_id.as_deref(), None);

        info!(
            trace_id = %trace_id,
            event_type = %event_type,
            event_id = ?header_event_id,
            "Received Feishu event"
        );
        global_metrics().record_inbound_event(&event_type);
        global_metrics().record_trace_event(flow, "received");

        if let Some(ref event_id) = header_event_id {
            if bridge.is_feishu_event_processed(event_id).await? {
                global_metrics().record_trace_event(flow, "duplicate");
                debug!(
                    trace_id = %trace_id,
                    event_id = %event_id,
                    event_type = %event_type,
                    "Skipping duplicate Feishu event"
                );
                return Ok(EventDispatchResult::Duplicate);
            }
        }

        match event_type.as_str() {
            "im.message.receive_v1" | "message.receive_v1" | "message" => {
                let bridge_message = self
                    .webhook_event_to_bridge_message(&payload)
                    .context("failed to parse receive event")?;
                let content_preview = summarize_for_log(&bridge_message.content, 120);
                info!(
                    trace_id = %trace_id,
                    event_type = %event_type,
                    feishu_message_id = %bridge_message.id,
                    chat_id = %bridge_message.room_id,
                    sender = %bridge_message.sender,
                    msg_type = ?bridge_message.msg_type,
                    content_preview = %content_preview,
                    attachment_count = bridge_message.attachments.len(),
                    "Parsed Feishu message event for Matrix sync"
                );
                let chat_id = bridge_message.room_id.clone();
                let feishu_message_id = bridge_message.id.clone();
                let bridge_message_for_task = bridge_message.clone();
                let event_type_for_task = event_type.clone();
                let event_id_for_task = header_event_id.clone();
                let dead_letter_payload = json!({
                    "message": bridge_message
                });
                let dedupe_key = format!(
                    "{}:{}",
                    event_type_for_task,
                    event_id_for_task
                        .clone()
                        .unwrap_or_else(|| feishu_message_id.clone())
                );
                global_metrics().record_trace_event(flow, "queued");
                self.queue_chat_task(chat_id.clone(), async move {
                    info!(
                        event_type = %event_type_for_task,
                        chat_id = %chat_id,
                        feishu_message_id = %feishu_message_id,
                        "Starting Feishu user sync -> Matrix delivery pipeline"
                    );
                    if let Err(err) = bridge.handle_feishu_message(bridge_message_for_task).await {
                        global_metrics().record_trace_event(flow, "failed");
                        error!(
                            event_type = "im.message.receive_v1",
                            chat_id = %chat_id,
                            feishu_message_id = %feishu_message_id,
                            error = %err,
                            "Failed to process Feishu receive event"
                        );
                        if let Err(store_err) = bridge
                            .record_dead_letter(
                                &event_type_for_task,
                                &dedupe_key,
                                Some(chat_id.clone()),
                                dead_letter_payload.clone(),
                                &err.to_string(),
                            )
                            .await
                        {
                            warn!(
                                event_type = %event_type_for_task,
                                chat_id = %chat_id,
                                error = %store_err,
                                "Failed to persist dead-letter event"
                            );
                        }
                        return;
                    }
                    info!(
                        event_type = %event_type_for_task,
                        chat_id = %chat_id,
                        feishu_message_id = %feishu_message_id,
                        "Completed Feishu user sync -> Matrix delivery pipeline"
                    );

                    if let Some(event_id) = &event_id_for_task {
                        if let Err(err) = bridge
                            .mark_feishu_event_processed(event_id, &event_type_for_task)
                            .await
                        {
                            warn!(
                                event_id = %event_id,
                                event_type = %event_type_for_task,
                                error = %err,
                                "Failed to mark Feishu event as processed"
                            );
                        }
                    }
                    global_metrics().record_trace_event(flow, "processed");
                })
                .await;
                Ok(EventDispatchResult::Accepted)
            }
            "im.message.recalled_v1" => {
                let recalled = self
                    .webhook_event_to_recalled_message(&payload)
                    .context("failed to parse recalled event")?;
                let message_id = recalled.message_id.clone();
                let chat_id = recalled.chat_id.clone();
                let event_type_for_task = event_type.clone();
                let event_id_for_task = header_event_id.clone();
                let dead_letter_payload = json!({
                    "chat_id": chat_id,
                    "message_id": message_id
                });
                let dedupe_key = format!(
                    "{}:{}",
                    event_type_for_task,
                    event_id_for_task
                        .clone()
                        .unwrap_or_else(|| message_id.clone())
                );
                global_metrics().record_trace_event(flow, "queued");
                self.queue_chat_task(chat_id.clone(), async move {
                    if let Err(err) = bridge
                        .handle_feishu_message_recalled(&chat_id, &message_id)
                        .await
                    {
                        global_metrics().record_trace_event(flow, "failed");
                        error!(
                            event_type = "im.message.recalled_v1",
                            chat_id = %chat_id,
                            feishu_message_id = %message_id,
                            error = %err,
                            "Failed to process Feishu recalled event"
                        );
                        if let Err(store_err) = bridge
                            .record_dead_letter(
                                &event_type_for_task,
                                &dedupe_key,
                                Some(chat_id.clone()),
                                dead_letter_payload.clone(),
                                &err.to_string(),
                            )
                            .await
                        {
                            warn!(
                                event_type = %event_type_for_task,
                                chat_id = %chat_id,
                                error = %store_err,
                                "Failed to persist dead-letter event"
                            );
                        }
                        return;
                    }

                    if let Some(event_id) = &event_id_for_task {
                        if let Err(err) = bridge
                            .mark_feishu_event_processed(event_id, &event_type_for_task)
                            .await
                        {
                            warn!(
                                event_id = %event_id,
                                event_type = %event_type_for_task,
                                error = %err,
                                "Failed to mark Feishu event as processed"
                            );
                        }
                    }
                    global_metrics().record_trace_event(flow, "processed");
                })
                .await;
                Ok(EventDispatchResult::Accepted)
            }
            "im.chat.member.user.added_v1" => {
                let event = self
                    .webhook_event_to_chat_member_change(&payload)
                    .context("failed to parse member added event")?;
                let chat_id = event.chat_id.clone();
                let user_ids = event.user_ids.clone();
                for user_id in &user_ids {
                    self.invalidate_user_cache(user_id).await;
                }
                let event_type_for_task = event_type.clone();
                let event_id_for_task = header_event_id.clone();
                let dead_letter_payload = json!({
                    "chat_id": event.chat_id,
                    "user_ids": event.user_ids
                });
                let dedupe_key = format!(
                    "{}:{}",
                    event_type_for_task,
                    event_id_for_task.clone().unwrap_or_else(|| chat_id.clone())
                );
                global_metrics().record_trace_event(flow, "queued");
                self.queue_chat_task(chat_id.clone(), async move {
                    if let Err(err) = bridge
                        .handle_feishu_chat_member_added(&chat_id, &user_ids)
                        .await
                    {
                        global_metrics().record_trace_event(flow, "failed");
                        error!(
                            event_type = "im.chat.member.user.added_v1",
                            chat_id = %chat_id,
                            error = %err,
                            "Failed to process Feishu member added event"
                        );
                        if let Err(store_err) = bridge
                            .record_dead_letter(
                                &event_type_for_task,
                                &dedupe_key,
                                Some(chat_id.clone()),
                                dead_letter_payload.clone(),
                                &err.to_string(),
                            )
                            .await
                        {
                            warn!(
                                event_type = %event_type_for_task,
                                chat_id = %chat_id,
                                error = %store_err,
                                "Failed to persist dead-letter event"
                            );
                        }
                        return;
                    }

                    if let Some(event_id) = &event_id_for_task {
                        if let Err(err) = bridge
                            .mark_feishu_event_processed(event_id, &event_type_for_task)
                            .await
                        {
                            warn!(
                                event_id = %event_id,
                                event_type = %event_type_for_task,
                                error = %err,
                                "Failed to mark Feishu event as processed"
                            );
                        }
                    }
                    global_metrics().record_trace_event(flow, "processed");
                })
                .await;
                Ok(EventDispatchResult::Accepted)
            }
            "im.chat.member.user.deleted_v1" => {
                let event = self
                    .webhook_event_to_chat_member_change(&payload)
                    .context("failed to parse member deleted event")?;
                let chat_id = event.chat_id.clone();
                let user_ids = event.user_ids.clone();
                for user_id in &user_ids {
                    self.invalidate_user_cache(user_id).await;
                }
                let event_type_for_task = event_type.clone();
                let event_id_for_task = header_event_id.clone();
                let dead_letter_payload = json!({
                    "chat_id": event.chat_id,
                    "user_ids": event.user_ids
                });
                let dedupe_key = format!(
                    "{}:{}",
                    event_type_for_task,
                    event_id_for_task.clone().unwrap_or_else(|| chat_id.clone())
                );
                global_metrics().record_trace_event(flow, "queued");
                self.queue_chat_task(chat_id.clone(), async move {
                    if let Err(err) = bridge
                        .handle_feishu_chat_member_deleted(&chat_id, &user_ids)
                        .await
                    {
                        global_metrics().record_trace_event(flow, "failed");
                        error!(
                            event_type = "im.chat.member.user.deleted_v1",
                            chat_id = %chat_id,
                            error = %err,
                            "Failed to process Feishu member deleted event"
                        );
                        if let Err(store_err) = bridge
                            .record_dead_letter(
                                &event_type_for_task,
                                &dedupe_key,
                                Some(chat_id.clone()),
                                dead_letter_payload.clone(),
                                &err.to_string(),
                            )
                            .await
                        {
                            warn!(
                                event_type = %event_type_for_task,
                                chat_id = %chat_id,
                                error = %store_err,
                                "Failed to persist dead-letter event"
                            );
                        }
                        return;
                    }

                    if let Some(event_id) = &event_id_for_task {
                        if let Err(err) = bridge
                            .mark_feishu_event_processed(event_id, &event_type_for_task)
                            .await
                        {
                            warn!(
                                event_id = %event_id,
                                event_type = %event_type_for_task,
                                error = %err,
                                "Failed to mark Feishu event as processed"
                            );
                        }
                    }
                    global_metrics().record_trace_event(flow, "processed");
                })
                .await;
                Ok(EventDispatchResult::Accepted)
            }
            "im.chat.updated_v1" => {
                let event = self
                    .webhook_event_to_chat_updated(&payload)
                    .context("failed to parse chat updated event")?;
                let chat_id = event.chat_id.clone();
                self.invalidate_chat_cache(&chat_id).await;
                let chat_name = event.chat_name.clone();
                let chat_mode = event.chat_mode.clone();
                let chat_type = event.chat_type.clone();
                let event_type_for_task = event_type.clone();
                let event_id_for_task = header_event_id.clone();
                let dead_letter_payload = json!({
                    "chat_id": event.chat_id,
                    "chat_name": event.chat_name,
                    "chat_mode": event.chat_mode,
                    "chat_type": event.chat_type
                });
                let dedupe_key = format!(
                    "{}:{}",
                    event_type_for_task,
                    event_id_for_task.clone().unwrap_or_else(|| chat_id.clone())
                );
                global_metrics().record_trace_event(flow, "queued");
                self.queue_chat_task(chat_id.clone(), async move {
                    if let Err(err) = bridge
                        .handle_feishu_chat_updated(&chat_id, chat_name, chat_mode, chat_type)
                        .await
                    {
                        global_metrics().record_trace_event(flow, "failed");
                        error!(
                            event_type = "im.chat.updated_v1",
                            chat_id = %chat_id,
                            error = %err,
                            "Failed to process Feishu chat updated event"
                        );
                        if let Err(store_err) = bridge
                            .record_dead_letter(
                                &event_type_for_task,
                                &dedupe_key,
                                Some(chat_id.clone()),
                                dead_letter_payload.clone(),
                                &err.to_string(),
                            )
                            .await
                        {
                            warn!(
                                event_type = %event_type_for_task,
                                chat_id = %chat_id,
                                error = %store_err,
                                "Failed to persist dead-letter event"
                            );
                        }
                        return;
                    }

                    if let Some(event_id) = &event_id_for_task {
                        if let Err(err) = bridge
                            .mark_feishu_event_processed(event_id, &event_type_for_task)
                            .await
                        {
                            warn!(
                                event_id = %event_id,
                                event_type = %event_type_for_task,
                                error = %err,
                                "Failed to mark Feishu event as processed"
                            );
                        }
                    }
                    global_metrics().record_trace_event(flow, "processed");
                })
                .await;
                Ok(EventDispatchResult::Accepted)
            }
            "im.chat.disbanded_v1" => {
                let chat_id = self
                    .webhook_event_to_chat_disbanded(&payload)
                    .context("failed to parse chat disbanded event")?;
                self.invalidate_chat_cache(&chat_id).await;
                let event_type_for_task = event_type.clone();
                let event_id_for_task = header_event_id.clone();
                let dead_letter_payload = json!({
                    "chat_id": chat_id
                });
                let dedupe_key = format!(
                    "{}:{}",
                    event_type_for_task,
                    event_id_for_task.clone().unwrap_or_else(|| chat_id.clone())
                );
                global_metrics().record_trace_event(flow, "queued");
                self.queue_chat_task(chat_id.clone(), async move {
                    if let Err(err) = bridge.handle_feishu_chat_disbanded(&chat_id).await {
                        global_metrics().record_trace_event(flow, "failed");
                        error!(
                            event_type = "im.chat.disbanded_v1",
                            chat_id = %chat_id,
                            error = %err,
                            "Failed to process Feishu chat disbanded event"
                        );
                        if let Err(store_err) = bridge
                            .record_dead_letter(
                                &event_type_for_task,
                                &dedupe_key,
                                Some(chat_id.clone()),
                                dead_letter_payload.clone(),
                                &err.to_string(),
                            )
                            .await
                        {
                            warn!(
                                event_type = %event_type_for_task,
                                chat_id = %chat_id,
                                error = %store_err,
                                "Failed to persist dead-letter event"
                            );
                        }
                        return;
                    }

                    if let Some(event_id) = &event_id_for_task {
                        if let Err(err) = bridge
                            .mark_feishu_event_processed(event_id, &event_type_for_task)
                            .await
                        {
                            warn!(
                                event_id = %event_id,
                                event_type = %event_type_for_task,
                                error = %err,
                                "Failed to mark Feishu event as processed"
                            );
                        }
                    }
                    global_metrics().record_trace_event(flow, "processed");
                })
                .await;
                Ok(EventDispatchResult::Accepted)
            }
            _ => {
                global_metrics().record_trace_event(flow, "ignored");
                debug!(event_type = %event_type, "Ignoring unsupported Feishu event type");
                Ok(EventDispatchResult::Ignored)
            }
        }
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

        match self
            .dispatch_event_payload(payload, bridge, "feishu_webhook")
            .await?
        {
            EventDispatchResult::Accepted => {
                res.status_code(StatusCode::OK);
                res.render("accepted");
            }
            EventDispatchResult::Duplicate => {
                res.status_code(StatusCode::OK);
                res.render("duplicate");
            }
            EventDispatchResult::Ignored => {
                res.status_code(StatusCode::OK);
                res.render("ignored");
            }
            EventDispatchResult::BadRequest => {
                res.status_code(StatusCode::BAD_REQUEST);
                res.render("missing event type");
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
        let message = event.get("message").filter(|value| value.is_object()).unwrap_or(event);

        let message_id = pick_first_string(message, &["/message_id", "/open_message_id"])
            .or_else(|| pick_first_string(event, &["/message_id", "/open_message_id"]))
            .ok_or_else(|| anyhow::anyhow!("missing message id"))?;
        let chat_id = pick_first_string(message, &["/chat_id", "/open_chat_id"])
            .or_else(|| pick_first_string(event, &["/chat_id", "/open_chat_id"]))
            .ok_or_else(|| anyhow::anyhow!("missing chat id"))?;
        let msg_type = pick_first_string(message, &["/msg_type", "/message_type"])
            .or_else(|| pick_first_string(event, &["/msg_type", "/message_type"]))
            .unwrap_or_else(|| "text".to_string());
        let create_time = pick_first_string(message, &["/create_time"])
            .or_else(|| pick_first_string(event, &["/create_time", "/ts"]))
            .or_else(|| pick_first_string(payload, &["/header/create_time"]))
            .unwrap_or_default();
        let parent_id = pick_first_string(message, &["/parent_id"])
            .or_else(|| pick_first_string(event, &["/parent_id"]));
        let thread_id = pick_first_string(message, &["/thread_id"])
            .or_else(|| pick_first_string(event, &["/thread_id"]));
        let root_id = pick_first_string(message, &["/root_id"])
            .or_else(|| pick_first_string(event, &["/root_id"]));

        let sender = pick_first_string(
            event,
            &[
                "/sender/sender_id/user_id",
                "/sender/sender_id/open_id",
                "/sender/sender_id/union_id",
                "/sender/user_id",
                "/sender/open_id",
                "/sender/union_id",
                "/sender_id/user_id",
                "/sender_id/open_id",
                "/sender_id/union_id",
                "/user_id",
                "/open_id",
                "/operator_id/user_id",
                "/operator_id/open_id",
            ],
        )
        .unwrap_or_else(|| "unknown".to_string());

        let raw_content = message
            .get("content")
            .cloned()
            .or_else(|| event.get("content").cloned())
            .or_else(|| {
                message
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| json!({ "text": text }))
            })
            .or_else(|| {
                event
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|text| json!({ "text": text }))
            })
            .unwrap_or_else(|| Value::String(String::new()));
        let parsed_content = parse_feishu_message_content(&raw_content);

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
            timestamp: parse_event_timestamp(&create_time),
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

    fn webhook_event_to_chat_member_change(
        &self,
        payload: &Value,
    ) -> Result<ChatMemberChangedEvent> {
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
        let cache_name = "feishu_user_meta";
        {
            let mut cache = self.user_cache.lock().await;
            if let Some(cached_user) = cache.get(&user_id.to_string()) {
                global_metrics().record_cache_hit(cache_name);
                return Ok(cached_user);
            }
        }

        global_metrics().record_cache_miss(cache_name);
        let api = "contact.v3.users.get";
        global_metrics().record_outbound_call(api);
        let mut client = self.client.lock().await;
        let result = client.get_user(user_id).await;
        if let Ok(user) = &result {
            self.user_cache
                .lock()
                .await
                .insert(user_id.to_string(), user.clone());
        }
        if let Err(err) = &result {
            global_metrics().record_outbound_failure(api, &extract_error_code(err));
            log_feishu_api_failure(api, err);
        }
        result
    }

    pub async fn get_chat(&self, chat_id: &str) -> Result<FeishuChatProfile> {
        let cache_name = "feishu_chat_meta";
        {
            let mut cache = self.chat_cache.lock().await;
            if let Some(cached_chat) = cache.get(&chat_id.to_string()) {
                global_metrics().record_cache_hit(cache_name);
                return Ok(cached_chat);
            }
        }

        global_metrics().record_cache_miss(cache_name);
        let api = "im.v1.chats.get";
        global_metrics().record_outbound_call(api);
        let mut client = self.client.lock().await;
        let result = client.get_chat(chat_id).await;
        if let Ok(chat) = &result {
            self.chat_cache
                .lock()
                .await
                .insert(chat_id.to_string(), chat.clone());
        }
        if let Err(err) = &result {
            global_metrics().record_outbound_failure(api, &extract_error_code(err));
            log_feishu_api_failure(api, err);
        }
        result
    }

    pub async fn invalidate_user_cache(&self, user_id: &str) {
        self.user_cache
            .lock()
            .await
            .invalidate(&user_id.to_string());
    }

    pub async fn invalidate_chat_cache(&self, chat_id: &str) {
        self.chat_cache
            .lock()
            .await
            .invalidate(&chat_id.to_string());
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
            log_feishu_api_failure(api, err);
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
            log_feishu_api_failure(api, err);
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
            log_feishu_api_failure(api, err);
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
            log_feishu_api_failure(api, err);
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
            log_feishu_api_failure(api, err);
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
            log_feishu_api_failure(api, err);
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
            log_feishu_api_failure(api, err);
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
            log_feishu_api_failure(api, err);
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
            log_feishu_api_failure(api, err);
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
            log_feishu_api_failure(api, err);
        }
        result
    }
}

#[derive(Debug, Deserialize)]
struct LongConnectionEndpointResponse {
    code: i64,
    msg: String,
    data: Option<LongConnectionEndpointData>,
}

#[derive(Debug, Deserialize)]
struct LongConnectionEndpointData {
    #[serde(rename = "URL")]
    url: String,
    #[serde(rename = "ClientConfig")]
    client_config: Option<LongConnectionClientConfig>,
}

#[derive(Debug)]
struct LongConnectionEndpoint {
    url: String,
    client_config: LongConnectionClientConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct LongConnectionClientConfig {
    #[serde(rename = "ReconnectCount")]
    _reconnect_count: Option<i64>,
    #[serde(rename = "ReconnectInterval")]
    _reconnect_interval: Option<i64>,
    #[serde(rename = "ReconnectNonce")]
    _reconnect_nonce: Option<i64>,
    #[serde(rename = "PingInterval")]
    ping_interval: Option<i64>,
}

impl LongConnectionClientConfig {
    fn ping_interval_secs(&self) -> u64 {
        self.ping_interval
            .filter(|value| *value > 0)
            .map(|value| value as u64)
            .unwrap_or(FEISHU_DEFAULT_PING_INTERVAL_SECS)
    }
}

#[derive(Debug, Serialize)]
struct LongConnectionResponse {
    code: i32,
    headers: HashMap<String, String>,
    data: Vec<u8>,
}

#[derive(Clone, PartialEq, ProstMessage)]
struct LongConnectionHeader {
    #[prost(string, tag = "1")]
    key: String,
    #[prost(string, tag = "2")]
    value: String,
}

#[derive(Clone, PartialEq, ProstMessage)]
struct LongConnectionFrame {
    #[prost(uint64, tag = "1")]
    seq_id: u64,
    #[prost(uint64, tag = "2")]
    log_id: u64,
    #[prost(int32, tag = "3")]
    service: i32,
    #[prost(int32, tag = "4")]
    method: i32,
    #[prost(message, repeated, tag = "5")]
    headers: Vec<LongConnectionHeader>,
    #[prost(string, tag = "6")]
    payload_encoding: String,
    #[prost(string, tag = "7")]
    payload_type: String,
    #[prost(bytes = "vec", tag = "8")]
    payload: Vec<u8>,
    #[prost(string, tag = "9")]
    log_id_new: String,
}

#[derive(Debug)]
struct LongConnectionPayloadReassembler {
    ttl: Duration,
    pending: HashMap<String, LongConnectionPendingPayload>,
}

#[derive(Debug)]
struct LongConnectionPendingPayload {
    created_at: Instant,
    parts: Vec<Option<Vec<u8>>>,
}

impl LongConnectionPayloadReassembler {
    fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            pending: HashMap::new(),
        }
    }

    fn push(
        &mut self,
        message_id: &str,
        sum: usize,
        seq: usize,
        payload: Vec<u8>,
    ) -> Option<Vec<u8>> {
        if sum == 0 || seq >= sum {
            return Some(payload);
        }

        self.cleanup_expired();
        let entry = self
            .pending
            .entry(message_id.to_string())
            .or_insert_with(|| LongConnectionPendingPayload {
                created_at: Instant::now(),
                parts: vec![None; sum],
            });
        if entry.parts.len() != sum {
            entry.parts = vec![None; sum];
            entry.created_at = Instant::now();
        }
        entry.parts[seq] = Some(payload);

        let complete = entry.parts.iter().all(Option::is_some);
        if !complete {
            return None;
        }

        let mut merged = Vec::new();
        if let Some(done) = self.pending.remove(message_id) {
            for chunk in done.parts {
                if let Some(bytes) = chunk {
                    merged.extend_from_slice(&bytes);
                }
            }
        }
        Some(merged)
    }

    fn cleanup_expired(&mut self) {
        let ttl = self.ttl;
        self.pending
            .retain(|_, pending| pending.created_at.elapsed() <= ttl);
    }
}

fn build_long_connection_ping_frame(service_id: i32) -> LongConnectionFrame {
    LongConnectionFrame {
        seq_id: 0,
        log_id: 0,
        service: service_id,
        method: 0,
        headers: vec![LongConnectionHeader {
            key: "type".to_string(),
            value: "ping".to_string(),
        }],
        payload_encoding: String::new(),
        payload_type: String::new(),
        payload: Vec::new(),
        log_id_new: String::new(),
    }
}

fn header_value(headers: &[LongConnectionHeader], key: &str) -> Option<String> {
    headers
        .iter()
        .find_map(|header| (header.key == key).then_some(header.value.clone()))
}

fn set_header_value(headers: &mut Vec<LongConnectionHeader>, key: &str, value: String) {
    if let Some(existing) = headers.iter_mut().find(|header| header.key == key) {
        existing.value = value;
        return;
    }

    headers.push(LongConnectionHeader {
        key: key.to_string(),
        value,
    });
}

fn format_long_connection_headers(headers: &[LongConnectionHeader]) -> String {
    headers
        .iter()
        .map(|header| format!("{}={}", header.key, header.value))
        .collect::<Vec<_>>()
        .join(",")
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
                                    let text =
                                        item.get("text").and_then(Value::as_str).unwrap_or("");
                                    let href =
                                        item.get("href").and_then(Value::as_str).unwrap_or("");
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
    let mut parts = Vec::new();

    if let Some(header) = content
        .pointer("/header/title/content")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        parts.push(header.to_string());
    }

    if let Some(elements) = content.get("elements").and_then(Value::as_array) {
        for element in elements {
            if let Some(text) = element
                .pointer("/text/content")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                parts.push(text.to_string());
            }

            if let Some(button_text) = element
                .pointer("/actions/0/text/content")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                let button_url = element
                    .pointer("/actions/0/multi_url/default_url")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if button_url.is_empty() {
                    parts.push(button_text.to_string());
                } else {
                    parts.push(format!("{} ({})", button_text, button_url));
                }
            }
        }
    }

    if parts.is_empty() {
        content.to_string()
    } else {
        parts.join("\n")
    }
}

fn summarize_for_log(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut summary: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        summary.push_str("...");
    }
    summary.replace('\n', "\\n")
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
    let message = format!("{:#}", err);
    if let Some(idx) = message.find("code=") {
        let suffix = &message[idx + 5..];
        let code: String = suffix
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        if !code.is_empty() {
            return code;
        }
    }
    "unknown".to_string()
}

fn log_feishu_api_failure(api: &str, err: &anyhow::Error) {
    let fields = parse_feishu_api_error(api, err);
    let error_chain = format!("{:#}", err);
    warn!(
        api = %fields.api,
        code = %fields.code,
        msg = %fields.msg,
        retryable = fields.retryable,
        error = %err,
        error_chain = %error_chain,
        "Feishu API call failed"
    );
}

fn pick_first_string(root: &Value, pointers: &[&str]) -> Option<String> {
    pointers.iter().find_map(|pointer| {
        root.pointer(pointer)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn extract_event_type(payload: &Value) -> Option<String> {
    pick_first_string(payload, &["/header/event_type", "/event_type"]).or_else(|| {
        payload
            .pointer("/event/type")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn extract_event_id(payload: &Value) -> Option<String> {
    pick_first_string(payload, &["/header/event_id", "/event_id", "/uuid", "/header/log_id"])
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

fn read_env_u64(key: &str, default_value: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(default_value)
}

fn read_env_usize(key: &str, default_value: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default_value)
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

    use super::{FeishuService, extract_event_type, parse_feishu_message_content};

    fn build_service() -> FeishuService {
        FeishuService::new(
            "app_id".to_string(),
            "app_secret".to_string(),
            "webhook".to_string(),
            "127.0.0.1:8081".to_string(),
            "listen_secret".to_string(),
            "https://open.feishu.cn".to_string(),
            5,
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

    #[test]
    fn parse_receive_event_accepts_message_type_and_open_ids() {
        let service = build_service();
        let payload = json!({
            "event": {
                "sender": {
                    "sender_id": {
                        "union_id": "on_sender"
                    }
                },
                "message": {
                    "open_message_id": "om_open_1",
                    "open_chat_id": "oc_open_chat",
                    "message_type": "text",
                    "create_time": "1700000010",
                    "content": "{\"text\":\"compat text\"}"
                }
            }
        });

        let parsed = service
            .webhook_event_to_bridge_message(&payload)
            .expect("receive event with open ids should parse");
        assert_eq!(parsed.id, "om_open_1");
        assert_eq!(parsed.room_id, "oc_open_chat");
        assert_eq!(parsed.sender, "on_sender");
        assert_eq!(parsed.content, "compat text");
    }

    #[test]
    fn parse_receive_event_accepts_legacy_message_shape() {
        let service = build_service();
        let payload = json!({
            "type": "event_callback",
            "event": {
                "type": "message",
                "open_message_id": "om_legacy",
                "open_chat_id": "oc_legacy_chat",
                "open_id": "ou_legacy_sender",
                "text": "legacy hello",
                "ts": "1700000020"
            }
        });

        let parsed = service
            .webhook_event_to_bridge_message(&payload)
            .expect("legacy receive event should parse");
        assert_eq!(parsed.id, "om_legacy");
        assert_eq!(parsed.room_id, "oc_legacy_chat");
        assert_eq!(parsed.sender, "ou_legacy_sender");
        assert_eq!(parsed.content, "legacy hello");
    }

    #[test]
    fn extract_event_type_accepts_legacy_event_path() {
        let payload = json!({
            "type": "event_callback",
            "event": {
                "type": "message"
            }
        });
        assert_eq!(extract_event_type(&payload).as_deref(), Some("message"));
    }
}
