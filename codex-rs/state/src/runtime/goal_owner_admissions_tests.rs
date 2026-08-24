use super::*;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;

fn fingerprint() -> GoalOwnerAdmissionAccountContextFingerprint {
    GoalOwnerAdmissionAccountContextFingerprint::try_from("a".repeat(64))
        .expect("canonical fingerprint")
}

fn observation(
    thread_id: ThreadId,
    goal_id: &str,
    origin_request_id: &str,
    deadline_at: DateTime<Utc>,
    requested_phase: GoalOwnerAdmissionPhase,
) -> GoalOwnerAdmissionObservation {
    observation_with_max(
        thread_id,
        goal_id,
        origin_request_id,
        deadline_at,
        requested_phase,
        /*max_attempts*/ 1,
    )
}

fn observation_with_max(
    thread_id: ThreadId,
    goal_id: &str,
    origin_request_id: &str,
    deadline_at: DateTime<Utc>,
    requested_phase: GoalOwnerAdmissionPhase,
    max_attempts: i64,
) -> GoalOwnerAdmissionObservation {
    GoalOwnerAdmissionObservation {
        thread_id,
        goal_id: goal_id.to_string(),
        origin_turn_id: "turn-1".to_string(),
        origin_request_id: origin_request_id.to_string(),
        denial_class: GoalOwnerAdmissionDenialClass::Capacity,
        configured_provider_key: Some("openai".to_string()),
        requested_model: Some("gpt-5".to_string()),
        effective_provider_id: Some("openai".to_string()),
        effective_model: None,
        intended_request_kind: "turn".to_string(),
        successor_turn_id: "turn-successor".to_string(),
        logical_successor_request_id: format!("successor-{origin_request_id}"),
        decision_id: Uuid::now_v7(),
        account_context_fingerprint: Some(fingerprint()),
        deadline_at,
        max_attempts,
        requested_phase,
        phase: requested_phase,
    }
}

async fn runtime() -> Arc<StateRuntime> {
    let codex_home = unique_temp_dir();
    StateRuntime::init(
        crate::SqliteConfig::new_for_testing(codex_home.as_path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("initialize state runtime")
}

async fn acquire(
    store: &GoalOwnerAdmissionStore,
    record: &GoalOwnerAdmissionRecord,
) -> GoalOwnerAdmissionLease {
    let authority = record.continuation_authority();
    match store
        .try_acquire(&authority, Utc::now())
        .await
        .expect("try to acquire admission")
    {
        GoalOwnerAdmissionAcquireResult::Acquired(lease) => lease,
        result => panic!("expected acquired admission, got {result:?}"),
    }
}

async fn finish_succeeded(
    store: &GoalOwnerAdmissionStore,
    lease: &GoalOwnerAdmissionLease,
) -> GoalOwnerAdmissionRecord {
    store.open_lease(lease).await.expect("open exact lease");
    store
        .finish(
            lease,
            GoalOwnerAdmissionTerminalOutcome::Succeeded,
            GoalOwnerAdmissionTerminalDisposition::None,
        )
        .await
        .expect("finish exact lease")
        .expect("persisted terminal outcome")
}

async fn insert_origin_history_only(
    store: &GoalOwnerAdmissionStore,
    thread_id: ThreadId,
    origin_request_id: &str,
    generation: i64,
) {
    sqlx::query(
        r#"
INSERT INTO goal_owner_admission_origins (
    thread_id, origin_request_id, generation, goal_id, origin_turn_id, denial_class,
    configured_provider_key, requested_model, effective_provider_id, effective_model,
    intended_request_kind, successor_turn_id, logical_successor_request_id, decision_id,
    account_context_fingerprint, deadline_at_ms, max_attempts, requested_phase
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(thread_id.to_string())
    .bind(origin_request_id)
    .bind(generation)
    .bind("goal-history")
    .bind("turn-history")
    .bind("capacity")
    .bind(Some("openai"))
    .bind(Some("gpt-5"))
    .bind(Some("openai"))
    .bind(Option::<&str>::None)
    .bind("turn")
    .bind("turn-successor")
    .bind(format!("successor-{origin_request_id}"))
    .bind(Uuid::now_v7().to_string())
    .bind(Option::<&str>::None)
    .bind(0_i64)
    .bind(1_i64)
    .bind("pending")
    .execute(store.pool.as_ref())
    .await
    .expect("insert immutable origin history without an admission");
}

#[tokio::test]
async fn exact_origin_replay_is_idempotent_and_conflicting_replay_fails_closed() {
    let runtime = runtime().await;
    let thread_id = ThreadId::new();
    let first_observation = observation(
        thread_id,
        "goal-a",
        "request-a",
        Utc::now(),
        GoalOwnerAdmissionPhase::Pending,
    );
    let first = runtime
        .goal_owner_admissions()
        .observe_denial(&first_observation)
        .await
        .expect("record first denial");
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .observe_denial(&first_observation)
            .await
            .expect("replay first denial"),
        first
    );

    let mut conflicting = first_observation;
    conflicting.effective_model = Some("gpt-5.1".to_string());
    assert!(
        runtime
            .goal_owner_admissions()
            .observe_denial(&conflicting)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn exact_origin_replay_returns_its_retired_generation_while_newer_is_active() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let thread_id = ThreadId::new();
    let origin_a = observation(
        thread_id,
        "goal-a",
        "request-a",
        Utc::now() - chrono::Duration::seconds(1),
        GoalOwnerAdmissionPhase::Pending,
    );
    let first = store
        .observe_denial(&origin_a)
        .await
        .expect("record first origin");
    let first_lease = acquire(store, &first).await;
    let first_terminal = finish_succeeded(store, &first_lease).await;
    let first_retired = store
        .retire(
            &first_terminal.authority,
            GoalOwnerAdmissionRetirementReason::Superseded,
        )
        .await
        .expect("retire settled first generation")
        .expect("record retired first generation");
    let second = store
        .observe_denial(&observation(
            thread_id,
            "goal-b",
            "request-b",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record active second generation");

    assert_eq!(
        store.get(thread_id).await.expect("read active generation"),
        Some(second)
    );
    assert_eq!(
        store
            .observe_denial(&origin_a)
            .await
            .expect("replay retired first origin"),
        first_retired
    );
}

#[tokio::test]
async fn exact_origin_replay_fails_closed_when_its_generation_is_missing_while_newer_is_active() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let thread_id = ThreadId::new();
    let origin_a = observation(
        thread_id,
        "goal-a",
        "request-a",
        Utc::now(),
        GoalOwnerAdmissionPhase::Pending,
    );
    let first = store
        .observe_denial(&origin_a)
        .await
        .expect("record first origin");
    store
        .retire(
            &first.authority,
            GoalOwnerAdmissionRetirementReason::Superseded,
        )
        .await
        .expect("retire first generation")
        .expect("record retired first generation");
    let second = store
        .observe_denial(&observation(
            thread_id,
            "goal-b",
            "request-b",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record active second generation");
    sqlx::query("DELETE FROM goal_owner_admissions WHERE thread_id = ? AND generation = ?")
        .bind(thread_id.to_string())
        .bind(first.authority.generation)
        .execute(store.pool.as_ref())
        .await
        .expect("delete referenced historical generation");

    assert_eq!(
        store.get(thread_id).await.expect("read active generation"),
        Some(second)
    );
    assert!(store.observe_denial(&origin_a).await.is_err());
}

#[tokio::test]
async fn exact_origin_replay_fails_closed_when_its_generation_is_malformed_while_newer_is_active() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let thread_id = ThreadId::new();
    let origin_a = observation(
        thread_id,
        "goal-a",
        "request-a",
        Utc::now(),
        GoalOwnerAdmissionPhase::Pending,
    );
    let first = store
        .observe_denial(&origin_a)
        .await
        .expect("record first origin");
    store
        .retire(
            &first.authority,
            GoalOwnerAdmissionRetirementReason::Superseded,
        )
        .await
        .expect("retire first generation")
        .expect("record retired first generation");
    let second = store
        .observe_denial(&observation(
            thread_id,
            "goal-b",
            "request-b",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record active second generation");
    sqlx::query(
        "UPDATE goal_owner_admissions SET updated_at_ms = ? WHERE thread_id = ? AND generation = ?",
    )
    .bind(i64::MAX)
    .bind(thread_id.to_string())
    .bind(first.authority.generation)
    .execute(store.pool.as_ref())
    .await
    .expect("inject malformed referenced historical generation");

    assert_eq!(
        store.get(thread_id).await.expect("read active generation"),
        Some(second)
    );
    assert!(store.observe_denial(&origin_a).await.is_err());
}

#[tokio::test]
async fn malformed_origin_history_fails_closed_before_replay() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let thread_id = ThreadId::new();
    let origin = observation(
        thread_id,
        "goal-a",
        "request-a",
        Utc::now(),
        GoalOwnerAdmissionPhase::Pending,
    );
    store
        .observe_denial(&origin)
        .await
        .expect("record origin history");
    sqlx::query(
        "UPDATE goal_owner_admission_origins SET requested_phase = 'terminal' WHERE thread_id = ? AND origin_request_id = ?",
    )
    .bind(thread_id.to_string())
    .bind(&origin.origin_request_id)
    .execute(store.pool.as_ref())
    .await
    .expect("inject malformed immutable history");
    assert!(store.observe_denial(&origin).await.is_err());
}

#[tokio::test]
async fn cancel_acquired_definite() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let record = store
        .observe_denial(&observation(
            ThreadId::new(),
            "goal-a",
            "request-a",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record admission");
    let lease = acquire(store, &record).await;
    let cancelled = store
        .cancel(
            &record.authority,
            GoalOwnerAdmissionTerminalDisposition::ManualReview,
        )
        .await
        .expect("cancel acquired reservation")
        .expect("persist definite cancellation");
    assert_eq!(cancelled.phase, GoalOwnerAdmissionPhase::Terminal);
    assert_eq!(
        cancelled.terminal_outcome,
        GoalOwnerAdmissionTerminalOutcome::Cancelled
    );
    assert_eq!(
        cancelled.deferred_terminal_disposition,
        GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn
    );
    assert_eq!(cancelled.lease_id, None);
    assert_eq!(cancelled.chain_attempts_started, 1);
    assert_eq!(
        store
            .cancel(
                &record.authority,
                GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn,
            )
            .await
            .expect("replay original definite cancellation"),
        Some(cancelled.clone())
    );
    let mut conflicting_authority = record.authority.clone();
    conflicting_authority.cancellation_epoch += 2;
    assert_eq!(
        store
            .cancel(
                &conflicting_authority,
                GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn,
            )
            .await
            .expect("reject conflicting stale authority"),
        None
    );
    assert!(
        !store
            .open_lease(&lease)
            .await
            .expect("fence opened request")
    );
    assert_eq!(
        store
            .finish(
                &lease,
                GoalOwnerAdmissionTerminalOutcome::Succeeded,
                GoalOwnerAdmissionTerminalDisposition::None,
            )
            .await
            .expect("late pre-open outcome is fenced"),
        None
    );
}

#[tokio::test]
async fn release_acquired_lease_decrements_generation_and_chain_once() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let record = store
        .observe_denial(&observation(
            ThreadId::new(),
            "goal-a",
            "request-a",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record admission");
    let lease = acquire(store, &record).await;
    assert!(
        store
            .release_acquired_lease(&lease)
            .await
            .expect("release pre-network reservation")
    );
    assert!(
        !store
            .release_acquired_lease(&lease)
            .await
            .expect("releasing the same reservation twice is fenced")
    );
    let released = store
        .get_generation(&record.authority)
        .await
        .expect("read released generation")
        .expect("released generation persists");
    assert_eq!(released.phase, GoalOwnerAdmissionPhase::Pending);
    assert_eq!(released.attempts_started, 0);
    assert_eq!(released.chain_attempts_started, 0);
    assert_eq!(released.lease_id, None);
}

#[tokio::test]
async fn retire_rejects_acquired_in_flight_and_uncertain_work() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let acquired = store
        .observe_denial(&observation(
            ThreadId::new(),
            "goal-acquired",
            "request-acquired",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record acquired admission");
    let acquired_lease = acquire(store, &acquired).await;
    assert!(
        store
            .retire(
                &acquired.authority,
                GoalOwnerAdmissionRetirementReason::Superseded,
            )
            .await
            .is_err()
    );
    assert!(
        store
            .release_acquired_lease(&acquired_lease)
            .await
            .expect("release acquired reservation before retirement")
    );
    assert!(
        store
            .retire(
                &acquired.authority,
                GoalOwnerAdmissionRetirementReason::Superseded,
            )
            .await
            .expect("retire released reservation")
            .is_some()
    );

    let in_flight = store
        .observe_denial(&observation(
            ThreadId::new(),
            "goal-in-flight",
            "request-in-flight",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record in-flight admission");
    let in_flight_lease = acquire(store, &in_flight).await;
    assert!(
        store
            .open_lease(&in_flight_lease)
            .await
            .expect("open in-flight lease")
    );
    assert!(
        store
            .retire(
                &in_flight.authority,
                GoalOwnerAdmissionRetirementReason::Superseded,
            )
            .await
            .is_err()
    );
    let uncertain = store
        .cancel(
            &in_flight.authority,
            GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn,
        )
        .await
        .expect("cancel in-flight lease")
        .expect("persist uncertain provider effect");
    assert!(
        store
            .retire(
                &uncertain.authority,
                GoalOwnerAdmissionRetirementReason::UserRecovery,
            )
            .await
            .is_err()
    );
    let settled = store
        .finish(
            &in_flight_lease,
            GoalOwnerAdmissionTerminalOutcome::Succeeded,
            GoalOwnerAdmissionTerminalDisposition::None,
        )
        .await
        .expect("resolve exact uncertain lease")
        .expect("persist definitive outcome");
    assert!(
        store
            .retire(
                &settled.authority,
                GoalOwnerAdmissionRetirementReason::UserRecovery,
            )
            .await
            .expect("retire settled provider evidence")
            .is_some()
    );
}

#[tokio::test]
async fn cancel_in_flight_uncertain() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let record = store
        .observe_denial(&observation(
            ThreadId::new(),
            "goal-a",
            "request-a",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record admission");
    let lease = acquire(store, &record).await;
    assert!(store.open_lease(&lease).await.expect("open exact lease"));

    let cancelled = store
        .cancel(
            &record.authority,
            GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn,
        )
        .await
        .expect("cancel in-flight lease")
        .expect("persist uncertain outcome");
    assert_eq!(cancelled.phase, GoalOwnerAdmissionPhase::Terminal);
    assert_eq!(
        cancelled.terminal_outcome,
        GoalOwnerAdmissionTerminalOutcome::Uncertain
    );
    assert_eq!(
        cancelled.deferred_terminal_disposition,
        GoalOwnerAdmissionTerminalDisposition::ManualReview
    );
    assert_eq!(cancelled.lease_id, Some(lease.lease_id));
    assert_eq!(
        cancelled.lease_cancellation_epoch,
        Some(record.authority.cancellation_epoch)
    );
    assert_eq!(
        cancelled.authority.cancellation_epoch,
        record.authority.cancellation_epoch + 1
    );
    assert_eq!(
        store
            .cancel(
                &record.authority,
                GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn,
            )
            .await
            .expect("replay original in-flight cancellation"),
        Some(cancelled)
    );
}

#[tokio::test]
async fn same_lease_definitive_outcome_refines_uncertainty() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let record = store
        .observe_denial(&observation(
            ThreadId::new(),
            "goal-a",
            "request-a",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record admission");
    let lease = acquire(store, &record).await;
    assert!(store.open_lease(&lease).await.expect("open exact lease"));
    store
        .cancel(
            &record.authority,
            GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn,
        )
        .await
        .expect("record uncertain cancellation");

    let refined = store
        .finish(
            &lease,
            GoalOwnerAdmissionTerminalOutcome::Succeeded,
            GoalOwnerAdmissionTerminalDisposition::None,
        )
        .await
        .expect("finish same late lease")
        .expect("refine uncertainty");
    assert_eq!(refined.phase, GoalOwnerAdmissionPhase::Terminal);
    assert_eq!(
        refined.terminal_outcome,
        GoalOwnerAdmissionTerminalOutcome::Succeeded
    );
    assert_eq!(
        refined.deferred_terminal_disposition,
        GoalOwnerAdmissionTerminalDisposition::None
    );
    assert_eq!(refined.lease_id, Some(lease.lease_id));
}

#[tokio::test]
async fn wrong_lease_generation_or_epoch_cannot_refine_uncertainty() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let record = store
        .observe_denial(&observation(
            ThreadId::new(),
            "goal-a",
            "request-a",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record admission");
    let lease = acquire(store, &record).await;
    assert!(store.open_lease(&lease).await.expect("open exact lease"));
    store
        .cancel(
            &record.authority,
            GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn,
        )
        .await
        .expect("record uncertain cancellation");

    let mut wrong_lease = lease.clone();
    wrong_lease.lease_id = Uuid::now_v7();
    assert_eq!(
        store
            .finish(
                &wrong_lease,
                GoalOwnerAdmissionTerminalOutcome::Succeeded,
                GoalOwnerAdmissionTerminalDisposition::None,
            )
            .await
            .expect("wrong lease is fenced"),
        None
    );
    let mut wrong_epoch = lease.clone();
    wrong_epoch.authority.cancellation_epoch += 1;
    wrong_epoch.continuation_authority.authority = wrong_epoch.authority.clone();
    assert_eq!(
        store
            .finish(
                &wrong_epoch,
                GoalOwnerAdmissionTerminalOutcome::Succeeded,
                GoalOwnerAdmissionTerminalDisposition::None,
            )
            .await
            .expect("wrong epoch is fenced"),
        None
    );
    let mut wrong_generation = lease.clone();
    wrong_generation.authority.generation += 1;
    wrong_generation.continuation_authority.authority = wrong_generation.authority.clone();
    assert_eq!(
        store
            .finish(
                &wrong_generation,
                GoalOwnerAdmissionTerminalOutcome::Succeeded,
                GoalOwnerAdmissionTerminalDisposition::None,
            )
            .await
            .expect("wrong generation is fenced"),
        None
    );
    let preserved = store
        .get_generation(&record.authority)
        .await
        .expect("read exact history")
        .expect("history persists");
    assert_eq!(
        preserved.terminal_outcome,
        GoalOwnerAdmissionTerminalOutcome::Uncertain
    );
}

#[tokio::test]
async fn same_goal_generations_share_one_total_attempt_and_second_claim_exhausts_no_third() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let thread_id = ThreadId::new();
    let first = store
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-a",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record first generation");
    let first_lease = acquire(store, &first).await;
    let terminal = finish_succeeded(store, &first_lease).await;
    assert_eq!(terminal.chain_attempts_started, 1);
    store
        .retire(
            &terminal.authority,
            GoalOwnerAdmissionRetirementReason::Superseded,
        )
        .await
        .expect("retire consumed generation")
        .expect("retirement persists");

    let second = store
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-b",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record same-goal generation");
    assert_eq!(second.authority.generation, 2);
    assert_eq!(second.chain_attempts_started, 1);
    let authority = second.continuation_authority();
    let exhausted = store
        .try_acquire(&authority, Utc::now())
        .await
        .expect("atomically exhaust chain");
    let GoalOwnerAdmissionAcquireResult::Exhausted(exhausted) = exhausted else {
        panic!("second chain claim must exhaust")
    };
    assert_eq!(exhausted.phase, GoalOwnerAdmissionPhase::Terminal);
    assert_eq!(
        exhausted.terminal_outcome,
        GoalOwnerAdmissionTerminalOutcome::Exhausted
    );
    assert_eq!(
        exhausted.deferred_terminal_disposition,
        GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn
    );
    assert_eq!(exhausted.lease_id, None);
    let third = store
        .try_acquire(&authority, Utc::now())
        .await
        .expect("third chain claim reads durable exhaustion");
    assert_eq!(third, GoalOwnerAdmissionAcquireResult::Exhausted(exhausted));
}

#[tokio::test]
async fn restart_does_not_reopen_a_durably_exhausted_chain() {
    let codex_home = unique_temp_dir();
    let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
    let runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("initialize runtime");
    let store = runtime.goal_owner_admissions();
    let thread_id = ThreadId::new();
    let first = store
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-a",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record first generation");
    let first_lease = acquire(store, &first).await;
    let settled = finish_succeeded(store, &first_lease).await;
    store
        .retire(
            &settled.authority,
            GoalOwnerAdmissionRetirementReason::Superseded,
        )
        .await
        .expect("retire settled generation");
    let second = store
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-b",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record exhausted generation");
    let GoalOwnerAdmissionAcquireResult::Exhausted(exhausted) = store
        .try_acquire(&second.continuation_authority(), Utc::now())
        .await
        .expect("exhaust same goal chain")
    else {
        panic!("same goal chain must become durably exhausted")
    };
    runtime.close().await;

    let reopened = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("reopen exhausted runtime");
    let persisted = reopened
        .goal_owner_admissions()
        .get(thread_id)
        .await
        .expect("read exhausted current admission")
        .expect("exhausted admission remains current");
    assert_eq!(persisted, exhausted);
    assert_eq!(persisted.chain_attempts_started, 1);
    assert_eq!(persisted.chain_max_attempts, 1);
}

#[tokio::test]
async fn new_goal_has_fresh_budget_only_after_explicit_retirement() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let thread_id = ThreadId::new();
    let original = store
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-a",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record original goal");
    assert!(
        store
            .observe_denial(&observation(
                thread_id,
                "goal-b",
                "request-b",
                Utc::now() - chrono::Duration::seconds(1),
                GoalOwnerAdmissionPhase::Pending,
            ))
            .await
            .is_err()
    );
    store
        .retire(
            &original.authority,
            GoalOwnerAdmissionRetirementReason::Superseded,
        )
        .await
        .expect("retire old goal")
        .expect("old goal retired");
    let replacement = store
        .observe_denial(&observation(
            thread_id,
            "goal-b",
            "request-b",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record new goal after retirement");
    assert_eq!(replacement.authority.generation, 2);
    assert_eq!(replacement.chain_attempts_started, 0);
    let replacement_lease = acquire(store, &replacement).await;
    assert_eq!(replacement_lease.authority.goal_id, "goal-b");
}

#[tokio::test]
async fn replacement_retirement_preserves_old_terminal_evidence() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let thread_id = ThreadId::new();
    let original = store
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-a",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record old goal");
    let lease = acquire(store, &original).await;
    assert!(store.open_lease(&lease).await.expect("open old goal"));
    let terminal = store
        .finish(
            &lease,
            GoalOwnerAdmissionTerminalOutcome::Rejected,
            GoalOwnerAdmissionTerminalDisposition::None,
        )
        .await
        .expect("finish old goal")
        .expect("old terminal evidence");
    store
        .retire(
            &terminal.authority,
            GoalOwnerAdmissionRetirementReason::UserRecovery,
        )
        .await
        .expect("retire old terminal")
        .expect("old terminal retired");
    let replacement = store
        .observe_denial(&observation(
            thread_id,
            "goal-b",
            "request-b",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record replacement goal");
    let old = store
        .get_generation(&terminal.authority)
        .await
        .expect("read retired history")
        .expect("old evidence remains");
    assert_eq!(old.phase, GoalOwnerAdmissionPhase::Terminal);
    assert_eq!(
        old.terminal_outcome,
        GoalOwnerAdmissionTerminalOutcome::Rejected
    );
    assert_eq!(
        old.retirement_reason,
        Some(GoalOwnerAdmissionRetirementReason::UserRecovery)
    );
    assert!(old.retired_at.is_some());
    assert_eq!(replacement.authority.generation, 2);
}

#[tokio::test]
async fn acquired_reopen_decrements_generation_and_chain_counts_once() {
    let codex_home = unique_temp_dir();
    let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
    let runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("initialize runtime");
    let record = runtime
        .goal_owner_admissions()
        .observe_denial(&observation(
            ThreadId::new(),
            "goal-a",
            "request-a",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record pre-network admission");
    let _lease = acquire(runtime.goal_owner_admissions(), &record).await;
    runtime.close().await;

    let reopened = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("reopen runtime");
    let recovered = reopened
        .goal_owner_admissions()
        .get_generation(&record.authority)
        .await
        .expect("read recovered generation")
        .expect("recovered generation exists");
    assert_eq!(recovered.phase, GoalOwnerAdmissionPhase::Pending);
    assert_eq!(recovered.attempts_started, 0);
    assert_eq!(recovered.chain_attempts_started, 0);
    assert_eq!(recovered.lease_id, None);
    reopened.close().await;

    let reopened_again = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("reopen recovered runtime");
    let recovered_again = reopened_again
        .goal_owner_admissions()
        .get_generation(&record.authority)
        .await
        .expect("read recovered generation again")
        .expect("recovered generation remains");
    assert_eq!(recovered_again.attempts_started, 0);
    assert_eq!(recovered_again.chain_attempts_started, 0);
}

#[tokio::test]
async fn in_flight_reopen_is_uncertain_and_preserves_lease() {
    let codex_home = unique_temp_dir();
    let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
    let runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("initialize runtime");
    let record = runtime
        .goal_owner_admissions()
        .observe_denial(&observation(
            ThreadId::new(),
            "goal-a",
            "request-a",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record in-flight admission");
    let lease = acquire(runtime.goal_owner_admissions(), &record).await;
    assert!(
        runtime
            .goal_owner_admissions()
            .open_lease(&lease)
            .await
            .expect("open in-flight lease")
    );
    runtime.close().await;

    let reopened = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("reopen runtime");
    let recovered = reopened
        .goal_owner_admissions()
        .get_generation(&record.authority)
        .await
        .expect("read recovered in-flight generation")
        .expect("recovered in-flight generation exists");
    assert_eq!(recovered.phase, GoalOwnerAdmissionPhase::Terminal);
    assert_eq!(
        recovered.terminal_outcome,
        GoalOwnerAdmissionTerminalOutcome::Uncertain
    );
    assert_eq!(
        recovered.deferred_terminal_disposition,
        GoalOwnerAdmissionTerminalDisposition::ManualReview
    );
    assert_eq!(recovered.lease_id, Some(lease.lease_id));
    assert_eq!(recovered.chain_attempts_started, 1);
}

#[tokio::test]
async fn typed_acquisition_variants_are_unambiguous() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let thread_id = ThreadId::new();
    let dormant = store
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-a",
            Utc::now(),
            GoalOwnerAdmissionPhase::Dormant,
        ))
        .await
        .expect("record dormant admission");
    assert_eq!(
        store
            .try_acquire(&dormant.continuation_authority(), Utc::now())
            .await
            .expect("read dormant result"),
        GoalOwnerAdmissionAcquireResult::Dormant
    );
    store
        .retire(
            &dormant.authority,
            GoalOwnerAdmissionRetirementReason::UserRecovery,
        )
        .await
        .expect("retire dormant admission");
    assert_eq!(
        store
            .try_acquire(&dormant.continuation_authority(), Utc::now())
            .await
            .expect("read non-current result"),
        GoalOwnerAdmissionAcquireResult::NotCurrent
    );

    let deferred = store
        .observe_denial(&observation(
            thread_id,
            "goal-b",
            "request-b",
            Utc::now() + chrono::Duration::minutes(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record deferred admission");
    assert_eq!(
        store
            .try_acquire(&deferred.continuation_authority(), Utc::now())
            .await
            .expect("read not eligible result"),
        GoalOwnerAdmissionAcquireResult::NotEligible
    );
    store
        .retire(
            &deferred.authority,
            GoalOwnerAdmissionRetirementReason::Superseded,
        )
        .await
        .expect("retire deferred admission");
    let ready = store
        .observe_denial(&observation(
            thread_id,
            "goal-c",
            "request-c",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record ready admission");
    assert!(matches!(
        store
            .try_acquire(&ready.continuation_authority(), Utc::now())
            .await
            .expect("read acquired result"),
        GoalOwnerAdmissionAcquireResult::Acquired(_)
    ));
}

#[tokio::test]
async fn concurrent_claims_converge_on_one_durable_exhaustion() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let thread_id = ThreadId::new();
    let first = store
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-a",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record first generation");
    let first_lease = acquire(store, &first).await;
    let terminal = finish_succeeded(store, &first_lease).await;
    store
        .retire(
            &terminal.authority,
            GoalOwnerAdmissionRetirementReason::Superseded,
        )
        .await
        .expect("retire consumed generation");
    let second = store
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-b",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record exhausted generation");
    let authority = second.continuation_authority();
    let store_a = store.clone();
    let store_b = store.clone();
    let (first_result, second_result) = tokio::join!(
        store_a.try_acquire(&authority, Utc::now()),
        store_b.try_acquire(&authority, Utc::now()),
    );
    for result in [
        first_result.expect("first concurrent claim"),
        second_result.expect("second concurrent claim"),
    ] {
        assert!(matches!(
            result,
            GoalOwnerAdmissionAcquireResult::Exhausted(_)
        ));
    }
    let current = store
        .get(thread_id)
        .await
        .expect("read current exhaustion")
        .expect("current exhausted generation");
    assert_eq!(current.phase, GoalOwnerAdmissionPhase::Terminal);
    assert_eq!(
        current.terminal_outcome,
        GoalOwnerAdmissionTerminalOutcome::Exhausted
    );
}

#[tokio::test]
async fn malformed_and_contradictory_rows_fail_closed() {
    assert!(
        GoalOwnerAdmissionAccountContextFingerprint::try_from("user@example.com".to_string())
            .is_err()
    );
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let thread_id = ThreadId::new();
    let mut malformed = observation(
        thread_id,
        "goal-a",
        "request-a",
        Utc::now(),
        GoalOwnerAdmissionPhase::Pending,
    );
    malformed.origin_turn_id.clear();
    assert!(store.observe_denial(&malformed).await.is_err());
    assert_eq!(store.get(thread_id).await.expect("read absent row"), None);

    let record = store
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-a",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record valid admission");
    assert!(
        sqlx::query(
            "UPDATE goal_owner_admissions SET terminal_outcome = 'succeeded' WHERE thread_id = ? AND generation = ?",
        )
        .bind(thread_id.to_string())
        .bind(record.authority.generation)
        .execute(store.pool.as_ref())
        .await
        .is_err()
    );
    assert!(
        sqlx::query(
            "UPDATE goal_owner_admissions SET lease_cancellation_epoch = 9 WHERE thread_id = ? AND generation = ?",
        )
        .bind(thread_id.to_string())
        .bind(record.authority.generation)
        .execute(store.pool.as_ref())
        .await
        .is_err()
    );
    assert_eq!(
        store.get(thread_id).await.expect("read protected row"),
        Some(record)
    );
}

#[tokio::test]
async fn deletion_clears_active_history_and_goal_chain() {
    let runtime = runtime().await;
    let thread_id = ThreadId::new();
    runtime
        .thread_goals()
        .replace_thread_goal(
            thread_id,
            "preserve no retry admission after deletion",
            crate::ThreadGoalStatus::Active,
            /*token_budget*/ None,
        )
        .await
        .expect("record thread goal");
    let record = runtime
        .goal_owner_admissions()
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-a",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record admission");
    sqlx::query("DELETE FROM thread_goals WHERE thread_id = ?")
        .bind(thread_id.to_string())
        .execute(runtime.goal_owner_admissions().pool.as_ref())
        .await
        .expect("delete thread goal");
    let repeat_delete = sqlx::query("DELETE FROM thread_goals WHERE thread_id = ?")
        .bind(thread_id.to_string())
        .execute(runtime.goal_owner_admissions().pool.as_ref())
        .await
        .expect("repeat canonical deletion");
    assert_eq!(repeat_delete.rows_affected(), 0);
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .get(thread_id)
            .await
            .expect("read deleted admission"),
        None
    );
    let origins = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM goal_owner_admission_origins WHERE thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_one(runtime.goal_owner_admissions().pool.as_ref())
    .await
    .expect("count deleted origins");
    let chains = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM goal_owner_admission_goal_chains WHERE thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_one(runtime.goal_owner_admissions().pool.as_ref())
    .await
    .expect("count deleted chains");
    assert_eq!(origins, 0);
    assert_eq!(chains, 0);
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .try_acquire(&record.continuation_authority(), Utc::now())
            .await
            .expect("deleted authority result"),
        GoalOwnerAdmissionAcquireResult::NotCurrent
    );
}

#[tokio::test]
async fn direct_last_admission_delete_cleans_history_without_a_thread_goal() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let thread_id = ThreadId::new();
    let original = store
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-a",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record admission without thread goal");
    sqlx::query("DELETE FROM goal_owner_admissions WHERE thread_id = ?")
        .bind(thread_id.to_string())
        .execute(store.pool.as_ref())
        .await
        .expect("delete last direct admission");
    let origins = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM goal_owner_admission_origins WHERE thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_one(store.pool.as_ref())
    .await
    .expect("count direct-delete origins");
    let chains = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM goal_owner_admission_goal_chains WHERE thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_one(store.pool.as_ref())
    .await
    .expect("count direct-delete chains");
    assert_eq!(origins, 0);
    assert_eq!(chains, 0);

    let fresh = store
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-b",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("observe fresh admission after direct cleanup");
    assert_eq!(fresh.authority.generation, 1);
    assert_eq!(fresh.chain_attempts_started, 0);
    assert_eq!(
        store
            .try_acquire(&original.continuation_authority(), Utc::now())
            .await
            .expect("old direct-deleted authority is fenced"),
        GoalOwnerAdmissionAcquireResult::NotCurrent
    );
}

#[tokio::test]
async fn direct_single_generation_delete_preserves_remaining_history_and_chain() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let thread_id = ThreadId::new();
    let first = store
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-a",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record first generation");
    store
        .retire(
            &first.authority,
            GoalOwnerAdmissionRetirementReason::Superseded,
        )
        .await
        .expect("retire first generation");
    let second = store
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-b",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record second generation");
    sqlx::query("DELETE FROM goal_owner_admissions WHERE thread_id = ? AND generation = ?")
        .bind(thread_id.to_string())
        .bind(first.authority.generation)
        .execute(store.pool.as_ref())
        .await
        .expect("delete one historical generation");
    let admissions = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM goal_owner_admissions WHERE thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_one(store.pool.as_ref())
    .await
    .expect("count remaining admissions");
    let origins = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM goal_owner_admission_origins WHERE thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_one(store.pool.as_ref())
    .await
    .expect("count preserved origins");
    let chains = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM goal_owner_admission_goal_chains WHERE thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_one(store.pool.as_ref())
    .await
    .expect("count preserved chains");
    assert_eq!(admissions, 1);
    assert_eq!(origins, 2);
    assert_eq!(chains, 1);
    assert_eq!(
        store
            .get(thread_id)
            .await
            .expect("read remaining generation"),
        Some(second)
    );
}

#[tokio::test]
async fn deleting_highest_admission_generation_allocates_after_preserved_origin_history() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let thread_id = ThreadId::new();
    let first = store
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-a",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record lower generation");
    store
        .retire(
            &first.authority,
            GoalOwnerAdmissionRetirementReason::Superseded,
        )
        .await
        .expect("retire lower generation")
        .expect("record retired lower generation");
    let second = store
        .observe_denial(&observation(
            thread_id,
            "goal-b",
            "request-b",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record highest generation");
    sqlx::query("DELETE FROM goal_owner_admissions WHERE thread_id = ? AND generation = ?")
        .bind(thread_id.to_string())
        .bind(second.authority.generation)
        .execute(store.pool.as_ref())
        .await
        .expect("delete highest admission while lower history remains");
    let highest_history = sqlx::query_scalar::<_, i64>(
        "SELECT MAX(generation) FROM goal_owner_admission_origins WHERE thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_one(store.pool.as_ref())
    .await
    .expect("read preserved highest origin generation");
    assert_eq!(highest_history, second.authority.generation);

    let third = store
        .observe_denial(&observation(
            thread_id,
            "goal-c",
            "request-c",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("allocate after preserved origin history");
    assert_eq!(third.authority.generation, 3);
}

#[tokio::test]
async fn origin_only_history_sets_the_next_admission_generation() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let thread_id = ThreadId::new();
    insert_origin_history_only(store, thread_id, "request-history", 7).await;

    let record = store
        .observe_denial(&observation(
            thread_id,
            "goal-current",
            "request-current",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("allocate after origin-only history");
    assert_eq!(record.authority.generation, 8);
}

#[tokio::test]
async fn origin_history_generation_overflow_fails_closed() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let thread_id = ThreadId::new();
    insert_origin_history_only(store, thread_id, "request-overflow", i64::MAX).await;

    assert!(
        store
            .observe_denial(&observation(
                thread_id,
                "goal-current",
                "request-current",
                Utc::now(),
                GoalOwnerAdmissionPhase::Pending,
            ))
            .await
            .is_err()
    );
    let admissions = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM goal_owner_admissions WHERE thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_one(store.pool.as_ref())
    .await
    .expect("count failed allocation admissions");
    let chains = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM goal_owner_admission_goal_chains WHERE thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_one(store.pool.as_ref())
    .await
    .expect("count failed allocation chains");
    assert_eq!(admissions, 0);
    assert_eq!(chains, 0);
}

#[test]
fn admission_timestamps_preserve_milliseconds_without_legacy_seconds_inference() {
    let pre_2020 = DateTime::<Utc>::from_timestamp(946_684_800, 987_654_321).expect("timestamp");
    let stored = admission_datetime_to_epoch_millis(pre_2020);
    assert_eq!(stored, 946_684_800_987);
    assert_eq!(
        admission_epoch_millis_to_datetime(stored).expect("decode timestamp"),
        DateTime::<Utc>::from_timestamp_millis(stored).expect("millisecond timestamp")
    );
}
