use crate::{
    is_provably_closed_peer_cycle, ActivationDecision, ActivationDisposition, AdmissionReceipt,
    Continuation, ContinuationState, Dependency, DependencyMode, DispatchDecision, DispatchState,
    Envelope, GoalBinding, Mailbox, MailboxError, MessageDraft, MessageKind, NativeWork,
    ObservationTarget, Outcome, RequestDisposition, RequestSnapshot, ReturnWhen, Selection,
    Selector, Snapshot, SnapshotItem, SnapshotState, TurnOutcomeReceipt, WorkRef, WorkSnapshot,
};
use codex_protocol::{
    protocol::InterAgentCommunication, AgentPath, ThreadId,
};
use codex_state::SqliteConfig;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

struct TestMailbox {
    _home: TempDir,
    config: SqliteConfig,
    mailbox: Mailbox,
}

async fn test_mailbox() -> TestMailbox {
    let home = tempfile::tempdir().expect("temporary mailbox home");
    let config = SqliteConfig::new_for_testing(home.path().abs());
    let mailbox = Mailbox::open(&config).await.expect("open mailbox");
    TestMailbox { _home: home, config, mailbox }
}

fn thread_id(index: u64) -> ThreadId {
    ThreadId::from_string(&format!("00000000-0000-0000-0000-{index:012x}"))
        .expect("fixture thread ID is a UUID")
}

fn communication(content: &str) -> InterAgentCommunication {
    InterAgentCommunication::new(
        AgentPath::root(), AgentPath::root(), Vec::new(), content.to_string(), false,
    )
}

fn native_work(issuer: ThreadId, agent: ThreadId, key: &str, content: &str) -> NativeWork {
    NativeWork {
        native_key: key.to_string(), issuer, agent,
        submission_id: format!("submission-{key}"), input: communication(content),
    }
}

fn draft(
    key: &str, sender: ThreadId, recipient: ThreadId, kind: MessageKind, content: &str,
    legacy_attention: bool,
) -> MessageDraft {
    MessageDraft {
        invocation_key: key.to_string(), sender, recipient,
        communication: communication(content), kind, activate_if_idle: false,
        work_ref: None, reply_to: None, supersedes: None, legacy_attention,
    }
}

fn selection(
    caller: ThreadId,
    agent: Option<ThreadId>,
    selectors: Vec<Selector>,
    when: ReturnWhen,
) -> Selection {
    Selection { caller, agent, selectors, return_when: when, limit: 100 }
}

async fn take_snapshot(
    mailbox: &Mailbox,
    caller: ThreadId,
    agent: Option<ThreadId>,
    selectors: Vec<Selector>,
    when: ReturnWhen,
) -> Snapshot {
    mailbox.snapshot(&selection(caller, agent, selectors, when)).await.unwrap()
}

fn work_item<'a>(snapshot: &'a Snapshot, id: &str) -> &'a WorkSnapshot {
    snapshot.items.iter().find_map(|item| match item {
        SnapshotItem::Work(work) if work.work_ref.id == id => Some(work), _ => None,
    }).expect("selected work is in snapshot")
}

fn request_item<'a>(snapshot: &'a Snapshot, id: &str) -> &'a RequestSnapshot {
    snapshot.items.iter().find_map(|item| match item {
        SnapshotItem::Request(request) if request.request_id == id => Some(request), _ => None,
    }).expect("selected request is in snapshot")
}

async fn accept_work(
    mailbox: &Mailbox,
    issuer: ThreadId,
    agent: ThreadId,
    key: &str,
    content: &str,
) -> (WorkRef, String) {
    let work = mailbox
        .accept_work(native_work(issuer, agent, key, content))
        .await
        .expect("accept work");
    let input_id = mailbox
        .accepted_input(&work)
        .await
        .expect("read accepted input")
        .id.expect("accepted input has provider ID").to_string();
    (work, input_id)
}

async fn admit(mailbox: &Mailbox, agent: ThreadId, turn: &str, ids: &[String]) -> AdmissionReceipt {
    mailbox.record_admission(agent, turn, ids).await.expect("record admission")
}

async fn finish(
    mailbox: &Mailbox, agent: ThreadId, turn: &str, outcome: Outcome,
    response: Option<InterAgentCommunication>, continuation: Continuation,
) -> TurnOutcomeReceipt {
    mailbox.record_turn_outcome(agent, turn, outcome, response, continuation).await
        .expect("record turn outcome")
}

async fn send(mailbox: &Mailbox, message: MessageDraft) -> Envelope {
    mailbox.accept_message(message).await.expect("accept message")
}

fn goal_binding(
    parent: ThreadId,
    child: ThreadId,
    generation: u64,
    source_turn: &str,
) -> GoalBinding {
    GoalBinding {
        goal_id: "goal-1".to_string(), parent_thread_id: parent, child_thread_id: child,
        control_generation: generation, revision: 1, source_turn_id: source_turn.to_string(),
    }
}

fn dependency(owner: &str, peers: &[&str], mode: DependencyMode) -> Dependency {
    Dependency {
        owner: owner.to_string(),
        peers: peers.iter().map(|peer| (*peer).to_string()).collect(),
        mode,
    }
}

#[tokio::test]
async fn native_retry_dispatch_reopen_and_capture_backpressure_are_safe() {
    let fixture = test_mailbox().await;
    let (issuer, agent) = (thread_id(1), thread_id(2));
    let original = native_work(issuer, agent, "native-1", "do the work");
    let work = fixture.mailbox.accept_work(original.clone()).await.unwrap();
    let captured = fixture.mailbox.accepted_input(&work).await.unwrap();
    assert!(captured.id.as_ref().unwrap().as_str().starts_with("amsg_"));
    assert_eq!(fixture.mailbox.accept_work(original).await.unwrap(), work);
    assert_eq!(fixture.mailbox.accepted_input(&work).await.unwrap(), captured);

    let mut conflict = native_work(issuer, agent, "native-1", "changed input");
    conflict.input.trigger_turn = true;
    assert!(matches!(
        fixture.mailbox.accept_work(conflict).await,
        Err(MailboxError::IdempotencyConflict)
    ));
    assert_eq!(fixture.mailbox.begin_dispatch(&work).await.unwrap(), DispatchDecision::Dispatch);
    assert_eq!(
        fixture.mailbox.begin_dispatch(&work).await.unwrap(),
        DispatchDecision::RecoveryNeeded
    );

    let reopened = Mailbox::open(&fixture.config).await.unwrap();
    assert_eq!(reopened.dispatch_state(&work).await.unwrap(), DispatchState::Dispatching);
    assert_eq!(reopened.begin_dispatch(&work).await.unwrap(), DispatchDecision::RecoveryNeeded);
    reopened.mark_dispatched(&work).await.unwrap();
    assert_eq!(reopened.begin_dispatch(&work).await.unwrap(), DispatchDecision::AlreadyDispatched);

    let mut too_large = native_work(issuer, agent, "too-large", "x");
    too_large.input.content = "x".repeat(1_000_001);
    assert!(matches!(
        fixture.mailbox.accept_work(too_large).await,
        Err(MailboxError::Backpressure(_))
    ));
    assert!(fixture.mailbox.work_for_native_key(issuer, "too-large").await.unwrap().is_none());
}

#[tokio::test]
async fn native_accept_retry_after_pre_dispatch_handle_drop_reuses_original_identity() {
    let TestMailbox { _home, config, mailbox } = test_mailbox().await;
    let (issuer, agent) = (thread_id(22), thread_id(23));
    let original = native_work(issuer, agent, "pre-dispatch-drop", "preserve this accepted input");
    let accepted = mailbox.accept_work(original.clone()).await.unwrap();
    let accepted_input_id = mailbox
        .accepted_input(&accepted)
        .await
        .unwrap()
        .id
        .unwrap()
        .to_string();
    assert!(accepted_input_id.starts_with("amsg_"));

    // The durable accept has happened, but the caller drops the handle before dispatch begins.
    drop(mailbox);
    let retried_mailbox = Mailbox::open(&config).await.unwrap();
    let retried = retried_mailbox.accept_work(original).await.unwrap();
    assert_eq!(retried, accepted);
    assert_eq!(
        retried_mailbox.work_for_native_key(issuer, "pre-dispatch-drop").await.unwrap(),
        Some(accepted.clone())
    );
    assert_eq!(
        retried_mailbox
            .accepted_input(&retried)
            .await
            .unwrap()
            .id
            .unwrap()
            .to_string(),
        accepted_input_id
    );

    assert_eq!(
        retried_mailbox.begin_dispatch(&retried).await.unwrap(),
        DispatchDecision::Dispatch
    );
    assert_eq!(
        retried_mailbox.begin_dispatch(&retried).await.unwrap(),
        DispatchDecision::RecoveryNeeded
    );
    retried_mailbox.mark_dispatched(&retried).await.unwrap();
    assert_eq!(
        retried_mailbox.begin_dispatch(&retried).await.unwrap(),
        DispatchDecision::AlreadyDispatched
    );
}

#[tokio::test]
async fn admission_is_incremental_but_outcomes_and_snapshots_stay_exactly_scoped() {
    let fixture = test_mailbox().await;
    let (issuer, other_issuer, agent) = (thread_id(3), thread_id(4), thread_id(5));
    let (a, id_a) = accept_work(&fixture.mailbox, issuer, agent, "a", "A").await;
    let (b, id_b) = accept_work(&fixture.mailbox, issuer, agent, "b", "B").await;
    let (c, _id_c) = accept_work(&fixture.mailbox, issuer, agent, "c", "C").await;
    assert_eq!(
        admit(&fixture.mailbox, agent, "turn", &[id_a.clone()]).await.work_refs,
        vec![a.clone()]
    );
    assert!(admit(&fixture.mailbox, agent, "turn", &[id_a.clone()]).await.already_recorded);
    assert_eq!(
        admit(&fixture.mailbox, agent, "turn", &[id_b.clone()]).await.work_refs,
        vec![b.clone()]
    );
    assert!(matches!(fixture.mailbox.record_admission(agent, "other-turn", &[id_a.clone()]).await,
        Err(MailboxError::IdempotencyConflict)));

    // New work after admission remains outside the outcome; unrelated issuer history is hidden.
    let (late, _) = accept_work(&fixture.mailbox, issuer, agent, "late", "late").await;
    let (foreign, _) = accept_work(
        &fixture.mailbox, other_issuer, agent, "foreign", "foreign",
    ).await;
    fixture.mailbox.record_outcome(
        &foreign, Outcome::Completed, None, Continuation::None,
    ).await.unwrap();
    let outcomes = finish(
        &fixture.mailbox, agent, "turn", Outcome::Completed, None, Continuation::None,
    ).await;
    assert_eq!(outcomes.outcomes.len(), 2);
    assert!(outcomes.outcomes.iter().any(|item| item.work_ref == a));
    assert!(outcomes.outcomes.iter().any(|item| item.work_ref == b));
    assert!(outcomes.outcomes.iter().all(|item| item.work_ref != c && item.work_ref != late));

    let snapshot = take_snapshot(&fixture.mailbox, issuer, Some(agent), vec![
        Selector::Work(a.id.clone()), Selector::Work(b.id.clone()), Selector::Work(c.id.clone()),
        Selector::Work(late.id.clone()), Selector::Work(foreign.id.clone()),
    ], ReturnWhen::All).await;
    assert_eq!(snapshot.state, SnapshotState::Pending);
    assert_eq!(snapshot.items.len(), 4);
    assert!(work_item(&snapshot, &a.id).outcome.is_some());
    assert!(work_item(&snapshot, &b.id).outcome.is_some());
    assert!(work_item(&snapshot, &c.id).outcome.is_none());
    assert!(work_item(&snapshot, &late.id).outcome.is_none());
    assert!(!snapshot.items.iter().any(|item| matches!(
        item, SnapshotItem::Work(work) if work.work_ref.id == foreign.id
    )));

    let too_many = (0..101).map(|i| format!("amsg-limit-{i}")).collect::<Vec<_>>();
    assert!(matches!(fixture.mailbox.record_admission(agent, "over-limit", &too_many).await,
        Err(MailboxError::Backpressure(_))));
}

#[tokio::test]
async fn mixed_issuer_turn_creates_distinct_handbacks() {
    let fixture = test_mailbox().await;
    let (issuer_a, issuer_b, agent) = (thread_id(6), thread_id(7), thread_id(8));
    let (a, id_a) = accept_work(&fixture.mailbox, issuer_a, agent, "multi-a", "A").await;
    let (b, id_b) = accept_work(&fixture.mailbox, issuer_b, agent, "multi-b", "B").await;
    admit(&fixture.mailbox, agent, "shared-turn", &[id_a, id_b]).await;
    let receipt = finish(&fixture.mailbox, agent, "shared-turn", Outcome::Completed,
        Some(communication("one result")), Continuation::None).await;
    assert_eq!(receipt.outcomes.len(), 2);
    let id_a = receipt.outcomes.iter().find(|r| r.work_ref == a)
        .and_then(|r| r.response_message_id.clone()).unwrap();
    let id_b = receipt.outcomes.iter().find(|r| r.work_ref == b)
        .and_then(|r| r.response_message_id.clone()).unwrap();
    assert_ne!(id_a, id_b);
    for (issuer, id) in [(issuer_a, id_a), (issuer_b, id_b)] {
        let message = fixture.mailbox.get_message(issuer, &id).await.unwrap().unwrap();
        assert_eq!((message.sender, message.recipient), (agent, issuer));
    }
}

#[tokio::test]
async fn pending_deferred_and_terminal_continuations_bind_to_exact_generation() {
    let fixture = test_mailbox().await;
    let (issuer, agent) = (thread_id(9), thread_id(10));
    let (a, id_a) = accept_work(&fixture.mailbox, issuer, agent, "pending", "A").await;
    let (b, id_b) = accept_work(&fixture.mailbox, issuer, agent, "deferred", "B").await;
    let bindings = [
        goal_binding(issuer, agent, 4, "turn-p"),
        goal_binding(issuer, agent, 5, "turn-d"),
    ];
    for (work, id, turn, continuation, state) in [
        (
            &a, id_a, "turn-p", Continuation::PendingGoalBinding(bindings[0].clone()),
            ContinuationState::PendingGoalBinding,
        ),
        (
            &b, id_b, "turn-d", Continuation::Deferred(bindings[1].clone()),
            ContinuationState::Deferred,
        ),
    ] {
        admit(&fixture.mailbox, agent, turn, &[id]).await;
        let receipt = finish(
            &fixture.mailbox, agent, turn, Outcome::Completed, None, continuation,
        ).await;
        assert!(receipt.outcomes.is_empty());
        assert_eq!(receipt.continuation_work, vec![(*work).clone()]);
        let snapshot = take_snapshot(
            &fixture.mailbox, issuer, Some(agent), vec![Selector::Work(work.id.clone())],
            ReturnWhen::Any,
        ).await;
        let item = work_item(&snapshot, &work.id);
        assert!(item.outcome.is_none());
        assert_eq!(item.continuations[0].state, state);
    }
    let mut stale = bindings[0].clone();
    stale.control_generation += 1;
    assert!(finish(&fixture.mailbox, agent, "stale-generation", Outcome::Completed, None,
        Continuation::Terminal(stale)).await.no_matching_work);
    let still_deferred = take_snapshot(
        &fixture.mailbox, issuer, Some(agent), vec![Selector::Work(a.id.clone())],
        ReturnWhen::Any,
    ).await;
    assert!(work_item(&still_deferred, &a.id).outcome.is_none());

    for (work, binding, turn) in [
        (&a, bindings[0].clone(), "terminal-p"),
        (&b, bindings[1].clone(), "terminal-d"),
    ] {
        let result = finish(&fixture.mailbox, agent, turn, Outcome::Completed, None,
            Continuation::Terminal(binding.clone())).await;
        assert_eq!(result.outcomes.len(), 1);
        let snapshot = take_snapshot(
            &fixture.mailbox, issuer, Some(agent), vec![Selector::Work(work.id.clone())],
            ReturnWhen::Any,
        ).await;
        assert_eq!(work_item(&snapshot, &work.id).outcome.as_ref().unwrap().continuation,
            Continuation::Terminal(binding));
    }
}

#[tokio::test]
async fn quiet_reply_and_author_dispositions_settle_only_the_exact_request() {
    let fixture = test_mailbox().await;
    let (author, recipient, stranger) = (thread_id(11), thread_id(12), thread_id(13));
    let request = send(
        &fixture.mailbox,
        draft("request", author, recipient, MessageKind::Request, "decide", false),
    ).await;
    let (_, input_id) = accept_work(
        &fixture.mailbox, author, recipient, "unrelated", "other task",
    ).await;
    admit(&fixture.mailbox, recipient, "unrelated-turn", &[input_id]).await;
    finish(
        &fixture.mailbox, recipient, "unrelated-turn", Outcome::Completed, None,
        Continuation::None,
    ).await;
    let open = take_snapshot(
        &fixture.mailbox, author, Some(recipient), vec![Selector::Reply(request.id.clone())],
        ReturnWhen::Any,
    ).await;
    assert_eq!(open.state, SnapshotState::Pending);
    assert_eq!(request_item(&open, &request.id).disposition, RequestDisposition::Open);
    fixture.mailbox.mark_canonical(recipient, &request.id).await.unwrap();
    let mut reply_draft = draft("reply", recipient, author, MessageKind::Note, "done", false);
    reply_draft.reply_to = Some(request.id.clone());
    let reply = send(&fixture.mailbox, reply_draft).await;
    let replied = take_snapshot(
        &fixture.mailbox, author, Some(recipient), vec![Selector::Reply(request.id.clone())],
        ReturnWhen::Any,
    ).await;
    assert_eq!(replied.state, SnapshotState::AlreadySatisfied);
    assert_eq!(request_item(&replied, &request.id).disposition, RequestDisposition::Replied);
    assert_eq!(request_item(&replied, &request.id).reply.as_ref().unwrap().id, reply.id);

    let open_request = send(
        &fixture.mailbox,
        draft("replace-me", author, recipient, MessageKind::Request, "old", false),
    ).await;
    let mut unauthorized = draft(
        "stranger-replace", stranger, recipient, MessageKind::Request, "no", false,
    );
    unauthorized.supersedes = Some(open_request.id.clone());
    assert!(matches!(
        fixture.mailbox.accept_message(unauthorized).await,
        Err(MailboxError::WrongRecipient)
    ));
    let mut replacement_draft = draft(
        "replacement", author, recipient, MessageKind::Request, "new", false,
    );
    replacement_draft.supersedes = Some(open_request.id.clone());
    let replacement = send(&fixture.mailbox, replacement_draft).await;
    let superseded = take_snapshot(
        &fixture.mailbox, author, None, vec![Selector::Reply(open_request.id.clone())],
        ReturnWhen::Any,
    ).await;
    assert_eq!(superseded.state, SnapshotState::AlreadySatisfied);
    let old = request_item(&superseded, &open_request.id);
    assert_eq!(old.disposition, RequestDisposition::Superseded);
    assert_eq!(old.replacement.as_deref(), Some(replacement.id.as_str()));

    let withdrawal = send(
        &fixture.mailbox,
        draft("withdraw-me", author, recipient, MessageKind::Request, "cancel", false),
    ).await;
    assert!(matches!(fixture.mailbox.withdraw_message(stranger, &withdrawal.id).await,
        Err(MailboxError::WrongRecipient)));
    fixture.mailbox.withdraw_message(author, &withdrawal.id).await.unwrap();
    let withdrawn = take_snapshot(
        &fixture.mailbox, author, None, vec![Selector::Reply(withdrawal.id.clone())],
        ReturnWhen::Any,
    ).await;
    assert_eq!(withdrawn.state, SnapshotState::AlreadySatisfied);
    assert_eq!(request_item(&withdrawn, &withdrawal.id).disposition, RequestDisposition::Withdrawn);
}

#[tokio::test]
async fn progress_coalesces_exact_unadmitted_scope_and_closes_at_terminal_outcome() {
    let fixture = test_mailbox().await;
    let (author, agent) = (thread_id(14), thread_id(15));
    let (work_a, _) = accept_work(&fixture.mailbox, author, agent, "work-a", "A").await;
    let (work_b, _) = accept_work(&fixture.mailbox, author, agent, "work-b", "B").await;
    let mut first_draft = draft("p1", author, agent, MessageKind::Progress, "first", false);
    first_draft.work_ref = Some(work_a.clone());
    let first = send(&fixture.mailbox, first_draft).await;
    let mut update_draft = draft("p2", author, agent, MessageKind::Progress, "latest", false);
    update_draft.work_ref = Some(work_a.clone());
    let update = send(&fixture.mailbox, update_draft).await;
    assert_ne!(update.id, first.id);
    assert_eq!(update.supersedes.as_deref(), Some(first.id.as_str()));
    let original = fixture.mailbox.get_message(agent, &first.id).await.unwrap().unwrap();
    assert_eq!(original.communication.content, "first");
    let latest = fixture.mailbox.get_message(agent, &update.id).await.unwrap().unwrap();
    assert_eq!(latest.communication.content, "latest");
    assert_eq!(latest.supersedes.as_deref(), Some(first.id.as_str()));
    let pending = fixture.mailbox.pending(agent, 20).await.unwrap();
    assert!(pending.iter().any(|envelope| envelope.id == update.id));
    assert!(!pending.iter().any(|envelope| envelope.id == first.id));

    let mut other_author = draft("p3", author, agent, MessageKind::Progress, "other path", false);
    other_author.work_ref = Some(work_a.clone());
    other_author.communication.author = AgentPath::from_string("/root/child".to_string()).unwrap();
    assert_ne!(send(&fixture.mailbox, other_author).await.id, first.id);
    let mut other_path = draft("p-path", author, agent, MessageKind::Progress, "other path", false);
    other_path.work_ref = Some(work_a.clone());
    other_path.communication.recipient = AgentPath::from_string("/root/other".to_string()).unwrap();
    assert_ne!(send(&fixture.mailbox, other_path).await.id, first.id);
    let mut other_work = draft("p4", author, agent, MessageKind::Progress, "work B", false);
    other_work.work_ref = Some(work_b);
    assert_ne!(send(&fixture.mailbox, other_work).await.id, first.id);

    fixture.mailbox.record_outcome(
        &work_a, Outcome::Completed, None, Continuation::None,
    ).await.unwrap();
    let mut late = draft("p-late", author, agent, MessageKind::Progress, "late", false);
    late.work_ref = Some(work_a);
    assert!(matches!(
        fixture.mailbox.accept_message(late).await,
        Err(MailboxError::OutcomeAlreadyRecorded)
    ));
}

#[tokio::test]
async fn canonical_outcomes_are_observed_per_item_without_cursor_loss() {
    let fixture = test_mailbox().await;
    let (issuer, agent) = (thread_id(16), thread_id(17));
    let (a, id_a) = accept_work(&fixture.mailbox, issuer, agent, "observe-a", "A").await;
    let (b, id_b) = accept_work(&fixture.mailbox, issuer, agent, "observe-b", "B").await;
    for (work, id, turn) in [(&a, id_a, "turn-a"), (&b, id_b, "turn-b")] {
        admit(&fixture.mailbox, agent, turn, &[id]).await;
        let outcome = finish(
            &fixture.mailbox, agent, turn, Outcome::Completed, None, Continuation::None,
        ).await;
        fixture.mailbox.mark_outcome_canonical(issuer, &outcome.outcomes[0].id).await.unwrap();
        assert_eq!(&outcome.outcomes[0].work_ref, work);
    }
    fixture.mailbox.acknowledge_observed(
        issuer, &[ObservationTarget::Work(b.clone())],
    ).await.unwrap();
    let selection = selection(
        issuer, Some(agent),
        vec![Selector::Work(a.id.clone()), Selector::Work(b.id.clone())],
        ReturnWhen::All,
    );
    let partial = fixture.mailbox.snapshot(&selection).await.unwrap();
    assert_eq!(partial.state, SnapshotState::AlreadySatisfied);
    assert!(!work_item(&partial, &a.id).observed);
    assert!(work_item(&partial, &b.id).observed);
    assert!(
        work_item(&partial, &a.id).outcome.is_some()
            && work_item(&partial, &b.id).outcome.is_some()
    );
    fixture.mailbox.acknowledge_observed(issuer, &[ObservationTarget::Work(a)])
        .await.unwrap();
    assert_eq!(
        fixture.mailbox.snapshot(&selection).await.unwrap().state,
        SnapshotState::AlreadySatisfied
    );
}

#[tokio::test]
async fn message_kinds_default_to_quiet_delivery_with_legacy_attention_separate() {
    let fixture = test_mailbox().await;
    let (sender, recipient) = (thread_id(18), thread_id(19));
    let kinds = [
        MessageKind::Note, MessageKind::Progress, MessageKind::Request, MessageKind::Alert,
    ];
    assert_eq!(MessageKind::default(), MessageKind::Note);
    for kind in kinds {
        send(
            &fixture.mailbox,
            draft(&format!("quiet-{kind:?}"), sender, recipient, kind, "quiet", false),
        ).await;
        send(
            &fixture.mailbox,
            draft(&format!("legacy-{kind:?}"), sender, recipient, kind, "legacy", true),
        ).await;
    }
    let pending = fixture.mailbox.pending(recipient, 20).await.unwrap();
    assert_eq!(pending.len(), 8);
    for kind in kinds { assert_eq!(pending.iter().filter(|m| m.kind == kind).count(), 2); }
    assert_eq!(pending.iter().filter(|m| m.legacy_attention).count(), 1);
    assert_eq!(fixture.mailbox.quiet_count(recipient).await.unwrap(), 3);
    assert_eq!(fixture.mailbox.attention(recipient, 20).await.unwrap().len(), 5);
}

#[tokio::test]
async fn explicit_attention_is_one_shot_after_canonical_read_without_settling_request() {
    let fixture = test_mailbox().await;
    let (sender, recipient) = (thread_id(24), thread_id(25));
    let request = send(
        &fixture.mailbox,
        draft("attention-request", sender, recipient, MessageKind::Request, "please decide", false),
    )
    .await;
    let alert = send(
        &fixture.mailbox,
        draft("attention-alert", sender, recipient, MessageKind::Alert, "needs attention", false),
    )
    .await;
    let note = send(
        &fixture.mailbox,
        draft("quiet-note", sender, recipient, MessageKind::Note, "quiet note", false),
    )
    .await;
    let progress = send(
        &fixture.mailbox,
        draft("quiet-progress", sender, recipient, MessageKind::Progress, "quiet progress", false),
    )
    .await;

    let legacy_draft = draft(
        "legacy-attention-once", sender, recipient, MessageKind::Note, "legacy wake", true,
    );
    let legacy = send(&fixture.mailbox, legacy_draft.clone()).await;
    assert_eq!(send(&fixture.mailbox, legacy_draft).await.id, legacy.id);

    let uncanonical = fixture.mailbox.pending_page(recipient, None, 20).await.unwrap();
    let uncanonical_ids = uncanonical.items.iter().map(|item| item.id.as_str()).collect::<Vec<_>>();
    for message in [&request, &alert, &note, &progress, &legacy] {
        assert!(uncanonical_ids.contains(&message.id.as_str()));
        fixture.mailbox.mark_canonical(recipient, &message.id).await.unwrap();
    }
    let canonical = fixture.mailbox.read_page(recipient, None, 20).await.unwrap();
    assert!(canonical.items.iter().any(|message| message.id == request.id));
    assert!(canonical.items.iter().any(|message| message.id == alert.id));

    let mut attention_ids = fixture
        .mailbox
        .attention(recipient, 20)
        .await
        .unwrap()
        .into_iter()
        .map(|attention| attention.id)
        .collect::<Vec<_>>();
    attention_ids.sort();
    let mut expected_attention_ids = vec![
        format!("attention:{}", request.id),
        format!("attention:{}", alert.id),
        format!("attention:{}", legacy.id),
    ];
    expected_attention_ids.sort();
    assert_eq!(attention_ids, expected_attention_ids);

    let observations = attention_ids
        .iter()
        .cloned()
        .map(ObservationTarget::Attention)
        .collect::<Vec<_>>();
    fixture
        .mailbox
        .acknowledge_observed(recipient, &observations)
        .await
        .unwrap();
    assert!(fixture.mailbox.attention(recipient, 20).await.unwrap().is_empty());

    let old_request = take_snapshot(
        &fixture.mailbox,
        sender,
        Some(recipient),
        vec![Selector::Reply(request.id.clone())],
        ReturnWhen::Any,
    )
    .await;
    assert_eq!(old_request.state, SnapshotState::Pending);
    assert_eq!(request_item(&old_request, &request.id).disposition, RequestDisposition::Open);

    let new_request = send(
        &fixture.mailbox,
        draft(
            "new-attention-request",
            sender,
            recipient,
            MessageKind::Request,
            "new request",
            false,
        ),
    )
    .await;
    fixture.mailbox.mark_canonical(recipient, &new_request.id).await.unwrap();
    let new_attention = fixture.mailbox.attention(recipient, 20).await.unwrap();
    let new_attention_ids = new_attention
        .iter()
        .map(|attention| attention.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        new_attention_ids,
        vec![format!("attention:{}", new_request.id)]
    );
    fixture
        .mailbox
        .acknowledge_observed(
            recipient,
            &[ObservationTarget::Attention(format!("attention:{}", new_request.id))],
        )
        .await
        .unwrap();
    assert!(fixture.mailbox.attention(recipient, 20).await.unwrap().is_empty());
}

#[tokio::test]
async fn activation_claim_is_single_use_across_a_lost_result() {
    let fixture = test_mailbox().await;
    let (sender, recipient) = (thread_id(20), thread_id(21));
    let mut message_draft = draft(
        "activation", sender, recipient, MessageKind::Alert, "activate", false,
    );
    message_draft.activate_if_idle = true;
    let message = send(&fixture.mailbox, message_draft).await;
    assert_eq!(
        fixture.mailbox.begin_activation(sender, &message.id).await.unwrap(),
        ActivationDecision::Activate);
    assert_eq!(
        fixture.mailbox.begin_activation(sender, &message.id).await.unwrap(),
        ActivationDecision::RecoveryNeeded);
    for _ in 0..2 {
        fixture.mailbox.record_activation(sender, &message.id, ActivationDisposition::Activated)
            .await.unwrap();
    }
    assert_eq!(
        fixture.mailbox.begin_activation(sender, &message.id).await.unwrap(),
        ActivationDecision::AlreadyDecided(ActivationDisposition::Activated));
    assert!(matches!(
        fixture.mailbox.record_activation(sender, &message.id, ActivationDisposition::Denied).await,
        Err(MailboxError::IdempotencyConflict)
    ));
}

#[test]
fn only_closed_any_and_all_peer_cycles_are_provable() {
    let closed = [
        dependency("a", &["b", "c"], DependencyMode::Any),
        dependency("b", &["a"], DependencyMode::All),
        dependency("c", &["a"], DependencyMode::All),
        dependency("x", &["y", "z"], DependencyMode::All),
        dependency("y", &["x"], DependencyMode::Any),
        dependency("z", &["x"], DependencyMode::Any),
    ];
    assert!(is_provably_closed_peer_cycle("a", &closed));
    assert!(is_provably_closed_peer_cycle("x", &closed));
    let open = [
        dependency("a", &["b", "outside"], DependencyMode::Any),
        dependency("b", &["a"], DependencyMode::All),
    ];
    assert!(!is_provably_closed_peer_cycle("a", &open));
    let closed_all = [
        dependency("x", &["y", "outside"], DependencyMode::All),
        dependency("y", &["x"], DependencyMode::Any),
    ];
    assert!(is_provably_closed_peer_cycle("x", &closed_all));
    let open_all = [
        dependency("x", &["y"], DependencyMode::All),
        dependency("y", &["x", "outside"], DependencyMode::Any),
    ];
    assert!(!is_provably_closed_peer_cycle("x", &open_all));
}

#[tokio::test]
async fn wait_cycle_rejects_only_the_new_registration_and_drop_is_exact() {
    let fixture = test_mailbox().await;
    let (owner_a, owner_b) = (thread_id(22), thread_id(23));
    let wait_a = fixture.mailbox.begin_wait(
        "wait-a".to_string(), owner_a, vec![owner_b], ReturnWhen::Any, false,
    ).unwrap();
    assert_eq!(wait_a.wait_id(), "wait-a");
    assert!(matches!(fixture.mailbox.begin_wait(
        "closed-b".to_string(), owner_b, vec![owner_a], ReturnWhen::All, false,
    ), Err(MailboxError::ClosedPeerCycle)));
    assert!(matches!(fixture.mailbox.begin_wait(
        "still-closed-b".to_string(), owner_b, vec![owner_a], ReturnWhen::All, false,
    ), Err(MailboxError::ClosedPeerCycle)));
    let escape = fixture.mailbox.begin_wait(
        "escape-b".to_string(), owner_b, vec![owner_a], ReturnWhen::All, true,
    ).unwrap();
    drop(escape);
    assert!(matches!(fixture.mailbox.begin_wait(
        "closed-again-b".to_string(), owner_b, vec![owner_a], ReturnWhen::All, false,
    ), Err(MailboxError::ClosedPeerCycle)));
    drop(wait_a);
    assert!(fixture.mailbox.begin_wait(
        "open-b".to_string(), owner_b, vec![owner_a], ReturnWhen::All, false,
    ).is_ok());
}

#[tokio::test]
async fn input_publication_invalidates_stale_waits_and_defers_cycle_claims_until_rescan() {
    let fixture = test_mailbox().await;
    let (a, b) = (thread_id(28), thread_id(29));
    let before = fixture.mailbox.wait_input_generation();
    let old = fixture.mailbox.begin_wait_at(
        "a".into(), a, vec![b], ReturnWhen::Any, false, before,
    ).unwrap();
    let arrival = fixture.mailbox.begin_input_arrival();
    assert!(matches!(fixture.mailbox.begin_wait_at(
        "stale".into(), b, vec![a], ReturnWhen::All, false, before,
    ), Err(MailboxError::StaleWaitSnapshot)));
    let during = fixture.mailbox.wait_input_generation();
    let provisional_a = fixture.mailbox.begin_wait_at(
        "a".into(), a, vec![b], ReturnWhen::Any, false, during,
    ).unwrap();
    // A stale RAII handle must not remove a replacement registration.
    drop(old);
    let provisional_b = fixture.mailbox.begin_wait_at(
        "b".into(), b, vec![a], ReturnWhen::All, false, during,
    ).unwrap();
    drop(arrival);
    assert!(matches!(fixture.mailbox.begin_wait_at(
        "needs-rescan".into(), b, vec![a], ReturnWhen::All, false, during,
    ), Err(MailboxError::StaleWaitSnapshot)));
    drop(provisional_a);
    drop(provisional_b);
    let current = fixture.mailbox.wait_input_generation();
    let _waiting_a = fixture.mailbox.begin_wait_at(
        "fresh-a".into(), a, vec![b], ReturnWhen::Any, false, current,
    ).unwrap();
    assert!(matches!(fixture.mailbox.begin_wait_at(
        "fresh-b".into(), b, vec![a], ReturnWhen::All, false, current,
    ), Err(MailboxError::ClosedPeerCycle)));
}

#[tokio::test]
async fn canonical_result_body_resolves_only_its_exact_covered_work_and_attention() {
    let fixture = test_mailbox().await;
    let (issuer, agent) = (thread_id(30), thread_id(31));
    let (covered, input) = accept_work(&fixture.mailbox, issuer, agent, "covered", "A").await;
    let (unrelated, _) = accept_work(&fixture.mailbox, issuer, agent, "unrelated", "B").await;
    admit(&fixture.mailbox, agent, "covered-turn", &[input]).await;
    let terminal = finish(&fixture.mailbox, agent, "covered-turn", Outcome::NeedsAttention, None, Continuation::None).await;
    let outcome = &terminal.outcomes[0];
    let envelope = fixture.mailbox.get_message(issuer, &outcome.id).await.unwrap().unwrap();
    assert_eq!(envelope.communication.id.as_ref().unwrap().as_str(), outcome.id);
    assert!(!fixture.mailbox.attention(issuer, 100).await.unwrap().is_empty());
    fixture.mailbox.mark_canonical(issuer, &outcome.id).await.unwrap();
    let exact = fixture.mailbox.work_for_outcome(issuer, &outcome.id).await.unwrap();
    assert_eq!(exact, vec![covered.clone()]);
    fixture.mailbox.acknowledge_observed(issuer, &[ObservationTarget::Work(covered.clone())]).await.unwrap();
    assert!(fixture.mailbox.attention(issuer, 100).await.unwrap().is_empty());
    let snapshot = take_snapshot(&fixture.mailbox, issuer, Some(agent), vec![Selector::Work(covered.id.clone()), Selector::Work(unrelated.id.clone())], ReturnWhen::All).await;
    assert!(work_item(&snapshot, &covered.id).observed);
    assert!(!work_item(&snapshot, &unrelated.id).observed);
}
