use super::*;
use crate::agent::agent_resolver::resolve_agent_target;
use crate::agent::status::is_final;
use crate::session::InputQueueActivity;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_tools::ToolSpec;
use futures::FutureExt;
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
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
        let targetless_native_allowed = turn
            .session_source
            .get_agent_path()
            .is_none_or(|path| path.is_root());
        if args.native_event_wait && args.targets.is_empty() && !targetless_native_allowed {
            return Err(FunctionCallError::RespondToModel(
                "native_event_wait requires at least one exact target".to_string(),
            ));
        }
        let timeout_ms = resolve_timeout(
            args.timeout_ms,
            turn.config.multi_agent_v2.min_wait_timeout_ms,
            turn.config.multi_agent_v2.max_wait_timeout_ms,
            turn.config.multi_agent_v2.default_wait_timeout_ms,
        )?;

        let mut target_ids = Vec::with_capacity(args.targets.len());
        for target in &args.targets {
            target_ids.push(resolve_agent_target(&session, &turn, target).await?);
        }
        let mut unique_targets = HashSet::with_capacity(target_ids.len());
        if target_ids.iter().any(|id| !unique_targets.insert(*id)) {
            return Err(FunctionCallError::RespondToModel(
                "targets must resolve to unique agents".to_string(),
            ));
        }

        // Capture the mailbox boundary before subscribing. This makes a
        // native wait insensitive to entries that were already queued before
        // the wait began while retaining a single event-driven subscription.
        let (mut activity_rx, mut pending_activity, mailbox_generation, _pending_mailbox) =
            if args.native_event_wait {
                session.input_queue.subscribe_native_activity().await
            } else {
                let turn_state = session
                    .input_queue
                    .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
                    .await;
                let (rx, pending) = session
                    .input_queue
                    .subscribe_activity(turn_state.as_deref())
                    .await;
                (
                    rx,
                    pending,
                    session.input_queue.mailbox_generation(),
                    Vec::new(),
                )
            };
        if args.native_event_wait
            && pending_activity.is_none()
            && session
                .input_queue
                .has_pending_input(&session.active_turn)
                .await
        {
            pending_activity = Some(InputQueueActivity::Steer);
        }
        let target_paths = target_ids
            .iter()
            .filter_map(|id| {
                session
                    .services
                    .agent_control
                    .get_agent_metadata(*id)
                    .and_then(|metadata| metadata.agent_path)
            })
            .collect::<Vec<_>>();
        let mut statuses = HashMap::new();
        let mut status_futures = FuturesUnordered::new();
        for id in &target_ids {
            let mut status_rx = match session.services.agent_control.subscribe_status(*id).await {
                Ok(rx) => rx,
                Err(err) => {
                    if err.to_string().to_ascii_lowercase().contains("not found") {
                        statuses.insert(*id, AgentStatus::NotFound);
                        continue;
                    }
                    return Err(FunctionCallError::RespondToModel(err.to_string()));
                }
            };
            let status = status_rx.borrow().clone();
            if is_final(&status) {
                statuses.insert(*id, status);
            }
            let id = *id;
            status_futures.push(
                async move {
                    let changed = status_rx.changed().await;
                    (id, status_rx, changed)
                }
                .boxed(),
            );
        }

        session
            .emit_turn_item_started(
                &turn,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: target_ids.clone(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: statuses.clone(),
                }),
            )
            .await;

        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        let (reason, timed_out) = wait_for_event(
            &session,
            &mut activity_rx,
            pending_activity,
            mailbox_generation,
            &target_ids,
            &target_paths,
            args.return_when,
            &mut statuses,
            &mut status_futures,
            deadline,
            args.native_event_wait,
        )
        .await;

        let mut message = reason.message();
        if let Some(requested) = args.timeout_ms.filter(|requested| *requested < timeout_ms) {
            message = format!(
                "{message}\n\nRequested timeout of {requested}ms was clamped to the minimum of {timeout_ms}ms."
            );
        }
        if args.native_event_wait {
            message = format!(
                "{message} Wake cause: {}; origin: {}; disposition: {}.",
                reason.wake_cause(),
                reason.notification_origin(),
                reason.delivery_disposition()
            );
        }
        let result = WaitAgentResult { message, timed_out };
        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::Completed,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: target_ids,
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: statuses,
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
    #[serde(default)]
    targets: Vec<String>,
    timeout_ms: Option<i64>,
    #[serde(default)]
    return_when: ReturnWhen,
    #[serde(default)]
    native_event_wait: bool,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitReason {
    TargetTerminal,
    Mailbox,
    TerminalCompletion,
    Steer,
    Timeout,
    SubscriptionLoss,
}

impl WaitReason {
    fn message(self) -> String {
        match self {
            Self::TargetTerminal | Self::Mailbox | Self::TerminalCompletion => {
                "Wait completed.".into()
            }
            Self::Steer => "Wait interrupted by new input.".into(),
            Self::Timeout => "Wait timed out.".into(),
            Self::SubscriptionLoss => "Wait ended because an event subscription was lost.".into(),
        }
    }
    fn wake_cause(self) -> &'static str {
        match self {
            Self::TargetTerminal => "target_terminal",
            Self::Mailbox => "target_actionable_message",
            Self::TerminalCompletion => "terminal_completion",
            Self::Steer => "operator_message",
            Self::Timeout => "timeout_lease_expiry",
            Self::SubscriptionLoss => "runtime_system_event",
        }
    }
    fn notification_origin(self) -> &'static str {
        match self {
            Self::Mailbox => "agent_mailbox",
            Self::TerminalCompletion => "unified_exec",
            Self::Steer => "operator",
            Self::TargetTerminal => "agent_status",
            Self::Timeout => "runtime",
            Self::SubscriptionLoss => "runtime",
        }
    }
    fn delivery_disposition(self) -> &'static str {
        match self {
            Self::Mailbox => "queued",
            Self::TerminalCompletion => "terminal",
            Self::Steer => "turn_triggered",
            Self::TargetTerminal => "terminal",
            Self::Timeout => "lease_expired",
            Self::SubscriptionLoss => "lost",
        }
    }
}

fn resolve_timeout(
    requested: Option<i64>,
    min: i64,
    max: i64,
    default: i64,
) -> Result<i64, FunctionCallError> {
    let min = min.max(0);
    let max = max.max(min);
    match requested {
        Some(value) if value < min => Ok(min),
        Some(value) if value > max => Err(FunctionCallError::RespondToModel(format!(
            "timeout_ms must be at most {max}"
        ))),
        Some(value) => Ok(value),
        None => Ok(default.clamp(min, max)),
    }
}

async fn wait_for_event(
    session: &crate::session::session::Session,
    activity_rx: &mut tokio::sync::watch::Receiver<InputQueueActivity>,
    pending_activity: Option<InputQueueActivity>,
    mailbox_generation: u64,
    target_ids: &[ThreadId],
    target_paths: &[codex_protocol::AgentPath],
    return_when: ReturnWhen,
    statuses: &mut HashMap<ThreadId, AgentStatus>,
    status_futures: &mut FuturesUnordered<
        futures::future::BoxFuture<
            'static,
            (
                ThreadId,
                tokio::sync::watch::Receiver<AgentStatus>,
                Result<(), tokio::sync::watch::error::RecvError>,
            ),
        >,
    >,
    mut deadline: Instant,
    native_event_wait: bool,
) -> (WaitReason, bool) {
    if terminal_rule_satisfied(target_ids, return_when, statuses) {
        return (WaitReason::TargetTerminal, false);
    }
    if matches!(
        pending_activity,
        Some(InputQueueActivity::TerminalCompletion)
    ) {
        return (WaitReason::TerminalCompletion, false);
    }
    if matches!(pending_activity, Some(InputQueueActivity::Steer)) {
        return (WaitReason::Steer, false);
    }
    if !native_event_wait {
        if matches!(pending_activity, Some(InputQueueActivity::Steer)) {
            return (WaitReason::Steer, false);
        }
        match pending_activity {
            Some(InputQueueActivity::Mailbox) => return (WaitReason::Mailbox, false),
            Some(InputQueueActivity::TerminalCompletion) => {
                return (WaitReason::TerminalCompletion, false);
            }
            _ => {}
        }
    }
    loop {
        tokio::select! {
            changed = activity_rx.changed() => {
                if changed.is_err() { return (WaitReason::SubscriptionLoss, false); }
                let activity = *activity_rx.borrow_and_update();
                if matches!(activity, InputQueueActivity::Steer) { return (WaitReason::Steer, false); }
                if matches!(activity, InputQueueActivity::TerminalCompletion) {
                    return (WaitReason::TerminalCompletion, false);
                }
                if matches!(activity, InputQueueActivity::Mailbox) {
                    for id in target_ids {
                        let status = session.services.agent_control.get_status(*id).await;
                        if is_final(&status) {
                            statuses.insert(*id, status);
                        }
                    }
                    if terminal_rule_satisfied(target_ids, return_when, statuses) {
                        return (WaitReason::TargetTerminal, false);
                    }
                }
                if matches!(activity, InputQueueActivity::Mailbox | InputQueueActivity::TerminalCompletion)
                    && !native_event_wait
                {
                    if terminal_rule_satisfied(target_ids, return_when, statuses) {
                        return (WaitReason::TargetTerminal, false);
                    }
                    return (if activity == InputQueueActivity::TerminalCompletion { WaitReason::TerminalCompletion } else { WaitReason::Mailbox }, false);
                }
                if native_event_wait
                    && matches!(activity, InputQueueActivity::Mailbox)
                    && session.input_queue.mailbox_generation() > mailbox_generation
                    && (target_ids.is_empty()
                        || session
                            .input_queue
                            .pending_mailbox_authors()
                            .await
                            .iter()
                            .any(|(author, sequence, trigger_turn)| {
                                *sequence > mailbox_generation
                                    && *trigger_turn
                                    && target_paths.contains(author)
                            }))
                {
                    return (WaitReason::Mailbox, false);
                }
            }
            status = status_futures.next(), if !status_futures.is_empty() => {
                let Some((id, mut rx, changed)) = status else { continue; };
                if changed.is_err() { return (WaitReason::SubscriptionLoss, false); }
                let value = rx.borrow().clone();
                if is_final(&value) { statuses.insert(id, value); }
                if terminal_rule_satisfied(target_ids, return_when, statuses) { return (WaitReason::TargetTerminal, false); }
                status_futures.push(async move { let changed = rx.changed().await; (id, rx, changed) }.boxed());
            }
            _ = tokio::time::sleep_until(deadline) => {
                if native_event_wait {
                    deadline = Instant::now() + Duration::from_secs(60);
                    continue;
                }
                return (WaitReason::Timeout, true);
            }
        }
    }
}

fn terminal_rule_satisfied(
    target_ids: &[ThreadId],
    return_when: ReturnWhen,
    statuses: &HashMap<ThreadId, AgentStatus>,
) -> bool {
    if target_ids.is_empty() {
        return false;
    }
    match return_when {
        ReturnWhen::Any => target_ids
            .iter()
            .any(|id| statuses.get(id).is_some_and(is_final)),
        ReturnWhen::All => target_ids
            .iter()
            .all(|id| statuses.get(id).is_some_and(is_final)),
    }
}
