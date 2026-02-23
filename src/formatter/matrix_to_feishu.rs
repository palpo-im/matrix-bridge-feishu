use crate::bridge::message::{BridgeMessage, MessageType};
use crate::feishu::types::FeishuRichText;
use serde_json::{json, Value};

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
    // Create rich text content for Feishu
    let rich_text = FeishuRichText {
        title: None,
        content: vec![crate::feishu::types::FeishuRichTextElement {
            segment_type: "text".to_string(),
            content: crate::feishu::types::FeishuRichTextContent {
                text: Some(content.to_string()),
                link: None,
                mention: None,
                image: None,
            },
        }],
    };

    serde_json::to_string(&rich_text).unwrap_or_default()
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
