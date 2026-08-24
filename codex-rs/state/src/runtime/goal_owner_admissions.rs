use super::*;
use anyhow::bail;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use sqlx::Row;
use uuid::Uuid;

const MAX_ORIGIN_ID_LENGTH: usize = 512;
const MAX_EVIDENCE_LENGTH: usize = 512;
const MAX_ACCOUNT_DOMAIN_LENGTH: usize = 255;

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
    pub provider_id: Option<String>,
    pub requested_model: Option<String>,
    pub effective_model: Option<String>,
    /// A provider account domain, never a raw account identifier.
    pub account_domain: Option<String>,
    pub deadline_at: DateTime<Utc>,
    pub max_attempts: i64,
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
    pub provider_id: Option<String>,
    pub requested_model: Option<String>,
    pub effective_model: Option<String>,
    pub account_domain: Option<String>,
    pub deadline_at: DateTime<Utc>,
    pub attempts_started: i64,
    pub max_attempts: i64,
    pub phase: GoalOwnerAdmissionPhase,
    pub terminal_outcome: GoalOwnerAdmissionTerminalOutcome,
    pub lease_id: Option<Uuid>,
    pub lease_acquired_at: Option<DateTime<Utc>>,
    pub deferred_terminal_disposition: GoalOwnerAdmissionTerminalDisposition,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct GoalOwnerAdmissionStore {
    pool: Arc<SqlitePool>,
}

impl GoalOwnerAdmissionStore {
    pub(crate) fn new(pool: Arc<SqlitePool>) -> Self {
        Self { pool }
    }

    /// Converts orphaned in-flight work to a conservative terminal state on reopen.
    pub(crate) async fn recover_in_flight_on_open(pool: &SqlitePool) -> anyhow::Result<()> {
        let now_ms = datetime_to_epoch_millis(Utc::now());
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

    /// Insert a denial, or return the existing state for an exact replay.
    ///
    /// A changed replay with the same origin request is rejected. A different origin
    /// request advances the generation while preserving the cancellation epoch.
    pub async fn observe_denial(
        &self,
        observation: &GoalOwnerAdmissionObservation,
    ) -> anyhow::Result<GoalOwnerAdmissionRecord> {
        validate_observation(observation)?;
        let mut transaction = self.pool.begin().await?;
        let existing = fetch_record(&mut *transaction, observation.thread_id).await?;
        if let Some(existing) = existing {
            if observation_matches_record(observation, &existing) {
                transaction.commit().await?;
                return Ok(existing);
            }
            if existing.origin_request_id == observation.origin_request_id {
                bail!("conflicting replay for goal-owner admission origin request")
            }
            let generation = existing
                .authority
                .generation
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("goal-owner admission generation overflow"))?;
            let now_ms = datetime_to_epoch_millis(Utc::now());
            let record = sqlx::query(REPLACE_ADMISSION_SQL)
                .bind(&observation.goal_id)
                .bind(generation)
                .bind(&observation.origin_turn_id)
                .bind(&observation.origin_request_id)
                .bind(observation.denial_class.as_str())
                .bind(&observation.provider_id)
                .bind(&observation.requested_model)
                .bind(&observation.effective_model)
                .bind(&observation.account_domain)
                .bind(datetime_to_epoch_millis(observation.deadline_at))
                .bind(observation.max_attempts)
                .bind(existing.authority.cancellation_epoch)
                .bind(observation.phase.as_str())
                .bind(now_ms)
                .bind(observation.thread_id.to_string())
                .fetch_one(&mut *transaction)
                .await?;
            transaction.commit().await?;
            return record_from_row(&record);
        }

        let now_ms = datetime_to_epoch_millis(Utc::now());
        let record = sqlx::query(INSERT_ADMISSION_SQL)
            .bind(observation.thread_id.to_string())
            .bind(&observation.goal_id)
            .bind(&observation.origin_turn_id)
            .bind(&observation.origin_request_id)
            .bind(observation.denial_class.as_str())
            .bind(&observation.provider_id)
            .bind(&observation.requested_model)
            .bind(&observation.effective_model)
            .bind(&observation.account_domain)
            .bind(datetime_to_epoch_millis(observation.deadline_at))
            .bind(observation.max_attempts)
            .bind(observation.phase.as_str())
            .bind(now_ms)
            .fetch_one(&mut *transaction)
            .await?;
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
        let now_ms = datetime_to_epoch_millis(now);
        let record = sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET phase = 'in_flight',
    attempts_started = attempts_started + 1,
    lease_id = ?,
    lease_acquired_at_ms = ?,
    updated_at_ms = ?
WHERE thread_id = ?
  AND goal_id = ?
  AND generation = ?
  AND cancellation_epoch = ?
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
        .bind(now_ms)
        .fetch_optional(self.pool.as_ref())
        .await?;
        record
            .map(|row| lease_from_record(record_from_row(&row)?))
            .transpose()
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
        let now_ms = datetime_to_epoch_millis(Utc::now());
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
        .fetch_optional(self.pool.as_ref())
        .await?;
        if let Some(row) = row {
            return record_from_row(&row).map(Some);
        }

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
        let now_ms = datetime_to_epoch_millis(Utc::now());
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
  AND phase IN ('dormant', 'pending', 'in_flight')
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
        .fetch_optional(self.pool.as_ref())
        .await?;
        if let Some(row) = row {
            return record_from_row(&row).map(Some);
        }

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
    provider_id, requested_model, effective_model, account_domain, deadline_at_ms,
    max_attempts, phase, terminal_outcome, deferred_terminal_disposition, created_at_ms,
    updated_at_ms
) VALUES (?, ?, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'none', 'none', ?, ?)
RETURNING *
"#;

const REPLACE_ADMISSION_SQL: &str = r#"
UPDATE goal_owner_admissions
SET goal_id = ?, generation = ?, origin_turn_id = ?, origin_request_id = ?, denial_class = ?,
    provider_id = ?, requested_model = ?, effective_model = ?, account_domain = ?,
    deadline_at_ms = ?, attempts_started = 0, max_attempts = ?, cancellation_epoch = ?,
    phase = ?, terminal_outcome = 'none', lease_id = NULL, lease_acquired_at_ms = NULL,
    deferred_terminal_disposition = 'none', updated_at_ms = ?
WHERE thread_id = ?
RETURNING *
"#;

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
        provider_id: row.try_get("provider_id")?,
        requested_model: row.try_get("requested_model")?,
        effective_model: row.try_get("effective_model")?,
        account_domain: row.try_get("account_domain")?,
        deadline_at: epoch_millis_to_datetime(row.try_get("deadline_at_ms")?)?,
        attempts_started: row.try_get("attempts_started")?,
        max_attempts: row.try_get("max_attempts")?,
        phase: GoalOwnerAdmissionPhase::try_from(row.try_get::<String, _>("phase")?.as_str())?,
        terminal_outcome: GoalOwnerAdmissionTerminalOutcome::try_from(
            row.try_get::<String, _>("terminal_outcome")?.as_str(),
        )?,
        lease_id: lease_id
            .map(|lease_id| Uuid::parse_str(&lease_id))
            .transpose()?,
        lease_acquired_at: lease_acquired_at_ms
            .map(epoch_millis_to_datetime)
            .transpose()?,
        deferred_terminal_disposition: GoalOwnerAdmissionTerminalDisposition::try_from(
            row.try_get::<String, _>("deferred_terminal_disposition")?
                .as_str(),
        )?,
        created_at: epoch_millis_to_datetime(row.try_get("created_at_ms")?)?,
        updated_at: epoch_millis_to_datetime(row.try_get("updated_at_ms")?)?,
    };
    validate_record(&record)?;
    Ok(record)
}

fn lease_from_record(record: GoalOwnerAdmissionRecord) -> anyhow::Result<GoalOwnerAdmissionLease> {
    if record.phase != GoalOwnerAdmissionPhase::InFlight {
        bail!("acquired goal-owner admission is not in flight")
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
    validate_nonempty("goal id", &observation.goal_id, MAX_ORIGIN_ID_LENGTH)?;
    validate_nonempty(
        "origin turn id",
        &observation.origin_turn_id,
        MAX_ORIGIN_ID_LENGTH,
    )?;
    validate_nonempty(
        "origin request id",
        &observation.origin_request_id,
        MAX_ORIGIN_ID_LENGTH,
    )?;
    validate_evidence(
        "provider id",
        observation.provider_id.as_deref(),
        MAX_EVIDENCE_LENGTH,
    )?;
    validate_evidence(
        "requested model",
        observation.requested_model.as_deref(),
        MAX_EVIDENCE_LENGTH,
    )?;
    validate_evidence(
        "effective model",
        observation.effective_model.as_deref(),
        MAX_EVIDENCE_LENGTH,
    )?;
    validate_evidence(
        "account domain",
        observation.account_domain.as_deref(),
        MAX_ACCOUNT_DOMAIN_LENGTH,
    )?;
    if observation.max_attempts < 1 {
        bail!("goal-owner admission max attempts must be positive")
    }
    if !matches!(
        observation.phase,
        GoalOwnerAdmissionPhase::Dormant | GoalOwnerAdmissionPhase::Pending
    ) {
        bail!("new goal-owner admission must be dormant or pending")
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
        "provider id",
        record.provider_id.as_deref(),
        MAX_EVIDENCE_LENGTH,
    )?;
    validate_evidence(
        "requested model",
        record.requested_model.as_deref(),
        MAX_EVIDENCE_LENGTH,
    )?;
    validate_evidence(
        "effective model",
        record.effective_model.as_deref(),
        MAX_EVIDENCE_LENGTH,
    )?;
    validate_evidence(
        "account domain",
        record.account_domain.as_deref(),
        MAX_ACCOUNT_DOMAIN_LENGTH,
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
            if record.terminal_outcome != GoalOwnerAdmissionTerminalOutcome::None
                || has_lease
                || record.deferred_terminal_disposition
                    != GoalOwnerAdmissionTerminalDisposition::None
            {
                bail!("contradictory non-terminal goal-owner admission state")
            }
        }
        GoalOwnerAdmissionPhase::InFlight => {
            if record.terminal_outcome != GoalOwnerAdmissionTerminalOutcome::None
                || !has_lease
                || record.deferred_terminal_disposition
                    != GoalOwnerAdmissionTerminalDisposition::None
            {
                bail!("contradictory in-flight goal-owner admission state")
            }
        }
        GoalOwnerAdmissionPhase::Terminal => {
            if record.terminal_outcome == GoalOwnerAdmissionTerminalOutcome::None {
                bail!("terminal goal-owner admission is missing its outcome")
            }
        }
    }
    Ok(())
}

fn observation_matches_record(
    observation: &GoalOwnerAdmissionObservation,
    record: &GoalOwnerAdmissionRecord,
) -> bool {
    observation.goal_id == record.authority.goal_id
        && observation.origin_turn_id == record.origin_turn_id
        && observation.origin_request_id == record.origin_request_id
        && observation.denial_class == record.denial_class
        && observation.provider_id == record.provider_id
        && observation.requested_model == record.requested_model
        && observation.effective_model == record.effective_model
        && observation.account_domain == record.account_domain
        && observation.deadline_at == record.deadline_at
        && observation.max_attempts == record.max_attempts
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
