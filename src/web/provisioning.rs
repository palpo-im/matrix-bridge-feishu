use std::sync::Arc;

use salvo::affix_state;
use salvo::oapi::extract::JsonBody;
use salvo::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::bridge::{PendingBridgeRequest, ProvisioningCoordinator};
use crate::database::RoomStore;

#[derive(Clone)]
pub struct ProvisioningApi {
    room_store: Arc<dyn RoomStore>,
    provisioning: Arc<ProvisioningCoordinator>,
    token: String,
    admin_token: String,
}

impl ProvisioningApi {
    pub fn new(
        room_store: Arc<dyn RoomStore>,
        provisioning: Arc<ProvisioningCoordinator>,
        token: String,
        admin_token: String,
    ) -> Self {
        Self {
            room_store,
            provisioning,
            token,
            admin_token,
        }
    }

    pub fn router(self) -> Router {
        Router::new()
            .push(Router::with_path("bridge").post(create_bridge))
            .push(Router::with_path("bridge/<room_id>").delete(delete_bridge))
            .push(
                Router::with_path("bridges")
                    .get(list_bridges)
                    .post(create_bridge),
            )
            .push(Router::with_path("bridges/<room_id>").delete(delete_bridge))
            .push(Router::with_path("pending").get(list_pending))
            .hoop(affix_state::inject(self))
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BridgeRequest {
    pub matrix_room_id: String,
    pub feishu_chat_id: String,
    pub requestor: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BridgeResponse {
    pub success: bool,
    pub message: String,
}

#[handler]
async fn create_bridge(
    raw_req: &mut Request,
    req: JsonBody<BridgeRequest>,
    depot: &mut Depot,
    res: &mut Response,
) {
    let api: &ProvisioningApi = depot.obtain().unwrap();
    let Some(actor) = require_auth(raw_req, api, false, res) else {
        return;
    };
    info!(
        action = "create_bridge",
        actor = %actor,
        chat_id = %req.feishu_chat_id,
        matrix_room_id = %req.matrix_room_id,
        "Provisioning request received"
    );

    match api
        .provisioning
        .request_bridge(&req.feishu_chat_id, &req.matrix_room_id, &req.requestor)
        .await
    {
        Ok(()) => {
            res.status_code(StatusCode::CREATED);
            res.render(Json(BridgeResponse {
                success: true,
                message: "Bridge request created".to_string(),
            }));
        }
        Err(e) => {
            res.status_code(StatusCode::BAD_REQUEST);
            warn!(
                action = "create_bridge",
                actor = %actor,
                chat_id = %req.feishu_chat_id,
                matrix_room_id = %req.matrix_room_id,
                error = %e,
                "Provisioning request failed"
            );
            res.render(Json(BridgeResponse {
                success: false,
                message: e.to_string(),
            }));
        }
    }
}

#[handler]
async fn delete_bridge(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let api: &ProvisioningApi = depot.obtain().unwrap();
    let Some(actor) = require_auth(req, api, true, res) else {
        return;
    };
    let room_id = req.param::<String>("room_id").unwrap_or_default();
    info!(
        action = "delete_bridge",
        actor = %actor,
        matrix_room_id = %room_id,
        "Provisioning delete requested"
    );

    match api.room_store.get_room_by_matrix_id(&room_id).await {
        Ok(Some(mapping)) => {
            if let Err(e) = api.room_store.delete_room_mapping(mapping.id).await {
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                warn!(
                    action = "delete_bridge",
                    actor = %actor,
                    matrix_room_id = %room_id,
                    chat_id = %mapping.feishu_chat_id,
                    error = %e,
                    "Provisioning delete failed"
                );
                res.render(Json(BridgeResponse {
                    success: false,
                    message: format!("Failed to delete bridge: {}", e),
                }));
                return;
            }
            info!(
                action = "delete_bridge",
                actor = %actor,
                matrix_room_id = %room_id,
                chat_id = %mapping.feishu_chat_id,
                "Provisioning delete applied"
            );
            res.render(Json(BridgeResponse {
                success: true,
                message: "Bridge deleted".to_string(),
            }));
        }
        Ok(None) => {
            res.status_code(StatusCode::NOT_FOUND);
            res.render(Json(BridgeResponse {
                success: false,
                message: "Bridge not found".to_string(),
            }));
        }
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(BridgeResponse {
                success: false,
                message: format!("Database error: {}", e),
            }));
        }
    }
}

#[handler]
async fn list_bridges(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let api: &ProvisioningApi = depot.obtain().unwrap();
    let Some(actor) = require_auth(req, api, false, res) else {
        return;
    };
    info!(action = "list_bridges", actor = %actor, "Provisioning list bridges");

    match api.room_store.list_room_mappings(Some(100), None).await {
        Ok(bridges) => {
            res.render(Json(serde_json::json!({
                "bridges": bridges,
                "count": bridges.len()
            })));
        }
        Err(e) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(serde_json::json!({
                "error": e.to_string()
            })));
        }
    }
}

#[derive(Debug, Serialize)]
struct PendingResponse {
    pending: Vec<PendingBridgeRequestJson>,
    count: usize,
}

#[derive(Debug, Serialize)]
struct PendingBridgeRequestJson {
    pub feishu_chat_id: String,
    pub matrix_room_id: String,
    pub matrix_requestor: String,
    pub created_at: String,
    pub status: String,
}

impl From<PendingBridgeRequest> for PendingBridgeRequestJson {
    fn from(req: PendingBridgeRequest) -> Self {
        Self {
            feishu_chat_id: req.feishu_chat_id,
            matrix_room_id: req.matrix_room_id,
            matrix_requestor: req.matrix_requestor,
            created_at: req.created_at.to_rfc3339(),
            status: format!("{:?}", req.status),
        }
    }
}

#[handler]
async fn list_pending(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let api: &ProvisioningApi = depot.obtain().unwrap();
    let Some(actor) = require_auth(req, api, false, res) else {
        return;
    };
    info!(action = "list_pending", actor = %actor, "Provisioning list pending");

    let pending: Vec<PendingBridgeRequestJson> = api
        .provisioning
        .get_pending_requests()
        .await
        .into_iter()
        .map(|r| r.into())
        .collect();

    let count = pending.len();
    res.render(Json(PendingResponse { pending, count }));
}

fn require_auth(
    req: &Request,
    api: &ProvisioningApi,
    admin_required: bool,
    res: &mut Response,
) -> Option<String> {
    let provided = extract_access_token(req);
    let expected = if admin_required {
        api.admin_token.as_str()
    } else {
        api.token.as_str()
    };

    let Some(token) = provided else {
        warn!(
            action = if admin_required { "admin_auth" } else { "auth" },
            "Provisioning request missing auth token"
        );
        res.status_code(StatusCode::UNAUTHORIZED);
        res.render(Json(BridgeResponse {
            success: false,
            message: "missing authorization token".to_string(),
        }));
        return None;
    };

    if token != expected {
        warn!(
            action = if admin_required { "admin_auth" } else { "auth" },
            "Provisioning request token mismatch"
        );
        res.status_code(StatusCode::UNAUTHORIZED);
        res.render(Json(BridgeResponse {
            success: false,
            message: "invalid authorization token".to_string(),
        }));
        return None;
    }

    Some(resolve_actor(req, &token))
}

fn extract_access_token(req: &Request) -> Option<String> {
    if let Some(auth_header) = req.header::<String>("Authorization") {
        if let Some(token) = auth_header.strip_prefix("Bearer ") {
            return Some(token.trim().to_string());
        }
    }
    req.query::<String>("access_token")
}

fn resolve_actor(req: &Request, token: &str) -> String {
    if let Some(actor) = req.header::<String>("X-Actor") {
        let actor = actor.trim();
        if !actor.is_empty() {
            return actor.to_string();
        }
    }

    let suffix: String = token
        .chars()
        .rev()
        .take(6)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("token:{}", suffix)
}
