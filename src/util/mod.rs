use std::collections::HashMap;

use regex::Regex;

pub struct UidGenerator {
    cache: HashMap<String, String>,
    regex: Regex,
}

#[derive(Debug, Clone)]
pub struct FeishuApiErrorFields {
    pub api: String,
    pub code: String,
    pub msg: String,
    pub retryable: bool,
}

impl UidGenerator {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
            regex: Regex::new(r"[^a-zA-Z0-9._-]").unwrap(),
        }
    }

    pub fn generate_mxid(&mut self, feishu_id: &str, domain: &str) -> String {
        if let Some(cached) = self.cache.get(feishu_id) {
            return format!("@{}:{}", cached, domain);
        }

        let sanitized = self.sanitize_username(feishu_id);
        let final_id = format!("feishu_{}", sanitized);

        self.cache.insert(feishu_id.to_string(), final_id.clone());

        format!("@{}:{}", final_id, domain)
    }

    fn sanitize_username(&self, username: &str) -> String {
        let sanitized = self.regex.replace_all(username, "_");
        let mut result = sanitized.to_string();

        result = result.trim_matches('_').trim_matches('.').to_string();

        if let Some(first_char) = result.chars().next() {
            if first_char.is_numeric() {
                result = format!("user_{}", result);
            }
        }

        if result.len() > 64 {
            result.truncate(64);
        }

        if result.is_empty() {
            result = "unknown".to_string();
        }

        result
    }

    pub fn is_feishu_mxid(&self, mxid: &str) -> bool {
        mxid.starts_with("@feishu_")
    }
}

impl Default for UidGenerator {
    fn default() -> Self {
        Self::new()
    }
}

pub fn parse_feishu_api_error(api: &str, err: &anyhow::Error) -> FeishuApiErrorFields {
    let message = err.to_string();
    let code = capture_error_field(&message, "code=").unwrap_or_else(|| "unknown".to_string());
    let msg = capture_error_field(&message, "msg=").unwrap_or_else(|| message.clone());
    let retryable = capture_error_field(&message, "retryable=")
        .map(|value| value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    FeishuApiErrorFields {
        api: api.to_string(),
        code,
        msg,
        retryable,
    }
}

fn capture_error_field(message: &str, marker: &str) -> Option<String> {
    let start = message.find(marker)?;
    let value = &message[start + marker.len()..];
    let trimmed = value.trim_start();
    let end = trimmed
        .find(|ch: char| ch == ' ' || ch == ',' || ch == '}')
        .unwrap_or(trimmed.len());
    let raw = trimmed[..end].trim_matches('"').trim();
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}
