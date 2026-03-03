use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, TimeZone, Utc};
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::sqlite::SqliteConnection;
use matrix_bot_sdk::appservice::{Appservice, AppserviceHandler, Intent};
use matrix_bot_sdk::client::{MatrixAuth, MatrixClient};
use reqwest::header::CONTENT_TYPE;
use reqwest::Method;
use salvo::affix_state;
use salvo::prelude::*;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use url::Url;
use uuid::Uuid;

use super::MatrixEvent;
use super::message::{BridgeMessage, MessageType};
use super::portal::{BridgePortal, RoomType};
use super::puppet::BridgePuppet;
use super::user::{BridgeUser, UserSyncPolicy};
use crate::bridge::{
    MatrixCommandHandler, MatrixCommandOutcome, MatrixEventProcessor, MessageFlow, PresenceHandler,
    ProvisioningCoordinator,
};
use crate::config::Config;
use crate::database::sqlite_stores::SqliteStores;
use crate::database::{
    Database, DeadLetterEvent, DeadLetterStore, EventStore, MediaStore, MessageMapping,
    MessageStore, ProcessedEvent, RoomStore, UserMapping, UserStore,
};
use crate::feishu::FeishuService;
use crate::formatter;
use crate::util::build_trace_id;
use crate::web::{ProvisioningApi, ScopedTimer, global_metrics, metrics_endpoint};

type SqlitePool = Pool<ConnectionManager<SqliteConnection>>;

#[derive(Clone)]
pub struct FeishuBridge {
    pub config: Arc<Config>,
    pub db: Database,
    pub feishu_service: Arc<FeishuService>,
    pub appservice: Arc<Appservice>,
    pub bot_intent: Intent,
    stores: SqliteStores,
    _users_by_mxid: Arc<RwLock<HashMap<String, BridgeUser>>>,
    portals_by_mxid: Arc<RwLock<HashMap<String, BridgePortal>>>,
    portals_by_feishu_room: Arc<RwLock<HashMap<String, String>>>,
    _puppets: Arc<RwLock<HashMap<String, BridgePuppet>>>,
    intents: Arc<RwLock<HashMap<String, Intent>>>,
    command_handler: Arc<MatrixCommandHandler>,
    provisioning: Arc<ProvisioningCoordinator>,
    _presence_handler: Arc<PresenceHandler>,
    started_at: Instant,
    user_sync_policy: UserSyncPolicy,
    user_last_synced_at: Arc<RwLock<HashMap<String, Instant>>>,
}

impl FeishuBridge {
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let config = Arc::new(config);

        let db_type = &config.database.r#type;
        let db_uri = &config.database.uri;
        let max_open = config.database.max_open_conns;
        let max_idle = config.database.max_idle_conns;

        if !db_type.eq_ignore_ascii_case("sqlite") {
            anyhow::bail!(
                "database type '{}' is not supported for bridge stores; please use sqlite",
                db_type
            );
        }

        let db = Database::connect(db_type, db_uri, max_open, max_idle).await?;
        db.run_migrations().await?;

        let sqlite_pool = Self::create_sqlite_pool(db_type, db_uri, max_open, max_idle).await?;
        let stores = SqliteStores::new(sqlite_pool);

        let feishu_service = Arc::new(FeishuService::new(
            config.bridge.app_id.clone(),
            config.bridge.app_secret.clone(),
            config.bridge.event_mode.clone(),
            config.bridge.listen_address.clone(),
            config.bridge.listen_secret.clone(),
            config.bridge.long_connection_domain.clone(),
            config.bridge.long_connection_reconnect_interval_secs,
            config.bridge.encrypt_key.clone(),
            config.bridge.verification_token.clone(),
        ));

        let homeserver_url = Url::parse(&config.bridge.homeserver_url)?;
        let bot_mxid = format!("@{}:{}", config.bridge.bot_username, config.bridge.domain);

        let client = MatrixClient::new(
            homeserver_url,
            MatrixAuth::new(&config.registration.as_token).with_user_id(&bot_mxid),
        );

        let appservice = Appservice::new(
            config.registration.hs_token.clone(),
            config.registration.as_token.clone(),
            client,
        )
        .with_appservice_id(&config.registration.id)
        .with_protocols(["feishu"]);

        let bot_intent = Intent::new(&bot_mxid, appservice.client.clone());

        let command_handler = Arc::new(MatrixCommandHandler::new(true));
        let provisioning = Arc::new(ProvisioningCoordinator::new(config.bridge.webhook_timeout));
        let presence_handler = Arc::new(PresenceHandler::new(Some(50)));
        let user_sync_policy = UserSyncPolicy::new(
            Duration::from_secs(config.bridge.user_sync_interval_secs),
            ChronoDuration::hours(config.bridge.user_mapping_stale_ttl_hours as i64),
        );

        Ok(Self {
            config,
            db,
            feishu_service,
            appservice: Arc::new(appservice),
            bot_intent,
            stores,
            _users_by_mxid: Arc::new(RwLock::new(HashMap::new())),
            portals_by_mxid: Arc::new(RwLock::new(HashMap::new())),
            portals_by_feishu_room: Arc::new(RwLock::new(HashMap::new())),
            _puppets: Arc::new(RwLock::new(HashMap::new())),
            intents: Arc::new(RwLock::new(HashMap::new())),
            command_handler,
            provisioning,
            _presence_handler: presence_handler,
            started_at: Instant::now(),
            user_sync_policy,
            user_last_synced_at: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    async fn create_sqlite_pool(
        db_type: &str,
        db_uri: &str,
        max_open: u32,
        max_idle: u32,
    ) -> anyhow::Result<SqlitePool> {
        if db_type != "sqlite" {
            anyhow::bail!("Only sqlite is currently supported for stores");
        }

        let db_path = db_uri
            .strip_prefix("sqlite://")
            .or_else(|| db_uri.strip_prefix("sqlite:"))
            .unwrap_or(db_uri);

        let manager = ConnectionManager::<SqliteConnection>::new(db_path);
        let pool = Pool::builder()
            .max_size(max_open.max(1))
            .min_idle(Some(max_idle.min(max_open)))
            .build(manager)?;

        Ok(pool)
    }

    pub async fn start(&self) -> anyhow::Result<()> {
        info!("Starting Feishu bridge");

        self.bot_intent.ensure_registered().await?;

        let service = self.feishu_service.clone();
        let bridge_clone = self.clone();
        tokio::spawn(async move {
            if let Err(e) = service.start(bridge_clone).await {
                error!("Feishu service error: {}", e);
            }
        });

        let maintenance_bridge = self.clone();
        tokio::spawn(async move {
            maintenance_bridge.run_user_sync_maintenance_loop().await;
        });

        let room_store = self.stores.room_store();
        let message_store = self.stores.message_store();
        let event_store = self.stores.event_store();
        let media_store = self.stores.media_store();
        let message_flow = Arc::new(MessageFlow::new(
            self.config.clone(),
            self.feishu_service.clone(),
        ));

        let event_processor = Arc::new(MatrixEventProcessor::new(
            self.config.clone(),
            self.feishu_service.clone(),
            room_store,
            self.user_store(),
            message_store,
            event_store,
            media_store,
            message_flow,
        ));

        let handler = Arc::new(BridgeHandler {
            bridge: self.clone(),
            event_processor,
        });

        let appservice_with_handler = Appservice::new(
            self.config.registration.hs_token.clone(),
            self.config.registration.as_token.clone(),
            self.appservice.client.clone(),
        )
        .with_appservice_id(&self.config.registration.id)
        .with_protocols(["feishu"])
        .with_handler(handler);

        let base_router = appservice_with_handler.router();
        let default_provisioning_token = std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_TOKEN")
            .unwrap_or_else(|_| self.config.registration.as_token.clone());
        let provisioning_read_token = std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_READ_TOKEN")
            .unwrap_or_else(|_| default_provisioning_token.clone());
        let provisioning_write_token =
            std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_WRITE_TOKEN")
                .unwrap_or_else(|_| default_provisioning_token.clone());
        let provisioning_delete_token =
            std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_DELETE_TOKEN")
                .or_else(|_| std::env::var("MATRIX_BRIDGE_FEISHU_PROVISIONING_ADMIN_TOKEN"))
                .unwrap_or_else(|_| provisioning_write_token.clone());
        let provisioning_api = ProvisioningApi::new(
            self.room_store(),
            self.dead_letter_store(),
            self.clone(),
            self.provisioning.clone(),
            provisioning_read_token,
            provisioning_write_token,
            provisioning_delete_token,
            self.started_at,
        );
        let status_state = BridgeStatusState {
            room_store: self.room_store(),
            started_at: self.started_at,
        };

        let health_router = Router::new()
            .push(Router::with_path("/health").get(health_handler))
            .push(Router::with_path("/ready").get(ready_handler))
            .push(Router::with_path("/metrics").get(metrics_endpoint))
            .push(
                Router::with_path("/status")
                    .hoop(affix_state::inject(status_state))
                    .get(status_handler),
            );

        let provisioning_router = Router::new()
            .push(Router::with_path("/_matrix/app/v1").push(provisioning_api.clone().router()))
            .push(Router::with_path("/admin").push(provisioning_api.router()));

        let router = Router::new()
            .push(base_router)
            .push(provisioning_router)
            .push(health_router);

        let acceptor = TcpListener::new(format!(
            "{}:{}",
            self.config.bridge.bind_address, self.config.bridge.port
        ))
        .bind()
        .await;

        info!(
            "Appservice listening on {}:{}",
            self.config.bridge.bind_address, self.config.bridge.port
        );

        Server::new(acceptor).serve(router).await;
        Ok(())
    }

    pub async fn stop(&self) {
        info!("Stopping Feishu bridge");
    }

    pub async fn get_or_create_intent(&self, user_id: &str) -> Intent {
        let intents = self.intents.read().await;
        if let Some(intent) = intents.get(user_id) {
            return intent.clone();
        }
        drop(intents);

        let intent = Intent::new(user_id, self.appservice.client.clone());
        self.intents
            .write()
            .await
            .insert(user_id.to_string(), intent.clone());
        intent
    }

    pub fn room_store(&self) -> Arc<dyn RoomStore> {
        self.stores.room_store()
    }

    pub fn user_store(&self) -> Arc<dyn UserStore> {
        self.stores.user_store()
    }

    pub fn event_store(&self) -> Arc<dyn EventStore> {
        self.stores.event_store()
    }

    pub fn message_store(&self) -> Arc<dyn MessageStore> {
        self.stores.message_store()
    }

    pub fn dead_letter_store(&self) -> Arc<dyn DeadLetterStore> {
        self.stores.dead_letter_store()
    }

    pub fn media_store(&self) -> Arc<dyn MediaStore> {
        self.stores.media_store()
    }

    pub async fn is_feishu_event_processed(&self, event_id: &str) -> anyhow::Result<bool> {
        self.stores
            .event_store()
            .is_event_processed(&format!("feishu:{}", event_id))
            .await
            .map_err(Into::into)
    }

    pub async fn mark_feishu_event_processed(
        &self,
        event_id: &str,
        event_type: &str,
    ) -> anyhow::Result<()> {
        let processed = ProcessedEvent {
            id: 0,
            event_id: format!("feishu:{}", event_id),
            event_type: event_type.to_string(),
            source: "feishu".to_string(),
            processed_at: Utc::now(),
        };
        self.stores
            .event_store()
            .mark_event_processed(&processed)
            .await
            .map_err(Into::into)
    }

    pub async fn record_dead_letter(
        &self,
        event_type: &str,
        dedupe_key: &str,
        chat_id: Option<String>,
        payload: Value,
        error: &str,
    ) -> anyhow::Result<DeadLetterEvent> {
        let now = Utc::now();
        let dead_letter = DeadLetterEvent {
            id: 0,
            source: "feishu".to_string(),
            event_type: event_type.to_string(),
            dedupe_key: dedupe_key.to_string(),
            chat_id,
            payload: payload.to_string(),
            error: error.to_string(),
            status: "pending".to_string(),
            replay_count: 0,
            last_replayed_at: None,
            created_at: now,
            updated_at: now,
        };
        self.stores
            .dead_letter_store()
            .create_dead_letter(&dead_letter)
            .await
            .map_err(Into::into)
    }

    pub async fn list_dead_letters(
        &self,
        status: Option<&str>,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> anyhow::Result<Vec<DeadLetterEvent>> {
        self.stores
            .dead_letter_store()
            .list_dead_letters(status, limit, offset)
            .await
            .map_err(Into::into)
    }

    pub async fn replay_dead_letter(&self, id: i64) -> anyhow::Result<()> {
        let Some(event) = self
            .stores
            .dead_letter_store()
            .get_dead_letter_by_id(id)
            .await?
        else {
            anyhow::bail!("dead-letter id={} not found", id);
        };

        if event.status.eq_ignore_ascii_case("replayed") {
            info!(
                dead_letter_id = id,
                event_type = %event.event_type,
                "Skipping replay because dead-letter is already marked replayed"
            );
            return Ok(());
        }

        let payload: Value = serde_json::from_str(&event.payload)
            .map_err(|err| anyhow::anyhow!("invalid dead-letter payload json: {}", err))?;

        let replay_result = self
            .replay_dead_letter_payload(&event.event_type, &payload)
            .await;
        match replay_result {
            Ok(()) => {
                self.stores
                    .dead_letter_store()
                    .mark_dead_letter_replayed(event.id)
                    .await?;
                Ok(())
            }
            Err(err) => {
                self.stores
                    .dead_letter_store()
                    .mark_dead_letter_failed(event.id, &err.to_string())
                    .await?;
                Err(err)
            }
        }
    }

    async fn replay_dead_letter_payload(
        &self,
        event_type: &str,
        payload: &Value,
    ) -> anyhow::Result<()> {
        match event_type {
            "im.message.receive_v1" | "message.receive_v1" => {
                let message: BridgeMessage = serde_json::from_value(
                    payload
                        .get("message")
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("dead-letter missing message field"))?,
                )?;
                self.handle_feishu_message(message).await
            }
            "im.message.recalled_v1" => {
                let chat_id = payload
                    .get("chat_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("dead-letter missing chat_id"))?;
                let message_id = payload
                    .get("message_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("dead-letter missing message_id"))?;
                self.handle_feishu_message_recalled(chat_id, message_id)
                    .await
            }
            "im.chat.member.user.added_v1" => {
                let chat_id = payload
                    .get("chat_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("dead-letter missing chat_id"))?;
                let user_ids = payload
                    .get("user_ids")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow::anyhow!("dead-letter missing user_ids"))?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>();
                self.handle_feishu_chat_member_added(chat_id, &user_ids)
                    .await
            }
            "im.chat.member.user.deleted_v1" => {
                let chat_id = payload
                    .get("chat_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("dead-letter missing chat_id"))?;
                let user_ids = payload
                    .get("user_ids")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow::anyhow!("dead-letter missing user_ids"))?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>();
                self.handle_feishu_chat_member_deleted(chat_id, &user_ids)
                    .await
            }
            "im.chat.updated_v1" => {
                let chat_id = payload
                    .get("chat_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("dead-letter missing chat_id"))?;
                let chat_name = payload
                    .get("chat_name")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let chat_mode = payload
                    .get("chat_mode")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                let chat_type = payload
                    .get("chat_type")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                self.handle_feishu_chat_updated(chat_id, chat_name, chat_mode, chat_type)
                    .await
            }
            "im.chat.disbanded_v1" => {
                let chat_id = payload
                    .get("chat_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("dead-letter missing chat_id"))?;
                self.handle_feishu_chat_disbanded(chat_id).await
            }
            _ => anyhow::bail!("unsupported dead-letter event_type '{}'", event_type),
        }
    }

    async fn reserve_user_sync_slot(&self, feishu_user_id: &str) -> bool {
        let mut guard = self.user_last_synced_at.write().await;
        let last_synced_at = guard.get(feishu_user_id).copied();
        if !self.user_sync_policy.should_refresh(last_synced_at) {
            return false;
        }
        guard.insert(feishu_user_id.to_string(), Instant::now());
        true
    }

    async fn sync_feishu_user_mapping(
        &self,
        feishu_user_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let feishu_user_id = feishu_user_id.trim();
        if feishu_user_id.is_empty() {
            return Ok(None);
        }

        let user_store = self.user_store();
        if !self.reserve_user_sync_slot(feishu_user_id).await {
            if let Some(mapping) = user_store.get_user_by_feishu_id(feishu_user_id).await? {
                return Ok(Some(mapping.feishu_user_id));
            }
            debug!(
                feishu_user_id = %feishu_user_id,
                "No direct user mapping while user sync slot is cooling down; resolving canonical Feishu user ID"
            );
        }

        let profile = self.feishu_service.get_user(feishu_user_id).await?;
        let matrix_localpart = self.config.format_username(&profile.user_id);
        let matrix_user_id = format!("@{}:{}", matrix_localpart, self.config.bridge.domain);
        let displayname = profile.name.trim().to_string();
        let avatar_url = profile
            .avatar
            .as_ref()
            .map(|avatar| avatar.avatar_240.trim().to_string())
            .filter(|value| !value.is_empty());

        if let Some(mut mapping) = user_store.get_user_by_feishu_id(&profile.user_id).await? {
            let mut changed = false;
            if mapping.matrix_user_id != matrix_user_id {
                mapping.matrix_user_id = matrix_user_id.clone();
                changed = true;
            }
            if mapping.feishu_username.as_deref() != Some(displayname.as_str()) {
                mapping.feishu_username = Some(displayname.clone());
                changed = true;
            }
            if mapping.feishu_avatar != avatar_url {
                mapping.feishu_avatar = avatar_url.clone();
                changed = true;
            }
            if changed {
                mapping.updated_at = Utc::now();
                user_store.update_user_mapping(&mapping).await?;
            }
        } else {
            let mut mapping = UserMapping::new(
                matrix_user_id.clone(),
                profile.user_id.clone(),
                Some(displayname.clone()),
            );
            mapping.feishu_avatar = avatar_url.clone();
            user_store.create_user_mapping(&mapping).await?;
        }

        {
            let mut users = self._users_by_mxid.write().await;
            let entry = users
                .entry(matrix_user_id.clone())
                .or_insert_with(|| BridgeUser::new(matrix_user_id.clone()));
            entry.feishu_user_id = Some(profile.user_id.clone());
            entry.connection_state = super::user::ConnectionState::Connected;
        }
        {
            let mut puppets = self._puppets.write().await;
            let puppet = puppets.entry(profile.user_id.clone()).or_insert_with(|| {
                BridgePuppet::new(
                    profile.user_id.clone(),
                    matrix_user_id.clone(),
                    displayname.clone(),
                )
            });
            puppet.apply_profile_sync(Some(&displayname), avatar_url.as_deref());
        }

        Ok(Some(profile.user_id))
    }

    async fn run_user_sync_maintenance_loop(self) {
        let interval_secs = self.config.bridge.user_sync_interval_secs.max(30);
        let ticker = Duration::from_secs(interval_secs);
        let user_store = self.user_store();

        loop {
            tokio::time::sleep(ticker).await;

            let cutoff = self.user_sync_policy.stale_cutoff(Utc::now());
            match user_store.cleanup_stale_user_mappings(cutoff).await {
                Ok(removed) => {
                    if removed > 0 {
                        info!(
                            removed = removed,
                            stale_cutoff = %cutoff,
                            "Cleaned stale user mappings"
                        );
                    }
                }
                Err(err) => warn!(
                    error = %err,
                    "Failed to clean stale user mappings"
                ),
            }

            let retention = Duration::from_secs(interval_secs.saturating_mul(4).max(120));
            let now = Instant::now();
            self.user_last_synced_at
                .write()
                .await
                .retain(|_, synced_at| now.duration_since(*synced_at) <= retention);
        }
    }

    pub async fn handle_feishu_message(&self, message: BridgeMessage) -> anyhow::Result<()> {
        let _timer = ScopedTimer::new("feishu_message_process");
        let trace_id = build_trace_id("feishu_to_matrix", None, Some(&message.id));
        let content_preview = summarize_for_log(&message.content, 120);
        global_metrics().record_trace_event("feishu_to_matrix", "received");
        info!(
            trace_id = %trace_id,
            feishu_message_id = %message.id,
            chat_id = %message.room_id,
            sender = %message.sender,
            msg_type = ?message.msg_type,
            content_preview = %content_preview,
            attachment_count = message.attachments.len(),
            "Handling Feishu message"
        );

        if self
            .stores
            .message_store()
            .get_message_by_feishu_id(&message.id)
            .await?
            .is_some()
        {
            global_metrics().record_trace_event("feishu_to_matrix", "duplicate");
            debug!(
                trace_id = %trace_id,
                feishu_message_id = %message.id,
                chat_id = %message.room_id,
                "Skipping already bridged Feishu message (duplicate or echo)"
            );
            return Ok(());
        }

        let bridge_bot_mxid = format!(
            "@{}:{}",
            self.config.bridge.bot_username, self.config.bridge.domain
        );
        let mut matrix_sender_mxid = bridge_bot_mxid.clone();
        let mut canonical_sender_feishu_id = message.sender.clone();

        info!(
            trace_id = %trace_id,
            feishu_user_id = %message.sender,
            "Syncing Feishu user metadata before Matrix delivery"
        );
        match self.sync_feishu_user_mapping(&message.sender).await {
            Ok(Some(canonical_feishu_user_id)) => {
                canonical_sender_feishu_id = canonical_feishu_user_id.clone();
                match self
                    .user_store()
                    .get_user_by_feishu_id(&canonical_feishu_user_id)
                    .await
                {
                    Ok(Some(mapping)) => {
                        matrix_sender_mxid = mapping.matrix_user_id.clone();
                        println!(
                            "[Feishu->Matrix Sender] mapping found: msg_id={} feishu_sender={} canonical_feishu_sender={} matrix_sender={}",
                            message.id, message.sender, canonical_feishu_user_id, matrix_sender_mxid
                        );
                        info!(
                            trace_id = %trace_id,
                            feishu_user_id = %message.sender,
                            canonical_feishu_user_id = %canonical_feishu_user_id,
                            matrix_user_id = %matrix_sender_mxid,
                            "Resolved mapped Matrix sender for Feishu message"
                        );
                    }
                    Ok(None) => warn!(
                        trace_id = %trace_id,
                        feishu_user_id = %message.sender,
                        canonical_feishu_user_id = %canonical_feishu_user_id,
                        "Feishu user sync completed but no user mapping found; falling back to bridge bot sender"
                    ),
                    Err(err) => warn!(
                        trace_id = %trace_id,
                        feishu_user_id = %message.sender,
                        canonical_feishu_user_id = %canonical_feishu_user_id,
                        error = %err,
                        "Failed to load synced Feishu user mapping; falling back to bridge bot sender"
                    ),
                }
            }
            Ok(None) => warn!(
                trace_id = %trace_id,
                feishu_user_id = %message.sender,
                "Feishu message sender is empty; falling back to bridge bot sender"
            ),
            Err(err) => warn!(
                trace_id = %trace_id,
                feishu_user_id = %message.sender,
                error = %err,
                "Failed to sync Feishu user mapping metadata; falling back to bridge bot sender"
            ),
        }
        if matrix_sender_mxid == bridge_bot_mxid {
            println!(
                "[Feishu->Matrix Sender] fallback to bot: msg_id={} feishu_sender={} canonical_feishu_sender={} bot_sender={}",
                message.id, message.sender, canonical_sender_feishu_id, bridge_bot_mxid
            );
        }

        let room_mapping = self
            .stores
            .room_store()
            .get_room_by_feishu_id(&message.room_id)
            .await?;

        let chat_profile = if room_mapping
            .as_ref()
            .and_then(|mapping| mapping.feishu_chat_name.as_deref())
            .is_none()
        {
            self.feishu_service.get_chat(&message.room_id).await.ok()
        } else {
            None
        };

        let portal = if let Some(mut mapping) = room_mapping {
            info!(
                trace_id = %trace_id,
                feishu_chat_id = %message.room_id,
                matrix_room_id = %mapping.matrix_room_id,
                chat_type = %mapping.feishu_chat_type,
                "Found room mapping for Feishu message"
            );
            if mapping.feishu_chat_name.is_none() {
                if let Some(name) = chat_profile.as_ref().and_then(|chat| chat.name.clone()) {
                    mapping.feishu_chat_name = Some(name.clone());
                    mapping.updated_at = Utc::now();
                    if let Err(err) = self.stores.room_store().update_room_mapping(&mapping).await {
                        warn!(
                            chat_id = %message.room_id,
                            error = %err,
                            "Failed to backfill Feishu chat name into room mapping"
                        );
                    }
                }
            }

            BridgePortal::new(
                message.room_id.clone(),
                mapping.matrix_room_id.clone(),
                mapping
                    .feishu_chat_name
                    .or_else(|| chat_profile.as_ref().and_then(|chat| chat.name.clone()))
                    .unwrap_or_else(|| message.room_id.clone()),
                format!(
                    "@{}:{}",
                    self.config.bridge.bot_username, self.config.bridge.domain
                ),
            )
        } else {
            warn!(
                trace_id = %trace_id,
                feishu_chat_id = %message.room_id,
                "No room mapping found; creating fallback portal (message may not reach intended Matrix room)"
            );
            self.get_or_create_portal_by_feishu_room(&message.room_id)
                .await?
        };

        info!(
            trace_id = %trace_id,
            feishu_message_id = %message.id,
            matrix_room_id = %portal.mxid,
            "Resolved Matrix destination for Feishu message"
        );

        let mut intent = self.get_or_create_intent(&matrix_sender_mxid).await;
        if matrix_sender_mxid == bridge_bot_mxid {
            intent.ensure_registered().await?;
        } else if let Err(err) = self
            .ensure_matrix_sender_joined_room(
                &intent,
                &matrix_sender_mxid,
                &bridge_bot_mxid,
                &portal.mxid,
            )
            .await
        {
            warn!(
                trace_id = %trace_id,
                matrix_room_id = %portal.mxid,
                feishu_user_id = %canonical_sender_feishu_id,
                matrix_user_id = %matrix_sender_mxid,
                error = %err,
                "Failed to prepare mapped Matrix sender; falling back to bridge bot sender"
            );
            println!(
                "[Feishu->Matrix Sender] sender preparation failed, fallback to bot: msg_id={} matrix_sender={} room_id={} err={}",
                message.id, matrix_sender_mxid, portal.mxid, err
            );
            matrix_sender_mxid = bridge_bot_mxid.clone();
            intent = self.get_or_create_intent(&bridge_bot_mxid).await;
            intent.ensure_registered().await?;
        }
        println!(
            "[Feishu->Matrix Sender] effective sender selected: msg_id={} matrix_sender={} room_id={}",
            message.id, matrix_sender_mxid, portal.mxid
        );

        let mut reply_to_matrix_event_id = None;
        if let Some(parent_id) = message.parent_id.as_deref() {
            if let Some(parent_mapping) = self
                .stores
                .message_store()
                .get_message_by_feishu_id(parent_id)
                .await?
            {
                reply_to_matrix_event_id = Some(parent_mapping.matrix_event_id);
            } else {
                debug!(
                    "No Matrix mapping found for Feishu parent message {}",
                    parent_id
                );
            }
        }

        let mut primary_matrix_event_id = None;
        if !message.content.trim().is_empty() {
            info!(
                trace_id = %trace_id,
                feishu_message_id = %message.id,
                matrix_room_id = %portal.mxid,
                reply_to = ?reply_to_matrix_event_id,
                "Sending Feishu text content to Matrix"
            );
            let event_id = self
                .send_matrix_text_message(
                    &matrix_sender_mxid,
                    &portal.mxid,
                    &message.content,
                    reply_to_matrix_event_id.as_deref(),
                )
                .await?;
            println!(
                "[Feishu->Matrix Sender] text delivered: msg_id={} matrix_event_id={} matrix_sender={} room_id={}",
                message.id, event_id, matrix_sender_mxid, portal.mxid
            );
            info!(
                trace_id = %trace_id,
                feishu_message_id = %message.id,
                matrix_event_id = %event_id,
                "Sent Feishu text content to Matrix"
            );
            primary_matrix_event_id = Some(event_id);
        }

        if !message.attachments.is_empty() {
            info!(
                trace_id = %trace_id,
                feishu_message_id = %message.id,
                attachment_count = message.attachments.len(),
                "Forwarding Feishu attachments to Matrix"
            );
        }
        let attachment_event_ids = self
            .forward_feishu_attachments_to_matrix(
                &intent,
                &matrix_sender_mxid,
                &portal.mxid,
                &message,
                if primary_matrix_event_id.is_none() {
                    reply_to_matrix_event_id.as_deref()
                } else {
                    None
                },
            )
            .await?;
        if !attachment_event_ids.is_empty() {
            info!(
                trace_id = %trace_id,
                feishu_message_id = %message.id,
                matrix_event_count = attachment_event_ids.len(),
                "Sent Feishu attachments to Matrix"
            );
        }
        if primary_matrix_event_id.is_none() {
            primary_matrix_event_id = attachment_event_ids.first().cloned();
        }

        if let Some(matrix_event_id) = primary_matrix_event_id {
            let link = MessageMapping::new(
                matrix_event_id,
                message.id,
                portal.mxid.clone(),
                matrix_sender_mxid.clone(),
                canonical_sender_feishu_id.clone(),
            )
            .with_threading(
                message.thread_id.clone(),
                message.root_id.clone(),
                message.parent_id.clone(),
            );
            if let Err(err) = self
                .stores
                .message_store()
                .create_message_mapping(&link)
                .await
            {
                global_metrics().record_trace_event("feishu_to_matrix", "mapping_store_failed");
                warn!(
                    trace_id = %trace_id,
                    matrix_event_id = %link.matrix_event_id,
                    feishu_message_id = %link.feishu_message_id,
                    chat_id = %message.room_id,
                    error = %err,
                    "Failed to persist Feishu->Matrix message mapping"
                );
            }

            global_metrics().record_trace_event("feishu_to_matrix", "success");
            info!(
                trace_id = %trace_id,
                matrix_event_id = %link.matrix_event_id,
                feishu_message_id = %link.feishu_message_id,
                chat_id = %message.room_id,
                "Bridged Feishu message to Matrix"
            );
            return Ok(());
        }

        global_metrics().record_trace_event("feishu_to_matrix", "attachments_only_or_empty");

        Ok(())
    }

    pub async fn handle_feishu_message_recalled(
        &self,
        feishu_chat_id: &str,
        feishu_message_id: &str,
    ) -> anyhow::Result<()> {
        info!(
            chat_id = %feishu_chat_id,
            feishu_message_id = %feishu_message_id,
            "Handling Feishu recalled event"
        );

        let mapping = self
            .stores
            .message_store()
            .get_message_by_feishu_id(feishu_message_id)
            .await?;

        let Some(mapping) = mapping else {
            debug!(
                feishu_message_id = %feishu_message_id,
                chat_id = %feishu_chat_id,
                "No message mapping found for recalled Feishu message"
            );
            return Ok(());
        };

        if let Err(err) = self
            .bot_intent
            .redact_event(
                &mapping.room_id,
                &mapping.matrix_event_id,
                Some("Message recalled in Feishu"),
            )
            .await
        {
            warn!(
                "Failed to redact Matrix event {} for Feishu message {}: {}",
                mapping.matrix_event_id, feishu_message_id, err
            );
        }

        if let Err(err) = self
            .stores
            .message_store()
            .delete_message_mapping(mapping.id)
            .await
        {
            warn!(
                "Failed to delete recalled message mapping {} (Feishu {}): {}",
                mapping.id, feishu_message_id, err
            );
        }

        Ok(())
    }

    pub async fn handle_feishu_chat_member_added(
        &self,
        feishu_chat_id: &str,
        user_ids: &[String],
    ) -> anyhow::Result<()> {
        info!(
            "Handling Feishu member added event: chat={} users={}",
            feishu_chat_id,
            user_ids.join(",")
        );

        let Some(mapping) = self
            .stores
            .room_store()
            .get_room_by_feishu_id(feishu_chat_id)
            .await?
        else {
            debug!(
                "No room mapping found for member added event in chat {}",
                feishu_chat_id
            );
            return Ok(());
        };

        if !user_ids.is_empty() {
            for user_id in user_ids {
                if let Err(err) = self.sync_feishu_user_mapping(user_id).await {
                    warn!(
                        feishu_user_id = %user_id,
                        chat_id = %feishu_chat_id,
                        error = %err,
                        "Failed to sync Feishu user mapping on member-added event"
                    );
                }
            }
            let labels = self.resolve_feishu_user_labels(user_ids).await;
            let notice = format!("Feishu members joined: {}", labels.join(", "));
            if let Err(err) = self
                .bot_intent
                .send_notice(&mapping.matrix_room_id, &notice)
                .await
            {
                warn!(
                    "Failed to send Matrix join notice for Feishu chat {}: {}",
                    feishu_chat_id, err
                );
            }
        }

        Ok(())
    }

    pub async fn handle_feishu_chat_member_deleted(
        &self,
        feishu_chat_id: &str,
        user_ids: &[String],
    ) -> anyhow::Result<()> {
        info!(
            "Handling Feishu member deleted event: chat={} users={}",
            feishu_chat_id,
            user_ids.join(",")
        );

        let Some(mapping) = self
            .stores
            .room_store()
            .get_room_by_feishu_id(feishu_chat_id)
            .await?
        else {
            debug!(
                "No room mapping found for member deleted event in chat {}",
                feishu_chat_id
            );
            return Ok(());
        };

        if !user_ids.is_empty() {
            for user_id in user_ids {
                if let Err(err) = self.sync_feishu_user_mapping(user_id).await {
                    warn!(
                        feishu_user_id = %user_id,
                        chat_id = %feishu_chat_id,
                        error = %err,
                        "Failed to sync Feishu user mapping on member-deleted event"
                    );
                }
            }
            let labels = self.resolve_feishu_user_labels(user_ids).await;
            let notice = format!("Feishu members left: {}", labels.join(", "));
            if let Err(err) = self
                .bot_intent
                .send_notice(&mapping.matrix_room_id, &notice)
                .await
            {
                warn!(
                    "Failed to send Matrix leave notice for Feishu chat {}: {}",
                    feishu_chat_id, err
                );
            }
        }

        Ok(())
    }

    async fn resolve_feishu_user_labels(&self, user_ids: &[String]) -> Vec<String> {
        let mut labels = Vec::with_capacity(user_ids.len());
        for user_id in user_ids {
            match self.feishu_service.get_user(user_id).await {
                Ok(user) => {
                    if user.name.trim().is_empty() {
                        labels.push(user_id.clone());
                    } else {
                        labels.push(format!("{}({})", user.name, user_id));
                    }
                }
                Err(_) => labels.push(user_id.clone()),
            }
        }
        labels
    }

    pub async fn handle_feishu_chat_updated(
        &self,
        feishu_chat_id: &str,
        chat_name: Option<String>,
        chat_mode: Option<String>,
        chat_type: Option<String>,
    ) -> anyhow::Result<()> {
        info!(
            "Handling Feishu chat updated event: chat={} name={:?} mode={:?} type={:?}",
            feishu_chat_id, chat_name, chat_mode, chat_type
        );

        let Some(mut mapping) = self
            .stores
            .room_store()
            .get_room_by_feishu_id(feishu_chat_id)
            .await?
        else {
            debug!(
                "No room mapping found for chat updated event in chat {}",
                feishu_chat_id
            );
            return Ok(());
        };

        let normalized_name = chat_name.and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
        let normalized_mode = chat_mode.and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
        let normalized_type = chat_type.and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });

        let mut changed = false;
        if let Some(name) = &normalized_name {
            if mapping.feishu_chat_name.as_deref() != Some(name.as_str()) {
                mapping.feishu_chat_name = Some(name.clone());
                changed = true;
            }
        }

        if let Some(mode) = &normalized_mode {
            if mapping.feishu_chat_type != *mode {
                mapping.feishu_chat_type = mode.clone();
                changed = true;
            }
        } else if let Some(chat_type) = &normalized_type {
            if mapping.feishu_chat_type != *chat_type {
                mapping.feishu_chat_type = chat_type.clone();
                changed = true;
            }
        }

        if changed {
            mapping.updated_at = Utc::now();
            self.stores
                .room_store()
                .update_room_mapping(&mapping)
                .await?;
        }

        let mut portals = self.portals_by_mxid.write().await;
        if let Some(portal) = portals.get_mut(&mapping.matrix_room_id) {
            if let Some(name) = normalized_name {
                portal.name = name;
            }
            if let Some(mode) = &normalized_mode {
                portal
                    .bridge_info
                    .channel
                    .insert("chat_mode".to_string(), Value::String(mode.clone()));
                portal.room_type = room_type_from_chat_type(normalized_type.as_deref());
            }
            if let Some(kind) = normalized_type {
                portal
                    .bridge_info
                    .channel
                    .insert("chat_type".to_string(), Value::String(kind.clone()));
                portal.room_type = room_type_from_chat_type(Some(&kind));
            }
            portal.last_event = Some("im.chat.updated_v1".to_string());
        }
        drop(portals);

        if let Some(mode) = &normalized_mode {
            let notice = if mode.eq_ignore_ascii_case("thread") {
                "Feishu chat mode changed to thread; bridge reply strategy is now thread mode."
            } else {
                "Feishu chat mode changed; bridge reply strategy switched to non-thread mode."
            };
            if let Err(err) = self
                .bot_intent
                .send_notice(&mapping.matrix_room_id, notice)
                .await
            {
                warn!(
                    "Failed to send chat mode update notice for chat {}: {}",
                    feishu_chat_id, err
                );
            }
        }

        Ok(())
    }

    pub async fn handle_feishu_chat_disbanded(&self, feishu_chat_id: &str) -> anyhow::Result<()> {
        info!(
            "Handling Feishu chat disbanded event: chat={}",
            feishu_chat_id
        );

        let mapping = self
            .stores
            .room_store()
            .get_room_by_feishu_id(feishu_chat_id)
            .await?;

        let Some(mapping) = mapping else {
            debug!(
                "No room mapping found for disbanded chat {}; cleaning memory cache only",
                feishu_chat_id
            );
            if let Some(mxid) = self
                .portals_by_feishu_room
                .write()
                .await
                .remove(feishu_chat_id)
            {
                self.portals_by_mxid.write().await.remove(&mxid);
            }
            return Ok(());
        };

        let historical_mappings = self
            .stores
            .message_store()
            .get_messages_by_room(&mapping.matrix_room_id, Some(2000))
            .await?;
        for message in historical_mappings {
            if let Err(err) = self
                .stores
                .message_store()
                .delete_message_mapping(message.id)
                .await
            {
                warn!(
                    "Failed to delete message mapping {} while disbanding chat {}: {}",
                    message.id, feishu_chat_id, err
                );
            }
        }

        self.stores
            .room_store()
            .delete_room_mapping(mapping.id)
            .await?;

        self.portals_by_feishu_room
            .write()
            .await
            .remove(feishu_chat_id);
        self.portals_by_mxid
            .write()
            .await
            .remove(&mapping.matrix_room_id);

        if let Err(err) = self
            .bot_intent
            .send_notice(
                &mapping.matrix_room_id,
                "Feishu chat has been disbanded; bridge mapping was removed automatically.",
            )
            .await
        {
            warn!(
                "Failed to send disband notice to Matrix room {}: {}",
                mapping.matrix_room_id, err
            );
        }

        Ok(())
    }

    pub async fn handle_matrix_message(&self, room_id: &str, event: Value) -> anyhow::Result<()> {
        info!("Handling Matrix message in room {}", room_id);
        let sender = event
            .get("sender")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if self.is_bridge_bot_sender(sender) {
            debug!(
                room_id = %room_id,
                sender = %sender,
                "Ignoring Matrix message sent by bridge bot to prevent self-loop"
            );
            return Ok(());
        }

        let body = event
            .get("content")
            .and_then(|c| c.get("body"))
            .and_then(Value::as_str)
            .unwrap_or_default();

        if self.command_handler.is_command(body) {
            debug!("Matrix command detected: {}", body);
            let room_mapping: Option<crate::database::RoomMapping> = self
                .stores
                .room_store()
                .get_room_by_matrix_id(room_id)
                .await?;

            let outcome = self
                .command_handler
                .handle(body, room_mapping.is_some(), |_| true);
            self.handle_command_outcome(outcome, room_id, sender).await?;
            return Ok(());
        }

        let message = self.matrix_event_to_bridge_message(room_id, event)?;
        let portal = self.get_or_create_portal_by_matrix_room(room_id).await?;

        if portal.feishu_room_id.starts_with("mx_") {
            warn!(
                "Skipping Matrix->Feishu forward for room {} because no Feishu room mapping exists yet",
                room_id
            );
            return Ok(());
        }

        let feishu_content = formatter::format_matrix_to_feishu(message)?;
        self.feishu_service
            .send_text_message(&portal.feishu_room_id, &feishu_content)
            .await?;

        Ok(())
    }

    fn is_bridge_bot_sender(&self, sender: &str) -> bool {
        let configured_bot = format!(
            "@{}:{}",
            self.config.bridge.bot_username, self.config.bridge.domain
        );
        if sender.eq_ignore_ascii_case(&configured_bot) {
            return true;
        }

        let sender_localpart = self.config.registration.sender_localpart.trim();
        if sender_localpart.is_empty() {
            return false;
        }

        let registration_bot = format!("@{}:{}", sender_localpart, self.config.bridge.domain);
        sender.eq_ignore_ascii_case(&registration_bot)
    }

    async fn handle_command_outcome(
        &self,
        outcome: MatrixCommandOutcome,
        room_id: &str,
        _sender: &str,
    ) -> anyhow::Result<()> {
        match outcome {
            MatrixCommandOutcome::Ignored => {}
            MatrixCommandOutcome::Reply(reply) => {
                self.bot_intent.send_text(room_id, &reply).await?;
            }
            MatrixCommandOutcome::BridgeRequested { feishu_chat_id } => {
                let mapping = crate::database::RoomMapping::new(
                    room_id.to_string(),
                    feishu_chat_id.clone(),
                    Some(format!("Feishu {}", feishu_chat_id)),
                );
                self.stores
                    .room_store()
                    .create_room_mapping(&mapping)
                    .await?;
                info!("Created bridge: {} <-> {}", room_id, feishu_chat_id);
                self.bot_intent
                    .send_text(
                        room_id,
                        &format!("Bridged to Feishu chat: {}", feishu_chat_id),
                    )
                    .await?;
            }
            MatrixCommandOutcome::UnbridgeRequested => {
                if let Some(mapping) = self
                    .stores
                    .room_store()
                    .get_room_by_matrix_id(room_id)
                    .await?
                {
                    self.stores
                        .room_store()
                        .delete_room_mapping(mapping.id)
                        .await?;
                    info!("Removed bridge for room {}", room_id);
                    self.bot_intent.send_text(room_id, "Bridge removed").await?;
                }
            }
        }
        Ok(())
    }

    async fn get_or_create_portal_by_feishu_room(
        &self,
        feishu_room_id: &str,
    ) -> anyhow::Result<BridgePortal> {
        if let Some(mxid) = self
            .portals_by_feishu_room
            .read()
            .await
            .get(feishu_room_id)
            .cloned()
        {
            if let Some(portal) = self.portals_by_mxid.read().await.get(&mxid).cloned() {
                return Ok(portal);
            }
        }

        let mxid = format!(
            "!feishu_{}:{}",
            sanitize_identifier(feishu_room_id),
            self.config.bridge.domain
        );
        let name = format!("Feishu {}", feishu_room_id);
        let portal = BridgePortal::new(
            feishu_room_id.to_string(),
            mxid.clone(),
            name,
            format!(
                "@{}:{}",
                self.config.bridge.bot_username, self.config.bridge.domain
            ),
        );

        self.portals_by_mxid
            .write()
            .await
            .insert(mxid.clone(), portal.clone());
        self.portals_by_feishu_room
            .write()
            .await
            .insert(feishu_room_id.to_string(), mxid);

        Ok(portal)
    }

    async fn get_or_create_portal_by_matrix_room(
        &self,
        room_id: &str,
    ) -> anyhow::Result<BridgePortal> {
        if let Some(portal) = self.portals_by_mxid.read().await.get(room_id).cloned() {
            return Ok(portal);
        }

        let generated_feishu_room_id = format!("mx_{}", sanitize_identifier(room_id));
        let portal = BridgePortal::new(
            generated_feishu_room_id,
            room_id.to_string(),
            format!("Matrix {}", room_id),
            format!(
                "@{}:{}",
                self.config.bridge.bot_username, self.config.bridge.domain
            ),
        );

        self.portals_by_mxid
            .write()
            .await
            .insert(room_id.to_string(), portal.clone());
        Ok(portal)
    }

    fn matrix_event_to_bridge_message(
        &self,
        room_id: &str,
        event: Value,
    ) -> anyhow::Result<BridgeMessage> {
        let event_id = event
            .get("event_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let sender = event
            .get("sender")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let content = event.get("content").cloned().unwrap_or(Value::Null);

        let body = content
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let msgtype = content
            .get("msgtype")
            .and_then(Value::as_str)
            .unwrap_or("m.text");

        let msg_type = match msgtype {
            "m.text" | "m.notice" => MessageType::Text,
            "m.image" => MessageType::Image,
            "m.video" => MessageType::Video,
            "m.audio" => MessageType::Audio,
            "m.file" => MessageType::File,
            _ => MessageType::Text,
        };

        let timestamp = event
            .get("origin_server_ts")
            .and_then(Value::as_i64)
            .and_then(|value| Utc.timestamp_millis_opt(value).single())
            .unwrap_or_else(Utc::now);

        Ok(BridgeMessage {
            id: if event_id.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                event_id
            },
            sender,
            room_id: room_id.to_string(),
            content: body,
            msg_type,
            timestamp,
            attachments: vec![],
            thread_id: None,
            root_id: None,
            parent_id: None,
        })
    }

    async fn ensure_matrix_sender_joined_room(
        &self,
        sender_intent: &Intent,
        sender_mxid: &str,
        bridge_bot_mxid: &str,
        matrix_room_id: &str,
    ) -> anyhow::Result<()> {
        sender_intent.ensure_registered().await?;
        self.ensure_matrix_user_registered(sender_mxid).await?;
        println!(
            "[Feishu->Matrix Sender] ensure joined start: matrix_sender={} room_id={}",
            sender_mxid, matrix_room_id
        );

        if self
            .join_matrix_room_as_user(matrix_room_id, sender_mxid)
            .await
            .is_ok()
        {
            println!(
                "[Feishu->Matrix Sender] sender already joined or join succeeded: matrix_sender={} room_id={}",
                sender_mxid, matrix_room_id
            );
            return Ok(());
        }

        warn!(
            matrix_user_id = %sender_mxid,
            matrix_room_id = %matrix_room_id,
            "Mapped Matrix sender is not joined; inviting via bridge bot"
        );
        println!(
            "[Feishu->Matrix Sender] sender join failed initially, inviting: matrix_sender={} room_id={} inviter={}",
            sender_mxid, matrix_room_id, bridge_bot_mxid
        );

        if let Err(err) = self.bot_intent.invite_user(sender_mxid, matrix_room_id).await {
            let detail = err.to_string().to_ascii_lowercase();
            let ignorable = detail.contains("already in room")
                || detail.contains("is already joined")
                || detail.contains("already invited")
                || detail.contains("m_user_in_use");
            if !ignorable {
                anyhow::bail!(
                    "failed to invite mapped Matrix sender {} to room {} using {}: {}",
                    sender_mxid,
                    matrix_room_id,
                    bridge_bot_mxid,
                    err
                );
            }
            println!(
                "[Feishu->Matrix Sender] invite result ignorable: matrix_sender={} room_id={} err={}",
                sender_mxid, matrix_room_id, err
            );
        }

        self.join_matrix_room_as_user(matrix_room_id, sender_mxid)
            .await
            .with_context(|| {
                format!(
                    "failed to join mapped Matrix sender {} to room {} after invite",
                    sender_mxid, matrix_room_id
                )
            })?;
        println!(
            "[Feishu->Matrix Sender] sender joined after invite: matrix_sender={} room_id={}",
            sender_mxid, matrix_room_id
        );

        Ok(())
    }

    async fn ensure_matrix_user_registered(&self, matrix_user_id: &str) -> anyhow::Result<()> {
        let localpart = matrix_user_id
            .strip_prefix('@')
            .and_then(|value| value.split(':').next())
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("invalid Matrix user id for registration: {}", matrix_user_id))?;
        let endpoint = "/_matrix/client/v3/register?kind=user";
        let payload = json!({
            "type": "m.login.application_service",
            "username": localpart,
            "inhibit_login": true
        });
        let response = self
            .appservice
            .client
            .raw_json(Method::POST, endpoint, Some(payload))
            .await
            .with_context(|| format!("failed Matrix register request for {}", matrix_user_id))?;
        if let Some(errcode) = response.get("errcode").and_then(Value::as_str) {
            if errcode != "M_USER_IN_USE" {
                anyhow::bail!(
                    "Matrix register failed for {}: {}",
                    matrix_user_id,
                    response
                );
            }
            println!(
                "[Feishu->Matrix Sender] user already registered: matrix_sender={} errcode={} response={}",
                matrix_user_id, errcode, response
            );
            return Ok(());
        }
        println!(
            "[Feishu->Matrix Sender] user registered or ensured: matrix_sender={} response={}",
            matrix_user_id, response
        );
        Ok(())
    }

    async fn join_matrix_room_as_user(
        &self,
        matrix_room_id: &str,
        matrix_user_id: &str,
    ) -> anyhow::Result<String> {
        let endpoint = format!(
            "/_matrix/client/v3/rooms/{}/join?user_id={}",
            urlencoding::encode(matrix_room_id),
            urlencoding::encode(matrix_user_id)
        );
        let response = self
            .appservice
            .client
            .raw_json(Method::POST, &endpoint, None)
            .await
            .with_context(|| {
                format!(
                    "failed Matrix join request for user {} room {}",
                    matrix_user_id, matrix_room_id
                )
            })?;
        if response.get("errcode").is_some() {
            anyhow::bail!(
                "Matrix join failed for user {} room {}: {}",
                matrix_user_id,
                matrix_room_id,
                response
            );
        }
        let joined_room_id = response
            .get("room_id")
            .and_then(Value::as_str)
            .unwrap_or(matrix_room_id)
            .to_string();
        println!(
            "[Feishu->Matrix Sender] join as user ok: matrix_sender={} room_id={} response={}",
            matrix_user_id, matrix_room_id, response
        );
        Ok(joined_room_id)
    }

    async fn send_matrix_room_message_as_user(
        &self,
        matrix_user_id: &str,
        matrix_room_id: &str,
        content: Value,
    ) -> anyhow::Result<String> {
        let txn_id = Uuid::new_v4().to_string();
        let endpoint = format!(
            "/_matrix/client/v3/rooms/{}/send/{}/{}?user_id={}",
            urlencoding::encode(matrix_room_id),
            urlencoding::encode("m.room.message"),
            txn_id,
            urlencoding::encode(matrix_user_id)
        );
        println!(
            "[Feishu->Matrix Sender] send as user request: matrix_sender={} room_id={} endpoint={}",
            matrix_user_id, matrix_room_id, endpoint
        );
        let response = self
            .appservice
            .client
            .raw_json(Method::PUT, &endpoint, Some(content))
            .await
            .with_context(|| {
                format!(
                    "failed Matrix send request for user {} room {}",
                    matrix_user_id, matrix_room_id
                )
            })?;
        if response.get("errcode").is_some() {
            anyhow::bail!(
                "Matrix send failed for user {} room {}: {}",
                matrix_user_id,
                matrix_room_id,
                response
            );
        }
        let event_id = response
            .get("event_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "missing event_id in Matrix send response for user {} room {}: {}",
                    matrix_user_id,
                    matrix_room_id,
                    response
                )
            })?
            .to_string();
        println!(
            "[Feishu->Matrix Sender] send as user ok: matrix_sender={} room_id={} event_id={}",
            matrix_user_id, matrix_room_id, event_id
        );
        Ok(event_id)
    }

    async fn send_matrix_text_message(
        &self,
        matrix_sender_mxid: &str,
        matrix_room_id: &str,
        body: &str,
        reply_to_matrix_event_id: Option<&str>,
    ) -> anyhow::Result<String> {
        if let Some(reply_event_id) = reply_to_matrix_event_id {
            let content = json!({
                "msgtype": "m.text",
                "body": body,
                "m.relates_to": {
                    "m.in_reply_to": {
                        "event_id": reply_event_id
                    }
                }
            });
            return self
                .send_matrix_room_message_as_user(matrix_sender_mxid, matrix_room_id, content)
                .await;
        }

        self.send_matrix_room_message_as_user(
            matrix_sender_mxid,
            matrix_room_id,
            json!({
                "msgtype": "m.text",
                "body": body
            }),
        )
        .await
    }

    async fn forward_feishu_attachments_to_matrix(
        &self,
        intent: &Intent,
        matrix_sender_mxid: &str,
        matrix_room_id: &str,
        message: &BridgeMessage,
        reply_to_matrix_event_id: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        let mut event_ids = Vec::new();
        let mut pending_reply_target = reply_to_matrix_event_id.map(ToOwned::to_owned);

        for attachment in &message.attachments {
            let current_reply_target = pending_reply_target.as_deref();
            match self
                .forward_single_feishu_attachment(
                    intent,
                    matrix_sender_mxid,
                    matrix_room_id,
                    &message.id,
                    attachment,
                    current_reply_target,
                )
                .await
            {
                Ok(event_id) => {
                    event_ids.push(event_id);
                    pending_reply_target = None;
                }
                Err(err) => warn!(
                    "Failed to forward Feishu attachment {} for message {}: {}",
                    attachment.url, message.id, err
                ),
            }
        }

        Ok(event_ids)
    }

    async fn forward_single_feishu_attachment(
        &self,
        _intent: &Intent,
        matrix_sender_mxid: &str,
        matrix_room_id: &str,
        feishu_message_id: &str,
        attachment: &super::message::Attachment,
        reply_to_matrix_event_id: Option<&str>,
    ) -> anyhow::Result<String> {
        let (kind, key) = parse_feishu_attachment_url(&attachment.url)
            .ok_or_else(|| anyhow::anyhow!("invalid feishu attachment url: {}", attachment.url))?;

        let resource_type = feishu_resource_type_for_kind(kind)
            .ok_or_else(|| anyhow::anyhow!("unsupported feishu attachment kind '{}'", kind))?;

        let bytes = self
            .feishu_service
            .get_message_resource(feishu_message_id, key, resource_type)
            .await?;

        if self.config.bridge.max_media_size > 0 && bytes.len() > self.config.bridge.max_media_size
        {
            anyhow::bail!(
                "feishu attachment exceeds configured max_media_size: {} > {}",
                bytes.len(),
                self.config.bridge.max_media_size
            );
        }

        let mime_type = resolve_attachment_mime_type(kind, &attachment.mime_type, &bytes);
        let file_name = normalize_attachment_filename(&attachment.name, kind, key, &mime_type);
        let msgtype = matrix_msgtype_for_kind_and_mime(kind, &mime_type);
        debug!(
            feishu_message_id = %feishu_message_id,
            attachment_kind = %kind,
            declared_mime = %attachment.mime_type,
            resolved_mime = %mime_type,
            file_name = %file_name,
            matrix_msgtype = %msgtype,
            bytes = bytes.len(),
            "Prepared Feishu attachment for Matrix upload"
        );

        let mxc = self
            .upload_matrix_media(bytes.clone(), &mime_type, &file_name)
            .await?;

        let mut content = json!({
            "msgtype": msgtype,
            "body": file_name,
            "filename": file_name,
            "url": mxc,
            "info": {
                "mimetype": mime_type,
                "size": bytes.len() as u64
            }
        });
        if let Some(reply_event_id) = reply_to_matrix_event_id {
            content["m.relates_to"] = json!({
                "m.in_reply_to": {
                    "event_id": reply_event_id
                }
            });
        }

        self.send_matrix_room_message_as_user(matrix_sender_mxid, matrix_room_id, content)
            .await
    }

    async fn upload_matrix_media(
        &self,
        bytes: Vec<u8>,
        content_type: &str,
        file_name: &str,
    ) -> anyhow::Result<String> {
        let homeserver = self.config.bridge.homeserver_url.trim_end_matches('/');
        let endpoint = format!(
            "{}/_matrix/media/v3/upload?filename={}",
            homeserver,
            urlencoding::encode(file_name)
        );
        let client = build_media_http_client(homeserver)?;
        let response = client
            .post(&endpoint)
            .bearer_auth(&self.config.registration.as_token)
            .header(CONTENT_TYPE, content_type)
            .body(bytes)
            .send()
            .await
            .with_context(|| format!("failed to upload media to Matrix endpoint {}", endpoint))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .context("failed to read Matrix media upload response body")?;
        if !status.is_success() {
            anyhow::bail!(
                "Matrix media upload failed: status={} body={}",
                status,
                String::from_utf8_lossy(&body)
            );
        }

        let payload: Value = serde_json::from_slice(&body).with_context(|| {
            format!(
                "invalid Matrix media upload response JSON: {}",
                String::from_utf8_lossy(&body)
            )
        })?;
        let content_uri = payload
            .get("content_uri")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("missing content_uri in Matrix media upload response"))?;
        if !content_uri.starts_with("mxc://") {
            anyhow::bail!(
                "unexpected Matrix content_uri format: {} (payload={})",
                content_uri,
                payload
            );
        }
        debug!(
            endpoint = %endpoint,
            content_type = %content_type,
            file_name = %file_name,
            content_uri = %content_uri,
            "Uploaded Feishu attachment to Matrix media API"
        );
        Ok(content_uri.to_string())
    }
}

fn sanitize_identifier(input: &str) -> String {
    let sanitized = input
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
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

fn parse_feishu_attachment_url(url: &str) -> Option<(&str, &str)> {
    let stripped = url.strip_prefix("feishu://")?;
    let mut parts = stripped.splitn(2, '/');
    let kind = parts.next()?;
    let key = parts.next()?;
    if kind.is_empty() || key.is_empty() {
        return None;
    }
    Some((kind, key))
}

fn feishu_resource_type_for_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "image" | "sticker" => Some("image"),
        "file" => Some("file"),
        "audio" => Some("audio"),
        "video" => Some("media"),
        _ => None,
    }
}

fn matrix_msgtype_for_kind_and_mime(kind: &str, mime_type: &str) -> &'static str {
    match kind {
        "image" | "sticker" if mime_type.starts_with("image/") => "m.image",
        "audio" if mime_type.starts_with("audio/") => "m.audio",
        "video" if mime_type.starts_with("video/") => "m.video",
        _ => "m.file",
    }
}

fn default_mime_for_kind(kind: &str) -> &'static str {
    match kind {
        "image" | "sticker" => "image/png",
        "audio" => "audio/ogg",
        "video" => "video/mp4",
        _ => "application/octet-stream",
    }
}

fn resolve_attachment_mime_type(kind: &str, declared_mime: &str, bytes: &[u8]) -> String {
    let declared = normalize_declared_mime(declared_mime);
    let sniffed = sniff_mime_from_bytes(bytes);

    if let Some(sniffed) = sniffed {
        if mime_matches_kind(kind, sniffed) || declared.is_none() {
            return sniffed.to_string();
        }
    }

    if let Some(declared) = declared {
        if mime_matches_kind(kind, declared) || kind == "file" {
            return declared.to_string();
        }
    }

    default_mime_for_kind(kind).to_string()
}

fn normalize_declared_mime(declared_mime: &str) -> Option<&str> {
    let trimmed = declared_mime.trim();
    if trimmed.is_empty() || trimmed.ends_with("/*") {
        None
    } else {
        Some(trimmed)
    }
}

fn mime_matches_kind(kind: &str, mime_type: &str) -> bool {
    match kind {
        "image" | "sticker" => mime_type.starts_with("image/"),
        "audio" => mime_type.starts_with("audio/"),
        "video" => mime_type.starts_with("video/"),
        _ => true,
    }
}

fn sniff_mime_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8
        && bytes[0] == 0x89
        && bytes[1] == b'P'
        && bytes[2] == b'N'
        && bytes[3] == b'G'
        && bytes[4] == 0x0D
        && bytes[5] == 0x0A
        && bytes[6] == 0x1A
        && bytes[7] == 0x0A
    {
        return Some("image/png");
    }
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return Some("image/jpeg");
    }
    if bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.len() >= 2 && bytes[0] == b'B' && bytes[1] == b'M' {
        return Some("image/bmp");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        return Some("audio/wav");
    }
    if bytes.len() >= 4 && &bytes[..4] == b"OggS" {
        return Some("audio/ogg");
    }
    if bytes.len() >= 3 && &bytes[..3] == b"ID3" {
        return Some("audio/mpeg");
    }
    if bytes.len() >= 2
        && bytes[0] == 0xFF
        && (bytes[1] & 0xE0) == 0xE0
        && ((bytes[1] & 0x18) != 0x08)
    {
        return Some("audio/mpeg");
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return Some("video/mp4");
    }
    if bytes.len() >= 4 && &bytes[..4] == b"%PDF" {
        return Some("application/pdf");
    }
    None
}

fn default_attachment_filename(kind: &str, key: &str, mime_type: &str) -> String {
    let ext = extension_for_mime(mime_type);
    format!("{}_{}.{}", kind, key, ext)
}

fn normalize_attachment_filename(name: &str, kind: &str, key: &str, mime_type: &str) -> String {
    let trimmed = name.trim();
    let ext = extension_for_mime(mime_type);
    if trimmed.is_empty() {
        return default_attachment_filename(kind, key, mime_type);
    }

    let has_ext = trimmed
        .rsplit_once('.')
        .map(|(_, suffix)| !suffix.trim().is_empty() && suffix.len() <= 10)
        .unwrap_or(false);
    if has_ext {
        return trimmed.to_string();
    }

    format!("{}.{}", trimmed, ext)
}

fn extension_for_mime(mime_type: &str) -> &'static str {
    match mime_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "audio/ogg" => "ogg",
        "audio/wav" => "wav",
        "audio/mpeg" => "mp3",
        "video/mp4" => "mp4",
        "application/pdf" => "pdf",
        _ => "bin",
    }
}

fn room_type_from_chat_type(chat_type: Option<&str>) -> RoomType {
    match chat_type {
        Some("p2p") | Some("private") | Some("single") => RoomType::Direct,
        _ => RoomType::Group,
    }
}

fn build_media_http_client(homeserver: &str) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();
    if is_local_homeserver(homeserver) {
        builder = builder.no_proxy();
    }
    builder
        .build()
        .context("failed to build Matrix media upload HTTP client")
}

fn is_local_homeserver(homeserver: &str) -> bool {
    let parsed = match Url::parse(homeserver) {
        Ok(url) => url,
        Err(_) => return false,
    };
    let host = match parsed.host_str() {
        Some(host) => host.to_ascii_lowercase(),
        None => return false,
    };
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

#[handler]
async fn health_handler(res: &mut Response) {
    res.status_code(StatusCode::OK);
    res.render(Json(serde_json::json!({
        "status": "ok",
        "timestamp": chrono::Utc::now().to_rfc3339()
    })));
}

#[handler]
async fn ready_handler(res: &mut Response) {
    res.status_code(StatusCode::OK);
    res.render(Json(serde_json::json!({ "ready": true })));
}

#[derive(Clone)]
struct BridgeStatusState {
    room_store: Arc<dyn RoomStore>,
    started_at: Instant,
}

#[handler]
async fn status_handler(depot: &mut Depot, res: &mut Response) {
    let state: &BridgeStatusState = depot.obtain().unwrap();
    match state.room_store.count_rooms().await {
        Ok(bridged_rooms) => {
            res.status_code(StatusCode::OK);
            res.render(Json(serde_json::json!({
                "status": "running",
                "version": env!("CARGO_PKG_VERSION"),
                "uptime_seconds": state.started_at.elapsed().as_secs(),
                "bridged_rooms": bridged_rooms,
            })));
        }
        Err(err) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(serde_json::json!({
                "status": "degraded",
                "error": err.to_string(),
            })));
        }
    }
}

struct BridgeHandler {
    bridge: FeishuBridge,
    event_processor: Arc<MatrixEventProcessor>,
}

impl BridgeHandler {
    fn configured_bot_mxid(&self) -> String {
        format!(
            "@{}:{}",
            self.bridge.config.bridge.bot_username, self.bridge.config.bridge.domain
        )
    }

    async fn maybe_join_bot_on_invite(&self, event: &Value) {
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if event_type != "m.room.member" {
            return;
        }

        let membership = event
            .pointer("/content/membership")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if membership != "invite" {
            return;
        }

        let room_id = event
            .get("room_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let sender = event
            .get("sender")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let state_key = event
            .get("state_key")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let event_id = event
            .get("event_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let configured_bot = self.configured_bot_mxid();

        println!(
            "[matrix-invite-debug] event_id={} room_id={} sender={} state_key={} membership={} configured_bot={}",
            event_id, room_id, sender, state_key, membership, configured_bot
        );

        if state_key != configured_bot {
            println!(
                "[matrix-invite-debug] skip auto-join: invite target {} != configured bot {}; check config.bridge.bot_username vs registration sender_localpart",
                state_key, configured_bot
            );
            return;
        }

        if room_id.is_empty() {
            println!("[matrix-invite-debug] skip auto-join: empty room_id");
            return;
        }

        match self.bridge.bot_intent.join_room(room_id).await {
            Ok(joined_room_id) => {
                println!(
                    "[matrix-invite-debug] auto-join succeeded: bot={} room_id={} joined_room_id={}",
                    configured_bot, room_id, joined_room_id
                );
                info!(
                    bot_user = %configured_bot,
                    room_id = %room_id,
                    joined_room_id = %joined_room_id,
                    sender = %sender,
                    event_id = %event_id,
                    "Auto-joined Matrix room after invite"
                );
            }
            Err(err) => {
                println!(
                    "[matrix-invite-debug] auto-join failed: bot={} room_id={} error={}",
                    configured_bot, room_id, err
                );
                warn!(
                    bot_user = %configured_bot,
                    room_id = %room_id,
                    sender = %sender,
                    event_id = %event_id,
                    error = %err,
                    "Failed to auto-join Matrix room after invite"
                );
            }
        }
    }
}

#[async_trait]
impl AppserviceHandler for BridgeHandler {
    async fn on_transaction(&self, _txn_id: &str, body: &Value) -> anyhow::Result<()> {
        let events = body
            .get("events")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        for event in events {
            let event_type = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();

            let room_id = event
                .get("room_id")
                .and_then(Value::as_str)
                .unwrap_or_default();

            let sender = event
                .get("sender")
                .and_then(Value::as_str)
                .unwrap_or_default();

            self.maybe_join_bot_on_invite(&event).await;

            let matrix_event = MatrixEvent {
                event_id: event
                    .get("event_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                event_type: event_type.to_string(),
                room_id: room_id.to_string(),
                sender: sender.to_string(),
                state_key: event
                    .get("state_key")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                content: event.get("content").cloned(),
                timestamp: event.get("origin_server_ts").map(|v| v.to_string()),
            };

            if let Err(err) = self.event_processor.process_event(matrix_event).await {
                error!("Failed to process Matrix event in {}: {}", room_id, err);
            }
        }

        Ok(())
    }

    async fn query_user(&self, user_id: &str) -> anyhow::Result<Option<Value>> {
        info!("Query user: {}", user_id);

        let localpart = user_id
            .strip_prefix('@')
            .and_then(|s| s.split(':').next())
            .unwrap_or(user_id);

        if localpart.starts_with(
            &self
                .bridge
                .config
                .bridge
                .username_template
                .replace("{{.}}", ""),
        ) {
            return Ok(Some(json!({
                "displayname": localpart,
            })));
        }

        Ok(None)
    }

    async fn query_room_alias(&self, room_alias: &str) -> anyhow::Result<Option<Value>> {
        info!("Query room alias: {}", room_alias);

        let localpart = room_alias
            .strip_prefix('#')
            .and_then(|s| s.split(':').next())
            .unwrap_or(room_alias);

        if localpart.starts_with("feishu_") {
            return Ok(Some(json!({
                "name": format!("Feishu {}", localpart),
                "topic": "Bridged from Feishu",
                "preset": "private_chat",
                "visibility": "private",
            })));
        }

        Ok(None)
    }

    async fn thirdparty_protocol(&self, _protocol: &str) -> anyhow::Result<Option<Value>> {
        Ok(Some(json!({
            "user_fields": ["id", "name"],
            "location_fields": ["id", "name"],
            "icon": "mxc://example.org/feishu",
            "field_types": {
                "id": {
                    "regexp": ".*",
                    "placeholder": "Feishu ID"
                },
                "name": {
                    "regexp": ".*",
                    "placeholder": "Display name"
                }
            },
            "instances": [{
                "network_id": "feishu",
                "bot_user_id": format!("@{}:{}", self.bridge.config.bridge.bot_username, self.bridge.config.bridge.domain),
                "desc": "Feishu",
                "icon": "mxc://example.org/feishu",
                "fields": {}
            }]
        })))
    }

    async fn thirdparty_user_remote(
        &self,
        _protocol: &str,
        _fields: &HashMap<String, String>,
    ) -> anyhow::Result<Vec<Value>> {
        Ok(Vec::new())
    }

    async fn thirdparty_user_matrix(&self, _user_id: &str) -> anyhow::Result<Vec<Value>> {
        Ok(Vec::new())
    }

    async fn thirdparty_location_remote(
        &self,
        _protocol: &str,
        _fields: &HashMap<String, String>,
    ) -> anyhow::Result<Vec<Value>> {
        Ok(Vec::new())
    }

    async fn thirdparty_location_matrix(&self, _alias: &str) -> anyhow::Result<Vec<Value>> {
        Ok(Vec::new())
    }
}
