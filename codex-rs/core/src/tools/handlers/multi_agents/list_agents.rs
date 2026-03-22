use super::*;
use crate::agent::control::SUBAGENT_IDENTITY_SOURCE_THREAD_CONFIG_SNAPSHOT;
use crate::agent::control::SubAgentInventoryInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_state::DirectionalThreadSpawnEdgeStatus;
use std::collections::HashMap;
use tracing::warn;

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
        let include_descendants = args.include_descendants;
        let filter_ids = args
            .ids
            .map(|ids| {
                ids.into_iter()
                    .map(|id| agent_id(&id))
                    .collect::<Result<Vec<_>, FunctionCallError>>()
            })
            .transpose()?;
        let live_agents = if include_descendants {
            session
                .services
                .agent_control
                .list_live_subagent_descendant_inventory(session.conversation_id)
                .await
        } else {
            session
                .services
                .agent_control
                .list_direct_child_subagent_inventory(session.conversation_id)
                .await
        };
        let mut persisted_descendant_edge_statuses =
            HashMap::<ThreadId, ListAgentSpawnEdgeStatus>::new();
        if include_descendants {
            match session
                .services
                .agent_control
                .list_persisted_subagent_descendants_with_edge_status(session.conversation_id)
                .await
            {
                Ok(descendants) => {
                    persisted_descendant_edge_statuses.extend(
                        descendants
                            .into_iter()
                            .map(|(thread_id, edge_status)| (thread_id, edge_status.into())),
                    );
                }
                Err(err) => {
                    warn!(
                        "failed to load persisted descendants for {}: {err}",
                        session.conversation_id
                    );
                }
            }
        }
        let agents = if let Some(filter_ids) = filter_ids {
            let mut live_agents_by_id: HashMap<_, _> = live_agents
                .into_iter()
                .map(|agent| (agent.thread_id, agent))
                .collect();
            filter_ids
                .into_iter()
                .map(|thread_id| {
                    let mut entry = live_agents_by_id
                        .remove(&thread_id)
                        .map(ListAgentEntry::from)
                        .unwrap_or_else(|| {
                            ListAgentEntry::not_found(
                                thread_id,
                                if include_descendants {
                                    persisted_descendant_edge_statuses.get(&thread_id).copied()
                                } else {
                                    None
                                },
                            )
                        });
                    if include_descendants {
                        entry.spawn_edge_status =
                            persisted_descendant_edge_statuses.get(&thread_id).copied();
                    }
                    entry
                })
                .collect()
        } else if include_descendants {
            let mut agents_by_id: HashMap<ThreadId, ListAgentEntry> = HashMap::new();
            for live_agent in live_agents {
                let thread_id = live_agent.thread_id;
                let mut entry = ListAgentEntry::from(live_agent);
                entry.spawn_edge_status =
                    persisted_descendant_edge_statuses.get(&thread_id).copied();
                agents_by_id.insert(thread_id, entry);
            }
            for (descendant_id, edge_status) in &persisted_descendant_edge_statuses {
                agents_by_id.entry(*descendant_id).or_insert_with(|| {
                    ListAgentEntry::not_found(*descendant_id, Some(*edge_status))
                });
            }
            let mut entries = agents_by_id.into_values().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.agent_id.cmp(&right.agent_id));
            entries
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
    #[serde(default)]
    include_descendants: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ListAgentSpawnEdgeStatus {
    Open,
    Closed,
}

impl From<DirectionalThreadSpawnEdgeStatus> for ListAgentSpawnEdgeStatus {
    fn from(value: DirectionalThreadSpawnEdgeStatus) -> Self {
        match value {
            DirectionalThreadSpawnEdgeStatus::Open => Self::Open,
            DirectionalThreadSpawnEdgeStatus::Closed => Self::Closed,
        }
    }
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
    pub(crate) spawn_edge_status: Option<ListAgentSpawnEdgeStatus>,
    pub(crate) effective_model: Option<String>,
    pub(crate) effective_reasoning_effort: Option<ReasoningEffort>,
    pub(crate) effective_model_provider_id: String,
    pub(crate) identity_source: String,
}

impl ListAgentEntry {
    fn not_found(thread_id: ThreadId, spawn_edge_status: Option<ListAgentSpawnEdgeStatus>) -> Self {
        Self {
            agent_id: thread_id.to_string(),
            nickname: None,
            role: None,
            status: AgentStatus::NotFound,
            spawn_edge_status,
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
            spawn_edge_status: None,
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
