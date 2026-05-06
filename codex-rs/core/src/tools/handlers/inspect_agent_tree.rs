use crate::agent::control::AgentTreeInspection;
use crate::agent::control::AgentTreeScope;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::multi_agents_common::tool_output_code_mode_result;
use crate::tools::handlers::multi_agents_common::tool_output_json_text;
use crate::tools::handlers::multi_agents_common::tool_output_response_item;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use codex_tools::ToolName;
use codex_protocol::models::ResponseInputItem;
use serde::Deserialize;
use serde_json::Value as JsonValue;

const DEFAULT_TREE_MAX_DEPTH: usize = 2;
const DEFAULT_TREE_MAX_AGENTS: usize = 25;

pub struct InspectAgentTreeHandler {
    tool_name: ToolName,
}

impl InspectAgentTreeHandler {
    pub fn new(tool_name: ToolName) -> Self {
        Self { tool_name }
    }
}

impl ToolHandler for InspectAgentTreeHandler {
    type Output = AgentTreeInspection;

    fn tool_name(&self) -> ToolName {
        self.tool_name.clone()
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Self::Output, crate::function_tool::FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            ..
        } = invocation;
        let ToolPayload::Function { arguments } = payload else {
            return Err(crate::function_tool::FunctionCallError::RespondToModel(
                "inspect_agent_tree received unsupported payload".to_string(),
            ));
        };
        let args: InspectAgentTreeArgs = parse_arguments(&arguments)?;
        let max_depth = args.max_depth.unwrap_or(DEFAULT_TREE_MAX_DEPTH);
        let max_agents = args.max_agents.unwrap_or(DEFAULT_TREE_MAX_AGENTS);
        if max_depth == 0 {
            return Err(crate::function_tool::FunctionCallError::RespondToModel(
                "max_depth must be greater than zero".to_string(),
            ));
        }
        if max_agents == 0 {
            return Err(crate::function_tool::FunctionCallError::RespondToModel(
                "max_agents must be greater than zero".to_string(),
            ));
        }

        session
            .services
            .agent_control
            .register_session_root(session.conversation_id, &turn.session_source);
        session
            .services
            .agent_control
            .inspect_agent_tree(
                session.conversation_id,
                &turn.session_source,
                args.target.as_deref(),
                args.agent_roots.as_deref(),
                args.scope.unwrap_or(AgentTreeScope::Live),
                max_depth,
                max_agents,
            )
            .await
            .map_err(|err| crate::function_tool::FunctionCallError::RespondToModel(err.to_string()))
    }
}

#[derive(Debug, Deserialize)]
struct InspectAgentTreeArgs {
    target: Option<String>,
    agent_roots: Option<Vec<String>>,
    scope: Option<AgentTreeScope>,
    max_depth: Option<usize>,
    max_agents: Option<usize>,
}

impl ToolOutput for AgentTreeInspection {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "inspect_agent_tree")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "inspect_agent_tree")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "inspect_agent_tree")
    }
}
