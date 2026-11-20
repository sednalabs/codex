use super::*;
use crate::agent::api::AgentControl;
use crate::agent::status::is_final;
use crate::session::InputQueueActivity;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentWaitCompletionReason;
use codex_protocol::items::CollabAgentWaitReturnWhen;
use codex_tools::ToolSpec;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
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

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("wait_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_wait_agent_tool_v2(self.options)
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
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
        let min_timeout_ms = turn.config.multi_agent_v2.min_wait_timeout_ms;
        let max_timeout_ms = turn.config.multi_agent_v2.max_wait_timeout_ms;
        let default_timeout_ms = turn.config.multi_agent_v2.default_wait_timeout_ms;
        if args.native_event_wait
            && args.targets.is_empty()
            && turn.session_source.is_non_root_agent()
            && !turn
                .session_source
                .get_agent_role()
                .is_some_and(|role| role.eq_ignore_ascii_case("orchestrator"))
        {
            return Err(FunctionCallError::RespondToModel(
                "targetless native_event_wait is reserved for root and orchestrator agents"
                    .to_string(),
            ));
        }
        let requested_timeout_ms = args.timeout_ms;
        let timeout_ms = match requested_timeout_ms {
            Some(ms) if ms > max_timeout_ms => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "timeout_ms must be at most {max_timeout_ms}"
                )));
            }
            Some(ms) => ms.max(min_timeout_ms),
            None => default_timeout_ms,
        };

        let mut receiver_thread_ids = Vec::with_capacity(args.targets.len());
        let mut seen = HashSet::new();
        for target in &args.targets {
            let id = resolve_agent_target(&session, &turn, target).await?;
            if args.native_event_wait
                && (id == session.thread_id || Some(id) == turn.parent_thread_id)
            {
                return Err(FunctionCallError::RespondToModel(
                    "native waits cannot target the current agent or its parent".to_string(),
                ));
            }
            if !seen.insert(id) {
                return Err(FunctionCallError::RespondToModel(
                    "targets must resolve to unique agents".to_string(),
                ));
            }
            receiver_thread_ids.push(id);
        }
        let activity_rx = session.input_queue.subscribe_wait_updates();

        session
            .emit_turn_item_started(
                &turn,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: receiver_thread_ids.clone(),
                    completion_reason: None,
                    return_when: Some(args.return_when),
                    completed_target_thread_ids: Vec::new(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: Default::default(),
                }),
            )
            .await;

        let wait_started = Instant::now();
        let outcome = wait_with_policy(
            wait_for_actionable_activity(
                &session,
                &turn.sub_id,
                activity_rx,
                &receiver_thread_ids,
                args.return_when,
                args.native_event_wait
                    .then(|| turn.session_source.get_agent_path())
                    .flatten(),
            ),
            args.native_event_wait,
            timeout_ms,
        )
        .await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                session
                    .emit_turn_item_completed(
                        &turn,
                        TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                            id: call_id,
                            tool: CollabAgentTool::Wait,
                            status: CollabAgentToolCallStatus::Failed,
                            sender_thread_id: session.thread_id,
                            receiver_thread_ids,
                            completion_reason: None,
                            return_when: Some(args.return_when),
                            completed_target_thread_ids: Vec::new(),
                            receiver_agents: Vec::new(),
                            prompt: None,
                            model: None,
                            reasoning_effort: None,
                            agents_states: HashMap::new(),
                        }),
                    )
                    .await;
                return Err(error);
            }
        };
        // A completed wait may wake for a message, user input, or its timeout.
        // Dropped waits do not have an observed outcome and are not included.
        turn.session_telemetry.record_duration(
            "codex.multi_agent.wait.duration_ms",
            wait_started.elapsed(),
            &[(
                "outcome",
                match outcome.reason {
                    CollabAgentWaitCompletionReason::AnyTargetCompleted
                    | CollabAgentWaitCompletionReason::AllTargetsCompleted
                    | CollabAgentWaitCompletionReason::ActionableMessage => "mailbox",
                    CollabAgentWaitCompletionReason::UserInput => "steered",
                    CollabAgentWaitCompletionReason::TimedOut => "timed_out",
                },
            )],
        );
        let result = WaitAgentResult::from_outcome(
            outcome,
            receiver_thread_ids.clone(),
            args.return_when,
            requested_timeout_ms,
            timeout_ms,
        );

        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::Completed,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids,
                    completion_reason: Some(result.completion_reason),
                    return_when: Some(result.return_when),
                    completed_target_thread_ids: result.completed_target_thread_ids.clone(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: HashMap::new(),
                }),
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
#[serde(deny_unknown_fields)]
struct WaitArgs {
    #[serde(default, alias = "ids")]
    targets: Vec<String>,
    timeout_ms: Option<i64>,
    #[serde(default)]
    return_when: CollabAgentWaitReturnWhen,
    #[serde(default)]
    native_event_wait: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct WaitOutcome {
    reason: CollabAgentWaitCompletionReason,
    completed_target_thread_ids: Vec<ThreadId>,
}

impl WaitOutcome {
    fn new(reason: CollabAgentWaitCompletionReason) -> Self {
        Self {
            reason,
            completed_target_thread_ids: Vec::new(),
        }
    }
}

async fn wait_for_target_completion(
    control: Arc<dyn AgentControl>,
    id: ThreadId,
    current_path: Option<AgentPath>,
) -> Result<ThreadId, FunctionCallError> {
    let mut updates = control.subscribe_status(id).await.map_err(|error| {
        FunctionCallError::RespondToModel(format!("cannot observe target {id}: {error}"))
    })?;
    while let Some(update) = updates.next().await {
        let update = update.map_err(|error| {
            FunctionCallError::RespondToModel(format!("cannot observe target {id}: {error}"))
        })?;
        if let (Some(current), Some(target)) =
            (current_path.as_ref(), update.metadata().agent_path.as_ref())
            && (target == current
                || current
                    .as_str()
                    .strip_prefix(target.as_str())
                    .is_some_and(|suffix| suffix.starts_with('/')))
        {
            return Err(FunctionCallError::RespondToModel(
                "native waits cannot target the current agent or an ancestor".to_string(),
            ));
        }
        if update.status().is_some_and(is_final) {
            return Ok(id);
        }
    }
    Err(FunctionCallError::RespondToModel(format!(
        "status observation ended for target {id}"
    )))
}

async fn wait_for_actionable_activity(
    session: &Arc<crate::session::session::Session>,
    turn_id: &str,
    mut activity: tokio::sync::watch::Receiver<InputQueueActivity>,
    targets: &[ThreadId],
    return_when: CollabAgentWaitReturnWhen,
    current_path: Option<AgentPath>,
) -> Result<WaitOutcome, FunctionCallError> {
    let mut completions = FuturesUnordered::new();
    for id in targets {
        completions.push(wait_for_target_completion(
            Arc::clone(&session.services.agent_control),
            *id,
            current_path.clone(),
        ));
    }
    let mut completed = HashSet::new();
    loop {
        if let Some(activity) = session
            .input_queue
            .actionable_wait_activity(&session.active_turn, turn_id, !targets.is_empty())
            .await
        {
            return Ok(WaitOutcome::new(match activity {
                InputQueueActivity::Mailbox => CollabAgentWaitCompletionReason::ActionableMessage,
                InputQueueActivity::Steer => CollabAgentWaitCompletionReason::UserInput,
            }));
        }
        tokio::select! {
            result = completions.next(), if !completions.is_empty() => {
                if let Some(result) = result {
                    completed.insert(result?);
                    if matches!(return_when, CollabAgentWaitReturnWhen::Any) || completed.len() == targets.len() {
                        let reason = match return_when {
                            CollabAgentWaitReturnWhen::Any => CollabAgentWaitCompletionReason::AnyTargetCompleted,
                            CollabAgentWaitReturnWhen::All => CollabAgentWaitCompletionReason::AllTargetsCompleted,
                        };
                        return Ok(WaitOutcome {
                            reason,
                            completed_target_thread_ids: targets
                                .iter()
                                .copied()
                                .filter(|id| completed.contains(id))
                                .collect(),
                        });
                    }
                }
            }
            changed = activity.changed() => {
                if changed.is_err() {
                    return Ok(WaitOutcome::new(CollabAgentWaitCompletionReason::TimedOut));
                }
                activity.borrow_and_update();
            }
        }
    }
}

// Native wait timeouts are leases, not a model-visible result. Keep the same
// future and its subscriptions alive across lease renewals.
async fn wait_with_policy<F>(
    wait: F,
    native_event_wait: bool,
    timeout_ms: i64,
) -> Result<WaitOutcome, FunctionCallError>
where
    F: Future<Output = Result<WaitOutcome, FunctionCallError>>,
{
    if !native_event_wait {
        return tokio::time::timeout(Duration::from_millis(timeout_ms as u64), wait)
            .await
            .unwrap_or_else(|_| Ok(WaitOutcome::new(CollabAgentWaitCompletionReason::TimedOut)));
    }
    if timeout_ms == 0 {
        return wait.await;
    }
    tokio::pin!(wait);
    loop {
        match tokio::time::timeout(Duration::from_millis(timeout_ms as u64), &mut wait).await {
            Ok(result) => return result,
            Err(_) => tracing::trace!("native agent wait lease renewed"),
        }
    }
}

#[cfg(test)]
mod native_wait_tests {
    use super::*;

    #[tokio::test]
    async fn native_wait_renews_without_completing_or_restarting_the_future() {
        let result = wait_with_policy(
            async {
                tokio::time::sleep(Duration::from_millis(40)).await;
                Ok(WaitOutcome::new(
                    CollabAgentWaitCompletionReason::ActionableMessage,
                ))
            },
            true,
            10,
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            WaitOutcome::new(CollabAgentWaitCompletionReason::ActionableMessage)
        );
    }

    #[tokio::test]
    async fn ordinary_wait_still_times_out() {
        let result = wait_with_policy(
            async {
                tokio::time::sleep(Duration::from_millis(40)).await;
                Ok(WaitOutcome::new(
                    CollabAgentWaitCompletionReason::ActionableMessage,
                ))
            },
            false,
            10,
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            WaitOutcome::new(CollabAgentWaitCompletionReason::TimedOut)
        );
    }

    #[test]
    fn wait_result_preserves_target_completion_metadata() {
        let first = ThreadId::new();
        let second = ThreadId::new();
        let result = WaitAgentResult::from_outcome(
            WaitOutcome {
                reason: CollabAgentWaitCompletionReason::AnyTargetCompleted,
                completed_target_thread_ids: vec![second],
            },
            vec![first, second],
            CollabAgentWaitReturnWhen::Any,
            None,
            1000,
        );
        assert_eq!(result.receiver_thread_ids, vec![first, second]);
        assert_eq!(result.completed_target_thread_ids, vec![second]);
        assert_eq!(result.return_when, CollabAgentWaitReturnWhen::Any);
        assert_eq!(
            result.completion_reason,
            CollabAgentWaitCompletionReason::AnyTargetCompleted
        );
        assert!(!result.timed_out);
    }
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentResult {
    pub(crate) message: String,
    pub(crate) timed_out: bool,
    pub(crate) completion_reason: CollabAgentWaitCompletionReason,
    pub(crate) return_when: CollabAgentWaitReturnWhen,
    pub(crate) receiver_thread_ids: Vec<ThreadId>,
    pub(crate) completed_target_thread_ids: Vec<ThreadId>,
}

impl WaitAgentResult {
    fn from_outcome(
        outcome: WaitOutcome,
        receiver_thread_ids: Vec<ThreadId>,
        return_when: CollabAgentWaitReturnWhen,
        requested_timeout_ms: Option<i64>,
        timeout_ms: i64,
    ) -> Self {
        let message = match outcome.reason {
            CollabAgentWaitCompletionReason::AnyTargetCompleted
            | CollabAgentWaitCompletionReason::AllTargetsCompleted
            | CollabAgentWaitCompletionReason::ActionableMessage => "Wait completed.",
            CollabAgentWaitCompletionReason::UserInput => "Wait interrupted by new input.",
            CollabAgentWaitCompletionReason::TimedOut => "Wait timed out.",
        };
        let message = match requested_timeout_ms {
            Some(requested_timeout_ms) if requested_timeout_ms < timeout_ms => format!(
                "{message}\n\nRequested timeout of {requested_timeout_ms}ms was clamped to the minimum of {timeout_ms}ms."
            ),
            Some(_) | None => message.to_string(),
        };
        Self {
            message,
            timed_out: outcome.reason == CollabAgentWaitCompletionReason::TimedOut,
            completion_reason: outcome.reason,
            return_when,
            receiver_thread_ids,
            completed_target_thread_ids: outcome.completed_target_thread_ids,
        }
    }
}

impl ToolOutput for WaitAgentResult {
    fn log_output(&self) -> String {
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
