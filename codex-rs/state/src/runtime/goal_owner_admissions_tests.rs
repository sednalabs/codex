use super::*;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;

fn observation(
    thread_id: ThreadId,
    goal_id: &str,
    origin_request_id: &str,
    deadline_at: DateTime<Utc>,
    requested_phase: GoalOwnerAdmissionPhase,
) -> GoalOwnerAdmissionObservation {
    GoalOwnerAdmissionObservation {
        thread_id,
        goal_id: goal_id.to_string(),
        origin_turn_id: "turn-1".to_string(),
        origin_request_id: origin_request_id.to_string(),
        denial_class: GoalOwnerAdmissionDenialClass::Capacity,
        deadline_at,
        max_attempts: 2,
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

async fn ensure_thread_goal(runtime: &StateRuntime, thread_id: ThreadId) {
    if runtime
        .thread_goals()
        .get_thread_goal(thread_id)
        .await
        .expect("read test thread goal")
        .is_none()
    {
        runtime
            .thread_goals()
            .replace_thread_goal(
                thread_id,
                "test goal for durable admission",
                crate::ThreadGoalStatus::Active,
                /*token_budget*/ None,
            )
            .await
            .expect("create test thread goal");
    }
}

async fn observe_denial(
    runtime: &StateRuntime,
    observation: &GoalOwnerAdmissionObservation,
) -> anyhow::Result<GoalOwnerAdmissionRecord> {
    ensure_thread_goal(runtime, observation.thread_id).await;
    runtime
        .goal_owner_admissions()
        .observe_denial(observation)
        .await
}

#[tokio::test]
async fn observe_denial_is_idempotent_for_an_exact_replay_and_fences_conflicts() {
    let runtime = runtime().await;
    let thread_id = ThreadId::new();
    let observation = observation(
        thread_id,
        "goal-a",
        "request-a",
        Utc::now(),
        GoalOwnerAdmissionPhase::Pending,
    );

    let first = observe_denial(&runtime, &observation)
        .await
        .expect("record initial denial");
    let replay = observe_denial(&runtime, &observation)
        .await
        .expect("replay initial denial");
    assert_eq!(replay, first);

    let mut conflicting_replay = observation.clone();
    conflicting_replay.denial_class = GoalOwnerAdmissionDenialClass::RateLimited;
    assert!(observe_denial(&runtime, &conflicting_replay).await.is_err());

    let next_observation = observation(
        thread_id,
        "goal-b",
        "request-b",
        Utc::now(),
        GoalOwnerAdmissionPhase::Pending,
    );
    let next = observe_denial(&runtime, &next_observation)
        .await
        .expect("record next denial");
    assert_eq!(next.authority.generation, 2);
    assert_eq!(next.authority.goal_id, "goal-b");
    assert_eq!(next.authority.cancellation_epoch, 0);
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .try_acquire(&first.authority, Utc::now())
            .await
            .expect("reject replaced goal authority"),
        None
    );
}

#[tokio::test]
async fn acquire_requires_current_pending_authority_deadline_and_attempt_budget() {
    let runtime = runtime().await;
    let thread_id = ThreadId::new();
    let future = observation(
        thread_id,
        "goal-a",
        "request-a",
        Utc::now() + chrono::Duration::minutes(1),
        GoalOwnerAdmissionPhase::Pending,
    );
    let future_record = observe_denial(&runtime, &future)
        .await
        .expect("record future admission");
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .try_acquire(&future_record.authority, Utc::now())
            .await
            .expect("evaluate future admission"),
        None
    );

    let ready = observation(
        thread_id,
        "goal-a",
        "request-b",
        Utc::now() - chrono::Duration::seconds(1),
        GoalOwnerAdmissionPhase::Pending,
    );
    let ready_record = observe_denial(&runtime, &ready)
        .await
        .expect("record ready admission");
    let stale_authority = GoalOwnerAdmissionAuthority {
        generation: ready_record.authority.generation - 1,
        ..ready_record.authority.clone()
    };
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .try_acquire(&stale_authority, Utc::now())
            .await
            .expect("reject stale generation"),
        None
    );

    let lease = runtime
        .goal_owner_admissions()
        .try_acquire(&ready_record.authority, Utc::now())
        .await
        .expect("acquire ready admission")
        .expect("admission should acquire");
    assert_eq!(lease.authority, ready_record.authority);
    let in_flight = runtime
        .goal_owner_admissions()
        .get(thread_id)
        .await
        .expect("read acquired admission")
        .expect("acquired admission should persist");
    assert_eq!(
        observe_denial(&runtime, &ready)
            .await
            .expect("replay original denial after acquisition"),
        in_flight
    );
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .try_acquire(&ready_record.authority, Utc::now())
            .await
            .expect("reject duplicate acquire"),
        None
    );
}

#[tokio::test]
async fn outcomes_and_cancellation_are_lease_and_epoch_fenced() {
    let runtime = runtime().await;
    let thread_id = ThreadId::new();
    let first = observe_denial(
        &runtime,
        &observation(
            thread_id,
            "goal-a",
            "request-a",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ),
    )
    .await
    .expect("record first admission");
    let lease = runtime
        .goal_owner_admissions()
        .try_acquire(&first.authority, Utc::now())
        .await
        .expect("acquire first admission")
        .expect("first admission should acquire");
    assert!(
        runtime
            .goal_owner_admissions()
            .open_lease(&lease)
            .await
            .expect("open first admission")
    );
    let completed = runtime
        .goal_owner_admissions()
        .finish(
            &lease,
            GoalOwnerAdmissionTerminalOutcome::Succeeded,
            GoalOwnerAdmissionTerminalDisposition::None,
        )
        .await
        .expect("finish first admission")
        .expect("finish should be accepted");
    let replay = runtime
        .goal_owner_admissions()
        .finish(
            &lease,
            GoalOwnerAdmissionTerminalOutcome::Succeeded,
            GoalOwnerAdmissionTerminalDisposition::None,
        )
        .await
        .expect("replay completed outcome")
        .expect("exact outcome replay should be accepted");
    assert_eq!(replay, completed);
    assert!(
        runtime
            .goal_owner_admissions()
            .finish(
                &lease,
                GoalOwnerAdmissionTerminalOutcome::Rejected,
                GoalOwnerAdmissionTerminalDisposition::None,
            )
            .await
            .is_err()
    );

    let second = observe_denial(
        &runtime,
        &observation(
            thread_id,
            "goal-a",
            "request-b",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ),
    )
    .await
    .expect("record second admission");
    let second_lease = runtime
        .goal_owner_admissions()
        .try_acquire(&second.authority, Utc::now())
        .await
        .expect("acquire second admission")
        .expect("second admission should acquire");
    let cancelled = runtime
        .goal_owner_admissions()
        .cancel(
            &second.authority,
            GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn,
        )
        .await
        .expect("cancel second admission")
        .expect("cancellation should be accepted");
    assert_eq!(cancelled.authority.cancellation_epoch, 1);
    assert_eq!(
        cancelled.terminal_outcome,
        GoalOwnerAdmissionTerminalOutcome::Cancelled
    );
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .finish(
                &second_lease,
                GoalOwnerAdmissionTerminalOutcome::Succeeded,
                GoalOwnerAdmissionTerminalDisposition::None,
            )
            .await
            .expect("reject cancelled lease outcome"),
        None
    );
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .cancel(
                &second.authority,
                GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn,
            )
            .await
            .expect("replay cancellation")
            .expect("exact cancellation replay should be accepted"),
        cancelled
    );
}

#[tokio::test]
async fn cancellation_of_an_acquired_lease_fences_open_before_provider_io() {
    let runtime = runtime().await;
    let thread_id = ThreadId::new();
    let pending = observe_denial(
        &runtime,
        &observation(
            thread_id,
            "goal-acquired-cancel",
            "request-acquired-cancel",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ),
    )
    .await
    .expect("record acquired cancellation admission");
    let lease = runtime
        .goal_owner_admissions()
        .try_acquire(&pending.authority, Utc::now())
        .await
        .expect("acquire admission")
        .expect("admission should acquire");

    let cancelled = runtime
        .goal_owner_admissions()
        .cancel(
            &pending.authority,
            GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn,
        )
        .await
        .expect("cancel acquired admission")
        .expect("acquired cancellation should be accepted");
    assert_eq!(
        cancelled.terminal_outcome,
        GoalOwnerAdmissionTerminalOutcome::Cancelled
    );
    assert_eq!(cancelled.authority.cancellation_epoch, 1);
    assert!(
        !runtime
            .goal_owner_admissions()
            .open_lease(&lease)
            .await
            .expect("cancelled acquired lease cannot open")
    );
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .finish(
                &lease,
                GoalOwnerAdmissionTerminalOutcome::Succeeded,
                GoalOwnerAdmissionTerminalDisposition::None,
            )
            .await
            .expect("cancelled acquired lease cannot finish"),
        None
    );
}

#[tokio::test]
async fn cancellation_of_in_flight_work_is_uncertain_and_not_replayable() {
    let runtime = runtime().await;
    let thread_id = ThreadId::new();
    let pending = observe_denial(
        &runtime,
        &observation(
            thread_id,
            "goal-in-flight-cancel",
            "request-in-flight-cancel",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ),
    )
    .await
    .expect("record in-flight cancellation admission");
    let lease = runtime
        .goal_owner_admissions()
        .try_acquire(&pending.authority, Utc::now())
        .await
        .expect("acquire admission")
        .expect("admission should acquire");
    assert!(
        runtime
            .goal_owner_admissions()
            .open_lease(&lease)
            .await
            .expect("open admission")
    );

    let uncertain = runtime
        .goal_owner_admissions()
        .cancel(
            &pending.authority,
            GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn,
        )
        .await
        .expect("cancel in-flight admission")
        .expect("in-flight cancellation should be accepted");
    assert_eq!(
        uncertain.terminal_outcome,
        GoalOwnerAdmissionTerminalOutcome::Uncertain
    );
    assert_eq!(
        uncertain.deferred_terminal_disposition,
        GoalOwnerAdmissionTerminalDisposition::ManualReview
    );
    assert_eq!(uncertain.authority.cancellation_epoch, 1);
    assert!(
        observe_denial(
            &runtime,
            &observation(
                thread_id,
                "goal-replacement-after-cancel",
                "request-replacement-after-cancel",
                Utc::now(),
                GoalOwnerAdmissionPhase::Pending,
            ),
        )
        .await
        .is_err()
    );
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .cancel(
                &pending.authority,
                GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn,
            )
            .await
            .expect("replay uncertain cancellation"),
        Some(uncertain.clone())
    );
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .finish(
                &lease,
                GoalOwnerAdmissionTerminalOutcome::Succeeded,
                GoalOwnerAdmissionTerminalDisposition::None,
            )
            .await
            .expect("uncertain lease cannot finish"),
        None
    );
}

#[tokio::test]
async fn reopen_terminalizes_only_in_flight_admissions_as_uncertain() {
    let codex_home = unique_temp_dir();
    let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
    let runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("initialize runtime");
    let in_flight_thread = ThreadId::new();
    let in_flight = observe_denial(
        &runtime,
        &observation(
            in_flight_thread,
            "goal-in-flight",
            "request-in-flight",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ),
    )
    .await
    .expect("record in-flight admission");
    let in_flight_lease = runtime
        .goal_owner_admissions()
        .try_acquire(&in_flight.authority, Utc::now())
        .await
        .expect("acquire in-flight admission")
        .expect("in-flight admission should acquire");
    assert!(
        runtime
            .goal_owner_admissions()
            .open_lease(&in_flight_lease)
            .await
            .expect("open in-flight admission")
    );
    assert!(
        observe_denial(
            &runtime,
            &observation(
                in_flight_thread,
                "goal-replacement",
                "request-replacement",
                Utc::now(),
                GoalOwnerAdmissionPhase::Pending,
            )
        )
        .await
        .is_err()
    );

    let dormant_thread = ThreadId::new();
    let dormant = observe_denial(
        &runtime,
        &observation(
            dormant_thread,
            "goal-dormant",
            "request-dormant",
            Utc::now() + chrono::Duration::minutes(1),
            GoalOwnerAdmissionPhase::Dormant,
        ),
    )
    .await
    .expect("record dormant admission");
    let pending_thread = ThreadId::new();
    let pending = observe_denial(
        &runtime,
        &observation(
            pending_thread,
            "goal-pending",
            "request-pending",
            Utc::now() + chrono::Duration::minutes(1),
            GoalOwnerAdmissionPhase::Pending,
        ),
    )
    .await
    .expect("record pending admission");
    runtime.close().await;
    drop(runtime);

    let reopened = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("reopen runtime");
    let recovered = reopened
        .goal_owner_admissions()
        .get(in_flight_thread)
        .await
        .expect("read recovered admission")
        .expect("in-flight admission should persist");
    assert_eq!(recovered.phase, GoalOwnerAdmissionPhase::Terminal);
    assert_eq!(
        recovered.terminal_outcome,
        GoalOwnerAdmissionTerminalOutcome::Uncertain
    );
    assert_eq!(
        recovered.deferred_terminal_disposition,
        GoalOwnerAdmissionTerminalDisposition::ManualReview
    );
    assert!(
        observe_denial(
            &reopened,
            &observation(
                in_flight_thread,
                "goal-replacement",
                "request-replacement",
                Utc::now(),
                GoalOwnerAdmissionPhase::Pending,
            )
        )
        .await
        .is_err()
    );
    assert_eq!(
        reopened
            .goal_owner_admissions()
            .get(dormant_thread)
            .await
            .expect("read dormant admission"),
        Some(dormant)
    );
    assert_eq!(
        reopened
            .goal_owner_admissions()
            .get(pending_thread)
            .await
            .expect("read pending admission"),
        Some(pending)
    );
}

#[tokio::test]
async fn a_second_live_runtime_cannot_recover_the_first_runtime_admission() {
    let codex_home = unique_temp_dir();
    let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
    let owner = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("initialize owning runtime");
    let thread_id = ThreadId::new();
    let pending = observe_denial(
        &owner,
        &observation(
            thread_id,
            "goal-live-runtime",
            "request-live-runtime",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ),
    )
    .await
    .expect("record live runtime admission");
    let lease = owner
        .goal_owner_admissions()
        .try_acquire(&pending.authority, Utc::now())
        .await
        .expect("acquire live runtime admission")
        .expect("live runtime admission should acquire");
    assert!(
        owner
            .goal_owner_admissions()
            .open_lease(&lease)
            .await
            .expect("open live runtime admission")
    );

    let second = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("initialize diagnostic runtime while owner is live");
    let still_in_flight = second
        .goal_owner_admissions()
        .get(thread_id)
        .await
        .expect("read admission from diagnostic runtime")
        .expect("live admission should remain present");
    assert_eq!(still_in_flight.phase, GoalOwnerAdmissionPhase::InFlight);
    assert_eq!(
        still_in_flight,
        owner
            .goal_owner_admissions()
            .get(thread_id)
            .await
            .unwrap()
            .unwrap()
    );

    second.close().await;
    owner.close().await;
    drop(second);
    drop(owner);
}

#[tokio::test]
async fn acquired_open_release_and_reopen_are_exactly_fenced() {
    let runtime = runtime().await;
    let thread_id = ThreadId::new();
    let pending = observe_denial(
        &runtime,
        &observation(
            thread_id,
            "goal-lifecycle",
            "request-lifecycle",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ),
    )
    .await
    .expect("record lifecycle admission");
    let lease = runtime
        .goal_owner_admissions()
        .try_acquire(&pending.authority, Utc::now())
        .await
        .expect("acquire lifecycle admission")
        .expect("lifecycle admission should acquire");
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .get(thread_id)
            .await
            .expect("read acquired admission")
            .expect("acquired admission exists")
            .phase,
        GoalOwnerAdmissionPhase::Acquired
    );
    assert!(
        runtime
            .goal_owner_admissions()
            .release_acquired_lease(&lease)
            .await
            .expect("release acquired admission")
    );
    let restored = runtime
        .goal_owner_admissions()
        .get(thread_id)
        .await
        .expect("read restored admission")
        .expect("restored admission exists");
    assert_eq!(restored.phase, GoalOwnerAdmissionPhase::Pending);
    assert_eq!(restored.attempts_started, 0);
    assert!(
        !runtime
            .goal_owner_admissions()
            .release_acquired_lease(&lease)
            .await
            .expect("stale release is safe")
    );

    let lease = runtime
        .goal_owner_admissions()
        .try_acquire(&restored.authority, Utc::now())
        .await
        .expect("reacquire lifecycle admission")
        .expect("lifecycle admission reacquires");
    assert!(
        runtime
            .goal_owner_admissions()
            .open_lease(&lease)
            .await
            .expect("open lifecycle admission")
    );
    assert!(
        !runtime
            .goal_owner_admissions()
            .open_lease(&lease)
            .await
            .expect("duplicate open is safe")
    );
    assert!(
        runtime
            .goal_owner_admissions()
            .reopen(&lease)
            .await
            .expect("reopen in-flight admission")
    );
    let uncertain = runtime
        .goal_owner_admissions()
        .get(thread_id)
        .await
        .expect("read uncertain admission")
        .expect("uncertain admission exists");
    assert_eq!(uncertain.phase, GoalOwnerAdmissionPhase::Terminal);
    assert_eq!(
        uncertain.terminal_outcome,
        GoalOwnerAdmissionTerminalOutcome::Uncertain
    );
    assert_eq!(
        uncertain.deferred_terminal_disposition,
        GoalOwnerAdmissionTerminalDisposition::ManualReview
    );
}

#[tokio::test]
async fn malformed_input_and_contradictory_database_updates_fail_closed() {
    let runtime = runtime().await;
    let thread_id = ThreadId::new();
    let mut malformed = observation(
        thread_id,
        "goal-a",
        "request-a",
        Utc::now(),
        GoalOwnerAdmissionPhase::Pending,
    );
    malformed.origin_turn_id.clear();
    assert!(
        runtime
            .goal_owner_admissions()
            .observe_denial(&malformed)
            .await
            .is_err()
    );
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .get(thread_id)
            .await
            .expect("read absent admission"),
        None
    );

    let record = observe_denial(
        &runtime,
        &observation(
            thread_id,
            "goal-a",
            "request-a",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ),
    )
    .await
    .expect("record valid admission");
    assert!(
        sqlx::query(
            "UPDATE goal_owner_admissions SET terminal_outcome = 'succeeded' WHERE thread_id = ?",
        )
        .bind(thread_id.to_string())
        .execute(runtime.goal_owner_admissions().pool.as_ref())
        .await
        .is_err()
    );
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .get(thread_id)
            .await
            .expect("read valid admission after rejected update"),
        Some(record)
    );
}

#[tokio::test]
async fn admission_without_a_thread_goal_is_rejected_and_cannot_be_acquired() {
    let runtime = runtime().await;
    let thread_id = ThreadId::new();
    let admission = observation(
        thread_id,
        "goal-without-thread",
        "request-without-thread",
        Utc::now() - chrono::Duration::seconds(1),
        GoalOwnerAdmissionPhase::Pending,
    );
    assert!(
        runtime
            .goal_owner_admissions()
            .observe_denial(&admission)
            .await
            .is_err()
    );
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .get(thread_id)
            .await
            .expect("read rejected admission"),
        None
    );

    ensure_thread_goal(&runtime, thread_id).await;
    let admitted = runtime
        .goal_owner_admissions()
        .observe_denial(&admission)
        .await
        .expect("record admission after creating thread goal");
    sqlx::query("DELETE FROM thread_goals WHERE thread_id = ?")
        .bind(thread_id.to_string())
        .execute(runtime.goal_owner_admissions().pool.as_ref())
        .await
        .expect("delete thread goal");
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .get(thread_id)
            .await
            .expect("read deleted admission"),
        None
    );
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .try_acquire(&admitted.authority, Utc::now())
            .await
            .expect("deleted admission cannot acquire"),
        None
    );
}

#[tokio::test]
async fn requested_phase_contradictions_are_rejected_by_schema_and_acquire_guard() {
    let runtime = runtime().await;
    let thread_id = ThreadId::new();
    let admission = observe_denial(
        &runtime,
        &observation(
            thread_id,
            "goal-phase-guard",
            "request-phase-guard",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ),
    )
    .await
    .expect("record phase guard admission");
    assert!(
        sqlx::query(
            "UPDATE goal_owner_admissions SET requested_phase = 'dormant' WHERE thread_id = ?",
        )
        .bind(thread_id.to_string())
        .execute(runtime.goal_owner_admissions().pool.as_ref())
        .await
        .is_err()
    );
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .try_acquire(&admission.authority, Utc::now())
            .await
            .expect("phase contradiction remains fenced")
            .expect("valid pending admission should acquire")
            .authority,
        admission.authority
    );
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

#[tokio::test]
async fn phase_replays_fail_closed() {
    let runtime = runtime().await;
    for (thread_id, initial, conflicting) in [
        (
            ThreadId::new(),
            GoalOwnerAdmissionPhase::Pending,
            GoalOwnerAdmissionPhase::Dormant,
        ),
        (
            ThreadId::new(),
            GoalOwnerAdmissionPhase::Dormant,
            GoalOwnerAdmissionPhase::Pending,
        ),
    ] {
        let request = observation(thread_id, "goal", "request", Utc::now(), initial);
        observe_denial(&runtime, &request)
            .await
            .expect("record admission");
        let mut conflict = request;
        conflict.phase = conflicting;
        assert!(observe_denial(&runtime, &conflict).await.is_err());
    }
}

#[tokio::test]
async fn origin_history_replays_return_the_current_admission_across_replacements() {
    let runtime = runtime().await;
    let thread_id = ThreadId::new();
    let deadline_at = Utc::now() - chrono::Duration::seconds(1);
    let origin_a = observation(
        thread_id,
        "goal-a",
        "request-a",
        deadline_at,
        GoalOwnerAdmissionPhase::Pending,
    );
    let first = observe_denial(&runtime, &origin_a)
        .await
        .expect("record first origin");
    let origin_b = observation(
        thread_id,
        "goal-b",
        "request-b",
        deadline_at,
        GoalOwnerAdmissionPhase::Pending,
    );
    let pending = observe_denial(&runtime, &origin_b)
        .await
        .expect("replace current admission with second origin");
    assert_eq!(pending.authority.generation, 2);
    assert_eq!(
        observe_denial(&runtime, &origin_a)
            .await
            .expect("replay first origin while second is pending"),
        pending
    );

    let lease = runtime
        .goal_owner_admissions()
        .try_acquire(&pending.authority, Utc::now())
        .await
        .expect("acquire second admission")
        .expect("second admission should acquire");
    assert!(
        runtime
            .goal_owner_admissions()
            .open_lease(&lease)
            .await
            .expect("open second admission")
    );
    let terminal = runtime
        .goal_owner_admissions()
        .finish(
            &lease,
            GoalOwnerAdmissionTerminalOutcome::Succeeded,
            GoalOwnerAdmissionTerminalDisposition::None,
        )
        .await
        .expect("finish second admission")
        .expect("finish should be accepted");
    assert_eq!(
        observe_denial(&runtime, &origin_a)
            .await
            .expect("replay first origin after second is terminal"),
        terminal
    );
    assert_eq!(first.authority.generation, 1);

    let mut conflicting_old_replay = origin_a;
    conflicting_old_replay.max_attempts = 3;
    assert!(
        observe_denial(&runtime, &conflicting_old_replay)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn concurrent_exact_origin_replays_converge_without_duplicate_history() {
    let runtime = runtime().await;
    let thread_id = ThreadId::new();
    let origin = observation(
        thread_id,
        "goal-a",
        "request-a",
        Utc::now(),
        GoalOwnerAdmissionPhase::Pending,
    );
    ensure_thread_goal(&runtime, thread_id).await;
    let store_a = runtime.goal_owner_admissions().clone();
    let store_b = runtime.goal_owner_admissions().clone();
    let (first, second) = tokio::join!(
        store_a.observe_denial(&origin),
        store_b.observe_denial(&origin),
    );
    let first = first.expect("first exact origin observation should succeed");
    let second = second.expect("second exact origin observation should succeed");
    assert_eq!(first, second);
    assert_eq!(first.authority.generation, 1);
    let origins = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM goal_owner_admission_origins WHERE thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_one(runtime.goal_owner_admissions().pool.as_ref())
    .await
    .expect("count origin history rows");
    assert_eq!(origins, 1);
}

#[tokio::test]
async fn concurrent_distinct_origins_are_recorded_and_generation_ordered() {
    let runtime = runtime().await;
    let thread_id = ThreadId::new();
    let origin_a = observation(
        thread_id,
        "goal-a",
        "request-a",
        Utc::now(),
        GoalOwnerAdmissionPhase::Pending,
    );
    let origin_b = observation(
        thread_id,
        "goal-b",
        "request-b",
        Utc::now(),
        GoalOwnerAdmissionPhase::Pending,
    );
    ensure_thread_goal(&runtime, thread_id).await;
    let store_a = runtime.goal_owner_admissions().clone();
    let store_b = runtime.goal_owner_admissions().clone();
    let (first, second) = tokio::join!(
        store_a.observe_denial(&origin_a),
        store_b.observe_denial(&origin_b),
    );
    let first = first.expect("first distinct origin observation should succeed");
    let second = second.expect("second distinct origin observation should succeed");
    let mut generations = [first.authority.generation, second.authority.generation];
    generations.sort_unstable();
    assert_eq!(generations, [1, 2]);

    let current = runtime
        .goal_owner_admissions()
        .get(thread_id)
        .await
        .expect("read current admission")
        .expect("current admission should exist");
    assert_eq!(current.authority.generation, 2);
    let origins = sqlx::query_scalar::<_, String>(
        r#"
SELECT origin_request_id
FROM goal_owner_admission_origins
WHERE thread_id = ?
ORDER BY origin_request_id
        "#,
    )
    .bind(thread_id.to_string())
    .fetch_all(runtime.goal_owner_admissions().pool.as_ref())
    .await
    .expect("read origin history rows");
    assert_eq!(
        origins,
        vec!["request-a".to_string(), "request-b".to_string()]
    );
}

#[tokio::test]
async fn direct_thread_goal_deletion_clears_admission_and_origin_history() {
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
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record admission origin");

    sqlx::query("DELETE FROM thread_goals WHERE thread_id = ?")
        .bind(thread_id.to_string())
        .execute(runtime.goal_owner_admissions().pool.as_ref())
        .await
        .expect("delete thread goal directly");

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
    .expect("count deleted origin history rows");
    assert_eq!(origins, 0);
    assert_eq!(
        runtime
            .goal_owner_admissions()
            .try_acquire(&record.authority, Utc::now())
            .await
            .expect("deleted admission cannot acquire"),
        None
    );
}
