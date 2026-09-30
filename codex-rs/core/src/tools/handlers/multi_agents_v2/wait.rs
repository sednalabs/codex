use super::*;
use crate::agent::api::AgentOutcomeSnapshot;
use crate::agent::api::AgentReadiness;
use crate::agent::status::is_final;
use crate::session::InputQueueActivity;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_protocol::ThreadId;
use codex_protocol::items::AgentWaitReason;
use codex_protocol::items::AgentWaitWakeCause;
use codex_tools::ToolSpec;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio::time::sleep_until;

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
        if args.targets.is_empty() && args.return_when == ReturnWhen::All {
            return Err(FunctionCallError::RespondToModel(
                "return_when=all requires at least one target".to_string(),
            ));
        }

        let local_control = session
            .services
            .local_agent_runtime
            .control(session.session_id());
        let mut receiver_thread_ids = Vec::new();
        let mut receiver_agents = Vec::new();
        let mut target_by_thread_id = HashMap::new();
        let mut seen = HashSet::new();
        for target in &args.targets {
            let id = resolve_agent_target(&session, &turn, target).await?;
            if id == session.thread_id {
                return Err(FunctionCallError::RespondToModel(
                    "wait_agent cannot wait on its own thread".to_string(),
                ));
            }
            if !seen.insert(id) {
                continue;
            }
            let metadata = local_control.get_agent_metadata(id).unwrap_or_default();
            target_by_thread_id.insert(
                id,
                metadata
                    .agent_path
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| id.to_string()),
            );
            receiver_agents.push(CollabAgentRef {
                thread_id: id,
                agent_nickname: metadata.agent_nickname,
                agent_role: metadata.agent_role,
            });
            receiver_thread_ids.push(id);
        }

        // Subscribe before inspecting pending work. The queue and each producer watch
        // retain the newest state, so publication at this boundary cannot be lost.
        let turn_state = session
            .input_queue
            .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
            .await;
        let (mut activity_rx, pending_activity) = session
            .input_queue
            .subscribe_activity(turn_state.as_deref())
            .await;
        let mut target_rxs = Vec::with_capacity(receiver_thread_ids.len());
        let mut snapshots = HashMap::with_capacity(receiver_thread_ids.len());
        for id in &receiver_thread_ids {
            let raw_status = local_control.get_status(*id).await;
            let publisher = session.services.local_agent_runtime.outcome_publisher(*id);
            let rx = publisher.subscribe_actionable();
            let mut snapshot = publisher.snapshot();
            // Old/unbound runtimes have no typed publication. Preserve their final
            // status instead of treating missing metadata as a quiet goal turn.
            if matches!(&raw_status, AgentStatus::NotFound)
                || (snapshot.turn_id.is_none() && is_final(&raw_status))
            {
                snapshot.status = raw_status;
                snapshot.readiness = AgentReadiness::Terminal;
            } else if snapshot.turn_id.is_none() && matches!(&raw_status, AgentStatus::Interrupted)
            {
                snapshot.status = raw_status;
                snapshot.readiness = AgentReadiness::ActionRequired;
            }
            snapshots.insert(*id, snapshot);
            target_rxs.push((*id, rx));
        }

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
                    wait_reason: None,
                    wait_wake_cause: None,
                    queued_update_count: None,
                }),
            )
            .await;

        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        let outcome = wait_for_event(
            &mut activity_rx,
            pending_activity,
            target_rxs,
            snapshots,
            args.return_when,
            deadline,
        )
        .await;
        let status_by_id = outcome
            .snapshots
            .iter()
            .map(|(id, snapshot)| (*id, snapshot.status.clone()))
            .collect::<HashMap<_, _>>();
        let status = status_by_id
            .iter()
            .filter_map(|(id, status)| {
                target_by_thread_id
                    .get(id)
                    .map(|target| (target.clone(), status.clone()))
            })
            .collect();
        let queued_update_count = session
            .input_queue
            .queued_update_count(turn_state.as_deref())
            .await;
        let result = WaitAgentResult::from_outcome(
            outcome.reason,
            outcome.wake_cause,
            status,
            Some(queued_update_count),
            requested_timeout_ms,
            timeout_ms,
        );
        let tool_status = if status_by_id
            .values()
            .any(|status| matches!(status, AgentStatus::Errored(_) | AgentStatus::NotFound))
        {
            CollabAgentToolCallStatus::Failed
        } else {
            CollabAgentToolCallStatus::Completed
        };

        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status: tool_status,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids,
                    receiver_agents,
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: status_by_id,
                    wait_reason: Some(match result.reason {
                        WaitReason::TargetTerminal => AgentWaitReason::TargetTerminal,
                        WaitReason::MailboxActivity => AgentWaitReason::MailboxActivity,
                        WaitReason::Steered => AgentWaitReason::Steered,
                        WaitReason::TimedOut => AgentWaitReason::TimedOut,
                        WaitReason::Unknown => AgentWaitReason::Unknown,
                    }),
                    wait_wake_cause: Some(match result.wake_cause {
                        WakeCause::TargetStatus => AgentWaitWakeCause::TargetStatus,
                        WakeCause::MailboxTurnRequested => AgentWaitWakeCause::MailboxTurnRequested,
                        WakeCause::OperatorSteer => AgentWaitWakeCause::OperatorSteer,
                        WakeCause::Timeout => AgentWaitWakeCause::Timeout,
                        WakeCause::Unknown => AgentWaitWakeCause::Unknown,
                    }),
                    queued_update_count: result.queued_update_count,
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum ReturnWhen {
    #[default]
    Any,
    All,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    #[serde(default)]
    targets: Vec<String>,
    #[serde(default)]
    return_when: ReturnWhen,
    timeout_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WaitReason {
    TargetTerminal,
    MailboxActivity,
    Steered,
    TimedOut,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WakeCause {
    TargetStatus,
    MailboxTurnRequested,
    OperatorSteer,
    Timeout,
    Unknown,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentResult {
    pub(crate) message: String,
    pub(crate) timed_out: bool,
    pub(crate) reason: WaitReason,
    pub(crate) wake_cause: WakeCause,
    /// Raw statuses are diagnostic; callers must use `reason` for readiness.
    pub(crate) status: HashMap<String, AgentStatus>,
    /// Queue-only updates observed at return, saturated at u32::MAX. This does
    /// not mean they caused the wake or started a model turn.
    pub(crate) queued_update_count: Option<u32>,
}

impl WaitAgentResult {
    fn from_outcome(
        reason: WaitReason,
        wake_cause: WakeCause,
        status: HashMap<String, AgentStatus>,
        queued_update_count: Option<u32>,
        requested_timeout_ms: Option<i64>,
        timeout_ms: i64,
    ) -> Self {
        let message = match (reason, wake_cause) {
            (WaitReason::TargetTerminal, _) => {
                "A target reached a logical terminal or actionable state."
            }
            (WaitReason::MailboxActivity, _) => "Mailbox activity requested a turn.",
            (WaitReason::Steered, _) => "Wait interrupted by new input.",
            (WaitReason::TimedOut, _) => "Wait timed out.",
            (WaitReason::Unknown, _) => "Wait ended with an unknown wake cause.",
        };
        let message = match requested_timeout_ms {
            Some(requested_timeout_ms) if requested_timeout_ms < timeout_ms => format!(
                "{message}\n\nRequested timeout of {requested_timeout_ms}ms was clamped to the minimum of {timeout_ms}ms."
            ),
            Some(_) | None => message.to_string(),
        };
        Self {
            message,
            timed_out: reason == WaitReason::TimedOut,
            reason,
            wake_cause,
            status,
            queued_update_count,
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

struct WaitOutcome {
    reason: WaitReason,
    wake_cause: WakeCause,
    snapshots: HashMap<ThreadId, AgentOutcomeSnapshot>,
}

fn terminal_rule_satisfied(
    snapshots: &HashMap<ThreadId, AgentOutcomeSnapshot>,
    return_when: ReturnWhen,
) -> bool {
    if snapshots.is_empty() {
        return false;
    }
    match return_when {
        ReturnWhen::Any => snapshots
            .values()
            .any(|snapshot| snapshot.readiness.wakes_wait()),
        ReturnWhen::All => snapshots
            .values()
            .all(|snapshot| snapshot.readiness.wakes_wait()),
    }
}

fn mailbox_wake_matches(activity: InputQueueActivity) -> bool {
    match activity {
        InputQueueActivity::Steer => true,
        InputQueueActivity::Mailbox { trigger_turn } => trigger_turn,
    }
}

fn activity_result(activity: InputQueueActivity) -> (WaitReason, WakeCause) {
    match activity {
        InputQueueActivity::Steer => (WaitReason::Steered, WakeCause::OperatorSteer),
        InputQueueActivity::Mailbox {
            trigger_turn: false,
        } => (WaitReason::Unknown, WakeCause::Unknown),
        InputQueueActivity::Mailbox { trigger_turn: true } => {
            (WaitReason::MailboxActivity, WakeCause::MailboxTurnRequested)
        }
    }
}

async fn next_target_update(
    id: ThreadId,
    mut rx: broadcast::Receiver<AgentOutcomeSnapshot>,
) -> (
    ThreadId,
    Result<AgentOutcomeSnapshot, broadcast::error::RecvError>,
) {
    (id, rx.recv().await)
}

async fn wait_for_event(
    activity_rx: &mut watch::Receiver<InputQueueActivity>,
    pending_activity: Option<InputQueueActivity>,
    target_rxs: Vec<(ThreadId, broadcast::Receiver<AgentOutcomeSnapshot>)>,
    mut snapshots: HashMap<ThreadId, AgentOutcomeSnapshot>,
    return_when: ReturnWhen,
    deadline: Instant,
) -> WaitOutcome {
    if terminal_rule_satisfied(&snapshots, return_when) {
        return WaitOutcome {
            reason: WaitReason::TargetTerminal,
            wake_cause: WakeCause::TargetStatus,
            snapshots,
        };
    }
    if let Some(activity) = pending_activity
        && mailbox_wake_matches(activity)
    {
        let (reason, wake_cause) = activity_result(activity);
        return WaitOutcome {
            reason,
            wake_cause,
            snapshots,
        };
    }
    let mut updates = FuturesUnordered::new();
    for (id, rx) in target_rxs {
        updates.push(next_target_update(id, rx));
    }
    let mut activity_open = true;
    loop {
        tokio::select! {
            _ = sleep_until(deadline) => {
                return WaitOutcome { reason: WaitReason::TimedOut, wake_cause: WakeCause::Timeout, snapshots };
            }
            result = activity_rx.changed(), if activity_open => {
                match result {
                    Ok(()) => {
                        let activity = *activity_rx.borrow_and_update();
                        if mailbox_wake_matches(activity) {
                            let (reason, wake_cause) = activity_result(activity);
                            return WaitOutcome { reason, wake_cause, snapshots };
                        }
                    }
                    Err(_) => {
                        activity_open = false;
                        if snapshots.is_empty() {
                            return WaitOutcome { reason: WaitReason::Unknown, wake_cause: WakeCause::Unknown, snapshots };
                        }
                    }
                }
            }
            Some((id, snapshot)) = updates.next(), if !updates.is_empty() => {
                if let Ok(snapshot) = snapshot {
                    snapshots.insert(id, snapshot);
                    if terminal_rule_satisfied(&snapshots, return_when) {
                        return WaitOutcome { reason: WaitReason::TargetTerminal, wake_cause: WakeCause::TargetStatus, snapshots };
                    }
                } else {
                    // A lagged or closed event stream cannot establish which turn
                    // changed; never mislabel it as a target terminal transition.
                    return WaitOutcome { reason: WaitReason::Unknown, wake_cause: WakeCause::Unknown, snapshots };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::api::AgentOutcomePublisher;

    fn snapshot(id: &str, readiness: AgentReadiness) -> AgentOutcomeSnapshot {
        AgentOutcomeSnapshot {
            turn_id: Some(id.to_string()),
            status: AgentStatus::Completed(Some(id.to_string())),
            readiness,
        }
    }

    #[test]
    fn terminal_rule_keeps_goal_progress_pending_and_preserves_any_all() {
        let a = ThreadId::new();
        let b = ThreadId::new();
        let mut statuses = HashMap::from([
            (
                a,
                snapshot(
                    "a1",
                    AgentReadiness::GoalContinuing {
                        goal_id: "g1".to_string(),
                        turn_id: "a1".to_string(),
                    },
                ),
            ),
            (b, snapshot("b1", AgentReadiness::Pending)),
        ]);
        assert!(!terminal_rule_satisfied(&statuses, ReturnWhen::Any));
        statuses.insert(a, snapshot("a2", AgentReadiness::Terminal));
        assert!(terminal_rule_satisfied(&statuses, ReturnWhen::Any));
        assert!(!terminal_rule_satisfied(&statuses, ReturnWhen::All));
        statuses.insert(b, snapshot("b2", AgentReadiness::ActionRequired));
        assert!(terminal_rule_satisfied(&statuses, ReturnWhen::All));
    }

    #[test]
    fn queued_mail_is_not_an_exact_target_wake() {
        let queue_only = InputQueueActivity::Mailbox {
            trigger_turn: false,
        };
        assert!(!mailbox_wake_matches(queue_only));
        assert!(mailbox_wake_matches(InputQueueActivity::Steer));
        assert!(mailbox_wake_matches(InputQueueActivity::Mailbox {
            trigger_turn: true
        },));
    }

    #[tokio::test]
    async fn targeted_wait_stays_pending_through_intermediate_success() {
        let id = ThreadId::new();
        let publisher = AgentOutcomePublisher::default();
        let rx = publisher.subscribe_actionable();
        let (activity_tx, mut activity_rx) = watch::channel(InputQueueActivity::Steer);
        let initial = HashMap::from([(id, publisher.snapshot())]);
        let mut waiting = Box::pin(wait_for_event(
            &mut activity_rx,
            None,
            vec![(id, rx)],
            initial,
            ReturnWhen::Any,
            Instant::now() + Duration::from_secs(1),
        ));
        publisher.publish(snapshot(
            "turn-1",
            AgentReadiness::GoalContinuing {
                goal_id: "goal-1".to_string(),
                turn_id: "turn-1".to_string(),
            },
        ));
        publisher.publish(snapshot(
            "turn-2",
            AgentReadiness::GoalContinuing {
                goal_id: "goal-1".to_string(),
                turn_id: "turn-2".to_string(),
            },
        ));
        activity_tx.send_replace(InputQueueActivity::Mailbox {
            trigger_turn: false,
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiting)
                .await
                .is_err()
        );
        publisher.publish(snapshot("turn-3", AgentReadiness::Terminal));
        let result = waiting.await;
        assert_eq!(result.reason, WaitReason::TargetTerminal);
        assert_eq!(result.wake_cause, WakeCause::TargetStatus);
        assert_eq!(result.snapshots[&id].turn_id.as_deref(), Some("turn-3"));
    }

    #[tokio::test]
    async fn targetless_wait_ignores_queued_mail_until_a_waking_event() {
        let (activity_tx, mut activity_rx) = watch::channel(InputQueueActivity::Mailbox {
            trigger_turn: false,
        });
        let mut waiting = Box::pin(wait_for_event(
            &mut activity_rx,
            Some(InputQueueActivity::Mailbox {
                trigger_turn: false,
            }),
            Vec::new(),
            HashMap::new(),
            ReturnWhen::Any,
            Instant::now() + Duration::from_secs(1),
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut waiting)
                .await
                .is_err()
        );
        activity_tx.send_replace(InputQueueActivity::Mailbox { trigger_turn: true });
        let result = waiting.await;
        assert_eq!(result.reason, WaitReason::MailboxActivity);
        assert_eq!(result.wake_cause, WakeCause::MailboxTurnRequested);
    }

    #[tokio::test]
    async fn actionable_publication_survives_watch_coalescing() {
        let id = ThreadId::new();
        let publisher = AgentOutcomePublisher::default();
        let rx = publisher.subscribe_actionable();
        publisher.publish(snapshot("turn-1", AgentReadiness::Terminal));
        publisher.publish(AgentOutcomeSnapshot {
            turn_id: Some("turn-2".to_string()),
            status: AgentStatus::Running,
            readiness: AgentReadiness::Pending,
        });
        let (_activity_tx, mut activity_rx) = watch::channel(InputQueueActivity::Steer);
        let result = wait_for_event(
            &mut activity_rx,
            None,
            vec![(id, rx)],
            HashMap::from([(id, publisher.snapshot())]),
            ReturnWhen::Any,
            Instant::now() + Duration::from_secs(1),
        )
        .await;
        assert_eq!(result.reason, WaitReason::TargetTerminal);
        assert_eq!(result.snapshots[&id].turn_id.as_deref(), Some("turn-1"));
    }
}
