use super::*;
use crate::agent::agent_resolver::resolve_agent_target;
use crate::agent::api::AgentWaitRegistration;
use crate::agent::api::AgentWaitResult;
use crate::agent::api::AgentWaitReturnWhen;
use crate::session::ExplicitActivitySignal;
use crate::session::InputQueue;
use crate::session::InputQueueActivity;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_protocol::ThreadId;
use codex_protocol::items::WaitAgentOutcome;
use codex_protocol::protocol::AgentStatus;
use codex_tools::ToolSpec;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::watch;
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
        let requested_timeout_ms = args.timeout_ms;
        let mut targets = Vec::with_capacity(args.targets.len());
        for target in &args.targets {
            targets.push(resolve_agent_target(&session, &turn, target).await?);
        }
        let mut unique_targets = std::collections::HashSet::with_capacity(targets.len());
        if targets.iter().any(|target| !unique_targets.insert(*target)) {
            return Err(FunctionCallError::RespondToModel(
                "targets must resolve to unique agents".to_string(),
            ));
        }
        for target in &targets {
            if !session.services.local_agent_runtime.owns_agent(*target) {
                return Err(FunctionCallError::RespondToModel(
                    "targeted completion waits require an agent managed by the local runtime"
                        .to_string(),
                ));
            }
        }
        let current_agent_path = turn.session_source.get_agent_path().or_else(|| {
            session
                .services
                .local_agent_runtime
                .agent_metadata(session.thread_id)
                .and_then(|metadata| metadata.agent_path)
        });
        let mut explicit_target_paths = Vec::with_capacity(targets.len());
        for target in &targets {
            let target_agent_path = session
                .services
                .local_agent_runtime
                .agent_metadata(*target)
                .and_then(|metadata| metadata.agent_path);
            if let Some(message) =
                reverse_wait_error(current_agent_path.as_ref(), target_agent_path.as_ref())
            {
                return Err(FunctionCallError::RespondToModel(message));
            }
            if let Some(target_path) = target_agent_path {
                explicit_target_paths.push((*target, target_path));
            }
        }
        let timeout_ms = match requested_timeout_ms {
            Some(ms) if ms > max_timeout_ms => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "timeout_ms must be at most {max_timeout_ms}"
                )));
            }
            Some(ms) => ms.max(min_timeout_ms),
            None => default_timeout_ms,
        };

        let turn_state = session
            .input_queue
            .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
            .await;
        let mailbox_enqueue_watermark = session.input_queue.mailbox_enqueue_watermark().await;
        let (mut activity_rx, pending_activity) = session
            .input_queue
            .subscribe_activity(turn_state.as_deref())
            .await;
        let explicit_activity_rx = session.input_queue.subscribe_explicit_activity();
        let recipient_path = session.registered_agent_path();
        let return_when = match args.return_when {
            ReturnWhen::Any => AgentWaitReturnWhen::Any,
            ReturnWhen::All => AgentWaitReturnWhen::All,
        };
        let mut agent_wait = (!targets.is_empty()).then(|| {
            session
                .services
                .local_agent_runtime
                .register_agent_wait(targets.clone(), return_when)
        });
        if let Some(registration) = agent_wait.as_mut() {
            for target in &targets {
                let status = session
                    .services
                    .local_agent_runtime
                    .raw_agent_status(*target)
                    .await;
                if let Some(status) = status {
                    registration.seed_raw_status(*target, status);
                }
            }
        }
        let initial_agent_outcome = agent_wait.as_mut().and_then(AgentWaitRegistration::current);
        let mut agents_states = agent_wait_states(initial_agent_outcome.as_ref());
        let receiver_agents = receiver_agent_refs(&session, &targets);

        session
            .emit_turn_item_started(
                &turn,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: targets.clone(),
                    receiver_agents: receiver_agents.clone(),
                    wait_outcome: None,
                    queued_update_count: None,
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: agents_states.clone(),
                }),
            )
            .await;

        let wait_started = Instant::now();
        let deadline = wait_started + Duration::from_millis(timeout_ms as u64);
        let outcome = if let Some(outcome) = initial_agent_outcome {
            WaitOutcome::TargetTerminal(outcome)
        } else {
            wait_for_activity_with_explicit(
                &mut activity_rx,
                pending_activity,
                deadline,
                &mut agent_wait,
                Some(ExplicitActivityWait {
                    activity_rx: explicit_activity_rx,
                    input_queue: &session.input_queue,
                    recipient_thread_id: session.thread_id,
                    recipient_path: recipient_path.as_ref(),
                    target_agents: &explicit_target_paths,
                    turn_state: turn_state.as_deref(),
                }),
            )
            .await
        };
        if let WaitOutcome::TargetTerminal(outcome) = &outcome {
            agents_states = agent_wait_states(Some(outcome));
        }
        let queued_update_count = session
            .input_queue
            .pending_mailbox_communication_count_since(mailbox_enqueue_watermark)
            .await;
        // A completed wait may wake for a message, user input, or its timeout.
        // Dropped waits do not have an observed outcome and are not included.
        turn.session_telemetry.record_duration(
            "codex.multi_agent.wait.duration_ms",
            wait_started.elapsed(),
            &[(
                "outcome",
                match &outcome {
                    WaitOutcome::MailboxActivity => "mailbox",
                    WaitOutcome::ExplicitActionableMessage { targetless: false } => {
                        "explicit_actionable_message"
                    }
                    WaitOutcome::ExplicitActionableMessage { targetless: true } => {
                        "targetless_actionable_message"
                    }
                    WaitOutcome::Steered => "steered",
                    WaitOutcome::TargetTerminal(_) => "target_terminal",
                    WaitOutcome::TimedOut => "timed_out",
                    WaitOutcome::SubscriptionLoss => "subscription_loss",
                },
            )],
        );
        let result = WaitAgentResult::from_outcome(&outcome, requested_timeout_ms, timeout_ms);
        let completed_item = completed_wait_item(
            call_id,
            session.thread_id,
            targets,
            receiver_agents,
            &outcome,
            queued_update_count,
            agents_states,
        );

        session
            .emit_turn_item_completed(&turn, TurnItem::CollabAgentToolCall(completed_item))
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
    #[serde(default)]
    targets: Vec<String>,
    timeout_ms: Option<i64>,
    #[serde(default)]
    return_when: ReturnWhen,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ReturnWhen {
    #[default]
    Any,
    All,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentResult {
    pub(crate) message: String,
    pub(crate) timed_out: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<WaitAgentOutcome>,
}

impl WaitAgentResult {
    fn from_outcome(
        outcome: &WaitOutcome,
        requested_timeout_ms: Option<i64>,
        timeout_ms: i64,
    ) -> Self {
        let message = match outcome {
            WaitOutcome::MailboxActivity => "Wait completed.",
            WaitOutcome::ExplicitActionableMessage { targetless: false } => {
                "Wait completed for an actionable update addressed to this agent while targets were selected."
            }
            WaitOutcome::ExplicitActionableMessage { targetless: true } => {
                "Wait completed for an actionable update addressed to this agent."
            }
            WaitOutcome::Steered => "Wait interrupted by new input.",
            WaitOutcome::TargetTerminal(_) => "Target agent completion is actionable.",
            WaitOutcome::TimedOut => "Wait timed out.",
            WaitOutcome::SubscriptionLoss => "Wait ended because a subscription was lost.",
        };
        let message = match requested_timeout_ms {
            Some(requested_timeout_ms) if requested_timeout_ms < timeout_ms => format!(
                "{message}\n\nRequested timeout of {requested_timeout_ms}ms was clamped to the minimum of {timeout_ms}ms."
            ),
            Some(_) | None => message.to_string(),
        };
        Self {
            message,
            timed_out: matches!(outcome, WaitOutcome::TimedOut),
            outcome: Some(outcome.protocol_outcome()),
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

#[derive(Clone, Debug)]
enum WaitOutcome {
    MailboxActivity,
    ExplicitActionableMessage { targetless: bool },
    Steered,
    TargetTerminal(AgentWaitResult),
    TimedOut,
    SubscriptionLoss,
}

impl WaitOutcome {
    fn protocol_outcome(&self) -> WaitAgentOutcome {
        match self {
            Self::MailboxActivity => WaitAgentOutcome::UnattributedMailboxActivity,
            Self::ExplicitActionableMessage { targetless: false } => {
                WaitAgentOutcome::ExactTargetActionableMessage
            }
            Self::ExplicitActionableMessage { targetless: true } => {
                WaitAgentOutcome::TargetlessActionableMessage
            }
            Self::Steered => WaitAgentOutcome::OperatorSteer,
            Self::TargetTerminal(result) if result.all_targets => {
                WaitAgentOutcome::TargetTerminalAll
            }
            Self::TargetTerminal(_) => WaitAgentOutcome::TargetTerminalAny,
            Self::TimedOut => WaitAgentOutcome::Timeout,
            Self::SubscriptionLoss => WaitAgentOutcome::SubscriptionLoss,
        }
    }
}

fn completed_wait_item(
    id: String,
    sender_thread_id: ThreadId,
    receiver_thread_ids: Vec<ThreadId>,
    receiver_agents: Vec<codex_protocol::protocol::CollabAgentRef>,
    outcome: &WaitOutcome,
    queued_update_count: Option<u32>,
    agents_states: HashMap<ThreadId, AgentStatus>,
) -> CollabAgentToolCallItem {
    CollabAgentToolCallItem {
        id,
        tool: CollabAgentTool::Wait,
        status: CollabAgentToolCallStatus::Completed,
        sender_thread_id,
        receiver_thread_ids,
        receiver_agents,
        wait_outcome: Some(outcome.protocol_outcome()),
        queued_update_count,
        prompt: None,
        model: None,
        reasoning_effort: None,
        agents_states,
    }
}

#[cfg(test)]
async fn wait_for_activity(
    activity_rx: &mut tokio::sync::watch::Receiver<InputQueueActivity>,
    pending_activity: Option<InputQueueActivity>,
    deadline: Instant,
    agent_wait: &mut Option<AgentWaitRegistration>,
) -> WaitOutcome {
    wait_for_activity_with_explicit(
        activity_rx,
        pending_activity,
        deadline,
        agent_wait,
        /*explicit_wait*/ None,
    )
    .await
}

struct ExplicitActivityWait<'a> {
    activity_rx: watch::Receiver<ExplicitActivitySignal>,
    input_queue: &'a InputQueue,
    recipient_thread_id: ThreadId,
    recipient_path: Option<&'a codex_protocol::AgentPath>,
    target_agents: &'a [(ThreadId, codex_protocol::AgentPath)],
    turn_state: Option<&'a tokio::sync::Mutex<crate::state::TurnState>>,
}

struct ExplicitActivityMatcher<'a> {
    input_queue: &'a InputQueue,
    recipient_thread_id: ThreadId,
    recipient_path: Option<&'a codex_protocol::AgentPath>,
    target_agents: &'a [(ThreadId, codex_protocol::AgentPath)],
    turn_state: Option<&'a tokio::sync::Mutex<crate::state::TurnState>>,
}

fn explicit_actionable_outcome(matcher: &ExplicitActivityMatcher<'_>) -> WaitOutcome {
    WaitOutcome::ExplicitActionableMessage {
        targetless: matcher.target_agents.is_empty(),
    }
}

async fn wait_for_activity_with_explicit(
    activity_rx: &mut watch::Receiver<InputQueueActivity>,
    pending_activity: Option<InputQueueActivity>,
    deadline: Instant,
    agent_wait: &mut Option<AgentWaitRegistration>,
    explicit_wait: Option<ExplicitActivityWait<'_>>,
) -> WaitOutcome {
    let (mut explicit_activity_rx, explicit_matcher) = match explicit_wait {
        Some(wait) => (
            Some(wait.activity_rx),
            Some(ExplicitActivityMatcher {
                input_queue: wait.input_queue,
                recipient_thread_id: wait.recipient_thread_id,
                recipient_path: wait.recipient_path,
                target_agents: wait.target_agents,
                turn_state: wait.turn_state,
            }),
        ),
        None => (None, None),
    };
    if pending_activity == Some(InputQueueActivity::Steer)
        || pending_turn_steer(explicit_matcher.as_ref()).await
    {
        return WaitOutcome::Steered;
    }
    if let Some(outcome) = agent_wait.as_mut().and_then(AgentWaitRegistration::current) {
        return WaitOutcome::TargetTerminal(outcome);
    }
    if let Some(matcher) = explicit_matcher.as_ref()
        && matcher
            .input_queue
            .pending_explicit_activity_for_recipient(
                matcher.recipient_thread_id,
                matcher.recipient_path,
            )
            .await
            .is_some()
    {
        return explicit_actionable_outcome(matcher);
    }
    if let Some(activity) = pending_activity {
        return activity_wake_outcome(activity, agent_wait);
    }
    let has_agent_wait = agent_wait.is_some();
    let has_explicit_wait = explicit_activity_rx.is_some();
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return WaitOutcome::TimedOut,
            activity = activity_rx.changed() => match activity {
                Ok(()) => {
                    let observed_activity = *activity_rx.borrow_and_update();
                    if observed_activity == InputQueueActivity::Steer {
                        return WaitOutcome::Steered;
                    }
                    if pending_turn_steer(explicit_matcher.as_ref()).await {
                        return WaitOutcome::Steered;
                    }
                    if let Some(outcome) = agent_wait.as_mut().and_then(AgentWaitRegistration::current) {
                        return WaitOutcome::TargetTerminal(outcome);
                    }
                    if let Some(matcher) = explicit_matcher.as_ref()
                        && matcher
                            .input_queue
                            .pending_explicit_activity_for_recipient(
                                matcher.recipient_thread_id,
                                matcher.recipient_path,
                            )
                            .await
                            .is_some()
                    {
                        return explicit_actionable_outcome(matcher);
                    }
                    return activity_wake_outcome(observed_activity, agent_wait);
                }
                Err(_) => return WaitOutcome::SubscriptionLoss,
            },
            explicit = async {
                match explicit_activity_rx.as_mut() {
                    Some(activity_rx) => activity_rx.changed().await,
                    None => std::future::pending().await,
                }
            }, if has_explicit_wait => match explicit {
                Ok(()) => {
                    if pending_turn_steer(explicit_matcher.as_ref()).await {
                        return WaitOutcome::Steered;
                    }
                    if let Some(outcome) = agent_wait.as_mut().and_then(AgentWaitRegistration::current) {
                        return WaitOutcome::TargetTerminal(outcome);
                    }
                    if let Some(matcher) = explicit_matcher.as_ref()
                        && matcher
                            .input_queue
                            .pending_explicit_activity_for_recipient(
                                matcher.recipient_thread_id,
                                matcher.recipient_path,
                            )
                            .await
                            .is_some()
                    {
                        return explicit_actionable_outcome(matcher);
                    }
                }
                Err(_) => return WaitOutcome::SubscriptionLoss,
            },
            agent = async {
                match agent_wait.as_mut() {
                    Some(registration) => registration.changed().await,
                    None => std::future::pending().await,
                }
            }, if has_agent_wait => match agent {
                Ok(Some(outcome)) => return WaitOutcome::TargetTerminal(outcome),
                Ok(None) => {},
                Err(_) => return WaitOutcome::SubscriptionLoss,
            }
        }
    }
}

async fn pending_turn_steer(matcher: Option<&ExplicitActivityMatcher<'_>>) -> bool {
    let Some(matcher) = matcher else {
        return false;
    };
    matcher
        .input_queue
        .pending_turn_input_activity(matcher.turn_state)
        .await
        == Some(InputQueueActivity::Steer)
}

fn activity_wake_outcome(
    activity: InputQueueActivity,
    agent_wait: &mut Option<AgentWaitRegistration>,
) -> WaitOutcome {
    match activity {
        InputQueueActivity::Mailbox => agent_wait
            .as_mut()
            .and_then(AgentWaitRegistration::current)
            .map_or(WaitOutcome::MailboxActivity, WaitOutcome::TargetTerminal),
        InputQueueActivity::Steer => WaitOutcome::Steered,
    }
}

fn agent_wait_states(outcome: Option<&AgentWaitResult>) -> HashMap<ThreadId, AgentStatus> {
    outcome
        .into_iter()
        .flat_map(|outcome| outcome.outcomes.iter())
        .map(|(thread_id, outcome)| (*thread_id, outcome.status.clone()))
        .collect()
}

fn receiver_agent_refs(
    session: &crate::session::session::Session,
    targets: &[ThreadId],
) -> Vec<codex_protocol::protocol::CollabAgentRef> {
    targets
        .iter()
        .filter_map(|thread_id| {
            session
                .services
                .local_agent_runtime
                .agent_metadata(*thread_id)
                .map(|metadata| codex_protocol::protocol::CollabAgentRef {
                    thread_id: *thread_id,
                    agent_nickname: metadata.agent_nickname,
                    agent_role: metadata.agent_role,
                })
        })
        .collect()
}

fn reverse_wait_error(
    current_agent_path: Option<&codex_protocol::AgentPath>,
    target_agent_path: Option<&codex_protocol::AgentPath>,
) -> Option<String> {
    let (Some(current), Some(target)) = (current_agent_path, target_agent_path) else {
        return None;
    };
    let target_is_current_or_ancestor = target == current
        || current
            .as_str()
            .strip_prefix(target.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'));
    target_is_current_or_ancestor.then(|| {
        format!(
            "wait target `{target}` is the current agent or an ancestor of `{current}`; return the decision-complete result to the parent instead of waiting on it"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::api::AgentOutcomePublisher;
    use crate::agent::api::AgentOutcomeSnapshot;
    use crate::agent::api::AgentReadiness;
    use crate::agent::api::AgentWaitRegistry;
    use crate::agent::api::AgentWaitReturnWhen;
    use crate::agent::api::register_agent_wait;
    use crate::session::ExplicitMailboxActivity;
    use crate::session::InputQueue;
    use codex_history::InterAgentCommunicationIntent;
    use codex_protocol::AgentPath;
    use codex_protocol::ResponseItemId;
    use codex_protocol::protocol::AgentStatus;
    use std::sync::Arc;
    use std::sync::Mutex;

    #[test]
    fn reverse_wait_rejects_self_and_ancestor_targets() {
        let current = AgentPath::try_from("/root/worker/child").expect("current path");
        let ancestor = AgentPath::try_from("/root/worker").expect("ancestor path");
        let sibling = AgentPath::try_from("/root/other").expect("sibling path");
        assert!(reverse_wait_error(Some(&current), Some(&current)).is_some());
        assert!(reverse_wait_error(Some(&current), Some(&ancestor)).is_some());
        assert!(reverse_wait_error(Some(&current), Some(&sibling)).is_none());
        assert!(reverse_wait_error(/*current_agent_path*/ None, Some(&ancestor)).is_none());
    }

    #[test]
    fn target_wait_outcome_preserves_any_vs_all() {
        let result = AgentWaitResult {
            all_targets: false,
            outcomes: Vec::new(),
        };
        assert_eq!(
            WaitOutcome::TargetTerminal(result).protocol_outcome(),
            WaitAgentOutcome::TargetTerminalAny
        );
        let result = AgentWaitResult {
            all_targets: true,
            outcomes: Vec::new(),
        };
        assert_eq!(
            WaitOutcome::TargetTerminal(result).protocol_outcome(),
            WaitAgentOutcome::TargetTerminalAll
        );
    }

    fn wait_mail(
        content: &str,
        trigger_turn: bool,
    ) -> codex_protocol::protocol::InterAgentCommunication {
        codex_protocol::protocol::InterAgentCommunication::new(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            Vec::new(),
            content.to_string(),
            trigger_turn,
        )
    }

    fn explicit_wait_mail(
        update_id: ResponseItemId,
        sender_path: AgentPath,
        content: &str,
    ) -> codex_protocol::protocol::InterAgentCommunication {
        let mut communication = codex_protocol::protocol::InterAgentCommunication::new(
            sender_path,
            AgentPath::root(),
            Vec::new(),
            content.to_string(),
            /*trigger_turn*/ false,
        );
        communication.id = Some(update_id);
        communication
    }

    fn explicit_wait_activity(
        update_id: ResponseItemId,
        intent: InterAgentCommunicationIntent,
        sender_thread_id: ThreadId,
        recipient_thread_id: ThreadId,
        sender_path: AgentPath,
    ) -> ExplicitMailboxActivity {
        ExplicitMailboxActivity {
            update_id,
            intent,
            sender_thread_id,
            recipient_thread_id,
            sender_path,
            recipient_path: AgentPath::root(),
            generation: None,
        }
    }

    #[tokio::test]
    async fn queue_only_mail_waits_for_followup_and_is_delivered_once() {
        let registry = Arc::new(Mutex::new(AgentWaitRegistry::default()));
        let target = ThreadId::new();
        let mut agent_wait = Some(register_agent_wait(
            &registry,
            vec![target],
            AgentWaitReturnWhen::Any,
        ));
        let input_queue = InputQueue::new();
        let watermark = input_queue.mailbox_enqueue_watermark().await;
        let (mut activity_rx, pending) = input_queue.subscribe_activity(/*turn_state*/ None).await;
        let mut wait = Box::pin(wait_for_activity(
            &mut activity_rx,
            pending,
            Instant::now() + Duration::from_secs(60),
            &mut agent_wait,
        ));
        assert!(futures::poll!(wait.as_mut()).is_pending());

        let quiet = wait_mail("quiet update", /*trigger_turn*/ false);
        input_queue
            .enqueue_mailbox_communication(quiet.clone(), Default::default())
            .await;
        assert!(futures::poll!(wait.as_mut()).is_pending());
        assert!(input_queue.has_pending_mailbox_items().await);
        assert_eq!(
            input_queue
                .pending_mailbox_communication_count_since(watermark)
                .await,
            Some(1)
        );

        let followup = wait_mail("follow up", /*trigger_turn*/ true);
        input_queue
            .enqueue_mailbox_communication(followup.clone(), Default::default())
            .await;
        assert!(matches!(wait.await, WaitOutcome::MailboxActivity));
        assert_eq!(
            input_queue.drain_mailbox_input_items().await.0,
            vec![
                crate::session::TurnInput::InterAgentCommunication(quiet),
                crate::session::TurnInput::InterAgentCommunication(followup),
            ]
        );
        assert!(input_queue.drain_mailbox_input_items().await.0.is_empty());
    }

    #[tokio::test]
    async fn already_pending_queue_only_mail_does_not_complete_wait() {
        let input_queue = InputQueue::new();
        input_queue
            .enqueue_mailbox_communication(
                wait_mail("already queued", /*trigger_turn*/ false),
                Default::default(),
            )
            .await;
        let (mut activity_rx, pending) = input_queue.subscribe_activity(/*turn_state*/ None).await;
        assert_eq!(pending, None);
        let mut agent_wait = None;
        let mut wait = Box::pin(wait_for_activity(
            &mut activity_rx,
            pending,
            Instant::now() + Duration::from_secs(60),
            &mut agent_wait,
        ));
        assert!(futures::poll!(wait.as_mut()).is_pending());
        input_queue
            .enqueue_mailbox_communication(
                wait_mail("follow up", /*trigger_turn*/ true),
                Default::default(),
            )
            .await;
        assert!(matches!(wait.await, WaitOutcome::MailboxActivity));
        assert_eq!(input_queue.drain_mailbox().await.len(), 2);
        assert!(input_queue.drain_mailbox().await.is_empty());
    }

    #[tokio::test]
    async fn queue_only_mail_does_not_hide_operator_steer() {
        let input_queue = InputQueue::new();
        let turn_state = tokio::sync::Mutex::new(crate::state::TurnState::default());
        let (mut activity_rx, pending) = input_queue.subscribe_activity(Some(&turn_state)).await;
        let mut agent_wait = None;
        let mut wait = Box::pin(wait_for_activity(
            &mut activity_rx,
            pending,
            Instant::now() + Duration::from_secs(60),
            &mut agent_wait,
        ));
        assert!(futures::poll!(wait.as_mut()).is_pending());
        input_queue
            .enqueue_mailbox_communication(
                wait_mail("quiet update", /*trigger_turn*/ false),
                Default::default(),
            )
            .await;
        assert!(futures::poll!(wait.as_mut()).is_pending());
        drop(wait);
        input_queue
            .extend_pending_input_for_turn_state(
                &turn_state,
                vec![crate::session::TurnInput::UserInput {
                    content: Vec::new(),
                    client_id: None,
                    metadata: Default::default(),
                }],
            )
            .await;
        let (mut activity_rx, pending) = input_queue.subscribe_activity(Some(&turn_state)).await;
        assert_eq!(pending, Some(InputQueueActivity::Steer));
        assert!(matches!(
            wait_for_activity(
                &mut activity_rx,
                pending,
                Instant::now() + Duration::from_secs(60),
                &mut agent_wait,
            )
            .await,
            WaitOutcome::Steered
        ));
        assert_eq!(input_queue.drain_mailbox().await.len(), 1);
    }

    #[tokio::test]
    async fn active_wait_wakes_when_operator_steer_arrives() {
        let input_queue = InputQueue::new();
        let turn_state = tokio::sync::Mutex::new(crate::state::TurnState::default());
        let (mut activity_rx, pending) = input_queue.subscribe_activity(Some(&turn_state)).await;
        let mut agent_wait = None;
        let mut wait = Box::pin(wait_for_activity(
            &mut activity_rx,
            pending,
            Instant::now() + Duration::from_secs(60),
            &mut agent_wait,
        ));
        assert!(futures::poll!(wait.as_mut()).is_pending());

        input_queue
            .enqueue_mailbox_communication(
                wait_mail("quiet update", /*trigger_turn*/ false),
                Default::default(),
            )
            .await;
        assert!(futures::poll!(wait.as_mut()).is_pending());
        assert!(input_queue.has_pending_mailbox_items().await);

        input_queue
            .notify_operator_steer_for_test(&turn_state)
            .await;
        assert!(matches!(wait.await, WaitOutcome::Steered));
        assert_eq!(input_queue.drain_mailbox().await.len(), 1);
    }

    #[tokio::test]
    async fn active_target_wait_wakes_for_unselected_sender_action_but_not_progress_or_legacy_id() {
        let input_queue = InputQueue::new();
        let target = ThreadId::new();
        let unrelated = ThreadId::new();
        let recipient = ThreadId::new();
        let recipient_path = codex_protocol::AgentPath::root();
        let worker_path = AgentPath::try_from("/root/worker").expect("worker path");
        let unrelated_path = AgentPath::try_from("/root/unrelated").expect("other path");
        let explicit_targets = vec![(target, worker_path.clone())];
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(/*turn_state*/ None).await;
        let explicit_activity_rx = input_queue.subscribe_explicit_activity();
        assert_eq!(
            input_queue
                .pending_explicit_activity_for_recipient(recipient, Some(&recipient_path))
                .await,
            None
        );
        let mut agent_wait = None;
        let mut wait = Box::pin(wait_for_activity_with_explicit(
            &mut activity_rx,
            pending_activity,
            Instant::now() + Duration::from_secs(60),
            &mut agent_wait,
            Some(ExplicitActivityWait {
                activity_rx: explicit_activity_rx,
                input_queue: &input_queue,
                recipient_thread_id: recipient,
                recipient_path: Some(&recipient_path),
                target_agents: &explicit_targets,
                turn_state: None,
            }),
        ));
        assert!(futures::poll!(wait.as_mut()).is_pending());

        let legacy_id = ResponseItemId::new("legacy-public-id");
        input_queue
            .enqueue_mailbox_communication(
                explicit_wait_mail(legacy_id, worker_path.clone(), "legacy body"),
                Default::default(),
            )
            .await;
        assert!(futures::poll!(wait.as_mut()).is_pending());

        let progress_id = ResponseItemId::new("progress-quiet");
        input_queue
            .enqueue_explicit_mailbox_communication(
                explicit_wait_mail(progress_id.clone(), worker_path.clone(), "progress body"),
                Default::default(),
                explicit_wait_activity(
                    progress_id,
                    InterAgentCommunicationIntent::Progress,
                    target,
                    recipient,
                    worker_path.clone(),
                ),
            )
            .await;
        assert!(futures::poll!(wait.as_mut()).is_pending());

        let unrelated_id = ResponseItemId::new("unmatched-action");
        input_queue
            .enqueue_explicit_mailbox_communication(
                explicit_wait_mail(unrelated_id.clone(), unrelated_path.clone(), "other body"),
                Default::default(),
                explicit_wait_activity(
                    unrelated_id,
                    InterAgentCommunicationIntent::ActionRequired,
                    unrelated,
                    recipient,
                    unrelated_path,
                ),
            )
            .await;
        let WaitOutcome::ExplicitActionableMessage { targetless: false } = wait.await else {
            panic!("an addressed action from any authorized sender should wake the target wait");
        };
        let activity = input_queue
            .pending_explicit_activity_for_recipient(recipient, Some(&recipient_path))
            .await
            .expect("the actionable message remains pending after the wait wake");
        assert_eq!(activity.update_id, unrelated_id);
        assert_eq!(activity.sender_thread_id, unrelated);
        assert_eq!(activity.recipient_thread_id, recipient);
        assert_eq!(activity.sender_path, worker_path);
        assert_eq!(activity.recipient_path, AgentPath::root());
        assert_eq!(
            activity.intent,
            InterAgentCommunicationIntent::ActionRequired
        );
        assert_eq!(activity.generation, Some(2));
        assert_eq!(
            WaitOutcome::ExplicitActionableMessage { targetless: false }.protocol_outcome(),
            WaitAgentOutcome::ExactTargetActionableMessage
        );
        assert!(input_queue.has_pending_mailbox_items().await);
    }

    #[tokio::test]
    async fn targetless_wait_wakes_for_recipient_local_result_ready() {
        let input_queue = InputQueue::new();
        let sender = ThreadId::new();
        let recipient = ThreadId::new();
        let recipient_path = codex_protocol::AgentPath::root();
        let sender_path = AgentPath::try_from("/root/worker").expect("worker path");
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(/*turn_state*/ None).await;
        let explicit_activity_rx = input_queue.subscribe_explicit_activity();
        let mut agent_wait = None;
        let mut wait = Box::pin(wait_for_activity_with_explicit(
            &mut activity_rx,
            pending_activity,
            Instant::now() + Duration::from_secs(60),
            &mut agent_wait,
            Some(ExplicitActivityWait {
                activity_rx: explicit_activity_rx,
                input_queue: &input_queue,
                recipient_thread_id: recipient,
                recipient_path: Some(&recipient_path),
                target_agents: &[],
                turn_state: None,
            }),
        ));
        assert!(futures::poll!(wait.as_mut()).is_pending());

        let update_id = ResponseItemId::new("targetless-result-ready");
        input_queue
            .enqueue_explicit_mailbox_communication(
                explicit_wait_mail(update_id.clone(), sender_path.clone(), "result body"),
                Default::default(),
                explicit_wait_activity(
                    update_id.clone(),
                    InterAgentCommunicationIntent::ResultReady,
                    sender,
                    recipient,
                    sender_path,
                ),
            )
            .await;

        assert!(matches!(
            wait.await,
            WaitOutcome::ExplicitActionableMessage { targetless: true }
        ));
        assert_eq!(
            WaitOutcome::ExplicitActionableMessage { targetless: true }.protocol_outcome(),
            WaitAgentOutcome::TargetlessActionableMessage
        );
        assert_eq!(
            input_queue
                .pending_explicit_activity_for_recipient(recipient, Some(&recipient_path))
                .await
                .map(|activity| activity.update_id),
            Some(update_id)
        );
    }

    #[tokio::test]
    async fn pending_explicit_action_does_not_override_already_pending_steer() {
        let input_queue = InputQueue::new();
        let target = ThreadId::new();
        let recipient = ThreadId::new();
        let recipient_path = codex_protocol::AgentPath::root();
        let target_path = AgentPath::try_from("/root/worker").expect("worker path");
        let target_agents = vec![(target, target_path.clone())];
        let turn_state = tokio::sync::Mutex::new(crate::state::TurnState::default());
        input_queue
            .notify_operator_steer_for_test(&turn_state)
            .await;
        let update_id = ResponseItemId::new("pending-action-with-steer");
        input_queue
            .enqueue_explicit_mailbox_communication(
                explicit_wait_mail(update_id.clone(), target_path.clone(), "body"),
                Default::default(),
                explicit_wait_activity(
                    update_id,
                    InterAgentCommunicationIntent::ActionRequired,
                    target,
                    recipient,
                    target_path,
                ),
            )
            .await;

        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(Some(&turn_state)).await;
        let explicit_activity_rx = input_queue.subscribe_explicit_activity();
        assert!(
            input_queue
                .pending_explicit_activity_for_recipient(recipient, Some(&recipient_path))
                .await
                .is_some()
        );
        let mut agent_wait = None;
        let outcome = wait_for_activity_with_explicit(
            &mut activity_rx,
            pending_activity,
            Instant::now() + Duration::from_secs(60),
            &mut agent_wait,
            Some(ExplicitActivityWait {
                activity_rx: explicit_activity_rx,
                input_queue: &input_queue,
                recipient_thread_id: recipient,
                recipient_path: Some(&recipient_path),
                target_agents: &target_agents,
                turn_state: Some(&turn_state),
            }),
        )
        .await;
        assert!(matches!(outcome, WaitOutcome::Steered));
    }

    #[tokio::test]
    async fn active_operator_steer_wins_when_explicit_action_wakes_together() {
        let input_queue = InputQueue::new();
        let target = ThreadId::new();
        let recipient = ThreadId::new();
        let recipient_path = codex_protocol::AgentPath::root();
        let target_path = AgentPath::try_from("/root/worker").expect("worker path");
        let target_agents = vec![(target, target_path.clone())];
        let turn_state = tokio::sync::Mutex::new(crate::state::TurnState::default());
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(Some(&turn_state)).await;
        let explicit_activity_rx = input_queue.subscribe_explicit_activity();
        assert_eq!(
            input_queue
                .pending_explicit_activity_for_recipient(recipient, Some(&recipient_path))
                .await,
            None
        );
        let mut agent_wait = None;
        let mut wait = Box::pin(wait_for_activity_with_explicit(
            &mut activity_rx,
            pending_activity,
            Instant::now() + Duration::from_secs(60),
            &mut agent_wait,
            Some(ExplicitActivityWait {
                activity_rx: explicit_activity_rx,
                input_queue: &input_queue,
                recipient_thread_id: recipient,
                recipient_path: Some(&recipient_path),
                target_agents: &target_agents,
                turn_state: Some(&turn_state),
            }),
        ));
        assert!(futures::poll!(wait.as_mut()).is_pending());

        let update_id = ResponseItemId::new("racing-action-with-steer");
        input_queue
            .enqueue_explicit_mailbox_communication(
                explicit_wait_mail(update_id.clone(), target_path.clone(), "body"),
                Default::default(),
                explicit_wait_activity(
                    update_id,
                    InterAgentCommunicationIntent::ResultReady,
                    target,
                    recipient,
                    target_path,
                ),
            )
            .await;
        input_queue
            .notify_operator_steer_for_test(&turn_state)
            .await;

        assert!(matches!(wait.await, WaitOutcome::Steered));
    }

    #[tokio::test]
    async fn target_terminal_wins_when_explicit_action_wakes_together() {
        let input_queue = InputQueue::new();
        let registry = Arc::new(Mutex::new(AgentWaitRegistry::default()));
        let target = ThreadId::new();
        let recipient = ThreadId::new();
        let recipient_path = codex_protocol::AgentPath::root();
        let target_path = AgentPath::try_from("/root/worker").expect("worker path");
        let target_agents = vec![(target, target_path.clone())];
        let publisher = AgentOutcomePublisher::new(target, registry.clone());
        publisher.publish(AgentOutcomeSnapshot {
            turn_id: Some("turn-1".to_string()),
            status: AgentStatus::Running,
            readiness: AgentReadiness::Pending,
        });
        let mut agent_wait = Some(register_agent_wait(
            &registry,
            vec![target],
            AgentWaitReturnWhen::Any,
        ));
        let (mut activity_rx, pending_activity) =
            input_queue.subscribe_activity(/*turn_state*/ None).await;
        let explicit_activity_rx = input_queue.subscribe_explicit_activity();
        assert_eq!(
            input_queue
                .pending_explicit_activity_for_recipient(recipient, Some(&recipient_path))
                .await,
            None
        );
        let mut wait = Box::pin(wait_for_activity_with_explicit(
            &mut activity_rx,
            pending_activity,
            Instant::now() + Duration::from_secs(60),
            &mut agent_wait,
            Some(ExplicitActivityWait {
                activity_rx: explicit_activity_rx,
                input_queue: &input_queue,
                recipient_thread_id: recipient,
                recipient_path: Some(&recipient_path),
                target_agents: &target_agents,
                turn_state: None,
            }),
        ));
        assert!(futures::poll!(wait.as_mut()).is_pending());

        let update_id = ResponseItemId::new("racing-action-with-terminal");
        input_queue
            .enqueue_explicit_mailbox_communication(
                explicit_wait_mail(update_id.clone(), target_path.clone(), "body"),
                Default::default(),
                explicit_wait_activity(
                    update_id,
                    InterAgentCommunicationIntent::ActionRequired,
                    target,
                    recipient,
                    target_path,
                ),
            )
            .await;
        publisher.publish(AgentOutcomeSnapshot {
            turn_id: Some("turn-1".to_string()),
            status: AgentStatus::Completed(Some("done".to_string())),
            readiness: AgentReadiness::Terminal,
        });

        let WaitOutcome::TargetTerminal(result) = wait.await else {
            panic!("terminal target status must win over a concurrent explicit action");
        };
        assert_eq!(result.outcomes[0].0, target);
    }

    #[tokio::test]
    async fn explicit_action_already_pending_at_wait_start_uses_target_snapshot() {
        let input_queue = InputQueue::new();
        let target = ThreadId::new();
        let recipient = ThreadId::new();
        let recipient_path = codex_protocol::AgentPath::root();
        let update_id = ResponseItemId::new("result-ready-before-wait");
        let sender_path = AgentPath::try_from("/root/worker").expect("worker path");
        input_queue
            .enqueue_explicit_mailbox_communication(
                explicit_wait_mail(update_id.clone(), sender_path.clone(), "private body"),
                Default::default(),
                explicit_wait_activity(
                    update_id.clone(),
                    InterAgentCommunicationIntent::ResultReady,
                    target,
                    recipient,
                    sender_path.clone(),
                ),
            )
            .await;

        let _explicit_activity_rx = input_queue.subscribe_explicit_activity();
        let pending = input_queue
            .pending_explicit_activity_for_recipient(recipient, Some(&recipient_path))
            .await;
        assert_eq!(pending.map(|activity| activity.update_id), Some(update_id));
    }

    #[tokio::test]
    async fn queue_only_mail_does_not_hide_target_terminal() {
        let registry = Arc::new(Mutex::new(AgentWaitRegistry::default()));
        let target = ThreadId::new();
        let publisher = AgentOutcomePublisher::new(target, registry.clone());
        let mut agent_wait = Some(register_agent_wait(
            &registry,
            vec![target],
            AgentWaitReturnWhen::Any,
        ));
        let input_queue = InputQueue::new();
        let (mut activity_rx, pending) = input_queue.subscribe_activity(/*turn_state*/ None).await;
        let mut wait = Box::pin(wait_for_activity(
            &mut activity_rx,
            pending,
            Instant::now() + Duration::from_secs(60),
            &mut agent_wait,
        ));
        assert!(futures::poll!(wait.as_mut()).is_pending());
        input_queue
            .enqueue_mailbox_communication(
                wait_mail("quiet update", /*trigger_turn*/ false),
                Default::default(),
            )
            .await;
        assert!(futures::poll!(wait.as_mut()).is_pending());
        publisher.publish(AgentOutcomeSnapshot {
            turn_id: Some("turn-1".to_string()),
            status: AgentStatus::Completed(Some("done".to_string())),
            readiness: AgentReadiness::Terminal,
        });
        let WaitOutcome::TargetTerminal(result) = wait.await else {
            panic!("target completion must still wake a wait with queued quiet mail");
        };
        assert_eq!(result.outcomes[0].0, target);
        assert_eq!(input_queue.drain_mailbox().await.len(), 1);
    }

    #[tokio::test]
    async fn cancelling_wait_retains_queue_only_mail_for_later_followup() {
        let input_queue = InputQueue::new();
        let (mut activity_rx, pending) = input_queue.subscribe_activity(/*turn_state*/ None).await;
        let mut agent_wait = None;
        let mut wait = Box::pin(wait_for_activity(
            &mut activity_rx,
            pending,
            Instant::now() + Duration::from_secs(60),
            &mut agent_wait,
        ));
        assert!(futures::poll!(wait.as_mut()).is_pending());
        input_queue
            .enqueue_mailbox_communication(
                wait_mail("quiet update", /*trigger_turn*/ false),
                Default::default(),
            )
            .await;
        assert!(futures::poll!(wait.as_mut()).is_pending());
        // Tool cancellation drops this future; it must not consume retained input.
        drop(wait);
        let (mut activity_rx, pending) = input_queue.subscribe_activity(/*turn_state*/ None).await;
        assert_eq!(pending, None);
        input_queue
            .enqueue_mailbox_communication(
                wait_mail("follow up", /*trigger_turn*/ true),
                Default::default(),
            )
            .await;
        assert!(matches!(
            wait_for_activity(
                &mut activity_rx,
                pending,
                Instant::now() + Duration::from_secs(60),
                &mut agent_wait,
            )
            .await,
            WaitOutcome::MailboxActivity
        ));
        assert_eq!(input_queue.drain_mailbox().await.len(), 2);
        assert!(input_queue.drain_mailbox().await.is_empty());
    }

    #[tokio::test]
    async fn mailbox_wakeup_rechecks_concurrently_latched_target_outcome() {
        let registry = Arc::new(Mutex::new(AgentWaitRegistry::default()));
        let target = ThreadId::new();
        let publisher = AgentOutcomePublisher::new(target, registry.clone());
        publisher.publish(AgentOutcomeSnapshot {
            turn_id: Some("turn-1".to_string()),
            status: AgentStatus::Running,
            readiness: AgentReadiness::Pending,
        });
        let registration = register_agent_wait(&registry, vec![target], AgentWaitReturnWhen::Any);
        let mut agent_wait = Some(registration);
        let input_queue = InputQueue::new();
        let watermark = input_queue.mailbox_enqueue_watermark().await;
        let (mut activity_rx, pending) = input_queue.subscribe_activity(/*turn_state*/ None).await;
        assert_eq!(pending, None);
        input_queue
            .enqueue_mailbox_communication(
                codex_protocol::protocol::InterAgentCommunication::new(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    Vec::new(),
                    "quiet update".to_string(),
                    /*trigger_turn*/ false,
                ),
                Default::default(),
            )
            .await;
        input_queue
            .enqueue_mailbox_communication(
                wait_mail("follow up", /*trigger_turn*/ true),
                Default::default(),
            )
            .await;
        activity_rx.changed().await.expect("mailbox activity");
        publisher.publish(AgentOutcomeSnapshot {
            turn_id: Some("turn-1".to_string()),
            status: AgentStatus::Completed(Some("done".to_string())),
            readiness: AgentReadiness::Terminal,
        });

        let outcome = activity_wake_outcome(*activity_rx.borrow_and_update(), &mut agent_wait);
        let WaitOutcome::TargetTerminal(result) = outcome else {
            panic!("latched target outcome must win over concurrently ready mailbox activity");
        };
        assert_eq!(result.outcomes.len(), 1);
        assert_eq!(result.outcomes[0].0, target);
        assert_eq!(result.outcomes[0].1.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(
            result.outcomes[0].1.status,
            AgentStatus::Completed(Some("done".to_string()))
        );
        let queued_update_count = input_queue
            .pending_mailbox_communication_count_since(watermark)
            .await;
        let item = completed_wait_item(
            "wait-call".to_string(),
            ThreadId::new(),
            vec![target],
            Vec::new(),
            &WaitOutcome::TargetTerminal(result.clone()),
            queued_update_count,
            agent_wait_states(Some(&result)),
        );
        assert_eq!(item.wait_outcome, Some(WaitAgentOutcome::TargetTerminalAny));
        assert_eq!(item.queued_update_count, Some(1));
        assert_eq!(
            item.agents_states.get(&target),
            Some(&AgentStatus::Completed(Some("done".to_string())))
        );
        assert_eq!(
            queued_update_count,
            Some(1),
            "count observes without consuming"
        );
        assert!(input_queue.has_pending_mailbox_items().await);
    }
}
