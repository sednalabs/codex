use super::*;
use crate::agent::agent_resolver::resolve_agent_targets;
use crate::agent::status::is_final;
use crate::session::session::Session;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::CollabAgentRef;
use codex_protocol::protocol::CollabWaitingCompletionReason;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::watch::Receiver;
use tokio::time::Instant;

pub(crate) struct Handler;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WakeSource {
    TargetCompletion,
    Mailbox,
    Timeout,
}

impl WakeSource {
    fn completion_reason(self) -> CollabWaitingCompletionReason {
        match self {
            WakeSource::TargetCompletion => CollabWaitingCompletionReason::Terminal,
            WakeSource::Mailbox => CollabWaitingCompletionReason::Mailbox,
            WakeSource::Timeout => CollabWaitingCompletionReason::Timeout,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct CompletionRule {
    return_when: ReturnWhen,
}

impl CompletionRule {
    fn new(return_when: ReturnWhen) -> Self {
        Self { return_when }
    }

    fn is_satisfied(
        self,
        statuses: &HashMap<ThreadId, AgentStatus>,
        receiver_thread_ids: &[ThreadId],
    ) -> bool {
        match self.return_when {
            ReturnWhen::Any => !statuses.is_empty(),
            ReturnWhen::All => receiver_thread_ids
                .iter()
                .all(|id| statuses.get(id).is_some_and(is_final)),
        }
    }
}

impl ToolHandler for Handler {
    type Output = WaitAgentResult;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            call_id,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: WaitArgs = parse_arguments(&arguments)?;
        let receiver_thread_ids = resolve_agent_targets(&session, &turn, args.targets).await?;
        let mut seen = HashSet::with_capacity(receiver_thread_ids.len());
        for id in &receiver_thread_ids {
            if !seen.insert(*id) {
                return Err(FunctionCallError::RespondToModel(
                    "targets must resolve to unique agents".to_string(),
                ));
            }
        }
        let mut receiver_agents = Vec::with_capacity(receiver_thread_ids.len());
        for receiver_thread_id in &receiver_thread_ids {
            let agent_metadata = session
                .services
                .agent_control
                .get_agent_metadata(*receiver_thread_id)
                .unwrap_or_default();
            receiver_agents.push(CollabAgentRef {
                thread_id: *receiver_thread_id,
                agent_nickname: agent_metadata.agent_nickname,
                agent_role: agent_metadata.agent_role,
            });
        }

        let timeout_ms = args
            .timeout_ms
            .unwrap_or(turn.config.background_terminal_max_timeout as i64);
        let timeout_ms = match timeout_ms {
            ms if ms <= 0 => {
                return Err(FunctionCallError::RespondToModel(
                    "timeout_ms must be greater than zero".to_owned(),
                ));
            }
            ms => ms.clamp(MIN_WAIT_TIMEOUT_MS, MAX_WAIT_TIMEOUT_MS),
        };
        let mut mailbox_seq_rx = session.subscribe_mailbox_seq();

        session
            .send_event(
                &turn,
                CollabWaitingBeginEvent {
                    sender_thread_id: session.conversation_id,
                    receiver_thread_ids: receiver_thread_ids.clone(),
                    receiver_agents: receiver_agents.clone(),
                    call_id: call_id.clone(),
                }
                .into(),
            )
            .await;

        let mut status_rxs = Vec::with_capacity(receiver_thread_ids.len());
        let mut final_statuses = HashMap::new();
        for id in &receiver_thread_ids {
            match session.services.agent_control.subscribe_status(*id).await {
                Ok(rx) => {
                    let status = rx.borrow().clone();
                    if is_final(&status) {
                        final_statuses.insert(*id, status);
                    } else {
                        status_rxs.push((*id, rx));
                    }
                }
                Err(CodexErr::ThreadNotFound(_)) => {
                    final_statuses.insert(*id, AgentStatus::NotFound);
                }
                Err(err) => {
                    let statuses =
                        collect_wait_statuses(session.as_ref(), &receiver_thread_ids).await;
                    let pending_thread_ids =
                        pending_wait_thread_ids(&receiver_thread_ids, &statuses);
                    send_wait_end_event(
                        session.as_ref(),
                        turn.as_ref(),
                        call_id.clone(),
                        receiver_thread_ids.clone(),
                        &receiver_agents,
                        pending_thread_ids,
                        CollabWaitingCompletionReason::Terminal,
                        /*timed_out*/ false,
                        statuses,
                    )
                    .await;
                    return Err(collab_agent_error(*id, err));
                }
            }
        }

        let completion_rule = CompletionRule::new(args.return_when);
        let wake_source = if let Some(wake_source) = ready_wake_source(
            session.as_ref(),
            completion_rule,
            &final_statuses,
            &receiver_thread_ids,
        )
        .await
        {
            wake_source
        } else {
            wait_for_wake_source(
                session.clone(),
                &mut mailbox_seq_rx,
                status_rxs,
                &receiver_thread_ids,
                completion_rule,
                &mut final_statuses,
                Instant::now() + Duration::from_millis(timeout_ms as u64),
            )
            .await
        };
        let completion_reason = wake_source.completion_reason();

        let pending_thread_ids = receiver_thread_ids
            .iter()
            .filter(|receiver_thread_id| !final_statuses.contains_key(receiver_thread_id))
            .copied()
            .collect::<Vec<_>>();
        let mut pending_statuses = Vec::with_capacity(pending_thread_ids.len());
        for pending_thread_id in &pending_thread_ids {
            pending_statuses.push((
                *pending_thread_id,
                session
                    .services
                    .agent_control
                    .get_status(*pending_thread_id)
                    .await,
            ));
        }
        let statuses_by_id = merge_wait_end_statuses(final_statuses.clone(), pending_statuses);
        let result = WaitAgentResult::new(
            receiver_thread_ids.clone(),
            pending_thread_ids.clone(),
            completion_reason,
        );

        send_wait_end_event(
            session.as_ref(),
            turn.as_ref(),
            call_id,
            receiver_thread_ids,
            &receiver_agents,
            pending_thread_ids,
            completion_reason,
            result.timed_out,
            statuses_by_id,
        )
        .await;

        Ok(result)
    }
}

#[derive(Debug, Deserialize)]
struct WaitArgs {
    #[serde(default)]
    #[serde(alias = "ids")]
    targets: Vec<String>,
    timeout_ms: Option<i64>,
    #[serde(default)]
    return_when: ReturnWhen,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
enum ReturnWhen {
    #[default]
    Any,
    All,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentResult {
    pub(crate) message: String,
    pub(crate) requested_ids: Vec<ThreadId>,
    pub(crate) pending_ids: Vec<ThreadId>,
    pub(crate) completion_reason: CollabWaitingCompletionReason,
    pub(crate) timed_out: bool,
}

async fn ready_wake_source(
    session: &Session,
    completion_rule: CompletionRule,
    final_statuses: &HashMap<ThreadId, AgentStatus>,
    receiver_thread_ids: &[ThreadId],
) -> Option<WakeSource> {
    if completion_rule.is_satisfied(final_statuses, receiver_thread_ids) {
        Some(WakeSource::TargetCompletion)
    } else if session.has_pending_mailbox_items().await {
        Some(WakeSource::Mailbox)
    } else {
        None
    }
}

impl WaitAgentResult {
    fn new(
        requested_ids: Vec<ThreadId>,
        pending_ids: Vec<ThreadId>,
        completion_reason: CollabWaitingCompletionReason,
    ) -> Self {
        let message = match completion_reason {
            CollabWaitingCompletionReason::Terminal => "Wait completed.",
            CollabWaitingCompletionReason::Mailbox => "Wait woke due to mailbox activity.",
            CollabWaitingCompletionReason::Timeout => "Wait timed out.",
        };
        Self {
            message: message.to_string(),
            requested_ids,
            pending_ids,
            completion_reason,
            timed_out: matches!(completion_reason, CollabWaitingCompletionReason::Timeout),
        }
    }
}

fn merge_wait_end_statuses<I>(
    mut final_statuses: HashMap<ThreadId, AgentStatus>,
    pending_statuses: I,
) -> HashMap<ThreadId, AgentStatus>
where
    I: IntoIterator<Item = (ThreadId, AgentStatus)>,
{
    for (thread_id, status) in pending_statuses {
        final_statuses.insert(thread_id, status);
    }
    final_statuses
}

impl ToolOutput for WaitAgentResult {
    fn log_preview(&self) -> String {
        tool_output_json_text(self, "wait_agent")
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        tool_output_response_item(call_id, payload, self, /*success*/ None, "wait_agent")
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        tool_output_code_mode_result(self, "wait_agent")
    }
}

async fn wait_for_final_status(
    session: std::sync::Arc<Session>,
    thread_id: ThreadId,
    mut status_rx: Receiver<AgentStatus>,
) -> Option<(ThreadId, AgentStatus)> {
    let mut status = status_rx.borrow().clone();
    if is_final(&status) {
        return Some((thread_id, status));
    }

    loop {
        if status_rx.changed().await.is_err() {
            let latest = session.services.agent_control.get_status(thread_id).await;
            return is_final(&latest).then_some((thread_id, latest));
        }
        status = status_rx.borrow().clone();
        if is_final(&status) {
            return Some((thread_id, status));
        }
    }
}

async fn wait_for_wake_source(
    session: std::sync::Arc<Session>,
    mailbox_seq_rx: &mut tokio::sync::watch::Receiver<u64>,
    status_rxs: Vec<(ThreadId, Receiver<AgentStatus>)>,
    receiver_thread_ids: &[ThreadId],
    completion_rule: CompletionRule,
    final_statuses: &mut HashMap<ThreadId, AgentStatus>,
    deadline: Instant,
) -> WakeSource {
    let mut futures = FuturesUnordered::new();
    for (id, rx) in status_rxs {
        let session = session.clone();
        futures.push(wait_for_final_status(session, id, rx));
    }

    loop {
        if completion_rule.is_satisfied(final_statuses, receiver_thread_ids) {
            return WakeSource::TargetCompletion;
        }

        let sleep = tokio::time::sleep_until(deadline);
        tokio::pin!(sleep);

        tokio::select! {
            maybe_status = futures.next(), if !futures.is_empty() => {
                match maybe_status {
                    Some(Some((id, status))) => {
                        final_statuses.insert(id, status);
                    }
                    Some(None) => {}
                    None => {}
                }
            }
            mailbox_changed = mailbox_seq_rx.changed() => {
                if mailbox_changed.is_ok() {
                    return WakeSource::Mailbox;
                }
            }
            _ = &mut sleep => {
                return WakeSource::Timeout;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_source_maps_to_public_completion_reason() {
        assert_eq!(
            WakeSource::TargetCompletion.completion_reason(),
            CollabWaitingCompletionReason::Terminal
        );
        assert_eq!(
            WakeSource::Mailbox.completion_reason(),
            CollabWaitingCompletionReason::Mailbox
        );
        assert_eq!(
            WakeSource::Timeout.completion_reason(),
            CollabWaitingCompletionReason::Timeout
        );
    }

    #[test]
    fn completion_rule_distinguishes_any_from_all() {
        let finished_id = ThreadId::new();
        let running_id = ThreadId::new();
        let receiver_thread_ids = vec![finished_id, running_id];
        let statuses = HashMap::from([(
            finished_id,
            AgentStatus::Completed(Some("done".to_string())),
        )]);

        assert!(CompletionRule::new(ReturnWhen::Any).is_satisfied(&statuses, &receiver_thread_ids));
        assert!(
            !CompletionRule::new(ReturnWhen::All).is_satisfied(&statuses, &receiver_thread_ids)
        );
    }

    #[test]
    fn merge_wait_end_statuses_includes_pending_targets() {
        let completed_id = ThreadId::new();
        let pending_id = ThreadId::new();
        let statuses_by_id = merge_wait_end_statuses(
            HashMap::from([(
                completed_id,
                AgentStatus::Completed(Some("done".to_string())),
            )]),
            [(pending_id, AgentStatus::Running)],
        );

        assert_eq!(
            statuses_by_id.get(&completed_id),
            Some(&AgentStatus::Completed(Some("done".to_string())))
        );
        assert_eq!(statuses_by_id.get(&pending_id), Some(&AgentStatus::Running));
    }

    #[test]
    fn pending_thread_ids_for_statuses_includes_non_final_targets() {
        let finished_id = ThreadId::new();
        let running_id = ThreadId::new();
        let errored_id = ThreadId::new();
        let receiver_thread_ids = vec![finished_id, running_id, errored_id];
        let statuses = HashMap::from([
            (
                finished_id,
                AgentStatus::Completed(Some("done".to_string())),
            ),
            (running_id, AgentStatus::Running),
            (
                errored_id,
                AgentStatus::Errored("permission denied".to_string()),
            ),
        ]);
        let pending_thread_ids = pending_wait_thread_ids(&receiver_thread_ids, &statuses);

        assert_eq!(pending_thread_ids, vec![running_id]);
        assert_eq!(statuses.get(&running_id), Some(&AgentStatus::Running));
        assert_eq!(
            statuses.get(&errored_id),
            Some(&AgentStatus::Errored("permission denied".to_string()))
        );
    }
}
