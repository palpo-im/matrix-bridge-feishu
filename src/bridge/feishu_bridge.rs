use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use matrix_bot_sdk::appservice::{Appservice, AppserviceHandler, Intent};
use matrix_bot_sdk::client::{MatrixAuth, MatrixClient};
use salvo::prelude::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use url::Url;

use crate::config::Config;
use crate::database::Database;
use crate::feishu::FeishuService;
use crate::formatter;

use super::message::{BridgeMessage, MessageType};
use super::{portal::BridgePortal, puppet::BridgePuppet, user::BridgeUser};

#[derive(Clone)]
pub struct FeishuBridge {
    pub config: Config,
    pub db: Database,
    pub feishu_service: Arc<FeishuService>,
    pub appservice: Arc<Appservice>,
    pub bot_intent: Intent,
    _users_by_mxid: Arc<RwLock<HashMap<String, BridgeUser>>>,
    portals_by_mxid: Arc<RwLock<HashMap<String, BridgePortal>>>,
    portals_by_feishu_room: Arc<RwLock<HashMap<String, String>>>,
    _puppets: Arc<RwLock<HashMap<String, BridgePuppet>>>,
    intents: Arc<RwLock<HashMap<String, Intent>>>,
}

impl FeishuBridge {
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let db_type = &config.appservice.database.r#type;
        let db_uri = &config.appservice.database.uri;
        let max_open = config.appservice.database.max_open_conns;
        let max_idle = config.appservice.database.max_idle_conns;

        let db = Database::connect(db_type, db_uri, max_open, max_idle).await?;
        db.run_migrations().await?;

        let feishu_service = Arc::new(FeishuService::new(
            config.bridge.app_id.clone(),
            config.bridge.app_secret.clone(),
            config.bridge.listen_address.clone(),
            config.bridge.listen_secret.clone(),
            config.bridge.encrypt_key.clone(),
            config.bridge.verification_token.clone(),
        ));

        let homeserver_url = Url::parse(&config.homeserver.address)?;
        let bot_mxid = format!(
            "@{}:{}",
            config.appservice.bot.username, config.homeserver.domain
        );

        let client = MatrixClient::new(
            homeserver_url,
            MatrixAuth::new(&config.appservice.as_token).with_user_id(&bot_mxid),
        );

        let appservice = Appservice::new(
            config.appservice.hs_token.clone(),
            config.appservice.as_token.clone(),
            client,
        )
        .with_appservice_id(&config.appservice.id)
        .with_protocols(["feishu"]);

        let bot_intent = Intent::new(&bot_mxid, appservice.client.clone());

        Ok(Self {
            config,
            db,
            feishu_service,
            appservice: Arc::new(appservice),
            bot_intent,
            _users_by_mxid: Arc::new(RwLock::new(HashMap::new())),
            portals_by_mxid: Arc::new(RwLock::new(HashMap::new())),
            portals_by_feishu_room: Arc::new(RwLock::new(HashMap::new())),
            _puppets: Arc::new(RwLock::new(HashMap::new())),
            intents: Arc::new(RwLock::new(HashMap::new())),
        })
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

        let handler = Arc::new(BridgeHandler {
            bridge: self.clone(),
        });

        let appservice_with_handler = Appservice::new(
            self.config.appservice.hs_token.clone(),
            self.config.appservice.as_token.clone(),
            self.appservice.client.clone(),
        )
        .with_appservice_id(&self.config.appservice.id)
        .with_protocols(["feishu"])
        .with_handler(handler);

        let router = appservice_with_handler.router();

        let acceptor = TcpListener::new(format!(
            "{}:{}",
            self.config.appservice.hostname, self.config.appservice.port
        ))
        .bind()
        .await;

        info!(
            "Appservice listening on {}:{}",
            self.config.appservice.hostname, self.config.appservice.port
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
        self.intents.write().await.insert(user_id.to_string(), intent.clone());
        intent
    }

    pub async fn handle_feishu_message(&self, message: BridgeMessage) -> anyhow::Result<()> {
        info!(
            "Handling Feishu message {} in room {}",
            message.id, message.room_id
        );

        let portal = self
            .get_or_create_portal_by_feishu_room(&message.room_id)
            .await?;

        let intent = self.get_or_create_intent(&portal.bridge_info.bridgebot).await;
        intent.ensure_registered().await?;

        let matrix_text = formatter::convert_feishu_content_to_matrix_html(&message.content);
        intent.send_text(&portal.mxid, &matrix_text).await?;

        Ok(())
    }

    pub async fn handle_matrix_message(
        &self,
        room_id: &str,
        event: Value,
    ) -> anyhow::Result<()> {
        info!("Handling Matrix message in room {}", room_id);

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
            self.config.homeserver.domain
        );
        let name = format!("Feishu {}", feishu_room_id);
        let portal = BridgePortal::new(
            feishu_room_id.to_string(),
            mxid.clone(),
            name,
            format!(
                "@{}:{}",
                self.config.appservice.bot.username, self.config.homeserver.domain
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
                self.config.appservice.bot.username, self.config.homeserver.domain
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
        })
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

struct BridgeHandler {
    bridge: FeishuBridge,
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

            if event_type != "m.room.message" {
                continue;
            }

            let room_id = match event.get("room_id").and_then(Value::as_str) {
                Some(room_id) => room_id.to_string(),
                None => continue,
            };

            if let Err(err) = self.bridge.handle_matrix_message(&room_id, event).await {
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

        if localpart.starts_with(&self.bridge.config.bridge.username_template.replace("{{.}}", "")) {
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
                "bot_user_id": format!("@{}:{}", self.bridge.config.appservice.bot.username, self.bridge.config.homeserver.domain),
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
