use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct FeishuPresence {
    pub user_id: String,
    pub status: FeishuPresenceStatus,
    pub status_message: Option<String>,
    pub last_seen: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeishuPresenceStatus {
    Online,
    Offline,
    Busy,
    Away,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixPresenceState {
    Online,
    Offline,
    Unavailable,
}

#[async_trait]
pub trait MatrixPresenceTarget: Send + Sync {
    async fn set_presence(
        &self,
        feishu_user_id: &str,
        presence: MatrixPresenceState,
        status_message: &str,
    ) -> anyhow::Result<()>;
    async fn ensure_user_registered(
        &self,
        feishu_user_id: &str,
        username: Option<&str>,
    ) -> anyhow::Result<()>;
}

pub struct PresenceHandler {
    queue: Arc<Mutex<VecDeque<FeishuPresence>>>,
    batch_size: usize,
}

impl PresenceHandler {
    pub fn new(batch_size: Option<usize>) -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            batch_size: batch_size.unwrap_or(50),
        }
    }

    pub fn enqueue_user(&self, presence: FeishuPresence) {
        let queue = self.queue.clone();
        tokio::spawn(async move {
            let mut q = queue.lock().await;
            if q.len() >= 1000 {
                q.pop_front();
            }
            q.push_back(presence);
        });
    }

    pub async fn process_next<T: MatrixPresenceTarget + ?Sized>(
        &self,
        target: &T,
    ) -> anyhow::Result<()> {
        let batch: Vec<FeishuPresence> = {
            let mut q = self.queue.lock().await;
            let mut batch = Vec::with_capacity(self.batch_size);
            for _ in 0..self.batch_size {
                if let Some(p) = q.pop_front() {
                    batch.push(p);
                } else {
                    break;
                }
            }
            batch
        };

        for presence in batch {
            let matrix_presence = match presence.status {
                FeishuPresenceStatus::Online => MatrixPresenceState::Online,
                FeishuPresenceStatus::Offline => MatrixPresenceState::Offline,
                FeishuPresenceStatus::Busy | FeishuPresenceStatus::Away => {
                    MatrixPresenceState::Unavailable
                }
            };

            let status_msg = presence.status_message.unwrap_or_default();

            if let Err(e) = target
                .set_presence(&presence.user_id, matrix_presence, &status_msg)
                .await
            {
                tracing::warn!("Failed to set presence for {}: {}", presence.user_id, e);
            }
        }

        Ok(())
    }
}

impl Default for PresenceHandler {
    fn default() -> Self {
        Self::new(None)
    }
}
