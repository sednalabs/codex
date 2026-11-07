//! Exact durable joins layered on the protected native wait notification contract.
//!
//! SQLite snapshots own completion. Queue notifications only prompt a rescan;
//! quiet messages are neither drained nor acknowledged by this adapter.

use super::*;
use codex_agent_mailbox::DispatchState;
use codex_agent_mailbox::Mailbox;
use codex_agent_mailbox::MessageKind;
use codex_agent_mailbox::ObservationTarget;
use codex_agent_mailbox::Outcome;
use codex_agent_mailbox::RequestDisposition;
use codex_agent_mailbox::ReturnWhen as DurableReturnWhen;
use codex_agent_mailbox::Selection;
use codex_agent_mailbox::Selector;
use codex_agent_mailbox::SnapshotItem;
use codex_protocol::items::MailboxWaitReport;
use codex_protocol::items::MailboxWaitReturnWhen;
use codex_protocol::items::MailboxWaitSelectionItem;
use codex_protocol::items::MailboxWakeCause;
use codex_protocol::protocol::InterAgentCommunication;
use std::sync::Arc;

pub(super) fn mailbox_error(error: impl std::fmt::Display) -> FunctionCallError {
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
        .get_agent_metadata(agent)
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
    session
        .record_inter_agent_communication_checked(turn, envelope.communication.clone())
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


fn selected_body_ids(selected: &[SnapshotItem]) -> HashSet<String> {
    selected.iter().filter_map(|item| match item {
        SnapshotItem::Work(work) => work.outcome.as_ref().map(|outcome| outcome.id.clone()),
        SnapshotItem::Request(request) => request.reply.as_ref().map(|reply| reply.id.clone()),
    }).collect()
}

fn selection_satisfied(selected: &[SnapshotItem], mode: DurableReturnWhen) -> bool {
    !selected.is_empty() && match mode {
        DurableReturnWhen::Any => selected.iter().any(is_ready),
        DurableReturnWhen::All => selected.iter().all(is_ready),
    }
}

fn actionable_envelope(envelope: &codex_agent_mailbox::Envelope) -> bool {
    // An explicit queue-only intent always wins over the historical kind field.
    // Older rows predate typed intent and retain their request/alert contract.
    envelope.communication.intent != Some(AgentMessageIntent::Progress)
        && (is_actionable_wait_communication(&envelope.communication)
            || matches!(envelope.kind, MessageKind::Request | MessageKind::Alert))
}

fn durable_wake_source(cause: MailboxWakeCause) -> WakeSource {
    match cause {
        MailboxWakeCause::Result | MailboxWakeCause::Reply | MailboxWakeCause::AlreadySatisfied => WakeSource::TargetCompletion,
        MailboxWakeCause::Human => WakeSource::OperatorMessage,
        MailboxWakeCause::Request | MailboxWakeCause::NeedsAttention => WakeSource::Mailbox,
        MailboxWakeCause::Deadline => WakeSource::Timeout,
        MailboxWakeCause::SubscriptionLoss => WakeSource::SubscriptionLoss,
        _ => WakeSource::RuntimeSystemEvent,
    }
}

async fn actionable_queue_activity(
    session: &Session,
    ignored_ids: &HashSet<String>,
    initial: Option<InputQueueActivity>,
) -> Option<InputQueueActivity> {
    let activity = session.input_queue.pending_wait_input_activity(&session.active_turn).await.or(initial);
    if matches!(activity, Some(InputQueueActivity::Steer | InputQueueActivity::RuntimeSystemEvent | InputQueueActivity::TerminalCompletion)) {
        return activity;
    }
    let mut entries = session.input_queue.snapshot_mailbox_communications().await;
    entries.extend(session.input_queue.snapshot_pending_mailbox_communications(&session.active_turn).await);
    entries.iter().any(|(communication, _, _)| {
        is_actionable_wait_communication(communication)
            && !communication.id.as_ref().is_some_and(|id| ignored_ids.contains(id.as_str()))
    }).then_some(InputQueueActivity::Mailbox)
}

async fn emit_started(session: &Session, turn: &TurnContext, call_id: &str, agents: &[ThreadId]) {
    session.emit_turn_item_started(turn, &TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
        id: call_id.to_string(),
        tool: CollabAgentTool::Wait,
        status: CollabAgentToolCallStatus::InProgress,
        sender_thread_id: session.thread_id,
        receiver_thread_ids: agents.to_vec(),
        receiver_agents: Vec::new(),
        prompt: None,
        model: None,
        reasoning_effort: None,
        requested_model: None,
        requested_reasoning_effort: None,
        agents_states: HashMap::new(),
        wake_notifications: None,
        completion_reason: None,
        wake_cause: None,
    })).await;
}

pub(super) async fn handle(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    call_id: String,
    args: WaitArgs,
    mailbox: Arc<Mailbox>,
    timeout_ms: i64,
    cancellation_token: tokio_util::sync::CancellationToken,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let targetless = args.targets.is_empty() && args.replies_to.is_none();
    let targets = args.targets;
    let replies_to = args.replies_to.unwrap_or_default();
    validate_wait_selectors(&targets, &replies_to)?;
    let return_when = match args.return_when {
        super::ReturnWhen::Any => DurableReturnWhen::Any,
        super::ReturnWhen::All => DurableReturnWhen::All,
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
            if !agents.contains(&agent) {
                agents.push(agent);
            }
            let work = mailbox.issued_work_refs(session.thread_id, Some(agent), 100).await.map_err(mailbox_error)?;
            selectors.extend(work.into_iter().map(|work| Selector::Work(work.id)));
        }
    }
    selectors.extend(replies_to.iter().cloned().map(Selector::Reply));
    let mut unique = HashSet::new();
    selectors.retain(|selector| unique.insert(selector_key(selector)));
    if selectors.len() > 100 {
        return Err(mailbox_error("at most 100 exact work/reply joins are supported"));
    }

    // Subscribe before the first authoritative read. Notifications never consume
    // queue entries and are only a reason to rescan the durable selection.
    let mut mailbox_rx = mailbox.subscribe();
    let (mut input_rx, mut pending_activity) = session.input_queue.subscribe_activity(None).await;
    let started = Instant::now();
    let mut deadline = started + Duration::from_millis(timeout_ms as u64);
    let native_event_wait = args.native_event_wait;
    let timer_enabled = lease_timer_enabled(native_event_wait, timeout_ms);
    let lease_duration = Duration::from_millis(timeout_ms.max(1) as u64);
    let wait_id = format!("{}:{call_id}", session.thread_id);
    let mut report = MailboxWaitReport {
        requested_targets: targets,
        requested_replies_to: replies_to,
        return_when: match return_when {
            DurableReturnWhen::Any => MailboxWaitReturnWhen::Any,
            DurableReturnWhen::All => MailboxWaitReturnWhen::All,
        },
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
    loop {
        drop(registration.take());
        let input_generation = mailbox.wait_input_generation();
        if targetless {
            selectors = match mailbox.issued_work_refs(session.thread_id, None, 100).await {
                Ok(work) => work.into_iter().map(|work| Selector::Work(work.id)).collect(),
                Err(_) => { report.completion_reason = Some(MailboxWakeCause::RuntimeFault); break; }
            };
        }
        selected = if selectors.is_empty() {
            Vec::new()
        } else {
            match mailbox.snapshot(&Selection {
                caller: session.thread_id, agent: None, selectors: selectors.clone(), return_when, limit: 100,
            }).await {
                Ok(snapshot) => snapshot.items,
                Err(_) => { report.completion_reason = Some(MailboxWakeCause::RuntimeFault); break; }
            }
        };
        for item in &selected {
            let agent = match item {
                SnapshotItem::Work(work) => work.work_ref.agent,
                SnapshotItem::Request(request) => request.request.recipient,
            };
            if !agents.contains(&agent) { agents.push(agent); }
        }
        if native_event_wait {
            let current_path = turn.session_source.get_agent_path().or_else(|| session.services.agent_control
                .get_agent_metadata(session.thread_id).and_then(|metadata| metadata.agent_path));
            // An exact request/reply obligation is allowed for a leaf, including
            // a reply from its parent. Work joins retain the protected reverse
            // dependency rejection; the mailbox graph handles reply cycles.
            for agent in selected.iter().filter_map(|item| match item {
                SnapshotItem::Work(work) => Some(work.work_ref.agent),
                SnapshotItem::Request(_) => None,
            }) {
                if agent == session.thread_id {
                    return Err(mailbox_error("a native thread cannot join its own work"));
                }
                let target_path = session.services.agent_control.get_agent_metadata(agent).and_then(|metadata| metadata.agent_path);
                if let Some(error) = reverse_wait_error(current_path.as_ref(), target_path.as_ref()) {
                    return Err(FunctionCallError::RespondToModel(error));
                }
            }
        }
        if !started_item {
            emit_started(&session, &turn, &call_id, &agents).await;
            started_item = true;
        }
        update_report(&session, &mut report, &selected);
        report.quiet_count = match mailbox.quiet_count(session.thread_id).await {
            Ok(count) => count,
            Err(_) => { report.completion_reason = Some(MailboxWakeCause::RuntimeFault); break; }
        };
        validate_wait_report(&report)?;
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
            match item {
                SnapshotItem::Work(work) => {
                    report.wake_ref = Some(work.work_ref.id.clone());
                    report.wake_sender = Some(readable_agent_name(&session, work.work_ref.agent));
                }
                SnapshotItem::Request(request) => report.wake_ref = Some(request.request_id.clone()),
            }
            break;
        }
        if selection_satisfied(&selected, return_when) {
            report.condition_satisfied = true;
            report.completion_reason = Some(if selected.iter().all(already_observed) {
                MailboxWakeCause::AlreadySatisfied
            } else if selected.iter().any(|item| matches!(item, SnapshotItem::Request(request) if request.reply.is_some())) {
                MailboxWakeCause::Reply
            } else { MailboxWakeCause::Result });
            if let Some(item) = selected.iter().find(|item| is_ready(item)) {
                match item {
                    SnapshotItem::Work(work) => {
                        report.wake_ref = Some(work.work_ref.id.clone());
                        report.wake_sender = Some(readable_agent_name(&session, work.work_ref.agent));
                    }
                    SnapshotItem::Request(request) => {
                        report.wake_ref = Some(request.request_id.clone());
                        report.wake_sender = request.reply.as_ref().map(|reply| reply.communication.author.to_string());
                    }
                }
            }
            break;
        }

        // Selected partial results do not turn an `all` join into an `any` join.
        // Unrelated actionable messages and operator/system input retain #1000's
        // interrupt behavior. Explicit Progress remains queued and unobserved.
        let ignored_ids = selected_body_ids(&selected);
        let attention = match mailbox.attention(session.thread_id, 100).await {
            Ok(attention) => attention,
            Err(_) => { report.completion_reason = Some(MailboxWakeCause::RuntimeFault); break; }
        };
        for attention in attention {
            if let Some(message_id) = attention.id.strip_prefix("attention:").filter(|id| id.starts_with("amsg_")) {
                let envelope = match mailbox.get_message(session.thread_id, message_id).await {
                    Ok(Some(envelope)) => envelope,
                    _ => { report.completion_reason = Some(MailboxWakeCause::RuntimeFault); break; }
                };
                if !actionable_envelope(&envelope) || ignored_ids.contains(&envelope.id) { continue; }
                if deliver_envelope(&session, &turn, &envelope).await.is_err()
                    || mailbox.acknowledge_observed(session.thread_id, &[ObservationTarget::Attention(attention.id)]).await.is_err()
                {
                    report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
                    break;
                }
                report.wake_ref = Some(envelope.id);
                report.wake_sender = Some(envelope.communication.author.to_string());
                report.completion_reason = Some(if envelope.kind == MessageKind::Request { MailboxWakeCause::Request } else { MailboxWakeCause::NeedsAttention });
                break;
            }
            if let Some(work_id) = attention.id.strip_prefix("attention:work_") {
                let work_id = format!("work_{work_id}");
                match mailbox.snapshot(&Selection {
                    caller: session.thread_id, agent: None, selectors: vec![Selector::Work(work_id.clone())], return_when: DurableReturnWhen::Any, limit: 1,
                }).await {
                    Ok(snapshot) => {
                        if let Some(item) = snapshot.items.into_iter().next()
                            && !selected.iter().any(|existing| selector_key_for_snapshot(existing) == selector_key_for_snapshot(&item))
                        { selected.push(item); }
                    }
                    Err(_) => { report.completion_reason = Some(MailboxWakeCause::RuntimeFault); break; }
                }
                report.wake_ref = Some(work_id);
                report.completion_reason = Some(MailboxWakeCause::NeedsAttention);
                break;
            }
        }
        if report.completion_reason.is_some() { break; }
        if let Some(activity) = actionable_queue_activity(&session, &ignored_ids, pending_activity.take()).await {
            report.completion_reason = Some(match activity {
                InputQueueActivity::Steer => MailboxWakeCause::Human,
                InputQueueActivity::TerminalCompletion | InputQueueActivity::RuntimeSystemEvent => MailboxWakeCause::Tool,
                InputQueueActivity::Mailbox => MailboxWakeCause::Request,
            });
            break;
        }
        if cancellation_token.is_cancelled() {
            report.completion_reason = Some(MailboxWakeCause::Cancel);
            break;
        }
        if timer_enabled && Instant::now() >= deadline {
            if native_event_wait {
                deadline = Instant::now() + lease_duration;
            } else {
                report.completion_reason = Some(MailboxWakeCause::Deadline);
                break;
            }
        }
        let peers = selected.iter().filter(|item| !is_ready(item)).map(|item| match item {
            SnapshotItem::Work(work) => work.work_ref.agent,
            SnapshotItem::Request(request) => request.request.recipient,
        }).collect::<HashSet<_>>().into_iter().collect();
        match mailbox.begin_wait_at(wait_id.clone(), session.thread_id, peers, return_when,
            /*has_escape*/ !native_event_wait, input_generation)
        {
            Ok(guard) => registration = Some(guard),
            Err(codex_agent_mailbox::MailboxError::StaleWaitSnapshot) => continue,
            Err(codex_agent_mailbox::MailboxError::ClosedPeerCycle) => {
                report.completion_reason = Some(MailboxWakeCause::Cycle); break;
            }
            Err(_) => { report.completion_reason = Some(MailboxWakeCause::RuntimeFault); break; }
        }
        let timer = if timer_enabled {
            future::Either::Left(tokio::time::sleep_until(deadline))
        } else { future::Either::Right(future::pending()) };
        tokio::pin!(timer);
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => { report.completion_reason = Some(MailboxWakeCause::Cancel); break; }
            changed = mailbox_rx.changed() => {
                if changed.is_err() { report.completion_reason = Some(MailboxWakeCause::SubscriptionLoss); break; }
            }
            changed = input_rx.changed() => {
                if changed.is_err() { report.completion_reason = Some(MailboxWakeCause::SubscriptionLoss); break; }
                let activity = *input_rx.borrow_and_update();
                if matches!(activity, InputQueueActivity::Steer | InputQueueActivity::TerminalCompletion | InputQueueActivity::RuntimeSystemEvent) {
                    pending_activity = Some(activity);
                }
            }
            _ = &mut timer => {
                // Re-read durable state before declaring expiry. In native mode
                // this renews the runtime-owned lease without a provider turn.
            }
        }
    }
    drop(registration);
    if !started_item { emit_started(&session, &turn, &call_id, &agents).await; }
    if admit_selected(&session, &turn, &mailbox, &selected, 2).await.is_err() {
        report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
        report.condition_satisfied = false;
    }
    update_report(&session, &mut report, &selected);
    report.quiet_count = mailbox.quiet_count(session.thread_id).await.unwrap_or(report.quiet_count);
    validate_wait_report(&report)?;
    let cause = report.completion_reason.unwrap_or(MailboxWakeCause::RuntimeFault);
    let wake_source = durable_wake_source(cause);
    let statuses = collect_current_wait_statuses(&session, &agents).await;
    let pending_ids = pending_wait_thread_ids(&agents, &statuses);
    let snapshot = mailbox_snapshot_with_exclusions(&session, &call_id, &agents,
        target_set_relation(targetless), wake_source, &selected_body_ids(&selected)).await;
    let mut result = WaitAgentResult::new(agents.clone(), pending_ids, wake_source.completion_reason(), snapshot.notifications, snapshot.provenance);
    if cause == MailboxWakeCause::Cancel {
        result.wake_provenance.wake_cause = AgentWakeCause::CancellationInterruption;
    }
    result.message = match cause {
        MailboxWakeCause::Result | MailboxWakeCause::Reply | MailboxWakeCause::AlreadySatisfied => "Wait condition satisfied.",
        MailboxWakeCause::NoMatchingWork => "No matching mailbox work was found.",
        MailboxWakeCause::Cycle => "Wait stopped because the native dependency graph is cyclic.",
        MailboxWakeCause::RuntimeFault => "Wait stopped because mailbox custody could not be verified.",
        MailboxWakeCause::Cancel | MailboxWakeCause::Interrupted => "Wait cancelled.",
        _ => result.message.as_str(),
    }.to_string();
    result.mailbox_wait_report = Some(report);
    emit_wait_completion(&session, &turn, call_id, agents, Vec::new(), statuses,
        result.completion_reason, result.wake_notifications.clone().unwrap_or_default(), Some(result.wake_provenance.wake_cause)).await;
    turn.session_telemetry.record_duration("codex.multi_agent.wait.duration_ms", started.elapsed(), &[("outcome", match cause {
        MailboxWakeCause::Result | MailboxWakeCause::Reply | MailboxWakeCause::AlreadySatisfied => "completed",
        MailboxWakeCause::Deadline => "timed_out",
        MailboxWakeCause::Human | MailboxWakeCause::Request | MailboxWakeCause::NeedsAttention | MailboxWakeCause::Tool => "interrupted",
        _ => "faulted",
    })]);
    Ok(boxed_tool_output(result))
}

#[cfg(test)]
#[path = "wait_mailbox_v2_tests.rs"]
mod tests;
