use codex_protocol::AgentPath;
use codex_protocol::protocol::AgentStatus;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;

use crate::context::ContextualUserFragment;
use crate::context::InterAgentCompletionMessage;
use crate::context::SubagentNotification;

const COMPLETION_MESSAGE_MAX_TOKENS: usize = 1_000;
const COMPLETION_MESSAGE_ENVELOPE_TOKEN_RESERVE: usize = 100;
const ERROR_MAX_TOKENS: usize =
    COMPLETION_MESSAGE_MAX_TOKENS - COMPLETION_MESSAGE_ENVELOPE_TOKEN_RESERVE;
const ERROR_NEXT_ACTION: &str = "This agent's turn failed. If you still need this agent, use the available collaboration tools to give it another task.";

// Helpers for model-visible session state markers that are stored in user-role
// messages but are not user intent.

// TODO(jif) unify with structured schema
pub(crate) fn format_subagent_notification_message(
    agent_reference: &str,
    status: &AgentStatus,
) -> String {
    SubagentNotification::new(agent_reference, status.clone()).render()
}

pub(crate) fn format_inter_agent_completion_message(
    task_name: AgentPath,
    sender: AgentPath,
    status: &AgentStatus,
) -> Option<String> {
    let payload = match status {
        AgentStatus::Completed(Some(message)) => message.clone(),
        AgentStatus::Completed(None) => String::new(),
        AgentStatus::Errored(error) => {
            let error = truncate_text(error, TruncationPolicy::Tokens(ERROR_MAX_TOKENS));
            format!("Agent errored: {error}\n\n{ERROR_NEXT_ACTION}")
        }
        AgentStatus::Shutdown => "Agent shut down.".to_string(),
        AgentStatus::NotFound => "Agent was not found.".to_string(),
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted => return None,
    };
    Some(InterAgentCompletionMessage::new(task_name, sender, payload).render())
}

#[cfg(test)]
#[path = "session_prefix_tests.rs"]
mod tests;

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
