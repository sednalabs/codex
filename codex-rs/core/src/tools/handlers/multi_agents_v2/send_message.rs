use super::message_tool::MessageDeliveryMode;
use super::message_tool::SendMessageArgs;
use super::message_tool::handle_message_submission;
use super::*;
use crate::tools::handlers::multi_agents_spec::create_send_message_tool_with_mailbox;
use codex_tools::ToolSpec;

#[derive(Default)]
pub(crate) struct Handler {
    mailbox_enabled: bool,
}

impl Handler {
    pub(crate) fn new(mailbox_enabled: bool) -> Self {
        Self { mailbox_enabled }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("send_message")
    }

    fn spec(&self) -> ToolSpec {
        create_send_message_tool_with_mailbox(self.mailbox_enabled)
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
        let args: SendMessageArgs = parse_arguments(&arguments)?;
        let (target, message, interrupt, intent, kind, reply_to) = args.into_parts()?;
        handle_message_submission(
            invocation,
            MessageDeliveryMode::QueueOnly,
            target,
            message,
            interrupt,
            /*expected_model*/ None,
            intent,
            kind,
            reply_to,
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
