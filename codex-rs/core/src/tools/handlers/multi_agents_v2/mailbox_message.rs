//! Thin V2 facades: persistence and selection belong to `codex-agent-mailbox`.

use super::*;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::handlers::multi_agents_spec::create_mailbox_read_tool;
use crate::tools::handlers::multi_agents_spec::create_mailbox_send_message_tool;
use codex_agent_mailbox::ActivationDecision;
use codex_agent_mailbox::ActivationDisposition;
use codex_agent_mailbox::Envelope;
use codex_agent_mailbox::Mailbox;
use codex_agent_mailbox::MessageDraft;
use codex_agent_mailbox::MessageKind;
use codex_agent_mailbox::ObservationTarget;
use codex_tools::ToolSpec;
use std::sync::Arc;

pub(crate) struct SendHandler;
pub(crate) struct ReadHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    target: String,
    message: String,
    kind: Option<MessageKind>,
    work_ref: Option<String>,
    reply_to: Option<String>,
    supersedes: Option<String>,
    #[serde(default)]
    activate_if_idle: bool,
    #[serde(default)]
    wake_wait: bool,
}

#[derive(Serialize)]
struct SendReceipt {
    message_id: String,
    durable_state: &'static str,
    activation_disposition: ActivationDisposition,
    #[serde(skip_serializing_if = "Option::is_none")]
    activation_reason: Option<String>,
    recipient_task_name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
pub(super) struct MessageMetadata {
    pub(super) message_id: String,
    sender: String,
    kind: MessageKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    work_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    supersedes: Option<String>,
    canonical_delivered: bool,
}

#[derive(Serialize)]
struct ReadReceipt {
    messages: Vec<MessageMetadata>,
    next_cursor: Option<String>,
    quiet_count: usize,
}

pub(super) fn mailbox_error(error: impl std::fmt::Display) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!("mailbox operation failed: {error}"))
}

pub(super) fn mailbox_for(session: &Session) -> Result<Arc<Mailbox>, FunctionCallError> {
    session.services.agent_control.mailbox().ok_or_else(|| {
        FunctionCallError::RespondToModel("this session has no enabled V2 mailbox".to_string())
    })
}

/// Canonical history carries the body (including opaque encrypted content). The tool output
/// carries only retrieval/selection metadata, never a second copy or a comprehension claim.
pub(super) async fn deliver_envelope(
    session: &Session,
    turn: &TurnContext,
    envelope: &Envelope,
) -> Result<MessageMetadata, FunctionCallError> {
    if envelope.recipient != session.thread_id {
        return Err(mailbox_error("message is not addressed to this native thread"));
    }
    session
        .record_inter_agent_communication(turn, envelope.communication.clone())
        .await
        .map_err(mailbox_error)?;
    Ok(MessageMetadata {
        message_id: envelope.id.clone(),
        sender: envelope.communication.author.to_string(),
        kind: envelope.kind,
        work_ref: envelope.work_ref.as_ref().map(|work| work.id.clone()),
        reply_to: envelope.reply_to.clone(),
        supersedes: envelope.supersedes.clone(),
        canonical_delivered: true,
    })
}

impl ToolExecutor<ToolInvocation> for SendHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("send_message")
    }

    fn spec(&self) -> ToolSpec {
        create_mailbox_send_message_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolInvocation { session, turn, payload, call_id, .. } = invocation;
            let args: SendArgs = parse_arguments(&function_arguments(payload)?)?;
            if args.message.trim().is_empty() {
                return Err(mailbox_error("message cannot be empty"));
            }
            let mailbox = mailbox_for(&session)?;
            let recipient = resolve_agent_target(&session, &turn, &args.target).await?;
            let metadata = session.services.agent_control.ensure_agent_known(recipient)
                .map_err(|err| collab_agent_error(recipient, err))?;
            let recipient_path = metadata.agent_path.ok_or_else(|| mailbox_error("recipient has no native agent path"))?;
            let sender_path = turn.session_source.get_agent_path().unwrap_or_else(AgentPath::root);
            let work_ref = match args.work_ref.as_deref() {
                Some(id) => Some(mailbox.work_for_message_scope(session.thread_id, recipient, id)
                    .await.map_err(mailbox_error)?
                    .ok_or_else(|| mailbox_error("work_ref does not belong to this exact author/recipient scope"))?),
                None => None,
            };
            let legacy_attention = args.kind.is_none() && args.wake_wait;
            let envelope = mailbox.accept_message(MessageDraft {
                invocation_key: call_id,
                sender: session.thread_id,
                recipient,
                communication: InterAgentCommunication::new_encrypted(
                    sender_path,
                    recipient_path.clone(),
                    Vec::new(),
                    args.message,
                    /*trigger_turn*/ false,
                ),
                kind: args.kind.unwrap_or_default(),
                work_ref,
                reply_to: args.reply_to,
                supersedes: args.supersedes,
                legacy_attention,
                activate_if_idle: args.activate_if_idle,
            }).await.map_err(mailbox_error)?;
            let (activation_disposition, activation_reason) = if args.activate_if_idle {
                match mailbox.begin_activation(session.thread_id, &envelope.id).await {
                    Ok(ActivationDecision::Activate) => {
                        let result = session.services.agent_control
                            .mailbox_activate_if_idle(recipient, &envelope).await;
                        let (disposition, reason) = match result {
                            Ok(result) => result,
                            Err(_) => (ActivationDisposition::Unknown, Some("activation_outcome_unknown_original_message_retained".to_string())),
                        };
                        if mailbox.record_activation(session.thread_id, &envelope.id, disposition).await.is_err() {
                            (ActivationDisposition::Unknown, Some("activation_receipt_not_persisted_original_message_retained".to_string()))
                        } else {
                            (disposition, reason)
                        }
                    }
                    Ok(ActivationDecision::AlreadyDecided(disposition)) => (disposition, None),
                    Ok(ActivationDecision::RecoveryNeeded) | Err(_) => (
                        ActivationDisposition::Unknown,
                        Some("activation_outcome_unknown_original_message_retained".to_string()),
                    ),
                }
            } else {
                // The envelope is already committed. Optional activation bookkeeping must
                // never hide acceptance or invite the caller to create a duplicate message.
                let _ = mailbox.record_activation(session.thread_id, &envelope.id, ActivationDisposition::NotRequested).await;
                (ActivationDisposition::NotRequested, None)
            };
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                tool_output_json_text(&SendReceipt {
                    message_id: envelope.id,
                    durable_state: "accepted",
                    activation_disposition,
                    activation_reason,
                    recipient_task_name: recipient_path.to_string(),
                }, "send_message"),
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for SendHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

impl ToolExecutor<ToolInvocation> for ReadHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("mailbox_read")
    }

    fn spec(&self) -> ToolSpec {
        create_mailbox_read_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolInvocation { session, turn, payload, .. } = invocation;
            let args: ReadArgs = parse_arguments(&function_arguments(payload)?)?;
            let cursor = args.cursor.as_deref().map(str::parse::<i64>).transpose()
                .map_err(|_| mailbox_error("cursor must be a returned mailbox cursor"))?;
            let mailbox = mailbox_for(&session)?;
            let requested_limit = args.limit.unwrap_or(20);
            if !(1..=100).contains(&requested_limit) {
                return Err(mailbox_error("limit must be between 1 and 100"));
            }
            // An encrypted body cannot be safely truncated. Every accepted body is capped
            // at 8KiB; admit at most two per call and retain the rest behind the page cursor.
            let page = mailbox.pending_page(session.thread_id, cursor, requested_limit.min(2))
                .await.map_err(mailbox_error)?;
            let mut messages = Vec::with_capacity(page.items.len());
            for envelope in &page.items {
                messages.push(deliver_envelope(&session, &turn, envelope).await?);
                let mut observations = Vec::new();
                if matches!(envelope.kind, MessageKind::Request | MessageKind::Alert)
                    || envelope.legacy_attention
                {
                    observations.push(ObservationTarget::Attention(format!("attention:{}", envelope.id)));
                }
                if envelope.reply_to.is_some() {
                    observations.push(ObservationTarget::Reply(envelope.id.clone()));
                }
                if envelope.communication.origin == Some(codex_protocol::protocol::AgentCommunicationOrigin::Result) {
                    // A native terminal result can cover several admitted inputs. Resolve
                    // the bounded exact body relation rather than infer from agent history.
                    observations.extend(mailbox.work_for_outcome(session.thread_id, &envelope.id)
                        .await.map_err(mailbox_error)?.into_iter().map(ObservationTarget::Work));
                }
                // Reading a body can observe its own attention/reply only; it never observes
                // unrelated work outcomes or unresolved request obligations.
                if !observations.is_empty() {
                    for batch in observations.chunks(100) {
                        mailbox.acknowledge_observed(session.thread_id, batch).await.map_err(mailbox_error)?;
                    }
                }
            }
            let quiet_count = mailbox.quiet_count(session.thread_id).await.map_err(mailbox_error)?;
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                tool_output_json_text(&ReadReceipt {
                    messages,
                    next_cursor: page.next_cursor.map(|cursor| cursor.to_string()),
                    quiet_count,
                }, "mailbox_read"),
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for ReadHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}
