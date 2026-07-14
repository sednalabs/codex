use super::*;
use crate::agent::agent_resolver::resolve_agent_targets;
use crate::agent::control::EffectiveAgentIdentity;
use crate::agent::status::is_final;
use crate::session::input_queue::InputQueueActivity;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use crate::tools::tool_runtime_capabilities::ToolRuntimeCapabilities;
use crate::tools::tool_runtime_capabilities::registered_tool_runtime_capabilities;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::CollabAgentRef;
use codex_protocol::protocol::CollabWaitingCompletionReason;
use codex_tools::ToolSpec;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use serde_json::json;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::watch::Receiver;
use tokio::time::Instant;

#[derive(Default)]
pub(crate) struct Handler {
    options: WaitAgentTimeoutOptions,
}

impl Handler {
    pub(crate) fn new(options: WaitAgentTimeoutOptions) -> Self {
        Self { options }
    }
}

pub(crate) fn resolve_wait_timeout_ms(
    requested_timeout_ms: Option<i64>,
    min_wait_timeout_ms: i64,
    max_wait_timeout_ms: i64,
    default_wait_timeout_ms: i64,
) -> Result<i64, FunctionCallError> {
    let min_timeout_ms = min_wait_timeout_ms.clamp(0, MAX_WAIT_TIMEOUT_MS);
    let max_timeout_ms = max_wait_timeout_ms.clamp(min_timeout_ms, MAX_WAIT_TIMEOUT_MS);
    let default_timeout_ms = default_wait_timeout_ms.clamp(min_timeout_ms, max_timeout_ms);

    match requested_timeout_ms {
        Some(ms) if ms < min_timeout_ms => Err(FunctionCallError::RespondToModel(format!(
            "timeout_ms must be at least {min_timeout_ms}"
        ))),
        Some(ms) if ms > max_timeout_ms => Err(FunctionCallError::RespondToModel(format!(
            "timeout_ms must be at most {max_timeout_ms}"
        ))),
        Some(ms) => Ok(ms),
        None => Ok(default_timeout_ms),
    }
}

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
        if receiver_thread_ids.is_empty() {
            return false;
        }

        match self.return_when {
            ReturnWhen::Any => !statuses.is_empty(),
            ReturnWhen::All => receiver_thread_ids
                .iter()
                .all(|id| statuses.get(id).is_some_and(is_final)),
        }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("wait_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_wait_agent_tool_v2(self.options)
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
        let ToolInvocation {
            session,
            turn,
            payload,
            call_id,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: WaitArgs = parse_arguments(&arguments)?;
        let receiver_thread_ids = if args.targets.is_empty() {
            Vec::new()
        } else {
            resolve_agent_targets(&session, &turn, args.targets).await?
        };
        let mut seen = HashSet::with_capacity(receiver_thread_ids.len());
        for id in &receiver_thread_ids {
            if !seen.insert(*id) {
                return Err(FunctionCallError::RespondToModel(
                    "targets must resolve to unique agents".to_string(),
                ));
            }
        }
        let mut receiver_agents = Vec::with_capacity(receiver_thread_ids.len());
        let mut agent_identities = Vec::with_capacity(receiver_thread_ids.len());
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
            if let Some(identity) = session
                .services
                .agent_control
                .get_agent_identity(*receiver_thread_id)
                .await
            {
                agent_identities.push(WaitAgentIdentity {
                    agent_id: *receiver_thread_id,
                    identity,
                });
            }
        }

        let timeout_ms = resolve_wait_timeout_ms(
            args.timeout_ms,
            turn.config.multi_agent_v2.min_wait_timeout_ms,
            turn.config.multi_agent_v2.max_wait_timeout_ms,
            turn.config.multi_agent_v2.default_wait_timeout_ms,
        )?;
        let (mut input_activity_rx, pending_input_activity) = session
            .input_queue
            .subscribe_activity(/*turn_state*/ None)
            .await;

        session
            .emit_turn_item_started(
                &turn,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: receiver_thread_ids.clone(),
                    receiver_agents: receiver_agents.clone(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: Default::default(),
                }),
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
                    let agents_states =
                        collect_current_wait_statuses(session.as_ref(), &receiver_thread_ids).await;
                    emit_wait_completion(
                        session.as_ref(),
                        turn.as_ref(),
                        call_id.clone(),
                        receiver_thread_ids.clone(),
                        receiver_agents.clone(),
                        agents_states,
                    )
                    .await;
                    return Err(collab_agent_error(*id, err));
                }
            }
        }

        let wait_capability = registered_tool_runtime_capabilities().wait_agent;
        let return_when = wait_capability
            .filter(|capability| capability.return_when)
            .map_or(ReturnWhen::Any, |_| args.return_when);
        let wake_on_mailbox = wait_capability.is_some_and(|capability| capability.mailbox_wake);
        let completion_rule = CompletionRule::new(return_when);
        let wake_source = if let Some(wake_source) = ready_wake_source(
            session.as_ref(),
            completion_rule,
            &final_statuses,
            &receiver_thread_ids,
            wake_on_mailbox,
            pending_input_activity,
        )
        .await
        {
            wake_source
        } else {
            wait_for_wake_source(
                session.clone(),
                &mut input_activity_rx,
                status_rxs,
                &receiver_thread_ids,
                completion_rule,
                &mut final_statuses,
                wake_on_mailbox,
                Instant::now() + Duration::from_millis(timeout_ms as u64),
            )
            .await
        };
        let completion_reason = wake_source.completion_reason();

        let candidate_pending_ids = receiver_thread_ids
            .iter()
            .filter(|receiver_thread_id| !final_statuses.contains_key(receiver_thread_id))
            .copied()
            .collect::<Vec<_>>();
        let mut pending_statuses = Vec::with_capacity(candidate_pending_ids.len());
        for pending_thread_id in &candidate_pending_ids {
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
        let pending_thread_ids = pending_wait_thread_ids(&receiver_thread_ids, &statuses_by_id);
        let mut result = WaitAgentResult::new(
            receiver_thread_ids.clone(),
            pending_thread_ids,
            completion_reason,
        );
        result.agent_identities = agent_identities;

        emit_wait_completion(
            session.as_ref(),
            turn.as_ref(),
            call_id,
            receiver_thread_ids,
            receiver_agents,
            statuses_by_id,
        )
        .await;

        Ok(boxed_tool_output(result))
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
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
    pub(crate) agent_identities: Vec<WaitAgentIdentity>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentIdentity {
    pub(crate) agent_id: ThreadId,
    #[serde(flatten)]
    pub(crate) identity: EffectiveAgentIdentity,
}

async fn ready_wake_source(
    session: &Session,
    completion_rule: CompletionRule,
    final_statuses: &HashMap<ThreadId, AgentStatus>,
    receiver_thread_ids: &[ThreadId],
    wake_on_mailbox: bool,
    pending_input_activity: Option<InputQueueActivity>,
) -> Option<WakeSource> {
    if completion_rule.is_satisfied(final_statuses, receiver_thread_ids) {
        Some(WakeSource::TargetCompletion)
    } else if wake_on_mailbox
        && (pending_input_activity.is_some()
            || session
                .input_queue
                .has_pending_input(&session.active_turn)
                .await)
    {
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
            agent_identities: Vec::new(),
        }
    }

    fn output_value(&self, capabilities: ToolRuntimeCapabilities) -> JsonValue {
        let wait_capability = capabilities.wait_agent;
        let mut output = serde_json::Map::from_iter([
            ("message".to_string(), json!(self.message)),
            ("requested_ids".to_string(), json!(self.requested_ids)),
            ("timed_out".to_string(), json!(self.timed_out)),
            ("agent_identities".to_string(), json!(self.agent_identities)),
        ]);
        if wait_capability.is_some_and(|capability| capability.pending_ids) {
            output.insert("pending_ids".to_string(), json!(self.pending_ids));
        }
        if wait_capability.is_some_and(|capability| capability.completion_reason) {
            output.insert(
                "completion_reason".to_string(),
                json!(self.completion_reason),
            );
        }
        JsonValue::Object(output)
    }

    fn output_json_text(&self, capabilities: ToolRuntimeCapabilities) -> String {
        self.output_value(capabilities).to_string()
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

fn pending_wait_thread_ids(
    receiver_thread_ids: &[ThreadId],
    statuses_by_id: &HashMap<ThreadId, AgentStatus>,
) -> Vec<ThreadId> {
    receiver_thread_ids
        .iter()
        .filter(|receiver_thread_id| !statuses_by_id.get(receiver_thread_id).is_some_and(is_final))
        .copied()
        .collect()
}

async fn collect_current_wait_statuses(
    session: &Session,
    receiver_thread_ids: &[ThreadId],
) -> HashMap<ThreadId, AgentStatus> {
    let mut statuses = HashMap::with_capacity(receiver_thread_ids.len());
    for receiver_thread_id in receiver_thread_ids {
        statuses.insert(
            *receiver_thread_id,
            session
                .services
                .agent_control
                .get_status(*receiver_thread_id)
                .await,
        );
    }
    statuses
}

async fn emit_wait_completion(
    session: &Session,
    turn: &TurnContext,
    call_id: String,
    receiver_thread_ids: Vec<ThreadId>,
    receiver_agents: Vec<CollabAgentRef>,
    agents_states: HashMap<ThreadId, AgentStatus>,
) {
    let status = if agents_states.values().any(|agent_status| {
        matches!(
            agent_status,
            AgentStatus::Errored(_) | AgentStatus::NotFound
        )
    }) {
        CollabAgentToolCallStatus::Failed
    } else {
        CollabAgentToolCallStatus::Completed
    };

    session
        .emit_turn_item_completed(
            turn,
            TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                id: call_id,
                tool: CollabAgentTool::Wait,
                status,
                sender_thread_id: session.thread_id,
                receiver_thread_ids,
                receiver_agents,
                prompt: None,
                model: None,
                reasoning_effort: None,
                agents_states,
            }),
        )
        .await;
}

impl ToolOutput for WaitAgentResult {
    fn log_preview(&self) -> String {
        self.output_json_text(registered_tool_runtime_capabilities())
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        FunctionToolOutput::from_text(
            self.output_json_text(registered_tool_runtime_capabilities()),
            /*success*/ None,
        )
        .to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        self.output_value(registered_tool_runtime_capabilities())
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

#[allow(clippy::too_many_arguments)]
async fn wait_for_wake_source(
    session: std::sync::Arc<Session>,
    input_activity_rx: &mut tokio::sync::watch::Receiver<InputQueueActivity>,
    status_rxs: Vec<(ThreadId, Receiver<AgentStatus>)>,
    receiver_thread_ids: &[ThreadId],
    completion_rule: CompletionRule,
    final_statuses: &mut HashMap<ThreadId, AgentStatus>,
    wake_on_mailbox: bool,
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
            input_activity_changed = input_activity_rx.changed(), if wake_on_mailbox => {
                if input_activity_changed.is_ok()
                    && session
                        .input_queue
                        .has_pending_input(&session.active_turn)
                        .await
                {
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
        let refreshed_completed_id = ThreadId::new();
        let statuses_by_id = merge_wait_end_statuses(
            HashMap::from([(
                completed_id,
                AgentStatus::Completed(Some("done".to_string())),
            )]),
            [(
                refreshed_completed_id,
                AgentStatus::Completed(Some("just finished".to_string())),
            )],
        );

        assert_eq!(
            statuses_by_id.get(&completed_id),
            Some(&AgentStatus::Completed(Some("done".to_string())))
        );
        assert_eq!(
            statuses_by_id.get(&refreshed_completed_id),
            Some(&AgentStatus::Completed(Some("just finished".to_string())))
        );
        assert!(
            pending_wait_thread_ids(&[completed_id, refreshed_completed_id], &statuses_by_id)
                .is_empty()
        );
    }

    #[test]
    fn resolve_wait_timeout_uses_configured_default() {
        assert_eq!(
            resolve_wait_timeout_ms(
                /*requested_timeout_ms*/ None, /*min_wait_timeout_ms*/ 1,
                /*max_wait_timeout_ms*/ 1_000, /*default_wait_timeout_ms*/ 50
            )
            .expect("configured default should be accepted"),
            50
        );
    }

    #[test]
    fn wait_agent_output_omits_capability_owned_fields_without_provider() {
        let requested_id = ThreadId::new();
        let pending_id = ThreadId::new();
        let result = WaitAgentResult::new(
            vec![requested_id],
            vec![pending_id],
            CollabWaitingCompletionReason::Timeout,
        );

        let output = result.output_value(ToolRuntimeCapabilities::upstream_default());

        assert_eq!(output["message"], json!("Wait timed out."));
        assert_eq!(output["requested_ids"], json!([requested_id]));
        assert_eq!(output["timed_out"], json!(true));
        assert_eq!(output["agent_identities"], json!([]));
        assert!(
            !output
                .as_object()
                .expect("output should be object")
                .contains_key("pending_ids")
        );
        assert!(
            !output
                .as_object()
                .expect("output should be object")
                .contains_key("completion_reason")
        );
    }

    #[test]
    fn status_classification_keeps_only_non_final_targets_pending() {
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
