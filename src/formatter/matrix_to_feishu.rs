use serde_json::{Value, json};

use crate::bridge::message::{BridgeMessage, MessageType};
use crate::feishu::types::FeishuRichText;

pub fn format_matrix_to_feishu(message: BridgeMessage) -> Result<String, anyhow::Error> {
    let content = match message.msg_type {
        MessageType::Text => convert_matrix_text_to_feishu(&message.content),
        MessageType::Markdown => convert_matrix_markdown_to_feishu(&message.content),
        MessageType::RichText => create_feishu_rich_text(&message.content),
        MessageType::Image => {
            format!("[Image: {}]", message.content)
        }
        MessageType::Video => {
            format!("[Video: {}]", message.content)
        }
        MessageType::Audio => {
            format!("[Audio: {}]", message.content)
        }
        MessageType::File => {
            format!("[File: {}]", message.content)
        }
        MessageType::Card => {
            format!("[Card: {}]", message.content)
        }
    };

    Ok(content)
}

pub fn convert_matrix_text_to_feishu(content: &str) -> String {
    // Convert Matrix text formatting to Feishu compatible format
    let converted = content
        // Remove HTML tags
        .replace("<b>", "")
        .replace("</b>", "")
        .replace("<i>", "")
        .replace("</i>", "")
        .replace("<u>", "")
        .replace("</u>", "")
        .replace("<s>", "")
        .replace("</s>", "")
        .replace("<code>", "")
        .replace("</code>", "")
        .replace("<pre>", "")
        .replace("</pre>", "")
        // Convert some common formatting
        .replace("**", "*")
        .replace("__", "*")
        // Convert mentions (@user:domain.com -> @user)
        .replace(':', "");

    // Extract mentions and convert them to Feishu format
    extract_matrix_mentions(&converted)
}

pub fn convert_matrix_markdown_to_feishu(content: &str) -> String {
    // Convert Markdown to rich text for Feishu
    let converted = content
        .replace("# ", "")
        .replace("## ", "")
        .replace("### ", "")
        .replace("- ", "• ")
        .replace("* ", "• ")
        .replace("**", "")
        .replace("__", "")
        .replace("`", "");

    convert_matrix_text_to_feishu(&converted)
}

pub fn create_feishu_rich_text(content: &str) -> String {
    // Feishu `post` payload with lightweight mention/link extraction.
    let mut row = Vec::new();
    for token in content.split_whitespace() {
        if token.starts_with('@') && token.len() > 1 {
            row.push(json!({
                "tag": "at",
                "user_name": token.trim_start_matches('@')
            }));
            row.push(json!({ "tag": "text", "text": " " }));
            continue;
        }

        if token.starts_with("http://") || token.starts_with("https://") {
            row.push(json!({
                "tag": "a",
                "text": token,
                "href": token
            }));
            row.push(json!({ "tag": "text", "text": " " }));
            continue;
        }

        row.push(json!({ "tag": "text", "text": token }));
        row.push(json!({ "tag": "text", "text": " " }));
    }

    if row.is_empty() {
        row.push(json!({ "tag": "text", "text": content }));
    }

    json!({
        "zh_cn": {
            "title": "",
            "content": [row]
        }
    })
    .to_string()
}

pub fn extract_matrix_mentions(content: &str) -> String {
    let mut result = content.to_string();

    // Find Matrix mentions (@user:domain.com) and convert to @user
    let mention_regex =
        regex::Regex::new(r"@([a-zA-Z0-9._%+-]+):[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}").unwrap();

    result = mention_regex.replace_all(&result, "@$1").to_string();

    result
}

pub fn convert_matrix_html_to_feishu(html: &str) -> String {
    // Basic HTML to text conversion for Feishu
    let mut result = html.to_string();

    // Remove common HTML tags but preserve content
    result = result.replace("<p>", "").replace("</p>", "\n");
    result = result.replace("<br>", "\n").replace("<br/>", "\n");
    result = result.replace("<div>", "").replace("</div>", "\n");
    result = result.replace("<span>", "").replace("</span>", "");
    result = result.replace("<strong>", "").replace("</strong>", "");
    result = result.replace("<em>", "").replace("</em>", "");
    result = result.replace("<b>", "").replace("</b>", "");
    result = result.replace("<i>", "").replace("</i>", "");
    result = result.replace("<u>", "").replace("</u>", "");
    result = result.replace("<s>", "").replace("</s>", "");
    result = result.replace("<del>", "").replace("</del>", "");

    // Handle links
    let link_regex = regex::Regex::new(r#"<a[^>]+href="([^"]+)"[^>]*>([^<]+)</a>"#).unwrap();
    result = link_regex.replace_all(&result, "$2 ($1)").to_string();

    // Handle images
    let img_regex = regex::Regex::new(r#"<img[^>]+src="([^"]+)"[^>]+alt="([^"]*)"[^>]*>"#).unwrap();
    result = img_regex.replace_all(&result, "[Image: $2]").to_string();

    // Handle code blocks
    let code_regex = regex::Regex::new(r#"<code[^>]*>([^<]+)</code>"#).unwrap();
    result = code_regex.replace_all(&result, "`$1`").to_string();

    let pre_regex = regex::Regex::new(r#"<pre[^>]*>([^<]+)</pre>"#).unwrap();
    result = pre_regex.replace_all(&result, "```\n$1\n```").to_string();

    // Clean up extra whitespace
    result = result
        .lines()
        .map(|line| line.trim())
        .collect::<Vec<_>>()
        .join("\n");

    convert_matrix_text_to_feishu(&result)
}

pub fn convert_matrix_emoticons(content: &str) -> String {
    // Convert Matrix emoticons to Feishu compatible format
    content
        .replace("😊", "[微笑]")
        .replace("😄", "[哈哈]")
        .replace("👍", "[赞]")
        .replace("🤝", "[握手]")
        .replace("🙏", "[抱拳]")
        .replace("💪", "[加油]")
        .replace("🎉", "[庆祝]")
        .replace("💐", "[鲜花]")
        .replace("❤️", "[爱心]")
        .replace("🔥", "[强]")
}

pub fn create_feishu_text_message(content: &str) -> Value {
    json!({
        "text": content
    })
}

pub fn create_feishu_rich_text_message(rich_text: &FeishuRichText) -> Value {
    json!(rich_text)
}

pub fn create_feishu_card_message(title: &str, content: &str) -> Value {
    json!({
        "card": {
            "config": {
                "wide_screen_mode": true
            },
            "elements": [
                {
                    "tag": "div",
                    "text": {
                        "content": content,
                        "tag": "lark_md"
                    }
                }
            ],
            "header": {
                "title": {
                    "content": title,
                    "tag": "plain_text"
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::{
        convert_matrix_emoticons, convert_matrix_html_to_feishu, convert_matrix_text_to_feishu,
        create_feishu_rich_text, extract_matrix_mentions,
    };

    #[test]
    fn convert_matrix_text_to_feishu_strips_html_and_mentions() {
        let input = "<b>@alice:example.com</b> says <i>hello</i>";
        let converted = convert_matrix_text_to_feishu(input);
        assert!(converted.contains("@alice"));
        assert!(!converted.contains("<b>"));
        assert!(!converted.contains(":example.com"));
    }

    #[test]
    fn create_feishu_rich_text_extracts_mentions_and_links() {
        let rich = create_feishu_rich_text("@alice check https://example.com");
        let parsed: Value = serde_json::from_str(&rich).expect("valid rich text json");
        let content = parsed
            .pointer("/zh_cn/content/0")
            .and_then(Value::as_array)
            .expect("content row should exist");
        let tags: Vec<String> = content
            .iter()
            .filter_map(|item| item.get("tag").and_then(Value::as_str))
            .map(ToOwned::to_owned)
            .collect();
        assert!(tags.contains(&"at".to_string()));
        assert!(tags.contains(&"a".to_string()));
    }

    #[test]
    fn convert_matrix_html_to_feishu_keeps_link_text_and_url() {
        let html = r#"<p>Hello <a href="https://example.com">example</a></p>"#;
        let converted = convert_matrix_html_to_feishu(html);
        assert!(converted.contains("example (https//example.com)"));
    }

    #[test]
    fn extract_matrix_mentions_normalizes_matrix_user_ids() {
        let mentions = extract_matrix_mentions("ping @bob:example.com and @carol:example.net");
        assert_eq!(mentions, "ping @bob and @carol");
    }

    #[test]
    fn convert_matrix_emoticons_maps_common_unicode() {
        let converted = convert_matrix_emoticons("Great 😊 👍");
        assert_eq!(converted, "Great [微笑] [赞]");
    }
}
