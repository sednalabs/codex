use super::analytics::ToolCallAnalytics;
use super::message_tool::SendMessageArgs;
use super::message_tool::handle_message_string_tool;
use super::*;
use crate::agent::api::ExplicitInterAgentUpdate;
use crate::agent::types::MessageDeliveryMode;
use crate::tools::handlers::multi_agents_spec::create_send_message_tool;
use codex_tools::ToolSpec;

pub(crate) struct Handler;

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("send_message")
    }

    fn spec(&self) -> ToolSpec {
        create_send_message_tool()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let mut analytics = ToolCallAnalytics::new(&invocation, CollabAgentTool::SendMessage);
            let result = self.handle_call(invocation, &mut analytics).await;
            analytics.finish(&result);
            result
        })
    }
}

impl Handler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
        analytics: &mut ToolCallAnalytics,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let arguments = function_arguments(invocation.payload.clone())?;
        let args: SendMessageArgs = parse_arguments(&arguments)?;
        let explicit_update = match (args.intent, args.idempotency_key, args.supersedes) {
            (None, None, None) => None,
            (Some(intent), Some(idempotency_key), supersedes) => Some(ExplicitInterAgentUpdate {
                idempotency_key,
                intent,
                supersedes: supersedes.unwrap_or_default(),
            }),
            (Some(_), None, _) => {
                return Err(FunctionCallError::RespondToModel(
                    "explicit send_message intent requires idempotency_key".to_string(),
                ));
            }
            (None, Some(_), _) | (None, None, Some(_)) => {
                return Err(FunctionCallError::RespondToModel(
                    "send_message idempotency_key and supersedes require intent".to_string(),
                ));
            }
        };
        handle_message_string_tool(
            invocation,
            MessageDeliveryMode::QueueOnly,
            args.target,
            args.message,
            explicit_update,
            analytics,
        )
        .await
        .map(boxed_tool_output)
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}
