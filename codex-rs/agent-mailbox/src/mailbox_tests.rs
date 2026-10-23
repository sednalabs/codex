use crate::{
    is_provably_closed_peer_cycle, ActivationDecision, ActivationDisposition, Dependency,
    DependencyMode, DispatchDecision, DispatchState, Mailbox, MailboxError, MessageDraft,
    MessageKind, NativeWork, ObservationTarget, RequestDisposition, RequestSnapshot, ReturnWhen,
    Selection, Selector, Snapshot, SnapshotItem, SnapshotState, WorkSnapshot, Continuation,
    ContinuationState, GoalBinding, Outcome,
};
use codex_protocol::{
    protocol::{AgentPath, InterAgentCommunication},
    ThreadId,
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
    let mailbox = Mailbox::open(&config).await.expect("open test mailbox");
    TestMailbox {
        _home: home,
        config,
        mailbox,
    }
}

fn thread_id(index: u64) -> ThreadId {
    ThreadId::from_string(&format!("00000000-0000-0000-0000-{index:012x}"))
        .expect("fixture thread ID is a UUID")
}

fn communication(content: &str) -> InterAgentCommunication {
    InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root(),
        Vec::new(),
        content.to_string(),
        false,
    )
}

fn native_work(
    issuer: ThreadId,
    agent: ThreadId,
    native_key: &str,
    submission_id: &str,
    content: &str,
) -> NativeWork {
    NativeWork {
        native_key: native_key.to_string(),
        issuer,
        agent,
        submission_id: submission_id.to_string(),
        input: communication(content),
    }
}

fn message_draft(
    invocation_key: &str,
    sender: ThreadId,
    recipient: ThreadId,
    kind: MessageKind,
    content: &str,
    legacy_attention: bool,
) -> MessageDraft {
    MessageDraft {
        invocation_key: invocation_key.to_string(),
        sender,
        recipient,
        communication: communication(content),
        kind,
        activate_if_idle: false,
        work_ref: None,
        reply_to: None,
        supersedes: None,
        legacy_attention,
    }
}

fn selection(
    caller: ThreadId,
    agent: Option<ThreadId>,
    selectors: Vec<Selector>,
    return_when: ReturnWhen,
) -> Selection {
    Selection {
        caller,
        agent,
        selectors,
        return_when,
        limit: 100,
    }
}

fn work_item<'a>(snapshot: &'a Snapshot, work_id: &str) -> &'a WorkSnapshot {
    snapshot
        .items
        .iter()
        .find_map(|item| match item {
            SnapshotItem::Work(work) if work.work_ref.id == work_id => Some(work),
            _ => None,
        })
        .expect("selected work is present in the snapshot")
}

fn request_item<'a>(snapshot: &'a Snapshot, request_id: &str) -> &'a RequestSnapshot {
    snapshot
        .items
        .iter()
        .find_map(|item| match item {
            SnapshotItem::Request(request) if request.request_id == request_id => Some(request),
            _ => None,
        })
        .expect("selected request is present in the snapshot")
}

fn goal_binding(parent: ThreadId, child: ThreadId, generation: u64, turn: &str) -> GoalBinding {
    GoalBinding {
        goal_id: "goal-1".to_string(),
        parent_thread_id: parent,
        child_thread_id: child,
        control_generation: generation,
        revision: 1,
        source_turn_id: turn.to_string(),
    }
}

#[tokio::test]
async fn accepted_native_key_retry_preserves_work_and_provider_message_identity() {
    let fixture = test_mailbox().await;
    let issuer = thread_id(1);
    let agent = thread_id(2);
    let original = native_work(issuer, agent, "native-1", "submission-1", "do the work");

    let accepted = fixture
        .mailbox
        .accept_work(original.clone())
        .await
        .expect("accept native work");
    let captured_input = fixture
        .mailbox
        .accepted_input(&accepted)
        .await
        .expect("read captured native input");
    let captured_id = captured_input
        .id
        .as_ref()
        .expect("accepted input receives a stable ID");
    assert!(captured_id.as_str().starts_with("amsg_"));

    // A retry after a lost publication reuses both the immutable work reference
    // and the already-assigned provider item ID.
    assert_eq!(
        fixture.mailbox.accept_work(original).await.unwrap(),
        accepted
    );
    assert_eq!(
        fixture.mailbox.accepted_input(&accepted).await.unwrap(),
        captured_input
    );

    let mut conflicting_retry = native_work(issuer, agent, "native-1", "submission-1", "changed");
    conflicting_retry.input.trigger_turn = true;
    assert!(matches!(
        fixture.mailbox.accept_work(conflicting_retry).await,
        Err(MailboxError::IdempotencyConflict)
    ));

    assert_eq!(
        fixture.mailbox.begin_dispatch(&accepted).await.unwrap(),
        DispatchDecision::Dispatch
    );
    assert_eq!(
        fixture.mailbox.begin_dispatch(&accepted).await.unwrap(),
        DispatchDecision::RecoveryNeeded
    );

    // Reopening the durable store does not turn an unresolved claim into
    // permission to send again.
    let reopened = Mailbox::open(&fixture.config).await.unwrap();
    assert_eq!(
        reopened.dispatch_state(&accepted).await.unwrap(),
        DispatchState::Dispatching
    );
    assert_eq!(
        reopened.begin_dispatch(&accepted).await.unwrap(),
        DispatchDecision::RecoveryNeeded
    );
    reopened.mark_dispatched(&accepted).await.unwrap();
    assert_eq!(
        reopened.begin_dispatch(&accepted).await.unwrap(),
        DispatchDecision::AlreadyDispatched
    );
}

#[tokio::test]
async fn admission_is_bound_to_the_exact_input_ids_and_turn() {
    let fixture = test_mailbox().await;
    let issuer = thread_id(3);
    let agent = thread_id(4);
    let work_a = fixture
        .mailbox
        .accept_work(native_work(issuer, agent, "native-a", "submission-a", "a"))
        .await
        .unwrap();
    let work_b = fixture
        .mailbox
        .accept_work(native_work(issuer, agent, "native-b", "submission-b", "b"))
        .await
        .unwrap();
    fixture
        .mailbox
        .accept_work(native_work(issuer, agent, "native-c", "submission-c", "c"))
        .await
        .unwrap();
    let id_a = fixture
        .mailbox
        .accepted_input(&work_a)
        .await
        .unwrap()
        .id
        .expect("accepted input has an ID")
        .to_string();
    let id_b = fixture
        .mailbox
        .accepted_input(&work_b)
        .await
        .unwrap()
        .id
        .expect("accepted input has an ID")
        .to_string();
    let work_c = fixture
        .mailbox
        .work_for_native_key(issuer, "native-c")
        .await
        .unwrap()
        .expect("third native work was accepted");
    let id_c = fixture
        .mailbox
        .accepted_input(&work_c)
        .await
        .unwrap()
        .id
        .unwrap()
        .to_string();

    let receipt = fixture
        .mailbox
        .record_admission(agent, "turn-one", &[id_a.clone()])
        .await
        .unwrap();
    assert_eq!(receipt.work_refs, vec![work_a.clone()]);
    assert!(!receipt.already_recorded);

    let retry = fixture
        .mailbox
        .record_admission(agent, "turn-one", &[id_a.clone()])
        .await
        .unwrap();
    assert_eq!(retry.work_refs, receipt.work_refs);
    assert!(retry.already_recorded);

    let appended = fixture
        .mailbox
        .record_admission(agent, "turn-one", &[id_b.clone()])
        .await
        .unwrap();
    assert_eq!(appended.work_refs, vec![work_b.clone()]);
    assert!(!appended.already_recorded);
    let covered = fixture
        .mailbox
        .record_turn_outcome(
            agent,
            "turn-one",
            Outcome::Completed,
            None,
            Continuation::None,
        )
        .await
        .unwrap();
    assert_eq!(covered.outcomes.len(), 2);
    assert!(covered.outcomes.iter().any(|item| item.work_ref == work_a));
    assert!(covered.outcomes.iter().any(|item| item.work_ref == work_b));
    assert!(covered.outcomes.iter().all(|item| item.work_ref != work_c));

    assert!(matches!(
        fixture
            .mailbox
            .record_admission(agent, "turn-two", &[id_a])
            .await,
        Err(MailboxError::IdempotencyConflict)
    ));
    assert!(matches!(
        fixture
            .mailbox
            .record_admission(agent, "turn-one", &[id_c, "amsg_not-admitted".to_string()])
            .await,
        Err(MailboxError::ReferenceMismatch)
    ));

    let too_many = (0..101)
        .map(|index| format!("amsg-over-limit-{index}"))
        .collect::<Vec<_>>();
    assert!(matches!(
        fixture
            .mailbox
            .record_admission(agent, "oversized-turn", &too_many)
            .await,
        Err(MailboxError::Backpressure(_))
    ));
}

#[tokio::test]
async fn turn_outcome_covers_only_admitted_refs_and_snapshot_is_caller_scoped() {
    let fixture = test_mailbox().await;
    let issuer = thread_id(5);
    let other_issuer = thread_id(6);
    let agent = thread_id(7);
    let work_a = fixture
        .mailbox
        .accept_work(native_work(issuer, agent, "native-a", "submission-a", "a"))
        .await
        .unwrap();
    let work_b = fixture
        .mailbox
        .accept_work(native_work(issuer, agent, "native-b", "submission-b", "b"))
        .await
        .unwrap();
    let id_a = fixture
        .mailbox
        .accepted_input(&work_a)
        .await
        .unwrap()
        .id
        .unwrap()
        .to_string();
    let id_b = fixture
        .mailbox
        .accepted_input(&work_b)
        .await
        .unwrap()
        .id
        .unwrap()
        .to_string();
    fixture
        .mailbox
        .record_admission(agent, "turn-covered", &[id_a, id_b])
        .await
        .unwrap();

    // This accepted input arrives after the native execution admitted its
    // fixed set and must remain pending for a later turn.
    let unadmitted = fixture
        .mailbox
        .accept_work(native_work(issuer, agent, "native-late", "submission-late", "late"))
        .await
        .unwrap();
    let unrelated = fixture
        .mailbox
        .accept_work(native_work(
            other_issuer,
            agent,
            "native-other-issuer",
            "submission-other-issuer",
            "other issuer history",
        ))
        .await
        .unwrap();
    fixture
        .mailbox
        .record_outcome(
            &unrelated,
            Outcome::Completed,
            None,
            Continuation::None,
        )
        .await
        .unwrap();

    let outcome = fixture
        .mailbox
        .record_turn_outcome(
            agent,
            "turn-covered",
            Outcome::Completed,
            None,
            Continuation::None,
        )
        .await
        .unwrap();
    assert_eq!(outcome.outcomes.len(), 2);
    assert!(outcome.outcomes.iter().any(|item| item.work_ref == work_a));
    assert!(outcome.outcomes.iter().any(|item| item.work_ref == work_b));
    assert!(outcome.outcomes.iter().all(|item| item.work_ref != unadmitted));

    let snapshot = fixture
        .mailbox
        .snapshot(&selection(
            issuer,
            Some(agent),
            vec![
                Selector::Work(work_a.id.clone()),
                Selector::Work(work_b.id.clone()),
                Selector::Work(unadmitted.id.clone()),
                Selector::Work(unrelated.id.clone()),
            ],
            ReturnWhen::All,
        ))
        .await
        .unwrap();
    assert_eq!(snapshot.state, SnapshotState::Pending);
    assert_eq!(snapshot.items.len(), 3);
    assert!(work_item(&snapshot, &work_a.id).outcome.is_some());
    assert!(work_item(&snapshot, &work_b.id).outcome.is_some());
    assert!(work_item(&snapshot, &unadmitted.id).outcome.is_none());
    assert!(!snapshot
        .items
        .iter()
        .any(|item| matches!(item, SnapshotItem::Work(work) if work.work_ref.id == unrelated.id)));
}

#[tokio::test]
async fn one_turn_returns_separate_handbacks_to_each_issuer() {
    let fixture = test_mailbox().await;
    let issuer_a = thread_id(8);
    let issuer_b = thread_id(9);
    let agent = thread_id(12);
    let work_a = fixture
        .mailbox
        .accept_work(native_work(issuer_a, agent, "native-a", "submission-a", "a"))
        .await
        .unwrap();
    let work_b = fixture
        .mailbox
        .accept_work(native_work(issuer_b, agent, "native-b", "submission-b", "b"))
        .await
        .unwrap();
    let id_a = fixture
        .mailbox
        .accepted_input(&work_a)
        .await
        .unwrap()
        .id
        .unwrap()
        .to_string();
    let id_b = fixture
        .mailbox
        .accepted_input(&work_b)
        .await
        .unwrap()
        .id
        .unwrap()
        .to_string();
    fixture
        .mailbox
        .record_admission(agent, "shared-turn", &[id_a, id_b])
        .await
        .unwrap();

    let result = fixture
        .mailbox
        .record_turn_outcome(
            agent,
            "shared-turn",
            Outcome::Completed,
            Some(communication("result for both issuers")),
            Continuation::None,
        )
        .await
        .unwrap();
    assert_eq!(result.outcomes.len(), 2);
    let response_a = result
        .outcomes
        .iter()
        .find(|item| item.work_ref == work_a)
        .and_then(|item| item.response_message_id.as_deref())
        .expect("issuer A receives its response");
    let response_b = result
        .outcomes
        .iter()
        .find(|item| item.work_ref == work_b)
        .and_then(|item| item.response_message_id.as_deref())
        .expect("issuer B receives its response");
    assert_ne!(response_a, response_b);

    let handback_a = fixture
        .mailbox
        .get_message(issuer_a, response_a)
        .await
        .unwrap()
        .expect("issuer A handback is addressable");
    let handback_b = fixture
        .mailbox
        .get_message(issuer_b, response_b)
        .await
        .unwrap()
        .expect("issuer B handback is addressable");
    assert_eq!(handback_a.sender, agent);
    assert_eq!(handback_a.recipient, issuer_a);
    assert_eq!(handback_b.sender, agent);
    assert_eq!(handback_b.recipient, issuer_b);
}

#[tokio::test]
async fn pending_and_deferred_goal_bindings_settle_only_for_the_exact_generation() {
    let fixture = test_mailbox().await;
    let issuer = thread_id(13);
    let agent = thread_id(14);
    let work_pending = fixture
        .mailbox
        .accept_work(native_work(
            issuer,
            agent,
            "native-pending",
            "submission-pending",
            "pending goal work",
        ))
        .await
        .unwrap();
    let work_deferred = fixture
        .mailbox
        .accept_work(native_work(
            issuer,
            agent,
            "native-deferred",
            "submission-deferred",
            "deferred goal work",
        ))
        .await
        .unwrap();
    let binding_pending = goal_binding(issuer, agent, 4, "pending-turn");
    let binding_deferred = goal_binding(issuer, agent, 5, "deferred-turn");
    for (work_ref, turn_id, continuation) in [
        (
            &work_pending,
            "pending-turn",
            Continuation::PendingGoalBinding(binding_pending.clone()),
        ),
        (
            &work_deferred,
            "deferred-turn",
            Continuation::Deferred(binding_deferred.clone()),
        ),
    ] {
        let input_id = fixture
            .mailbox
            .accepted_input(work_ref)
            .await
            .unwrap()
            .id
            .unwrap()
            .to_string();
        fixture
            .mailbox
            .record_admission(agent, turn_id, &[input_id])
            .await
            .unwrap();
        let result = fixture
            .mailbox
            .record_turn_outcome(
                agent,
                turn_id,
                Outcome::Completed,
                None,
                continuation,
            )
            .await
            .unwrap();
        assert!(result.outcomes.is_empty());
        assert_eq!(result.continuation_work, vec![(*work_ref).clone()]);
    }

    for (work_ref, expected_state) in [
        (&work_pending, ContinuationState::PendingGoalBinding),
        (&work_deferred, ContinuationState::Deferred),
    ] {
        let snapshot = fixture
            .mailbox
            .snapshot(&selection(
                issuer,
                Some(agent),
                vec![Selector::Work(work_ref.id.clone())],
                ReturnWhen::Any,
            ))
            .await
            .unwrap();
        let item = work_item(&snapshot, &work_ref.id);
        assert!(item.outcome.is_none());
        assert_eq!(item.continuations.len(), 1);
        assert_eq!(item.continuations[0].state, expected_state);
    }

    let mut stale_generation = binding_pending.clone();
    stale_generation.control_generation += 1;
    let stale = fixture
        .mailbox
        .record_turn_outcome(
            agent,
            "later-turn-stale-generation",
            Outcome::Completed,
            None,
            Continuation::Terminal(stale_generation),
        )
        .await
        .unwrap();
    assert!(stale.no_matching_work);
    assert!(fixture
        .mailbox
        .snapshot(&selection(
            issuer,
            Some(agent),
            vec![Selector::Work(work_pending.id.clone())],
            ReturnWhen::Any,
        ))
        .await
        .unwrap()
        .items
        .iter()
        .any(|item| matches!(item, SnapshotItem::Work(work) if work.outcome.is_none())));

    for (work_ref, binding, turn_id) in [
        (&work_pending, binding_pending, "terminal-pending"),
        (&work_deferred, binding_deferred, "terminal-deferred"),
    ] {
        let terminal = fixture
            .mailbox
            .record_turn_outcome(
                agent,
                turn_id,
                Outcome::Completed,
                None,
                Continuation::Terminal(binding.clone()),
            )
            .await
            .unwrap();
        assert_eq!(terminal.outcomes.len(), 1);
        let snapshot = fixture
            .mailbox
            .snapshot(&selection(
                issuer,
                Some(agent),
                vec![Selector::Work(work_ref.id.clone())],
                ReturnWhen::Any,
            ))
            .await
            .unwrap();
        let receipt = work_item(&snapshot, &work_ref.id)
            .outcome
            .as_ref()
            .expect("exact terminal binding settles the old covered work");
        assert_eq!(receipt.continuation, Continuation::Terminal(binding));
    }
}

#[tokio::test]
async fn quiet_reply_satisfies_its_exact_request_selector() {
    let fixture = test_mailbox().await;
    let issuer = thread_id(15);
    let agent = thread_id(16);
    let request = fixture
        .mailbox
        .accept_message(message_draft(
            "decision-request",
            issuer,
            agent,
            MessageKind::Request,
            "please decide",
            false,
        ))
        .await
        .unwrap();
    fixture
        .mailbox
        .mark_canonical(agent, &request.id)
        .await
        .unwrap();

    let mut reply_draft = message_draft(
        "decision-reply",
        agent,
        issuer,
        MessageKind::Note,
        "decision made",
        false,
    );
    reply_draft.reply_to = Some(request.id.clone());
    let reply = fixture
        .mailbox
        .accept_message(reply_draft)
        .await
        .unwrap();

    let snapshot = fixture
        .mailbox
        .snapshot(&selection(
            issuer,
            Some(agent),
            vec![Selector::Reply(request.id.clone())],
            ReturnWhen::Any,
        ))
        .await
        .unwrap();
    assert_eq!(snapshot.state, SnapshotState::AlreadySatisfied);
    let selected_request = request_item(&snapshot, &request.id);
    assert_eq!(selected_request.disposition, RequestDisposition::Replied);
    assert_eq!(
        selected_request.reply.as_ref().map(|message| message.id.as_str()),
        Some(reply.id.as_str())
    );
    assert!(!reply.legacy_attention);
}

#[tokio::test]
async fn unrelated_successful_work_does_not_resolve_an_open_request() {
    let fixture = test_mailbox().await;
    let issuer = thread_id(17);
    let agent = thread_id(18);
    let request = fixture
        .mailbox
        .accept_message(message_draft(
            "open-request",
            issuer,
            agent,
            MessageKind::Request,
            "await a separate decision",
            false,
        ))
        .await
        .unwrap();
    let work_ref = fixture
        .mailbox
        .accept_work(native_work(
            issuer,
            agent,
            "unrelated-success",
            "unrelated-submission",
            "complete different work",
        ))
        .await
        .unwrap();
    let input_id = fixture
        .mailbox
        .accepted_input(&work_ref)
        .await
        .unwrap()
        .id
        .unwrap()
        .to_string();
    fixture
        .mailbox
        .record_admission(agent, "unrelated-turn", &[input_id])
        .await
        .unwrap();
    fixture
        .mailbox
        .record_turn_outcome(
            agent,
            "unrelated-turn",
            Outcome::Completed,
            None,
            Continuation::None,
        )
        .await
        .unwrap();

    let snapshot = fixture
        .mailbox
        .snapshot(&selection(
            issuer,
            Some(agent),
            vec![Selector::Reply(request.id.clone())],
            ReturnWhen::Any,
        ))
        .await
        .unwrap();
    assert_eq!(snapshot.state, SnapshotState::Pending);
    assert_eq!(request_item(&snapshot, &request.id).disposition, RequestDisposition::Open);
}

#[tokio::test]
async fn only_the_request_author_can_supersede_or_withdraw_and_old_selectors_settle() {
    let fixture = test_mailbox().await;
    let author = thread_id(19);
    let recipient = thread_id(22);
    let stranger = thread_id(23);
    let request = fixture
        .mailbox
        .accept_message(message_draft(
            "replace-me",
            author,
            recipient,
            MessageKind::Request,
            "first request",
            false,
        ))
        .await
        .unwrap();

    let mut unauthorized_replacement = message_draft(
        "stranger-replacement",
        stranger,
        recipient,
        MessageKind::Request,
        "not yours to replace",
        false,
    );
    unauthorized_replacement.supersedes = Some(request.id.clone());
    assert!(matches!(
        fixture
            .mailbox
            .accept_message(unauthorized_replacement)
            .await,
        Err(MailboxError::WrongRecipient)
    ));

    let mut replacement_draft = message_draft(
        "author-replacement",
        author,
        recipient,
        MessageKind::Request,
        "updated request",
        false,
    );
    replacement_draft.supersedes = Some(request.id.clone());
    let replacement = fixture
        .mailbox
        .accept_message(replacement_draft)
        .await
        .unwrap();
    let replaced_snapshot = fixture
        .mailbox
        .snapshot(&selection(
            author,
            Some(recipient),
            vec![Selector::Reply(request.id.clone())],
            ReturnWhen::Any,
        ))
        .await
        .unwrap();
    assert_eq!(replaced_snapshot.state, SnapshotState::AlreadySatisfied);
    let old_request = request_item(&replaced_snapshot, &request.id);
    assert_eq!(old_request.disposition, RequestDisposition::Superseded);
    assert_eq!(old_request.replacement.as_deref(), Some(replacement.id.as_str()));

    let withdrawal = fixture
        .mailbox
        .accept_message(message_draft(
            "withdraw-me",
            author,
            recipient,
            MessageKind::Request,
            "cancel this request",
            false,
        ))
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .mailbox
            .withdraw_message(stranger, &withdrawal.id)
            .await,
        Err(MailboxError::WrongRecipient)
    ));
    fixture
        .mailbox
        .withdraw_message(author, &withdrawal.id)
        .await
        .unwrap();
    let withdrawn_snapshot = fixture
        .mailbox
        .snapshot(&selection(
            author,
            Some(recipient),
            vec![Selector::Reply(withdrawal.id.clone())],
            ReturnWhen::Any,
        ))
        .await
        .unwrap();
    assert_eq!(withdrawn_snapshot.state, SnapshotState::AlreadySatisfied);
    assert_eq!(
        request_item(&withdrawn_snapshot, &withdrawal.id).disposition,
        RequestDisposition::Withdrawn
    );
}

#[tokio::test]
async fn progress_coalesces_only_for_the_exact_unadmitted_author_and_work() {
    let fixture = test_mailbox().await;
    let issuer = thread_id(24);
    let agent = thread_id(25);
    let work_a = fixture
        .mailbox
        .accept_work(native_work(issuer, agent, "progress-a", "submission-a", "a"))
        .await
        .unwrap();
    let work_b = fixture
        .mailbox
        .accept_work(native_work(issuer, agent, "progress-b", "submission-b", "b"))
        .await
        .unwrap();

    let mut first_draft = message_draft(
        "progress-first",
        issuer,
        agent,
        MessageKind::Progress,
        "initial status",
        false,
    );
    first_draft.work_ref = Some(work_a.clone());
    let first = fixture
        .mailbox
        .accept_message(first_draft)
        .await
        .unwrap();
    let mut update_draft = message_draft(
        "progress-update",
        issuer,
        agent,
        MessageKind::Progress,
        "latest status",
        false,
    );
    update_draft.work_ref = Some(work_a.clone());
    let update = fixture
        .mailbox
        .accept_message(update_draft)
        .await
        .unwrap();
    assert_eq!(update.id, first.id);
    assert_eq!(
        fixture
            .mailbox
            .get_message(agent, &first.id)
            .await
            .unwrap()
            .unwrap()
            .communication
            .content,
        "latest status"
    );

    let mut different_author = message_draft(
        "progress-other-author",
        issuer,
        agent,
        MessageKind::Progress,
        "different path author",
        false,
    );
    different_author.work_ref = Some(work_a.clone());
    different_author.communication.author =
        AgentPath::from_string("/root/child".to_string()).expect("valid author path");
    let other_author = fixture
        .mailbox
        .accept_message(different_author)
        .await
        .unwrap();
    assert_ne!(other_author.id, first.id);

    let mut different_work = message_draft(
        "progress-other-work",
        issuer,
        agent,
        MessageKind::Progress,
        "different work",
        false,
    );
    different_work.work_ref = Some(work_b);
    let other_work = fixture
        .mailbox
        .accept_message(different_work)
        .await
        .unwrap();
    assert_ne!(other_work.id, first.id);

    // No admission is necessary to coalesce quiet progress, but once its
    // exact work is terminal the same writer cannot append late progress.
    fixture
        .mailbox
        .record_outcome(&work_a, Outcome::Completed, None, Continuation::None)
        .await
        .unwrap();
    let mut late_progress = message_draft(
        "progress-after-terminal",
        issuer,
        agent,
        MessageKind::Progress,
        "too late",
        false,
    );
    late_progress.work_ref = Some(work_a);
    assert!(matches!(
        fixture.mailbox.accept_message(late_progress).await,
        Err(MailboxError::OutcomeAlreadyRecorded)
    ));
}

#[tokio::test]
async fn out_of_order_observation_is_per_item_after_canonical_flush() {
    let fixture = test_mailbox().await;
    let issuer = thread_id(26);
    let agent = thread_id(27);
    let work_a = fixture
        .mailbox
        .accept_work(native_work(issuer, agent, "observed-a", "submission-a", "a"))
        .await
        .unwrap();
    let work_b = fixture
        .mailbox
        .accept_work(native_work(issuer, agent, "observed-b", "submission-b", "b"))
        .await
        .unwrap();

    for (work_ref, turn_id) in [(&work_a, "turn-a"), (&work_b, "turn-b")] {
        let input_id = fixture
            .mailbox
            .accepted_input(work_ref)
            .await
            .unwrap()
            .id
            .unwrap()
            .to_string();
        fixture
            .mailbox
            .record_admission(agent, turn_id, &[input_id])
            .await
            .unwrap();
        let result = fixture
            .mailbox
            .record_turn_outcome(
                agent,
                turn_id,
                Outcome::Completed,
                None,
                Continuation::None,
            )
            .await
            .unwrap();
        let receipt = result.outcomes.first().expect("one exact outcome");
        fixture
            .mailbox
            .mark_outcome_canonical(agent, &receipt.id)
            .await
            .unwrap();
    }

    fixture
        .mailbox
        .acknowledge_observed(issuer, &[ObservationTarget::Work(work_b.clone())])
        .await
        .unwrap();
    let partially_observed = fixture
        .mailbox
        .snapshot(&selection(
            issuer,
            Some(agent),
            vec![
                Selector::Work(work_a.id.clone()),
                Selector::Work(work_b.id.clone()),
            ],
            ReturnWhen::All,
        ))
        .await
        .unwrap();
    assert_eq!(partially_observed.state, SnapshotState::Pending);
    assert!(!work_item(&partially_observed, &work_a.id).observed);
    assert!(work_item(&partially_observed, &work_b.id).observed);
    assert!(work_item(&partially_observed, &work_a.id).outcome.is_some());
    assert!(work_item(&partially_observed, &work_b.id).outcome.is_some());

    fixture
        .mailbox
        .acknowledge_observed(issuer, &[ObservationTarget::Work(work_a.clone())])
        .await
        .unwrap();
    let fully_observed = fixture
        .mailbox
        .snapshot(&selection(
            issuer,
            Some(agent),
            vec![
                Selector::Work(work_a.id.clone()),
                Selector::Work(work_b.id.clone()),
            ],
            ReturnWhen::All,
        ))
        .await
        .unwrap();
    assert_eq!(fully_observed.state, SnapshotState::AlreadySatisfied);
}

#[tokio::test]
async fn default_and_legacy_attention_are_separate_from_quiet_message_delivery() {
    let fixture = test_mailbox().await;
    let sender = thread_id(10);
    let recipient = thread_id(11);
    let kinds = [
        MessageKind::Note,
        MessageKind::Progress,
        MessageKind::Request,
        MessageKind::Alert,
    ];
    assert_eq!(MessageKind::default(), MessageKind::Note);

    for kind in kinds {
        fixture
            .mailbox
            .accept_message(message_draft(
                &format!("quiet-{kind:?}"),
                sender,
                recipient,
                kind,
                "quiet message",
                false,
            ))
            .await
            .unwrap();
        fixture
            .mailbox
            .accept_message(message_draft(
                &format!("legacy-{kind:?}"),
                sender,
                recipient,
                kind,
                "legacy attention message",
                true,
            ))
            .await
            .unwrap();
    }

    let pending = fixture.mailbox.pending(recipient, 20).await.unwrap();
    assert_eq!(pending.len(), 8);
    for kind in kinds {
        assert_eq!(pending.iter().filter(|message| message.kind == kind).count(), 2);
    }
    assert_eq!(
        pending.iter().filter(|message| message.legacy_attention).count(),
        4
    );
    assert_eq!(fixture.mailbox.quiet_count(recipient).await.unwrap(), 4);
    assert_eq!(fixture.mailbox.attention(recipient, 20).await.unwrap().len(), 4);
}

#[tokio::test]
async fn activation_claim_is_not_reissued_after_a_lost_result() {
    let fixture = test_mailbox().await;
    let sender = thread_id(20);
    let recipient = thread_id(21);
    let message = fixture
        .mailbox
        .accept_message(message_draft(
            "activation-request",
            sender,
            recipient,
            MessageKind::Alert,
            "activate once",
            false,
        ))
        .await
        .unwrap();

    assert_eq!(
        fixture
            .mailbox
            .begin_activation(sender, &message.id)
            .await
            .unwrap(),
        ActivationDecision::Activate
    );
    assert_eq!(
        fixture
            .mailbox
            .begin_activation(sender, &message.id)
            .await
            .unwrap(),
        ActivationDecision::RecoveryNeeded
    );
    fixture
        .mailbox
        .record_activation(sender, &message.id, ActivationDisposition::Activated)
        .await
        .unwrap();
    fixture
        .mailbox
        .record_activation(sender, &message.id, ActivationDisposition::Activated)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .mailbox
            .begin_activation(sender, &message.id)
            .await
            .unwrap(),
        ActivationDecision::AlreadyDecided(ActivationDisposition::Activated)
    );
    assert!(matches!(
        fixture
            .mailbox
            .record_activation(sender, &message.id, ActivationDisposition::Denied)
            .await,
        Err(MailboxError::IdempotencyConflict)
    ));
}

#[tokio::test]
async fn oversized_native_capture_is_rejected_without_creating_dispatchable_work() {
    let fixture = test_mailbox().await;
    let issuer = thread_id(30);
    let agent = thread_id(31);
    let mut too_large = native_work(issuer, agent, "too-large", "large-submission", "x");
    too_large.input.content = "x".repeat(1_000_001);

    assert!(matches!(
        fixture.mailbox.accept_work(too_large).await,
        Err(MailboxError::Backpressure(_))
    ));
    assert!(fixture
        .mailbox
        .work_for_native_key(issuer, "too-large")
        .await
        .unwrap()
        .is_none());
}

#[test]
fn closed_peer_cycles_require_every_join_choice_to_remain_inside_the_cycle() {
    let any_cycle = [
        Dependency {
            owner: "a".to_string(),
            peers: vec!["b".to_string(), "c".to_string()],
            mode: DependencyMode::Any,
        },
        Dependency {
            owner: "b".to_string(),
            peers: vec!["a".to_string()],
            mode: DependencyMode::All,
        },
        Dependency {
            owner: "c".to_string(),
            peers: vec!["a".to_string()],
            mode: DependencyMode::All,
        },
    ];
    assert!(is_provably_closed_peer_cycle("a", &any_cycle));

    let all_cycle = [
        Dependency {
            owner: "x".to_string(),
            peers: vec!["y".to_string(), "z".to_string()],
            mode: DependencyMode::All,
        },
        Dependency {
            owner: "y".to_string(),
            peers: vec!["x".to_string()],
            mode: DependencyMode::Any,
        },
        Dependency {
            owner: "z".to_string(),
            peers: vec!["x".to_string()],
            mode: DependencyMode::Any,
        },
    ];
    assert!(is_provably_closed_peer_cycle("x", &all_cycle));

    let with_open_exit = [
        Dependency {
            owner: "a".to_string(),
            peers: vec!["b".to_string(), "not-in-cycle".to_string()],
            mode: DependencyMode::Any,
        },
        Dependency {
            owner: "b".to_string(),
            peers: vec!["a".to_string()],
            mode: DependencyMode::All,
        },
    ];
    assert!(!is_provably_closed_peer_cycle("a", &with_open_exit));

    let all_with_open_exit = [
        Dependency {
            owner: "x".to_string(),
            peers: vec!["y".to_string(), "not-in-cycle".to_string()],
            mode: DependencyMode::All,
        },
        Dependency {
            owner: "y".to_string(),
            peers: vec!["x".to_string()],
            mode: DependencyMode::Any,
        },
    ];
    assert!(!is_provably_closed_peer_cycle("x", &all_with_open_exit));
}
