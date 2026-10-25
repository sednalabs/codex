use super::*;
use crate::agent_communication::MessageDelivery;
use crate::agent_communication::PendingAgentMessage;
use crate::session::InputQueueActivity;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
use crate::tools::handlers::multi_agents_spec::create_wait_agent_tool_v2;
use codex_agent_mailbox::DispatchState;
use codex_agent_mailbox::Mailbox;
use codex_agent_mailbox::MessageKind;
use codex_agent_mailbox::ObservationTarget;
use codex_agent_mailbox::Outcome;
use codex_agent_mailbox::RequestDisposition;
use codex_agent_mailbox::ReturnWhen;
use codex_agent_mailbox::Selection;
use codex_agent_mailbox::Selector;
use codex_agent_mailbox::SnapshotItem;
use codex_protocol::items::MailboxWaitReport;
use codex_protocol::items::MailboxWaitReturnWhen;
use codex_protocol::items::MailboxWaitSelectionItem;
use codex_protocol::items::MailboxWakeCause;
use codex_tools::ToolSpec;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio::time::timeout_at;

#[derive(Default)]
pub(crate) struct Handler {
    options: WaitAgentTimeoutOptions,
    mailbox_enabled: bool,
}

impl Handler {
    pub(crate) fn new(options: WaitAgentTimeoutOptions, mailbox_enabled: bool) -> Self {
        Self {
            options,
            mailbox_enabled,
        }
    }
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("wait_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_wait_agent_tool_v2(self.options, self.mailbox_enabled)
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
            cancellation_token,
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

        let mailbox = session
            .services
            .agent_control
            .mailbox_v2_open(&turn.config)
            .await
            .map_err(|error| {
                FunctionCallError::RespondToModel(format!("mailbox operation failed: {error}"))
            })?;
        if let Some(mailbox) = mailbox {
            return handle_mailbox_wait(
                session,
                turn,
                call_id,
                args,
                requested_timeout_ms,
                timeout_ms,
                mailbox,
                cancellation_token,
            )
            .await;
        }
        if args.targets.is_some() || args.replies_to.is_some() {
            return Err(FunctionCallError::RespondToModel(
                "targeted durable joins require native V2 agent_mailbox to be enabled".to_string(),
            ));
        }

        let turn_state = session
            .input_queue
            .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
            .await;
        let (mut activity_rx, pending_activity) = session
            .input_queue
            .subscribe_activity(turn_state.as_deref())
            .await;

        session
            .emit_turn_item_started(
                &turn,
                &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id.clone(),
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::InProgress,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: Vec::new(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: Default::default(),
                    mailbox_wait_report: None,
                }),
            )
            .await;

        let wait_started = Instant::now();
        let deadline = wait_started + Duration::from_millis(timeout_ms as u64);
        let outcome = wait_for_activity(&mut activity_rx, pending_activity, deadline).await;
        // A completed wait may wake for a message, user input, or its timeout.
        // Dropped waits do not have an observed outcome and are not included.
        turn.session_telemetry.record_duration(
            "codex.multi_agent.wait.duration_ms",
            wait_started.elapsed(),
            &[(
                "outcome",
                match outcome {
                    WaitOutcome::MailboxActivity => "mailbox",
                    WaitOutcome::Steered => "steered",
                    WaitOutcome::TimedOut => "timed_out",
                },
            )],
        );
        let result = WaitAgentResult::from_outcome(outcome, requested_timeout_ms, timeout_ms);

        session
            .emit_turn_item_completed(
                &turn,
                TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
                    id: call_id,
                    tool: CollabAgentTool::Wait,
                    status: CollabAgentToolCallStatus::Completed,
                    sender_thread_id: session.thread_id,
                    receiver_thread_ids: Vec::new(),
                    receiver_agents: Vec::new(),
                    prompt: None,
                    model: None,
                    reasoning_effort: None,
                    agents_states: HashMap::new(),
                    mailbox_wait_report: None,
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

fn mailbox_error(error: impl std::fmt::Display) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!("mailbox operation failed: {error}"))
}

fn validate_wait_selectors(
    targets: &[String],
    replies_to: &[String],
) -> Result<(), FunctionCallError> {
    if targets.len() + replies_to.len() > 100 {
        return Err(mailbox_error(
            "at most 100 exact work/reply selectors are supported",
        ));
    }
    let mut total_bytes = 0_usize;
    for selector in targets.iter().chain(replies_to) {
        if selector.len() > 256 {
            return Err(mailbox_error(
                "each work/reply selector must be at most 256 UTF-8 bytes",
            ));
        }
        total_bytes = total_bytes.saturating_add(selector.len());
    }
    if total_bytes > 8_192 {
        return Err(mailbox_error(
            "combined work/reply selectors exceed 8192 UTF-8 bytes",
        ));
    }
    Ok(())
}

fn validate_wait_report(report: &MailboxWaitReport) -> Result<(), FunctionCallError> {
    if serde_json::to_vec(report).map_err(mailbox_error)?.len() > 16_384 {
        return Err(mailbox_error(
            "resolved wait report exceeds 16384 bytes; select fewer exact references",
        ));
    }
    Ok(())
}

fn selector_key(selector: &Selector) -> String {
    match selector {
        Selector::Work(id) => format!("work:{id}"),
        Selector::Reply(id) => format!("reply:{id}"),
    }
}

fn selector_key_for_snapshot(item: &SnapshotItem) -> String {
    match item {
        SnapshotItem::Work(work) => format!("work:{}", work.work_ref.id),
        SnapshotItem::Request(request) => format!("reply:{}", request.request_id),
    }
}

fn is_ready(item: &SnapshotItem) -> bool {
    match item {
        SnapshotItem::Work(work) => work.outcome.is_some() || work.observed,
        SnapshotItem::Request(request) => request.disposition != RequestDisposition::Open,
    }
}

fn already_observed(item: &SnapshotItem) -> bool {
    match item {
        SnapshotItem::Work(work) => work.observed,
        SnapshotItem::Request(request) => request.observed,
    }
}

fn needs_attention(item: &SnapshotItem) -> bool {
    match item {
        SnapshotItem::Work(work) => {
            work.dispatch_state == DispatchState::RecoveryNeeded
                || work
                    .outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.outcome != Outcome::Completed)
                || work
                    .continuations
                    .iter()
                    .any(|continuation| continuation.outcome == Outcome::NeedsAttention)
        }
        SnapshotItem::Request(request) => matches!(
            request.disposition,
            RequestDisposition::Superseded | RequestDisposition::Withdrawn
        ),
    }
}

fn readable_agent_name(session: &Session, agent: codex_protocol::ThreadId) -> String {
    session
        .services
        .agent_control
        .agent_metadata(agent)
        .and_then(|metadata| metadata.agent_path)
        .map(|path| path.to_string())
        .unwrap_or_else(|| agent.to_string())
}

fn update_report(session: &Session, report: &mut MailboxWaitReport, selected: &[SnapshotItem]) {
    report.completed.clear();
    report.pending.clear();
    for item in selected {
        let (summary, ready) = match item {
            SnapshotItem::Work(work) => {
                let status = if work.dispatch_state == DispatchState::RecoveryNeeded {
                    "recovery_needed".to_string()
                } else if work
                    .continuations
                    .iter()
                    .any(|continuation| continuation.outcome == Outcome::NeedsAttention)
                {
                    "needs_attention".to_string()
                } else {
                    work.outcome.as_ref().map_or_else(
                        || "pending".to_string(),
                        |outcome| {
                            match outcome.outcome {
                                Outcome::Completed => "completed",
                                Outcome::Failed => "failed",
                                Outcome::Cancelled => "cancelled",
                                Outcome::NeedsAttention => "needs_attention",
                            }
                            .to_string()
                        },
                    )
                };
                (
                    MailboxWaitSelectionItem {
                        work_ref: Some(work.work_ref.id.clone()),
                        message_id: work.outcome.as_ref().map(|outcome| outcome.id.clone()),
                        agent: Some(readable_agent_name(session, work.work_ref.agent)),
                        status,
                    },
                    is_ready(item),
                )
            }
            SnapshotItem::Request(request) => (
                MailboxWaitSelectionItem {
                    work_ref: None,
                    message_id: Some(request.request_id.clone()),
                    agent: Some(request.request.communication.recipient.to_string()),
                    status: match request.disposition {
                        RequestDisposition::Open => "pending".to_string(),
                        RequestDisposition::Replied => "replied".to_string(),
                        RequestDisposition::Superseded => format!(
                            "superseded:{}",
                            request.replacement.as_deref().unwrap_or("unknown")
                        ),
                        RequestDisposition::Withdrawn => "withdrawn".to_string(),
                    },
                },
                is_ready(item),
            ),
        };
        if ready {
            report.completed.push(summary);
        } else {
            report.pending.push(summary);
        }
    }
}

fn wait_report_item(
    session: &Session,
    call_id: &str,
    report: &MailboxWaitReport,
    status: CollabAgentToolCallStatus,
    agents: &[codex_protocol::ThreadId],
) -> TurnItem {
    TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
        id: call_id.to_string(),
        tool: CollabAgentTool::Wait,
        status,
        sender_thread_id: session.thread_id,
        receiver_thread_ids: agents.to_vec(),
        receiver_agents: Vec::new(),
        prompt: None,
        model: None,
        reasoning_effort: None,
        agents_states: HashMap::new(),
        mailbox_wait_report: Some(report.clone()),
    })
}

async fn deliver_envelope(
    session: &Session,
    turn: &TurnContext,
    envelope: &codex_agent_mailbox::Envelope,
) -> Result<(), FunctionCallError> {
    if envelope.recipient != session.thread_id {
        return Err(mailbox_error(
            "mailbox body is not addressed to this native thread",
        ));
    }
    let pending = PendingAgentMessage::new(
        envelope.communication.clone(),
        Some(MessageDelivery::restored_mailbox_input(
            envelope.id.clone(),
            Some(envelope.id.clone()),
        )),
    );
    session
        .record_inter_agent_communication_checked(turn, turn.model_info(), pending)
        .await
        .map_err(mailbox_error)
}

async fn admit_selected(
    session: &Session,
    turn: &TurnContext,
    mailbox: &Mailbox,
    selected: &[SnapshotItem],
    body_limit: usize,
) -> Result<(), FunctionCallError> {
    let mut observations = Vec::new();
    let mut delivered = HashSet::new();
    for item in selected {
        match item {
            SnapshotItem::Work(work) if !work.observed => {
                if let Some(outcome) = &work.outcome {
                    if !delivered.contains(&outcome.id) && delivered.len() >= body_limit {
                        continue;
                    }
                    let envelope = mailbox
                        .get_message(session.thread_id, &outcome.id)
                        .await
                        .map_err(mailbox_error)?
                        .ok_or_else(|| mailbox_error("terminal outcome body is unavailable"))?;
                    deliver_envelope(session, turn, &envelope).await?;
                    delivered.insert(outcome.id.clone());
                    observations.push(ObservationTarget::Work(work.work_ref.clone()));
                }
            }
            SnapshotItem::Request(request) if !request.observed => {
                if let Some(reply) = &request.reply {
                    if !delivered.contains(&reply.id) && delivered.len() >= body_limit {
                        continue;
                    }
                    deliver_envelope(session, turn, reply).await?;
                    delivered.insert(reply.id.clone());
                    observations.push(ObservationTarget::Reply(reply.id.clone()));
                }
            }
            SnapshotItem::Work(_) | SnapshotItem::Request(_) => {}
        }
    }
    if !observations.is_empty() {
        mailbox
            .acknowledge_observed(session.thread_id, &observations)
            .await
            .map_err(mailbox_error)?;
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    timeout_ms: Option<i64>,
    targets: Option<Vec<String>>,
    replies_to: Option<Vec<String>>,
    #[serde(default)]
    return_when: MailboxWaitReturnWhen,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WaitAgentResult {
    pub(crate) message: String,
    pub(crate) timed_out: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mailbox_wait_report: Option<MailboxWaitReport>,
}

impl WaitAgentResult {
    fn from_outcome(
        outcome: WaitOutcome,
        requested_timeout_ms: Option<i64>,
        timeout_ms: i64,
    ) -> Self {
        let message = match outcome {
            WaitOutcome::MailboxActivity => "Wait completed.",
            WaitOutcome::Steered => "Wait interrupted by new input.",
            WaitOutcome::TimedOut => "Wait timed out.",
        };
        let message = match requested_timeout_ms {
            Some(requested_timeout_ms) if requested_timeout_ms < timeout_ms => format!(
                "{message}\n\nRequested timeout of {requested_timeout_ms}ms was clamped to the minimum of {timeout_ms}ms."
            ),
            Some(_) | None => message.to_string(),
        };
        Self {
            message,
            timed_out: outcome == WaitOutcome::TimedOut,
            mailbox_wait_report: None,
        }
    }
}

async fn handle_mailbox_wait(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    call_id: String,
    args: WaitArgs,
    requested_timeout_ms: Option<i64>,
    timeout_ms: i64,
    mailbox: Arc<Mailbox>,
    cancellation_token: tokio_util::sync::CancellationToken,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let targetless = args.targets.is_none() && args.replies_to.is_none();
    let targets = args.targets.unwrap_or_default();
    let replies_to = args.replies_to.unwrap_or_default();
    validate_wait_selectors(&targets, &replies_to)?;
    let return_when = match args.return_when {
        MailboxWaitReturnWhen::Any => ReturnWhen::Any,
        MailboxWaitReturnWhen::All => ReturnWhen::All,
    };
    let mut selectors = Vec::new();
    let mut agents = Vec::new();
    for target in &targets {
        if target.starts_with("work_") {
            selectors.push(Selector::Work(target.clone()));
        } else {
            let agent = resolve_agent_target(&session, &turn, target).await?;
            if agent == session.thread_id {
                return Err(mailbox_error("a native thread cannot join its own work"));
            }
            agents.push(agent);
            let issued = mailbox
                .issued_work_refs(session.thread_id, Some(agent), /*limit*/ 100)
                .await
                .map_err(mailbox_error)?;
            selectors.extend(issued.into_iter().map(|work| Selector::Work(work.id)));
        }
    }
    selectors.extend(replies_to.iter().cloned().map(Selector::Reply));
    let mut unique = HashSet::new();
    selectors.retain(|selector| unique.insert(selector_key(selector)));
    if selectors.len() > 100 {
        return Err(mailbox_error(
            "at most 100 exact work/reply joins are supported",
        ));
    }

    // Subscribe before the authoritative reads so no mailbox publication can be missed.
    let mut mailbox_rx = mailbox.subscribe();
    let turn_state = session
        .input_queue
        .turn_state_for_sub_id(&session.active_turn, &turn.sub_id)
        .await;
    let (mut input_rx, pending_activity) = session
        .input_queue
        .subscribe_activity(turn_state.as_deref())
        .await;
    let started = Instant::now();
    let deadline = started + Duration::from_millis(timeout_ms as u64);
    let wait_id = format!("{}:{call_id}", session.thread_id);
    let mut report = MailboxWaitReport {
        requested_targets: targets,
        requested_replies_to: replies_to,
        return_when: args.return_when,
        completed: Vec::new(),
        pending: Vec::new(),
        completion_reason: None,
        condition_satisfied: false,
        quiet_count: 0,
        wake_sender: None,
        wake_ref: None,
    };
    let mut selected = Vec::new();
    let mut registration = None;
    let mut started_item = false;
    let mut last_report = None;

    loop {
        drop(registration.take());
        let input_generation = mailbox.wait_input_generation();
        if targetless {
            match mailbox
                .issued_work_refs(session.thread_id, None, /*limit*/ 100)
                .await
            {
                Ok(work) => {
                    selectors = work
                        .into_iter()
                        .map(|work| Selector::Work(work.id))
                        .collect();
                }
                Err(_) => {
                    report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
                    break;
                }
            }
        }
        selected = if selectors.is_empty() {
            Vec::new()
        } else {
            match mailbox
                .snapshot(&Selection {
                    caller: session.thread_id,
                    agent: None,
                    selectors: selectors.clone(),
                    return_when,
                    limit: 100,
                })
                .await
            {
                Ok(snapshot) => snapshot.items,
                Err(_) => {
                    report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
                    break;
                }
            }
        };
        for item in &selected {
            if let SnapshotItem::Work(work) = item
                && !agents.contains(&work.work_ref.agent)
            {
                agents.push(work.work_ref.agent);
            }
        }
        update_report(&session, &mut report, &selected);
        report.quiet_count = match mailbox.quiet_count(session.thread_id).await {
            Ok(count) => count,
            Err(_) => {
                report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
                break;
            }
        };
        if !started_item || last_report.as_ref() != Some(&report) {
            validate_wait_report(&report)?;
            let item = wait_report_item(
                &session,
                &call_id,
                &report,
                CollabAgentToolCallStatus::InProgress,
                &agents,
            );
            session.emit_turn_item_started(&turn, &item).await;
            started_item = true;
            last_report = Some(report.clone());
        }
        if mailbox.has_runtime_fault(&agents) {
            report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
            break;
        }
        if !targetless && (selectors.is_empty() || selected.len() != selectors.len()) {
            report.completion_reason = Some(MailboxWakeCause::NoMatchingWork);
            break;
        }
        if let Some(item) = selected.iter().find(|item| needs_attention(item)) {
            report.completion_reason = Some(MailboxWakeCause::NeedsAttention);
            report.wake_ref = match item {
                SnapshotItem::Work(work) => {
                    report.wake_sender = Some(readable_agent_name(&session, work.work_ref.agent));
                    Some(work.work_ref.id.clone())
                }
                SnapshotItem::Request(request) => Some(request.request_id.clone()),
            };
            break;
        }
        let ready_count = selected.iter().filter(|item| is_ready(item)).count();
        let satisfied = !selected.is_empty()
            && match return_when {
                ReturnWhen::Any => ready_count > 0,
                ReturnWhen::All => ready_count == selectors.len(),
            };
        let all_observed = !selected.is_empty() && selected.iter().all(already_observed);
        if all_observed {
            report.condition_satisfied = true;
            report.completion_reason = Some(MailboxWakeCause::AlreadySatisfied);
            break;
        }
        if satisfied {
            report.condition_satisfied = true;
            report.completion_reason = Some(if selected.iter().any(|item| {
                matches!(item, SnapshotItem::Request(request) if request.reply.is_some())
            }) {
                MailboxWakeCause::Reply
            } else {
                MailboxWakeCause::Result
            });
            if let Some(item) = selected.iter().find(|item| is_ready(item)) {
                match item {
                    SnapshotItem::Work(work) => {
                        report.wake_ref = Some(work.work_ref.id.clone());
                        report.wake_sender =
                            Some(readable_agent_name(&session, work.work_ref.agent));
                    }
                    SnapshotItem::Request(request) => {
                        report.wake_ref = Some(request.request_id.clone());
                        report.wake_sender = request
                            .reply
                            .as_ref()
                            .map(|reply| reply.communication.author.to_string());
                    }
                }
            }
            break;
        }

        // Explicit request/alert attention is a separate wake contract from quiet notes.
        match mailbox.attention(session.thread_id, /*limit*/ 100).await {
            Ok(attention) => {
                for attention in attention {
                    if let Some(message_id) = attention
                        .id
                        .strip_prefix("attention:")
                        .filter(|id| id.starts_with("amsg_"))
                    {
                        let Some(envelope) = mailbox
                            .get_message(session.thread_id, message_id)
                            .await
                            .map_err(mailbox_error)?
                        else {
                            report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
                            break;
                        };
                        if deliver_envelope(&session, &turn, &envelope).await.is_err() {
                            report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
                            break;
                        }
                        if mailbox
                            .acknowledge_observed(
                                session.thread_id,
                                &[ObservationTarget::Attention(attention.id.clone())],
                            )
                            .await
                            .is_err()
                        {
                            report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
                            break;
                        }
                        report.wake_sender = Some(envelope.communication.author.to_string());
                        report.wake_ref = Some(envelope.id);
                        report.completion_reason = Some(if envelope.kind == MessageKind::Request {
                            MailboxWakeCause::Request
                        } else {
                            MailboxWakeCause::NeedsAttention
                        });
                        break;
                    }
                    if let Some(work_id) = attention.id.strip_prefix("attention:work_") {
                        let work_id = format!("work_{work_id}");
                        match mailbox
                            .snapshot(&Selection {
                                caller: session.thread_id,
                                agent: None,
                                selectors: vec![Selector::Work(work_id.clone())],
                                return_when: ReturnWhen::Any,
                                limit: 1,
                            })
                            .await
                        {
                            Ok(snapshot) => {
                                if let Some(item) = snapshot.items.into_iter().next() {
                                    if !selected.iter().any(|existing| {
                                        selector_key_for_snapshot(existing)
                                            == selector_key_for_snapshot(&item)
                                    }) {
                                        selected.push(item);
                                    }
                                }
                            }
                            Err(_) => {
                                report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
                                break;
                            }
                        }
                        report.wake_ref = Some(work_id.clone());
                        report.wake_sender = match selected.iter().find(|item| {
                            matches!(item, SnapshotItem::Work(work) if work.work_ref.id == work_id)
                        }) {
                            Some(SnapshotItem::Work(work)) => {
                                Some(readable_agent_name(&session, work.work_ref.agent))
                            }
                            Some(SnapshotItem::Request(_)) | None => None,
                        };
                        report.completion_reason = Some(MailboxWakeCause::NeedsAttention);
                        break;
                    }
                }
            }
            Err(_) => report.completion_reason = Some(MailboxWakeCause::RuntimeFault),
        }
        if report.completion_reason.is_some() {
            break;
        }
        if pending_activity == Some(InputQueueActivity::Steer) {
            report.completion_reason = Some(MailboxWakeCause::Human);
            break;
        }
        if cancellation_token.is_cancelled() {
            report.completion_reason = Some(MailboxWakeCause::Cancel);
            break;
        }
        if Instant::now() >= deadline {
            report.completion_reason = Some(MailboxWakeCause::Deadline);
            break;
        }
        let peers = selected
            .iter()
            .filter_map(|item| match item {
                SnapshotItem::Work(work) if work.outcome.is_none() => Some(work.work_ref.agent),
                SnapshotItem::Request(request) if request.reply.is_none() => {
                    Some(request.request.recipient)
                }
                SnapshotItem::Work(_) | SnapshotItem::Request(_) => None,
            })
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        match mailbox.begin_wait_at(
            wait_id.clone(),
            session.thread_id,
            peers,
            return_when,
            /*has_escape*/ true,
            input_generation,
        ) {
            Ok(guard) => registration = Some(guard),
            Err(codex_agent_mailbox::MailboxError::StaleWaitSnapshot) => continue,
            Err(codex_agent_mailbox::MailboxError::ClosedPeerCycle) => {
                report.completion_reason = Some(MailboxWakeCause::Cycle);
                break;
            }
            Err(_) => {
                report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
                break;
            }
        }
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                report.completion_reason = Some(MailboxWakeCause::Cancel);
                break;
            }
            _ = tokio::time::sleep_until(deadline) => {
                report.completion_reason = Some(MailboxWakeCause::Deadline);
                break;
            }
            changed = mailbox_rx.changed() => {
                if changed.is_err() {
                    report.completion_reason = Some(MailboxWakeCause::SubscriptionLoss);
                    break;
                }
            }
            changed = input_rx.changed() => {
                if changed.is_err() {
                    report.completion_reason = Some(MailboxWakeCause::SubscriptionLoss);
                    break;
                }
                if *input_rx.borrow_and_update() == InputQueueActivity::Steer {
                    report.completion_reason = Some(MailboxWakeCause::Human);
                    break;
                }
                // Mailbox activity is only a reason to rescan. A quiet notification
                // never returns control to the provider ahead of the fixed deadline.
            }
        }
    }
    drop(registration);
    if admit_selected(&session, &turn, &mailbox, &selected, /*body_limit*/ 2)
        .await
        .is_err()
    {
        report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
        report.condition_satisfied = false;
    }
    update_report(&session, &mut report, &selected);
    report.quiet_count = mailbox
        .quiet_count(session.thread_id)
        .await
        .unwrap_or(report.quiet_count);
    validate_wait_report(&report)?;
    let completed = wait_report_item(
        &session,
        &call_id,
        &report,
        CollabAgentToolCallStatus::Completed,
        &agents,
    );
    session.emit_turn_item_completed(&turn, completed).await;
    turn.session_telemetry.record_duration(
        "codex.multi_agent.wait.duration_ms",
        started.elapsed(),
        &[(
            "outcome",
            match report
                .completion_reason
                .unwrap_or(MailboxWakeCause::RuntimeFault)
            {
                MailboxWakeCause::Result
                | MailboxWakeCause::Reply
                | MailboxWakeCause::AlreadySatisfied => "completed",
                MailboxWakeCause::Deadline => "timed_out",
                MailboxWakeCause::Human
                | MailboxWakeCause::Request
                | MailboxWakeCause::NeedsAttention => "interrupted",
                _ => "faulted",
            },
        )],
    );
    let timed_out = report.completion_reason == Some(MailboxWakeCause::Deadline);
    let mut message = match report.completion_reason {
        Some(
            MailboxWakeCause::Result | MailboxWakeCause::Reply | MailboxWakeCause::AlreadySatisfied,
        ) => "Wait condition satisfied.".to_string(),
        Some(MailboxWakeCause::Deadline) => "Wait timed out.".to_string(),
        Some(MailboxWakeCause::Human) => "Wait interrupted by new input.".to_string(),
        Some(MailboxWakeCause::Request | MailboxWakeCause::NeedsAttention) => {
            "Wait interrupted by mailbox attention.".to_string()
        }
        Some(MailboxWakeCause::Cancel | MailboxWakeCause::Interrupted) => {
            "Wait cancelled.".to_string()
        }
        Some(MailboxWakeCause::NoMatchingWork) => "No matching mailbox work was found.".to_string(),
        Some(MailboxWakeCause::Cycle) => {
            "Wait stopped because the native dependency graph is cyclic.".to_string()
        }
        _ => "Wait stopped because mailbox state could not be read safely.".to_string(),
    };
    if let Some(requested_ms) = requested_timeout_ms.filter(|requested| *requested < timeout_ms) {
        message.push_str(&format!(
            "\n\nRequested timeout of {requested_ms}ms was clamped to the minimum of {timeout_ms}ms."
        ));
    }
    Ok(boxed_tool_output(WaitAgentResult {
        message,
        timed_out,
        mailbox_wait_report: Some(report),
    }))
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
enum WaitOutcome {
    MailboxActivity,
    Steered,
    TimedOut,
}

async fn wait_for_activity(
    activity_rx: &mut tokio::sync::watch::Receiver<InputQueueActivity>,
    pending_activity: Option<InputQueueActivity>,
    deadline: Instant,
) -> WaitOutcome {
    if let Some(activity) = pending_activity {
        return match activity {
            InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
            InputQueueActivity::Steer => WaitOutcome::Steered,
        };
    }
    match timeout_at(deadline, activity_rx.changed()).await {
        Ok(Ok(())) => match *activity_rx.borrow_and_update() {
            InputQueueActivity::Mailbox => WaitOutcome::MailboxActivity,
            InputQueueActivity::Steer => WaitOutcome::Steered,
        },
        Ok(Err(_)) | Err(_) => WaitOutcome::TimedOut,
    }
}
