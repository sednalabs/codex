use super::*;
use crate::session::step_context::StepContext;
use crate::tools::context::ToolCallSource;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_agent_mailbox::Continuation;
use codex_agent_mailbox::MessageDraft;
use codex_agent_mailbox::MessageKind;
use codex_agent_mailbox::NativeWork;
use codex_agent_mailbox::WorkRef;
use codex_features::Feature;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::RolloutItem;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[test]
fn mailbox_wait_selectors_bound_utf8_count_and_combined_content() {
    let at_limit = "x".repeat(MAX_MAILBOX_WAIT_SELECTOR_BYTES);
    assert!(
        validate_wait_selectors(&vec![at_limit.clone(); 8], &vec![at_limit.clone(); 8]).is_ok()
    );
    assert!(validate_wait_selectors(&vec![at_limit.clone(); 8], &vec![at_limit; 9]).is_err());
    assert!(
        validate_wait_selectors(&vec![String::new(); MAX_MAILBOX_WAIT_SELECTORS + 1], &[]).is_err()
    );
    assert!(
        validate_wait_selectors(&["x".repeat(MAX_MAILBOX_WAIT_SELECTOR_BYTES + 1)], &[]).is_err()
    );
    assert!(
        validate_wait_selectors(&[], &["é".repeat(MAX_MAILBOX_WAIT_SELECTOR_BYTES / 2 + 1)]).is_err()
    );
}

#[test]
fn mailbox_wait_report_bounds_escaped_selector_echoes() {
    let mut report = MailboxWaitReport {
        requested_targets: Vec::new(),
        requested_replies_to: Vec::new(),
        return_when: MailboxWaitReturnWhen::Any,
        completed: Vec::new(),
        pending: Vec::new(),
        completion_reason: None,
        condition_satisfied: false,
        quiet_count: 0,
        wake_sender: None,
        wake_ref: None,
    };
    assert!(validate_wait_report(&report).is_ok());
    report.requested_targets = vec!["\0".repeat(MAX_MAILBOX_WAIT_SELECTOR_BYTES); 16];
    assert!(validate_wait_selectors(&report.requested_targets, &[]).is_ok());
    assert!(validate_wait_report(&report).is_err());
}

async fn fixture_with_path() -> (
    Arc<Session>,
    Arc<TurnContext>,
    Arc<Mailbox>,
    ThreadId,
    PathBuf,
) {
    let (mut session, mut turn) = crate::session::tests::make_session_and_context().await;
    let rollout_path = crate::session::tests::attach_thread_persistence(&mut session).await;
    let mut config = (*turn.config).clone();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.features.enable(Feature::AgentMailbox).unwrap();
    turn.multi_agent_version = config.multi_agent_version_from_features();
    turn.config = Arc::new(config);
    session
        .services
        .agent_control
        .mailbox_initialize(&turn.config)
        .await
        .unwrap();
    let mailbox = session.services.agent_control.mailbox().unwrap();
    (
        Arc::new(session),
        Arc::new(turn),
        mailbox,
        ThreadId::new(),
        rollout_path,
    )
}

async fn fixture() -> (Arc<Session>, Arc<TurnContext>, Arc<Mailbox>, ThreadId) {
    let (session, turn, mailbox, agent, _rollout_path) = fixture_with_path().await;
    (session, turn, mailbox, agent)
}

async fn durable_fixture() -> (
    Arc<Session>,
    Arc<TurnContext>,
    Arc<Mailbox>,
    ThreadId,
    PathBuf,
) {
    fixture_with_path().await
}

async fn work(
    mailbox: &Mailbox,
    issuer: ThreadId,
    agent: ThreadId,
    key: &str,
    outcome: Option<Outcome>,
) -> WorkRef {
    let work = mailbox
        .accept_work(NativeWork {
            native_key: key.to_string(),
            issuer,
            agent,
            submission_id: key.to_string(),
            input: InterAgentCommunication::new(
                AgentPath::root(),
                AgentPath::from_string("/root/worker".to_string()).unwrap(),
                Vec::new(),
                key.to_string(),
                /*trigger_turn*/ true,
            ),
        })
        .await
        .unwrap();
    if let Some(outcome) = outcome {
        let result = mailbox
            .record_outcome(&work, outcome, None, Continuation::None)
            .await
            .unwrap();
        // This fixture represents a previously surfaced result. Writer custody has
        // separate Session tests; this exercises replay through the actual wait facade.
        mailbox.mark_canonical(issuer, &result.id).await.unwrap();
        mailbox
            .acknowledge_observed(issuer, &[ObservationTarget::Work(work.clone())])
            .await
            .unwrap();
    }
    work
}

async fn wait(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    targets: Vec<String>,
    mode: &str,
) -> MailboxWaitReport {
    wait_with_call_id(
        session,
        turn,
        serde_json::json!({ "targets": targets, "return_when": mode, "timeout_ms": 0 }),
        format!("replay-{mode}"),
    )
    .await
    .0
}

async fn wait_with_call_id(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    arguments: serde_json::Value,
    call_id: String,
) -> (MailboxWaitReport, Option<bool>) {
    let payload = ToolPayload::Function {
        arguments: arguments.to_string(),
    };
    let output = handle_wait(ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
        call_id,
        tool_name: ToolName::plain("wait_agent"),
        source: ToolCallSource::Direct,
        payload: payload.clone(),
    })
    .await
    .unwrap();
    let ResponseInputItem::FunctionCallOutput { output, .. } =
        output.to_response_item("replay", &payload)
    else {
        panic!("expected a function result")
    };
    let success = output.success;
    let FunctionCallOutputBody::Text(body) = output.body else {
        panic!("expected report text")
    };
    (serde_json::from_str(&body).unwrap(), success)
}

async fn record_result(
    mailbox: &Mailbox,
    work_ref: &WorkRef,
    author: AgentPath,
    body: &str,
) -> String {
    mailbox
        .record_outcome(
            work_ref,
            Outcome::Completed,
            Some(InterAgentCommunication::new(
                author,
                AgentPath::root(),
                Vec::new(),
                body.to_string(),
                /*trigger_turn*/ false,
            )),
            Continuation::None,
        )
        .await
        .unwrap()
        .id
}

async fn assert_work_observed(mailbox: &Mailbox, recipient: ThreadId, work_ref: &WorkRef) {
    let snapshot = mailbox
        .snapshot(&Selection {
            caller: recipient,
            agent: None,
            selectors: vec![Selector::Work(work_ref.id.clone())],
            return_when: ReturnWhen::All,
            limit: 100,
        })
        .await
        .unwrap();
    let [SnapshotItem::Work(work)] = snapshot.items.as_slice() else {
        panic!("expected exact work snapshot")
    };
    assert!(work.observed);
}

async fn assert_persisted_message(rollout_path: &std::path::Path, id: &str, body: &str) {
    let InitialHistory::Resumed(resumed) =
        crate::rollout::recorder::RolloutRecorder::get_rollout_history(rollout_path)
            .await
            .unwrap()
    else {
        panic!("expected durable resumed rollout history")
    };
    assert!(resumed.history.iter().any(|item| match item {
        RolloutItem::ResponseItem(response) => {
            response
                .id()
                .is_some_and(|response_id| response_id.as_str() == id)
                && serde_json::to_value(response)
                    .unwrap()
                    .to_string()
                    .contains(body)
        }
        _ => false,
    }));
}

async fn seed_interrupted_wait_receipt(
    session: &Session,
    call_id: &str,
    report: MailboxWaitReport,
) {
    // This exact call/output/terminal-event trio is the canonical history shape consumed by
    // resume recovery; refresh the same derived receipt cache used by that path.
    let terminal_item = TurnItem::CollabAgentToolCall(CollabAgentToolCallItem {
        id: call_id.to_string(),
        tool: CollabAgentTool::Wait,
        status: CollabAgentToolCallStatus::Failed,
        sender_thread_id: session.thread_id,
        receiver_thread_ids: Vec::new(),
        receiver_agents: Vec::new(),
        prompt: None,
        model: None,
        reasoning_effort: None,
        requested_model: None,
        requested_reasoning_effort: None,
        agents_states: Default::default(),
        wake_notifications: None,
        completion_reason: None,
        wake_cause: None,
        mailbox_wait_report: Some(report.clone()),
    });
    let items = [
        RolloutItem::ResponseItem(ResponseItem::FunctionCall {
            id: None,
            name: "wait_agent".to_string(),
            namespace: None,
            arguments: serde_json::json!({}).to_string(),
            call_id: call_id.to_string(),
            internal_chat_message_metadata_passthrough: None,
        }),
        RolloutItem::ResponseItem(ResponseItem::FunctionCallOutput {
            id: None,
            call_id: call_id.to_string(),
            internal_chat_message_metadata_passthrough: None,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(serde_json::to_string(&report).unwrap()),
                success: Some(false),
            },
        }),
        RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
            thread_id: session.thread_id,
            turn_id: "interrupted-wait-test".to_string(),
            item: terminal_item,
            completed_at_ms: 1,
        })),
    ];
    session
        .refresh_mailbox_interrupted_wait_receipts(&items)
        .await;
}

#[tokio::test]
async fn observed_unsuccessful_work_remains_non_success_in_exact_and_mixed_replays() {
    for outcome in [Outcome::Failed, Outcome::Cancelled, Outcome::NeedsAttention] {
        let (session, turn, mailbox, agent) = fixture().await;
        let failed = work(
            &mailbox,
            session.thread_id,
            agent,
            "unsuccessful",
            Some(outcome),
        )
        .await;
        let pending = work(&mailbox, session.thread_id, agent, "pending", None).await;
        for mode in ["any", "all"] {
            for targets in [
                vec![failed.id.clone()],
                vec![failed.id.clone(), pending.id.clone()],
            ] {
                let report = wait(
                    Arc::clone(&session),
                    Arc::clone(&turn),
                    targets.clone(),
                    mode,
                )
                .await;
                assert_eq!(
                    report.completion_reason,
                    Some(MailboxWakeCause::NeedsAttention)
                );
                assert!(!report.condition_satisfied);
                assert_eq!(report.pending.len(), targets.len() - 1);
            }
        }
    }
}

#[tokio::test]
async fn unrelated_attention_does_not_erase_a_successful_selected_join() {
    let (session, turn, mailbox, agent) = fixture().await;
    let successful = work(
        &mailbox,
        session.thread_id,
        agent,
        "successful",
        Some(Outcome::Completed),
    )
    .await;
    let attention = mailbox
        .accept_message(MessageDraft {
            invocation_key: "unrelated-request".to_string(),
            sender: agent,
            recipient: session.thread_id,
            communication: InterAgentCommunication::new(
                AgentPath::from_string("/root/worker".to_string()).unwrap(),
                AgentPath::root(),
                Vec::new(),
                "independent request".to_string(),
                /*trigger_turn*/ false,
            ),
            kind: MessageKind::Request,
            activate_if_idle: false,
            work_ref: None,
            reply_to: None,
            supersedes: None,
            legacy_attention: false,
        })
        .await
        .unwrap();
    for mode in ["any", "all"] {
        let report = wait(
            Arc::clone(&session),
            Arc::clone(&turn),
            vec![successful.id.clone()],
            mode,
        )
        .await;
        assert_eq!(
            report.completion_reason,
            Some(MailboxWakeCause::AlreadySatisfied)
        );
        assert!(report.condition_satisfied);
        let pending_attention = mailbox.attention(session.thread_id, 100).await.unwrap();
        assert_eq!(pending_attention.len(), 1);
        assert_eq!(
            pending_attention[0].id,
            format!("attention:{}", attention.id)
        );
    }
}

#[tokio::test]
async fn wait_delivers_unobserved_result_before_acknowledging_observation() {
    let (session, turn, mailbox, agent, rollout_path) = durable_fixture().await;
    let work_ref = work(&mailbox, session.thread_id, agent, "deliver-result", None).await;
    let body = "durable result body";
    let result_id = record_result(
        &mailbox,
        &work_ref,
        AgentPath::from_string("/root/worker".to_string()).unwrap(),
        body,
    )
    .await;
    let before = mailbox
        .snapshot(&Selection {
            caller: session.thread_id,
            agent: None,
            selectors: vec![Selector::Work(work_ref.id.clone())],
            return_when: ReturnWhen::All,
            limit: 100,
        })
        .await
        .unwrap();
    let [SnapshotItem::Work(before)] = before.items.as_slice() else {
        panic!("expected exact unobserved work snapshot")
    };
    assert!(!before.observed);
    assert!(
        !mailbox
            .get_message(session.thread_id, &result_id)
            .await
            .unwrap()
            .unwrap()
            .canonical
    );

    let (report, success) = wait_with_call_id(
        Arc::clone(&session),
        Arc::clone(&turn),
        serde_json::json!({
            "targets": [work_ref.id.clone()],
            "return_when": "all",
            "timeout_ms": 0
        }),
        "deliver-unobserved-result".to_string(),
    )
    .await;

    assert_eq!(report.completion_reason, Some(MailboxWakeCause::Result));
    assert!(report.condition_satisfied);
    assert_eq!(success, Some(true));
    assert!(
        mailbox
            .get_message(session.thread_id, &result_id)
            .await
            .unwrap()
            .unwrap()
            .canonical
    );
    assert_work_observed(&mailbox, session.thread_id, &work_ref).await;
    assert_persisted_message(&rollout_path, &result_id, body).await;
}

#[tokio::test]
async fn wait_delivers_exact_quiet_reply_and_persists_it_before_observation() {
    let (session, turn, mailbox, agent, rollout_path) = durable_fixture().await;
    let worker_path = AgentPath::from_string("/root/worker".to_string()).unwrap();
    let request = mailbox
        .accept_message(MessageDraft {
            invocation_key: "quiet-reply-request".to_string(),
            sender: session.thread_id,
            recipient: agent,
            communication: InterAgentCommunication::new(
                AgentPath::root(),
                worker_path.clone(),
                Vec::new(),
                "request requiring reply".to_string(),
                /*trigger_turn*/ false,
            ),
            kind: MessageKind::Request,
            activate_if_idle: false,
            work_ref: None,
            reply_to: None,
            supersedes: None,
            legacy_attention: false,
        })
        .await
        .unwrap();
    // The replying agent must first have the request in canonical history; reply
    // admission is intentionally rejected until that exact request is canonical.
    mailbox.mark_canonical(agent, &request.id).await.unwrap();
    let body = "quiet exact reply body";
    let reply = mailbox
        .accept_message(MessageDraft {
            invocation_key: "quiet-reply-answer".to_string(),
            sender: agent,
            recipient: session.thread_id,
            communication: InterAgentCommunication::new(
                worker_path,
                AgentPath::root(),
                Vec::new(),
                body.to_string(),
                /*trigger_turn*/ false,
            ),
            kind: MessageKind::Note,
            activate_if_idle: false,
            work_ref: None,
            reply_to: Some(request.id.clone()),
            supersedes: None,
            legacy_attention: false,
        })
        .await
        .unwrap();
    assert!(!reply.canonical);
    assert_eq!(mailbox.quiet_count(session.thread_id).await.unwrap(), 1);
    assert!(
        mailbox
            .attention(session.thread_id, 100)
            .await
            .unwrap()
            .is_empty()
    );
    let before = mailbox
        .snapshot(&Selection {
            caller: session.thread_id,
            agent: None,
            selectors: vec![Selector::Reply(request.id.clone())],
            return_when: ReturnWhen::All,
            limit: 100,
        })
        .await
        .unwrap();
    let [SnapshotItem::Request(before)] = before.items.as_slice() else {
        panic!("expected exact unobserved request/reply snapshot")
    };
    assert!(!before.observed);

    let (report, success) = wait_with_call_id(
        Arc::clone(&session),
        Arc::clone(&turn),
        serde_json::json!({
            "replies_to": [request.id.clone()],
            "return_when": "all",
            "timeout_ms": 0
        }),
        "deliver-quiet-reply".to_string(),
    )
    .await;

    assert_eq!(report.completion_reason, Some(MailboxWakeCause::Reply));
    assert!(report.condition_satisfied);
    assert_eq!(report.quiet_count, 1);
    assert_eq!(success, Some(true));
    assert!(
        mailbox
            .attention(session.thread_id, 100)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        mailbox
            .get_message(session.thread_id, &reply.id)
            .await
            .unwrap()
            .unwrap()
            .canonical
    );
    let snapshot = mailbox
        .snapshot(&Selection {
            caller: session.thread_id,
            agent: None,
            selectors: vec![Selector::Reply(request.id)],
            return_when: ReturnWhen::All,
            limit: 100,
        })
        .await
        .unwrap();
    let [SnapshotItem::Request(request)] = snapshot.items.as_slice() else {
        panic!("expected exact request/reply snapshot")
    };
    assert!(request.observed);
    assert_eq!(
        request.reply.as_ref().map(|reply| reply.id.as_str()),
        Some(reply.id.as_str())
    );
    assert_persisted_message(&rollout_path, &reply.id, body).await;
}

#[tokio::test]
async fn wait_leaves_over_limit_bodies_retrievable_and_a_later_exact_wait_delivers_them() {
    let (session, turn, mailbox, agent, rollout_path) = durable_fixture().await;
    let worker_path = AgentPath::from_string("/root/worker".to_string()).unwrap();
    let mut work_refs = Vec::new();
    let mut result_ids = Vec::new();
    let mut bodies = Vec::new();
    for index in 0..3 {
        let work_ref = work(
            &mailbox,
            session.thread_id,
            agent,
            &format!("bounded-delivery-{index}"),
            None,
        )
        .await;
        let body = format!("bounded body {index}");
        let result_id = record_result(&mailbox, &work_ref, worker_path.clone(), &body).await;
        work_refs.push(work_ref);
        result_ids.push(result_id);
        bodies.push(body);
    }

    let (report, success) = wait_with_call_id(
        Arc::clone(&session),
        Arc::clone(&turn),
        serde_json::json!({
            "targets": work_refs.iter().map(|work| work.id.clone()).collect::<Vec<_>>(),
            "return_when": "all",
            "timeout_ms": 0
        }),
        "deliver-bounded-results".to_string(),
    )
    .await;
    assert_eq!(report.completion_reason, Some(MailboxWakeCause::Result));
    assert!(report.condition_satisfied);
    assert_eq!(report.completed.len(), 3);
    assert_eq!(success, Some(true));

    for index in 0..2 {
        assert!(
            mailbox
                .get_message(session.thread_id, &result_ids[index])
                .await
                .unwrap()
                .unwrap()
                .canonical
        );
        assert_work_observed(&mailbox, session.thread_id, &work_refs[index]).await;
        assert_persisted_message(&rollout_path, &result_ids[index], &bodies[index]).await;
    }
    let omitted = mailbox
        .get_message(session.thread_id, &result_ids[2])
        .await
        .unwrap()
        .unwrap();
    assert!(!omitted.canonical);
    assert_eq!(omitted.communication.content, bodies[2]);
    let snapshot = mailbox
        .snapshot(&Selection {
            caller: session.thread_id,
            agent: None,
            selectors: vec![Selector::Work(work_refs[2].id.clone())],
            return_when: ReturnWhen::All,
            limit: 100,
        })
        .await
        .unwrap();
    let [SnapshotItem::Work(work)] = snapshot.items.as_slice() else {
        panic!("expected omitted exact work snapshot")
    };
    assert!(!work.observed);

    let (later_report, later_success) = wait_with_call_id(
        Arc::clone(&session),
        Arc::clone(&turn),
        serde_json::json!({
            "targets": [work_refs[2].id.clone()],
            "return_when": "all",
            "timeout_ms": 0
        }),
        "deliver-omitted-result".to_string(),
    )
    .await;
    assert_eq!(
        later_report.completion_reason,
        Some(MailboxWakeCause::Result)
    );
    assert!(later_report.condition_satisfied);
    assert_eq!(later_success, Some(true));
    assert!(
        mailbox
            .get_message(session.thread_id, &result_ids[2])
            .await
            .unwrap()
            .unwrap()
            .canonical
    );
    assert_work_observed(&mailbox, session.thread_id, &work_refs[2]).await;
    assert_persisted_message(&rollout_path, &result_ids[2], &bodies[2]).await;
}

#[tokio::test]
async fn interrupted_wait_call_id_replays_failure_but_a_fresh_id_uses_the_wait_path() {
    let (session, turn, mailbox, agent, _rollout_path) = durable_fixture().await;
    let pending = work(
        &mailbox,
        session.thread_id,
        agent,
        "pending-after-interruption",
        None,
    )
    .await;
    let call_id = "interrupted-wait-call";
    let interrupted = MailboxWaitReport {
        requested_targets: vec![pending.id.clone()],
        requested_replies_to: Vec::new(),
        return_when: MailboxWaitReturnWhen::All,
        completed: Vec::new(),
        pending: vec![MailboxWaitSelectionItem {
            work_ref: Some(pending.id.clone()),
            message_id: None,
            agent: Some("/root/worker".to_string()),
            status: "running".to_string(),
        }],
        completion_reason: Some(MailboxWakeCause::Interrupted),
        condition_satisfied: false,
        quiet_count: 0,
        wake_sender: None,
        wake_ref: None,
    };
    seed_interrupted_wait_receipt(&session, call_id, interrupted.clone()).await;

    let (replayed, replay_success) = wait_with_call_id(
        Arc::clone(&session),
        Arc::clone(&turn),
        serde_json::json!({
            "targets": [pending.id.clone()],
            "return_when": "all",
            "timeout_ms": 0
        }),
        call_id.to_string(),
    )
    .await;
    assert_eq!(
        replayed.completion_reason,
        Some(MailboxWakeCause::Interrupted)
    );
    assert_eq!(replayed.pending, interrupted.pending);
    assert!(!replayed.condition_satisfied);
    assert_eq!(replay_success, Some(false));

    let (fresh, fresh_success) = wait_with_call_id(
        session,
        turn,
        serde_json::json!({
            "targets": [pending.id],
            "return_when": "all",
            "timeout_ms": 0
        }),
        "fresh-wait-call".to_string(),
    )
    .await;
    assert_eq!(fresh.completion_reason, Some(MailboxWakeCause::Deadline));
    assert!(!fresh.condition_satisfied);
    assert_eq!(fresh_success, Some(true));
}
