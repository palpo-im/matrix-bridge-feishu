use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq)]
pub enum MatrixCommandOutcome {
    Ignored,
    Reply(String),
    BridgeRequested { feishu_chat_id: String },
    UnbridgeRequested,
}

pub struct MatrixCommandHandler {
    command_prefix: String,
    self_service_enabled: bool,
}

impl MatrixCommandHandler {
    pub fn new(self_service_enabled: bool) -> Self {
        Self {
            command_prefix: "!feishu".to_string(),
            self_service_enabled,
        }
    }

    pub fn is_command(&self, body: &str) -> bool {
        body.trim().starts_with(&self.command_prefix)
    }

    pub fn handle<F>(
        &self,
        body: &str,
        is_room_bridged: bool,
        _permission_checker: F,
    ) -> MatrixCommandOutcome
    where
        F: Fn(&str) -> bool,
    {
        let body = body.trim();
        let parts: Vec<&str> = body.split_whitespace().collect();

        if parts.is_empty() || parts[0] != self.command_prefix {
            return MatrixCommandOutcome::Ignored;
        }

        match parts.get(1).map(|s| *s) {
            Some("bridge") => {
                if !self.self_service_enabled {
                    return MatrixCommandOutcome::Reply(
                        "Self-service bridging is not enabled on this bridge.".to_string(),
                    );
                }

                if is_room_bridged {
                    return MatrixCommandOutcome::Reply(
                        "This room is already bridged to a Feishu chat.".to_string(),
                    );
                }

                let feishu_chat_id = parts.get(2).map(|s| s.to_string());
                match feishu_chat_id {
                    Some(chat_id) => MatrixCommandOutcome::BridgeRequested {
                        feishu_chat_id: chat_id,
                    },
                    None => MatrixCommandOutcome::Reply(
                        "Usage: !feishu bridge <feishu_chat_id>".to_string(),
                    ),
                }
            }
            Some("unbridge") => {
                if !is_room_bridged {
                    return MatrixCommandOutcome::Reply(
                        "This room is not bridged to any Feishu chat.".to_string(),
                    );
                }

                MatrixCommandOutcome::UnbridgeRequested
            }
            Some("help") => MatrixCommandOutcome::Reply(self.help_text()),
            Some("ping") => MatrixCommandOutcome::Reply("Pong!".to_string()),
            _ => MatrixCommandOutcome::Reply(format!(
                "Unknown command. Use `{} help` for available commands.",
                self.command_prefix
            )),
        }
    }

    fn help_text(&self) -> String {
        let mut help = vec![
            format!("{} help - Show this help message", self.command_prefix),
            format!(
                "{} ping - Check if the bridge is responsive",
                self.command_prefix
            ),
        ];

        if self.self_service_enabled {
            help.push(format!(
                "{} bridge <chat_id> - Bridge this room to a Feishu chat",
                self.command_prefix
            ));
            help.push(format!(
                "{} unbridge - Remove the bridge from this room",
                self.command_prefix
            ));
        }

        help.join("\n")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FeishuCommandOutcome {
    Ignored,
    Reply(String),
    ApproveRequested,
    DenyRequested,
    UnbridgeRequested,
}

pub struct FeishuCommandHandler {
    command_prefix: String,
}

impl FeishuCommandHandler {
    pub fn new() -> Self {
        Self {
            command_prefix: "/feishu".to_string(),
        }
    }

    pub fn is_command(&self, content: &str) -> bool {
        content.trim().starts_with(&self.command_prefix)
    }

    pub fn handle(
        &self,
        content: &str,
        is_room_bridged: bool,
        permissions: &HashSet<String>,
    ) -> FeishuCommandOutcome {
        let content = content.trim();
        let parts: Vec<&str> = content.split_whitespace().collect();

        if parts.is_empty() || parts[0] != self.command_prefix {
            return FeishuCommandOutcome::Ignored;
        }

        match parts.get(1).map(|s| *s) {
            Some("approve") => {
                if permissions.contains("admin") || permissions.contains("bridge") {
                    FeishuCommandOutcome::ApproveRequested
                } else {
                    FeishuCommandOutcome::Reply(
                        "You don't have permission to approve bridges.".to_string(),
                    )
                }
            }
            Some("deny") => {
                if permissions.contains("admin") || permissions.contains("bridge") {
                    FeishuCommandOutcome::DenyRequested
                } else {
                    FeishuCommandOutcome::Reply(
                        "You don't have permission to deny bridges.".to_string(),
                    )
                }
            }
            Some("unbridge") => {
                if !is_room_bridged {
                    return FeishuCommandOutcome::Reply(
                        "This chat is not bridged to any Matrix room.".to_string(),
                    );
                }

                if permissions.contains("admin") || permissions.contains("bridge") {
                    FeishuCommandOutcome::UnbridgeRequested
                } else {
                    FeishuCommandOutcome::Reply(
                        "You don't have permission to unbridge this chat.".to_string(),
                    )
                }
            }
            Some("help") => FeishuCommandOutcome::Reply(self.help_text()),
            _ => FeishuCommandOutcome::Reply(format!(
                "Unknown command. Use `{} help` for available commands.",
                self.command_prefix
            )),
        }
    }

    fn help_text(&self) -> String {
        format!(
            "{} help - Show this help message\n\
             {} approve - Approve a pending bridge request\n\
             {} deny - Deny a pending bridge request\n\
             {} unbridge - Remove the bridge from this chat",
            self.command_prefix, self.command_prefix, self.command_prefix, self.command_prefix
        )
    }
}

impl Default for FeishuCommandHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_command_handler_detects_commands() {
        let handler = MatrixCommandHandler::new(true);
        assert!(handler.is_command("!feishu bridge abc"));
        assert!(handler.is_command("!feishu help"));
        assert!(!handler.is_command("hello world"));
    }

    #[test]
    fn matrix_command_handler_handles_bridge() {
        let handler = MatrixCommandHandler::new(true);
        let result = handler.handle("!feishu bridge oc_xxx", false, |_| true);
        assert_eq!(
            result,
            MatrixCommandOutcome::BridgeRequested {
                feishu_chat_id: "oc_xxx".to_string()
            }
        );
    }

    #[test]
    fn matrix_command_handler_handles_unbridge() {
        let handler = MatrixCommandHandler::new(true);
        let result = handler.handle("!feishu unbridge", true, |_| true);
        assert_eq!(result, MatrixCommandOutcome::UnbridgeRequested);
    }

    #[test]
    fn feishu_command_handler_handles_approve() {
        let handler = FeishuCommandHandler::new();
        let mut perms = HashSet::new();
        perms.insert("admin".to_string());
        let result = handler.handle("/feishu approve", true, &perms);
        assert_eq!(result, FeishuCommandOutcome::ApproveRequested);
    }
}
