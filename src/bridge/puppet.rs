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

    pub fn apply_profile_sync(
        &mut self,
        displayname: Option<&str>,
        avatar_url: Option<&str>,
    ) -> bool {
        let mut changed = false;

        if let Some(displayname) = displayname {
            let normalized = displayname.trim();
            if !normalized.is_empty() && self.displayname != normalized {
                self.displayname = normalized.to_string();
                self.name_set = true;
                changed = true;
            }
        }

        let normalized_avatar = avatar_url
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        if self.avatar_url != normalized_avatar {
            self.avatar_url = normalized_avatar;
            self.avatar_set = self.avatar_url.is_some();
            changed = true;
        }

        changed
    }
}
