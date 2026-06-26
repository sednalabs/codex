use codex_protocol::AgentPath;
use codex_protocol::items::SubagentNotificationItem;
use codex_protocol::items::parse_subagent_notification_response_item;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_utils_output_truncation::approx_token_count;
use pretty_assertions::assert_eq;

use super::COMPLETION_MESSAGE_MAX_TOKENS;
use super::ERROR_NEXT_ACTION;
use super::format_inter_agent_completion_message;
use super::format_subagent_notification_message;

#[test]
fn error_completion_message_stays_below_manual_review_threshold() {
    let message = format_inter_agent_completion_message(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("valid agent path"),
        &AgentStatus::Errored("stream disconnected ".repeat(1_000)),
    )
    .expect("error status should produce a completion message");

    assert!(approx_token_count(&message) < COMPLETION_MESSAGE_MAX_TOKENS);
    assert!(message.contains(ERROR_NEXT_ACTION));
}

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
        internal_chat_message_metadata_passthrough: None,
    };

    assert_eq!(
        parse_subagent_notification_response_item(&item),
        Some(SubagentNotificationItem {
            agent_id: "agent-123".to_string(),
            status,
        })
    );
}
