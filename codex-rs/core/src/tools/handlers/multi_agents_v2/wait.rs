use super::*;
use crate::agent::status::is_final;
use codex_protocol::protocol::CollabWaitingCompletionReason;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::watch::Receiver;
use tokio::time::Instant;
use tokio::time::timeout_at;

pub(crate) struct Handler;

#[async_trait]
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
        let mut initial_final_statuses = Vec::new();
        for id in &receiver_thread_ids {
            match session.services.agent_control.subscribe_status(*id).await {
                Ok(rx) => {
                    let status = rx.borrow().clone();
                    if is_final(&status) {
                        initial_final_statuses.push((*id, status));
                    }
                    status_rxs.push((*id, rx));
                }
                Err(crate::error::CodexErr::ThreadNotFound(_)) => {
                    initial_final_statuses.push((*id, AgentStatus::NotFound));
                }
                Err(err) => {
                    let mut statuses = HashMap::with_capacity(receiver_thread_ids.len());
                    for receiver_thread_id in &receiver_thread_ids {
                        statuses.insert(
                            *receiver_thread_id,
                            session
                                .services
                                .agent_control
                                .get_status(*receiver_thread_id)
                                .await,
                        );
                    }
                    let pending_thread_ids = build_error_pending_thread_ids(
                        &receiver_thread_ids,
                        &initial_final_statuses,
                        &statuses,
                    );
                    session
                        .send_event(
                            &turn,
                            CollabWaitingEndEvent {
                                sender_thread_id: session.conversation_id,
                                call_id: call_id.clone(),
                                receiver_thread_ids: receiver_thread_ids.clone(),
                                pending_thread_ids,
                                completion_reason: CollabWaitingCompletionReason::Terminal,
                                timed_out: false,
                                agent_statuses: build_wait_agent_statuses(
                                    &statuses,
                                    &receiver_agents,
                                ),
                                statuses,
                            }
                            .into(),
                        )
                        .await;
                    return Err(collab_agent_error(*id, err));
                }
            }
        }

        let mut final_statuses = initial_final_statuses
            .into_iter()
            .collect::<HashMap<_, _>>();
        let mut timed_out = false;
        if !has_return_condition(&final_statuses, &receiver_thread_ids, args.return_when) {
            let mut futures = FuturesUnordered::new();
            for (id, rx) in status_rxs {
                let session = session.clone();
                futures.push(wait_for_final_status(session, id, rx));
            }
            let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
            loop {
                match timeout_at(deadline, futures.next()).await {
                    Ok(Some(Some((id, status)))) => {
                        final_statuses.insert(id, status);
                        if has_return_condition(
                            &final_statuses,
                            &receiver_thread_ids,
                            args.return_when,
                        ) {
                            break;
                        }
                    }
                    Ok(Some(None)) => continue,
                    Ok(None) | Err(_) => {
                        timed_out = true;
                        break;
                    }
                }
            }
        }

        let mut pending_thread_ids = Vec::new();
        for receiver_thread_id in &receiver_thread_ids {
            if !final_statuses.contains_key(receiver_thread_id) {
                pending_thread_ids.push(*receiver_thread_id);
            }
        }
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
        let agent_statuses = build_wait_agent_statuses(&statuses_by_id, &receiver_agents);
        let completion_reason = if timed_out {
            CollabWaitingCompletionReason::Timeout
        } else {
            CollabWaitingCompletionReason::Terminal
        };
        let result = WaitAgentResult::from_timed_out(timed_out);

        session
            .send_event(
                &turn,
                CollabWaitingEndEvent {
                    sender_thread_id: session.conversation_id,
                    call_id,
                    receiver_thread_ids,
                    pending_thread_ids,
                    completion_reason,
                    timed_out,
                    agent_statuses,
                    statuses: statuses_by_id,
                }
                .into(),
            )
            .await;

        Ok(result)
    }
}

fn build_error_pending_thread_ids(
    receiver_thread_ids: &[ThreadId],
    initial_final_statuses: &[(ThreadId, AgentStatus)],
    statuses: &HashMap<ThreadId, AgentStatus>,
) -> Vec<ThreadId> {
    receiver_thread_ids
        .iter()
        .filter(|id| {
            !is_final(statuses.get(id).unwrap_or(&AgentStatus::NotFound))
                && !initial_final_statuses
                    .iter()
                    .any(|(thread_id, _)| thread_id == *id)
        })
        .copied()
        .collect()
}

#[derive(Debug, Deserialize)]
struct WaitArgs {
    #[serde(default)]
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
    pub(crate) timed_out: bool,
}

fn has_return_condition(
    statuses: &HashMap<ThreadId, AgentStatus>,
    receiver_thread_ids: &[ThreadId],
    return_when: ReturnWhen,
) -> bool {
    match return_when {
        ReturnWhen::Any => !statuses.is_empty(),
        ReturnWhen::All => receiver_thread_ids
            .iter()
            .all(|id| statuses.get(id).is_some_and(is_final)),
    }
}

impl WaitAgentResult {
    fn from_timed_out(timed_out: bool) -> Self {
        let message = if timed_out {
            "Wait timed out."
        } else {
            "Wait completed."
        };
        Self {
            message: message.to_string(),
            timed_out,
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn build_error_pending_thread_ids_includes_non_final_pending_targets() {
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
        let pending_thread_ids = build_error_pending_thread_ids(
            &receiver_thread_ids,
            &[(
                finished_id,
                AgentStatus::Completed(Some("done".to_string())),
            )],
            &statuses,
        );

        assert_eq!(pending_thread_ids, vec![running_id]);
        assert_eq!(statuses.get(&running_id), Some(&AgentStatus::Running));
        assert_eq!(
            statuses.get(&errored_id),
            Some(&AgentStatus::Errored("permission denied".to_string()))
        );
    }
}
