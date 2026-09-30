use crate::agent::AgentStatus;
use crate::agent::api::AgentControl;
use crate::agent::api::AgentInfo;
use codex_protocol::ThreadId;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::openai_models::ReasoningEffort;
use serde::Serialize;

/// A bounded projection of one existing agent snapshot for model-facing tools.
///
/// Missing status or configuration is serialized as `null`; it is never inferred
/// from a requested model, task name, or earlier status observation.
#[derive(Debug, Serialize)]
pub(crate) struct InspectedAgent {
    pub(crate) agent_id: String,
    pub(crate) agent_name: String,
    pub(crate) canonical_path: Option<String>,
    pub(crate) nickname: Option<String>,
    pub(crate) agent_status: Option<AgentStatus>,
    pub(crate) configured_model: Option<String>,
    pub(crate) configured_reasoning_effort: Option<ReasoningEffort>,
}

impl InspectedAgent {
    fn from_info(agent_id: ThreadId, info: AgentInfo) -> Self {
        let metadata = info.metadata();
        let canonical_path = metadata.agent_path.as_ref().map(ToString::to_string);
        let agent_name = canonical_path
            .clone()
            .unwrap_or_else(|| agent_id.to_string());
        let (configured_model, configured_reasoning_effort) = match &info {
            AgentInfo::Loaded { config, .. } => {
                (Some(config.model.clone()), config.reasoning_effort.clone())
            }
            AgentInfo::Unloaded(_) => (None, None),
        };

        Self {
            agent_id: agent_id.to_string(),
            agent_name,
            canonical_path,
            nickname: metadata.agent_nickname.clone(),
            agent_status: info.status().cloned(),
            configured_model,
            configured_reasoning_effort,
        }
    }
}

pub(crate) async fn inspect_agent(
    control: &dyn AgentControl,
    agent_id: ThreadId,
) -> CodexResult<InspectedAgent> {
    let info = control.inspect(agent_id).await?;
    Ok(InspectedAgent::from_info(agent_id, info))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::AgentMetadata;
    use codex_protocol::AgentPath;
    use serde_json::Value;

    #[test]
    fn unloaded_agent_serializes_unknown_status_and_configuration_explicitly() {
        let agent_id = ThreadId::new();
        let info = AgentInfo::Unloaded(AgentMetadata {
            agent_id: Some(agent_id),
            agent_path: Some(AgentPath::try_from("/root/worker").expect("valid path")),
            ..AgentMetadata::default()
        });

        let serialized = serde_json::to_value(InspectedAgent::from_info(agent_id, info))
            .expect("inspection should serialize");
        assert_eq!(serialized["agent_id"], agent_id.to_string());
        assert_eq!(serialized["agent_name"], "/root/worker");
        assert_eq!(serialized["canonical_path"], "/root/worker");
        assert_eq!(serialized["agent_status"], Value::Null);
        assert_eq!(serialized["configured_model"], Value::Null);
        assert_eq!(serialized["configured_reasoning_effort"], Value::Null);
    }
}
