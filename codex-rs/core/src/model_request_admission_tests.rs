use super::*;

use chrono::Duration;
use codex_state::GoalOwnerAdmissionAcquireResult;
use codex_state::GoalOwnerAdmissionDenialClass;
use codex_state::GoalOwnerAdmissionObservation;
use codex_state::GoalOwnerAdmissionPhase;
use codex_state::GoalOwnerAdmissionRecord;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tempfile::TempDir;

const SUCCESSOR_TURN_ID: &str = "turn-successor";
const SUCCESSOR_REQUEST_ID: &str = "successor-request";

fn fenced(continuation: GoalOwnerContinuation) -> GoalOwnerContinuation {
    continuation.with_fence(Arc::new(GoalContinuationFence::new()), 0)
}

fn identity(thread_id: ThreadId, kind: InferenceRequestKind) -> ModelRequestIdentity {
    identity_with(thread_id, kind, SUCCESSOR_TURN_ID, SUCCESSOR_REQUEST_ID)
}

fn identity_with(
    thread_id: ThreadId,
    kind: InferenceRequestKind,
    turn_id: &str,
    logical_request_id: &str,
) -> ModelRequestIdentity {
    ModelRequestIdentity::inference(
        thread_id,
        Some(turn_id.to_string()),
        kind,
        "configured-provider".to_string(),
        Some("configured-model".to_string()),
        "effective-provider".to_string(),
        "effective-model".to_string(),
        Some("priority".to_string()),
        SessionSource::Cli,
        /*parent_continuity_decision_id*/ None,
        Some(logical_request_id.to_string()),
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
    kind: InferenceRequestKind,
) -> GoalOwnerAdmissionObservation {
    GoalOwnerAdmissionObservation {
        thread_id,
        goal_id: "goal-a".to_string(),
        origin_turn_id: "denial-turn".to_string(),
        origin_request_id: origin_request_id.to_string(),
        denial_class: GoalOwnerAdmissionDenialClass::Capacity,
        configured_provider_key: Some("configured-provider".to_string()),
        requested_model: Some("configured-model".to_string()),
        effective_provider_id: Some("effective-provider".to_string()),
        effective_model: Some("effective-model".to_string()),
        intended_request_kind: kind.as_str().to_string(),
        successor_turn_id: SUCCESSOR_TURN_ID.to_string(),
        logical_successor_request_id: SUCCESSOR_REQUEST_ID.to_string(),
        decision_id: Uuid::now_v7(),
        account_context_fingerprint: None,
        deadline_at,
        max_attempts: 2,
        requested_phase: phase,
        phase,
    }
}

async fn admit(
    broker: &ModelRequestAdmissionBroker,
    record: &GoalOwnerAdmissionRecord,
    identity: &ModelRequestIdentity,
) -> ModelRequestAdmissionDecision {
    let claim_id = broker
        .state_db
        .as_ref()
        .expect("state runtime")
        .goal_owner_admissions()
        .claim_dispatch(&record.continuation_authority(), Utc::now())
        .await
        .expect("claim exact admission")
        .expect("eligible admission claim");
    let continuation = fenced(GoalOwnerContinuation::with_dispatch_claim(
        record.continuation_authority(),
        claim_id,
    ));
    broker
        .admit(identity, Some(&continuation))
        .await
        .expect("evaluate exact admission")
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
async fn nonterminal_records_require_exact_authority_before_any_provider_call() {
    let (_home, state_db) = runtime().await;
    let broker = ModelRequestAdmissionBroker::new(Some(state_db));
    let store = broker
        .state_db
        .as_ref()
        .expect("state runtime")
        .goal_owner_admissions();
    let now = Utc::now();

    for (phase, deadline, name) in [
        (GoalOwnerAdmissionPhase::Dormant, now, "dormant"),
        (
            GoalOwnerAdmissionPhase::Pending,
            now + Duration::minutes(1),
            "deferred",
        ),
    ] {
        let thread_id = ThreadId::new();
        store
            .observe_denial(&observation(
                thread_id,
                name,
                phase,
                deadline,
                InferenceRequestKind::Turn,
            ))
            .await
            .expect("record blocked admission");
        let decision = broker
            .admit(
                &identity(thread_id, InferenceRequestKind::Turn),
                /*continuation*/ None,
            )
            .await
            .expect("evaluate blocked admission");
        let calls = AtomicUsize::new(0);
        assert!(fake_stream_request(&decision, &calls).await.is_err());
        assert!(fake_unary_compact_request(&decision, &calls).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{name} made network I/O");
    }
}

#[tokio::test]
async fn continuation_token_fences_same_thread_wrong_kind_turn_and_logical_request() {
    let (_home, state_db) = runtime().await;
    let broker = ModelRequestAdmissionBroker::new(Some(state_db));
    let store = broker
        .state_db
        .as_ref()
        .expect("state runtime")
        .goal_owner_admissions();
    let thread_id = ThreadId::new();
    let record = store
        .observe_denial(&observation(
            thread_id,
            "exact",
            GoalOwnerAdmissionPhase::Pending,
            Utc::now() - Duration::seconds(1),
            InferenceRequestKind::Turn,
        ))
        .await
        .expect("record eligible admission");
    let authority = fenced(GoalOwnerContinuation::new(record.continuation_authority()));

    let wrong_kind = identity(thread_id, InferenceRequestKind::RemoteCompact);
    let wrong_turn = identity_with(
        thread_id,
        InferenceRequestKind::Turn,
        "unrelated-memory-or-review-turn",
        SUCCESSOR_REQUEST_ID,
    );
    let wrong_request = identity_with(
        thread_id,
        InferenceRequestKind::Turn,
        SUCCESSOR_TURN_ID,
        "unrelated-logical-request",
    );
    let mut wrong_effective_provider = identity(thread_id, InferenceRequestKind::Turn);
    wrong_effective_provider.effective_provider_id = "unrelated-fallback-provider".to_string();
    let mut wrong_authority = authority.authority().clone();
    wrong_authority.decision_id = Uuid::now_v7();

    for (identity, authority) in [
        (identity(thread_id, InferenceRequestKind::Turn), None),
        (wrong_kind, Some(authority.clone())),
        (wrong_turn, Some(authority.clone())),
        (wrong_request, Some(authority.clone())),
        (wrong_effective_provider, Some(authority.clone())),
        (
            identity(thread_id, InferenceRequestKind::Turn),
            Some(fenced(GoalOwnerContinuation::new(wrong_authority))),
        ),
    ] {
        let decision = broker
            .admit(&identity, authority.as_ref())
            .await
            .expect("evaluate fenced admission");
        let calls = AtomicUsize::new(0);
        assert!(fake_stream_request(&decision, &calls).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0, "fenced request made I/O");
    }

    let decision = admit(
        &broker,
        &record,
        &identity(thread_id, InferenceRequestKind::Turn),
    )
    .await;
    let calls = AtomicUsize::new(0);
    fake_stream_request(&decision, &calls)
        .await
        .expect("exact successor is admitted");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(decision.begin_network_request().await.is_err());
}

#[tokio::test]
async fn cancellation_before_request_open_fence_causes_zero_physical_calls() {
    let (_home, state_db) = runtime().await;
    let broker = ModelRequestAdmissionBroker::new(Some(state_db));
    let store = broker
        .state_db
        .as_ref()
        .expect("state runtime")
        .goal_owner_admissions();
    let thread_id = ThreadId::new();
    let record = store
        .observe_denial(&observation(
            thread_id,
            "cancel-before-open",
            GoalOwnerAdmissionPhase::Pending,
            Utc::now() - Duration::seconds(1),
            InferenceRequestKind::Turn,
        ))
        .await
        .expect("record eligible admission");
    let decision = admit(
        &broker,
        &record,
        &identity(thread_id, InferenceRequestKind::Turn),
    )
    .await;
    store
        .cancel(
            &record.authority,
            GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn,
        )
        .await
        .expect("cancel acquired reservation");
    let calls = AtomicUsize::new(0);
    assert!(fake_stream_request(&decision, &calls).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn terminal_success_is_unrestricted_before_identity_matching() {
    let (_home, state_db) = runtime().await;
    let broker = ModelRequestAdmissionBroker::new(Some(state_db));
    let store = broker
        .state_db
        .as_ref()
        .expect("state runtime")
        .goal_owner_admissions();
    let thread_id = ThreadId::new();
    let record = store
        .observe_denial(&observation(
            thread_id,
            "succeeded",
            GoalOwnerAdmissionPhase::Pending,
            Utc::now() - Duration::seconds(1),
            InferenceRequestKind::Turn,
        ))
        .await
        .expect("record eligible admission");
    let lease = match store
        .try_acquire(&record.continuation_authority(), Utc::now())
        .await
        .expect("acquire admission")
    {
        GoalOwnerAdmissionAcquireResult::Acquired(lease) => *lease,
        result => panic!("expected eligible lease, got {result:?}"),
    };
    assert!(store.open_lease(&lease).await.expect("open admission"));
    store
        .finish(
            &lease,
            GoalOwnerAdmissionTerminalOutcome::Succeeded,
            GoalOwnerAdmissionTerminalDisposition::None,
        )
        .await
        .expect("finish successful admission");

    let mut later_identity = identity(thread_id, InferenceRequestKind::RemoteCompact);
    later_identity.configured_provider_key = "different-provider-map-key".to_string();
    later_identity.configured_requested_model = Some("different-configured-model".to_string());
    later_identity.effective_provider_id = "fallback-provider".to_string();
    later_identity.effective_model = "fallback-model".to_string();
    let decision = broker
        .admit(&later_identity, /*continuation_authority*/ None)
        .await
        .expect("succeeded admission is unrestricted");
    assert!(matches!(
        decision,
        ModelRequestAdmissionDecision::Unrestricted
    ));
    let calls = AtomicUsize::new(0);
    fake_unary_compact_request(&decision, &calls)
        .await
        .expect("unrestricted successor can call provider");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let continuation_decision = broker
        .admit(
            &identity(thread_id, InferenceRequestKind::Turn),
            Some(&fenced(GoalOwnerContinuation::new(
                record.continuation_authority(),
            ))),
        )
        .await
        .expect("evaluate settled continuation");
    assert!(matches!(
        continuation_decision,
        ModelRequestAdmissionDecision::Dormant
    ));
}

#[tokio::test]
async fn continuation_token_without_active_record_fails_closed() {
    let (_home, state_db) = runtime().await;
    let broker = ModelRequestAdmissionBroker::new(Some(state_db));
    let store = broker
        .state_db
        .as_ref()
        .expect("state runtime")
        .goal_owner_admissions();
    let thread_id = ThreadId::new();
    let record = store
        .observe_denial(&observation(
            thread_id,
            "retired-before-continuation",
            GoalOwnerAdmissionPhase::Pending,
            Utc::now() - Duration::seconds(1),
            InferenceRequestKind::Turn,
        ))
        .await
        .expect("record eligible admission");
    let lease = match store
        .try_acquire(&record.continuation_authority(), Utc::now())
        .await
        .expect("acquire admission")
    {
        GoalOwnerAdmissionAcquireResult::Acquired(lease) => *lease,
        result => panic!("expected eligible lease, got {result:?}"),
    };
    store
        .finish(
            &lease,
            GoalOwnerAdmissionTerminalOutcome::Succeeded,
            GoalOwnerAdmissionTerminalDisposition::None,
        )
        .await
        .expect("finish admission");
    store
        .retire(
            &record.authority,
            codex_state::GoalOwnerAdmissionRetirementReason::Superseded,
        )
        .await
        .expect("retire admission")
        .expect("retirement persists");

    let decision = broker
        .admit(
            &identity(thread_id, InferenceRequestKind::Turn),
            Some(&fenced(GoalOwnerContinuation::new(
                record.continuation_authority(),
            ))),
        )
        .await
        .expect("evaluate missing active generation");
    assert!(matches!(decision, ModelRequestAdmissionDecision::Dormant));
    let calls = AtomicUsize::new(0);
    assert!(fake_stream_request(&decision, &calls).await.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let no_state_broker = ModelRequestAdmissionBroker::new(None);
    let no_state_decision = no_state_broker
        .admit(
            &identity(thread_id, InferenceRequestKind::Turn),
            Some(&fenced(GoalOwnerContinuation::new(
                record.continuation_authority(),
            ))),
        )
        .await
        .expect("evaluate continuation without state runtime");
    assert!(matches!(
        no_state_decision,
        ModelRequestAdmissionDecision::Dormant
    ));
}

#[test]
fn identity_preserves_configured_and_effective_provider_model_values() {
    let thread_id = ThreadId::new();
    for (configured_provider_key, configured_model, effective_provider, effective_model) in [
        ("openai", Some("gpt-5"), "openai", "gpt-5"),
        ("custom-alias", Some("my-alias"), "gateway", "gpt-5.1"),
        ("openai", Some("gpt-5.1"), "fallback-provider", "gpt-5"),
    ] {
        let identity = ModelRequestIdentity::inference(
            thread_id,
            Some(SUCCESSOR_TURN_ID.to_string()),
            InferenceRequestKind::Turn,
            configured_provider_key.to_string(),
            configured_model.map(ToString::to_string),
            effective_provider.to_string(),
            effective_model.to_string(),
            /*service_tier*/ None,
            SessionSource::Cli,
            /*parent_continuity_decision_id*/ None,
            Some(SUCCESSOR_REQUEST_ID.to_string()),
        );
        assert_eq!(identity.configured_provider_key, configured_provider_key);
        assert_eq!(
            identity.configured_requested_model.as_deref(),
            configured_model
        );
        assert_eq!(identity.effective_provider_id, effective_provider);
        assert_eq!(identity.effective_model, effective_model);
    }
}

#[tokio::test]
async fn private_typed_prewarm_bypasses_the_admission_ledger_only_for_generate_false() {
    let (_home, state_db) = runtime().await;
    let broker = ModelRequestAdmissionBroker::new(Some(state_db));
    let identity = ModelRequestIdentity::prewarm(
        ThreadId::new(),
        Some(SUCCESSOR_TURN_ID.to_string()),
        "configured-provider".to_string(),
        Some("configured-model".to_string()),
        "effective-provider".to_string(),
        "effective-model".to_string(),
        /*service_tier*/ None,
        SessionSource::Cli,
    );
    let decision = broker
        .admit(&identity, /*continuation_authority*/ None)
        .await
        .expect("typed prewarm is unrestricted");
    assert!(matches!(
        decision,
        ModelRequestAdmissionDecision::Unrestricted
    ));
}
