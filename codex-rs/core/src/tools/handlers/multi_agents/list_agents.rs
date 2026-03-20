use super::*;
use crate::agent::control::SUBAGENT_IDENTITY_SOURCE_THREAD_CONFIG_SNAPSHOT;
use crate::agent::control::SubAgentInventoryInfo;
use codex_protocol::openai_models::ReasoningEffort;
use std::collections::HashMap;

pub(crate) struct Handler;

#[async_trait]
impl ToolHandler for Handler {
    type Output = ListAgentsResult;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let ToolInvocation {
            session, payload, ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: ListAgentsArgs = parse_arguments(&arguments)?;
        let filter_ids = args
            .ids
            .map(|ids| {
                ids.into_iter()
                    .map(|id| agent_id(&id))
                    .collect::<Result<Vec<_>, FunctionCallError>>()
            })
            .transpose()?;
        let live_agents = session
            .services
            .agent_control
            .list_direct_child_subagent_inventory(session.conversation_id)
            .await;
        let agents = if let Some(filter_ids) = filter_ids {
            let mut live_agents_by_id: HashMap<_, _> = live_agents
                .into_iter()
                .map(|agent| (agent.thread_id, agent))
                .collect();
            filter_ids
                .into_iter()
                .map(|thread_id| {
                    live_agents_by_id
                        .remove(&thread_id)
                        .map(ListAgentEntry::from)
                        .unwrap_or_else(|| ListAgentEntry::not_found(thread_id))
                })
                .collect()
        } else {
            live_agents.into_iter().map(ListAgentEntry::from).collect()
        };

        Ok(ListAgentsResult { agents })
    }
}

#[derive(Debug, Deserialize)]
struct ListAgentsArgs {
    #[serde(default)]
    ids: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ListAgentsResult {
    pub(crate) agents: Vec<ListAgentEntry>,
}

/// Serialized `list_agents` row.
///
/// `status` is live, while `effective_*` and `identity_source` are resolved
/// inventory metadata from the current config snapshot.
#[derive(Debug, Serialize)]
pub(crate) struct ListAgentEntry {
    pub(crate) agent_id: String,
    pub(crate) nickname: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) status: AgentStatus,
    pub(crate) effective_model: Option<String>,
    pub(crate) effective_reasoning_effort: Option<ReasoningEffort>,
    pub(crate) effective_model_provider_id: String,
    pub(crate) identity_source: String,
}

impl ListAgentEntry {
    fn not_found(thread_id: ThreadId) -> Self {
        Self {
            agent_id: thread_id.to_string(),
            nickname: None,
            role: None,
            status: AgentStatus::NotFound,
            effective_model: None,
            effective_reasoning_effort: None,
            effective_model_provider_id: String::new(),
            identity_source: SUBAGENT_IDENTITY_SOURCE_THREAD_CONFIG_SNAPSHOT.to_string(),
        }
    }
}

impl From<SubAgentInventoryInfo> for ListAgentEntry {
    fn from(agent: SubAgentInventoryInfo) -> Self {
        Self {
            agent_id: agent.thread_id.to_string(),
            nickname: agent.nickname,
            role: agent.role,
            status: agent.status,
            effective_model: agent.effective_model,
            effective_reasoning_effort: agent.effective_reasoning_effort,
            effective_model_provider_id: agent.effective_model_provider_id,
            identity_source: agent.identity_source,
        }
    }
}

impl ToolOutput for ListAgentsResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "list_agents")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "list_agents")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "list_agents")
    }
}
