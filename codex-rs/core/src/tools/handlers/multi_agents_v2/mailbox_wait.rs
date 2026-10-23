//! Selection-aware waits over immutable native work and explicit request replies.

use super::*;
use super::mailbox_message::deliver_envelope;
use super::mailbox_message::mailbox_error;
use super::mailbox_message::mailbox_for;
use crate::session::input_queue::InputQueueActivity;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::handlers::multi_agents_spec::create_mailbox_wait_agent_tool;
use codex_agent_mailbox::DispatchState;
use codex_agent_mailbox::Mailbox;
use codex_agent_mailbox::ObservationTarget;
use codex_agent_mailbox::Outcome;
use codex_agent_mailbox::RequestDisposition;
use codex_agent_mailbox::ReturnWhen;
use codex_agent_mailbox::Selection;
use codex_agent_mailbox::Selector;
use codex_agent_mailbox::SnapshotItem;
use codex_protocol::ThreadId;
use codex_protocol::items::MailboxWaitReport;
use codex_protocol::items::MailboxWaitReturnWhen;
use codex_protocol::items::MailboxWaitSelectionItem;
use codex_protocol::items::MailboxWakeCause;
use codex_tools::ToolSpec;
use std::collections::HashSet;
use std::time::Duration;
use tokio::time::Instant;

pub(crate) struct Handler;

#[cfg(test)]
#[path = "mailbox_wait_tests.rs"]
mod tests;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    #[serde(alias = "ids")]
    targets: Option<Vec<String>>,
    replies_to: Option<Vec<String>>,
    #[serde(default)]
    return_when: MailboxWaitReturnWhen,
    timeout_ms: Option<u64>,
    /// Compatibility only: mailbox waits are always native and event-driven.
    native_event_wait: Option<bool>,
}

impl ToolExecutor<ToolInvocation> for Handler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("wait_agent")
    }

    fn spec(&self) -> ToolSpec {
        create_mailbox_wait_agent_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(handle_wait(invocation))
    }
}

impl CoreToolRuntime for Handler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn waits_for_runtime_cancellation(&self) -> bool {
        true
    }
}

async fn handle_wait(invocation: ToolInvocation) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let ToolInvocation { session, turn, payload, call_id, cancellation_token, .. } = invocation;
    if let Some(report) = session.mailbox_interrupted_wait_receipt(&call_id).await {
        return Ok(boxed_tool_output(FunctionToolOutput::from_text(
            tool_output_json_text(&report, "wait_agent"),
            Some(false),
        )));
    }
    let args: WaitArgs = parse_arguments(&function_arguments(payload)?)?;
    let _compatibility_only = args.native_event_wait;
    let mailbox = mailbox_for(&session)?;
    session.services.agent_control.register_session_root(session.thread_id, turn.parent_thread_id);
    // Both subscriptions precede authoritative rescans. Quiet notifications may rescan, but
    // never shorten the fixed deadline or return control to the provider.
    let mut mailbox_rx = mailbox.subscribe();
    let (mut input_rx, _) = session.input_queue.subscribe_activity(/*turn_state*/ None).await;
    let targetless = args.targets.is_none() && args.replies_to.is_none();
    let targets = args.targets.unwrap_or_default();
    let replies_to = args.replies_to.unwrap_or_default();
    if targets.len() + replies_to.len() > 100 {
        return Err(mailbox_error("at most 100 work/reply selectors may be joined"));
    }
    let mut selectors = Vec::new();
    let mut agents = Vec::new();
    for target in &targets {
        if target.starts_with("work_") {
            selectors.push(Selector::Work(target.clone()));
        } else {
            let agent = resolve_agent_target(&session, &turn, target).await?;
            reject_reverse_completion_join(&session, &turn, agent)?;
            agents.push(agent);
            selectors.extend(mailbox.issued_work_refs(session.thread_id, Some(agent), 100)
                .await.map_err(mailbox_error)?.into_iter().map(|work| Selector::Work(work.id)));
        }
    }
    selectors.extend(replies_to.iter().cloned().map(Selector::Reply));
    let mut seen = HashSet::new();
    selectors.retain(|selector| seen.insert(selector_key(selector)));
    let return_when = match args.return_when {
        MailboxWaitReturnWhen::Any => ReturnWhen::Any,
        MailboxWaitReturnWhen::All => ReturnWhen::All,
    };
    let deadline = args.timeout_ms.map(|timeout| {
        Instant::now().checked_add(Duration::from_millis(timeout))
            .ok_or_else(|| mailbox_error("timeout_ms exceeds the monotonic clock range"))
    }).transpose()?;
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
    report.pending = selectors.iter().map(|selector| MailboxWaitSelectionItem {
        work_ref: match selector { Selector::Work(id) => Some(id.clone()), Selector::Reply(_) => None },
        message_id: match selector { Selector::Reply(id) => Some(id.clone()), Selector::Work(_) => None },
        agent: None,
        status: "pending".to_string(),
    }).collect();
    let mut selected = Vec::new();
    let mut attention_to_observe = None;
    let mut started = false;
    let mut last_report = None;
    let mut registration = None;
    loop {
        drop(registration.take());
        let input_generation = mailbox.wait_input_generation();
        if targetless {
            match mailbox.issued_work_refs(session.thread_id, None, 100).await {
                Ok(work) => selectors = work.into_iter().map(|work| Selector::Work(work.id)).collect(),
                Err(_) => {
                    report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
                    break;
                }
            }
        }
        selected = if selectors.is_empty() {
            Vec::new()
        } else {
            match mailbox.snapshot(&Selection {
                caller: session.thread_id,
                agent: None,
                selectors: selectors.clone(),
                return_when,
                limit: 100,
            }).await {
                Ok(snapshot) => snapshot.items,
                Err(_) => {
                    report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
                    break;
                }
            }
        };
        for item in &selected {
            if let SnapshotItem::Work(work) = item {
                reject_reverse_completion_join(&session, &turn, work.work_ref.agent)?;
                if !agents.contains(&work.work_ref.agent) {
                    agents.push(work.work_ref.agent);
                }
            }
        }
        let mut recovery_changed = false;
        let mut recovery_fault = false;
        for item in &selected {
            if let SnapshotItem::Work(work) = item
                && work.dispatch_state == DispatchState::Dispatched
                && work.outcome.is_none()
            {
                match session.services.agent_control.mailbox_reconcile_pending_work(work).await {
                    Ok(changed) => recovery_changed |= changed,
                    Err(_) => { recovery_fault = true; break; }
                }
            }
        }
        if recovery_fault {
            report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
            break;
        }
        if recovery_changed {
            continue;
        }
        update_report(&session, &mut report, &selected);
        match mailbox.quiet_count(session.thread_id).await {
            Ok(count) => report.quiet_count = count,
            Err(_) => {
                report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
                break;
            }
        }
        let ready = report.completed.len();
        let early_attention = selected.iter().any(needs_attention);
        let satisfied = !selected.is_empty() && match return_when {
            ReturnWhen::Any => ready > 0,
            ReturnWhen::All => ready == selectors.len(),
        };
        let all_observed = !selected.is_empty() && selected.iter().all(already_observed);
        if !started {
            emit_report(&session, &turn, &call_id, &agents, &report, CollabAgentToolCallStatus::InProgress).await?;
            started = true;
            last_report = Some(report.clone());
        } else if last_report.as_ref() != Some(&report) {
            emit_report(&session, &turn, &call_id, &agents, &report, CollabAgentToolCallStatus::InProgress).await?;
            last_report = Some(report.clone());
        }
        if mailbox.has_runtime_fault(&agents) {
            report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
            break;
        }
        if !targetless && selected.len() != selectors.len() {
            report.completion_reason = Some(MailboxWakeCause::NoMatchingWork);
            break;
        }
        if !targetless && selectors.is_empty() {
            report.completion_reason = Some(MailboxWakeCause::NoMatchingWork);
            break;
        }
        if early_attention {
            report.completion_reason = Some(MailboxWakeCause::NeedsAttention);
            break;
        }
        if all_observed {
            report.condition_satisfied = true;
            report.completion_reason = Some(MailboxWakeCause::AlreadySatisfied);
            break;
        }
        if satisfied {
            report.condition_satisfied = true;
            report.completion_reason = Some(if all_observed {
                MailboxWakeCause::AlreadySatisfied
            } else if selected.iter().any(|item| matches!(item, SnapshotItem::Request(request) if request.reply.is_some())) {
                MailboxWakeCause::Reply
            } else {
                MailboxWakeCause::Result
            });
            break;
        }
        // An addressed request/alert interrupts a join but never satisfies it. Its body is
        // admitted before its one-shot notification is observed; unanswered obligation remains.
        let attention = match mailbox.attention(session.thread_id, 100).await {
            Ok(attention) => attention,
            Err(_) => {
                report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
                break;
            }
        };
        for attention in attention {
            let Some(message_id) = attention.id.strip_prefix("attention:amsg_").map(|suffix| format!("amsg_{suffix}")) else { continue; };
            if let Some(envelope) = mailbox.get_message(session.thread_id, &message_id).await.map_err(mailbox_error)? {
                if deliver_envelope(&session, &turn, &envelope).await.is_err() {
                    report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
                    break;
                }
                report.wake_sender = Some(envelope.communication.author.to_string());
                report.wake_ref = Some(envelope.id);
                report.completion_reason = Some(MailboxWakeCause::Request);
                attention_to_observe = Some(attention.id);
                break;
            }
        }
        if report.completion_reason.is_some() {
            break;
        }
        if let Some(activity) = session.input_queue.pending_wait_input_activity(&session.active_turn).await {
            match activity {
                InputQueueActivity::Steer => {
                    report.completion_reason = Some(MailboxWakeCause::Human);
                    break;
                }
                InputQueueActivity::RuntimeSystemEvent | InputQueueActivity::Mailbox => {
                    report.completion_reason = Some(MailboxWakeCause::Tool);
                    break;
                }
                InputQueueActivity::TerminalCompletion => {}
            }
        }
        if cancellation_token.is_cancelled() {
            report.completion_reason = Some(MailboxWakeCause::Cancel);
            break;
        }
        if deadline.is_some_and(|at| Instant::now() >= at) {
            report.completion_reason = Some(MailboxWakeCause::Deadline);
            break;
        }
        let peers = selected.iter().filter_map(|item| match item {
            SnapshotItem::Work(work) if work.outcome.is_none() && !work.observed => Some(work.work_ref.agent),
            SnapshotItem::Request(request) if request.reply.is_none() && !request.observed => Some(request.request.recipient),
            SnapshotItem::Work(_) | SnapshotItem::Request(_) => None,
        }).collect::<HashSet<_>>().into_iter().collect();
        let wait_id = format!("{}:{call_id}", session.thread_id);
        match mailbox.begin_wait_at(wait_id, session.thread_id, peers, return_when, deadline.is_some(), input_generation) {
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
        let deadline_wait = async {
            match deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => {
                report.completion_reason = Some(MailboxWakeCause::Cancel);
                break;
            }
            _ = deadline_wait => {
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
            }
        }
    }
    drop(registration);
    // Every accepted body is at most 8KiB. Bound each wait result to two bodies,
    // including an interrupting attention message. Metadata is non-consuming;
    // omitted bodies retain their per-item observation state and retrieval IDs.
    let body_limit = if attention_to_observe.is_some() { 1 } else { 2 };
    if admit_selected(&session, &turn, &mailbox, &selected, body_limit).await.is_err() {
        report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
        report.condition_satisfied = false;
    }
    if let Some(id) = attention_to_observe {
        if mailbox.acknowledge_observed(session.thread_id, &[ObservationTarget::Attention(id)])
            .await.is_err()
        {
            report.completion_reason = Some(MailboxWakeCause::RuntimeFault);
            report.condition_satisfied = false;
        }
    }
    emit_report(&session, &turn, &call_id, &agents, &report, CollabAgentToolCallStatus::Completed).await?;
    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        tool_output_json_text(&report, "wait_agent"), Some(true),
    )))
}

fn selector_key(selector: &Selector) -> String {
    match selector {
        Selector::Work(id) => format!("work:{id}"),
        Selector::Reply(id) => format!("reply:{id}"),
    }
}

fn reject_reverse_completion_join(session: &Session, turn: &TurnContext, agent: ThreadId) -> Result<(), FunctionCallError> {
    let current = turn.session_source.get_agent_path().unwrap_or_else(AgentPath::root);
    let target = session.services.agent_control.get_agent_metadata(agent).and_then(|metadata| metadata.agent_path);
    if agent == session.thread_id || target.as_ref().is_some_and(|target| target == &current
        || current.as_str().strip_prefix(target.as_str()).is_some_and(|suffix| suffix.starts_with('/'))) {
        return Err(mailbox_error("self/ancestor completion joins are not allowed; explicit parent-reply waits remain valid"));
    }
    Ok(())
}

fn needs_attention(item: &SnapshotItem) -> bool {
    match item {
        SnapshotItem::Work(work) => work.dispatch_state == DispatchState::RecoveryNeeded
            || work.outcome.as_ref().is_some_and(|result| result.outcome != Outcome::Completed)
            || work.continuations.iter().any(|continuation| continuation.outcome == Outcome::NeedsAttention),
        SnapshotItem::Request(request) => matches!(request.disposition, RequestDisposition::Superseded | RequestDisposition::Withdrawn),
    }
}

fn already_observed(item: &SnapshotItem) -> bool {
    match item {
        SnapshotItem::Work(work) => work.observed,
        SnapshotItem::Request(request) => request.observed,
    }
}

fn update_report(session: &Session, report: &mut MailboxWaitReport, selected: &[SnapshotItem]) {
    report.completed.clear();
    report.pending.clear();
    for item in selected {
        let (metadata, ready) = match item {
            SnapshotItem::Work(work) => {
                let status = if work.dispatch_state == DispatchState::RecoveryNeeded {
                    "recovery_needed".to_string()
                } else if work.continuations.iter().any(|continuation| continuation.outcome == Outcome::NeedsAttention) {
                    "needs_attention".to_string()
                } else {
                    work.outcome.as_ref().map_or_else(|| "pending".to_string(), |outcome| match outcome.outcome {
                        Outcome::Completed => "completed",
                        Outcome::Failed => "failed",
                        Outcome::Cancelled => "cancelled",
                        Outcome::NeedsAttention => "needs_attention",
                    }.to_string())
                };
                (MailboxWaitSelectionItem {
                    work_ref: Some(work.work_ref.id.clone()),
                    message_id: work.outcome.as_ref().map(|outcome| outcome.id.clone()),
                    agent: session.services.agent_control.get_agent_metadata(work.work_ref.agent)
                        .and_then(|metadata| metadata.agent_path).map(|path| path.to_string()),
                    status,
                }, work.outcome.is_some() || work.observed)
            }
            SnapshotItem::Request(request) => (MailboxWaitSelectionItem {
                work_ref: None,
                message_id: Some(request.request_id.clone()),
                agent: Some(request.request.communication.recipient.to_string()),
                status: match request.disposition {
                    RequestDisposition::Open => "pending".to_string(),
                    RequestDisposition::Replied => "replied".to_string(),
                    RequestDisposition::Superseded => format!("superseded:{}", request.replacement.as_deref().unwrap_or("unknown")),
                    RequestDisposition::Withdrawn => "withdrawn".to_string(),
                },
            }, request.reply.is_some() || request.observed),
        };
        if ready { report.completed.push(metadata); } else { report.pending.push(metadata); }
    }
}

async fn admit_selected(session: &Session, turn: &TurnContext, mailbox: &Mailbox, selected: &[SnapshotItem], body_limit: usize) -> Result<(), FunctionCallError> {
    let mut observations = Vec::new();
    let mut delivered = HashSet::new();
    for item in selected {
        match item {
            SnapshotItem::Work(work) if !work.observed => {
                if let Some(outcome) = &work.outcome {
                    if !delivered.contains(&outcome.id) && delivered.len() >= body_limit {
                        continue;
                    }
                    let envelope = mailbox.get_message(session.thread_id, &outcome.id).await
                        .map_err(mailbox_error)?.ok_or_else(|| mailbox_error("terminal result body requires compatible recovery"))?;
                    // Session owns append + successful flush + exact canonical acknowledgement
                    // under one custody gate, including synthetic typed terminal envelopes.
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
        mailbox.acknowledge_observed(session.thread_id, &observations).await.map_err(mailbox_error)?;
    }
    Ok(())
}

async fn emit_report(session: &Session, turn: &TurnContext, call_id: &str, agents: &[ThreadId], report: &MailboxWaitReport, status: CollabAgentToolCallStatus) -> Result<(), FunctionCallError> {
    let item = TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
        id: call_id.to_string(), tool: CollabAgentTool::Wait, status,
        sender_thread_id: session.thread_id, receiver_thread_ids: agents.to_vec(), receiver_agents: Vec::new(),
        prompt: None, model: None, reasoning_effort: None, requested_model: None, requested_reasoning_effort: None,
        agents_states: Default::default(), wake_notifications: None, completion_reason: None, wake_cause: None,
        mailbox_wait_report: Some(report.clone()),
    });
    session
        .emit_mailbox_wait_report(turn, item)
        .await
        .map_err(mailbox_error)
}
