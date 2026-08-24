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

/// The logical purpose of a Responses request.
///
/// `Prewarm` is the only non-inference kind. It corresponds to the v2
/// websocket `generate=false` request and is intentionally exempt from the
/// goal-owner admission ledger.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelRequestKind {
    Turn,
    LocalCompaction,
    RemoteCompactionV2,
    RemoteCompact,
    Prewarm,
}

impl ModelRequestKind {
    pub(crate) const fn is_inference(self) -> bool {
        !matches!(self, Self::Prewarm)
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
    pub(crate) provider_id: String,
    pub(crate) requested_model: String,
    pub(crate) effective_model: String,
    pub(crate) service_tier: Option<String>,
    pub(crate) session_source: SessionSource,
    pub(crate) parent_continuity_decision_id: Option<Uuid>,
}

impl ModelRequestIdentity {
    pub(crate) fn new(
        thread_id: ThreadId,
        turn_id: Option<String>,
        kind: ModelRequestKind,
        provider_id: String,
        requested_model: String,
        effective_model: String,
        service_tier: Option<String>,
        session_source: SessionSource,
        parent_continuity_decision_id: Option<Uuid>,
    ) -> Self {
        Self {
            thread_id,
            turn_id,
            logical_request_id: Uuid::now_v7().to_string(),
            kind,
            provider_id,
            requested_model,
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
                let mut lifecycle = admitted.lifecycle.lock().await;
                if lifecycle.request_opened {
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
                lifecycle.request_opened = true;
                lifecycle.stage = LeaseStage::RequestOpened;
                Ok(ModelRequestLeaseGuard::admitted(Arc::clone(admitted)))
            }
            decision => Err(decision.blocked_error()),
        }
    }

    /// Terminalize an acquired lease when setup fails before a stream/response
    /// guard has taken ownership. The ledger has no "setup failed" outcome;
    /// conservative uncertainty prevents an automatic replay.
    pub(crate) async fn terminalize_if_unfinished(&self) {
        let Self::Admitted(admitted) = self else {
            return;
        };
        if let Err(error) = admitted
            .finish_if_unfinished(
                LeaseStage::CancelledBeforeAcknowledgement,
                GoalOwnerAdmissionTerminalOutcome::Uncertain,
                GoalOwnerAdmissionTerminalDisposition::ManualReview,
            )
            .await
        {
            warn!(error = %error, lease_id = %admitted.lease.lease_id, "failed to conservatively terminalize a goal-owner admission lease");
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
    ) -> Result<ModelRequestAdmissionDecision> {
        debug!(
            thread_id = %identity.thread_id,
            turn_id = ?identity.turn_id,
            logical_request_id = %identity.logical_request_id,
            request_kind = ?identity.kind,
            provider_id = %identity.provider_id,
            requested_model = %identity.requested_model,
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
        if !record_matches_identity(&record, identity) {
            // A continuation lease is scoped to the provider/model evidence
            // that produced the original denial. Missing or different
            // evidence is not permission to send a different request.
            return Ok(ModelRequestAdmissionDecision::Dormant);
        }

        if record.phase != GoalOwnerAdmissionPhase::Pending {
            return Ok(decision_for_record(&record, now));
        }
        if record.deadline_at > now {
            return Ok(ModelRequestAdmissionDecision::Deferred);
        }

        let lease = store
            .try_acquire(&record.authority, now)
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
        GoalOwnerAdmissionPhase::Dormant | GoalOwnerAdmissionPhase::InFlight => {
            ModelRequestAdmissionDecision::Dormant
        }
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
    record.provider_id.as_deref() == Some(identity.provider_id.as_str())
        && record.requested_model.as_deref() == Some(identity.requested_model.as_str())
        && record.effective_model.as_deref() == Some(identity.effective_model.as_str())
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
    request_opened: bool,
    acknowledged: bool,
    terminalized: bool,
    stage: LeaseStage,
}

impl Default for LeaseLifecycle {
    fn default() -> Self {
        Self {
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
