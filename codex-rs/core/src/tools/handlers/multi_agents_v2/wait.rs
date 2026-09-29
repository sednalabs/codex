use super::*;
use crate::agent::agent_resolver::resolve_agent_target;
use crate::agent::status::is_final;
use crate::session::InputQueueActivity;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentRef;
use codex_protocol::items::WaitAgentOutcome;
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

type StatusFuture = futures::future::BoxFuture<
    'static,
    (
        ThreadId,
        tokio::sync::watch::Receiver<AgentStatus>,
        Result<(), tokio::sync::watch::error::RecvError>,
    ),
>;
type StatusFutures = FuturesUnordered<StatusFuture>;

// Native waits stay inside this helper across provider/runtime lease renewal.
// Renewal is deliberately invisible to the model: it must never turn into a
// new tool result or provider turn. This is an internal safety lease, not the
// operator-requested wait timeout.
const NATIVE_WAIT_LEASE: Duration = Duration::from_secs(60 * 60);

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
        if args.native_event_wait {
            let current_agent_path = turn.session_source.get_agent_path().or_else(|| {
                session
                    .services
                    .agent_control
                    .get_agent_metadata(session.thread_id)
                    .and_then(|metadata| metadata.agent_path)
            });
            for target_id in &target_ids {
                let target_agent_path = session
                    .services
                    .agent_control
                    .get_agent_metadata(*target_id)
                    .and_then(|metadata| metadata.agent_path);
                if let Some(message) =
                    reverse_wait_error(current_agent_path.as_ref(), target_agent_path.as_ref())
                {
                    return Err(FunctionCallError::RespondToModel(message));
                }
            }
        }

        // Capture the mailbox boundary and receiver under the queue's native
        // activity boundary. This makes a native wait insensitive to entries
        // already queued before it began while retaining a single
        // event-driven subscription with no enqueue gap.
        let (mut activity_rx, mut pending_activity, mailbox_generation, pending_mailbox) =
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
        let mut status_futures: StatusFutures = FuturesUnordered::new();
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
                    receiver_agents: receiver_agent_refs(&session, &target_ids),
                    wait_outcome: None,
                    queued_update_count: None,
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: statuses.clone(),
                }),
            )
            .await;

        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        let (reason, timed_out) = wait_for_event(WaitEventContext {
            session: &session,
            activity_rx: &mut activity_rx,
            pending_activity,
            mailbox_generation,
            target_ids: &target_ids,
            target_paths: &target_paths,
            return_when: args.return_when,
            statuses: &mut statuses,
            status_futures: &mut status_futures,
            deadline,
            native_event_wait: args.native_event_wait,
            pending_mailbox: &pending_mailbox,
        })
        .await;

        let queued_update_count = if args.native_event_wait {
            Some(queued_non_waking_mailbox_count(
                &session.input_queue.pending_mailbox_authors().await,
                mailbox_generation,
            ))
        } else {
            None
        };

        let mut message = reason.message();
        if let Some(requested) = args.timeout_ms.filter(|requested| *requested < timeout_ms) {
            message = format!(
                "{message}\n\nRequested timeout of {requested}ms was clamped to the minimum of {timeout_ms}ms."
            );
        }
        message = format!(
            "{message} Wait outcome: {}.",
            wait_outcome_label(reason.outcome()),
        );
        if let Some(count) = queued_update_count {
            message = format!(
                "{message} Quiet queued updates: {count} (did not wake this wait)."
            );
        }
        let outcome = reason.outcome();
        let completed_receiver_agents = receiver_agent_refs(&session, &target_ids);
        let result = WaitAgentResult {
            message,
            timed_out,
            outcome: Some(outcome),
            queued_update_count,
        };
        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::Completed,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: target_ids,
                    receiver_agents: completed_receiver_agents,
                    wait_outcome: Some(outcome),
                    queued_update_count,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<WaitAgentOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) queued_update_count: Option<u32>,
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
    TargetTerminal(ReturnWhen),
    ExactTargetActionableMessage,
    TargetlessActionableMessage,
    UnattributedMailboxActivity,
    AmbiguousMailboxActivity,
    TerminalCompletion,
    Steer,
    Timeout,
    SubscriptionLoss,
}

impl WaitReason {
    fn message(self) -> String {
        match self {
            Self::TargetTerminal(_) | Self::ExactTargetActionableMessage
            | Self::TargetlessActionableMessage | Self::UnattributedMailboxActivity
            | Self::AmbiguousMailboxActivity | Self::TerminalCompletion => {
                "Wait completed.".into()
            }
            Self::Steer => "Wait interrupted by new input.".into(),
            Self::Timeout => "Wait timed out.".into(),
            Self::SubscriptionLoss => "Wait ended because an event subscription was lost.".into(),
        }
    }
    fn outcome(self) -> WaitAgentOutcome {
        match self {
            Self::TargetTerminal(ReturnWhen::Any) => WaitAgentOutcome::TargetTerminalAny,
            Self::TargetTerminal(ReturnWhen::All) => WaitAgentOutcome::TargetTerminalAll,
            Self::ExactTargetActionableMessage => {
                WaitAgentOutcome::ExactTargetActionableMessage
            }
            Self::TargetlessActionableMessage => WaitAgentOutcome::TargetlessActionableMessage,
            Self::UnattributedMailboxActivity => WaitAgentOutcome::UnattributedMailboxActivity,
            Self::AmbiguousMailboxActivity => WaitAgentOutcome::AmbiguousMailboxActivity,
            Self::TerminalCompletion => WaitAgentOutcome::TerminalCompletion,
            Self::Steer => WaitAgentOutcome::OperatorSteer,
            Self::Timeout => WaitAgentOutcome::Timeout,
            Self::SubscriptionLoss => WaitAgentOutcome::SubscriptionLoss,
        }
    }
}

fn wait_outcome_label(outcome: WaitAgentOutcome) -> &'static str {
    match outcome {
        WaitAgentOutcome::TargetTerminalAny => "target_terminal_any",
        WaitAgentOutcome::TargetTerminalAll => "target_terminal_all",
        WaitAgentOutcome::ExactTargetActionableMessage => "exact_target_actionable_message",
        WaitAgentOutcome::TargetlessActionableMessage => "targetless_actionable_message",
        WaitAgentOutcome::UnattributedMailboxActivity => "unattributed_mailbox_activity",
        WaitAgentOutcome::AmbiguousMailboxActivity => "ambiguous_mailbox_activity",
        WaitAgentOutcome::TerminalCompletion => "terminal_completion",
        WaitAgentOutcome::OperatorSteer => "operator_steer",
        WaitAgentOutcome::Timeout => "timeout",
        WaitAgentOutcome::SubscriptionLoss => "subscription_loss",
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

struct WaitEventContext<'a> {
    session: &'a crate::session::session::Session,
    activity_rx: &'a mut tokio::sync::watch::Receiver<InputQueueActivity>,
    pending_activity: Option<InputQueueActivity>,
    mailbox_generation: u64,
    target_ids: &'a [ThreadId],
    target_paths: &'a [codex_protocol::AgentPath],
    return_when: ReturnWhen,
    statuses: &'a mut HashMap<ThreadId, AgentStatus>,
    status_futures: &'a mut StatusFutures,
    deadline: Instant,
    native_event_wait: bool,
    pending_mailbox: &'a [(codex_protocol::AgentPath, u64, bool)],
}

async fn wait_for_event(context: WaitEventContext<'_>) -> (WaitReason, bool) {
    let WaitEventContext {
        session,
        activity_rx,
        pending_activity,
        mailbox_generation,
        target_ids,
        target_paths,
        return_when,
        statuses,
        status_futures,
        mut deadline,
        native_event_wait,
        pending_mailbox,
    } = context;
    if terminal_rule_satisfied(target_ids, return_when, statuses) {
        return (WaitReason::TargetTerminal(return_when), false);
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
    if native_event_wait
        && let Some(reason) = mailbox_wake_reason(
            target_ids,
            target_paths,
            pending_mailbox,
            mailbox_generation,
        )
    {
        return (reason, false);
    }
    if !native_event_wait {
        if matches!(pending_activity, Some(InputQueueActivity::Steer)) {
            return (WaitReason::Steer, false);
        }
        match pending_activity {
            Some(InputQueueActivity::Mailbox) => {
                return (WaitReason::UnattributedMailboxActivity, false);
            }
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
                        return (WaitReason::TargetTerminal(return_when), false);
                    }
                }
                if matches!(activity, InputQueueActivity::Mailbox | InputQueueActivity::TerminalCompletion)
                    && !native_event_wait
                {
                    if terminal_rule_satisfied(target_ids, return_when, statuses) {
                        return (WaitReason::TargetTerminal(return_when), false);
                    }
                    return (
                        if activity == InputQueueActivity::TerminalCompletion {
                            WaitReason::TerminalCompletion
                        } else {
                            WaitReason::UnattributedMailboxActivity
                        },
                        false,
                    );
                }
                if native_event_wait && matches!(activity, InputQueueActivity::Mailbox) {
                    let pending_mailbox = session.input_queue.pending_mailbox_authors().await;
                    if let Some(reason) = mailbox_wake_reason(
                        target_ids,
                        target_paths,
                        &pending_mailbox,
                        mailbox_generation,
                    ) {
                        return (reason, false);
                    }
                }
            }
            status = status_futures.next(), if !status_futures.is_empty() => {
                let Some((id, mut rx, changed)) = status else { continue; };
                if changed.is_err() { return (WaitReason::SubscriptionLoss, false); }
                let value = rx.borrow().clone();
                if is_final(&value) { statuses.insert(id, value); }
                if terminal_rule_satisfied(target_ids, return_when, statuses) { return (WaitReason::TargetTerminal(return_when), false); }
                status_futures.push(async move { let changed = rx.changed().await; (id, rx, changed) }.boxed());
            }
            _ = tokio::time::sleep_until(deadline) => {
                if native_event_wait {
                    deadline = Instant::now() + NATIVE_WAIT_LEASE;
                    continue;
                }
                return (WaitReason::Timeout, true);
            }
        }
    }
}

fn mailbox_wake_reason(
    target_ids: &[ThreadId],
    target_paths: &[codex_protocol::AgentPath],
    pending_mailbox: &[(codex_protocol::AgentPath, u64, bool)],
    mailbox_generation: u64,
) -> Option<WaitReason> {
    let newly_actionable = pending_mailbox
        .iter()
        .filter(|(_, sequence, trigger_turn)| {
            *sequence > mailbox_generation && *trigger_turn
        })
        .collect::<Vec<_>>();
    if newly_actionable.is_empty() {
        return None;
    }

    if target_ids.is_empty() {
        return Some(if newly_actionable.len() == 1 {
            WaitReason::TargetlessActionableMessage
        } else {
            WaitReason::AmbiguousMailboxActivity
        });
    }

    let eligible = newly_actionable
        .iter()
        .filter(|(author, _, _)| target_paths.contains(author))
        .count();
    if eligible == 0 {
        return None;
    }
    let outside_targets = newly_actionable.len().saturating_sub(eligible);
    Some(if eligible == 1 && outside_targets == 0 {
        WaitReason::ExactTargetActionableMessage
    } else {
        WaitReason::AmbiguousMailboxActivity
    })
}

fn mailbox_wake_matches(
    target_ids: &[ThreadId],
    target_paths: &[codex_protocol::AgentPath],
    pending_mailbox: &[(codex_protocol::AgentPath, u64, bool)],
    mailbox_generation: u64,
) -> bool {
    mailbox_wake_reason(target_ids, target_paths, pending_mailbox, mailbox_generation).is_some()
}

fn queued_non_waking_mailbox_count(
    pending_mailbox: &[(codex_protocol::AgentPath, u64, bool)],
    mailbox_generation: u64,
) -> u32 {
    pending_mailbox
        .iter()
        .filter(|(_, sequence, trigger_turn)| {
            *sequence > mailbox_generation && !*trigger_turn
        })
        .count()
        .min(u32::MAX as usize) as u32
}

fn receiver_agent_refs(
    session: &crate::session::session::Session,
    receiver_thread_ids: &[ThreadId],
) -> Vec<CollabAgentRef> {
    receiver_thread_ids
        .iter()
        .map(|thread_id| {
            let metadata = session.services.agent_control.get_agent_metadata(*thread_id);
            CollabAgentRef {
                thread_id: *thread_id,
                agent_nickname: metadata
                    .as_ref()
                    .and_then(|metadata| metadata.agent_nickname.clone()),
                agent_role: metadata.and_then(|metadata| metadata.agent_role),
            }
        })
        .collect()
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

/// A native wait on the current agent or one of its ancestors creates a reverse
/// dependency: the ancestor normally waits for this child to return, so both
/// sides can remain in native waits forever. Bounded non-native status waits
/// retain their existing compatibility, while descendants and unrelated peers
/// remain valid native wait targets.
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
    use crate::session::tests::make_session_and_context;
    use codex_protocol::AgentPath;
    use codex_protocol::protocol::InterAgentCommunication;

    fn path(value: &str) -> codex_protocol::AgentPath {
        AgentPath::try_from(value).expect("agent path")
    }

    #[test]
    fn native_wait_ignores_queue_only_mailbox_progress() {
        let worker = path("/root/worker");
        let target_paths = vec![worker.clone()];
        let queued_only = vec![(worker, 1, false)];
        assert!(!mailbox_wake_matches(
            &[ThreadId::new()],
            &target_paths,
            &queued_only,
            /*mailbox_generation*/ 0,
        ));
    }

    #[test]
    fn native_wait_accepts_actionable_target_mailbox_progress() {
        let worker = path("/root/worker");
        let target_paths = vec![worker.clone()];
        let actionable = vec![(worker, 1, true)];
        assert!(mailbox_wake_matches(
            &[ThreadId::new()],
            &target_paths,
            &actionable,
            /*mailbox_generation*/ 0,
        ));
    }

    #[test]
    fn targetless_native_wait_accepts_any_actionable_mailbox_progress() {
        let worker = path("/root/worker");
        let actionable = vec![(worker, 1, true)];
        assert!(mailbox_wake_matches(
            &[],
            &[],
            &actionable,
            /*mailbox_generation*/ 0,
        ));
    }

    #[test]
    fn native_wait_does_not_replay_the_snapshot_boundary() {
        let worker = path("/root/worker");
        let snapshot = vec![(worker, 1, true)];
        assert!(!mailbox_wake_matches(
            &[],
            &[],
            &snapshot,
            /*mailbox_generation*/ 1,
        ));
    }

    #[test]
    fn completion_rule_distinguishes_any_from_all() {
        let first = ThreadId::new();
        let second = ThreadId::new();
        let statuses = HashMap::from([(first, AgentStatus::Shutdown)]);

        assert!(terminal_rule_satisfied(
            &[first, second],
            ReturnWhen::Any,
            &statuses,
        ));
        assert!(!terminal_rule_satisfied(
            &[first, second],
            ReturnWhen::All,
            &statuses,
        ));

        let statuses = HashMap::from([
            (first, AgentStatus::Shutdown),
            (second, AgentStatus::Shutdown),
        ]);
        assert!(terminal_rule_satisfied(
            &[first, second],
            ReturnWhen::All,
            &statuses,
        ));
    }

    #[test]
    fn reverse_wait_rejects_parent_and_self_but_allows_noncyclic_targets() {
        let parent = path("/root/staff_r2_signing");
        let reviewer = path("/root/staff_r2_signing/staff_custody_review");
        let child = path("/root/staff_r2_signing/staff_custody_review/worker");
        let sibling = path("/root/staff_r2_signing/other_review");

        let parent_error = reverse_wait_error(Some(&reviewer), Some(&parent))
            .expect("a reviewer must not wait on its parent");
        assert!(parent_error.contains("current agent or an ancestor"));
        assert!(reverse_wait_error(Some(&reviewer), Some(&reviewer)).is_some());
        assert!(reverse_wait_error(Some(&reviewer), Some(&child)).is_none());
        assert!(reverse_wait_error(Some(&reviewer), Some(&sibling)).is_none());
    }

    #[test]
    fn reverse_wait_guard_is_conservative_when_paths_are_unknown() {
        let reviewer = path("/root/staff_r2_signing/staff_custody_review");
        let parent = path("/root/staff_r2_signing");

        assert!(reverse_wait_error(/*current_agent_path*/ None, Some(&parent)).is_none());
        assert!(reverse_wait_error(Some(&reviewer), /*target_agent_path*/ None).is_none());
    }

    #[tokio::test]
    async fn native_wait_all_stays_pending_until_every_target_is_terminal() {
        let (session, _) = make_session_and_context().await;
        let (mut activity_rx, pending_activity, mailbox_generation, pending_mailbox) =
            session.input_queue.subscribe_native_activity().await;
        let first = ThreadId::new();
        let second = ThreadId::new();
        let (first_tx, mut first_rx) = tokio::sync::watch::channel(AgentStatus::Running);
        let (second_tx, mut second_rx) = tokio::sync::watch::channel(AgentStatus::Running);
        let mut statuses = HashMap::new();
        let mut status_futures: StatusFutures = FuturesUnordered::new();
        status_futures.push(
            async move {
                let changed = first_rx.changed().await;
                (first, first_rx, changed)
            }
            .boxed(),
        );
        status_futures.push(
            async move {
                let changed = second_rx.changed().await;
                (second, second_rx, changed)
            }
            .boxed(),
        );
        let target_ids = [first, second];
        let target_paths = Vec::new();
        let wait = wait_for_event(WaitEventContext {
            session: &session,
            activity_rx: &mut activity_rx,
            pending_activity,
            mailbox_generation,
            target_ids: &target_ids,
            target_paths: &target_paths,
            return_when: ReturnWhen::All,
            statuses: &mut statuses,
            status_futures: &mut status_futures,
            deadline: Instant::now() + NATIVE_WAIT_LEASE,
            native_event_wait: true,
            pending_mailbox: &pending_mailbox,
        });
        tokio::pin!(wait);

        first_tx
            .send(AgentStatus::Shutdown)
            .expect("first status receiver");
        tokio::select! {
            biased;
            result = &mut wait => panic!("all returned after one target: {result:?}"),
            () = tokio::task::yield_now() => {}
        }

        second_tx
            .send(AgentStatus::Shutdown)
            .expect("second status receiver");
        assert_eq!(wait.await, (WaitReason::TargetTerminal(ReturnWhen::All), false));
    }

    #[tokio::test]
    async fn native_lease_expiry_and_queue_only_mail_stay_inside_wait() {
        let (session, _) = make_session_and_context().await;
        let (mut activity_rx, pending_activity, mailbox_generation, pending_mailbox) =
            session.input_queue.subscribe_native_activity().await;
        let target_ids = Vec::new();
        let target_paths = Vec::new();
        let mut statuses = HashMap::new();
        let mut status_futures: StatusFutures = FuturesUnordered::new();

        tokio::time::pause();
        let wait = wait_for_event(WaitEventContext {
            session: &session,
            activity_rx: &mut activity_rx,
            pending_activity,
            mailbox_generation,
            target_ids: &target_ids,
            target_paths: &target_paths,
            return_when: ReturnWhen::Any,
            statuses: &mut statuses,
            status_futures: &mut status_futures,
            deadline: Instant::now() + Duration::from_millis(5),
            native_event_wait: true,
            pending_mailbox: &pending_mailbox,
        });
        tokio::pin!(wait);

        session
            .input_queue
            .enqueue_mailbox_communication(
                InterAgentCommunication::new(
                    path("/root/worker"),
                    AgentPath::root(),
                    Vec::new(),
                    "queued progress".to_string(),
                    /*trigger_turn*/ false,
                ),
                Default::default(),
            )
            .await;
        tokio::select! {
            biased;
            result = &mut wait => panic!("queue-only mail completed native wait: {result:?}"),
            () = tokio::task::yield_now() => {}
        }

        tokio::time::advance(Duration::from_millis(5)).await;
        tokio::select! {
            biased;
            result = &mut wait => panic!("initial native lease escaped the tool: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        tokio::time::advance(NATIVE_WAIT_LEASE).await;
        tokio::select! {
            biased;
            result = &mut wait => panic!("renewed native lease escaped the tool: {result:?}"),
            () = tokio::task::yield_now() => {}
        }

        session
            .input_queue
            .enqueue_mailbox_communication(
                InterAgentCommunication::new(
                    path("/root/worker"),
                    AgentPath::root(),
                    Vec::new(),
                    "action required".to_string(),
                    /*trigger_turn*/ true,
                ),
                Default::default(),
            )
            .await;
        assert_eq!(
            wait.await,
            (WaitReason::TargetlessActionableMessage, false)
        );
        tokio::time::resume();
    }

    #[test]
    fn mailbox_outcome_requires_unique_causal_target() {
        let worker = path("/root/worker");
        let other = path("/root/other");
        let target = ThreadId::new();
        let targets = [target];
        let target_paths = [worker.clone()];

        let one_target = vec![(worker.clone(), 11, true)];
        assert_eq!(
            mailbox_wake_reason(&targets, &target_paths, &one_target, 10),
            Some(WaitReason::ExactTargetActionableMessage)
        );

        let competing_targets = vec![(worker.clone(), 11, true), (worker.clone(), 12, true)];
        assert_eq!(
            mailbox_wake_reason(&targets, &target_paths, &competing_targets, 10),
            Some(WaitReason::AmbiguousMailboxActivity)
        );

        let target_and_outside = vec![(worker.clone(), 11, true), (other, 12, true)];
        assert_eq!(
            mailbox_wake_reason(&targets, &target_paths, &target_and_outside, 10),
            Some(WaitReason::AmbiguousMailboxActivity)
        );

        let queued_only = vec![(worker, 11, false)];
        assert_eq!(
            mailbox_wake_reason(&targets, &target_paths, &queued_only, 10),
            None
        );
        assert_eq!(queued_non_waking_mailbox_count(&queued_only, 10), 1);
        assert_eq!(queued_non_waking_mailbox_count(&queued_only, 11), 0);
    }

    #[test]
    fn targetless_mailbox_outcome_marks_competing_messages_ambiguous() {
        let one_message = vec![(path("/root/worker"), 11, true)];
        assert_eq!(
            mailbox_wake_reason(&[], &[], &one_message, 10),
            Some(WaitReason::TargetlessActionableMessage)
        );

        let two_messages = vec![
            (path("/root/worker"), 11, true),
            (path("/root/other"), 12, true),
        ];
        assert_eq!(
            mailbox_wake_reason(&[], &[], &two_messages, 10),
            Some(WaitReason::AmbiguousMailboxActivity)
        );
    }
}
