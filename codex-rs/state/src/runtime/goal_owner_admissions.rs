use super::*;
use anyhow::bail;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use sqlx::Row;
use uuid::Uuid;

const MAX_ORIGIN_ID_LENGTH: usize = 512;

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

/// Fencing tuple for an exact durable admission generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalOwnerAdmissionAuthority {
    pub thread_id: ThreadId,
    pub goal_id: String,
    pub generation: i64,
    pub cancellation_epoch: i64,
}

/// Immutable denial evidence used to create one durable admission generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalOwnerAdmissionObservation {
    pub thread_id: ThreadId,
    pub goal_id: String,
    pub origin_turn_id: String,
    pub origin_request_id: String,
    pub denial_class: GoalOwnerAdmissionDenialClass,
    pub deadline_at: DateTime<Utc>,
    pub max_attempts: i64,
    /// Immutable requested state used to distinguish replays from lifecycle transitions.
    pub requested_phase: GoalOwnerAdmissionPhase,
    pub phase: GoalOwnerAdmissionPhase,
}

/// Lease returned by an atomic transition from pending to in-flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalOwnerAdmissionLease {
    pub authority: GoalOwnerAdmissionAuthority,
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

    /// Close the shared goals database pool during runtime shutdown.
    pub(crate) async fn close(&self) {
        self.pool.close().await;
    }

    /// Converts orphaned in-flight work to a conservative terminal state on reopen.
    pub(crate) async fn recover_in_flight_on_open(
        pool: &SqlitePool,
        _runtime_owner: &super::RuntimeProcessLock,
    ) -> anyhow::Result<()> {
        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET phase = 'pending',
    attempts_started = attempts_started - 1,
    lease_id = NULL,
    lease_acquired_at_ms = NULL,
    updated_at_ms = ?
WHERE phase = 'acquired'
  AND requested_phase = 'pending'
  AND attempts_started > 0
            "#,
        )
        .bind(now_ms)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET phase = 'terminal',
    terminal_outcome = 'uncertain',
    deferred_terminal_disposition = 'manual_review',
    updated_at_ms = ?
WHERE phase = 'in_flight'
  AND requested_phase = 'pending'
            "#,
        )
        .bind(now_ms)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
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
        let thread_goal_exists =
            sqlx::query_scalar::<_, i64>("SELECT 1 FROM thread_goals WHERE thread_id = ?")
                .bind(observation.thread_id.to_string())
                .fetch_optional(&mut *transaction)
                .await?
                .is_some();
        if !thread_goal_exists {
            bail!("goal-owner admission requires an existing thread goal")
        }
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

    /// Atomically lease a deadline-eligible pending admission for one exact authority tuple.
    pub async fn try_acquire(
        &self,
        authority: &GoalOwnerAdmissionAuthority,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<GoalOwnerAdmissionLease>> {
        validate_authority(authority)?;
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
  AND phase = 'pending'
  AND requested_phase = 'pending'
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
        .bind(now_ms)
        .fetch_optional(&mut *transaction)
        .await?;
        let lease = record
            .map(|row| lease_from_record(record_from_row(&row)?))
            .transpose()?;
        transaction.commit().await?;
        Ok(lease)
    }

    /// Cross the pre-network boundary for an exact acquired reservation.
    /// Cancellation that commits first makes this return `false`, prohibiting
    /// the caller from issuing provider I/O.
    pub async fn open_lease(&self, lease: &GoalOwnerAdmissionLease) -> anyhow::Result<bool> {
        validate_lease(lease)?;
        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        let result = sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET phase = 'in_flight', updated_at_ms = ?
WHERE thread_id = ? AND goal_id = ? AND generation = ? AND cancellation_epoch = ?
  AND phase = 'acquired' AND lease_id = ?
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

    /// Release an exact pre-network reservation and restore its attempt.
    pub async fn release_acquired_lease(
        &self,
        lease: &GoalOwnerAdmissionLease,
    ) -> anyhow::Result<bool> {
        validate_lease(lease)?;
        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let result = sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET phase = 'pending', attempts_started = attempts_started - 1,
    lease_id = NULL, lease_acquired_at_ms = NULL, updated_at_ms = ?
WHERE thread_id = ? AND goal_id = ? AND generation = ? AND cancellation_epoch = ?
  AND phase = 'acquired' AND lease_id = ? AND attempts_started > 0
            "#,
        )
        .bind(now_ms)
        .bind(lease.authority.thread_id.to_string())
        .bind(&lease.authority.goal_id)
        .bind(lease.authority.generation)
        .bind(lease.authority.cancellation_epoch)
        .bind(lease.lease_id.to_string())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    /// Recover one exact lease: pre-network reservations return to pending;
    /// in-flight work is conservatively marked uncertain for manual review.
    pub async fn reopen(&self, lease: &GoalOwnerAdmissionLease) -> anyhow::Result<bool> {
        validate_lease(lease)?;
        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let acquired = sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET phase = 'pending', attempts_started = attempts_started - 1,
    lease_id = NULL, lease_acquired_at_ms = NULL, updated_at_ms = ?
WHERE thread_id = ? AND goal_id = ? AND generation = ? AND cancellation_epoch = ?
  AND phase = 'acquired' AND lease_id = ? AND attempts_started > 0
            "#,
        )
        .bind(now_ms)
        .bind(lease.authority.thread_id.to_string())
        .bind(&lease.authority.goal_id)
        .bind(lease.authority.generation)
        .bind(lease.authority.cancellation_epoch)
        .bind(lease.lease_id.to_string())
        .execute(&mut *transaction)
        .await?;
        if acquired.rows_affected() == 1 {
            transaction.commit().await?;
            return Ok(true);
        }
        let inflight = sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET phase = 'terminal', terminal_outcome = 'uncertain',
    deferred_terminal_disposition = 'manual_review', updated_at_ms = ?
WHERE thread_id = ? AND goal_id = ? AND generation = ? AND cancellation_epoch = ?
  AND phase = 'in_flight' AND lease_id = ?
            "#,
        )
        .bind(now_ms)
        .bind(lease.authority.thread_id.to_string())
        .bind(&lease.authority.goal_id)
        .bind(lease.authority.generation)
        .bind(lease.authority.cancellation_epoch)
        .bind(lease.lease_id.to_string())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(inflight.rows_affected() == 1)
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
            Some(record)
                if same_lease(&record, lease)
                    && matches!(
                        record.terminal_outcome,
                        GoalOwnerAdmissionTerminalOutcome::Cancelled
                            | GoalOwnerAdmissionTerminalOutcome::Uncertain
                    ) =>
            {
                Ok(None)
            }
            Some(record) if same_lease(&record, lease) => {
                bail!("conflicting replay for goal-owner admission lease outcome")
            }
            _ => Ok(None),
        }
    }

    /// Increment the cancellation epoch and terminalize the exact durable generation.
    ///
    /// Cancellation before a provider request is opened is definitive. Once a
    /// request is in flight, its external effect is unknowable, so cancellation
    /// records `uncertain/manual_review` and permanently fences replacement.
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
    terminal_outcome = CASE
        WHEN phase = 'in_flight' THEN 'uncertain'
        ELSE 'cancelled'
    END,
    cancellation_epoch = ?,
    deferred_terminal_disposition = CASE
        WHEN phase = 'in_flight' THEN 'manual_review'
        ELSE ?
    END,
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
            Some(record)
                if record.authority.thread_id == authority.thread_id
                    && record.authority.goal_id == authority.goal_id
                    && record.authority.generation == authority.generation
                    && record.authority.cancellation_epoch
                        == authority
                            .cancellation_epoch
                            .checked_add(1)
                            .unwrap_or(i64::MIN)
                    && matches!(
                        record.terminal_outcome,
                        GoalOwnerAdmissionTerminalOutcome::Cancelled
                            | GoalOwnerAdmissionTerminalOutcome::Uncertain
                    ) =>
            {
                bail!("conflicting replay for goal-owner admission cancellation")
            }
            _ => Ok(None),
        }
    }
}

const INSERT_ADMISSION_SQL: &str = r#"
INSERT INTO goal_owner_admissions (
    thread_id, goal_id, generation, origin_turn_id, origin_request_id, denial_class,
    deadline_at_ms, max_attempts, requested_phase, phase, terminal_outcome, deferred_terminal_disposition, created_at_ms,
    updated_at_ms
) VALUES (?, ?, 1, ?, ?, ?, ?, ?, ?, ?, 'none', 'none', ?, ?)
RETURNING *
"#;

const REPLACE_ADMISSION_SQL: &str = r#"
UPDATE goal_owner_admissions
SET goal_id = ?, generation = ?, origin_turn_id = ?, origin_request_id = ?, denial_class = ?,
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
    thread_id, origin_request_id, goal_id, origin_turn_id, denial_class, deadline_at_ms,
    max_attempts, requested_phase
) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(observation.thread_id.to_string())
    .bind(&origin.origin_request_id)
    .bind(&origin.goal_id)
    .bind(&origin.origin_turn_id)
    .bind(origin.denial_class.as_str())
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
        bail!("goal-owner admission is not acquired")
    }
    Ok(GoalOwnerAdmissionLease {
        authority: record.authority,
        lease_id: record.lease_id.ok_or_else(|| {
            anyhow::anyhow!("in-flight goal-owner admission is missing its lease")
        })?,
        acquired_at: record.lease_acquired_at.ok_or_else(|| {
            anyhow::anyhow!("in-flight goal-owner admission is missing its lease timestamp")
        })?,
    })
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

fn validate_lease(lease: &GoalOwnerAdmissionLease) -> anyhow::Result<()> {
    validate_authority(&lease.authority)?;
    if lease.lease_id.is_nil() {
        bail!("goal-owner admission lease id must be non-nil")
    }
    admission_datetime_to_epoch_millis(lease.acquired_at);
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

fn validate_record(record: &GoalOwnerAdmissionRecord) -> anyhow::Result<()> {
    validate_authority(&record.authority)?;
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
            if record.requested_phase != GoalOwnerAdmissionPhase::Pending
                || record.terminal_outcome != GoalOwnerAdmissionTerminalOutcome::None
                || !has_lease
                || record.attempts_started == 0
                || record.deferred_terminal_disposition
                    != GoalOwnerAdmissionTerminalDisposition::None
            {
                bail!("contradictory in-flight goal-owner admission state")
            }
        }
        GoalOwnerAdmissionPhase::Terminal => {
            if record.terminal_outcome == GoalOwnerAdmissionTerminalOutcome::None
                || matches!(
                    record.requested_phase,
                    GoalOwnerAdmissionPhase::InFlight | GoalOwnerAdmissionPhase::Terminal
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
        && match record.terminal_outcome {
            GoalOwnerAdmissionTerminalOutcome::Cancelled => {
                record.deferred_terminal_disposition == disposition
            }
            GoalOwnerAdmissionTerminalOutcome::Uncertain => {
                record.deferred_terminal_disposition
                    == GoalOwnerAdmissionTerminalDisposition::ManualReview
                    && disposition != GoalOwnerAdmissionTerminalDisposition::None
            }
            _ => false,
        }
}

#[cfg(test)]
#[path = "goal_owner_admissions_tests.rs"]
mod tests;
