use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgePuppet {
    pub feishu_id: String,
    pub mxid: String,
    pub displayname: String,
    pub avatar_url: Option<String>,
    pub access_token: Option<String>,
    pub custom_mxid: Option<String>,
    pub next_batch: Option<String>,
    pub base_url: Option<String>,
    pub is_online: bool,
    pub name_set: bool,
    pub avatar_set: bool,
}

impl BridgePuppet {
    pub fn new(feishu_id: String, mxid: String, displayname: String) -> Self {
        Self {
            feishu_id,
            mxid,
            displayname,
            avatar_url: None,
            access_token: None,
            custom_mxid: None,
            next_batch: None,
            base_url: None,
            is_online: false,
            name_set: false,
            avatar_set: false,
        }
    }

    pub async fn start(&mut self, _feishu_service: &crate::feishu::FeishuService) -> Result<()> {
        self.is_online = true;
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<()> {
        self.is_online = false;
        Ok(())
    }
}
