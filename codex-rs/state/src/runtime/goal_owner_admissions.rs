use super::*;
use anyhow::bail;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use sqlx::Row;
use uuid::Uuid;

const MAX_ORIGIN_ID_LENGTH: usize = 512;
const MAX_EVIDENCE_LENGTH: usize = 512;
const MAX_SUCCESSOR_ID_LENGTH: usize = 512;
const ACCOUNT_CONTEXT_FINGERPRINT_LENGTH: usize = 64;

macro_rules! admission_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name { $($variant),+ }

        impl $name {
            const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $value),+ }
            }
        }

        impl TryFrom<&str> for $name {
            type Error = anyhow::Error;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                match value {
                    $($value => Ok(Self::$variant)),+,
                    _ => bail!("invalid goal-owner admission state value"),
                }
            }
        }
    };
}

/// Exact provider denial recorded before a goal owner may attempt a retry.
admission_enum!(GoalOwnerAdmissionDenialClass {
    Capacity => "capacity",
    RateLimited => "rate_limited",
    ProviderUnavailable => "provider_unavailable",
    PolicyDenied => "policy_denied",
    AuthenticationDenied => "authentication_denied",
});

admission_enum!(GoalOwnerAdmissionPhase {
    Dormant => "dormant",
    Pending => "pending",
    Acquired => "acquired",
    InFlight => "in_flight",
    Terminal => "terminal",
});

admission_enum!(GoalOwnerAdmissionTerminalOutcome {
    None => "none",
    Succeeded => "succeeded",
    Rejected => "rejected",
    Exhausted => "exhausted",
    Cancelled => "cancelled",
    Uncertain => "uncertain",
});

/// Bounded instruction for a terminal admission that must remain deferred.
admission_enum!(GoalOwnerAdmissionTerminalDisposition {
    None => "none",
    AwaitUserTurn => "await_user_turn",
    ManualReview => "manual_review",
});

/// Canonical SHA-256 digest for non-secret account-context correlation evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalOwnerAdmissionAccountContextFingerprint(String);

impl GoalOwnerAdmissionAccountContextFingerprint {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for GoalOwnerAdmissionAccountContextFingerprint {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() != ACCOUNT_CONTEXT_FINGERPRINT_LENGTH
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("invalid goal-owner account-context fingerprint")
        }
        Ok(Self(value))
    }
}

/// Fencing tuple for an exact durable admission generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalOwnerAdmissionAuthority {
    pub thread_id: ThreadId,
    pub goal_id: String,
    pub generation: i64,
    pub cancellation_epoch: i64,
}

/// The one scheduled successor that may consume an admission generation.
///
/// This token is deliberately narrower than a thread-level authority: a
/// scheduler must bind it to the exact logical continuation request before
/// Core may acquire or open a provider request. Core never synthesizes one
/// from ambient thread state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalOwnerAdmissionContinuationAuthority {
    pub authority: GoalOwnerAdmissionAuthority,
    pub intended_request_kind: String,
    pub successor_turn_id: String,
    pub logical_successor_request_id: String,
    pub decision_id: Uuid,
}

/// Immutable denial evidence used to create one durable admission generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalOwnerAdmissionObservation {
    pub thread_id: ThreadId,
    pub goal_id: String,
    pub origin_turn_id: String,
    pub origin_request_id: String,
    pub denial_class: GoalOwnerAdmissionDenialClass,
    /// The configured provider-map key, distinct from the provider ultimately used.
    pub configured_provider_key: Option<String>,
    pub requested_model: Option<String>,
    pub effective_provider_id: Option<String>,
    pub effective_model: Option<String>,
    pub intended_request_kind: String,
    pub successor_turn_id: String,
    pub logical_successor_request_id: String,
    pub decision_id: Uuid,
    /// Optional non-secret SHA-256 account-context correlation evidence.
    pub account_context_fingerprint: Option<GoalOwnerAdmissionAccountContextFingerprint>,
    pub deadline_at: DateTime<Utc>,
    pub max_attempts: i64,
    /// Immutable requested state used to distinguish replays from lifecycle transitions.
    pub requested_phase: GoalOwnerAdmissionPhase,
    pub phase: GoalOwnerAdmissionPhase,
}

/// Reservation returned by an atomic transition from pending to acquired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalOwnerAdmissionLease {
    pub authority: GoalOwnerAdmissionAuthority,
    pub continuation_authority: GoalOwnerAdmissionContinuationAuthority,
    pub lease_id: Uuid,
    pub acquired_at: DateTime<Utc>,
}

/// Fully decoded durable admission state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalOwnerAdmissionRecord {
    pub authority: GoalOwnerAdmissionAuthority,
    pub origin_turn_id: String,
    pub origin_request_id: String,
    pub denial_class: GoalOwnerAdmissionDenialClass,
    pub configured_provider_key: Option<String>,
    pub requested_model: Option<String>,
    pub effective_provider_id: Option<String>,
    pub effective_model: Option<String>,
    pub intended_request_kind: String,
    pub successor_turn_id: String,
    pub logical_successor_request_id: String,
    pub decision_id: Uuid,
    pub account_context_fingerprint: Option<GoalOwnerAdmissionAccountContextFingerprint>,
    pub deadline_at: DateTime<Utc>,
    pub attempts_started: i64,
    pub max_attempts: i64,
    pub requested_phase: GoalOwnerAdmissionPhase,
    pub phase: GoalOwnerAdmissionPhase,
    pub terminal_outcome: GoalOwnerAdmissionTerminalOutcome,
    pub lease_id: Option<Uuid>,
    pub lease_acquired_at: Option<DateTime<Utc>>,
    pub deferred_terminal_disposition: GoalOwnerAdmissionTerminalDisposition,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GoalOwnerAdmissionOrigin {
    goal_id: String,
    origin_turn_id: String,
    origin_request_id: String,
    denial_class: GoalOwnerAdmissionDenialClass,
    configured_provider_key: Option<String>,
    requested_model: Option<String>,
    effective_provider_id: Option<String>,
    effective_model: Option<String>,
    intended_request_kind: String,
    successor_turn_id: String,
    logical_successor_request_id: String,
    decision_id: Uuid,
    account_context_fingerprint: Option<GoalOwnerAdmissionAccountContextFingerprint>,
    deadline_at_ms: i64,
    max_attempts: i64,
    requested_phase: GoalOwnerAdmissionPhase,
}

#[derive(Clone)]
pub struct GoalOwnerAdmissionStore {
    pool: Arc<SqlitePool>,
}

impl GoalOwnerAdmissionStore {
    pub(crate) fn new(pool: Arc<SqlitePool>) -> Self {
        Self { pool }
    }

    /// Recovers a lease based on its durable request-open boundary.
    ///
    /// `acquired` is provably pre-network and can be returned to pending. An
    /// `in_flight` lease may already have reached the provider, so reopen
    /// terminalizes it conservatively instead.
    pub(crate) async fn recover_in_flight_on_open(pool: &SqlitePool) -> anyhow::Result<()> {
        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET phase = 'pending',
    attempts_started = attempts_started - 1,
    lease_id = NULL,
    lease_acquired_at_ms = NULL,
    updated_at_ms = ?
WHERE phase = 'acquired'
            "#,
        )
        .bind(now_ms)
        .execute(pool)
        .await?;
        sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET phase = 'terminal',
    terminal_outcome = 'uncertain',
    deferred_terminal_disposition = 'manual_review',
    updated_at_ms = ?
WHERE phase = 'in_flight'
            "#,
        )
        .bind(now_ms)
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn get(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<GoalOwnerAdmissionRecord>> {
        fetch_record(self.pool.as_ref(), thread_id).await
    }

    /// Insert a denial, or return the current state for an exact origin replay.
    ///
    /// The immutable origin history and mutable current admission are coordinated in
    /// one immediate write transaction. A changed replay with the same origin request
    /// is rejected, even after a later origin has replaced the current admission. A
    /// different origin request advances the generation while preserving the
    /// cancellation epoch.
    pub async fn observe_denial(
        &self,
        observation: &GoalOwnerAdmissionObservation,
    ) -> anyhow::Result<GoalOwnerAdmissionRecord> {
        validate_observation(observation)?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let origin = fetch_origin(
            &mut *transaction,
            observation.thread_id,
            &observation.origin_request_id,
        )
        .await?;
        let existing = fetch_record(&mut *transaction, observation.thread_id).await?;
        if let Some(origin) = origin {
            if !observation_matches_origin(observation, &origin) {
                bail!("conflicting replay for goal-owner admission origin request")
            }
            let current = existing.ok_or_else(|| {
                anyhow::anyhow!(
                    "goal-owner admission origin history exists without a current admission"
                )
            })?;
            transaction.commit().await?;
            return Ok(current);
        }

        let generation = if let Some(existing) = &existing {
            if matches!(
                existing.phase,
                GoalOwnerAdmissionPhase::Acquired | GoalOwnerAdmissionPhase::InFlight
            ) || (existing.phase == GoalOwnerAdmissionPhase::Terminal
                && existing.terminal_outcome == GoalOwnerAdmissionTerminalOutcome::Uncertain
                && existing.deferred_terminal_disposition
                    == GoalOwnerAdmissionTerminalDisposition::ManualReview)
            {
                bail!("goal-owner admission replacement is not authorized for the current state")
            }
            existing
                .authority
                .generation
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("goal-owner admission generation overflow"))?
        } else {
            1
        };

        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        insert_origin(&mut transaction, observation).await?;
        let record = match existing {
            Some(existing) => {
                sqlx::query(REPLACE_ADMISSION_SQL)
                    .bind(&observation.goal_id)
                    .bind(generation)
                    .bind(&observation.origin_turn_id)
                    .bind(&observation.origin_request_id)
                    .bind(observation.denial_class.as_str())
                    .bind(&observation.configured_provider_key)
                    .bind(&observation.requested_model)
                    .bind(&observation.effective_provider_id)
                    .bind(&observation.effective_model)
                    .bind(&observation.intended_request_kind)
                    .bind(&observation.successor_turn_id)
                    .bind(&observation.logical_successor_request_id)
                    .bind(observation.decision_id.to_string())
                    .bind(
                        observation
                            .account_context_fingerprint
                            .as_ref()
                            .map(|value| value.as_str()),
                    )
                    .bind(admission_datetime_to_epoch_millis(observation.deadline_at))
                    .bind(observation.max_attempts)
                    .bind(existing.authority.cancellation_epoch)
                    .bind(observation.requested_phase.as_str())
                    .bind(observation.phase.as_str())
                    .bind(now_ms)
                    .bind(observation.thread_id.to_string())
                    .fetch_one(&mut *transaction)
                    .await?
            }
            None => {
                sqlx::query(INSERT_ADMISSION_SQL)
                    .bind(observation.thread_id.to_string())
                    .bind(&observation.goal_id)
                    .bind(&observation.origin_turn_id)
                    .bind(&observation.origin_request_id)
                    .bind(observation.denial_class.as_str())
                    .bind(&observation.configured_provider_key)
                    .bind(&observation.requested_model)
                    .bind(&observation.effective_provider_id)
                    .bind(&observation.effective_model)
                    .bind(&observation.intended_request_kind)
                    .bind(&observation.successor_turn_id)
                    .bind(&observation.logical_successor_request_id)
                    .bind(observation.decision_id.to_string())
                    .bind(
                        observation
                            .account_context_fingerprint
                            .as_ref()
                            .map(|value| value.as_str()),
                    )
                    .bind(admission_datetime_to_epoch_millis(observation.deadline_at))
                    .bind(observation.max_attempts)
                    .bind(observation.requested_phase.as_str())
                    .bind(observation.phase.as_str())
                    .bind(now_ms)
                    .bind(now_ms)
                    .fetch_one(&mut *transaction)
                    .await?
            }
        };
        transaction.commit().await?;
        record_from_row(&record)
    }

    /// Atomically reserve a deadline-eligible pending admission for one exact successor.
    ///
    /// The resulting `acquired` lease is not provider permission yet. Call
    /// [`Self::open_lease`] immediately before physical network I/O to win the
    /// durable cancellation-versus-open race.
    pub async fn try_acquire(
        &self,
        continuation_authority: &GoalOwnerAdmissionContinuationAuthority,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<GoalOwnerAdmissionLease>> {
        let authority = &continuation_authority.authority;
        validate_authority(authority)?;
        validate_continuation_authority(continuation_authority)?;
        let lease_id = Uuid::now_v7();
        let now_ms = admission_datetime_to_epoch_millis(now);
        let mut transaction = self.pool.begin().await?;
        let record = sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET phase = 'acquired',
    attempts_started = attempts_started + 1,
    lease_id = ?,
    lease_acquired_at_ms = ?,
    updated_at_ms = ?
WHERE thread_id = ?
  AND goal_id = ?
  AND generation = ?
  AND cancellation_epoch = ?
  AND intended_request_kind = ?
  AND successor_turn_id = ?
  AND logical_successor_request_id = ?
  AND decision_id = ?
  AND phase = 'pending'
  AND deadline_at_ms <= ?
  AND attempts_started < max_attempts
RETURNING *
            "#,
        )
        .bind(lease_id.to_string())
        .bind(now_ms)
        .bind(now_ms)
        .bind(authority.thread_id.to_string())
        .bind(&authority.goal_id)
        .bind(authority.generation)
        .bind(authority.cancellation_epoch)
        .bind(&continuation_authority.intended_request_kind)
        .bind(&continuation_authority.successor_turn_id)
        .bind(&continuation_authority.logical_successor_request_id)
        .bind(continuation_authority.decision_id.to_string())
        .bind(now_ms)
        .fetch_optional(&mut *transaction)
        .await?;
        let lease = record
            .map(|row| lease_from_record(record_from_row(&row)?))
            .transpose()?;
        transaction.commit().await?;
        Ok(lease)
    }

    /// Atomically linearize an acquired lease as provider work immediately
    /// before network I/O. A cancellation that commits first leaves this
    /// method with no row, which prohibits the physical request.
    pub async fn open_lease(&self, lease: &GoalOwnerAdmissionLease) -> anyhow::Result<bool> {
        validate_authority(&lease.authority)?;
        validate_continuation_authority(&lease.continuation_authority)?;
        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        let result = sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET phase = 'in_flight',
    updated_at_ms = ?
WHERE thread_id = ?
  AND goal_id = ?
  AND generation = ?
  AND cancellation_epoch = ?
  AND intended_request_kind = ?
  AND successor_turn_id = ?
  AND logical_successor_request_id = ?
  AND decision_id = ?
  AND phase = 'acquired'
  AND lease_id = ?
            "#,
        )
        .bind(now_ms)
        .bind(lease.authority.thread_id.to_string())
        .bind(&lease.authority.goal_id)
        .bind(lease.authority.generation)
        .bind(lease.authority.cancellation_epoch)
        .bind(&lease.continuation_authority.intended_request_kind)
        .bind(&lease.continuation_authority.successor_turn_id)
        .bind(&lease.continuation_authority.logical_successor_request_id)
        .bind(lease.continuation_authority.decision_id.to_string())
        .bind(lease.lease_id.to_string())
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Return a pre-network reservation to pending when request setup fails.
    pub async fn release_acquired_lease(
        &self,
        lease: &GoalOwnerAdmissionLease,
    ) -> anyhow::Result<bool> {
        validate_authority(&lease.authority)?;
        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        let result = sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET phase = 'pending',
    attempts_started = attempts_started - 1,
    lease_id = NULL,
    lease_acquired_at_ms = NULL,
    updated_at_ms = ?
WHERE thread_id = ?
  AND goal_id = ?
  AND generation = ?
  AND cancellation_epoch = ?
  AND phase = 'acquired'
  AND lease_id = ?
            "#,
        )
        .bind(now_ms)
        .bind(lease.authority.thread_id.to_string())
        .bind(&lease.authority.goal_id)
        .bind(lease.authority.generation)
        .bind(lease.authority.cancellation_epoch)
        .bind(lease.lease_id.to_string())
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Finish only the exact in-flight lease. An exact terminal replay is idempotent.
    pub async fn finish(
        &self,
        lease: &GoalOwnerAdmissionLease,
        outcome: GoalOwnerAdmissionTerminalOutcome,
        disposition: GoalOwnerAdmissionTerminalDisposition,
    ) -> anyhow::Result<Option<GoalOwnerAdmissionRecord>> {
        if matches!(
            outcome,
            GoalOwnerAdmissionTerminalOutcome::None | GoalOwnerAdmissionTerminalOutcome::Cancelled
        ) {
            bail!("finish requires a non-cancelled terminal outcome")
        }
        validate_terminal_transition(outcome, disposition)?;
        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET phase = 'terminal',
    terminal_outcome = ?,
    deferred_terminal_disposition = ?,
    updated_at_ms = ?
WHERE thread_id = ?
  AND goal_id = ?
  AND generation = ?
  AND cancellation_epoch = ?
  AND phase = 'in_flight'
  AND lease_id = ?
RETURNING *
            "#,
        )
        .bind(outcome.as_str())
        .bind(disposition.as_str())
        .bind(now_ms)
        .bind(lease.authority.thread_id.to_string())
        .bind(&lease.authority.goal_id)
        .bind(lease.authority.generation)
        .bind(lease.authority.cancellation_epoch)
        .bind(lease.lease_id.to_string())
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(row) = row {
            let record = record_from_row(&row)?;
            transaction.commit().await?;
            return Ok(Some(record));
        }
        transaction.rollback().await?;

        let current = self.get(lease.authority.thread_id).await?;
        match current {
            Some(record) if exact_terminal_replay(&record, lease, outcome, disposition) => {
                Ok(Some(record))
            }
            Some(record) if same_lease(&record, lease) => {
                bail!("conflicting replay for goal-owner admission lease outcome")
            }
            _ => Ok(None),
        }
    }

    /// Increment the cancellation epoch and terminalize the exact durable generation.
    pub async fn cancel(
        &self,
        authority: &GoalOwnerAdmissionAuthority,
        disposition: GoalOwnerAdmissionTerminalDisposition,
    ) -> anyhow::Result<Option<GoalOwnerAdmissionRecord>> {
        validate_authority(authority)?;
        let next_epoch = authority
            .cancellation_epoch
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("goal-owner admission cancellation epoch overflow"))?;
        validate_cancellation_disposition(disposition)?;
        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET phase = 'terminal',
    terminal_outcome = 'cancelled',
    cancellation_epoch = ?,
    deferred_terminal_disposition = ?,
    updated_at_ms = ?
WHERE thread_id = ?
  AND goal_id = ?
  AND generation = ?
  AND cancellation_epoch = ?
  AND phase IN ('dormant', 'pending', 'acquired', 'in_flight')
RETURNING *
            "#,
        )
        .bind(next_epoch)
        .bind(disposition.as_str())
        .bind(now_ms)
        .bind(authority.thread_id.to_string())
        .bind(&authority.goal_id)
        .bind(authority.generation)
        .bind(authority.cancellation_epoch)
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some(row) = row {
            let record = record_from_row(&row)?;
            transaction.commit().await?;
            return Ok(Some(record));
        }
        transaction.rollback().await?;

        let current = self.get(authority.thread_id).await?;
        match current {
            Some(record) if exact_cancellation_replay(&record, authority, disposition) => {
                Ok(Some(record))
            }
            _ => Ok(None),
        }
    }
}

const INSERT_ADMISSION_SQL: &str = r#"
INSERT INTO goal_owner_admissions (
    thread_id, goal_id, generation, origin_turn_id, origin_request_id, denial_class,
    configured_provider_key, requested_model, effective_provider_id, effective_model,
    intended_request_kind, successor_turn_id,
    logical_successor_request_id, decision_id, account_context_fingerprint, deadline_at_ms,
    max_attempts, requested_phase, phase, terminal_outcome, deferred_terminal_disposition, created_at_ms,
    updated_at_ms
) VALUES (
    ?, ?, 1,
    ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
    'none', 'none', ?, ?
)
RETURNING *
"#;

const REPLACE_ADMISSION_SQL: &str = r#"
UPDATE goal_owner_admissions
SET goal_id = ?, generation = ?, origin_turn_id = ?, origin_request_id = ?, denial_class = ?,
    configured_provider_key = ?, requested_model = ?, effective_provider_id = ?, effective_model = ?,
    intended_request_kind = ?, successor_turn_id = ?,
    logical_successor_request_id = ?, decision_id = ?, account_context_fingerprint = ?,
    deadline_at_ms = ?, attempts_started = 0, max_attempts = ?, cancellation_epoch = ?,
    requested_phase = ?, phase = ?, terminal_outcome = 'none', lease_id = NULL, lease_acquired_at_ms = NULL,
    deferred_terminal_disposition = 'none', updated_at_ms = ?
WHERE thread_id = ?
RETURNING *
"#;

async fn insert_origin(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    observation: &GoalOwnerAdmissionObservation,
) -> anyhow::Result<()> {
    let origin = origin_from_observation(observation);
    sqlx::query(
        r#"
INSERT INTO goal_owner_admission_origins (
    thread_id, origin_request_id, goal_id, origin_turn_id, denial_class, configured_provider_key,
    requested_model, effective_provider_id, effective_model, intended_request_kind, successor_turn_id,
    logical_successor_request_id, decision_id, account_context_fingerprint, deadline_at_ms,
    max_attempts, requested_phase
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(observation.thread_id.to_string())
    .bind(&origin.origin_request_id)
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
            .map(|value| value.as_str()),
    )
    .bind(origin.deadline_at_ms)
    .bind(origin.max_attempts)
    .bind(origin.requested_phase.as_str())
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn fetch_record<'e, E>(
    executor: E,
    thread_id: ThreadId,
) -> anyhow::Result<Option<GoalOwnerAdmissionRecord>>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let row = sqlx::query("SELECT * FROM goal_owner_admissions WHERE thread_id = ?")
        .bind(thread_id.to_string())
        .fetch_optional(executor)
        .await?;
    row.map(|row| record_from_row(&row)).transpose()
}

async fn fetch_origin<'e, E>(
    executor: E,
    thread_id: ThreadId,
    origin_request_id: &str,
) -> anyhow::Result<Option<GoalOwnerAdmissionOrigin>>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let row = sqlx::query(
        r#"
SELECT
    goal_id,
    origin_turn_id,
    origin_request_id,
    denial_class,
    configured_provider_key,
    requested_model,
    effective_provider_id,
    effective_model,
    intended_request_kind,
    successor_turn_id,
    logical_successor_request_id,
    decision_id,
    account_context_fingerprint,
    deadline_at_ms,
    max_attempts,
    requested_phase
FROM goal_owner_admission_origins
WHERE thread_id = ? AND origin_request_id = ?
        "#,
    )
    .bind(thread_id.to_string())
    .bind(origin_request_id)
    .fetch_optional(executor)
    .await?;
    row.map(|row| origin_from_row(&row)).transpose()
}

fn record_from_row(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<GoalOwnerAdmissionRecord> {
    let lease_id: Option<String> = row.try_get("lease_id")?;
    let lease_acquired_at_ms: Option<i64> = row.try_get("lease_acquired_at_ms")?;
    let record = GoalOwnerAdmissionRecord {
        authority: GoalOwnerAdmissionAuthority {
            thread_id: ThreadId::try_from(row.try_get::<String, _>("thread_id")?)?,
            goal_id: row.try_get("goal_id")?,
            generation: row.try_get("generation")?,
            cancellation_epoch: row.try_get("cancellation_epoch")?,
        },
        origin_turn_id: row.try_get("origin_turn_id")?,
        origin_request_id: row.try_get("origin_request_id")?,
        denial_class: GoalOwnerAdmissionDenialClass::try_from(
            row.try_get::<String, _>("denial_class")?.as_str(),
        )?,
        configured_provider_key: row.try_get("configured_provider_key")?,
        requested_model: row.try_get("requested_model")?,
        effective_provider_id: row.try_get("effective_provider_id")?,
        effective_model: row.try_get("effective_model")?,
        intended_request_kind: row.try_get("intended_request_kind")?,
        successor_turn_id: row.try_get("successor_turn_id")?,
        logical_successor_request_id: row.try_get("logical_successor_request_id")?,
        decision_id: Uuid::parse_str(&row.try_get::<String, _>("decision_id")?)?,
        account_context_fingerprint: row
            .try_get::<Option<String>, _>("account_context_fingerprint")?
            .map(GoalOwnerAdmissionAccountContextFingerprint::try_from)
            .transpose()?,
        deadline_at: admission_epoch_millis_to_datetime(row.try_get("deadline_at_ms")?)?,
        attempts_started: row.try_get("attempts_started")?,
        max_attempts: row.try_get("max_attempts")?,
        requested_phase: GoalOwnerAdmissionPhase::try_from(
            row.try_get::<String, _>("requested_phase")?.as_str(),
        )?,
        phase: GoalOwnerAdmissionPhase::try_from(row.try_get::<String, _>("phase")?.as_str())?,
        terminal_outcome: GoalOwnerAdmissionTerminalOutcome::try_from(
            row.try_get::<String, _>("terminal_outcome")?.as_str(),
        )?,
        lease_id: lease_id
            .map(|lease_id| Uuid::parse_str(&lease_id))
            .transpose()?,
        lease_acquired_at: lease_acquired_at_ms
            .map(admission_epoch_millis_to_datetime)
            .transpose()?,
        deferred_terminal_disposition: GoalOwnerAdmissionTerminalDisposition::try_from(
            row.try_get::<String, _>("deferred_terminal_disposition")?
                .as_str(),
        )?,
        created_at: admission_epoch_millis_to_datetime(row.try_get("created_at_ms")?)?,
        updated_at: admission_epoch_millis_to_datetime(row.try_get("updated_at_ms")?)?,
    };
    validate_record(&record)?;
    Ok(record)
}

fn origin_from_row(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<GoalOwnerAdmissionOrigin> {
    let origin = GoalOwnerAdmissionOrigin {
        goal_id: row.try_get("goal_id")?,
        origin_turn_id: row.try_get("origin_turn_id")?,
        origin_request_id: row.try_get("origin_request_id")?,
        denial_class: GoalOwnerAdmissionDenialClass::try_from(
            row.try_get::<String, _>("denial_class")?.as_str(),
        )?,
        configured_provider_key: row.try_get("configured_provider_key")?,
        requested_model: row.try_get("requested_model")?,
        effective_provider_id: row.try_get("effective_provider_id")?,
        effective_model: row.try_get("effective_model")?,
        intended_request_kind: row.try_get("intended_request_kind")?,
        successor_turn_id: row.try_get("successor_turn_id")?,
        logical_successor_request_id: row.try_get("logical_successor_request_id")?,
        decision_id: Uuid::parse_str(&row.try_get::<String, _>("decision_id")?)?,
        account_context_fingerprint: row
            .try_get::<Option<String>, _>("account_context_fingerprint")?
            .map(GoalOwnerAdmissionAccountContextFingerprint::try_from)
            .transpose()?,
        deadline_at_ms: row.try_get("deadline_at_ms")?,
        max_attempts: row.try_get("max_attempts")?,
        requested_phase: GoalOwnerAdmissionPhase::try_from(
            row.try_get::<String, _>("requested_phase")?.as_str(),
        )?,
    };
    validate_origin(&origin)?;
    Ok(origin)
}

fn lease_from_record(record: GoalOwnerAdmissionRecord) -> anyhow::Result<GoalOwnerAdmissionLease> {
    if record.phase != GoalOwnerAdmissionPhase::Acquired {
        bail!("acquired goal-owner admission is not reserved")
    }
    Ok(GoalOwnerAdmissionLease {
        continuation_authority: record.continuation_authority(),
        authority: record.authority,
        lease_id: record.lease_id.ok_or_else(|| {
            anyhow::anyhow!("in-flight goal-owner admission is missing its lease")
        })?,
        acquired_at: record.lease_acquired_at.ok_or_else(|| {
            anyhow::anyhow!("in-flight goal-owner admission is missing its lease timestamp")
        })?,
    })
}

impl GoalOwnerAdmissionRecord {
    /// Reconstruct the narrow successor token persisted with this generation.
    pub fn continuation_authority(&self) -> GoalOwnerAdmissionContinuationAuthority {
        GoalOwnerAdmissionContinuationAuthority {
            authority: self.authority.clone(),
            intended_request_kind: self.intended_request_kind.clone(),
            successor_turn_id: self.successor_turn_id.clone(),
            logical_successor_request_id: self.logical_successor_request_id.clone(),
            decision_id: self.decision_id,
        }
    }
}

fn validate_observation(observation: &GoalOwnerAdmissionObservation) -> anyhow::Result<()> {
    validate_origin(&origin_from_observation(observation))?;
    if !matches!(
        observation.phase,
        GoalOwnerAdmissionPhase::Dormant | GoalOwnerAdmissionPhase::Pending
    ) || observation.requested_phase != observation.phase
    {
        bail!("new goal-owner admission must be dormant or pending")
    }
    Ok(())
}

fn origin_from_observation(
    observation: &GoalOwnerAdmissionObservation,
) -> GoalOwnerAdmissionOrigin {
    GoalOwnerAdmissionOrigin {
        goal_id: observation.goal_id.clone(),
        origin_turn_id: observation.origin_turn_id.clone(),
        origin_request_id: observation.origin_request_id.clone(),
        denial_class: observation.denial_class,
        configured_provider_key: observation.configured_provider_key.clone(),
        requested_model: observation.requested_model.clone(),
        effective_provider_id: observation.effective_provider_id.clone(),
        effective_model: observation.effective_model.clone(),
        intended_request_kind: observation.intended_request_kind.clone(),
        successor_turn_id: observation.successor_turn_id.clone(),
        logical_successor_request_id: observation.logical_successor_request_id.clone(),
        decision_id: observation.decision_id,
        account_context_fingerprint: observation.account_context_fingerprint.clone(),
        deadline_at_ms: admission_datetime_to_epoch_millis(observation.deadline_at),
        max_attempts: observation.max_attempts,
        requested_phase: observation.requested_phase,
    }
}

fn validate_origin(origin: &GoalOwnerAdmissionOrigin) -> anyhow::Result<()> {
    validate_nonempty("goal id", &origin.goal_id, MAX_ORIGIN_ID_LENGTH)?;
    validate_nonempty(
        "origin turn id",
        &origin.origin_turn_id,
        MAX_ORIGIN_ID_LENGTH,
    )?;
    validate_nonempty(
        "origin request id",
        &origin.origin_request_id,
        MAX_ORIGIN_ID_LENGTH,
    )?;
    validate_evidence(
        "configured provider key",
        origin.configured_provider_key.as_deref(),
        MAX_EVIDENCE_LENGTH,
    )?;
    validate_evidence(
        "requested model",
        origin.requested_model.as_deref(),
        MAX_EVIDENCE_LENGTH,
    )?;
    validate_evidence(
        "effective provider id",
        origin.effective_provider_id.as_deref(),
        MAX_EVIDENCE_LENGTH,
    )?;
    validate_evidence(
        "effective model",
        origin.effective_model.as_deref(),
        MAX_EVIDENCE_LENGTH,
    )?;
    validate_nonempty(
        "intended request kind",
        &origin.intended_request_kind,
        MAX_SUCCESSOR_ID_LENGTH,
    )?;
    validate_nonempty(
        "successor turn id",
        &origin.successor_turn_id,
        MAX_SUCCESSOR_ID_LENGTH,
    )?;
    validate_nonempty(
        "logical successor request id",
        &origin.logical_successor_request_id,
        MAX_SUCCESSOR_ID_LENGTH,
    )?;
    admission_epoch_millis_to_datetime(origin.deadline_at_ms)?;
    if origin.max_attempts < 1 {
        bail!("goal-owner admission max attempts must be positive")
    }
    if !matches!(
        origin.requested_phase,
        GoalOwnerAdmissionPhase::Dormant | GoalOwnerAdmissionPhase::Pending
    ) {
        bail!("goal-owner admission requested phase must be dormant or pending")
    }
    Ok(())
}

fn validate_authority(authority: &GoalOwnerAdmissionAuthority) -> anyhow::Result<()> {
    validate_nonempty("goal id", &authority.goal_id, MAX_ORIGIN_ID_LENGTH)?;
    if authority.generation < 1 || authority.cancellation_epoch < 0 {
        bail!("invalid goal-owner admission authority")
    }
    Ok(())
}

fn validate_continuation_authority(
    continuation_authority: &GoalOwnerAdmissionContinuationAuthority,
) -> anyhow::Result<()> {
    validate_authority(&continuation_authority.authority)?;
    validate_nonempty(
        "intended request kind",
        &continuation_authority.intended_request_kind,
        MAX_SUCCESSOR_ID_LENGTH,
    )?;
    validate_nonempty(
        "successor turn id",
        &continuation_authority.successor_turn_id,
        MAX_SUCCESSOR_ID_LENGTH,
    )?;
    validate_nonempty(
        "logical successor request id",
        &continuation_authority.logical_successor_request_id,
        MAX_SUCCESSOR_ID_LENGTH,
    )?;
    Ok(())
}

fn admission_datetime_to_epoch_millis(value: DateTime<Utc>) -> i64 {
    value.timestamp_millis()
}

fn admission_epoch_millis_to_datetime(value: i64) -> anyhow::Result<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_millis(value)
        .ok_or_else(|| anyhow::anyhow!("invalid goal-owner admission timestamp millis: {value}"))
}

fn validate_terminal_transition(
    outcome: GoalOwnerAdmissionTerminalOutcome,
    disposition: GoalOwnerAdmissionTerminalDisposition,
) -> anyhow::Result<()> {
    match outcome {
        GoalOwnerAdmissionTerminalOutcome::Succeeded
        | GoalOwnerAdmissionTerminalOutcome::Rejected
        | GoalOwnerAdmissionTerminalOutcome::Exhausted
            if disposition == GoalOwnerAdmissionTerminalDisposition::None =>
        {
            Ok(())
        }
        GoalOwnerAdmissionTerminalOutcome::Uncertain
            if disposition == GoalOwnerAdmissionTerminalDisposition::ManualReview =>
        {
            Ok(())
        }
        _ => bail!("invalid goal-owner admission terminal transition"),
    }
}

fn validate_cancellation_disposition(
    disposition: GoalOwnerAdmissionTerminalDisposition,
) -> anyhow::Result<()> {
    if disposition == GoalOwnerAdmissionTerminalDisposition::None {
        bail!("cancelled goal-owner admission requires a terminal disposition")
    }
    Ok(())
}

fn validate_nonempty(name: &str, value: &str, max_length: usize) -> anyhow::Result<()> {
    if value.is_empty() || value.len() > max_length {
        bail!("invalid goal-owner admission {name}")
    }
    Ok(())
}

fn validate_evidence(name: &str, value: Option<&str>, max_length: usize) -> anyhow::Result<()> {
    if let Some(value) = value {
        validate_nonempty(name, value, max_length)?;
    }
    Ok(())
}

fn validate_record(record: &GoalOwnerAdmissionRecord) -> anyhow::Result<()> {
    validate_authority(&record.authority)?;
    validate_continuation_authority(&record.continuation_authority())?;
    validate_nonempty(
        "origin turn id",
        &record.origin_turn_id,
        MAX_ORIGIN_ID_LENGTH,
    )?;
    validate_nonempty(
        "origin request id",
        &record.origin_request_id,
        MAX_ORIGIN_ID_LENGTH,
    )?;
    validate_evidence(
        "configured provider key",
        record.configured_provider_key.as_deref(),
        MAX_EVIDENCE_LENGTH,
    )?;
    validate_evidence(
        "requested model",
        record.requested_model.as_deref(),
        MAX_EVIDENCE_LENGTH,
    )?;
    validate_evidence(
        "effective provider id",
        record.effective_provider_id.as_deref(),
        MAX_EVIDENCE_LENGTH,
    )?;
    validate_evidence(
        "effective model",
        record.effective_model.as_deref(),
        MAX_EVIDENCE_LENGTH,
    )?;
    if record.attempts_started < 0
        || record.max_attempts < 1
        || record.attempts_started > record.max_attempts
    {
        bail!("invalid goal-owner admission attempt counters")
    }
    let has_lease = record.lease_id.is_some();
    if has_lease != record.lease_acquired_at.is_some() {
        bail!("contradictory goal-owner admission lease state")
    }
    match record.phase {
        GoalOwnerAdmissionPhase::Dormant | GoalOwnerAdmissionPhase::Pending => {
            if record.requested_phase != record.phase {
                bail!("contradictory requested goal-owner admission phase")
            }
            if record.terminal_outcome != GoalOwnerAdmissionTerminalOutcome::None
                || has_lease
                || record.deferred_terminal_disposition
                    != GoalOwnerAdmissionTerminalDisposition::None
            {
                bail!("contradictory non-terminal goal-owner admission state")
            }
        }
        GoalOwnerAdmissionPhase::Acquired | GoalOwnerAdmissionPhase::InFlight => {
            if matches!(
                record.requested_phase,
                GoalOwnerAdmissionPhase::Acquired | GoalOwnerAdmissionPhase::InFlight
            ) || record.terminal_outcome != GoalOwnerAdmissionTerminalOutcome::None
                || !has_lease
                || record.attempts_started == 0
                || record.deferred_terminal_disposition
                    != GoalOwnerAdmissionTerminalDisposition::None
            {
                bail!("contradictory leased goal-owner admission state")
            }
        }
        GoalOwnerAdmissionPhase::Terminal => {
            if record.terminal_outcome == GoalOwnerAdmissionTerminalOutcome::None
                || matches!(
                    record.requested_phase,
                    GoalOwnerAdmissionPhase::Acquired
                        | GoalOwnerAdmissionPhase::InFlight
                        | GoalOwnerAdmissionPhase::Terminal
                )
                || !terminal_state_is_coherent(record, has_lease)
            {
                bail!("terminal goal-owner admission is missing its outcome")
            }
        }
    }
    Ok(())
}

fn terminal_state_is_coherent(record: &GoalOwnerAdmissionRecord, has_lease: bool) -> bool {
    match record.terminal_outcome {
        GoalOwnerAdmissionTerminalOutcome::Succeeded
        | GoalOwnerAdmissionTerminalOutcome::Rejected
        | GoalOwnerAdmissionTerminalOutcome::Exhausted => {
            record.attempts_started > 0
                && has_lease
                && record.deferred_terminal_disposition
                    == GoalOwnerAdmissionTerminalDisposition::None
        }
        GoalOwnerAdmissionTerminalOutcome::Uncertain => {
            record.attempts_started > 0
                && has_lease
                && record.deferred_terminal_disposition
                    == GoalOwnerAdmissionTerminalDisposition::ManualReview
        }
        GoalOwnerAdmissionTerminalOutcome::Cancelled => {
            record.deferred_terminal_disposition != GoalOwnerAdmissionTerminalDisposition::None
        }
        GoalOwnerAdmissionTerminalOutcome::None => false,
    }
}

fn observation_matches_origin(
    observation: &GoalOwnerAdmissionObservation,
    origin: &GoalOwnerAdmissionOrigin,
) -> bool {
    observation.goal_id == origin.goal_id
        && observation.origin_turn_id == origin.origin_turn_id
        && observation.origin_request_id == origin.origin_request_id
        && observation.denial_class == origin.denial_class
        && observation.configured_provider_key == origin.configured_provider_key
        && observation.requested_model == origin.requested_model
        && observation.effective_provider_id == origin.effective_provider_id
        && observation.effective_model == origin.effective_model
        && observation.intended_request_kind == origin.intended_request_kind
        && observation.successor_turn_id == origin.successor_turn_id
        && observation.logical_successor_request_id == origin.logical_successor_request_id
        && observation.decision_id == origin.decision_id
        && observation.account_context_fingerprint == origin.account_context_fingerprint
        && admission_datetime_to_epoch_millis(observation.deadline_at) == origin.deadline_at_ms
        && observation.max_attempts == origin.max_attempts
        && observation.requested_phase == origin.requested_phase
}

fn same_lease(record: &GoalOwnerAdmissionRecord, lease: &GoalOwnerAdmissionLease) -> bool {
    record.authority == lease.authority && record.lease_id == Some(lease.lease_id)
}

fn exact_terminal_replay(
    record: &GoalOwnerAdmissionRecord,
    lease: &GoalOwnerAdmissionLease,
    outcome: GoalOwnerAdmissionTerminalOutcome,
    disposition: GoalOwnerAdmissionTerminalDisposition,
) -> bool {
    same_lease(record, lease)
        && record.phase == GoalOwnerAdmissionPhase::Terminal
        && record.terminal_outcome == outcome
        && record.deferred_terminal_disposition == disposition
}

fn exact_cancellation_replay(
    record: &GoalOwnerAdmissionRecord,
    authority: &GoalOwnerAdmissionAuthority,
    disposition: GoalOwnerAdmissionTerminalDisposition,
) -> bool {
    record.authority.thread_id == authority.thread_id
        && record.authority.goal_id == authority.goal_id
        && record.authority.generation == authority.generation
        && record.authority.cancellation_epoch
            == authority
                .cancellation_epoch
                .checked_add(1)
                .unwrap_or(i64::MIN)
        && record.phase == GoalOwnerAdmissionPhase::Terminal
        && record.terminal_outcome == GoalOwnerAdmissionTerminalOutcome::Cancelled
        && record.deferred_terminal_disposition == disposition
}

#[cfg(test)]
#[path = "goal_owner_admissions_tests.rs"]
mod tests;
