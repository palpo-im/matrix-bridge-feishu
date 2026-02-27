use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

pub struct ProvisioningCoordinator {
    pending_requests: Arc<Mutex<HashMap<String, PendingBridgeRequest>>>,
    request_timeout: Duration,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingBridgeRequest {
    pub feishu_chat_id: String,
    pub matrix_room_id: String,
    pub matrix_requestor: String,
    pub created_at: DateTime<Utc>,
    pub status: BridgeRequestStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BridgeRequestStatus {
    Pending,
    Approved,
    Declined,
    Expired,
}

#[derive(Debug, Clone)]
pub enum ProvisioningError {
    TimedOut,
    Declined,
    AlreadyExists,
    NotFound,
    Other(String),
}

impl std::fmt::Display for ProvisioningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProvisioningError::TimedOut => write!(f, "bridge request timed out"),
            ProvisioningError::Declined => write!(f, "bridge request was declined"),
            ProvisioningError::AlreadyExists => write!(f, "bridge already exists"),
            ProvisioningError::NotFound => write!(f, "bridge request not found"),
            ProvisioningError::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for ProvisioningError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalResponseStatus {
    Applied,
    Expired,
}

impl ProvisioningCoordinator {
    pub fn new(timeout_seconds: u64) -> Self {
        Self {
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            request_timeout: Duration::from_secs(timeout_seconds),
        }
    }

    pub async fn request_bridge(
        &self,
        feishu_chat_id: &str,
        matrix_room_id: &str,
        matrix_requestor: &str,
    ) -> Result<(), ProvisioningError> {
        let mut requests = self.pending_requests.lock().await;

        if requests.contains_key(feishu_chat_id) {
            return Err(ProvisioningError::AlreadyExists);
        }

        let request = PendingBridgeRequest {
            feishu_chat_id: feishu_chat_id.to_string(),
            matrix_room_id: matrix_room_id.to_string(),
            matrix_requestor: matrix_requestor.to_string(),
            created_at: Utc::now(),
            status: BridgeRequestStatus::Pending,
        };

        info!(
            "Created bridge request: chat={} room={} requestor={}",
            feishu_chat_id, matrix_room_id, matrix_requestor
        );

        requests.insert(feishu_chat_id.to_string(), request);
        Ok(())
    }

    pub async fn wait_for_approval(&self, feishu_chat_id: &str) -> Result<(), ProvisioningError> {
        let timeout = self.request_timeout;
        let start = std::time::Instant::now();

        loop {
            let status = {
                let requests = self.pending_requests.lock().await;
                if let Some(request) = requests.get(feishu_chat_id) {
                    request.status
                } else {
                    return Err(ProvisioningError::NotFound);
                }
            };

            match status {
                BridgeRequestStatus::Approved => {
                    self.remove_request(feishu_chat_id).await;
                    return Ok(());
                }
                BridgeRequestStatus::Declined => {
                    self.remove_request(feishu_chat_id).await;
                    return Err(ProvisioningError::Declined);
                }
                BridgeRequestStatus::Expired => {
                    self.remove_request(feishu_chat_id).await;
                    return Err(ProvisioningError::TimedOut);
                }
                BridgeRequestStatus::Pending => {}
            }

            if start.elapsed() > timeout {
                self.mark_expired(feishu_chat_id).await;
                return Err(ProvisioningError::TimedOut);
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    pub async fn mark_approval(
        &self,
        feishu_chat_id: &str,
        approved: bool,
    ) -> ApprovalResponseStatus {
        let mut requests = self.pending_requests.lock().await;

        if let Some(request) = requests.get_mut(feishu_chat_id) {
            if request.status != BridgeRequestStatus::Pending {
                return ApprovalResponseStatus::Expired;
            }

            request.status = if approved {
                BridgeRequestStatus::Approved
            } else {
                BridgeRequestStatus::Declined
            };

            info!(
                "Bridge request {} for chat {}",
                if approved { "approved" } else { "declined" },
                feishu_chat_id
            );

            ApprovalResponseStatus::Applied
        } else {
            ApprovalResponseStatus::Expired
        }
    }

    async fn mark_expired(&self, feishu_chat_id: &str) {
        let mut requests = self.pending_requests.lock().await;
        if let Some(request) = requests.get_mut(feishu_chat_id) {
            request.status = BridgeRequestStatus::Expired;
            warn!("Bridge request for chat {} expired", feishu_chat_id);
        }
    }

    async fn remove_request(&self, feishu_chat_id: &str) {
        let mut requests = self.pending_requests.lock().await;
        requests.remove(feishu_chat_id);
    }

    pub async fn cleanup_expired(&self) -> usize {
        let mut requests = self.pending_requests.lock().await;
        let now = Utc::now();
        let timeout_secs = self.request_timeout.as_secs() as i64;

        let expired_keys: Vec<String> = requests
            .iter()
            .filter(|(_, req)| {
                (now - req.created_at).num_seconds() > timeout_secs
                    && req.status == BridgeRequestStatus::Pending
            })
            .map(|(k, _)| k.clone())
            .collect();

        let count = expired_keys.len();
        for key in expired_keys {
            if let Some(req) = requests.get_mut(&key) {
                req.status = BridgeRequestStatus::Expired;
            }
        }

        if count > 0 {
            debug!("Marked {} expired bridge requests", count);
        }

        count
    }

    pub async fn get_pending_requests(&self) -> Vec<PendingBridgeRequest> {
        let requests = self.pending_requests.lock().await;
        requests
            .values()
            .filter(|r| r.status == BridgeRequestStatus::Pending)
            .cloned()
            .collect()
    }
}

impl Default for ProvisioningCoordinator {
    fn default() -> Self {
        Self::new(300)
    }
}
