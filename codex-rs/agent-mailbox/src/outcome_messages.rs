use crate::error::MailboxError;
use crate::error::MailboxResult;
use crate::messages::insert_message_tx;
use crate::types::Continuation;
use crate::types::MessageDraft;
use crate::types::MessageKind;
use crate::types::Outcome;
use crate::types::WorkRef;
use crate::work::exact_work_tx;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::protocol::InterAgentCommunication;
use serde::Serialize;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::Transaction;

const MAX_AUTO_RESPONSE_BYTES: usize = 16_384;
const MAX_RESPONSE_BYTES: usize = 8_192;

pub(crate) struct ResponseGroup {
    pub(crate) issuer: ThreadId,
    agent: ThreadId,
    author: AgentPath,
    recipient: AgentPath,
    pub(crate) response: Option<InterAgentCommunication>,
    pub(crate) output_id: Option<String>,
}

pub(crate) async fn response_groups(
    transaction: &mut Transaction<'_, Sqlite>,
    refs: &[WorkRef],
    response: Option<&InterAgentCommunication>,
) -> MailboxResult<Vec<ResponseGroup>> {
    let mut groups: Vec<ResponseGroup> = Vec::new();
    for work_ref in refs {
        let row = exact_work_tx(transaction, work_ref).await?;
        let input: InterAgentCommunication =
            serde_json::from_str(&row.try_get::<String, _>("input_json")?)?;
        let group = groups
            .iter_mut()
            .find(|group| group.issuer == work_ref.issuer);
        if let Some(group) = group {
            if group.agent != work_ref.agent
                || group.author != input.recipient
                || group.recipient != input.author
            {
                return Err(MailboxError::InvalidInput(
                    "issuer has inconsistent accepted communication paths".into(),
                ));
            }
        } else {
            let mut response = response.cloned();
            if let Some(response) = response.as_mut() {
                response.author = input.recipient.clone();
                response.recipient = input.author.clone();
                response.id = None;
            }
            groups.push(ResponseGroup {
                issuer: work_ref.issuer,
                agent: work_ref.agent,
                author: input.recipient,
                recipient: input.author,
                response,
                output_id: None,
            });
        }
    }
    Ok(groups)
}

pub(crate) fn validate_auto_response_size(groups: &[ResponseGroup]) -> MailboxResult<()> {
    let mut total = 0_usize;
    for group in groups {
        if let Some(response) = group.response.as_ref() {
            let bytes =
                response.content.len() + response.encrypted_content.as_ref().map_or(0, String::len);
            if bytes > MAX_RESPONSE_BYTES {
                return Err(MailboxError::Backpressure("response message byte limit"));
            }
            total = total.saturating_add(bytes);
        }
    }
    if total > MAX_AUTO_RESPONSE_BYTES {
        return Err(MailboxError::Backpressure(
            "automatic response aggregate byte limit",
        ));
    }
    Ok(())
}

#[derive(Serialize)]
struct NativeOutcomeRecord<'a> {
    work_refs: Vec<&'a str>,
    outcome: Outcome,
    continuation: &'a Continuation,
    source_ref: &'a str,
}

/// Ensure every terminal work outcome has a canonical, typed native envelope
/// even when the caller supplied no prose response. Nonterminal goal linkage
/// is deliberately excluded by the caller.
pub(crate) fn ensure_terminal_responses(
    groups: &mut [ResponseGroup],
    refs: &[WorkRef],
    outcome: Outcome,
    continuation: &Continuation,
    source_ref: &str,
) -> MailboxResult<()> {
    for group in groups.iter_mut().filter(|group| group.response.is_none()) {
        let work_refs = refs
            .iter()
            .filter(|work_ref| work_ref.issuer == group.issuer)
            .map(|work_ref| work_ref.id.as_str())
            .collect::<Vec<_>>();
        if work_refs.is_empty() {
            return Err(MailboxError::ReferenceMismatch);
        }
        let content = serde_json::to_string(&NativeOutcomeRecord {
            work_refs,
            outcome,
            continuation,
            source_ref,
        })?;
        if content.len() > MAX_RESPONSE_BYTES {
            return Err(MailboxError::Backpressure(
                "typed outcome envelope byte limit",
            ));
        }
        let communication = InterAgentCommunication::new(
            group.author.clone(),
            group.recipient.clone(),
            Vec::new(),
            content,
            /*trigger_turn*/ false,
        );
        group.response = Some(communication);
    }
    Ok(())
}

pub(crate) async fn insert_response_group(
    transaction: &mut Transaction<'_, Sqlite>,
    group: &mut ResponseGroup,
    invocation_key: String,
    outcome: Outcome,
    activate_if_idle: bool,
) -> MailboxResult<Option<String>> {
    let Some(mut communication) = group.response.clone() else {
        return Ok(None);
    };
    communication.author = group.author.clone();
    communication.recipient = group.recipient.clone();
    communication.id = None;
    let envelope = insert_message_tx(
        transaction,
        MessageDraft {
            invocation_key,
            sender: group.agent,
            recipient: group.issuer,
            communication,
            kind: MessageKind::Note,
            activate_if_idle,
            work_ref: None,
            reply_to: None,
            supersedes: None,
            legacy_attention: outcome == Outcome::NeedsAttention,
        },
        /*explicit_id*/ None,
    )
    .await?;
    group.output_id = Some(envelope.id);
    Ok(group.output_id.clone())
}
