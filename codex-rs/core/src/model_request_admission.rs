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

use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::protocol::SessionSource;
use codex_state::GoalOwnerAdmissionAcquireResult;
use codex_state::GoalOwnerAdmissionContinuationAuthority;
use codex_state::GoalOwnerAdmissionLease;
use codex_state::GoalOwnerAdmissionPhase;
use codex_state::GoalOwnerAdmissionRecord;
use codex_state::GoalOwnerAdmissionTerminalDisposition;
use codex_state::GoalOwnerAdmissionTerminalOutcome;
use codex_state::GoalRuntimeAdmissionFenceGuard;
use codex_state::GoalRuntimeThreadOwner;
use codex_state::canonical_provider_id;
use tokio::sync::Mutex;
use tracing::debug;
use tracing::warn;
use uuid::Uuid;

use crate::StateDbHandle;

/// Core-owned authority for the one successor turn allowed to consume a
/// goal-owner admission generation. The goal extension may schedule this
/// value, but Core alone carries it into a turn and its model request.
#[derive(Clone, Debug)]
pub struct GoalOwnerContinuation {
    authority: GoalOwnerAdmissionContinuationAuthority,
    /// Exact thread capability carried from the trusted Goal runtime. Core
    /// cannot turn a generic StateRuntime or installed facade into one.
    owner: Arc<GoalRuntimeThreadOwner>,
    dispatch_claim_id: Option<Uuid>,
    fence_epoch: u64,
    installation_epoch: u64,
}

/// Owner-side factory for continuations. The fence is intentionally opaque:
/// callers can mint a token only from the coordinator that owns its epoch and
/// cannot attach an unrelated fence to an existing token.
#[derive(Clone, Debug)]
pub struct GoalRuntimeContinuationIssuer {
    owner: Arc<GoalRuntimeThreadOwner>,
    // An issuer is valid for exactly one enablement generation. Clones of the
    // live coordinator share this stamp so that coordinator can disable and
    // later re-enable itself, while a separately retained issuer cannot mint
    // again after the owner has crossed a stop/restart boundary.
    issuer_enablement_epoch: Arc<std::sync::atomic::AtomicU64>,
    // Only a currently live issuer may resume the generation it stopped.
    // Facades derived while an owner is stopped can observe it for cleanup,
    // but cannot use a fresh local epoch stamp to restart it.
    may_reenable: bool,
    // The issuer is also pinned to the fence epoch it was issued under. A
    // later revoke/drain must not let a retained issuer read a fresh epoch and
    // mint another continuation from the same shared owner capability.
    issuer_continuation_epoch: Arc<std::sync::atomic::AtomicU64>,
}

impl GoalRuntimeContinuationIssuer {
    /// Construct an issuer from a capability already issued by the Runtime
    /// Custodian. There is intentionally no `for_thread` factory: Core cannot
    /// mint ownership from an ambient installed facade or a thread identifier.
    pub fn from_thread_owner(owner: Arc<GoalRuntimeThreadOwner>) -> Self {
        Self {
            issuer_enablement_epoch: Arc::new(std::sync::atomic::AtomicU64::new(
                owner.enablement_epoch(),
            )),
            // A current active custodian may toggle its own enablement. An
            // issuer first created while disabled cannot later revive the
            // shared owner, and a root handoff still requires the durable
            // Runtime Custodian receipt CAS.
            may_reenable: owner.is_enabled(),
            issuer_continuation_epoch: Arc::new(std::sync::atomic::AtomicU64::new(
                owner.continuation_epoch(),
            )),
            owner,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.owner.is_enabled_at_generation(
            self.issuer_enablement_epoch
                .load(std::sync::atomic::Ordering::Acquire),
        ) && self.owner.continuation_epoch()
            == self
                .issuer_continuation_epoch
                .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn enablement_epoch(&self) -> u64 {
        self.owner.enablement_epoch()
    }

    pub fn set_enabled(&self, enabled: bool) -> Option<u64> {
        // Only the issuer that owned the immediately preceding enablement
        // generation may transition it. A pre-stop retained issuer must not
        // revive itself by toggling the shared owner back on.
        if enabled && !self.may_reenable {
            return None;
        }
        let expected_enablement_epoch = self
            .issuer_enablement_epoch
            .load(std::sync::atomic::Ordering::Acquire);
        let enablement_epoch = self
            .owner
            .set_enabled_if_generation(expected_enablement_epoch, enabled);
        if let Some(enablement_epoch) = enablement_epoch {
            self.issuer_enablement_epoch
                .store(enablement_epoch, std::sync::atomic::Ordering::Release);
            self.issuer_continuation_epoch.store(
                self.owner.continuation_epoch(),
                std::sync::atomic::Ordering::Release,
            );
        }
        enablement_epoch
    }

    /// Atomically claim a pending generation using this coordinator's private
    /// fence. Callers never receive the persisted identity or a raw issuer.
    pub async fn claim_dispatch(
        &self,
        authority: &GoalOwnerAdmissionContinuationAuthority,
        now: DateTime<Utc>,
    ) -> anyhow::Result<Option<Uuid>> {
        if !self.is_enabled() {
            anyhow::bail!("goal continuation issuer is disabled or stale")
        }
        if authority.authority.thread_id != self.owner.thread_id() {
            anyhow::bail!("goal continuation authority belongs to a different installed owner")
        }
        self.owner.claim_dispatch(authority, now).await
    }

    /// Release only a claim owned by this coordinator's private fence.
    pub async fn release_dispatch_claim(
        &self,
        authority: &codex_state::GoalOwnerAdmissionAuthority,
        dispatch_claim_id: Uuid,
    ) -> anyhow::Result<bool> {
        self.owner
            .release_dispatch_claim(authority, dispatch_claim_id)
            .await
    }

    pub fn current_epoch(&self) -> u64 {
        self.issuer_continuation_epoch
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn revoke(&self) {
        self.owner.revoke_continuations();
    }

    pub fn continuation(
        &self,
        authority: GoalOwnerAdmissionContinuationAuthority,
    ) -> GoalOwnerContinuation {
        GoalOwnerContinuation::from_coordinator(authority, None, self)
    }

    pub fn continuation_with_dispatch_claim(
        &self,
        authority: GoalOwnerAdmissionContinuationAuthority,
        dispatch_claim_id: Uuid,
    ) -> GoalOwnerContinuation {
        GoalOwnerContinuation::from_coordinator(authority, Some(dispatch_claim_id), self)
    }
}

impl GoalOwnerContinuation {
    fn enter_fence(&self) -> Option<GoalRuntimeAdmissionFenceGuard> {
        self.owner.enter_continuation(self.fence_epoch)
    }

    pub(crate) fn has_fence(&self) -> bool {
        self.owner.thread_id() == self.authority.authority.thread_id
            && self.owner.is_enabled()
            && self.owner.enablement_epoch() == self.installation_epoch
    }

    fn from_coordinator(
        authority: GoalOwnerAdmissionContinuationAuthority,
        dispatch_claim_id: Option<Uuid>,
        coordinator: &GoalRuntimeContinuationIssuer,
    ) -> Self {
        Self {
            authority,
            owner: Arc::clone(&coordinator.owner),
            dispatch_claim_id,
            fence_epoch: coordinator.current_epoch(),
            installation_epoch: coordinator
                .issuer_enablement_epoch
                .load(std::sync::atomic::Ordering::Acquire),
        }
    }

    pub(crate) fn authority(&self) -> &GoalOwnerAdmissionContinuationAuthority {
        &self.authority
    }

    pub(crate) fn dispatch_claim_id(&self) -> Option<Uuid> {
        self.dispatch_claim_id
    }

    pub(crate) fn thread_owner(&self) -> &Arc<GoalRuntimeThreadOwner> {
        &self.owner
    }
}

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
    const fn is_inference(self) -> bool {
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
    kind: ModelRequestKind,
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
            /*parent_continuity_decision_id*/ None,
            /*logical_request_id*/ None,
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
                let Some(fence_guard) = admitted
                    .owner
                    .as_ref()
                    .and_then(|owner| owner.enter_continuation(admitted.fence_epoch))
                else {
                    // Admission acquired a durable pre-network reservation,
                    // but this continuation lost authority before it could
                    // open. Return that exact reservation rather than leave
                    // an acquired row for recovery or manufacture an
                    // uncertainty without provider I/O.
                    admitted.cancel_before_transport().await?;
                    return Err(CodexErr::Fatal(
                        "goal-owner continuation was revoked before request open".to_string(),
                    ));
                };
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
                // This returns only the exclusive local lease guard. The
                // durable `in_flight` transition happens at
                // `open_transport`, immediately where Core hands the request
                // to HTTP or WebSocket. A lifecycle stop in setup therefore
                // releases the acquired reservation rather than creating a
                // false provider-effect uncertainty.
                Ok(ModelRequestLeaseGuard::admitted(
                    Arc::clone(admitted),
                    fence_guard,
                ))
            }
            decision => Err(decision.blocked_error()),
        }
    }

    /// Release an acquired pre-network lease when local setup fails. If the
    /// transport handoff has already won, preserve conservative terminal
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

    /// Release a scheduler dispatch claim when a published continuation cannot
    /// reach its first model request. The exact pending-generation CAS makes
    /// this harmless after the claim has already been consumed by admission.
    pub(crate) async fn release_dispatch_claim(
        &self,
        continuation: &GoalOwnerContinuation,
    ) -> Result<()> {
        let Some(dispatch_claim_id) = continuation.dispatch_claim_id() else {
            return Ok(());
        };
        continuation
            .thread_owner()
            .release_dispatch_claim(&continuation.authority().authority, dispatch_claim_id)
            .await
            .map(|_| ())
            .map_err(storage_error)
    }

    pub(crate) async fn admit(
        &self,
        identity: &ModelRequestIdentity,
        continuation: Option<&GoalOwnerContinuation>,
    ) -> Result<ModelRequestAdmissionDecision> {
        let _fence_guard = if let Some(continuation) = continuation {
            if !continuation.has_fence() {
                return Ok(ModelRequestAdmissionDecision::Dormant);
            }
            let Some(guard) = continuation.enter_fence() else {
                return Ok(ModelRequestAdmissionDecision::Cancelled);
            };
            Some(guard)
        } else {
            None
        };
        let continuation_authority = continuation.map(GoalOwnerContinuation::authority);
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
            return Ok(if continuation_authority.is_some() {
                ModelRequestAdmissionDecision::Dormant
            } else {
                ModelRequestAdmissionDecision::Unrestricted
            });
        }
        let Some(state_db) = &self.state_db else {
            return Ok(if continuation_authority.is_some() {
                ModelRequestAdmissionDecision::Dormant
            } else {
                ModelRequestAdmissionDecision::Unrestricted
            });
        };
        // A continuation carries an exact Runtime Custodian thread capability.
        // Generic StateRuntime access remains diagnostics-only and serves
        // ordinary-turn observation only.
        let thread_owner = continuation.map(GoalOwnerContinuation::thread_owner);
        if let Some(continuation) = continuation
            && !state_db.validates_goal_runtime_thread_owner(continuation.thread_owner())
        {
            return Ok(ModelRequestAdmissionDecision::Dormant);
        }
        let now = Utc::now();
        let record = match thread_owner {
            Some(owner) => owner.get().await,
            None => {
                state_db
                    .goal_owner_admissions()
                    .get(identity.thread_id)
                    .await
            }
        }
        .map_err(storage_error)?;
        let Some(record) = record else {
            // A typed continuation is permission only when its exact durable generation is
            // present. A missing active row is a fail-closed lifecycle failure, never a reason
            // to fall through to ordinary unrestricted admission.
            return Ok(if continuation_authority.is_some() {
                ModelRequestAdmissionDecision::Dormant
            } else {
                ModelRequestAdmissionDecision::Unrestricted
            });
        };

        let Some(continuation_authority) = continuation_authority else {
            // A prior successful continuation no longer restricts the thread,
            // regardless of later model/provider resolution.
            if record.phase == GoalOwnerAdmissionPhase::Terminal {
                // Cancellation is scoped to the exact continuation authority. A subsequent
                // ordinary user request must not inherit a settled cancellation row.
                if record.terminal_outcome == GoalOwnerAdmissionTerminalOutcome::Cancelled {
                    return Ok(ModelRequestAdmissionDecision::Unrestricted);
                }
                return Ok(decision_for_record(&record, now));
            }
            if !record_matches_identity(&record, identity) {
                // A continuation lease is scoped to the provider/model evidence
                // that produced the original denial. Missing or different
                // evidence is not permission to send a different request.
                return Ok(ModelRequestAdmissionDecision::Dormant);
            }
            return Ok(ModelRequestAdmissionDecision::Dormant);
        };

        // A continuation token is valid only for the exact pending generation and request
        // identity. Terminal, stale, or mismatched rows are lifecycle failures and must not
        // become unrestricted merely because the row exists.
        if !record_matches_identity(&record, identity)
            || !continuation_matches_record(continuation_authority, &record)
            || !continuation_matches_identity(continuation_authority, identity)
        {
            return Ok(ModelRequestAdmissionDecision::Dormant);
        }
        let Some(dispatch_claim_id) =
            continuation.and_then(GoalOwnerContinuation::dispatch_claim_id)
        else {
            return Ok(ModelRequestAdmissionDecision::Dormant);
        };
        if record.dispatch_claim_id != Some(dispatch_claim_id) {
            return Ok(ModelRequestAdmissionDecision::Dormant);
        }
        // The durable dispatch claim is bound to the exact fence that minted
        // this token. Copying the authority and claim UUID into a token from a
        // foreign coordinator must never authorize provider I/O.
        if !continuation
            .is_some_and(|continuation| continuation.thread_owner().dispatch_claim_matches(&record))
        {
            return Ok(ModelRequestAdmissionDecision::Dormant);
        }
        if record.phase != GoalOwnerAdmissionPhase::Pending {
            return Ok(decision_for_continuation_record(&record));
        }

        if record.deadline_at > now {
            return Ok(ModelRequestAdmissionDecision::Deferred);
        }

        let Some(thread_owner) = thread_owner else {
            return Ok(ModelRequestAdmissionDecision::Dormant);
        };
        let acquire_result = thread_owner
            .try_acquire_claimed(continuation_authority, dispatch_claim_id, now)
            .await
            .map_err(storage_error)?;
        match acquire_result {
            GoalOwnerAdmissionAcquireResult::Acquired(lease) => {
                Ok(ModelRequestAdmissionDecision::Admitted(Arc::new(
                    AdmittedModelRequest::new(Arc::clone(thread_owner), *lease, continuation),
                )))
            }
            GoalOwnerAdmissionAcquireResult::Exhausted(_) => {
                Ok(ModelRequestAdmissionDecision::Exhausted)
            }
            GoalOwnerAdmissionAcquireResult::Dormant => Ok(ModelRequestAdmissionDecision::Dormant),
            GoalOwnerAdmissionAcquireResult::NotCurrent
            | GoalOwnerAdmissionAcquireResult::NotEligible => {
                // A failed acquisition can race with another owner or a
                // lifecycle transition. A fresh read may expose a final,
                // durable decision; a missing or still-uncertain record stays
                // fail-closed and cannot authorize provider I/O.
                let current = thread_owner.get().await.map_err(storage_error)?;
                Ok(
                    current.map_or(ModelRequestAdmissionDecision::Dormant, |record| {
                        if continuation_matches_record(continuation_authority, &record)
                            && continuation_matches_identity(continuation_authority, identity)
                            && record_matches_identity(&record, identity)
                            && record.dispatch_claim_id
                                == continuation.and_then(GoalOwnerContinuation::dispatch_claim_id)
                        {
                            decision_for_continuation_record(&record)
                        } else {
                            ModelRequestAdmissionDecision::Dormant
                        }
                    }),
                )
            }
        }
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

fn decision_for_continuation_record(
    record: &GoalOwnerAdmissionRecord,
) -> ModelRequestAdmissionDecision {
    match record.phase {
        GoalOwnerAdmissionPhase::Terminal => match record.terminal_outcome {
            GoalOwnerAdmissionTerminalOutcome::Exhausted => {
                ModelRequestAdmissionDecision::Exhausted
            }
            GoalOwnerAdmissionTerminalOutcome::Cancelled => {
                ModelRequestAdmissionDecision::Cancelled
            }
            GoalOwnerAdmissionTerminalOutcome::None
            | GoalOwnerAdmissionTerminalOutcome::Succeeded
            | GoalOwnerAdmissionTerminalOutcome::Rejected
            | GoalOwnerAdmissionTerminalOutcome::Uncertain => {
                ModelRequestAdmissionDecision::Dormant
            }
        },
        GoalOwnerAdmissionPhase::Dormant
        | GoalOwnerAdmissionPhase::Pending
        | GoalOwnerAdmissionPhase::Acquired
        | GoalOwnerAdmissionPhase::InFlight => ModelRequestAdmissionDecision::Dormant,
    }
}

fn record_matches_identity(
    record: &GoalOwnerAdmissionRecord,
    identity: &ModelRequestIdentity,
) -> bool {
    record.configured_provider_key.as_deref() == Some(identity.configured_provider_key.as_str())
        && record.requested_model == identity.configured_requested_model
        && record
            .effective_provider_id
            .as_deref()
            .map(canonical_provider_id)
            == Some(canonical_provider_id(
                identity.effective_provider_id.as_str(),
            ))
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
    thread_owner: Arc<GoalRuntimeThreadOwner>,
    lease: GoalOwnerAdmissionLease,
    lifecycle: Mutex<LeaseLifecycle>,
    owner: Option<Arc<GoalRuntimeThreadOwner>>,
    fence_epoch: u64,
}

impl AdmittedModelRequest {
    fn new(
        thread_owner: Arc<GoalRuntimeThreadOwner>,
        lease: GoalOwnerAdmissionLease,
        continuation: Option<&GoalOwnerContinuation>,
    ) -> Self {
        Self {
            thread_owner,
            lease,
            lifecycle: Mutex::new(LeaseLifecycle::default()),
            owner: continuation.map(|continuation| Arc::clone(&continuation.owner)),
            fence_epoch: continuation.map_or(0, |continuation| continuation.fence_epoch),
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
            .thread_owner
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

    /// Close an admitted lease after continuation authority is revoked but
    /// before any provider I/O. Prefer the exact acquired-to-pending release
    /// CAS; if request-open won concurrently, close that in-flight lease as a
    /// definite cancellation. Neither branch may create uncertainty because
    /// this method is called before transport receives the lease guard.
    async fn cancel_before_transport(&self) -> Result<()> {
        let request_opened = {
            let lifecycle = self.lifecycle.lock().await;
            if lifecycle.terminalized {
                return Ok(());
            }
            lifecycle.request_opened
        };

        if request_opened {
            let cancelled = self
                .thread_owner
                .cancel_opened_lease_before_transport(&self.lease)
                .await
                .map_err(storage_error)?;
            if !cancelled {
                return Err(CodexErr::Fatal(
                    "goal-owner opened admission is no longer current before transport".to_string(),
                ));
            }
            let mut lifecycle = self.lifecycle.lock().await;
            lifecycle.terminalized = true;
            lifecycle.stage = LeaseStage::CancelledBeforeAcknowledgement;
            return Ok(());
        }

        if self
            .thread_owner
            .release_acquired_lease(&self.lease)
            .await
            .map_err(storage_error)?
        {
            let mut lifecycle = self.lifecycle.lock().await;
            lifecycle.terminalized = true;
            lifecycle.stage = LeaseStage::CancelledBeforeAcknowledgement;
            return Ok(());
        }

        Err(CodexErr::Fatal(
            "goal-owner acquired admission is no longer current before transport".to_string(),
        ))
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
            .thread_owner
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
    _fence_guard: Option<GoalRuntimeAdmissionFenceGuard>,
}

impl ModelRequestLeaseGuard {
    fn unrestricted() -> Self {
        Self {
            admitted: None,
            _fence_guard: None,
        }
    }

    fn admitted(
        admitted: Arc<AdmittedModelRequest>,
        fence_guard: GoalRuntimeAdmissionFenceGuard,
    ) -> Self {
        Self {
            admitted: Some(admitted),
            _fence_guard: Some(fence_guard),
        }
    }

    pub(crate) fn is_admitted(&self) -> bool {
        self.admitted.is_some()
    }

    /// Recheck the continuation fence immediately before handing the request
    /// to a transport and atomically open its durable request fence. Every
    /// HTTP and WebSocket path must call this at the handoff site, not while
    /// constructing auth or request state, so a revoked continuation cannot
    /// physically send through a detached transport task.
    pub(crate) async fn open_transport(&mut self) -> Result<()> {
        if self
            ._fence_guard
            .as_ref()
            .is_some_and(|fence| !fence.is_current_epoch())
        {
            if let Some(admitted) = self.admitted.take() {
                admitted.cancel_before_transport().await?;
            }
            self._fence_guard = None;
            return Err(CodexErr::Fatal(
                "goal-owner continuation was revoked before transport open".to_string(),
            ));
        }

        let Some(admitted) = self.admitted.as_ref() else {
            return Ok(());
        };
        let needs_open = {
            let lifecycle = admitted.lifecycle.lock().await;
            if lifecycle.terminalized {
                return Err(CodexErr::Fatal(
                    "goal-owner admission lease is already terminal".to_string(),
                ));
            }
            !lifecycle.request_opened
        };
        if !needs_open {
            return Ok(());
        }
        let opened = match admitted.thread_owner.open_lease(&admitted.lease).await {
            Ok(opened) => opened,
            Err(error) => {
                let mut lifecycle = admitted.lifecycle.lock().await;
                lifecycle.opening = false;
                drop(lifecycle);
                let _ = admitted
                    .thread_owner
                    .release_acquired_lease(&admitted.lease)
                    .await;
                return Err(storage_error(error));
            }
        };
        if !opened {
            let mut lifecycle = admitted.lifecycle.lock().await;
            lifecycle.opening = false;
            lifecycle.terminalized = true;
            lifecycle.stage = LeaseStage::CancelledBeforeAcknowledgement;
            return Err(CodexErr::Fatal(
                "goal-owner admission was cancelled before its request-open fence".to_string(),
            ));
        }
        {
            let mut lifecycle = admitted.lifecycle.lock().await;
            lifecycle.opening = false;
            lifecycle.request_opened = true;
            lifecycle.stage = LeaseStage::RequestOpened;
        }
        if self
            ._fence_guard
            .as_ref()
            .is_some_and(|fence| !fence.is_current_epoch())
        {
            let admitted = self
                .admitted
                .take()
                .expect("admitted guard remains owned until transport handoff");
            admitted.cancel_before_transport().await?;
            self._fence_guard = None;
            return Err(CodexErr::Fatal(
                "goal-owner continuation was revoked before transport open".to_string(),
            ));
        }
        Ok(())
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
            if !admitted.lifecycle.lock().await.request_opened {
                return admitted.release_if_unopened().await;
            }
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
            if !admitted.lifecycle.lock().await.request_opened {
                if let Err(error) = admitted.release_if_unopened().await {
                    warn!(error = %error, lease_id = %admitted.lease.lease_id, "failed to release dropped unopened model request");
                }
                return;
            }
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
