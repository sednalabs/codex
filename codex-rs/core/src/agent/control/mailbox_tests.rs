use super::*;
use crate::agent::goal_notifications::GoalNotificationBinding;
use crate::agent::goal_notifications::GoalNotificationClassification;
use codex_agent_mailbox::DispatchState;
use codex_agent_mailbox::ReturnWhen;
use codex_agent_mailbox::Selection;
use codex_agent_mailbox::Selector;
use codex_agent_mailbox::SnapshotItem;
use codex_protocol::AgentPath;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn mailbox_shared_control_does_not_capture_stock_invocations() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.features.enable(Feature::AgentMailbox).unwrap();
    let control = &session.services.agent_control;
    control.mailbox_initialize(&config).await.unwrap();
    let recipient = ThreadId::new();
    let mut communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::from_string("/root/worker".to_string()).unwrap(),
        Vec::new(),
        "native input".to_string(),
        /*trigger_turn*/ true,
    );
    let original = communication.clone();
    let mut submission_id = "stock-submission".to_string();
    let context =
        AgentCommunicationContext::new(AgentCommunicationKind::Followup, session.thread_id);
    assert!(
        control
            .mailbox_accept_native_submission(
                recipient,
                &context,
                &mut communication,
                &mut submission_id
            )
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(communication, original);
    assert_eq!(submission_id, "stock-submission");
    let mailbox = control.mailbox().unwrap();
    assert!(
        mailbox
            .issued_work_refs(session.thread_id, None, 100)
            .await
            .unwrap()
            .is_empty()
    );

    let enabled_context = context.with_native_key("enabled-call".to_string());
    let (accepted, decision) = control
        .mailbox_accept_native_submission(
            recipient,
            &enabled_context,
            &mut communication,
            &mut submission_id,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(decision, DispatchDecision::Dispatch);
    assert_eq!(accepted.issuer, session.thread_id);
    assert_eq!(
        mailbox.accepted_input(&accepted).await.unwrap(),
        communication
    );
    assert!(communication.id.unwrap().as_str().starts_with("amsg_"));
}

fn goal_snapshot(issuer: ThreadId, agent: ThreadId) -> GoalNotificationSnapshot {
    GoalNotificationSnapshot {
        binding: GoalNotificationBinding {
            parent_thread_id: issuer,
            child_thread_id: agent,
            goal_id: "native-goal".to_string(),
            control_generation: 7,
        },
        status: ThreadGoalStatus::Active,
        phase: GoalNotificationPhase::ContinuationPending,
        revision: 1,
        last_completed_source_turn: Some("accepted-turn".to_string()),
        result_reference: None,
        classification: GoalNotificationClassification::Progress,
    }
}

async fn accepted_goal_work(mailbox: &Mailbox, issuer: ThreadId, agent: ThreadId) -> WorkRef {
    let work = mailbox.accept_work(NativeWork {
        native_key: "goal-call".to_string(),
        issuer,
        agent,
        submission_id: "goal-submission".to_string(),
        input: InterAgentCommunication::new(
            AgentPath::root(),
            AgentPath::from_string("/root/worker".to_string()).unwrap(),
            Vec::new(),
            "complete the accepted work".to_string(),
            /*trigger_turn*/ true,
        ),
    }).await.unwrap();
    assert_eq!(mailbox.begin_dispatch(&work).await.unwrap(), DispatchDecision::Dispatch);
    mailbox.mark_dispatched(&work).await.unwrap();
    let input = mailbox.accepted_input(&work).await.unwrap();
    mailbox.record_admission(agent, "accepted-turn", &[input.id.unwrap().to_string()]).await.unwrap();
    work
}

async fn exact_work_snapshot(mailbox: &Mailbox, work: &WorkRef) -> WorkSnapshot {
    let snapshot = mailbox.snapshot(&Selection {
        caller: work.issuer,
        agent: Some(work.agent),
        selectors: vec![Selector::Work(work.id.clone())],
        return_when: ReturnWhen::All,
        limit: 1,
    }).await.unwrap();
    match snapshot.items.into_iter().next().unwrap() {
        SnapshotItem::Work(work) => work,
        SnapshotItem::Request(_) => panic!("exact work selector returned a request"),
    }
}

#[tokio::test]
async fn mailbox_goal_terminal_matches_logical_binding_across_native_revisions_and_turns() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.features.enable(Feature::AgentMailbox).unwrap();
    let control = &session.services.agent_control;
    control.mailbox_initialize(&config).await.unwrap();
    let mailbox = control.mailbox().unwrap();
    let agent = ThreadId::new();
    let incarnation = ThreadId::new();
    let work = accepted_goal_work(&mailbox, session.thread_id, agent).await;
    let mut goal = goal_snapshot(session.thread_id, agent);

    assert!(control.mailbox_record_terminal(agent, "accepted-turn", AgentStatus::Completed(None), Some((incarnation, goal.clone())), None).await.unwrap());
    goal.phase = GoalNotificationPhase::DeferredWithOwner;
    goal.revision += 1;
    assert!(control.mailbox_record_terminal(agent, "accepted-turn", AgentStatus::Completed(None), Some((incarnation, goal.clone())), None).await.unwrap());
    let pending = exact_work_snapshot(&mailbox, &work).await;
    assert!(pending.outcome.is_none());
    assert_eq!(pending.continuations.len(), 1);
    assert_eq!(pending.continuations[0].source_turn_id, "accepted-turn");
    assert_eq!(pending.continuations[0].state, ContinuationState::Deferred);

    goal.phase = GoalNotificationPhase::ResultReady;
    goal.status = ThreadGoalStatus::Complete;
    goal.revision += 1;
    goal.last_completed_source_turn = Some("continuation-turn".to_string());
    goal.classification = GoalNotificationClassification::Result;
    // A restarted/replaced native store is not evidence for the old producer,
    // even when its goal ID, generation and thread IDs happen to be identical.
    assert!(!control.mailbox_record_terminal(agent, "continuation-turn", AgentStatus::Completed(None), Some((ThreadId::new(), goal.clone())), None).await.unwrap());
    assert!(exact_work_snapshot(&mailbox, &work).await.outcome.is_none());
    assert!(control.mailbox_record_terminal(agent, "continuation-turn", AgentStatus::Completed(None), Some((incarnation, goal)), None).await.unwrap());
    let completed = exact_work_snapshot(&mailbox, &work).await;
    assert_eq!(completed.outcome.unwrap().outcome, Outcome::Completed);
    assert_eq!(completed.continuations[0].state, ContinuationState::Terminal);
    // An unrelated native result must retain the existing native fallback path.
    assert!(!control.mailbox_record_terminal(ThreadId::new(), "uncaptured-turn", AgentStatus::Completed(None), None, None).await.unwrap());
}

#[tokio::test]
async fn mailbox_cold_goal_continuation_returns_recovery_needed_without_losing_custody() {
    let (mut session, turn) = crate::session::tests::make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.features.enable(Feature::AgentMailbox).unwrap();
    let manager = crate::ThreadManager::with_models_provider_for_tests(
        codex_login::CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
    );
    session.services.agent_control = manager.agent_control();
    let control = &session.services.agent_control;
    control.mailbox_initialize(&config).await.unwrap();
    let mailbox = control.mailbox().unwrap();
    let agent = ThreadId::new();
    let incarnation = ThreadId::new();
    let work = accepted_goal_work(&mailbox, session.thread_id, agent).await;
    let mut goal = goal_snapshot(session.thread_id, agent);
    goal.phase = GoalNotificationPhase::DeferredWithOwner;
    assert!(control.mailbox_record_terminal(agent, "accepted-turn", AgentStatus::Completed(None), Some((incarnation, goal.clone())), None).await.unwrap());
    let pending = exact_work_snapshot(&mailbox, &work).await;

    // This exact producer has no loaded native thread or continuation owner.
    assert!(control.mailbox_reconcile_pending_work(&pending).await.unwrap());
    let recovered = exact_work_snapshot(&mailbox, &work).await;
    assert_eq!(recovered.work_ref, work);
    assert_eq!(recovered.dispatch_state, DispatchState::RecoveryNeeded);
    assert!(recovered.outcome.is_none());
    assert_eq!(recovered.input, pending.input);
    assert_eq!(recovered.continuations, pending.continuations);
    assert!(!control.mailbox_reconcile_pending_work(&recovered).await.unwrap());

    goal.phase = GoalNotificationPhase::ResultReady;
    goal.status = ThreadGoalStatus::Complete;
    goal.revision += 1;
    goal.classification = GoalNotificationClassification::Result;
    assert!(control.mailbox_record_terminal(agent, "recovered-turn", AgentStatus::Completed(None), Some((incarnation, goal)), None).await.unwrap());
    assert_eq!(exact_work_snapshot(&mailbox, &work).await.outcome.unwrap().outcome, Outcome::Completed);
}

#[tokio::test]
async fn mailbox_cold_deferred_handback_uses_loaded_parents_shared_store() {
    let (_session, turn) = crate::session::tests::make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.features.enable(Feature::AgentMailbox).unwrap();
    let manager = crate::ThreadManager::with_models_provider_for_tests(
        codex_login::CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
    );
    let parent = manager.start_thread(crate::thread_manager::StartThreadOptions::new(config.clone())).await.unwrap();
    let control = &parent.thread.session.services.agent_control;
    control.mailbox_initialize(&config).await.unwrap();
    let mailbox = control.mailbox().unwrap();
    let agent = ThreadId::new();
    let incarnation = ThreadId::new();
    let work = accepted_goal_work(&mailbox, parent.thread_id, agent).await;
    let mut goal = goal_snapshot(parent.thread_id, agent);
    assert!(control.mailbox_record_terminal(agent, "accepted-turn", AgentStatus::Completed(None), Some((incarnation, goal.clone())), None).await.unwrap());
    assert!(manager.get_thread(agent).await.is_err());
    assert!(!parent.thread.session.input_queue.has_pending_mailbox_items().await);

    goal.phase = GoalNotificationPhase::ResultReady;
    goal.status = ThreadGoalStatus::Complete;
    goal.revision += 1;
    goal.classification = GoalNotificationClassification::Result;
    assert!(manager.notify_v2_child_completion(
        agent,
        parent.thread_id,
        AgentPath::from_string("/root/worker".to_string()).unwrap(),
        "accepted-turn",
        AgentStatus::Completed(Some("durable goal result".to_string())),
        Some((incarnation, goal)),
    ).await.unwrap());
    let settled = exact_work_snapshot(&mailbox, &work).await;
    assert_eq!(settled.work_ref, work);
    assert_eq!(settled.outcome.unwrap().outcome, Outcome::Completed);
    assert_eq!(settled.continuations[0].state, ContinuationState::Terminal);
    assert!(manager.get_thread(agent).await.is_err());
    assert!(!parent.thread.session.input_queue.has_pending_mailbox_items().await);
}
