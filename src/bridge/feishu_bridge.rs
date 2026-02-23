use chrono::{TimeZone, Utc};
use salvo::prelude::*;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

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
    _users_by_mxid: Arc<RwLock<HashMap<String, BridgeUser>>>,
    portals_by_mxid: Arc<RwLock<HashMap<String, BridgePortal>>>,
    portals_by_feishu_room: Arc<RwLock<HashMap<String, String>>>,
    _puppets: Arc<RwLock<HashMap<String, BridgePuppet>>>,
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

        Ok(Self {
            config,
            db,
            feishu_service,
            _users_by_mxid: Arc::new(RwLock::new(HashMap::new())),
            portals_by_mxid: Arc::new(RwLock::new(HashMap::new())),
            portals_by_feishu_room: Arc::new(RwLock::new(HashMap::new())),
            _puppets: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub async fn start(&self) -> anyhow::Result<()> {
        info!("Starting Feishu bridge");

        let service = self.feishu_service.clone();
        let bridge_clone = self.clone();
        tokio::spawn(async move {
            if let Err(e) = service.start(bridge_clone).await {
                error!("Feishu service error: {}", e);
            }
        });

        let appservice_router = self.create_appservice_router();
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

        Server::new(acceptor).serve(appservice_router).await;
        Ok(())
    }

    pub async fn stop(&self) {
        info!("Stopping Feishu bridge");
    }

    fn create_appservice_router(&self) -> Router {
        let handler = MatrixRequestHandler {
            bridge: self.clone(),
        };

        Router::new()
            .hoop(Logger::new())
            .push(Router::with_path("/_matrix/app/{*path}").put(handler))
            .push(Router::with_path("/health").get(bridge_health))
    }

    async fn handle_matrix_request(&self, req: &mut Request, res: &mut Response) {
        if !self.is_authorized_request(req).await {
            res.status_code(StatusCode::UNAUTHORIZED);
            res.render("unauthorized");
            return;
        }

        let path = req.uri().path().to_string();
        let is_transaction = path.contains("/transactions/");
        if !is_transaction {
            res.status_code(StatusCode::OK);
            res.render("{}");
            return;
        }

        let payload = match req.payload().await {
            Ok(bytes) => bytes,
            Err(err) => {
                error!("Failed to read appservice payload: {}", err);
                res.status_code(StatusCode::BAD_REQUEST);
                res.render("invalid payload");
                return;
            }
        };

        let body: Value = match serde_json::from_slice(payload) {
            Ok(body) => body,
            Err(err) => {
                error!("Failed to parse appservice JSON body: {}", err);
                res.status_code(StatusCode::BAD_REQUEST);
                res.render("invalid json");
                return;
            }
        };

        let mut processed = 0usize;
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

            if let Err(err) = self.handle_matrix_message(&room_id, event).await {
                error!("Failed to process Matrix event in {}: {}", room_id, err);
                continue;
            }
            processed += 1;
        }

        res.status_code(StatusCode::OK);
        res.render(format!("{{\"processed\":{}}}", processed));
    }

    async fn is_authorized_request(&self, req: &Request) -> bool {
        let tokens = [
            &self.config.appservice.hs_token,
            &self.config.appservice.as_token,
        ];

        if let Some(query_token) = req.query::<String>("access_token") {
            if tokens.iter().any(|token| query_token == **token) {
                return true;
            }
        }

        if let Some(auth) = req.header::<String>("Authorization") {
            let token = auth
                .strip_prefix("Bearer ")
                .or_else(|| auth.strip_prefix("bearer "))
                .unwrap_or(auth.as_str());
            if tokens.iter().any(|item| token == item.as_str()) {
                return true;
            }
        }

        false
    }

    pub async fn handle_feishu_message(&self, message: BridgeMessage) -> anyhow::Result<()> {
        info!(
            "Handling Feishu message {} in room {}",
            message.id, message.room_id
        );

        let portal = self
            .get_or_create_portal_by_feishu_room(&message.room_id)
            .await?;
        portal.handle_feishu_message(message)?;
        Ok(())
    }

    pub async fn handle_matrix_message(
        &self,
        room_id: &str,
        event: serde_json::Value,
    ) -> anyhow::Result<()> {
        info!("Handling Matrix message in room {}", room_id);

        let message = self.matrix_event_to_bridge_message(room_id, event)?;
        let portal = self.get_or_create_portal_by_matrix_room(room_id).await?;
        portal.handle_matrix_message(message.clone())?;

        // If a Matrix room has not yet been mapped to a real Feishu room, we skip sending.
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

struct MatrixRequestHandler {
    bridge: FeishuBridge,
}

#[async_trait]
impl Handler for MatrixRequestHandler {
    async fn handle(
        &self,
        req: &mut Request,
        _depot: &mut Depot,
        res: &mut Response,
        _ctrl: &mut FlowCtrl,
    ) {
        self.bridge.handle_matrix_request(req, res).await;
    }
}

#[handler]
async fn bridge_health(res: &mut Response) {
    res.status_code(StatusCode::OK);
    res.render("OK");
}
