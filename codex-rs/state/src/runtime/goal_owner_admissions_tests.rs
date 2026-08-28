use super::*;
use crate::runtime::test_support::unique_temp_dir;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::sync::Arc;

#[test]
fn canonical_provider_id_normalizes_provider_spelling() {
    assert_eq!(canonical_provider_id("  OpenAI  "), "openai");
    assert_eq!(canonical_provider_id("openai"), "openai");
}

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

enum GoalDbCorruption {
    DeleteOrigin {
        thread_id: String,
        generation: i64,
    },
    SetGoalId {
        thread_id: String,
        generation: i64,
        goal_id: String,
    },
    SetOriginRequestId {
        thread_id: String,
        generation: i64,
        origin_request_id: String,
    },
    SetAdmissionEffectiveModel {
        thread_id: String,
        generation: i64,
        effective_model: String,
    },
    SetOriginEffectiveModel {
        thread_id: String,
        generation: i64,
        effective_model: String,
    },
}

/// Open an isolated, foreign-key-disabled connection only for deliberate
/// integrity-test corruption. All assertions and production reads continue to
/// use the normal state-runtime pool with its foreign-key policy unchanged.
async fn inject_goal_db_corruption(sqlite: &crate::SqliteConfig, corruption: GoalDbCorruption) {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(sqlite.goals_db_path())
                .create_if_missing(false)
                .foreign_keys(false),
        )
        .await
        .expect("open isolated corruption injector");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
            .fetch_one(&pool)
            .await
            .expect("read corruption injector foreign-key mode"),
        0
    );

    match corruption {
        GoalDbCorruption::DeleteOrigin {
            thread_id,
            generation,
        } => {
            sqlx::query(
                "DELETE FROM goal_owner_admission_origins WHERE thread_id = ? AND generation = ?",
            )
            .bind(thread_id)
            .bind(generation)
            .execute(&pool)
            .await
            .expect("inject missing admission origin");
        }
        GoalDbCorruption::SetGoalId {
            thread_id,
            generation,
            goal_id,
        } => {
            sqlx::query(
                "UPDATE goal_owner_admissions SET goal_id = ? WHERE thread_id = ? AND generation = ?",
            )
            .bind(goal_id)
            .bind(thread_id)
            .bind(generation)
            .execute(&pool)
            .await
            .expect("inject wrong goal");
        }
        GoalDbCorruption::SetOriginRequestId {
            thread_id,
            generation,
            origin_request_id,
        } => {
            sqlx::query(
                "UPDATE goal_owner_admissions SET origin_request_id = ? WHERE thread_id = ? AND generation = ?",
            )
            .bind(origin_request_id)
            .bind(thread_id)
            .bind(generation)
            .execute(&pool)
            .await
            .expect("inject mismatched origin request");
        }
        GoalDbCorruption::SetAdmissionEffectiveModel {
            thread_id,
            generation,
            effective_model,
        } => {
            sqlx::query(
                "UPDATE goal_owner_admissions SET effective_model = ? WHERE thread_id = ? AND generation = ?",
            )
            .bind(effective_model)
            .bind(thread_id)
            .bind(generation)
            .execute(&pool)
            .await
            .expect("inject acquired admission corruption");
        }
        GoalDbCorruption::SetOriginEffectiveModel {
            thread_id,
            generation,
            effective_model,
        } => {
            sqlx::query(
                "UPDATE goal_owner_admission_origins SET effective_model = ? WHERE thread_id = ? AND generation = ?",
            )
            .bind(effective_model)
            .bind(thread_id)
            .bind(generation)
            .execute(&pool)
            .await
            .expect("inject in-flight origin corruption");
        }
    }

    pool.close().await;
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
        GoalOwnerAdmissionAcquireResult::Acquired(lease) => *lease,
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
) -> GoalOwnerAdmissionObservation {
    let observation = GoalOwnerAdmissionObservation {
        thread_id,
        goal_id: "goal-history".to_string(),
        origin_turn_id: "turn-history".to_string(),
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
        account_context_fingerprint: None,
        deadline_at: DateTime::<Utc>::from_timestamp_millis(0).expect("epoch timestamp"),
        max_attempts: 1,
        requested_phase: GoalOwnerAdmissionPhase::Pending,
        phase: GoalOwnerAdmissionPhase::Pending,
    };
    let origin = origin_from_observation(&observation, generation);
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
    .bind(origin.thread_id.to_string())
    .bind(&origin.origin_request_id)
    .bind(origin.generation)
    .bind(&origin.goal_id)
    .bind(&origin.origin_turn_id)
    .bind(origin.denial_class.as_str())
    .bind(&origin.configured_provider_key)
    .bind(&origin.requested_model)
    .bind(&origin.effective_provider_id)
    .bind(&origin.effective_model)
    .bind(&origin.intended_request_kind)
    .bind(&origin.successor_turn_id)
    .bind(&origin.logical_successor_request_id)
    .bind(origin.decision_id.to_string())
    .bind(
        origin
            .account_context_fingerprint
            .as_ref()
            .map(GoalOwnerAdmissionAccountContextFingerprint::as_str),
    )
    .bind(origin.deadline_at_ms)
    .bind(origin.max_attempts)
    .bind(origin.requested_phase.as_str())
    .execute(store.pool.as_ref())
    .await
    .expect("insert immutable origin history without an admission");
    observation
}

async fn raw_admission_phase(
    store: &GoalOwnerAdmissionStore,
    authority: &GoalOwnerAdmissionAuthority,
) -> String {
    sqlx::query_scalar(
        "SELECT phase FROM goal_owner_admissions WHERE thread_id = ? AND generation = ?",
    )
    .bind(authority.thread_id.to_string())
    .bind(authority.generation)
    .fetch_one(store.pool.as_ref())
    .await
    .expect("read raw admission phase")
}

async fn raw_admission_attempts(
    store: &GoalOwnerAdmissionStore,
    authority: &GoalOwnerAdmissionAuthority,
) -> i64 {
    sqlx::query_scalar(
        "SELECT attempts_started FROM goal_owner_admissions WHERE thread_id = ? AND generation = ?",
    )
    .bind(authority.thread_id.to_string())
    .bind(authority.generation)
    .fetch_one(store.pool.as_ref())
    .await
    .expect("read raw admission attempts")
}

async fn raw_chain_attempts(
    store: &GoalOwnerAdmissionStore,
    authority: &GoalOwnerAdmissionAuthority,
) -> i64 {
    sqlx::query_scalar(
        "SELECT attempts_started FROM goal_owner_admission_goal_chains WHERE thread_id = ? AND goal_id = ?",
    )
    .bind(authority.thread_id.to_string())
    .bind(&authority.goal_id)
    .fetch_one(store.pool.as_ref())
    .await
    .expect("read raw chain attempts")
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
async fn joined_reader_rejects_wrong_goal_with_a_valid_newer_goal_chain() {
    let codex_home = unique_temp_dir();
    let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
    let runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("initialize state runtime");
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
        .expect("record first generation");
    store
        .retire(
            &first.authority,
            GoalOwnerAdmissionRetirementReason::Superseded,
        )
        .await
        .expect("retire first generation")
        .expect("persist retired first generation");
    let newer = store
        .observe_denial(&observation(
            thread_id,
            "goal-b",
            "request-b",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record newer goal and chain");
    inject_goal_db_corruption(
        &sqlite,
        GoalDbCorruption::SetGoalId {
            thread_id: thread_id.to_string(),
            generation: first.authority.generation,
            goal_id: newer.authority.goal_id.clone(),
        },
    )
    .await;

    assert!(store.get_generation(&first.authority).await.is_err());
    assert!(store.observe_denial(&origin_a).await.is_err());
    assert_eq!(
        store
            .get(thread_id)
            .await
            .expect("read unaffected newer goal"),
        Some(newer)
    );
}

#[tokio::test]
async fn joined_reader_rejects_mismatched_origin_request_and_immutable_evidence() {
    let codex_home = unique_temp_dir();
    let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
    let runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("initialize state runtime");
    let store = runtime.goal_owner_admissions();
    let origin_request_mismatch = store
        .observe_denial(&observation(
            ThreadId::new(),
            "goal-request",
            "request-a",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record origin-request case");
    inject_goal_db_corruption(
        &sqlite,
        GoalDbCorruption::SetOriginRequestId {
            thread_id: origin_request_mismatch.authority.thread_id.to_string(),
            generation: origin_request_mismatch.authority.generation,
            origin_request_id: "request-mismatch".to_string(),
        },
    )
    .await;
    assert!(
        store
            .get(origin_request_mismatch.authority.thread_id)
            .await
            .is_err()
    );

    let evidence_mismatch = store
        .observe_denial(&observation(
            ThreadId::new(),
            "goal-evidence",
            "request-b",
            Utc::now(),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record evidence case");
    sqlx::query(
        "UPDATE goal_owner_admissions SET requested_model = ? WHERE thread_id = ? AND generation = ?",
    )
    .bind("gpt-mismatched")
    .bind(evidence_mismatch.authority.thread_id.to_string())
    .bind(evidence_mismatch.authority.generation)
    .execute(store.pool.as_ref())
    .await
    .expect("inject mismatched immutable evidence");
    assert!(
        store
            .get(evidence_mismatch.authority.thread_id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn isolated_corruption_injection_fails_closed_for_missing_active_origin() {
    let codex_home = unique_temp_dir();
    let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
    let runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("initialize state runtime");
    let store = runtime.goal_owner_admissions();
    let foreign_keys = sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
        .fetch_one(store.pool.as_ref())
        .await
        .expect("read normal runtime foreign-key enforcement mode");
    assert_eq!(foreign_keys, 1);

    let pending = store
        .observe_denial(&observation(
            ThreadId::new(),
            "goal-pending",
            "request-pending",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record pending admission");
    inject_goal_db_corruption(
        &sqlite,
        GoalDbCorruption::DeleteOrigin {
            thread_id: pending.authority.thread_id.to_string(),
            generation: pending.authority.generation,
        },
    )
    .await;
    assert!(store.get(pending.authority.thread_id).await.is_err());
    assert!(
        store
            .try_acquire(&pending.continuation_authority(), Utc::now())
            .await
            .is_err()
    );

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
    let lease = acquire(store, &acquired).await;
    inject_goal_db_corruption(
        &sqlite,
        GoalDbCorruption::DeleteOrigin {
            thread_id: acquired.authority.thread_id.to_string(),
            generation: acquired.authority.generation,
        },
    )
    .await;
    assert!(store.open_lease(&lease).await.is_err());
    assert_eq!(
        raw_admission_phase(store, &acquired.authority).await,
        GoalOwnerAdmissionPhase::Acquired.as_str()
    );
}

#[tokio::test]
async fn corrupted_acquired_admission_cannot_open_provider_work() {
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
    sqlx::query(
        "UPDATE goal_owner_admissions SET effective_model = ? WHERE thread_id = ? AND generation = ?",
    )
    .bind("gpt-corrupted")
    .bind(record.authority.thread_id.to_string())
    .bind(record.authority.generation)
    .execute(store.pool.as_ref())
    .await
    .expect("inject acquired immutable corruption");

    assert!(store.open_lease(&lease).await.is_err());
    assert_eq!(
        raw_admission_phase(store, &record.authority).await,
        GoalOwnerAdmissionPhase::Acquired.as_str()
    );
    assert_eq!(raw_admission_attempts(store, &record.authority).await, 1);
    assert_eq!(raw_chain_attempts(store, &record.authority).await, 1);
}

#[tokio::test]
async fn recovery_rejects_corrupt_acquired_rows_before_mutating_any_affected_row() {
    let codex_home = unique_temp_dir();
    let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
    let runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("initialize state runtime");
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
    let _acquired_lease = acquire(store, &acquired).await;
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
            .expect("open lease")
    );
    inject_goal_db_corruption(
        &sqlite,
        GoalDbCorruption::SetAdmissionEffectiveModel {
            thread_id: acquired.authority.thread_id.to_string(),
            generation: acquired.authority.generation,
            effective_model: "gpt-corrupted".to_string(),
        },
    )
    .await;

    assert!(
        GoalOwnerAdmissionStore::recover_in_flight_on_open(store.pool.as_ref())
            .await
            .is_err()
    );
    assert_eq!(
        raw_admission_phase(store, &acquired.authority).await,
        GoalOwnerAdmissionPhase::Acquired.as_str()
    );
    assert_eq!(raw_admission_attempts(store, &acquired.authority).await, 1);
    assert_eq!(raw_chain_attempts(store, &acquired.authority).await, 1);
    assert_eq!(
        raw_admission_phase(store, &in_flight.authority).await,
        GoalOwnerAdmissionPhase::InFlight.as_str()
    );
}

#[tokio::test]
async fn recovery_rejects_corrupt_in_flight_rows_before_mutating_any_affected_row() {
    let codex_home = unique_temp_dir();
    let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
    let runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("initialize state runtime");
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
    let _acquired_lease = acquire(store, &acquired).await;
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
            .expect("open lease")
    );
    inject_goal_db_corruption(
        &sqlite,
        GoalDbCorruption::SetOriginEffectiveModel {
            thread_id: in_flight.authority.thread_id.to_string(),
            generation: in_flight.authority.generation,
            effective_model: "gpt-corrupted".to_string(),
        },
    )
    .await;

    assert!(
        GoalOwnerAdmissionStore::recover_in_flight_on_open(store.pool.as_ref())
            .await
            .is_err()
    );
    assert_eq!(
        raw_admission_phase(store, &acquired.authority).await,
        GoalOwnerAdmissionPhase::Acquired.as_str()
    );
    assert_eq!(raw_admission_attempts(store, &acquired.authority).await, 1);
    assert_eq!(raw_chain_attempts(store, &acquired.authority).await, 1);
    assert_eq!(
        raw_admission_phase(store, &in_flight.authority).await,
        GoalOwnerAdmissionPhase::InFlight.as_str()
    );
}

#[tokio::test]
async fn dispatch_claim_is_single_owner_and_consumed_by_acquire() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let record = store
        .observe_denial(&observation(
            ThreadId::new(),
            "dispatch-claim",
            "request-dispatch-claim",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record pending admission");
    let authority = record.continuation_authority();
    let fence_identity = GoalOwnerDispatchFenceCapability::fresh();
    let claim_id = store
        .claim_dispatch(&authority, fence_identity, Utc::now())
        .await
        .expect("claim dispatch")
        .expect("claim exact pending generation");
    assert_eq!(
        store
            .claim_dispatch(&authority, fence_identity, Utc::now())
            .await
            .expect("reject second claimant"),
        None
    );
    let claimed = store
        .get_generation(&record.authority)
        .await
        .expect("read claimed generation")
        .expect("claimed generation exists");
    assert_eq!(claimed.dispatch_claim_id, Some(claim_id));
    assert_eq!(claimed.dispatch_fence_id, Some(fence_identity));
    assert!(
        !store
            .release_dispatch_claim(
                &record.authority,
                claim_id,
                GoalOwnerDispatchFenceCapability::fresh(),
            )
            .await
            .expect("reject foreign fence cleanup"),
        "a foreign fence must not release a valid dispatch claim"
    );
    assert!(
        !store
            .release_dispatch_claim(&record.authority, Uuid::now_v7(), fence_identity)
            .await
            .expect("reject stale claimant cleanup")
    );
    assert_eq!(
        store
            .get_generation(&record.authority)
            .await
            .expect("read claim after stale cleanup")
            .expect("generation exists")
            .dispatch_claim_id,
        Some(claim_id)
    );

    assert!(matches!(
        store
            .try_acquire_claimed(
                &authority,
                claim_id,
                GoalOwnerDispatchFenceCapability::fresh(),
                Utc::now(),
            )
            .await
            .expect("reject foreign fence acquisition"),
        GoalOwnerAdmissionAcquireResult::NotCurrent
    ));

    let lease = match store
        .try_acquire_claimed(&authority, claim_id, fence_identity, Utc::now())
        .await
        .expect("consume dispatch claim")
    {
        GoalOwnerAdmissionAcquireResult::Acquired(lease) => *lease,
        result => panic!("expected acquired claimed admission, got {result:?}"),
    };
    let acquired = store
        .get_generation(&record.authority)
        .await
        .expect("read acquired generation")
        .expect("acquired generation exists");
    assert_eq!(acquired.phase, GoalOwnerAdmissionPhase::Acquired);
    assert_eq!(acquired.dispatch_claim_id, None);
    assert!(store.open_lease(&lease).await.expect("open acquired lease"));
}

#[tokio::test]
async fn live_runtime_owner_blocks_competing_startup_recovery() {
    let codex_home = unique_temp_dir();
    let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
    let runtime_a = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("first runtime owns admission recovery");
    let record = runtime_a
        .goal_owner_admissions()
        .observe_denial(&observation(
            ThreadId::new(),
            "live-owner",
            "request-live-owner",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record admission");
    let _lease = acquire(runtime_a.goal_owner_admissions(), &record).await;

    let runtime_b = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("competing runtime may open without recovery authority");
    let persisted = runtime_b
        .goal_owner_admissions()
        .get_generation(&record.authority)
        .await
        .expect("read competing runtime admission")
        .expect("admission remains durable");
    assert_eq!(persisted.phase, GoalOwnerAdmissionPhase::Acquired);
    runtime_b.close().await;
    runtime_a.close().await;
}

#[tokio::test]
async fn read_only_runtime_cannot_self_claim_an_unclaimed_generation() {
    let codex_home = unique_temp_dir();
    let sqlite = crate::SqliteConfig::new_for_testing(codex_home.as_path().abs());
    let runtime_a = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("first runtime owns admission mutations");
    let record = runtime_a
        .goal_owner_admissions()
        .observe_denial(&observation(
            ThreadId::new(),
            "read-only-claim",
            "request-read-only-claim",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record unclaimed pending admission");

    let runtime_b = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("competing runtime opens read-only");
    assert!(!runtime_b.owns_goal_runtime());
    let error = runtime_b
        .goal_owner_admissions()
        .claim_dispatch(
            &record.continuation_authority(),
            GoalOwnerDispatchFenceCapability::fresh(),
            Utc::now(),
        )
        .await
        .expect_err("read-only downstream runtime must not self-claim");
    assert!(error.to_string().contains("runtime owner capability"));
    let unchanged = runtime_a
        .goal_owner_admissions()
        .get_generation(&record.authority)
        .await
        .expect("read exact pending generation")
        .expect("pending generation remains durable");
    assert_eq!(unchanged.dispatch_claim_id, None);
    runtime_b.close().await;
    runtime_a.close().await;
    let _ = tokio::fs::remove_dir_all(codex_home).await;
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
async fn cancelled_generation_retires_after_cancel_commit_without_second_cancel() {
    let runtime = runtime().await;
    let store = runtime.goal_owner_admissions();
    let record = store
        .observe_denial(&observation(
            ThreadId::new(),
            "cancel-then-reconcile",
            "request-cancel-then-reconcile",
            Utc::now() - chrono::Duration::seconds(1),
            GoalOwnerAdmissionPhase::Pending,
        ))
        .await
        .expect("record admission");
    let cancelled = store
        .cancel(
            &record.authority,
            GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn,
        )
        .await
        .expect("cancel admission")
        .expect("persist cancellation");

    let retired = store
        .retire_cancelled_generation(
            &record.authority,
            GoalOwnerAdmissionRetirementReason::Superseded,
        )
        .await
        .expect("reconcile committed cancellation")
        .expect("retirement persists");
    assert_eq!(retired.authority, cancelled.authority);
    assert_eq!(
        retired.terminal_outcome,
        GoalOwnerAdmissionTerminalOutcome::Cancelled
    );
    assert_eq!(
        retired.retirement_reason,
        Some(GoalOwnerAdmissionRetirementReason::Superseded)
    );
    assert!(retired.retired_at.is_some());
    assert!(
        store
            .get(record.authority.thread_id)
            .await
            .expect("read active admission")
            .is_none()
    );

    let replay = store
        .retire_cancelled_generation(
            &record.authority,
            GoalOwnerAdmissionRetirementReason::Superseded,
        )
        .await
        .expect("repeat reconciliation")
        .expect("idempotent retirement replay");
    assert_eq!(replay, retired);
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
async fn same_lease_definitive_outcome_cannot_mutate_uncertainty() {
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

    assert!(
        store
            .finish(
                &lease,
                GoalOwnerAdmissionTerminalOutcome::Succeeded,
                GoalOwnerAdmissionTerminalDisposition::None,
            )
            .await
            .is_err()
    );
    let preserved = store
        .get_generation(&record.authority)
        .await
        .expect("read exact history")
        .expect("history persists");
    assert_eq!(preserved.phase, GoalOwnerAdmissionPhase::Terminal);
    assert_eq!(
        preserved.terminal_outcome,
        GoalOwnerAdmissionTerminalOutcome::Uncertain
    );
    assert_eq!(
        preserved.deferred_terminal_disposition,
        GoalOwnerAdmissionTerminalDisposition::ManualReview
    );
    assert_eq!(preserved.lease_id, Some(lease.lease_id));
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
    assert_eq!(persisted, *exhausted);
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
    let historic =
        insert_origin_history_only(store, thread_id, "request-history", /*generation*/ 7).await;
    assert!(store.observe_denial(&historic).await.is_err());

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
    let _historic =
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
