//! Durable admission for Responses model-inference requests.
//!
//! A lease from the goal-owner admission ledger represents one, and only one,
//! physical inference request.  This module deliberately sits below turn and
//! compaction orchestration: callers must name the kind of work they are about
//! to send, and transports receive a lease guard immediately before network
//! I/O.  That keeps retries and transport fallbacks from silently replaying a
//! continuation whose provider effect is no longer knowable.

use std::fmt;
use std::sync::Arc;

use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::protocol::SessionSource;
use codex_state::GoalOwnerAdmissionContinuationAuthority;
use codex_state::GoalOwnerAdmissionLease;
use codex_state::GoalOwnerAdmissionPhase;
use codex_state::GoalOwnerAdmissionRecord;
use codex_state::GoalOwnerAdmissionStore;
use codex_state::GoalOwnerAdmissionTerminalDisposition;
use codex_state::GoalOwnerAdmissionTerminalOutcome;
use tokio::sync::Mutex;
use tracing::debug;
use tracing::warn;
use uuid::Uuid;

use crate::StateDbHandle;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InferenceRequestKind {
    Turn,
    LocalCompaction,
    RemoteCompactionV2,
    RemoteCompact,
}

impl InferenceRequestKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::LocalCompaction => "local_compaction",
            Self::RemoteCompactionV2 => "remote_compaction_v2",
            Self::RemoteCompact => "remote_compact",
        }
    }
}

/// The internal transport purpose. `Prewarm` is intentionally not an
/// inference kind and is only constructible inside the private warmup path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModelRequestKind {
    Inference(InferenceRequestKind),
    Prewarm,
}

impl ModelRequestKind {
    pub(crate) const fn is_inference(self) -> bool {
        matches!(self, Self::Inference(_))
    }

    const fn inference_kind(self) -> Option<InferenceRequestKind> {
        match self {
            Self::Inference(kind) => Some(kind),
            Self::Prewarm => None,
        }
    }
}

/// Core-owned identity for one logical Responses request.
///
/// This is intentionally broader than the durable ledger key: the ledger is
/// per thread, while the remaining fields make a dispatched request auditable
/// and preserve the distinction between the requested and effective model.
#[derive(Clone, Debug)]
pub(crate) struct ModelRequestIdentity {
    pub(crate) thread_id: ThreadId,
    pub(crate) turn_id: Option<String>,
    pub(crate) logical_request_id: String,
    pub(crate) kind: ModelRequestKind,
    pub(crate) configured_provider_key: String,
    pub(crate) configured_requested_model: Option<String>,
    pub(crate) effective_provider_id: String,
    pub(crate) effective_model: String,
    pub(crate) service_tier: Option<String>,
    pub(crate) session_source: SessionSource,
    pub(crate) parent_continuity_decision_id: Option<Uuid>,
}

impl ModelRequestIdentity {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn inference(
        thread_id: ThreadId,
        turn_id: Option<String>,
        kind: InferenceRequestKind,
        configured_provider_key: String,
        configured_requested_model: Option<String>,
        effective_provider_id: String,
        effective_model: String,
        service_tier: Option<String>,
        session_source: SessionSource,
        parent_continuity_decision_id: Option<Uuid>,
        logical_request_id: Option<String>,
    ) -> Self {
        Self::new(
            thread_id,
            turn_id,
            ModelRequestKind::Inference(kind),
            configured_provider_key,
            configured_requested_model,
            effective_provider_id,
            effective_model,
            service_tier,
            session_source,
            parent_continuity_decision_id,
            logical_request_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prewarm(
        thread_id: ThreadId,
        turn_id: Option<String>,
        configured_provider_key: String,
        configured_requested_model: Option<String>,
        effective_provider_id: String,
        effective_model: String,
        service_tier: Option<String>,
        session_source: SessionSource,
    ) -> Self {
        Self::new(
            thread_id,
            turn_id,
            ModelRequestKind::Prewarm,
            configured_provider_key,
            configured_requested_model,
            effective_provider_id,
            effective_model,
            service_tier,
            session_source,
            None,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        thread_id: ThreadId,
        turn_id: Option<String>,
        kind: ModelRequestKind,
        configured_provider_key: String,
        configured_requested_model: Option<String>,
        effective_provider_id: String,
        effective_model: String,
        service_tier: Option<String>,
        session_source: SessionSource,
        parent_continuity_decision_id: Option<Uuid>,
        logical_request_id: Option<String>,
    ) -> Self {
        Self {
            thread_id,
            turn_id,
            logical_request_id: logical_request_id.unwrap_or_else(|| Uuid::now_v7().to_string()),
            kind,
            configured_provider_key,
            configured_requested_model,
            effective_provider_id,
            effective_model,
            service_tier,
            session_source,
            parent_continuity_decision_id,
        }
    }
}

/// A typed result from the durable admission gate.
///
/// `Dormant` is deliberately fail-closed. It includes in-flight leases,
/// uncertain terminal effects, malformed/unknown terminal states, and an
/// eligible pending record whose compare-and-swap was lost to another owner.
#[derive(Clone)]
pub(crate) enum ModelRequestAdmissionDecision {
    Unrestricted,
    Admitted(Arc<AdmittedModelRequest>),
    Deferred,
    Dormant,
    Exhausted,
    Cancelled,
}

impl ModelRequestAdmissionDecision {
    pub(crate) fn is_admitted(&self) -> bool {
        matches!(self, Self::Admitted(_))
    }

    pub(crate) async fn begin_network_request(&self) -> Result<ModelRequestLeaseGuard> {
        match self {
            Self::Unrestricted => Ok(ModelRequestLeaseGuard::unrestricted()),
            Self::Admitted(admitted) => {
                {
                    let mut lifecycle = admitted.lifecycle.lock().await;
                    if lifecycle.opening || lifecycle.request_opened {
                        return Err(CodexErr::Fatal(
                            "refusing a second physical request for one goal-owner admission lease"
                                .to_string(),
                        ));
                    }
                    if lifecycle.terminalized {
                        return Err(CodexErr::Fatal(
                            "goal-owner admission lease is already terminal".to_string(),
                        ));
                    }
                    lifecycle.opening = true;
                }

                let opened = admitted
                    .store
                    .open_lease(&admitted.lease)
                    .await
                    .map_err(storage_error)?;
                let mut lifecycle = admitted.lifecycle.lock().await;
                lifecycle.opening = false;
                if !opened {
                    lifecycle.terminalized = true;
                    lifecycle.stage = LeaseStage::CancelledBeforeAcknowledgement;
                    return Err(CodexErr::Fatal(
                        "goal-owner admission was cancelled before its request-open fence"
                            .to_string(),
                    ));
                }
                if lifecycle.terminalized {
                    return Err(CodexErr::Fatal(
                        "goal-owner admission lease was terminalized while opening".to_string(),
                    ));
                }
                lifecycle.request_opened = true;
                lifecycle.stage = LeaseStage::RequestOpened;
                Ok(ModelRequestLeaseGuard::admitted(Arc::clone(admitted)))
            }
            decision => Err(decision.blocked_error()),
        }
    }

    /// Release an acquired pre-network lease when local setup fails. If the
    /// request-open fence has already won, preserve conservative terminal
    /// handling instead.
    pub(crate) async fn terminalize_if_unfinished(&self) {
        let Self::Admitted(admitted) = self else {
            return;
        };
        let request_opened = admitted.lifecycle.lock().await.request_opened;
        let result = if request_opened {
            admitted
                .finish_if_unfinished(
                    LeaseStage::CancelledBeforeAcknowledgement,
                    GoalOwnerAdmissionTerminalOutcome::Uncertain,
                    GoalOwnerAdmissionTerminalDisposition::ManualReview,
                )
                .await
        } else {
            admitted.release_if_unopened().await
        };
        if let Err(error) = result {
            warn!(error = %error, lease_id = %admitted.lease.lease_id, "failed to release or terminalize a goal-owner admission lease");
        }
    }

    pub(crate) fn terminal_error(&self, reason: &'static str) -> CodexErr {
        CodexErr::Fatal(format!("goal-owner admitted request is terminal: {reason}"))
    }

    fn blocked_error(&self) -> CodexErr {
        let reason = match self {
            Self::Unrestricted | Self::Admitted(_) => unreachable!("permitted admission"),
            Self::Deferred => "goal-owner admission remains deferred",
            Self::Dormant => "goal-owner admission is dormant until explicit recovery",
            Self::Exhausted => "goal-owner admission attempt budget is exhausted",
            Self::Cancelled => "goal-owner admission was cancelled",
        };
        CodexErr::Fatal(reason.to_string())
    }
}

impl fmt::Debug for ModelRequestAdmissionDecision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unrestricted => formatter.write_str("Unrestricted"),
            Self::Admitted(admitted) => formatter
                .debug_tuple("Admitted")
                .field(&admitted.lease.lease_id)
                .finish(),
            Self::Deferred => formatter.write_str("Deferred"),
            Self::Dormant => formatter.write_str("Dormant"),
            Self::Exhausted => formatter.write_str("Exhausted"),
            Self::Cancelled => formatter.write_str("Cancelled"),
        }
    }
}

/// Consults the optional local state runtime before each Responses inference request.
#[derive(Clone)]
pub(crate) struct ModelRequestAdmissionBroker {
    state_db: Option<StateDbHandle>,
}

impl ModelRequestAdmissionBroker {
    pub(crate) fn new(state_db: Option<StateDbHandle>) -> Self {
        Self { state_db }
    }

    pub(crate) async fn admit(
        &self,
        identity: &ModelRequestIdentity,
        continuation_authority: Option<&GoalOwnerAdmissionContinuationAuthority>,
    ) -> Result<ModelRequestAdmissionDecision> {
        debug!(
            thread_id = %identity.thread_id,
            turn_id = ?identity.turn_id,
            logical_request_id = %identity.logical_request_id,
            request_kind = ?identity.kind,
            configured_provider_key = %identity.configured_provider_key,
            configured_requested_model = ?identity.configured_requested_model,
            effective_provider_id = %identity.effective_provider_id,
            effective_model = %identity.effective_model,
            service_tier = ?identity.service_tier,
            session_source = %identity.session_source,
            parent_continuity_decision_id = ?identity.parent_continuity_decision_id,
            "evaluating model request admission"
        );

        // Prewarm has a concrete, typed non-inference purpose. No other kind
        // bypasses the ledger, including memory/review/child turns that are
        // represented by `Turn`.
        if !identity.kind.is_inference() {
            return Ok(ModelRequestAdmissionDecision::Unrestricted);
        }
        let Some(state_db) = &self.state_db else {
            return Ok(ModelRequestAdmissionDecision::Unrestricted);
        };
        let store = state_db.goal_owner_admissions();
        let now = Utc::now();
        let record = store.get(identity.thread_id).await.map_err(storage_error)?;
        let Some(record) = record else {
            return Ok(ModelRequestAdmissionDecision::Unrestricted);
        };

        // A prior successful continuation no longer restricts the thread,
        // regardless of later model/provider resolution.
        if record.phase == GoalOwnerAdmissionPhase::Terminal {
            return Ok(decision_for_record(&record, now));
        }
        if !record_matches_identity(&record, identity) {
            // A continuation lease is scoped to the provider/model evidence
            // that produced the original denial. Missing or different
            // evidence is not permission to send a different request.
            return Ok(ModelRequestAdmissionDecision::Dormant);
        }

        let Some(continuation_authority) = continuation_authority else {
            return Ok(ModelRequestAdmissionDecision::Dormant);
        };
        if !continuation_matches_record(continuation_authority, &record)
            || !continuation_matches_identity(continuation_authority, identity)
        {
            return Ok(ModelRequestAdmissionDecision::Dormant);
        }

        if record.phase != GoalOwnerAdmissionPhase::Pending {
            return Ok(decision_for_record(&record, now));
        }
        if record.deadline_at > now {
            return Ok(ModelRequestAdmissionDecision::Deferred);
        }

        let lease = store
            .try_acquire(continuation_authority, now)
            .await
            .map_err(storage_error)?;
        if let Some(lease) = lease {
            return Ok(ModelRequestAdmissionDecision::Admitted(Arc::new(
                AdmittedModelRequest::new(store.clone(), lease),
            )));
        }

        // A CAS miss means another owner may have opened the exact request. A
        // fresh read can expose a final state, but a still-pending result is
        // not permission to try again from this process.
        let current = store.get(identity.thread_id).await.map_err(storage_error)?;
        Ok(
            current.map_or(ModelRequestAdmissionDecision::Dormant, |record| {
                decision_for_record(&record, now)
            }),
        )
    }
}

impl fmt::Debug for ModelRequestAdmissionBroker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelRequestAdmissionBroker")
            .field("state_db_available", &self.state_db.is_some())
            .finish()
    }
}

fn decision_for_record(
    record: &GoalOwnerAdmissionRecord,
    now: chrono::DateTime<Utc>,
) -> ModelRequestAdmissionDecision {
    match record.phase {
        GoalOwnerAdmissionPhase::Dormant
        | GoalOwnerAdmissionPhase::Acquired
        | GoalOwnerAdmissionPhase::InFlight => ModelRequestAdmissionDecision::Dormant,
        GoalOwnerAdmissionPhase::Pending => {
            if record.deadline_at > now {
                ModelRequestAdmissionDecision::Deferred
            } else {
                // Only `admit` may turn an eligible pending record into a
                // lease. This branch is used after a failed CAS readback.
                ModelRequestAdmissionDecision::Dormant
            }
        }
        GoalOwnerAdmissionPhase::Terminal => match record.terminal_outcome {
            GoalOwnerAdmissionTerminalOutcome::Succeeded => {
                ModelRequestAdmissionDecision::Unrestricted
            }
            GoalOwnerAdmissionTerminalOutcome::Exhausted => {
                ModelRequestAdmissionDecision::Exhausted
            }
            GoalOwnerAdmissionTerminalOutcome::Cancelled => {
                ModelRequestAdmissionDecision::Cancelled
            }
            GoalOwnerAdmissionTerminalOutcome::Rejected
            | GoalOwnerAdmissionTerminalOutcome::Uncertain
            | GoalOwnerAdmissionTerminalOutcome::None => ModelRequestAdmissionDecision::Dormant,
        },
    }
}

fn record_matches_identity(
    record: &GoalOwnerAdmissionRecord,
    identity: &ModelRequestIdentity,
) -> bool {
    record.configured_provider_key.as_deref() == Some(identity.configured_provider_key.as_str())
        && record.requested_model == identity.configured_requested_model
        && record.effective_provider_id.as_deref() == Some(identity.effective_provider_id.as_str())
        && record.effective_model.as_deref() == Some(identity.effective_model.as_str())
}

fn continuation_matches_record(
    continuation_authority: &GoalOwnerAdmissionContinuationAuthority,
    record: &GoalOwnerAdmissionRecord,
) -> bool {
    continuation_authority == &record.continuation_authority()
}

fn continuation_matches_identity(
    continuation_authority: &GoalOwnerAdmissionContinuationAuthority,
    identity: &ModelRequestIdentity,
) -> bool {
    continuation_authority.authority.thread_id == identity.thread_id
        && identity.turn_id.as_deref() == Some(continuation_authority.successor_turn_id.as_str())
        && identity
            .kind
            .inference_kind()
            .is_some_and(|kind| continuation_authority.intended_request_kind == kind.as_str())
        && continuation_authority.logical_successor_request_id == identity.logical_request_id
}

fn storage_error(error: anyhow::Error) -> CodexErr {
    CodexErr::Fatal(format!("goal-owner admission ledger error: {error}"))
}

pub(crate) struct AdmittedModelRequest {
    store: GoalOwnerAdmissionStore,
    lease: GoalOwnerAdmissionLease,
    lifecycle: Mutex<LeaseLifecycle>,
}

impl AdmittedModelRequest {
    fn new(store: GoalOwnerAdmissionStore, lease: GoalOwnerAdmissionLease) -> Self {
        Self {
            store,
            lease,
            lifecycle: Mutex::new(LeaseLifecycle::default()),
        }
    }

    async fn acknowledge(&self) -> Result<()> {
        let mut lifecycle = self.lifecycle.lock().await;
        if lifecycle.terminalized {
            return Err(CodexErr::Fatal(
                "goal-owner admission received an acknowledgement after terminalization"
                    .to_string(),
            ));
        }
        lifecycle.acknowledged = true;
        lifecycle.stage = LeaseStage::Acknowledged;
        Ok(())
    }

    async fn release_if_unopened(&self) -> Result<()> {
        {
            let lifecycle = self.lifecycle.lock().await;
            if lifecycle.request_opened || lifecycle.terminalized {
                return Ok(());
            }
        }
        let released = self
            .store
            .release_acquired_lease(&self.lease)
            .await
            .map_err(storage_error)?;
        if !released {
            return Err(CodexErr::Fatal(
                "goal-owner admission reservation is no longer current".to_string(),
            ));
        }
        let mut lifecycle = self.lifecycle.lock().await;
        lifecycle.terminalized = true;
        lifecycle.stage = LeaseStage::CancelledBeforeAcknowledgement;
        Ok(())
    }

    async fn finish_if_unfinished(
        &self,
        stage: LeaseStage,
        outcome: GoalOwnerAdmissionTerminalOutcome,
        disposition: GoalOwnerAdmissionTerminalDisposition,
    ) -> Result<()> {
        {
            let lifecycle = self.lifecycle.lock().await;
            if lifecycle.terminalized {
                return Ok(());
            }
        }
        let persisted = self
            .store
            .finish(&self.lease, outcome, disposition)
            .await
            .map_err(storage_error)?;
        if persisted.is_none() {
            return Err(CodexErr::Fatal(
                "goal-owner admission lease is no longer current".to_string(),
            ));
        }
        let mut lifecycle = self.lifecycle.lock().await;
        let previous_stage = lifecycle.stage;
        lifecycle.stage = stage;
        lifecycle.terminalized = true;
        debug!(
            lease_id = %self.lease.lease_id,
            ?previous_stage,
            terminal_stage = ?stage,
            ?outcome,
            "recorded goal-owner admission lease outcome"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LeaseStage {
    Acquired,
    RequestOpened,
    Acknowledged,
    ProviderDenied,
    Succeeded,
    TransportLostBeforeAcknowledgement,
    TransportLostAfterAcknowledgement,
    CancelledBeforeAcknowledgement,
    CancelledAfterAcknowledgement,
}

#[derive(Debug)]
struct LeaseLifecycle {
    opening: bool,
    request_opened: bool,
    acknowledged: bool,
    terminalized: bool,
    stage: LeaseStage,
}

impl Default for LeaseLifecycle {
    fn default() -> Self {
        Self {
            opening: false,
            request_opened: false,
            acknowledged: false,
            terminalized: false,
            stage: LeaseStage::Acquired,
        }
    }
}

/// Owns the one physical-request permission for an admitted continuation.
///
/// The guard terminalizes conservatively on cancellation or drop. That is
/// important for unary compaction futures as well as streams: dropping the
/// future after the request has been opened must never make the same lease
/// reusable automatically.
pub(crate) struct ModelRequestLeaseGuard {
    admitted: Option<Arc<AdmittedModelRequest>>,
}

impl ModelRequestLeaseGuard {
    fn unrestricted() -> Self {
        Self { admitted: None }
    }

    fn admitted(admitted: Arc<AdmittedModelRequest>) -> Self {
        Self {
            admitted: Some(admitted),
        }
    }

    pub(crate) fn is_admitted(&self) -> bool {
        self.admitted.is_some()
    }

    pub(crate) async fn provider_acknowledged(&mut self) -> Result<()> {
        if let Some(admitted) = &self.admitted {
            admitted.acknowledge().await?;
        }
        Ok(())
    }

    pub(crate) async fn provider_denied(&mut self) -> Result<()> {
        if let Some(admitted) = &self.admitted {
            admitted
                .finish_if_unfinished(
                    LeaseStage::ProviderDenied,
                    GoalOwnerAdmissionTerminalOutcome::Rejected,
                    GoalOwnerAdmissionTerminalDisposition::None,
                )
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn completed(&mut self) -> Result<()> {
        if let Some(admitted) = &self.admitted {
            admitted
                .finish_if_unfinished(
                    LeaseStage::Succeeded,
                    GoalOwnerAdmissionTerminalOutcome::Succeeded,
                    GoalOwnerAdmissionTerminalDisposition::None,
                )
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn transport_lost(&mut self) -> Result<()> {
        if let Some(admitted) = &self.admitted {
            let stage = if admitted.lifecycle.lock().await.acknowledged {
                LeaseStage::TransportLostAfterAcknowledgement
            } else {
                LeaseStage::TransportLostBeforeAcknowledgement
            };
            admitted
                .finish_if_unfinished(
                    stage,
                    GoalOwnerAdmissionTerminalOutcome::Uncertain,
                    GoalOwnerAdmissionTerminalDisposition::ManualReview,
                )
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn cancelled_or_dropped(&mut self) -> Result<()> {
        if let Some(admitted) = &self.admitted {
            let stage = if admitted.lifecycle.lock().await.acknowledged {
                LeaseStage::CancelledAfterAcknowledgement
            } else {
                LeaseStage::CancelledBeforeAcknowledgement
            };
            admitted
                .finish_if_unfinished(
                    stage,
                    GoalOwnerAdmissionTerminalOutcome::Uncertain,
                    GoalOwnerAdmissionTerminalDisposition::ManualReview,
                )
                .await?;
        }
        Ok(())
    }
}

impl Drop for ModelRequestLeaseGuard {
    fn drop(&mut self) {
        let Some(admitted) = self.admitted.as_ref().map(Arc::clone) else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!(lease_id = %admitted.lease.lease_id, "admitted model request dropped outside a Tokio runtime");
            return;
        };
        handle.spawn(async move {
            let stage = if admitted.lifecycle.lock().await.acknowledged {
                LeaseStage::CancelledAfterAcknowledgement
            } else {
                LeaseStage::CancelledBeforeAcknowledgement
            };
            if let Err(error) = admitted
                .finish_if_unfinished(
                    stage,
                    GoalOwnerAdmissionTerminalOutcome::Uncertain,
                    GoalOwnerAdmissionTerminalDisposition::ManualReview,
                )
                .await
            {
                warn!(error = %error, lease_id = %admitted.lease.lease_id, "failed to terminalize dropped admitted model request");
            }
        });
    }
}

#[cfg(test)]
#[path = "model_request_admission_tests.rs"]
mod tests;
