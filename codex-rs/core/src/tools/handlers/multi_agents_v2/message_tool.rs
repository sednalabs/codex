//! Shared argument parsing and dispatch for the v2 text-only agent messaging tools.
//!
//! `send_message` and `assign_task` share the same submission path and differ only in whether the
//! resulting `InterAgentCommunication` should wake the target immediately.

use super::*;
use crate::tools::context::FunctionToolOutput;
use crate::turn_timing::now_unix_timestamp_ms;
use codex_protocol::protocol::InterAgentCommunication;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageDeliveryMode {
    QueueOnly,
    TriggerTurn,
}

impl MessageDeliveryMode {
    /// Returns whether the produced communication should start a turn immediately.
    fn apply(self, communication: InterAgentCommunication) -> InterAgentCommunication {
        match self {
            Self::QueueOnly => InterAgentCommunication {
                trigger_turn: false,
                ..communication
            },
            Self::TriggerTurn => InterAgentCommunication {
                trigger_turn: true,
                ..communication
            },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Input for the MultiAgentV2 `send_message` tool.
pub(crate) struct SendMessageArgs {
    pub(crate) target: String,
    pub(crate) items: Vec<UserInput>,
    #[serde(default)]
    pub(crate) interrupt: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Input for the MultiAgentV2 `assign_task` tool.
pub(crate) struct AssignTaskArgs {
    pub(crate) target: String,
    pub(crate) message: String,
}

#[derive(Debug, Serialize)]
/// Tool result shared by the MultiAgentV2 message-delivery tools.
pub(crate) struct MessageToolResult {
    submission_id: String,
}

impl ToolOutput for MessageToolResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "multi_agent_message")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, Some(true), "multi_agent_message")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "multi_agent_message")
    }
}

fn message_content(message: String) -> Result<String, FunctionCallError> {
    if message.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "Empty message can't be sent to an agent".to_string(),
        ));
    }
    Ok(message)
}

/// Handles the shared MultiAgentV2 plain-text message flow for both `send_message` and `followup_task`.
pub(crate) async fn handle_message_string_tool(
    invocation: ToolInvocation,
    mode: MessageDeliveryMode,
    target: String,
    message: String,
) -> Result<MessageToolResult, FunctionCallError> {
    handle_message_submission(
        invocation,
        mode,
        target,
        message_content(message)?,
        /*interrupt*/ false,
    )
    .await
}

fn message_content_from_items(
    tool_name: &str,
    items: Vec<UserInput>,
) -> Result<String, FunctionCallError> {
    if items.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "Items can't be empty".to_string(),
        ));
    }
    let mut text_segments = Vec::new();
    for item in items {
        match item {
            UserInput::Text { text, .. } if !text.trim().is_empty() => text_segments.push(text),
            UserInput::Text { .. } => {}
            UserInput::Image { .. }
            | UserInput::LocalImage { .. }
            | UserInput::Skill { .. }
            | UserInput::Mention { .. }
            | _ => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "{tool_name} only supports text content in MultiAgentV2 for now"
                )));
            }
        }
    }

    message_content(text_segments.join("\n"))
}

async fn handle_message_submission(
    invocation: ToolInvocation,
    mode: MessageDeliveryMode,
    target: String,
    prompt: String,
    interrupt: bool,
) -> Result<MessageToolResult, FunctionCallError> {
    let ToolInvocation {
        session,
        turn,
        payload,
        call_id,
        ..
    } = invocation;
    let _ = payload;
    let receiver_thread_id = resolve_agent_target(&session, &turn, &target).await?;
    let receiver_agent = session
        .services
        .agent_control
        .get_agent_metadata(receiver_thread_id)
        .unwrap_or_default();
    if mode == MessageDeliveryMode::TriggerTurn
        && receiver_agent
            .agent_path
            .as_ref()
            .is_some_and(AgentPath::is_root)
    {
        return Err(FunctionCallError::RespondToModel(
            "Tasks can't be assigned to the root agent".to_string(),
        ));
    }
    if interrupt {
        session
            .services
            .agent_control
            .interrupt_agent(receiver_thread_id)
            .await
            .map_err(|err| collab_agent_error(receiver_thread_id, err))?;
    }
    session
        .send_event(
            &turn,
            CollabAgentInteractionBeginEvent {
                call_id: call_id.clone(),
                started_at_ms: now_unix_timestamp_ms(),
                sender_thread_id: session.conversation_id,
                receiver_thread_id,
                prompt: prompt.clone(),
            }
            .into(),
        )
        .await;
    let receiver_agent_path = match receiver_agent.agent_path.clone() {
        Some(path) => path,
        None => {
            let status = session
                .services
                .agent_control
                .get_status(receiver_thread_id)
                .await;
            session
                .send_event(
                    &turn,
                    CollabAgentInteractionEndEvent {
                        call_id: call_id.clone(),
                        sender_thread_id: session.conversation_id,
                        receiver_thread_id,
                        receiver_agent_nickname: receiver_agent.agent_nickname,
                        receiver_agent_role: receiver_agent.agent_role,
                        prompt: prompt.clone(),
                        status,
                    }
                    .into(),
                )
                .await;
            return Err(FunctionCallError::RespondToModel(
                "target agent is missing an agent_path".to_string(),
            ));
        }
    };
    let communication = InterAgentCommunication::new(
        turn.session_source
            .get_agent_path()
            .unwrap_or_else(AgentPath::root),
        receiver_agent_path,
        Vec::new(),
        prompt.clone(),
        /*trigger_turn*/ true,
    );
    let result = session
        .services
        .agent_control
        .send_inter_agent_communication(receiver_thread_id, mode.apply(communication))
        .await
        .map_err(|err| collab_agent_error(receiver_thread_id, err));
    let status = session
        .services
        .agent_control
        .get_status(receiver_thread_id)
        .await;
    session
        .send_event(
            &turn,
            CollabAgentInteractionEndEvent {
                call_id,
                completed_at_ms: now_unix_timestamp_ms(),
                sender_thread_id: session.conversation_id,
                receiver_thread_id,
                receiver_agent_nickname: receiver_agent.agent_nickname,
                receiver_agent_role: receiver_agent.agent_role,
                prompt,
                status,
            }
            .into(),
        )
        .await;
    let submission_id = result?;

    Ok(MessageToolResult { submission_id })
}

pub(crate) async fn handle_message_items_tool(
    invocation: ToolInvocation,
    mode: MessageDeliveryMode,
    target: String,
    items: Vec<UserInput>,
    interrupt: bool,
) -> Result<MessageToolResult, FunctionCallError> {
    let tool_name = invocation.tool_name.clone();
    let prompt = message_content_from_items(tool_name.name.as_str(), items)?;
    handle_message_submission(invocation, mode, target, prompt, interrupt).await
}
