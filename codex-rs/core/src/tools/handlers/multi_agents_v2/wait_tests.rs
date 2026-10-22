use super::*;
use crate::session::TurnInput;
use crate::state::TurnState;
use codex_protocol::AgentPath;
use codex_protocol::protocol::AgentCommunicationOrigin;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;
use tokio::sync::Mutex;

fn mail(origin: Option<AgentCommunicationOrigin>, encrypted: bool) -> InterAgentCommunication {
    let mut mail = if encrypted {
        InterAgentCommunication::new_encrypted(
            AgentPath::root(),
            AgentPath::root(),
            Vec::new(),
            "opaque".to_string(),
            /*trigger_turn*/ false,
        )
    } else {
        InterAgentCommunication::new(
            AgentPath::root(),
            AgentPath::root(),
            Vec::new(),
            String::new(),
            /*trigger_turn*/ false,
        )
    };
    mail.origin = origin;
    mail
}

#[tokio::test]
async fn informational_mail_stays_quiet_and_ordered_until_completion() {
    for encrypted in [false, true] {
        let queue = InputQueue::new();
        let first = mail(Some(AgentCommunicationOrigin::Informational), encrypted);
        queue
            .enqueue_mailbox_communication(first.clone(), Default::default())
            .await;
        let (mut activity, pending) = queue.subscribe_activity(/*turn_state*/ None).await;
        let waiting = wait_for_activity(
            &queue,
            /*turn_state*/ None,
            &mut activity,
            pending,
            Instant::now() + Duration::from_secs(/*secs*/ 5),
        );
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        let second = first.clone();
        queue
            .enqueue_mailbox_communication(second.clone(), Default::default())
            .await;
        assert!(futures::poll!(&mut waiting).is_pending());
        let completion = mail(Some(AgentCommunicationOrigin::Result), encrypted);
        queue
            .enqueue_mailbox_communication(completion.clone(), Default::default())
            .await;
        assert_eq!(
            waiting.await,
            WaitOutcome::MailboxActivity(MailboxWakeCause::Completion)
        );
        assert_eq!(
            queue.drain_mailbox_input_items().await.0,
            vec![
                TurnInput::InterAgentCommunication(first),
                TurnInput::InterAgentCommunication(second),
                TurnInput::InterAgentCommunication(completion),
            ]
        );
    }
}

#[tokio::test]
async fn actionable_completion_and_legacy_mail_wake_without_idle_permission() {
    for (origin, cause) in [
        (
            Some(AgentCommunicationOrigin::Actionable),
            MailboxWakeCause::ActionableMessage,
        ),
        (
            Some(AgentCommunicationOrigin::Result),
            MailboxWakeCause::Completion,
        ),
        (None, MailboxWakeCause::LegacyMailbox),
    ] {
        for encrypted in [false, true] {
            let queue = InputQueue::new();
            queue
                .enqueue_mailbox_communication(mail(origin, encrypted), Default::default())
                .await;
            let (mut activity, pending) = queue.subscribe_activity(/*turn_state*/ None).await;
            assert_eq!(
                wait_for_activity(
                    &queue,
                    /*turn_state*/ None,
                    &mut activity,
                    pending,
                    Instant::now()
                )
                .await,
                WaitOutcome::MailboxActivity(cause)
            );
            assert!(!queue.has_trigger_turn_mailbox_items().await);
        }
    }
}

#[tokio::test]
async fn informational_notifications_do_not_extend_the_original_deadline() {
    tokio::time::pause();
    let queue = InputQueue::new();
    let (mut activity, pending) = queue.subscribe_activity(/*turn_state*/ None).await;
    let started = Instant::now();
    let waiting = wait_for_activity(
        &queue,
        /*turn_state*/ None,
        &mut activity,
        pending,
        started + Duration::from_secs(/*secs*/ 10),
    );
    tokio::pin!(waiting);
    for _ in 0..3 {
        assert!(futures::poll!(&mut waiting).is_pending());
        tokio::time::advance(Duration::from_secs(/*secs*/ 3)).await;
        queue
            .enqueue_mailbox_communication(
                mail(
                    Some(AgentCommunicationOrigin::Informational),
                    /*encrypted*/ false,
                ),
                Default::default(),
            )
            .await;
    }
    assert_eq!(waiting.await, WaitOutcome::TimedOut);
    // Match the native wait tests' millisecond timer resolution, not a reset deadline.
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(/*secs*/ 10)
            && elapsed <= Duration::from_millis(/*millis*/ 10_001),
        "informational mail must not reset the original deadline: {elapsed:?}"
    );
    assert_eq!(queue.drain_mailbox().await.len(), 3);
}

#[tokio::test]
async fn coalesced_mailbox_hint_preserves_steer_priority() {
    let queue = InputQueue::new();
    let turn_state = Mutex::new(TurnState::default());
    let (mut activity, pending) = queue.subscribe_activity(Some(&turn_state)).await;
    queue
        .extend_pending_input_for_turn_state(
            &turn_state,
            vec![TurnInput::UserInput {
                content: vec![UserInput::Text {
                    text: "steer".to_string(),
                    text_elements: Vec::new(),
                }],
                client_id: None,
                metadata: Default::default(),
            }],
        )
        .await;
    queue
        .enqueue_mailbox_communication(
            mail(
                Some(AgentCommunicationOrigin::Informational),
                /*encrypted*/ true,
            ),
            Default::default(),
        )
        .await;
    assert_eq!(
        wait_for_activity(
            &queue,
            Some(&turn_state),
            &mut activity,
            pending,
            Instant::now()
        )
        .await,
        WaitOutcome::Steered
    );
    assert_eq!(queue.drain_mailbox().await.len(), 1);
}

#[tokio::test]
async fn dropping_a_quiet_wait_preserves_mail_and_shared_sleep_notification() {
    let queue = InputQueue::new();
    let (mut activity, pending) = queue.subscribe_activity(/*turn_state*/ None).await;
    let (mut shared_activity, _) = queue.subscribe_activity(/*turn_state*/ None).await;
    let mut waiting = Box::pin(wait_for_activity(
        &queue,
        /*turn_state*/ None,
        &mut activity,
        pending,
        Instant::now() + Duration::from_secs(/*secs*/ 5),
    ));
    assert!(futures::poll!(&mut waiting).is_pending());
    queue
        .enqueue_mailbox_communication(
            mail(
                Some(AgentCommunicationOrigin::Informational),
                /*encrypted*/ false,
            ),
            Default::default(),
        )
        .await;
    assert!(futures::poll!(&mut waiting).is_pending());
    shared_activity.changed().await.unwrap();
    assert_eq!(
        *shared_activity.borrow_and_update(),
        InputQueueActivity::Mailbox
    );
    drop(waiting);
    assert_eq!(queue.drain_mailbox().await.len(), 1);
}

#[tokio::test]
async fn mixed_eligible_causes_are_reported_without_sender_attribution() {
    let queue = InputQueue::new();
    for origin in [
        AgentCommunicationOrigin::Actionable,
        AgentCommunicationOrigin::Result,
    ] {
        queue
            .enqueue_mailbox_communication(
                mail(Some(origin), /*encrypted*/ false),
                Default::default(),
            )
            .await;
    }
    let (mut activity, pending) = queue.subscribe_activity(/*turn_state*/ None).await;
    let outcome = wait_for_activity(
        &queue,
        /*turn_state*/ None,
        &mut activity,
        pending,
        Instant::now(),
    )
    .await;
    let result = WaitAgentResult::from_outcome(
        outcome,
        /*requested_timeout_ms*/ None,
        /*timeout_ms*/ 0,
    );
    assert_eq!(result.wake_cause, WaitWakeCause::MailboxActivity);
    assert!(!result.timed_out);
    assert_eq!(queue.drain_mailbox().await.len(), 2);
}
