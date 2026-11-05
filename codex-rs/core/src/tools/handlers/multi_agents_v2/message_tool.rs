//! Shared argument parsing and dispatch for the v2 agent messaging tools.
//!
//! `send_message` and `followup_task` share submission mechanics. Follow-ups start work, while
//! `send_message` may additionally carry native mailbox classification and exact reply identity.

use super::analytics::ToolCallAnalytics;
use super::*;
use crate::TurnStartOptions;
use crate::agent::api::AgentInput;
use crate::agent::api::AgentTarget;
use crate::agent::api::SendRequest;
use crate::agent::child_config::build_agent_resume_config;
use crate::agent::types::MessageDeliveryMode;
use crate::tools::context::FunctionToolOutput;
use codex_agent_mailbox::MessageKind;
use codex_features::Feature;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Input for the MultiAgentV2 `send_message` tool.
pub(crate) struct SendMessageArgs {
    pub(crate) target: String,
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) kind: Option<MessageKind>,
    #[serde(default)]
    pub(crate) reply_to: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
/// Input for the MultiAgentV2 `followup_task` tool.
pub(crate) struct FollowupTaskArgs {
    pub(crate) target: String,
    pub(crate) message: String,
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
pub(super) async fn handle_message_string_tool(
    invocation: ToolInvocation,
    mode: MessageDeliveryMode,
    target: String,
    message: String,
    message_kind: Option<MessageKind>,
    reply_to: Option<String>,
    analytics: &mut ToolCallAnalytics,
) -> Result<FunctionToolOutput, FunctionCallError> {
    let message = message_content(message)?;
    let ToolInvocation {
        session,
        turn,
        call_id,
        source,
        ..
    } = invocation;
    let mailbox_enabled = turn.config.features.enabled(Feature::AgentMailbox);
    if !mailbox_enabled && (message_kind.is_some() || reply_to.is_some()) {
        return Err(FunctionCallError::RespondToModel(
            "typed message kinds and replies require agent_mailbox to be enabled".to_string(),
        ));
    }
    let receiver_thread_id = resolve_agent_target(&session, &turn, &target).await?;
    analytics.set_receiver(receiver_thread_id);
    let resume_config =
        build_agent_resume_config(&turn).map_err(FunctionCallError::RespondToModel)?;
    let receipt = session
        .services
        .agent_control
        .send(SendRequest {
            caller: session.thread_id,
            target: AgentTarget::Id(receiver_thread_id),
            resume_config,
            input: AgentInput::Message {
                message: agent_message_from_tool(message, &source),
                mode,
                native_key: mailbox_enabled.then(|| call_id.clone()),
                message_kind,
                reply_to,
            },
            start_options: TurnStartOptions {
                parent_turn_id: (mode == MessageDeliveryMode::TriggerTurn)
                    .then(|| turn.sub_id.clone()),
                root_turn_id: turn.turn_metadata_state.root_turn_id(),
                turn_trigger: turn.turn_metadata_state.current_turn_trigger(),
                cyber_access_program: turn.cyber_access_program,
                ..Default::default()
            },
        })
        .await
        .map_err(|err| collab_v2_agent_error(receiver_thread_id, err))?;
    let receiver_agent_path = receipt.metadata.agent_path.ok_or_else(|| {
        FunctionCallError::RespondToModel("target agent is missing an agent_path".to_string())
    })?;
    emit_sub_agent_activity(
        &session,
        &turn,
        SubAgentActivityItem {
            model: None,
            reasoning_effort: None,
            id: call_id,
            agent_thread_id: receiver_thread_id,
            agent_path: receiver_agent_path,
            kind: SubAgentActivityKind::Interacted,
        },
    )
    .await;

    let output = if message_kind == Some(MessageKind::Request) {
        format!("Accepted request with stable id {}.", receipt.submission_id)
    } else {
        String::new()
    };
    Ok(FunctionToolOutput::from_text(output, Some(true)))
}

#[cfg(test)]
mod tests {
    use super::SendMessageArgs;
    use codex_agent_mailbox::MessageKind;

    #[test]
    fn send_message_args_keep_exact_request_and_reply_fields() {
        let args: SendMessageArgs = serde_json::from_value(serde_json::json!({
            "target": "/root/worker",
            "message": "please review this",
            "kind": "request",
            "reply_to": "amsg_exact-request-id"
        }))
        .expect("typed mailbox fields should parse");

        assert_eq!(args.kind, Some(MessageKind::Request));
        assert_eq!(args.reply_to.as_deref(), Some("amsg_exact-request-id"));
    }
}
