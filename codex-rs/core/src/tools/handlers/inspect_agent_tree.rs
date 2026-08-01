use crate::agent::control::AgentTreeInspection;
use crate::agent::control::AgentTreeScope;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::multi_agents_common::tool_output_code_mode_result;
use crate::tools::handlers::multi_agents_common::tool_output_json_text;
use crate::tools::handlers::multi_agents_common::tool_output_response_item;
use crate::tools::handlers::multi_agents_spec::INSPECT_AGENT_TREE_MAX_AGENTS;
use crate::tools::handlers::multi_agents_spec::INSPECT_AGENT_TREE_MAX_DEPTH;
use crate::tools::handlers::multi_agents_spec::create_inspect_agent_tree_tool;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::models::ResponseInputItem;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde_json::Value as JsonValue;

const DEFAULT_TREE_MAX_DEPTH: usize = 2;
const DEFAULT_TREE_MAX_AGENTS: usize = 25;

pub struct InspectAgentTreeHandler;

impl ToolExecutor<ToolInvocation> for InspectAgentTreeHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("inspect_agent_tree")
    }

    fn spec(&self) -> ToolSpec {
        create_inspect_agent_tree_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl InspectAgentTreeHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, crate::function_tool::FunctionCallError>
    {
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
        if max_depth > INSPECT_AGENT_TREE_MAX_DEPTH {
            return Err(crate::function_tool::FunctionCallError::RespondToModel(
                format!("max_depth must be at most {INSPECT_AGENT_TREE_MAX_DEPTH}"),
            ));
        }
        if max_agents == 0 {
            return Err(crate::function_tool::FunctionCallError::RespondToModel(
                "max_agents must be greater than zero".to_string(),
            ));
        }
        if max_agents > INSPECT_AGENT_TREE_MAX_AGENTS {
            return Err(crate::function_tool::FunctionCallError::RespondToModel(
                format!("max_agents must be at most {INSPECT_AGENT_TREE_MAX_AGENTS}"),
            ));
        }

        session
            .services
            .agent_control
            .register_session_root(session.thread_id, turn.session_source.parent_thread_id());
        session
            .services
            .agent_control
            .inspect_agent_tree(
                session.thread_id,
                &turn.session_source,
                args.target.as_deref(),
                args.agent_roots.as_deref(),
                args.scope.unwrap_or(AgentTreeScope::Live),
                max_depth,
                max_agents,
            )
            .await
            .map_err(|err| crate::function_tool::FunctionCallError::RespondToModel(err.to_string()))
            .map(boxed_tool_output)
    }
}

impl CoreToolRuntime for InspectAgentTreeHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
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
