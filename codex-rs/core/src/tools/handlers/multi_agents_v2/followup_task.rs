use super::message_tool::AssignTaskArgs;
use super::message_tool::MessageDeliveryMode;
use super::message_tool::handle_message_string_tool;
use super::*;
use crate::tools::handlers::multi_agents_spec::create_followup_task_tool;
use codex_tools::ToolSpec;

pub(crate) struct Handler;
pub(crate) struct MailboxHandler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("followup_task")
    }

    fn spec(&self) -> ToolSpec {
        create_followup_task_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let arguments = function_arguments(invocation.payload.clone())?;
        let args: AssignTaskArgs = parse_arguments(&arguments)?;
        handle_message_string_tool(
            invocation,
            MessageDeliveryMode::TriggerTurn,
            args.target,
            args.message,
            args.expected_model,
        )
        .await
        .map(boxed_tool_output)
    }
}

impl ToolExecutor<ToolInvocation> for MailboxHandler {
    fn tool_name(&self) -> ToolName { ToolName::plain("followup_task") }
    fn spec(&self) -> ToolSpec { create_followup_task_tool() }
    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            if invocation.cancellation_token.is_cancelled() {
                return Err(FunctionCallError::RespondToModel("follow-up was cancelled before native acceptance".to_string()));
            }
            Handler.handle_call(invocation).await
        })
    }
}

impl CoreToolRuntime for MailboxHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
    fn waits_for_runtime_cancellation(&self) -> bool {
        // A caller cancellation can suppress tool publication, but cannot drop a native
        // acceptance transaction between its durable commit and dispatch settlement.
        true
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}
