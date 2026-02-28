use std::sync::Arc;
use std::time::Instant;

use chrono::{Duration, Utc};
use salvo::affix_state;
use salvo::oapi::extract::JsonBody;
use salvo::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::bridge::{FeishuBridge, PendingBridgeRequest, ProvisioningCoordinator};
use crate::database::{DeadLetterStore, RoomStore};

#[derive(Clone)]
pub struct ProvisioningApi {
    room_store: Arc<dyn RoomStore>,
    dead_letter_store: Arc<dyn DeadLetterStore>,
    bridge: FeishuBridge,
    provisioning: Arc<ProvisioningCoordinator>,
    token: String,
    admin_token: String,
    started_at: Instant,
}

impl ProvisioningApi {
    pub fn new(
        room_store: Arc<dyn RoomStore>,
        dead_letter_store: Arc<dyn DeadLetterStore>,
        bridge: FeishuBridge,
        provisioning: Arc<ProvisioningCoordinator>,
        token: String,
        admin_token: String,
        started_at: Instant,
    ) -> Self {
        Self {
            room_store,
            dead_letter_store,
            bridge,
            provisioning,
            token,
            admin_token,
            started_at,
        }
    }

    pub fn router(self) -> Router {
        Router::new()
            .push(Router::with_path("status").get(provisioning_status))
            .push(Router::with_path("bridge").post(create_bridge))
            .push(Router::with_path("bridge/<room_id>").delete(delete_bridge))
            .push(
                Router::with_path("bridges")
                    .get(list_bridges)
                    .post(create_bridge),
            )
            .push(Router::with_path("mappings").get(list_mappings))
            .push(Router::with_path("bridges/<room_id>").delete(delete_bridge))
            .push(Router::with_path("pending").get(list_pending))
            .push(Router::with_path("dead-letters").get(list_dead_letters))
            .push(Router::with_path("dead-letters/<id>/replay").post(replay_dead_letter))
            .push(Router::with_path("dead-letters/replay").post(replay_dead_letters))
            .push(Router::with_path("dead-letters/cleanup").post(cleanup_dead_letters))
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

#[derive(Debug, Serialize)]
struct ProvisioningStatusResponse {
    status: String,
    version: String,
    uptime_seconds: u64,
    bridged_rooms: i64,
    pending_requests: usize,
    dead_letters: DeadLetterStatusSummary,
}

#[derive(Debug, Serialize)]
struct DeadLetterStatusSummary {
    pending: i64,
    failed: i64,
    replayed: i64,
    total: i64,
}

#[handler]
async fn provisioning_status(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let api: &ProvisioningApi = depot.obtain().unwrap();
    let Some(actor) = require_auth(req, api, false, res) else {
        return;
    };

    info!(action = "status", actor = %actor, "Provisioning status requested");

    let bridged_rooms = match api.room_store.count_rooms().await {
        Ok(value) => value,
        Err(err) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(serde_json::json!({
                "success": false,
                "message": err.to_string()
            })));
            return;
        }
    };

    let pending_requests = api.provisioning.get_pending_requests().await.len();
    let pending_dead_letters = api
        .dead_letter_store
        .count_dead_letters(Some("pending"))
        .await;
    let failed_dead_letters = api
        .dead_letter_store
        .count_dead_letters(Some("failed"))
        .await;
    let replayed_dead_letters = api
        .dead_letter_store
        .count_dead_letters(Some("replayed"))
        .await;
    let total_dead_letters = api.dead_letter_store.count_dead_letters(None).await;
    let dead_letters = match (
        pending_dead_letters,
        failed_dead_letters,
        replayed_dead_letters,
        total_dead_letters,
    ) {
        (Ok(pending), Ok(failed), Ok(replayed), Ok(total)) => DeadLetterStatusSummary {
            pending,
            failed,
            replayed,
            total,
        },
        _ => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(serde_json::json!({
                "success": false,
                "message": "failed to collect dead-letter counters"
            })));
            return;
        }
    };

    let body = ProvisioningStatusResponse {
        status: "running".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        uptime_seconds: api.started_at.elapsed().as_secs(),
        bridged_rooms,
        pending_requests,
        dead_letters,
    };
    res.render(Json(body));
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
    let (limit, offset) = normalized_pagination(req, 100);
    info!(
        action = "list_bridges",
        actor = %actor,
        limit = ?limit,
        offset = ?offset,
        "Provisioning list bridges"
    );

    match api.room_store.list_room_mappings(limit, offset).await {
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

#[handler]
async fn list_mappings(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let api: &ProvisioningApi = depot.obtain().unwrap();
    let Some(actor) = require_auth(req, api, false, res) else {
        return;
    };
    let (limit, offset) = normalized_pagination(req, 100);
    info!(
        action = "list_mappings",
        actor = %actor,
        limit = ?limit,
        offset = ?offset,
        "Provisioning list mappings"
    );

    match api.room_store.list_room_mappings(limit, offset).await {
        Ok(mappings) => {
            res.render(Json(serde_json::json!({
                "mappings": mappings,
                "count": mappings.len()
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

#[derive(Debug, Serialize)]
struct DeadLetterListResponse {
    dead_letters: Vec<crate::database::DeadLetterEvent>,
    count: usize,
}

#[handler]
async fn list_dead_letters(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let api: &ProvisioningApi = depot.obtain().unwrap();
    let Some(actor) = require_auth(req, api, false, res) else {
        return;
    };

    let status = req.query::<String>("status");
    let limit = req.query::<i64>("limit");
    let offset = req.query::<i64>("offset");
    info!(
        action = "list_dead_letters",
        actor = %actor,
        status = ?status,
        limit = ?limit,
        offset = ?offset,
        "Provisioning list dead letters"
    );

    match api
        .dead_letter_store
        .list_dead_letters(status.as_deref(), limit, offset)
        .await
    {
        Ok(dead_letters) => {
            let count = dead_letters.len();
            res.render(Json(DeadLetterListResponse {
                dead_letters,
                count,
            }));
        }
        Err(err) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(serde_json::json!({
                "success": false,
                "message": err.to_string()
            })));
        }
    }
}

#[handler]
async fn replay_dead_letter(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let api: &ProvisioningApi = depot.obtain().unwrap();
    let Some(actor) = require_auth(req, api, true, res) else {
        return;
    };

    let id = req.param::<i64>("id").unwrap_or_default();
    info!(
        action = "replay_dead_letter",
        actor = %actor,
        dead_letter_id = id,
        "Provisioning replay dead letter requested"
    );

    match api.bridge.replay_dead_letter(id).await {
        Ok(()) => {
            res.render(Json(serde_json::json!({
                "success": true,
                "message": "dead-letter replayed",
                "id": id
            })));
        }
        Err(err) => {
            res.status_code(StatusCode::BAD_REQUEST);
            res.render(Json(serde_json::json!({
                "success": false,
                "message": err.to_string(),
                "id": id
            })));
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReplayDeadLettersRequest {
    ids: Option<Vec<i64>>,
    status: Option<String>,
    limit: Option<i64>,
}

#[derive(Debug, Serialize)]
struct ReplayDeadLettersResponse {
    success: bool,
    requested: usize,
    replayed: usize,
    failed: usize,
    failures: Vec<ReplayDeadLetterFailure>,
}

#[derive(Debug, Serialize)]
struct ReplayDeadLetterFailure {
    id: i64,
    error: String,
}

#[handler]
async fn replay_dead_letters(
    req: &mut Request,
    body: JsonBody<ReplayDeadLettersRequest>,
    depot: &mut Depot,
    res: &mut Response,
) {
    let api: &ProvisioningApi = depot.obtain().unwrap();
    let Some(actor) = require_auth(req, api, true, res) else {
        return;
    };

    let ids = if let Some(ids) = &body.ids {
        ids.clone()
    } else {
        let status = body.status.as_deref().unwrap_or("pending");
        let limit = body.limit.unwrap_or(20).max(1);
        match api
            .dead_letter_store
            .list_dead_letters(Some(status), Some(limit), Some(0))
            .await
        {
            Ok(items) => items.into_iter().map(|item| item.id).collect(),
            Err(err) => {
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                res.render(Json(serde_json::json!({
                    "success": false,
                    "message": err.to_string(),
                })));
                return;
            }
        }
    };
    info!(
        action = "replay_dead_letters",
        actor = %actor,
        requested = ids.len(),
        "Provisioning replay dead letters batch requested"
    );

    let mut replayed = 0usize;
    let mut failures = Vec::new();
    for id in ids.iter().copied() {
        match api.bridge.replay_dead_letter(id).await {
            Ok(()) => replayed += 1,
            Err(err) => failures.push(ReplayDeadLetterFailure {
                id,
                error: err.to_string(),
            }),
        }
    }

    let response = ReplayDeadLettersResponse {
        success: failures.is_empty(),
        requested: ids.len(),
        replayed,
        failed: failures.len(),
        failures,
    };
    if !response.success {
        res.status_code(StatusCode::BAD_REQUEST);
    }
    res.render(Json(response));
}

#[derive(Debug, Deserialize)]
struct DeadLetterCleanupRequest {
    status: Option<String>,
    older_than_hours: Option<i64>,
    limit: Option<i64>,
    dry_run: Option<bool>,
}

#[derive(Debug, Serialize)]
struct DeadLetterCleanupResponse {
    success: bool,
    status: Option<String>,
    older_than_hours: Option<i64>,
    limit: i64,
    dry_run: bool,
    matched: usize,
    deleted: u64,
    candidate_ids: Vec<i64>,
}

#[handler]
async fn cleanup_dead_letters(
    req: &mut Request,
    body: JsonBody<DeadLetterCleanupRequest>,
    depot: &mut Depot,
    res: &mut Response,
) {
    let api: &ProvisioningApi = depot.obtain().unwrap();
    let Some(actor) = require_auth(req, api, true, res) else {
        return;
    };

    let status = body.status.clone();
    let older_than_hours = body.older_than_hours.filter(|value| *value > 0);
    let limit = body.limit.unwrap_or(200).max(1);
    let dry_run = body.dry_run.unwrap_or(false);
    let older_than = older_than_hours.map(|hours| Utc::now() - Duration::hours(hours));

    info!(
        action = "cleanup_dead_letters",
        actor = %actor,
        status = ?status,
        older_than_hours = ?older_than_hours,
        limit = limit,
        dry_run = dry_run,
        "Provisioning dead-letter cleanup requested"
    );

    if dry_run {
        match api
            .dead_letter_store
            .list_dead_letters(status.as_deref(), Some(limit), Some(0))
            .await
        {
            Ok(items) => {
                let candidates: Vec<i64> = items
                    .into_iter()
                    .filter(|item| older_than.is_none_or(|boundary| item.updated_at < boundary))
                    .map(|item| item.id)
                    .collect();
                let response = DeadLetterCleanupResponse {
                    success: true,
                    status,
                    older_than_hours,
                    limit,
                    dry_run,
                    matched: candidates.len(),
                    deleted: 0,
                    candidate_ids: candidates,
                };
                res.render(Json(response));
            }
            Err(err) => {
                res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
                res.render(Json(serde_json::json!({
                    "success": false,
                    "message": err.to_string(),
                })));
            }
        }
        return;
    }

    match api
        .dead_letter_store
        .cleanup_dead_letters(status.as_deref(), older_than, Some(limit))
        .await
    {
        Ok(deleted) => {
            let response = DeadLetterCleanupResponse {
                success: true,
                status,
                older_than_hours,
                limit,
                dry_run: false,
                matched: deleted as usize,
                deleted,
                candidate_ids: Vec::new(),
            };
            res.render(Json(response));
        }
        Err(err) => {
            res.status_code(StatusCode::INTERNAL_SERVER_ERROR);
            res.render(Json(serde_json::json!({
                "success": false,
                "message": err.to_string(),
            })));
        }
    }
}

fn require_auth(
    req: &Request,
    api: &ProvisioningApi,
    admin_required: bool,
    res: &mut Response,
) -> Option<String> {
    let provided = extract_access_token(req);
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

    let is_authorized = if admin_required {
        token == api.admin_token
    } else {
        token == api.token || token == api.admin_token
    };
    if !is_authorized {
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

fn normalized_pagination(req: &Request, default_limit: i64) -> (Option<i64>, Option<i64>) {
    let limit = req.query::<i64>("limit").map(|value| value.max(1));
    let offset = req.query::<i64>("offset").map(|value| value.max(0));
    (
        Some(limit.unwrap_or(default_limit.max(1))),
        Some(offset.unwrap_or(0)),
    )
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
