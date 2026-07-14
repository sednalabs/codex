use crate::agent::control::SubAgentInventoryInfo;
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
    agent: &SubAgentInventoryInfo,
) -> String {
    let nickname = agent
        .nickname
        .as_deref()
        .filter(|nickname| !nickname.is_empty())
        .map(|nickname| format!(": {nickname}"))
        .unwrap_or_default();
    let reasoning_effort = agent
        .identity
        .effective_reasoning_effort
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| "<not-set>".to_string());
    format!(
        "- {agent_reference}{nickname} [effective_model={} effective_model_provider_id={} effective_reasoning_effort={reasoning_effort} effective_service_tier={} identity_source={}]",
        agent
            .identity
            .effective_model
            .as_deref()
            .unwrap_or("<unavailable>"),
        agent
            .identity
            .effective_model_provider_id
            .as_deref()
            .unwrap_or("<unavailable>"),
        agent
            .identity
            .effective_service_tier
            .as_deref()
            .unwrap_or("<not-set>"),
        agent.identity.identity_source,
    )
}
