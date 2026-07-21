//! Shared argument parsing and dispatch for the v2 agent messaging tools.
//!
//! `send_message` accepts text items plus optional interruption, while `followup_task`
//! keeps the plain-text message path. Both share the same submission plumbing once the prompt is
//! assembled.

use super::*;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::tools::context::FunctionToolOutput;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::user_input::UserInput;
use futures::future::BoxFuture;
use serde::Serialize;

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
    pub(crate) expected_model: Option<String>,
}

#[derive(Debug, Serialize)]
struct FollowupTaskResult {
    task_name: String,
    effective_model: String,
    effective_model_provider_id: String,
    effective_reasoning_effort: Option<ReasoningEffort>,
    effective_service_tier: Option<String>,
}

#[derive(Debug, Serialize)]
struct SendMessageReceipt {
    task_name: String,
    handoff_state: &'static str,
    effective_model: String,
    effective_model_provider_id: String,
    effective_reasoning_effort: Option<ReasoningEffort>,
    effective_service_tier: Option<String>,
}

pub(super) fn message_content(message: String) -> Result<String, FunctionCallError> {
    if message.trim().is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "Empty message can't be sent to an agent".to_string(),
        ));
    }
    Ok(message)
}

/// Handles the shared MultiAgentV2 message flow for both `send_message` and `followup_task`.
pub(crate) async fn handle_message_string_tool(
    invocation: ToolInvocation,
    mode: MessageDeliveryMode,
    target: String,
    message: String,
    expected_model: Option<String>,
) -> Result<FunctionToolOutput, FunctionCallError> {
    handle_message_submission(
        invocation,
        mode,
        target,
        message_content(message)?,
        /*interrupt*/ false,
        expected_model,
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

fn handle_message_submission(
    invocation: ToolInvocation,
    mode: MessageDeliveryMode,
    target: String,
    message: String,
    interrupt: bool,
    expected_model: Option<String>,
) -> BoxFuture<'static, Result<FunctionToolOutput, FunctionCallError>> {
    Box::pin(handle_message_submission_inner(
        invocation,
        mode,
        target,
        message,
        interrupt,
        expected_model,
    ))
}

async fn handle_message_submission_inner(
    invocation: ToolInvocation,
    mode: MessageDeliveryMode,
    target: String,
    message: String,
    interrupt: bool,
    expected_model: Option<String>,
) -> Result<FunctionToolOutput, FunctionCallError> {
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
        .ensure_agent_known(receiver_thread_id)
        .map_err(|err| collab_agent_error(receiver_thread_id, err))?;
    if mode == MessageDeliveryMode::TriggerTurn
        && receiver_agent
            .agent_path
            .as_ref()
            .is_some_and(AgentPath::is_root)
    {
        return Err(FunctionCallError::RespondToModel(
            "Follow-up tasks can't target the root agent".to_string(),
        ));
    }
    let receiver_agent_path = receiver_agent.agent_path.clone().ok_or_else(|| {
        FunctionCallError::RespondToModel("target agent is missing an agent_path".to_string())
    })?;
    let delivery = match mode {
        MessageDeliveryMode::QueueOnly => {
            session
                .services
                .agent_control
                .prepare_v2_agent_delivery(receiver_thread_id)
                .await
        }
        MessageDeliveryMode::TriggerTurn => {
            let resume_config = build_agent_resume_config(turn.as_ref())?;
            session
                .services
                .agent_control
                .prepare_v2_agent_delivery_with_reload(resume_config, receiver_thread_id)
                .await
        }
    }
    .map_err(|err| collab_agent_error(receiver_thread_id, err))?;
    let receiver_config = delivery
        .config_snapshot()
        .await
        .map_err(|err| collab_agent_error(receiver_thread_id, err))?;
    if let Some(expected_model) = expected_model
        && receiver_config.model != expected_model
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "follow-up task was not sent: target {receiver_agent_path} uses model `{}`, not expected model `{expected_model}`",
            receiver_config.model,
        )));
    }
    let author = turn
        .session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root);
    let communication =
        communication_from_tool_message(author, receiver_agent_path.clone(), message);
    let kind = match mode {
        MessageDeliveryMode::QueueOnly => AgentCommunicationKind::Message,
        MessageDeliveryMode::TriggerTurn => AgentCommunicationKind::Followup,
    };
    let context = AgentCommunicationContext::new(kind, session.thread_id);
    let result = delivery
        .send(mode.apply(communication), context, interrupt)
        .await
        .map_err(|err| collab_agent_error(receiver_thread_id, err));
    result?;
    emit_sub_agent_activity(
        &session,
        &turn,
        SubAgentActivityItem {
            id: call_id,
            agent_thread_id: receiver_thread_id,
            agent_path: receiver_agent_path.clone(),
            kind: SubAgentActivityKind::Interacted,
        },
    )
    .await;

    let output = match mode {
        MessageDeliveryMode::QueueOnly => tool_output_json_text(
            &SendMessageReceipt {
                task_name: receiver_agent_path.to_string(),
                handoff_state: "queued",
                effective_model: receiver_config.model,
                effective_model_provider_id: receiver_config.model_provider_id,
                effective_reasoning_effort: receiver_config.reasoning_effort,
                effective_service_tier: receiver_config.service_tier,
            },
            "send_message",
        ),
        MessageDeliveryMode::TriggerTurn => {
            tool_output_json_text(
                &FollowupTaskResult {
                    task_name: receiver_agent_path.to_string(),
                    effective_model: receiver_config.model,
                    effective_model_provider_id: receiver_config.model_provider_id,
                    effective_reasoning_effort: receiver_config.reasoning_effort,
                    effective_service_tier: receiver_config.service_tier,
                },
                "followup_task",
            )
        }
    };
    Ok(FunctionToolOutput::from_text(output, Some(true)))
}

pub(crate) async fn handle_message_items_tool(
    invocation: ToolInvocation,
    mode: MessageDeliveryMode,
    target: String,
    items: Vec<UserInput>,
    interrupt: bool,
) -> Result<FunctionToolOutput, FunctionCallError> {
    let tool_name = invocation.tool_name.clone();
    let prompt = message_content_from_items(tool_name.name.as_str(), items)?;
    handle_message_submission(
        invocation, mode, target, prompt, interrupt, /*expected_model*/ None,
    )
    .await
}
