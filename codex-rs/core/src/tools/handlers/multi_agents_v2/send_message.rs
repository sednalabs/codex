use super::analytics::ToolCallAnalytics;
use super::message_tool::SendMessageArgs;
use super::message_tool::handle_message_submission;
use super::*;
use crate::agent::control::MessageDeliveryMode;
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
        let (target, message, interrupt) = args.into_parts()?;
        handle_message_submission(
            invocation,
            MessageDeliveryMode::QueueOnly,
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
            target,
            message,
            interrupt,
            /*expected_model*/ None,
=======
            args.target,
            args.message,
            analytics,
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
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
