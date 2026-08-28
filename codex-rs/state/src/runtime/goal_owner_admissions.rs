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

/// Stable provider identifier shared by persistence, admission, and observation joins.
pub fn canonical_provider_id(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

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

// Exact provider denial recorded before a goal owner may attempt a retry.
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

// Bounded instruction for a terminal admission that must remain deferred.
admission_enum!(GoalOwnerAdmissionTerminalDisposition {
    None => "none",
    AwaitUserTurn => "await_user_turn",
    ManualReview => "manual_review",
});

// Reason an admission was explicitly retired without erasing physical evidence.
admission_enum!(GoalOwnerAdmissionRetirementReason {
    Superseded => "superseded",
    UserRecovery => "user_recovery",
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
    /// The chain-wide continuation-attempt maximum. Production scheduler policy
    /// normally supplies one; higher values are retained only when explicitly
    /// recorded with the immutable origin evidence.
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

/// Precise result of attempting to reserve a scheduler continuation.
///
/// A caller must treat every non-`Acquired` variant as non-permission to begin
/// provider I/O. `Exhausted` carries the atomically terminalized record so the
/// scheduler cannot mistake an exhausted pending row for a retryable miss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalOwnerAdmissionAcquireResult {
    Acquired(Box<GoalOwnerAdmissionLease>),
    Exhausted(Box<GoalOwnerAdmissionRecord>),
    NotCurrent,
    NotEligible,
    Dormant,
}

/// Fully decoded durable admission state, including its immutable chain budget.
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
    /// Attempts acquired in this generation only.
    pub attempts_started: i64,
    /// The maximum for both this generation and its goal chain.
    pub max_attempts: i64,
    /// Attempts acquired across every generation for this exact goal ID.
    pub chain_attempts_started: i64,
    pub chain_max_attempts: i64,
    pub requested_phase: GoalOwnerAdmissionPhase,
    pub phase: GoalOwnerAdmissionPhase,
    pub terminal_outcome: GoalOwnerAdmissionTerminalOutcome,
    pub lease_id: Option<Uuid>,
    pub lease_acquired_at: Option<DateTime<Utc>>,
    /// The cancellation epoch at which the persisted lease was created.
    pub lease_cancellation_epoch: Option<i64>,
    /// The single scheduler owner that has claimed this pending generation for
    /// dispatch. A claim is consumed when the provider lease is acquired.
    pub dispatch_claim_id: Option<Uuid>,
    pub dispatch_claimed_at: Option<DateTime<Utc>>,
    pub deferred_terminal_disposition: GoalOwnerAdmissionTerminalDisposition,
    pub retired_at: Option<DateTime<Utc>>,
    pub retirement_reason: Option<GoalOwnerAdmissionRetirementReason>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GoalOwnerAdmissionOrigin {
    thread_id: ThreadId,
    generation: i64,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct GoalOwnerAdmissionGoalChain {
    thread_id: ThreadId,
    goal_id: String,
    attempts_started: i64,
    max_attempts: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct GoalOwnerAdmissionStore {
    pool: Arc<SqlitePool>,
}

impl GoalOwnerAdmissionStore {
    pub(crate) fn new(pool: Arc<SqlitePool>) -> Self {
        Self { pool }
    }

    /// Record the process that holds the OS-backed runtime ownership lock.
    ///
    /// The caller must acquire the process-lifetime lock before invoking this
    /// method. Replacing an old row is safe only under that lock: the kernel
    /// has already proved that the previous holder exited.
    pub async fn claim_runtime_owner(&self, owner_id: Uuid) -> anyhow::Result<()> {
        let result = sqlx::query(
            r#"
INSERT INTO goal_owner_runtime_owners (owner_key, owner_id, acquired_at_ms)
VALUES (1, ?, ?)
ON CONFLICT(owner_key) DO UPDATE SET
    owner_id = excluded.owner_id,
    acquired_at_ms = excluded.acquired_at_ms
            "#,
        )
        .bind(owner_id.to_string())
        .bind(admission_datetime_to_epoch_millis(Utc::now()))
        .execute(self.pool.as_ref())
        .await?;
        debug_assert_eq!(result.rows_affected(), 1);
        Ok(())
    }

    /// Release only the exact durable runtime owner acquired by this process.
    pub async fn release_runtime_owner(&self, owner_id: Uuid) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "DELETE FROM goal_owner_runtime_owners WHERE owner_key = 1 AND owner_id = ?",
        )
        .bind(owner_id.to_string())
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Recovers durable work after a process restart.
    ///
    /// Acquired leases have not crossed the provider boundary, so their
    /// generation and chain counters are returned exactly once. In-flight work
    /// retains its lease and becomes uncertain because the physical effect may
    /// already exist.
    pub(crate) async fn recover_in_flight_on_open_as_owner(
        &self,
        owner_id: Uuid,
    ) -> anyhow::Result<()> {
        let registered_owner = sqlx::query_scalar::<_, String>(
            "SELECT owner_id FROM goal_owner_runtime_owners WHERE owner_key = 1",
        )
        .fetch_optional(self.pool.as_ref())
        .await?;
        let expected_owner = owner_id.to_string();
        if registered_owner.as_deref() != Some(expected_owner.as_str()) {
            bail!("goal-owner admission recovery requires the exact runtime owner")
        }
        Self::recover_in_flight_on_open_impl(self.pool.as_ref()).await
    }

    #[cfg(test)]
    pub(crate) async fn recover_in_flight_on_open(pool: &SqlitePool) -> anyhow::Result<()> {
        Self::recover_in_flight_on_open_impl(pool).await
    }

    async fn recover_in_flight_on_open_impl(pool: &SqlitePool) -> anyhow::Result<()> {
        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        let rows = recoverable_admissions_query()
            .fetch_all(&mut *transaction)
            .await?;
        for row in rows {
            record_from_row(&row)?;
        }
        sqlx::query(
            r#"
UPDATE goal_owner_admission_goal_chains AS chain
SET attempts_started = attempts_started - (
    SELECT COUNT(*)
    FROM goal_owner_admissions AS admission
    WHERE admission.thread_id = chain.thread_id
      AND admission.goal_id = chain.goal_id
      AND admission.phase = 'acquired'
      AND admission.retired_at_ms IS NULL
)
WHERE EXISTS (
    SELECT 1
    FROM goal_owner_admissions AS admission
    WHERE admission.thread_id = chain.thread_id
      AND admission.goal_id = chain.goal_id
      AND admission.phase = 'acquired'
      AND admission.retired_at_ms IS NULL
)
            "#,
        )
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET phase = 'pending',
    attempts_started = attempts_started - 1,
    lease_id = NULL,
    lease_acquired_at_ms = NULL,
    lease_cancellation_epoch = NULL,
    dispatch_claim_id = NULL,
    dispatch_claimed_at_ms = NULL,
    updated_at_ms = ?
WHERE phase = 'acquired' AND retired_at_ms IS NULL
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
            "#,
        )
        .bind(now_ms)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Read the one active, non-retired admission for a thread.
    pub async fn get(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<GoalOwnerAdmissionRecord>> {
        fetch_active_record(self.pool.as_ref(), thread_id).await
    }

    /// Read an exact generation, including retired history, by its fencing tuple.
    pub async fn get_generation(
        &self,
        authority: &GoalOwnerAdmissionAuthority,
    ) -> anyhow::Result<Option<GoalOwnerAdmissionRecord>> {
        validate_authority(authority)?;
        fetch_record_for_authority(self.pool.as_ref(), authority).await
    }

    /// Claim one exact, deadline-eligible pending generation for dispatch.
    ///
    /// The claim is the durable owner fence between timer eligibility and
    /// publishing a successor turn. It is intentionally separate from the
    /// provider-attempt lease and can be released only by the exact claimant.
    pub async fn claim_dispatch(
        &self,
        continuation_authority: &GoalOwnerAdmissionContinuationAuthority,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<Uuid>> {
        validate_continuation_authority(continuation_authority)?;
        let authority = &continuation_authority.authority;
        let now_ms = admission_datetime_to_epoch_millis(now);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some(current) = fetch_active_record(&mut *transaction, authority.thread_id).await?
        else {
            transaction.commit().await?;
            return Ok(None);
        };
        if current.continuation_authority() != continuation_authority.clone()
            || current.phase != GoalOwnerAdmissionPhase::Pending
            || current.deadline_at > now
            || current.dispatch_claim_id.is_some()
        {
            transaction.commit().await?;
            return Ok(None);
        }
        let claim_id = Uuid::now_v7();
        let claimed = sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET dispatch_claim_id = ?,
    dispatch_claimed_at_ms = ?,
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
  AND dispatch_claim_id IS NULL
  AND retired_at_ms IS NULL
            "#,
        )
        .bind(claim_id.to_string())
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
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok((claimed.rows_affected() == 1).then_some(claim_id))
    }

    /// Release only an exact pending dispatch claim. A stale claimant is a
    /// no-op, so cleanup cannot clear a replacement generation's owner fence.
    pub async fn release_dispatch_claim(
        &self,
        authority: &GoalOwnerAdmissionAuthority,
        dispatch_claim_id: Uuid,
    ) -> anyhow::Result<bool> {
        validate_authority(authority)?;
        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        let result = sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET dispatch_claim_id = NULL,
    dispatch_claimed_at_ms = NULL,
    updated_at_ms = ?
WHERE thread_id = ?
  AND goal_id = ?
  AND generation = ?
  AND cancellation_epoch = ?
  AND phase = 'pending'
  AND dispatch_claim_id = ?
  AND retired_at_ms IS NULL
            "#,
        )
        .bind(now_ms)
        .bind(authority.thread_id.to_string())
        .bind(&authority.goal_id)
        .bind(authority.generation)
        .bind(authority.cancellation_epoch)
        .bind(dispatch_claim_id.to_string())
        .execute(self.pool.as_ref())
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Atomically clear the thread deferral only after the exact generation
    /// has retired and no replacement admission is active.
    pub async fn clear_deferral_if_retired(
        &self,
        authority: &GoalOwnerAdmissionAuthority,
    ) -> anyhow::Result<bool> {
        validate_authority(authority)?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some(record) = fetch_record_for_authority(&mut *transaction, authority).await? else {
            transaction.commit().await?;
            return Ok(false);
        };
        if record.retired_at.is_none() {
            transaction.commit().await?;
            return Ok(false);
        }
        let active = fetch_active_record(&mut *transaction, authority.thread_id).await?;
        if active.is_some() {
            transaction.commit().await?;
            return Ok(false);
        }
        let result =
            sqlx::query("DELETE FROM thread_goal_continuation_deferrals WHERE thread_id = ?")
                .bind(authority.thread_id.to_string())
                .execute(&mut *transaction)
                .await?;
        transaction.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    /// Insert a denial, or return the recorded generation for an exact origin replay.
    ///
    /// A changed origin can start only after the active generation has been
    /// explicitly retired. The retirement preserves the old lifecycle row;
    /// a same-goal replacement therefore shares its chain counter while a new
    /// goal ID creates a fresh chain.
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
        if let Some(origin) = origin {
            if !observation_matches_origin(observation, &origin) {
                bail!("conflicting replay for goal-owner admission origin request")
            }
            let record = fetch_record_by_generation(
                &mut *transaction,
                observation.thread_id,
                origin.generation,
            )
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "goal-owner admission origin history exists without its durable generation"
                )
            })?;
            transaction.commit().await?;
            return Ok(record);
        }

        if fetch_active_record(&mut *transaction, observation.thread_id)
            .await?
            .is_some()
        {
            bail!("active goal-owner admission must be explicitly retired before replacement")
        }

        let generation = next_generation(&mut transaction, observation.thread_id).await?;
        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        ensure_goal_chain(&mut transaction, observation, now_ms).await?;
        insert_origin(&mut transaction, observation, generation).await?;
        let origin = fetch_origin(
            &mut *transaction,
            observation.thread_id,
            &observation.origin_request_id,
        )
        .await?
        .ok_or_else(admission_integrity_error)?;
        if origin.thread_id != observation.thread_id
            || origin.generation != generation
            || !observation_matches_origin(observation, &origin)
        {
            return Err(admission_integrity_error());
        }
        insert_admission(&mut transaction, &origin, now_ms).await?;
        let record =
            fetch_record_by_generation(&mut *transaction, observation.thread_id, generation)
                .await?
                .ok_or_else(|| anyhow::anyhow!("inserted goal-owner admission is missing"))?;
        transaction.commit().await?;
        Ok(record)
    }

    /// Retire exactly one settled durable generation without changing its outcome.
    ///
    /// Retirement is a scheduler/user lifecycle decision, not a provider result.
    /// A reservation or uncertain provider effect must first be released,
    /// cancelled, or resolved; superseding it would make recovery ordering and
    /// physical evidence ambiguous.
    pub async fn retire(
        &self,
        authority: &GoalOwnerAdmissionAuthority,
        reason: GoalOwnerAdmissionRetirementReason,
    ) -> anyhow::Result<Option<GoalOwnerAdmissionRecord>> {
        validate_authority(authority)?;
        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let record = fetch_record_for_authority(&mut *transaction, authority).await?;
        let Some(record) = record else {
            transaction.commit().await?;
            return Ok(None);
        };
        if record.authority != authority.clone() {
            transaction.commit().await?;
            return Ok(None);
        }
        if retirement_is_unsettled(&record) {
            bail!("goal-owner admission with unsettled work cannot be retired")
        }
        if record.retired_at.is_some() {
            let replay = (record.retirement_reason == Some(reason)).then_some(record);
            transaction.commit().await?;
            return Ok(replay);
        }
        let result = sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET retired_at_ms = ?,
    retirement_reason = ?,
    updated_at_ms = ?
WHERE thread_id = ?
  AND goal_id = ?
  AND generation = ?
  AND cancellation_epoch = ?
  AND retired_at_ms IS NULL
            "#,
        )
        .bind(now_ms)
        .bind(reason.as_str())
        .bind(now_ms)
        .bind(authority.thread_id.to_string())
        .bind(&authority.goal_id)
        .bind(authority.generation)
        .bind(authority.cancellation_epoch)
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() != 1 {
            bail!("goal-owner retirement lost its exact settled generation")
        }
        let record = fetch_record_for_authority(&mut *transaction, authority)
            .await?
            .ok_or_else(|| anyhow::anyhow!("retired admission is missing"))?;
        transaction.commit().await?;
        Ok(Some(record))
    }

    /// Retire a cancellation that committed its terminal transition before the retirement step.
    ///
    /// Cancellation increments the fencing epoch, so retrying [`Self::cancel`] with the old
    /// authority is intentionally a no-op. Recovery callers use this exact generation helper to
    /// find the post-cancellation row and retire it without changing its preserved outcome.
    pub async fn retire_cancelled_generation(
        &self,
        authority: &GoalOwnerAdmissionAuthority,
        reason: GoalOwnerAdmissionRetirementReason,
    ) -> anyhow::Result<Option<GoalOwnerAdmissionRecord>> {
        validate_authority(authority)?;
        let expected_epoch = authority
            .cancellation_epoch
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("goal-owner admission cancellation epoch overflow"))?;
        let record = fetch_record_by_generation(
            self.pool.as_ref(),
            authority.thread_id,
            authority.generation,
        )
        .await?;
        let Some(record) = record else {
            return Ok(None);
        };
        if record.authority.goal_id != authority.goal_id
            || record.phase != GoalOwnerAdmissionPhase::Terminal
            || record.terminal_outcome != GoalOwnerAdmissionTerminalOutcome::Cancelled
            || (record.authority != authority.clone()
                && record.authority.cancellation_epoch != expected_epoch)
        {
            return Ok(None);
        }
        self.retire(&record.authority, reason).await
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
    ) -> anyhow::Result<GoalOwnerAdmissionAcquireResult> {
        self.try_acquire_inner(continuation_authority, None, now)
            .await
    }

    /// Atomically consume an exact scheduler dispatch claim while reserving
    /// the provider attempt. An unclaimed or differently claimed generation
    /// cannot be acquired through this path.
    pub async fn try_acquire_claimed(
        &self,
        continuation_authority: &GoalOwnerAdmissionContinuationAuthority,
        dispatch_claim_id: Uuid,
        now: DateTime<Utc>,
    ) -> anyhow::Result<GoalOwnerAdmissionAcquireResult> {
        self.try_acquire_inner(continuation_authority, Some(dispatch_claim_id), now)
            .await
    }

    async fn try_acquire_inner(
        &self,
        continuation_authority: &GoalOwnerAdmissionContinuationAuthority,
        dispatch_claim_id: Option<Uuid>,
        now: DateTime<Utc>,
    ) -> anyhow::Result<GoalOwnerAdmissionAcquireResult> {
        let authority = &continuation_authority.authority;
        validate_authority(authority)?;
        validate_continuation_authority(continuation_authority)?;
        let now_ms = admission_datetime_to_epoch_millis(now);
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some(current) = fetch_active_record(&mut *transaction, authority.thread_id).await?
        else {
            transaction.commit().await?;
            return Ok(GoalOwnerAdmissionAcquireResult::NotCurrent);
        };
        if current.continuation_authority() != continuation_authority.clone() {
            transaction.commit().await?;
            return Ok(GoalOwnerAdmissionAcquireResult::NotCurrent);
        }
        if current.dispatch_claim_id != dispatch_claim_id {
            transaction.commit().await?;
            return Ok(GoalOwnerAdmissionAcquireResult::NotCurrent);
        }
        match current.phase {
            GoalOwnerAdmissionPhase::Dormant => {
                transaction.commit().await?;
                return Ok(GoalOwnerAdmissionAcquireResult::Dormant);
            }
            GoalOwnerAdmissionPhase::Terminal
                if current.terminal_outcome == GoalOwnerAdmissionTerminalOutcome::Exhausted =>
            {
                transaction.commit().await?;
                return Ok(GoalOwnerAdmissionAcquireResult::Exhausted(Box::new(
                    current,
                )));
            }
            GoalOwnerAdmissionPhase::Pending if current.deadline_at <= now => {}
            GoalOwnerAdmissionPhase::Pending
            | GoalOwnerAdmissionPhase::Acquired
            | GoalOwnerAdmissionPhase::InFlight
            | GoalOwnerAdmissionPhase::Terminal => {
                transaction.commit().await?;
                return Ok(GoalOwnerAdmissionAcquireResult::NotEligible);
            }
        }

        let chain_incremented = sqlx::query(
            r#"
UPDATE goal_owner_admission_goal_chains
SET attempts_started = attempts_started + 1,
    updated_at_ms = ?
WHERE thread_id = ?
  AND goal_id = ?
  AND attempts_started < max_attempts
            "#,
        )
        .bind(now_ms)
        .bind(authority.thread_id.to_string())
        .bind(&authority.goal_id)
        .execute(&mut *transaction)
        .await?;
        if chain_incremented.rows_affected() == 0 {
            let terminalized = sqlx::query(
                r#"
UPDATE goal_owner_admissions
SET phase = 'terminal',
    terminal_outcome = 'exhausted',
    deferred_terminal_disposition = 'await_user_turn',
    dispatch_claim_id = NULL,
    dispatch_claimed_at_ms = NULL,
    updated_at_ms = ?
WHERE thread_id = ?
  AND goal_id = ?
  AND generation = ?
  AND cancellation_epoch = ?
  AND phase = 'pending'
  AND retired_at_ms IS NULL
                "#,
            )
            .bind(now_ms)
            .bind(authority.thread_id.to_string())
            .bind(&authority.goal_id)
            .bind(authority.generation)
            .bind(authority.cancellation_epoch)
            .execute(&mut *transaction)
            .await?;
            if terminalized.rows_affected() != 1 {
                bail!("goal-owner admission chain exhaustion lost its current pending generation")
            }
            let exhausted = fetch_record_for_authority(&mut *transaction, authority)
                .await?
                .ok_or_else(|| anyhow::anyhow!("exhausted admission is missing"))?;
            transaction.commit().await?;
            return Ok(GoalOwnerAdmissionAcquireResult::Exhausted(Box::new(
                exhausted,
            )));
        }

        let lease_id = Uuid::now_v7();
        let acquired = sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET phase = 'acquired',
    attempts_started = attempts_started + 1,
    lease_id = ?,
    lease_acquired_at_ms = ?,
    lease_cancellation_epoch = cancellation_epoch,
    dispatch_claim_id = NULL,
    dispatch_claimed_at_ms = NULL,
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
  AND dispatch_claim_id IS ?
  AND attempts_started < max_attempts
  AND retired_at_ms IS NULL
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
        .bind(dispatch_claim_id.map(|claim| claim.to_string()))
        .execute(&mut *transaction)
        .await?;
        if acquired.rows_affected() != 1 {
            bail!("goal-owner admission chain increment lost its exact pending generation")
        }
        let record = fetch_record_for_authority(&mut *transaction, authority)
            .await?
            .ok_or_else(|| anyhow::anyhow!("acquired admission is missing"))?;
        let lease = lease_from_record(record)?;
        transaction.commit().await?;
        Ok(GoalOwnerAdmissionAcquireResult::Acquired(Box::new(lease)))
    }

    /// Atomically linearize an acquired lease as provider work immediately
    /// before network I/O. A cancellation or retirement that commits first
    /// leaves this method with no row, which prohibits the physical request.
    pub async fn open_lease(&self, lease: &GoalOwnerAdmissionLease) -> anyhow::Result<bool> {
        validate_lease(lease)?;
        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = fetch_record_for_authority(&mut *transaction, &lease.authority).await?;
        let Some(current) = current else {
            transaction.commit().await?;
            return Ok(false);
        };
        if current.phase != GoalOwnerAdmissionPhase::Acquired
            || current.retired_at.is_some()
            || !same_lease(&current, lease)
            || current.continuation_authority() != lease.continuation_authority
        {
            transaction.commit().await?;
            return Ok(false);
        }
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
  AND lease_cancellation_epoch = ?
  AND retired_at_ms IS NULL
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
        .bind(lease.authority.cancellation_epoch)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(result.rows_affected() == 1)
    }

    /// Return a pre-network reservation to pending when request setup fails.
    pub async fn release_acquired_lease(
        &self,
        lease: &GoalOwnerAdmissionLease,
    ) -> anyhow::Result<bool> {
        validate_lease(lease)?;
        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = fetch_record_for_authority(&mut *transaction, &lease.authority).await?;
        let Some(current) = current else {
            transaction.commit().await?;
            return Ok(false);
        };
        if current.phase != GoalOwnerAdmissionPhase::Acquired
            || current.retired_at.is_some()
            || !same_lease(&current, lease)
        {
            transaction.commit().await?;
            return Ok(false);
        }
        let released = sqlx::query(
            r#"
UPDATE goal_owner_admissions
SET phase = 'pending',
    attempts_started = attempts_started - 1,
    lease_id = NULL,
    lease_acquired_at_ms = NULL,
    lease_cancellation_epoch = NULL,
    updated_at_ms = ?
WHERE thread_id = ?
  AND goal_id = ?
  AND generation = ?
  AND cancellation_epoch = ?
  AND phase = 'acquired'
  AND lease_id = ?
  AND lease_cancellation_epoch = ?
  AND retired_at_ms IS NULL
            "#,
        )
        .bind(now_ms)
        .bind(lease.authority.thread_id.to_string())
        .bind(&lease.authority.goal_id)
        .bind(lease.authority.generation)
        .bind(lease.authority.cancellation_epoch)
        .bind(lease.lease_id.to_string())
        .bind(lease.authority.cancellation_epoch)
        .execute(&mut *transaction)
        .await?;
        if released.rows_affected() == 0 {
            transaction.commit().await?;
            return Ok(false);
        }
        let decremented = sqlx::query(
            r#"
UPDATE goal_owner_admission_goal_chains
SET attempts_started = attempts_started - 1,
    updated_at_ms = ?
WHERE thread_id = ?
  AND goal_id = ?
  AND attempts_started > 0
            "#,
        )
        .bind(now_ms)
        .bind(lease.authority.thread_id.to_string())
        .bind(&lease.authority.goal_id)
        .execute(&mut *transaction)
        .await?;
        if decremented.rows_affected() != 1 {
            bail!("released goal-owner admission is missing its chain attempt")
        }
        transaction.commit().await?;
        Ok(true)
    }

    /// Finish only the exact in-flight lease. An exact terminal replay is idempotent.
    /// Once a terminal outcome is recorded, its provenance is immutable; a late
    /// contradictory provider result is rejected rather than rewriting history.
    pub async fn finish(
        &self,
        lease: &GoalOwnerAdmissionLease,
        outcome: GoalOwnerAdmissionTerminalOutcome,
        disposition: GoalOwnerAdmissionTerminalDisposition,
    ) -> anyhow::Result<Option<GoalOwnerAdmissionRecord>> {
        validate_lease(lease)?;
        if matches!(
            outcome,
            GoalOwnerAdmissionTerminalOutcome::None
                | GoalOwnerAdmissionTerminalOutcome::Cancelled
                | GoalOwnerAdmissionTerminalOutcome::Exhausted
        ) {
            bail!("finish requires a provider terminal outcome")
        }
        validate_terminal_transition(outcome, disposition)?;
        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = fetch_record_for_authority(&mut *transaction, &lease.authority).await?;
        let Some(current) = current else {
            transaction.commit().await?;
            return Ok(None);
        };
        let completed = sqlx::query(
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
  AND lease_cancellation_epoch = ?
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
        .bind(lease.authority.cancellation_epoch)
        .execute(&mut *transaction)
        .await?;
        if completed.rows_affected() == 1 {
            let record = fetch_record_for_authority(&mut *transaction, &lease.authority)
                .await?
                .ok_or_else(|| anyhow::anyhow!("completed admission is missing"))?;
            transaction.commit().await?;
            return Ok(Some(record));
        }

        if exact_terminal_replay(&current, lease, outcome, disposition) {
            transaction.commit().await?;
            return Ok(Some(current));
        }
        transaction.commit().await?;
        if same_lease(&current, lease) {
            bail!("conflicting replay for goal-owner admission lease outcome")
        }
        Ok(None)
    }

    /// Increment the cancellation epoch and terminalize the exact durable generation.
    ///
    /// Dormant, pending, and acquired states are definitely cancelled because
    /// they have not crossed the provider boundary. In-flight work becomes
    /// uncertain and retains its lease for a same-lease late provider outcome.
    pub async fn cancel(
        &self,
        authority: &GoalOwnerAdmissionAuthority,
        disposition: GoalOwnerAdmissionTerminalDisposition,
    ) -> anyhow::Result<Option<GoalOwnerAdmissionRecord>> {
        validate_authority(authority)?;
        if disposition == GoalOwnerAdmissionTerminalDisposition::None {
            bail!("goal-owner cancellation requires an operator disposition")
        }
        let next_epoch = authority
            .cancellation_epoch
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("goal-owner admission cancellation epoch overflow"))?;
        let now_ms = admission_datetime_to_epoch_millis(Utc::now());
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(record) = fetch_record_for_authority(&mut *transaction, authority).await?
            && exact_cancellation_replay(&record, authority)
        {
            transaction.commit().await?;
            return Ok(Some(record));
        }
        let current = fetch_active_record(&mut *transaction, authority.thread_id).await?;
        let Some(current) = current else {
            transaction.commit().await?;
            return Ok(None);
        };
        if current.authority != authority.clone() {
            transaction.commit().await?;
            return Ok(None);
        }
        let cancelled = match current.phase {
            GoalOwnerAdmissionPhase::Dormant
            | GoalOwnerAdmissionPhase::Pending
            | GoalOwnerAdmissionPhase::Acquired => {
                sqlx::query(
                    r#"
UPDATE goal_owner_admissions
SET phase = 'terminal',
    terminal_outcome = 'cancelled',
    cancellation_epoch = ?,
    lease_id = NULL,
    lease_acquired_at_ms = NULL,
    lease_cancellation_epoch = NULL,
    dispatch_claim_id = NULL,
    dispatch_claimed_at_ms = NULL,
    deferred_terminal_disposition = 'await_user_turn',
    updated_at_ms = ?
WHERE thread_id = ?
  AND goal_id = ?
  AND generation = ?
  AND cancellation_epoch = ?
  AND phase IN ('dormant', 'pending', 'acquired')
  AND retired_at_ms IS NULL
                    "#,
                )
                .bind(next_epoch)
                .bind(now_ms)
                .bind(authority.thread_id.to_string())
                .bind(&authority.goal_id)
                .bind(authority.generation)
                .bind(authority.cancellation_epoch)
                .execute(&mut *transaction)
                .await?
            }
            GoalOwnerAdmissionPhase::InFlight => {
                sqlx::query(
                    r#"
UPDATE goal_owner_admissions
SET phase = 'terminal',
    terminal_outcome = 'uncertain',
    cancellation_epoch = ?,
    deferred_terminal_disposition = 'manual_review',
    updated_at_ms = ?
WHERE thread_id = ?
  AND goal_id = ?
  AND generation = ?
  AND cancellation_epoch = ?
  AND phase = 'in_flight'
  AND retired_at_ms IS NULL
                    "#,
                )
                .bind(next_epoch)
                .bind(now_ms)
                .bind(authority.thread_id.to_string())
                .bind(&authority.goal_id)
                .bind(authority.generation)
                .bind(authority.cancellation_epoch)
                .execute(&mut *transaction)
                .await?
            }
            GoalOwnerAdmissionPhase::Terminal => {
                transaction.commit().await?;
                return Ok(None);
            }
        };
        if cancelled.rows_affected() != 1 {
            bail!("goal-owner cancellation lost its exact current generation")
        }
        let record = fetch_record_by_generation(
            &mut *transaction,
            authority.thread_id,
            authority.generation,
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("cancelled admission is missing"))?;
        transaction.commit().await?;
        Ok(Some(record))
    }
}

macro_rules! admission_select_query {
    ($suffix:literal) => {
        sqlx::query(concat!(
            r#"
SELECT
    admission.thread_id AS admission_thread_id,
    admission.goal_id AS admission_goal_id,
    admission.generation AS admission_generation,
    admission.origin_turn_id AS admission_origin_turn_id,
    admission.origin_request_id AS admission_origin_request_id,
    admission.denial_class AS admission_denial_class,
    admission.configured_provider_key AS admission_configured_provider_key,
    admission.requested_model AS admission_requested_model,
    admission.effective_provider_id AS admission_effective_provider_id,
    admission.effective_model AS admission_effective_model,
    admission.intended_request_kind AS admission_intended_request_kind,
    admission.successor_turn_id AS admission_successor_turn_id,
    admission.logical_successor_request_id AS admission_logical_successor_request_id,
    admission.decision_id AS admission_decision_id,
    admission.account_context_fingerprint AS admission_account_context_fingerprint,
    admission.deadline_at_ms AS admission_deadline_at_ms,
    admission.attempts_started AS admission_attempts_started,
    admission.max_attempts AS admission_max_attempts,
    admission.cancellation_epoch AS admission_cancellation_epoch,
    admission.requested_phase AS admission_requested_phase,
    admission.phase AS admission_phase,
    admission.terminal_outcome AS admission_terminal_outcome,
    admission.lease_id AS admission_lease_id,
    admission.lease_acquired_at_ms AS admission_lease_acquired_at_ms,
    admission.lease_cancellation_epoch AS admission_lease_cancellation_epoch,
    admission.dispatch_claim_id AS admission_dispatch_claim_id,
    admission.dispatch_claimed_at_ms AS admission_dispatch_claimed_at_ms,
    admission.deferred_terminal_disposition AS admission_deferred_terminal_disposition,
    admission.retired_at_ms AS admission_retired_at_ms,
    admission.retirement_reason AS admission_retirement_reason,
    admission.created_at_ms AS admission_created_at_ms,
    admission.updated_at_ms AS admission_updated_at_ms,
    origin.thread_id AS origin_thread_id,
    origin.origin_request_id AS origin_origin_request_id,
    origin.generation AS origin_generation,
    origin.goal_id AS origin_goal_id,
    origin.origin_turn_id AS origin_origin_turn_id,
    origin.denial_class AS origin_denial_class,
    origin.configured_provider_key AS origin_configured_provider_key,
    origin.requested_model AS origin_requested_model,
    origin.effective_provider_id AS origin_effective_provider_id,
    origin.effective_model AS origin_effective_model,
    origin.intended_request_kind AS origin_intended_request_kind,
    origin.successor_turn_id AS origin_successor_turn_id,
    origin.logical_successor_request_id AS origin_logical_successor_request_id,
    origin.decision_id AS origin_decision_id,
    origin.account_context_fingerprint AS origin_account_context_fingerprint,
    origin.deadline_at_ms AS origin_deadline_at_ms,
    origin.max_attempts AS origin_max_attempts,
    origin.requested_phase AS origin_requested_phase,
    chain.thread_id AS chain_thread_id,
    chain.goal_id AS chain_goal_id,
    chain.attempts_started AS chain_attempts_started,
    chain.max_attempts AS chain_max_attempts,
    chain.created_at_ms AS chain_created_at_ms,
    chain.updated_at_ms AS chain_updated_at_ms
FROM goal_owner_admissions AS admission
LEFT JOIN goal_owner_admission_origins AS origin
  ON origin.thread_id = admission.thread_id
 AND origin.generation = admission.generation
LEFT JOIN goal_owner_admission_goal_chains AS chain
  ON chain.thread_id = admission.thread_id AND chain.goal_id = admission.goal_id
"#,
            $suffix
        ))
    };
}

fn recoverable_admissions_query()
-> sqlx::query::Query<'static, Sqlite, sqlx::sqlite::SqliteArguments> {
    admission_select_query!(
        " WHERE (admission.phase = 'acquired' AND admission.retired_at_ms IS NULL) OR admission.phase = 'in_flight'"
    )
}

async fn ensure_goal_chain(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    observation: &GoalOwnerAdmissionObservation,
    now_ms: i64,
) -> anyhow::Result<()> {
    let existing = sqlx::query_scalar::<_, i64>(
        "SELECT max_attempts FROM goal_owner_admission_goal_chains WHERE thread_id = ? AND goal_id = ?",
    )
    .bind(observation.thread_id.to_string())
    .bind(&observation.goal_id)
    .fetch_optional(&mut **transaction)
    .await?;
    if let Some(max_attempts) = existing {
        if max_attempts != observation.max_attempts {
            bail!("goal-owner admission chain max attempts conflicts with immutable history")
        }
        return Ok(());
    }
    sqlx::query(
        r#"
INSERT INTO goal_owner_admission_goal_chains (
    thread_id, goal_id, max_attempts, created_at_ms, updated_at_ms
) VALUES (?, ?, ?, ?, ?)
        "#,
    )
    .bind(observation.thread_id.to_string())
    .bind(&observation.goal_id)
    .bind(observation.max_attempts)
    .bind(now_ms)
    .bind(now_ms)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn next_generation(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    thread_id: ThreadId,
) -> anyhow::Result<i64> {
    let latest = sqlx::query_scalar::<_, i64>(
        r#"
SELECT COALESCE(MAX(generation), 0)
FROM (
    SELECT generation FROM goal_owner_admissions WHERE thread_id = ?
    UNION ALL
    SELECT generation FROM goal_owner_admission_origins WHERE thread_id = ?
)
        "#,
    )
    .bind(thread_id.to_string())
    .bind(thread_id.to_string())
    .fetch_one(&mut **transaction)
    .await?;
    latest
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("goal-owner admission generation overflow"))
}

async fn insert_admission(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    origin: &GoalOwnerAdmissionOrigin,
    now_ms: i64,
) -> anyhow::Result<()> {
    let inserted = sqlx::query(
        r#"
INSERT INTO goal_owner_admissions (
    thread_id, goal_id, generation, origin_turn_id, origin_request_id, denial_class,
    configured_provider_key, requested_model, effective_provider_id, effective_model,
    intended_request_kind, successor_turn_id, logical_successor_request_id, decision_id,
    account_context_fingerprint, deadline_at_ms, attempts_started, max_attempts,
    cancellation_epoch, requested_phase, phase, terminal_outcome, lease_id,
    lease_acquired_at_ms, lease_cancellation_epoch, deferred_terminal_disposition,
    retired_at_ms, retirement_reason, created_at_ms, updated_at_ms
)
SELECT
    origin.thread_id,
    origin.goal_id,
    origin.generation,
    origin.origin_turn_id,
    origin.origin_request_id,
    origin.denial_class,
    origin.configured_provider_key,
    origin.requested_model,
    origin.effective_provider_id,
    origin.effective_model,
    origin.intended_request_kind,
    origin.successor_turn_id,
    origin.logical_successor_request_id,
    origin.decision_id,
    origin.account_context_fingerprint,
    origin.deadline_at_ms,
    0,
    origin.max_attempts,
    0,
    origin.requested_phase,
    origin.requested_phase,
    'none',
    NULL,
    NULL,
    NULL,
    'none',
    NULL,
    NULL,
    ?,
    ?
FROM goal_owner_admission_origins AS origin
WHERE origin.thread_id = ?
  AND origin.generation = ?
  AND origin.origin_request_id = ?
        "#,
    )
    .bind(now_ms)
    .bind(now_ms)
    .bind(origin.thread_id.to_string())
    .bind(origin.generation)
    .bind(&origin.origin_request_id)
    .execute(&mut **transaction)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(admission_integrity_error());
    }
    Ok(())
}

async fn insert_origin(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    observation: &GoalOwnerAdmissionObservation,
    generation: i64,
) -> anyhow::Result<()> {
    let origin = origin_from_observation(observation, generation);
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
    .bind(observation.thread_id.to_string())
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
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn fetch_active_record<'e, E>(
    executor: E,
    thread_id: ThreadId,
) -> anyhow::Result<Option<GoalOwnerAdmissionRecord>>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let row = admission_select_query!(
        " WHERE admission.thread_id = ? AND admission.retired_at_ms IS NULL"
    )
    .bind(thread_id.to_string())
    .fetch_optional(executor)
    .await?;
    row.map(|row| record_from_row(&row)).transpose()
}

async fn fetch_record_by_generation<'e, E>(
    executor: E,
    thread_id: ThreadId,
    generation: i64,
) -> anyhow::Result<Option<GoalOwnerAdmissionRecord>>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let row =
        admission_select_query!(" WHERE admission.thread_id = ? AND admission.generation = ?")
            .bind(thread_id.to_string())
            .bind(generation)
            .fetch_optional(executor)
            .await?;
    row.map(|row| record_from_row(&row)).transpose()
}

async fn fetch_record_for_authority<'e, E>(
    executor: E,
    authority: &GoalOwnerAdmissionAuthority,
) -> anyhow::Result<Option<GoalOwnerAdmissionRecord>>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let row =
        admission_select_query!(" WHERE admission.thread_id = ? AND admission.generation = ?")
            .bind(authority.thread_id.to_string())
            .bind(authority.generation)
            .fetch_optional(executor)
            .await?;
    let record = row.map(|row| record_from_row(&row)).transpose()?;
    Ok(record.filter(|record| record.authority.goal_id == authority.goal_id))
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
SELECT thread_id, generation, goal_id, origin_turn_id, origin_request_id, denial_class,
       configured_provider_key, requested_model, effective_provider_id, effective_model,
       intended_request_kind, successor_turn_id, logical_successor_request_id, decision_id,
       account_context_fingerprint, deadline_at_ms, max_attempts, requested_phase
FROM goal_owner_admission_origins
WHERE thread_id = ? AND origin_request_id = ?
        "#,
    )
    .bind(thread_id.to_string())
    .bind(origin_request_id)
    .fetch_optional(executor)
    .await?;
    row.map(|row| origin_from_row(&row))
        .transpose()
        .map_err(|_| admission_integrity_error())
}

fn admission_integrity_error() -> anyhow::Error {
    anyhow::anyhow!("goal-owner admission integrity error")
}

fn record_from_row(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<GoalOwnerAdmissionRecord> {
    (|| -> anyhow::Result<GoalOwnerAdmissionRecord> {
        let origin = GoalOwnerAdmissionOrigin {
            thread_id: ThreadId::try_from(row.try_get::<String, _>("origin_thread_id")?)?,
            generation: row.try_get("origin_generation")?,
            goal_id: row.try_get("origin_goal_id")?,
            origin_turn_id: row.try_get("origin_origin_turn_id")?,
            origin_request_id: row.try_get("origin_origin_request_id")?,
            denial_class: GoalOwnerAdmissionDenialClass::try_from(
                row.try_get::<String, _>("origin_denial_class")?.as_str(),
            )?,
            configured_provider_key: row.try_get("origin_configured_provider_key")?,
            requested_model: row.try_get("origin_requested_model")?,
            effective_provider_id: row.try_get("origin_effective_provider_id")?,
            effective_model: row.try_get("origin_effective_model")?,
            intended_request_kind: row.try_get("origin_intended_request_kind")?,
            successor_turn_id: row.try_get("origin_successor_turn_id")?,
            logical_successor_request_id: row.try_get("origin_logical_successor_request_id")?,
            decision_id: Uuid::parse_str(&row.try_get::<String, _>("origin_decision_id")?)?,
            account_context_fingerprint: row
                .try_get::<Option<String>, _>("origin_account_context_fingerprint")?
                .map(GoalOwnerAdmissionAccountContextFingerprint::try_from)
                .transpose()?,
            deadline_at_ms: row.try_get("origin_deadline_at_ms")?,
            max_attempts: row.try_get("origin_max_attempts")?,
            requested_phase: GoalOwnerAdmissionPhase::try_from(
                row.try_get::<String, _>("origin_requested_phase")?.as_str(),
            )?,
        };
        let chain = GoalOwnerAdmissionGoalChain {
            thread_id: ThreadId::try_from(row.try_get::<String, _>("chain_thread_id")?)?,
            goal_id: row.try_get("chain_goal_id")?,
            attempts_started: row.try_get("chain_attempts_started")?,
            max_attempts: row.try_get("chain_max_attempts")?,
            created_at: admission_epoch_millis_to_datetime(row.try_get("chain_created_at_ms")?)?,
            updated_at: admission_epoch_millis_to_datetime(row.try_get("chain_updated_at_ms")?)?,
        };
        let lease_id: Option<String> = row.try_get("admission_lease_id")?;
        let lease_acquired_at_ms: Option<i64> = row.try_get("admission_lease_acquired_at_ms")?;
        let dispatch_claim_id: Option<String> = row.try_get("admission_dispatch_claim_id")?;
        let dispatch_claimed_at_ms: Option<i64> =
            row.try_get("admission_dispatch_claimed_at_ms")?;
        let retired_at_ms: Option<i64> = row.try_get("admission_retired_at_ms")?;
        let retirement_reason: Option<String> = row.try_get("admission_retirement_reason")?;
        let record = GoalOwnerAdmissionRecord {
            authority: GoalOwnerAdmissionAuthority {
                thread_id: ThreadId::try_from(row.try_get::<String, _>("admission_thread_id")?)?,
                goal_id: row.try_get("admission_goal_id")?,
                generation: row.try_get("admission_generation")?,
                cancellation_epoch: row.try_get("admission_cancellation_epoch")?,
            },
            origin_turn_id: row.try_get("admission_origin_turn_id")?,
            origin_request_id: row.try_get("admission_origin_request_id")?,
            denial_class: GoalOwnerAdmissionDenialClass::try_from(
                row.try_get::<String, _>("admission_denial_class")?.as_str(),
            )?,
            configured_provider_key: row.try_get("admission_configured_provider_key")?,
            requested_model: row.try_get("admission_requested_model")?,
            effective_provider_id: row.try_get("admission_effective_provider_id")?,
            effective_model: row.try_get("admission_effective_model")?,
            intended_request_kind: row.try_get("admission_intended_request_kind")?,
            successor_turn_id: row.try_get("admission_successor_turn_id")?,
            logical_successor_request_id: row.try_get("admission_logical_successor_request_id")?,
            decision_id: Uuid::parse_str(&row.try_get::<String, _>("admission_decision_id")?)?,
            account_context_fingerprint: row
                .try_get::<Option<String>, _>("admission_account_context_fingerprint")?
                .map(GoalOwnerAdmissionAccountContextFingerprint::try_from)
                .transpose()?,
            deadline_at: admission_epoch_millis_to_datetime(
                row.try_get("admission_deadline_at_ms")?,
            )?,
            attempts_started: row.try_get("admission_attempts_started")?,
            max_attempts: row.try_get("admission_max_attempts")?,
            chain_attempts_started: chain.attempts_started,
            chain_max_attempts: chain.max_attempts,
            requested_phase: GoalOwnerAdmissionPhase::try_from(
                row.try_get::<String, _>("admission_requested_phase")?
                    .as_str(),
            )?,
            phase: GoalOwnerAdmissionPhase::try_from(
                row.try_get::<String, _>("admission_phase")?.as_str(),
            )?,
            terminal_outcome: GoalOwnerAdmissionTerminalOutcome::try_from(
                row.try_get::<String, _>("admission_terminal_outcome")?
                    .as_str(),
            )?,
            lease_id: lease_id.map(|value| Uuid::parse_str(&value)).transpose()?,
            lease_acquired_at: lease_acquired_at_ms
                .map(admission_epoch_millis_to_datetime)
                .transpose()?,
            lease_cancellation_epoch: row.try_get("admission_lease_cancellation_epoch")?,
            dispatch_claim_id: dispatch_claim_id
                .map(|value| Uuid::parse_str(&value))
                .transpose()?,
            dispatch_claimed_at: dispatch_claimed_at_ms
                .map(admission_epoch_millis_to_datetime)
                .transpose()?,
            deferred_terminal_disposition: GoalOwnerAdmissionTerminalDisposition::try_from(
                row.try_get::<String, _>("admission_deferred_terminal_disposition")?
                    .as_str(),
            )?,
            retired_at: retired_at_ms
                .map(admission_epoch_millis_to_datetime)
                .transpose()?,
            retirement_reason: retirement_reason
                .as_deref()
                .map(GoalOwnerAdmissionRetirementReason::try_from)
                .transpose()?,
            created_at: admission_epoch_millis_to_datetime(
                row.try_get("admission_created_at_ms")?,
            )?,
            updated_at: admission_epoch_millis_to_datetime(
                row.try_get("admission_updated_at_ms")?,
            )?,
        };
        validate_origin(&origin)?;
        validate_goal_chain(&chain)?;
        validate_record(&record)?;
        validate_joined_admission(&record, &origin, &chain)?;
        Ok(record)
    })()
    .map_err(|_| admission_integrity_error())
}

fn origin_from_row(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<GoalOwnerAdmissionOrigin> {
    let origin = GoalOwnerAdmissionOrigin {
        thread_id: ThreadId::try_from(row.try_get::<String, _>("thread_id")?)?,
        generation: row.try_get("generation")?,
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
    if record.phase != GoalOwnerAdmissionPhase::Acquired
        || record.lease_cancellation_epoch != Some(record.authority.cancellation_epoch)
    {
        bail!("acquired goal-owner admission is not reserved")
    }
    Ok(GoalOwnerAdmissionLease {
        continuation_authority: record.continuation_authority(),
        authority: record.authority,
        lease_id: record
            .lease_id
            .ok_or_else(|| anyhow::anyhow!("acquired goal-owner admission is missing its lease"))?,
        acquired_at: record.lease_acquired_at.ok_or_else(|| {
            anyhow::anyhow!("acquired goal-owner admission is missing its lease timestamp")
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
    validate_origin(&origin_from_observation(observation, /*generation*/ 1))?;
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
    generation: i64,
) -> GoalOwnerAdmissionOrigin {
    GoalOwnerAdmissionOrigin {
        thread_id: observation.thread_id,
        generation,
        goal_id: observation.goal_id.clone(),
        origin_turn_id: observation.origin_turn_id.clone(),
        origin_request_id: observation.origin_request_id.clone(),
        denial_class: observation.denial_class,
        configured_provider_key: observation.configured_provider_key.clone(),
        requested_model: observation.requested_model.clone(),
        effective_provider_id: observation
            .effective_provider_id
            .as_deref()
            .map(canonical_provider_id),
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
    if origin.generation < 1 {
        bail!("invalid goal-owner admission generation")
    }
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

fn validate_goal_chain(chain: &GoalOwnerAdmissionGoalChain) -> anyhow::Result<()> {
    validate_nonempty("goal id", &chain.goal_id, MAX_ORIGIN_ID_LENGTH)?;
    if chain.attempts_started < 0
        || chain.max_attempts < 1
        || chain.attempts_started > chain.max_attempts
        || chain.created_at > chain.updated_at
    {
        bail!("invalid goal-owner admission chain state")
    }
    Ok(())
}

fn validate_joined_admission(
    record: &GoalOwnerAdmissionRecord,
    origin: &GoalOwnerAdmissionOrigin,
    chain: &GoalOwnerAdmissionGoalChain,
) -> anyhow::Result<()> {
    if record.authority.thread_id != origin.thread_id
        || record.authority.generation != origin.generation
        || record.authority.goal_id != origin.goal_id
        || record.origin_turn_id != origin.origin_turn_id
        || record.origin_request_id != origin.origin_request_id
        || record.denial_class != origin.denial_class
        || record.configured_provider_key != origin.configured_provider_key
        || record.requested_model != origin.requested_model
        || record.effective_provider_id != origin.effective_provider_id
        || record.effective_model != origin.effective_model
        || record.intended_request_kind != origin.intended_request_kind
        || record.successor_turn_id != origin.successor_turn_id
        || record.logical_successor_request_id != origin.logical_successor_request_id
        || record.decision_id != origin.decision_id
        || record.account_context_fingerprint != origin.account_context_fingerprint
        || admission_datetime_to_epoch_millis(record.deadline_at) != origin.deadline_at_ms
        || record.max_attempts != origin.max_attempts
        || record.requested_phase != origin.requested_phase
        || record.authority.thread_id != chain.thread_id
        || record.authority.goal_id != chain.goal_id
        || record.chain_attempts_started != chain.attempts_started
        || record.chain_max_attempts != chain.max_attempts
        || record.max_attempts != chain.max_attempts
        || record.attempts_started > chain.attempts_started
    {
        bail!("goal-owner admission joined state is inconsistent")
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

fn validate_lease(lease: &GoalOwnerAdmissionLease) -> anyhow::Result<()> {
    validate_authority(&lease.authority)?;
    validate_continuation_authority(&lease.continuation_authority)?;
    if lease.continuation_authority.authority != lease.authority {
        bail!("goal-owner admission lease has contradictory authority")
    }
    Ok(())
}

fn admission_datetime_to_epoch_millis(value: DateTime<Utc>) -> i64 {
    value.timestamp_millis()
}

fn admission_epoch_millis_to_datetime(value: i64) -> anyhow::Result<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_millis(value)
        .ok_or_else(|| anyhow::anyhow!("invalid goal-owner admission timestamp"))
}

fn validate_terminal_transition(
    outcome: GoalOwnerAdmissionTerminalOutcome,
    disposition: GoalOwnerAdmissionTerminalDisposition,
) -> anyhow::Result<()> {
    match outcome {
        GoalOwnerAdmissionTerminalOutcome::Succeeded
        | GoalOwnerAdmissionTerminalOutcome::Rejected
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

fn retirement_is_unsettled(record: &GoalOwnerAdmissionRecord) -> bool {
    matches!(
        record.phase,
        GoalOwnerAdmissionPhase::Acquired | GoalOwnerAdmissionPhase::InFlight
    ) || (record.phase == GoalOwnerAdmissionPhase::Terminal
        && record.terminal_outcome == GoalOwnerAdmissionTerminalOutcome::Uncertain)
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
        || record.chain_attempts_started < 0
        || record.chain_max_attempts < 1
        || record.chain_attempts_started > record.chain_max_attempts
        || record.attempts_started > record.chain_attempts_started
        || record.max_attempts != record.chain_max_attempts
    {
        bail!("invalid goal-owner admission attempt counters")
    }
    let has_lease = record.lease_id.is_some();
    let has_dispatch_claim = record.dispatch_claim_id.is_some();
    if has_lease != record.lease_acquired_at.is_some()
        || has_lease != record.lease_cancellation_epoch.is_some()
        || has_dispatch_claim != record.dispatch_claimed_at.is_some()
        || record.retired_at.is_some() != record.retirement_reason.is_some()
    {
        bail!("contradictory goal-owner admission durable state")
    }
    if record.retired_at.is_some() && retirement_is_unsettled(record) {
        bail!("retired goal-owner admission has unsettled work")
    }
    match record.phase {
        GoalOwnerAdmissionPhase::Dormant | GoalOwnerAdmissionPhase::Pending => {
            if record.requested_phase != record.phase
                || record.terminal_outcome != GoalOwnerAdmissionTerminalOutcome::None
                || has_lease
                || record.deferred_terminal_disposition
                    != GoalOwnerAdmissionTerminalDisposition::None
            {
                bail!("contradictory non-terminal goal-owner admission state")
            }
            if record.phase == GoalOwnerAdmissionPhase::Dormant && has_dispatch_claim {
                bail!("dormant goal-owner admission cannot have a dispatch claim")
            }
        }
        GoalOwnerAdmissionPhase::Acquired | GoalOwnerAdmissionPhase::InFlight => {
            if record.terminal_outcome != GoalOwnerAdmissionTerminalOutcome::None
                || !has_lease
                || has_dispatch_claim
                || record.attempts_started == 0
                || record.lease_cancellation_epoch != Some(record.authority.cancellation_epoch)
                || record.deferred_terminal_disposition
                    != GoalOwnerAdmissionTerminalDisposition::None
            {
                bail!("contradictory leased goal-owner admission state")
            }
        }
        GoalOwnerAdmissionPhase::Terminal => {
            if has_dispatch_claim {
                bail!("terminal goal-owner admission cannot have a dispatch claim")
            }
            if record.terminal_outcome == GoalOwnerAdmissionTerminalOutcome::None
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
        | GoalOwnerAdmissionTerminalOutcome::Rejected => {
            record.attempts_started > 0
                && has_lease
                && record.deferred_terminal_disposition
                    == GoalOwnerAdmissionTerminalDisposition::None
                && record
                    .lease_cancellation_epoch
                    .is_some_and(|epoch| epoch <= record.authority.cancellation_epoch)
        }
        GoalOwnerAdmissionTerminalOutcome::Uncertain => {
            record.attempts_started > 0
                && has_lease
                && record.deferred_terminal_disposition
                    == GoalOwnerAdmissionTerminalDisposition::ManualReview
                && record
                    .lease_cancellation_epoch
                    .is_some_and(|epoch| epoch <= record.authority.cancellation_epoch)
        }
        GoalOwnerAdmissionTerminalOutcome::Exhausted => {
            !has_lease
                && record.chain_attempts_started == record.chain_max_attempts
                && record.deferred_terminal_disposition
                    == GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn
        }
        GoalOwnerAdmissionTerminalOutcome::Cancelled => {
            !has_lease
                && record.deferred_terminal_disposition
                    == GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn
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
        && canonical_optional_provider_id(observation.effective_provider_id.as_deref())
            == canonical_optional_provider_id(origin.effective_provider_id.as_deref())
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

fn canonical_optional_provider_id(value: Option<&str>) -> Option<String> {
    value.map(canonical_provider_id)
}

fn same_lease(record: &GoalOwnerAdmissionRecord, lease: &GoalOwnerAdmissionLease) -> bool {
    record.authority.thread_id == lease.authority.thread_id
        && record.authority.goal_id == lease.authority.goal_id
        && record.authority.generation == lease.authority.generation
        && record.lease_id == Some(lease.lease_id)
        && record.lease_cancellation_epoch == Some(lease.authority.cancellation_epoch)
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
        && matches!(
            (
                record.terminal_outcome,
                record.deferred_terminal_disposition
            ),
            (
                GoalOwnerAdmissionTerminalOutcome::Cancelled,
                GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn
            ) | (
                GoalOwnerAdmissionTerminalOutcome::Uncertain,
                GoalOwnerAdmissionTerminalDisposition::ManualReview
            )
        )
}

#[cfg(test)]
#[path = "goal_owner_admissions_tests.rs"]
mod tests;
