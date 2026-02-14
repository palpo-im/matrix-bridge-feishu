use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{info, error, debug};
use salvo::prelude::*;

use crate::config::Config;
use crate::database::Database;
use crate::feishu::FeishuService;

pub struct FeishuBridge {
    pub config: Config,
    pub db: Database,
    pub feishu_service: Arc<FeishuService>,
    
    users_by_mxid: RwLock<HashMap<String, super::user::BridgeUser>>,
    portals_by_mxid: RwLock<HashMap<String, super::portal::BridgePortal>>,
    puppets: RwLock<HashMap<String, super::puppet::BridgePuppet>>,
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
            users_by_mxid: RwLock::new(HashMap::new()),
            portals_by_mxid: RwLock::new(HashMap::new()),
            puppets: RwLock::new(HashMap::new()),
        })
    }

    pub async fn start(&self) -> anyhow::Result<()> {
        info!("Starting Feishu bridge");
        
        // Start Feishu service
        let service = self.feishu_service.clone();
        let bridge_clone = self.clone();
        tokio::spawn(async move {
            if let Err(e) = service.start(bridge_clone).await {
                error!("Feishu service error: {}", e);
            }
        });
        
        // Start Matrix appservice server
        let appservice_router = self.create_appservice_router();
        let acceptor = TcpListener::new(&format!("{}:{}", 
            self.config.appservice.hostname, 
            self.config.appservice.port)).bind().await;
        
        info!("Appservice listening on {}:{}", 
            self.config.appservice.hostname, 
            self.config.appservice.port);
        
        Server::new(acceptor).serve(appservice_router).await;
        
        info!("Feishu bridge started");
        Ok(())
    }

    pub async fn stop(&self) {
        info!("Stopping Feishu bridge");
    }

    fn create_appservice_router(&self) -> Router {
        Router::new()
            .hoop(Logger::new())
            .hoop(Cors::new())
            .push(
                Router::with_path("/_matrix/app/{*path}")
                    .handle(self.handle_matrix_request())
            )
            .push(
                Router::with_path("/feishu/webhook")
                    .handle(self.handle_feishu_webhook())
            )
            .push(
                Router::with_path("/health")
                    .handle(self.health_check())
            )
    }

    #[handler]
    async fn handle_matrix_request(&self, req: &mut Request, res: &mut Response) {
        info!("Received Matrix request: {} {}", req.method(), req.uri());
        
        // TODO: Implement Matrix appservice request handling
        
        res.status_code(StatusCode::OK);
        res.render("{}");
    }

    #[handler]
    async fn handle_feishu_webhook(&self, req: &mut Request, res: &mut Response) {
        info!("Received Feishu webhook: {} {}", req.method(), req.uri());
        
        // TODO: Implement Feishu webhook handling
        
        res.status_code(StatusCode::OK);
        res.render("success");
    }

    #[handler]
    async fn health_check(&self, _res: &mut Response) -> &'static str {
        "OK"
    }

    pub async fn handle_feishu_message(&self, message: super::message::BridgeMessage) -> anyhow::Result<()> {
        info!("Handling Feishu message from {}: {}", message.sender, message.content);
        
        // TODO: Process Feishu message and bridge to Matrix
        
        Ok(())
    }

    pub async fn handle_matrix_message(&self, room_id: &str, event: serde_json::Value) -> anyhow::Result<()> {
        info!("Handling Matrix message in room {}: {}", room_id, event);
        
        // TODO: Process Matrix message and bridge to Feishu
        
        Ok(())
    }
}

impl Clone for FeishuBridge {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            db: self.db.clone(),
            feishu_service: self.feishu_service.clone(),
            users_by_mxid: RwLock::new(HashMap::new()),
            portals_by_mxid: RwLock::new(HashMap::new()),
            puppets: RwLock::new(HashMap::new()),
        }
    }
}