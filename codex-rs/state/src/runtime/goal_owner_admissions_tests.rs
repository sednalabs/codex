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
    phase: GoalOwnerAdmissionPhase,
) -> GoalOwnerAdmissionObservation {
    GoalOwnerAdmissionObservation {
        thread_id,
        goal_id: goal_id.to_string(),
        origin_turn_id: "turn-1".to_string(),
        origin_request_id: origin_request_id.to_string(),
        denial_class: GoalOwnerAdmissionDenialClass::Capacity,
        provider_id: Some("openai".to_string()),
        requested_model: Some("gpt-5".to_string()),
        effective_model: None,
        account_context_fingerprint: Some(fingerprint()),
        deadline_at,
        max_attempts: 2,
        phase,
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

    let first = runtime
        .goal_owner_admissions()
        .observe_denial(&observation)
        .await
        .expect("record initial denial");
    let replay = runtime
        .goal_owner_admissions()
        .observe_denial(&observation)
        .await
        .expect("replay initial denial");
    assert_eq!(replay, first);

    let mut conflicting_replay = observation.clone();
    conflicting_replay.denial_class = GoalOwnerAdmissionDenialClass::RateLimited;
    assert!(
        runtime
            .goal_owner_admissions()
            .observe_denial(&conflicting_replay)
            .await
            .is_err()
    );

    let next_observation = observation(
        thread_id,
        "goal-b",
        "request-b",
        Utc::now(),
        GoalOwnerAdmissionPhase::Pending,
    );
    let next = runtime
        .goal_owner_admissions()
        .observe_denial(&next_observation)
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
    let future_record = runtime
        .goal_owner_admissions()
        .observe_denial(&future)
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
    let ready_record = runtime
        .goal_owner_admissions()
        .observe_denial(&ready)
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
        runtime
            .goal_owner_admissions()
            .observe_denial(&ready)
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
    let first = runtime
        .goal_owner_admissions()
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-a",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record first admission");
    let lease = runtime
        .goal_owner_admissions()
        .try_acquire(&first.authority, Utc::now())
        .await
        .expect("acquire first admission")
        .expect("first admission should acquire");
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

    let second = runtime
        .goal_owner_admissions()
        .observe_denial(&observation(
            thread_id,
            "goal-a",
            "request-b",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
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
async fn reopen_terminalizes_only_in_flight_admissions_as_uncertain() {
    let codex_home = unique_temp_dir();
    let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
    let runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("initialize runtime");
    let in_flight_thread = ThreadId::new();
    let in_flight = runtime
        .goal_owner_admissions()
        .observe_denial(&observation(
            in_flight_thread,
            "goal-in-flight",
            "request-in-flight",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record in-flight admission");
    runtime
        .goal_owner_admissions()
        .try_acquire(&in_flight.authority, Utc::now())
        .await
        .expect("acquire in-flight admission")
        .expect("in-flight admission should acquire");
    assert!(
        runtime
            .goal_owner_admissions()
            .observe_denial(&observation(
                in_flight_thread,
                "goal-replacement",
                "request-replacement",
                Utc::now(),
                GoalOwnerAdmissionPhase::Pending,
            ))
            .await
            .is_err()
    );

    let dormant_thread = ThreadId::new();
    let dormant = runtime
        .goal_owner_admissions()
        .observe_denial(&observation(
            dormant_thread,
            "goal-dormant",
            "request-dormant",
            Utc::now() + chrono::Duration::minutes(1),
            GoalOwnerAdmissionPhase::Dormant,
        ))
        .await
        .expect("record dormant admission");
    let pending_thread = ThreadId::new();
    let pending = runtime
        .goal_owner_admissions()
        .observe_denial(&observation(
            pending_thread,
            "goal-pending",
            "request-pending",
            Utc::now() + chrono::Duration::minutes(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record pending admission");
    runtime.close().await;

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
        reopened
            .goal_owner_admissions()
            .observe_denial(&observation(
                in_flight_thread,
                "goal-replacement",
                "request-replacement",
                Utc::now(),
                GoalOwnerAdmissionPhase::Pending,
            ))
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
async fn phase_and_fingerprint_replays_fail_closed() {
    assert!(
        GoalOwnerAdmissionAccountContextFingerprint::try_from("user@example.com".to_string())
            .is_err()
    );
    assert!(GoalOwnerAdmissionAccountContextFingerprint::try_from("a".repeat(63)).is_err());
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
        runtime
            .goal_owner_admissions()
            .observe_denial(&request)
            .await
            .expect("record admission");
        let mut conflict = request;
        conflict.phase = conflicting;
        assert!(
            runtime
                .goal_owner_admissions()
                .observe_denial(&conflict)
                .await
                .is_err()
        );
    }
}
