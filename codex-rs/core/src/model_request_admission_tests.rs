use super::*;

use chrono::Duration;
use codex_state::GoalOwnerAdmissionDenialClass;
use codex_state::GoalOwnerAdmissionObservation;
use codex_state::GoalOwnerAdmissionPhase;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tempfile::TempDir;

fn identity(thread_id: ThreadId, kind: ModelRequestKind) -> ModelRequestIdentity {
    ModelRequestIdentity::new(
        thread_id,
        Some("turn-1".to_string()),
        kind,
        "test-provider".to_string(),
        "requested-model".to_string(),
        "effective-model".to_string(),
        Some("priority".to_string()),
        SessionSource::Cli,
        None,
    )
}

async fn runtime() -> (TempDir, StateDbHandle) {
    let home = TempDir::new().expect("temporary Codex home");
    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(home.path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("initialize state runtime");
    (home, runtime)
}

fn observation(
    thread_id: ThreadId,
    origin_request_id: &str,
    phase: GoalOwnerAdmissionPhase,
    deadline_at: chrono::DateTime<Utc>,
) -> GoalOwnerAdmissionObservation {
    GoalOwnerAdmissionObservation {
        thread_id,
        goal_id: "goal-a".to_string(),
        origin_turn_id: "turn-1".to_string(),
        origin_request_id: origin_request_id.to_string(),
        denial_class: GoalOwnerAdmissionDenialClass::Capacity,
        provider_id: Some("test-provider".to_string()),
        requested_model: Some("requested-model".to_string()),
        effective_model: Some("effective-model".to_string()),
        account_context_fingerprint: None,
        deadline_at,
        max_attempts: 2,
        requested_phase: phase,
        phase,
    }
}

async fn fake_stream_request(
    decision: &ModelRequestAdmissionDecision,
    calls: &AtomicUsize,
) -> Result<()> {
    let mut lease = decision.begin_network_request().await?;
    calls.fetch_add(1, Ordering::SeqCst);
    lease.provider_acknowledged().await?;
    lease.completed().await
}

async fn fake_unary_compact_request(
    decision: &ModelRequestAdmissionDecision,
    calls: &AtomicUsize,
) -> Result<()> {
    let mut lease = decision.begin_network_request().await?;
    calls.fetch_add(1, Ordering::SeqCst);
    lease.completed().await
}

#[tokio::test]
async fn stream_and_unary_admission_decisions_prevent_unapproved_network_io() {
    let (_home, state_db) = runtime().await;
    let broker = ModelRequestAdmissionBroker::new(Some(state_db));
    let now = Utc::now();

    let cases = [
        (GoalOwnerAdmissionPhase::Dormant, now, "dormant"),
        (
            GoalOwnerAdmissionPhase::Pending,
            now + Duration::minutes(1),
            "deferred",
        ),
    ];
    for (index, (phase, deadline, name)) in cases.into_iter().enumerate() {
        let thread_id = ThreadId::new();
        let state_db = broker.state_db.as_ref().expect("state runtime");
        state_db
            .goal_owner_admissions()
            .observe_denial(&observation(
                thread_id,
                &format!("request-{name}-{index}"),
                phase,
                deadline,
            ))
            .await
            .expect("record blocked admission");
        let decision = broker
            .admit(&identity(thread_id, ModelRequestKind::Turn))
            .await
            .expect("evaluate admission");
        let calls = AtomicUsize::new(0);
        assert!(fake_stream_request(&decision, &calls).await.is_err());
        assert!(fake_unary_compact_request(&decision, &calls).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{name} made network I/O");
    }
}

#[tokio::test]
async fn eligible_lease_allows_exactly_one_stream_or_unary_request() {
    let (_home, state_db) = runtime().await;
    let broker = ModelRequestAdmissionBroker::new(Some(state_db));

    for kind in [ModelRequestKind::Turn, ModelRequestKind::RemoteCompact] {
        let thread_id = ThreadId::new();
        broker
            .state_db
            .as_ref()
            .expect("state runtime")
            .goal_owner_admissions()
            .observe_denial(&observation(
                thread_id,
                &format!("request-{kind:?}"),
                GoalOwnerAdmissionPhase::Pending,
                Utc::now() - Duration::seconds(1),
            ))
            .await
            .expect("record eligible admission");
        let decision = broker
            .admit(&identity(thread_id, kind))
            .await
            .expect("acquire admission");
        let calls = AtomicUsize::new(0);
        if kind == ModelRequestKind::RemoteCompact {
            fake_unary_compact_request(&decision, &calls)
                .await
                .expect("one unary request");
        } else {
            fake_stream_request(&decision, &calls)
                .await
                .expect("one stream request");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(decision.begin_network_request().await.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second lease use opened I/O"
        );
    }
}

#[tokio::test]
async fn exhausted_cancelled_and_uncertain_admissions_cause_zero_network_requests() {
    let (_home, state_db) = runtime().await;
    let broker = ModelRequestAdmissionBroker::new(Some(state_db));
    let store = broker
        .state_db
        .as_ref()
        .expect("state runtime")
        .goal_owner_admissions();

    for (origin, terminal_outcome) in [
        ("exhausted", GoalOwnerAdmissionTerminalOutcome::Exhausted),
        ("uncertain", GoalOwnerAdmissionTerminalOutcome::Uncertain),
    ] {
        let thread_id = ThreadId::new();
        let record = store
            .observe_denial(&observation(
                thread_id,
                origin,
                GoalOwnerAdmissionPhase::Pending,
                Utc::now() - Duration::seconds(1),
            ))
            .await
            .expect("record eligible admission");
        let lease = store
            .try_acquire(&record.authority, Utc::now())
            .await
            .expect("acquire admission")
            .expect("eligible lease");
        store
            .finish(
                &lease,
                terminal_outcome,
                if terminal_outcome == GoalOwnerAdmissionTerminalOutcome::Uncertain {
                    GoalOwnerAdmissionTerminalDisposition::ManualReview
                } else {
                    GoalOwnerAdmissionTerminalDisposition::None
                },
            )
            .await
            .expect("terminalize admission");
        let decision = broker
            .admit(&identity(thread_id, ModelRequestKind::Turn))
            .await
            .expect("read terminal admission");
        let calls = AtomicUsize::new(0);
        assert!(fake_stream_request(&decision, &calls).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{origin} made network I/O");
    }

    let thread_id = ThreadId::new();
    let record = store
        .observe_denial(&observation(
            thread_id,
            "in-flight",
            GoalOwnerAdmissionPhase::Pending,
            Utc::now() - Duration::seconds(1),
        ))
        .await
        .expect("record eligible admission");
    let _lease = store
        .try_acquire(&record.authority, Utc::now())
        .await
        .expect("acquire admission")
        .expect("eligible lease");
    let decision = broker
        .admit(&identity(thread_id, ModelRequestKind::Turn))
        .await
        .expect("read in-flight admission");
    let calls = AtomicUsize::new(0);
    assert!(fake_stream_request(&decision, &calls).await.is_err());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "in-flight made network I/O"
    );

    let thread_id = ThreadId::new();
    let record = store
        .observe_denial(&observation(
            thread_id,
            "cancelled",
            GoalOwnerAdmissionPhase::Pending,
            Utc::now() + Duration::minutes(1),
        ))
        .await
        .expect("record cancellable admission");
    store
        .cancel(
            &record.authority,
            GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn,
        )
        .await
        .expect("cancel admission");
    let decision = broker
        .admit(&identity(thread_id, ModelRequestKind::Turn))
        .await
        .expect("read cancelled admission");
    let calls = AtomicUsize::new(0);
    assert!(fake_unary_compact_request(&decision, &calls).await.is_err());
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "cancelled admission made network I/O"
    );
}

#[tokio::test]
async fn denial_and_ambiguous_drop_terminalize_without_automatic_replay() {
    let (_home, state_db) = runtime().await;
    let broker = ModelRequestAdmissionBroker::new(Some(state_db));

    for (origin, drop_after_open) in [("denial", false), ("drop", true)] {
        let thread_id = ThreadId::new();
        let store = broker
            .state_db
            .as_ref()
            .expect("state runtime")
            .goal_owner_admissions();
        store
            .observe_denial(&observation(
                thread_id,
                origin,
                GoalOwnerAdmissionPhase::Pending,
                Utc::now() - Duration::seconds(1),
            ))
            .await
            .expect("record eligible admission");
        let decision = broker
            .admit(&identity(thread_id, ModelRequestKind::Turn))
            .await
            .expect("acquire admission");
        let calls = Arc::new(AtomicUsize::new(0));
        let mut lease = decision.begin_network_request().await.expect("one request");
        calls.fetch_add(1, Ordering::SeqCst);
        if drop_after_open {
            lease
                .transport_lost()
                .await
                .expect("terminalize uncertainty");
        } else {
            lease
                .provider_denied()
                .await
                .expect("record provider denial");
        }
        let replay = broker
            .admit(&identity(thread_id, ModelRequestKind::Turn))
            .await
            .expect("read terminal admission");
        assert!(matches!(replay, ModelRequestAdmissionDecision::Dormant));
        assert!(replay.begin_network_request().await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "third request opened I/O");
    }
}

#[tokio::test]
async fn unrestricted_and_typed_prewarm_do_not_consume_goal_owner_admission() {
    let (_home, state_db) = runtime().await;
    let broker = ModelRequestAdmissionBroker::new(Some(state_db));
    let calls = AtomicUsize::new(0);

    let unrestricted = broker
        .admit(&identity(ThreadId::new(), ModelRequestKind::Turn))
        .await
        .expect("absent admission is unrestricted");
    fake_stream_request(&unrestricted, &calls)
        .await
        .expect("unrestricted stream");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let thread_id = ThreadId::new();
    let store = broker
        .state_db
        .as_ref()
        .expect("state runtime")
        .goal_owner_admissions();
    let record = store
        .observe_denial(&observation(
            thread_id,
            "succeeded",
            GoalOwnerAdmissionPhase::Pending,
            Utc::now() - Duration::seconds(1),
        ))
        .await
        .expect("record eligible admission");
    let lease = store
        .try_acquire(&record.authority, Utc::now())
        .await
        .expect("acquire admission")
        .expect("eligible lease");
    store
        .finish(
            &lease,
            GoalOwnerAdmissionTerminalOutcome::Succeeded,
            GoalOwnerAdmissionTerminalDisposition::None,
        )
        .await
        .expect("record successful lease");
    let succeeded = broker
        .admit(&identity(thread_id, ModelRequestKind::RemoteCompact))
        .await
        .expect("succeeded record becomes unrestricted");
    fake_unary_compact_request(&succeeded, &calls)
        .await
        .expect("unrestricted unary request after success");
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let prewarm_identity = identity(ThreadId::new(), ModelRequestKind::Prewarm);
    assert!(!prewarm_identity.kind.is_inference());
    let prewarm = broker
        .admit(&prewarm_identity)
        .await
        .expect("typed prewarm exemption");
    assert!(matches!(
        prewarm,
        ModelRequestAdmissionDecision::Unrestricted
    ));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "prewarm was counted as inference"
    );
}
