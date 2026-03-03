use crate::bridge::message::{BridgeMessage, MessageType};
use crate::feishu::types::FeishuMessage;

pub fn format_feishu_to_matrix(message: FeishuMessage) -> BridgeMessage {
    let (content, msg_type, attachments) = match message.msg_type.as_str() {
        "text" => {
            let text_content = message.content.text.unwrap_or_default();
            (text_content, MessageType::Text, vec![])
        }
        "rich_text" => {
            let text_content = extract_text_from_rich_text(&message.content.rich_text);
            (text_content, MessageType::RichText, vec![])
        }
        "image" => {
            let key = message.content.image_key.unwrap_or_default();
            let content = if key.is_empty() {
                "[Image]".to_string()
            } else {
                format!("[Image:{}]", key)
            };
            (content, MessageType::Image, vec![])
        }
        "file" => {
            let key = message.content.file_key.unwrap_or_default();
            let content = if key.is_empty() {
                "[File]".to_string()
            } else {
                format!("[File:{}]", key)
            };
            (content, MessageType::File, vec![])
        }
        "audio" => {
            let key = message.content.audio_key.unwrap_or_default();
            let content = if key.is_empty() {
                "[Audio]".to_string()
            } else {
                format!("[Audio:{}]", key)
            };
            (content, MessageType::Audio, vec![])
        }
        "video" => {
            let key = message.content.video_key.unwrap_or_default();
            let content = if key.is_empty() {
                "[Video]".to_string()
            } else {
                format!("[Video:{}]", key)
            };
            (content, MessageType::Video, vec![])
        }
        "card" => {
            let text_content = extract_text_from_card(&message.content.card);
            (text_content, MessageType::Card, vec![])
        }
        _ => {
            let content = format!("[Unsupported: {}]", message.msg_type);
            (content, MessageType::Text, vec![])
        }
    };

    BridgeMessage {
        id: message.message_id,
        sender: message.sender_id,
        room_id: message.chat_id,
        content,
        msg_type,
        timestamp: message.create_time,
        attachments,
        thread_id: message.thread_id,
        root_id: message.root_id,
        parent_id: message.parent_id,
    }
}

fn extract_text_from_rich_text(rich_text: &Option<crate::feishu::types::FeishuRichText>) -> String {
    if let Some(rich) = rich_text {
        let mut text_parts = Vec::new();

        for element in &rich.content {
            match element.segment_type.as_str() {
                "text" => {
                    if let Some(text) = &element.content.text {
                        text_parts.push(text.clone());
                    }
                }
                "mention" => {
                    if let Some(mention) = &element.content.mention {
                        text_parts.push(format!("@{}", mention.name));
                    }
                }
                "link" => {
                    if let Some(link) = &element.content.link {
                        text_parts.push(link.clone());
                    }
                }
                _ => {}
            }
        }

        text_parts.join("")
    } else {
        String::new()
    }
}

fn extract_text_from_card(card: &Option<crate::feishu::types::FeishuCard>) -> String {
    if let Some(card) = card {
        let mut text_parts = Vec::new();

        if let Some(header) = &card.header {
            text_parts.push(header.title.clone());
            if let Some(subtitle) = &header.subtitle {
                text_parts.push(subtitle.clone());
            }
        }

        for element in &card.elements {
            match element.tag.as_str() {
                "div" => {
                    if let Some(text) = &element.text {
                        text_parts.push(text.content.clone());
                    }
                }
                "button" => {
                    if let Some(button) = &element.button {
                        text_parts.push(format!("{} ({})", button.text.content, button.url));
                    }
                }
                "img" | "image" => {
                    if let Some(image) = &element.image {
                        text_parts.push(
                            image
                                .alt
                                .clone()
                                .unwrap_or_else(|| format!("[Image:{}]", image.img_key)),
                        );
                    }
                }
                _ => {}
            }
        }

        text_parts.join(" ")
    } else {
        String::new()
    }
}

pub fn convert_feishu_content_to_matrix_html(content: &str) -> String {
    // Convert Feishu specific formatting to Matrix HTML
    let html = content
        .replace("@", "<font color=\"#2e8b57\">@</font>")
        .replace("#", "<font color=\"#ff6347\">#</font>");

    format!("<message>{}</message>", html)
}

pub fn extract_mentions_from_rich_text(
    rich_text: &crate::feishu::types::FeishuRichText,
) -> Vec<String> {
    let mut mentions = Vec::new();

    for element in &rich_text.content {
        if element.segment_type == "mention" {
            if let Some(mention) = &element.content.mention {
                if let Some(user_id) = &mention.user_id {
                    mentions.push(user_id.clone());
                }
            }
        }
    }

    mentions
}

pub fn extract_links_from_rich_text(
    rich_text: &crate::feishu::types::FeishuRichText,
) -> Vec<String> {
    let mut links = Vec::new();

    for element in &rich_text.content {
        if element.segment_type == "link" {
            if let Some(link) = &element.content.link {
                links.push(link.clone());
            }
        }
    }

    links
}

pub fn convert_feishu_emoticons(content: &str) -> String {
    // Convert Feishu emoticons to Unicode or Matrix-compatible emoticons
    content
        .replace("[微笑]", "😊")
        .replace("[哈哈]", "😄")
        .replace("[赞]", "👍")
        .replace("[握手]", "🤝")
        .replace("[抱拳]", "🙏")
        .replace("[加油]", "💪")
        .replace("[庆祝]", "🎉")
        .replace("[鲜花]", "💐")
        .replace("[爱心]", "❤️")
        .replace("[强]", "💪")
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::{
        convert_feishu_emoticons, extract_links_from_rich_text, extract_mentions_from_rich_text,
        format_feishu_to_matrix,
    };
    use crate::bridge::message::MessageType;
    use crate::feishu::types::{
        FeishuMention, FeishuMessage, FeishuMessageContent, FeishuRichText, FeishuRichTextContent,
        FeishuRichTextElement,
    };

    #[test]
    fn extract_mentions_and_links_from_rich_text() {
        let rich = FeishuRichText {
            title: None,
            content: vec![
                FeishuRichTextElement {
                    segment_type: "mention".to_string(),
                    content: FeishuRichTextContent {
                        text: None,
                        link: None,
                        mention: Some(FeishuMention {
                            user_id: Some("ou_123".to_string()),
                            chat_id: None,
                            name: "alice".to_string(),
                        }),
                        image: None,
                    },
                },
                FeishuRichTextElement {
                    segment_type: "link".to_string(),
                    content: FeishuRichTextContent {
                        text: None,
                        link: Some("https://example.com".to_string()),
                        mention: None,
                        image: None,
                    },
                },
            ],
        };

        let mentions = extract_mentions_from_rich_text(&rich);
        let links = extract_links_from_rich_text(&rich);
        assert_eq!(mentions, vec!["ou_123".to_string()]);
        assert_eq!(links, vec!["https://example.com".to_string()]);
    }

    #[test]
    fn convert_feishu_emoticons_to_unicode() {
        assert_eq!(convert_feishu_emoticons("[微笑] [赞]"), "😊 👍");
    }

    #[test]
    fn format_feishu_to_matrix_falls_back_for_unsupported_type() {
        let message = FeishuMessage {
            message_id: "om_xxx".to_string(),
            chat_id: "oc_xxx".to_string(),
            chat_type: "group".to_string(),
            sender_id: "ou_xxx".to_string(),
            sender_type: "user".to_string(),
            create_time: Utc::now(),
            update_time: None,
            delete_time: None,
            msg_type: "unknown".to_string(),
            parent_id: None,
            thread_id: None,
            root_id: None,
            mentioned_sender: None,
            mentioned_users: Vec::new(),
            mentioned_chats: Vec::new(),
            content: FeishuMessageContent {
                text: None,
                rich_text: None,
                image_key: None,
                file_key: None,
                audio_key: None,
                video_key: None,
                sticker_id: None,
                card: None,
            },
        };

        let bridged = format_feishu_to_matrix(message);
        assert!(matches!(bridged.msg_type, MessageType::Text));
        assert!(bridged.content.contains("[Unsupported: unknown]"));
    }
}
