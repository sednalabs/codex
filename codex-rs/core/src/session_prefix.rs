use crate::agent::control::SubAgentInventoryInfo;
use codex_protocol::protocol::AgentStatus;

/// Helpers for model-visible session state markers that are stored in user-role
/// messages but are not user intent.
use crate::contextual_user_message::SUBAGENT_NOTIFICATION_FRAGMENT;

pub(crate) fn format_subagent_notification_message(agent_id: &str, status: &AgentStatus) -> String {
    let payload_json = serde_json::json!({
        "agent_id": agent_id,
        "status": status,
    })
    .to_string();
    SUBAGENT_NOTIFICATION_FRAGMENT.wrap(payload_json)
}

pub(crate) fn format_subagent_context_line(agent: &SubAgentInventoryInfo) -> String {
    let mut segments = vec![
        "status=".to_string(),
        "model=".to_string(),
        "provider=".to_string(),
    ];
    segments[0].push_str(agent_status_label(&agent.status));
    segments[1].push_str(agent.effective_model.as_deref().unwrap_or("<not-set>"));
    segments[2].push_str(&agent.effective_model_provider_id);
    if let Some(agent_nickname) = agent
        .nickname
        .as_deref()
        .filter(|nickname| !nickname.is_empty())
    {
        segments.push(format!("nickname={agent_nickname}"));
    }
    if let Some(agent_role) = agent.role.as_deref().filter(|role| !role.is_empty()) {
        segments.push(format!("role={agent_role}"));
    }
    format!("- {}: {}", agent.thread_id, segments.join(" "))
}

fn agent_status_label(status: &AgentStatus) -> &str {
    match status {
        AgentStatus::PendingInit => "pending_init",
        AgentStatus::Running => "running",
        AgentStatus::Completed(_) => "completed",
        AgentStatus::Errored(_) => "errored",
        AgentStatus::Shutdown => "shutdown",
        AgentStatus::NotFound => "not_found",
    }
}
