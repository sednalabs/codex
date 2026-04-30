use codex_protocol::protocol::AgentStatus;

use crate::context::ContextualUserFragment;
use crate::context::SubagentNotification;

// Helpers for model-visible session state markers that are stored in user-role
// messages but are not user intent.

// TODO(jif) unify with structured schema
pub(crate) fn format_subagent_notification_message(
    agent_reference: &str,
    status: &AgentStatus,
) -> String {
    SubagentNotification::new(agent_reference, status.clone()).render()
}

pub(crate) fn format_subagent_context_line(
    agent_reference: &str,
    agent_nickname: Option<&str>,
) -> String {
    match agent_nickname.filter(|nickname| !nickname.is_empty()) {
        Some(agent_nickname) => format!("- {agent_reference}: {agent_nickname}"),
        None => format!("- {agent_reference}"),
    }
}

#[cfg(test)]
mod tests {
    use super::format_subagent_notification_message;
    use codex_protocol::items::SubagentNotificationItem;
    use codex_protocol::items::parse_subagent_notification_response_item;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::AgentStatus;
    use pretty_assertions::assert_eq;

    #[test]
    fn format_subagent_notification_message_round_trips_completed_status() {
        let status = AgentStatus::Completed(Some("done".to_string()));
        let item = ResponseItem::Message {
            id: Some("msg-1".to_string()),
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format_subagent_notification_message("agent-123", &status),
            }],
            phase: None,
        };

        assert_eq!(
            parse_subagent_notification_response_item(&item),
            Some(SubagentNotificationItem {
                agent_id: "agent-123".to_string(),
                status,
            })
        );
    }
}
