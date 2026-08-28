use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use chrono::Utc;
use codex_core::GoalContinuationFence;
use codex_core::GoalOwnerContinuation;
use codex_core::ThreadManager;
use codex_extension_api::ProviderEvidenceAuthority;
use codex_extension_api::RateLimitDomain;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadGoal;

use crate::accounting::BudgetLimitedGoalDisposition;
use crate::accounting::GoalAccountingState;
use crate::analytics::GoalAnalytics;
use crate::analytics::GoalEventAttribution;
use crate::events::GoalEventEmitter;
use crate::metrics::GoalMetrics;
use crate::steering::continuation_steering_item;
use crate::steering::objective_updated_steering_item;
use crate::tool::protocol_goal_from_state;
use tokio::sync::Semaphore;
use tokio::sync::SemaphorePermit;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const MAX_PROVIDER_RATE_LIMIT_CONTINUATIONS_PER_GOAL: u8 = 1;
const MAX_PROVIDER_RATE_LIMIT_CONTINUATION_DELAY: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone)]
pub struct GoalRuntimeHandle {
    inner: Arc<GoalRuntimeInner>,
}

pub(crate) struct GoalRuntimeConfig {
    pub(crate) analytics: GoalAnalytics,
    pub(crate) enabled: bool,
    pub(crate) tools_available_for_thread: bool,
}

pub(crate) enum ActiveGoalStopReason {
    TurnError,
    UsageLimit,
}

struct GoalRuntimeInner {
    thread_id: ThreadId,
    state_dbs: Arc<codex_state::StateRuntime>,
    analytics: GoalAnalytics,
    event_emitter: GoalEventEmitter,
    metrics: GoalMetrics,
    thread_manager: Weak<ThreadManager>,
    accounting_state: Arc<GoalAccountingState>,
    tools_available_for_thread: bool,
    continuation: GoalContinuationCoordinator,
}

/// Per-thread lifecycle owner for the durable goal continuation. Runtime
/// callers retain only a wake handle and consult this coordinator for every
/// authority, epoch, claim, and lifecycle transition.
struct GoalContinuationCoordinator {
    enabled: AtomicBool,
    enablement_epoch: AtomicU64,
    fence: Arc<GoalContinuationFence>,
    state: Mutex<ProviderContinuationState>,
    lifecycle: Semaphore,
}

/// Owner-aware cleanup for the narrow window between durable claim and turn
/// publication. If the wake task is aborted at an await, Drop schedules the
/// same exact CAS release; after publication ownership transfers to the turn
/// token and the guard is disarmed.
struct DispatchClaimGuard {
    store: codex_state::GoalOwnerAdmissionStore,
    authority: codex_state::GoalOwnerAdmissionAuthority,
    claim_id: Uuid,
    armed: bool,
}

impl DispatchClaimGuard {
    fn new(
        store: codex_state::GoalOwnerAdmissionStore,
        authority: codex_state::GoalOwnerAdmissionAuthority,
        claim_id: Uuid,
    ) -> Self {
        Self {
            store,
            authority,
            claim_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DispatchClaimGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let store = self.store.clone();
        let authority = self.authority.clone();
        let claim_id = self.claim_id;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(error) = store.release_dispatch_claim(&authority, claim_id).await {
                    tracing::warn!(
                        thread_id = %authority.thread_id,
                        generation = authority.generation,
                        dispatch_claim_id = %claim_id,
                        error = %error,
                        "aborted goal continuation wake could not release its exact dispatch claim"
                    );
                }
            });
        }
    }
}

struct DeferredProviderContinuation {
    turn_id: String,
    goal_id: String,
    continuation: GoalOwnerContinuation,
    eligible_at: Option<Instant>,
    cancellation: CancellationToken,
    enablement_epoch: u64,
}

#[derive(Default)]
struct ProviderContinuationState {
    goal_id: Option<String>,
    attempts: u8,
    /// In-memory fail-closed gate for this runtime. The durable thread-goal gate below
    /// covers resume; this bit also covers a persistence failure or an exhausted attempt.
    blocked: bool,
    pending: Option<DeferredProviderContinuation>,
    last_authority: Option<codex_state::GoalOwnerAdmissionAuthority>,
}

pub(crate) struct AccountedGoalProgress {
    pub(crate) goal: ThreadGoal,
    pub(crate) goal_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviousGoalSnapshot {
    pub goal_id: String,
    pub status: codex_state::ThreadGoalStatus,
    pub objective: String,
}

impl From<&codex_state::ThreadGoal> for PreviousGoalSnapshot {
    fn from(goal: &codex_state::ThreadGoal) -> Self {
        Self {
            goal_id: goal.goal_id.clone(),
            status: goal.status,
            objective: goal.objective.clone(),
        }
    }
}

impl std::fmt::Debug for GoalRuntimeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoalRuntimeHandle").finish_non_exhaustive()
    }
}

impl GoalRuntimeHandle {
    pub(crate) fn new(
        thread_id: ThreadId,
        state_dbs: Arc<codex_state::StateRuntime>,
        event_emitter: GoalEventEmitter,
        metrics: GoalMetrics,
        thread_manager: Weak<ThreadManager>,
        accounting_state: Arc<GoalAccountingState>,
        config: GoalRuntimeConfig,
    ) -> Self {
        Self {
            inner: Arc::new(GoalRuntimeInner {
                thread_id,
                state_dbs,
                analytics: config.analytics,
                event_emitter,
                metrics,
                thread_manager,
                accounting_state,
                tools_available_for_thread: config.tools_available_for_thread,
                continuation: GoalContinuationCoordinator {
                    enabled: AtomicBool::new(config.enabled),
                    enablement_epoch: AtomicU64::new(0),
                    fence: Arc::new(GoalContinuationFence::new()),
                    state: Mutex::new(ProviderContinuationState::default()),
                    lifecycle: Semaphore::new(/*permits*/ 1),
                },
            }),
        }
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        let was_enabled = self
            .inner
            .continuation
            .enabled
            .swap(enabled, Ordering::Relaxed);
        if was_enabled == enabled {
            return;
        }
        let enablement_epoch = self
            .inner
            .continuation
            .enablement_epoch
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        let fence = Arc::clone(&self.inner.continuation.fence);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| fence.revoke_and_wait());
            }
            Ok(_) => fence.revoke(),
            Err(_) => fence.revoke_and_wait(),
        }
        if enabled {
            let authority = self
                .inner
                .continuation
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .last_authority
                .clone();
            let inner = Arc::downgrade(&self.inner);
            let reconcile = async move {
                let Some(inner) = inner.upgrade() else {
                    return;
                };
                let runtime = GoalRuntimeHandle {
                    inner: Arc::clone(&inner),
                };
                if runtime
                    .inner
                    .continuation
                    .enablement_epoch
                    .load(Ordering::Relaxed)
                    != enablement_epoch
                    || !runtime.inner.continuation.enabled.load(Ordering::Relaxed)
                {
                    return;
                }
                if let Some(authority) = authority {
                    let Ok(_goal_state_permit) = runtime.goal_state_permit().await else {
                        tracing::warn!("failed to acquire goal-state permit while re-enabling");
                        return;
                    };
                    if runtime
                        .inner
                        .continuation
                        .enablement_epoch
                        .load(Ordering::Relaxed)
                        != enablement_epoch
                        || !runtime.inner.continuation.enabled.load(Ordering::Relaxed)
                    {
                        return;
                    }
                    if let Err(error) = runtime.cancel_and_retire_admission(&authority).await {
                        tracing::warn!(error = %error, "failed to reconcile goal admission while re-enabling");
                        return;
                    }
                    if let Err(error) = runtime.clear_deferral_if_retired(&authority).await {
                        tracing::warn!(error = %error, "failed to clear retired goal continuation deferral while re-enabling");
                        return;
                    }
                }
                if runtime
                    .inner
                    .continuation
                    .enablement_epoch
                    .load(Ordering::Relaxed)
                    == enablement_epoch
                    && runtime.inner.continuation.enabled.load(Ordering::Relaxed)
                {
                    let mut state = runtime
                        .inner
                        .continuation
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.attempts = 0;
                    state.blocked = false;
                }
            };
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(reconcile);
            }
            return;
        }
        {
            let mut state = self
                .inner
                .continuation
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(pending) = state.pending.take() {
                pending.cancellation.cancel();
            }
            let authority = state.last_authority.clone();
            state.attempts = MAX_PROVIDER_RATE_LIMIT_CONTINUATIONS_PER_GOAL;
            state.blocked = true;
            if let Some(authority) = authority {
                let inner = Arc::downgrade(&self.inner);
                let cancel = async move {
                    let Some(inner) = inner.upgrade() else {
                        return;
                    };
                    if inner.continuation.enablement_epoch.load(Ordering::Relaxed)
                        != enablement_epoch
                        || inner.continuation.enabled.load(Ordering::Relaxed)
                    {
                        return;
                    }
                    let runtime = GoalRuntimeHandle { inner };
                    let Ok(_goal_state_permit) = runtime.goal_state_permit().await else {
                        return;
                    };
                    if inner.continuation.enablement_epoch.load(Ordering::Relaxed)
                        != enablement_epoch
                        || inner.continuation.enabled.load(Ordering::Relaxed)
                    {
                        return;
                    }
                    if let Err(error) = runtime.cancel_and_retire_admission(&authority).await {
                        tracing::warn!(
                            error = %error,
                            "failed to durably cancel disabled goal continuation"
                        );
                    }
                };
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    handle.spawn(cancel);
                }
            }
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.inner.continuation.enabled.load(Ordering::Relaxed)
    }

    pub(crate) fn tools_visible(&self) -> bool {
        self.is_enabled() && self.inner.tools_available_for_thread
    }

    pub(crate) fn thread_id(&self) -> ThreadId {
        self.inner.thread_id
    }

    pub(crate) fn accounting_state(&self) -> Arc<GoalAccountingState> {
        Arc::clone(&self.inner.accounting_state)
    }

    pub(crate) async fn goal_state_permit(&self) -> Result<SemaphorePermit<'_>, String> {
        self.inner
            .continuation
            .lifecycle
            .acquire()
            .await
            .map_err(|err| err.to_string())
    }

    pub async fn prepare_external_goal_mutation(&self) -> Result<(), String> {
        if !self.is_enabled() {
            return Ok(());
        }

        if let Some(turn_id) = self.inner.accounting_state.current_turn_id() {
            self.account_active_goal_progress(
                turn_id.as_str(),
                &format!("{turn_id}:external-goal-mutation"),
                codex_state::GoalAccountingMode::ActiveOnly,
                BudgetLimitedGoalDisposition::ClearActive,
            )
            .await?;
            return Ok(());
        }

        self.account_idle_goal_progress(
            &format!("{}:external-goal-mutation", self.inner.thread_id),
            codex_state::GoalAccountingMode::ActiveOnly,
            BudgetLimitedGoalDisposition::ClearActive,
        )
        .await?;
        Ok(())
    }

    pub async fn apply_external_goal_set(
        &self,
        goal: codex_state::ThreadGoal,
        previous_goal: Option<PreviousGoalSnapshot>,
    ) -> Result<(), String> {
        if !self.is_enabled() {
            return Ok(());
        }

        let replaced_existing_goal = previous_goal
            .as_ref()
            .is_some_and(|previous_goal| previous_goal.goal_id != goal.goal_id);
        if previous_goal.is_none() || replaced_existing_goal {
            self.inner.metrics.record_created();
            self.inner
                .analytics
                .created(&goal, GoalEventAttribution::NoTurn);
        }
        let previous_status = previous_goal
            .as_ref()
            .and_then(|previous_goal| (!replaced_existing_goal).then_some(previous_goal.status));
        self.inner
            .metrics
            .record_resumed_if_status_changed(previous_status, goal.status);
        self.inner
            .metrics
            .record_terminal_if_status_changed(previous_status, &goal);
        self.inner
            .analytics
            .status_changed(&goal, previous_status, GoalEventAttribution::NoTurn);
        if replaced_existing_goal {
            let _goal_state_permit = self.goal_state_permit().await?;
            let previous_authority = self
                .inner
                .continuation
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .last_authority
                .clone();
            self.cancel_provider_continuation_locked().await;
            if let Some(previous_authority) = previous_authority {
                self.retire_safe_terminal_admission(
                    &previous_authority,
                    /*clear_deferral*/ false,
                    /*allow_exhausted*/ true,
                )
                .await?;
            }
            let pending = {
                let mut state = self
                    .inner
                    .continuation
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let pending = state.pending.take();
                state.goal_id = Some(goal.goal_id.clone());
                state.attempts = 0;
                state.blocked = false;
                pending
            };
            if let Some(pending) = pending {
                pending.cancellation.cancel();
            }
            self.inner
                .state_dbs
                .thread_goals()
                .clear_thread_goal_continuation_deferral(self.thread_id())
                .await
                .map_err(|err| err.to_string())?;
        }
        let objective_changed = previous_goal.as_ref().is_some_and(|previous_goal| {
            !replaced_existing_goal && previous_goal.objective != goal.objective
        });
        match goal.status {
            codex_state::ThreadGoalStatus::Active => {
                if self.inner.accounting_state.current_turn_id().is_some() {
                    let _ = self
                        .inner
                        .accounting_state
                        .mark_current_turn_goal_active(goal.goal_id.clone());
                } else {
                    self.inner
                        .accounting_state
                        .mark_idle_goal_active(goal.goal_id.clone());
                }
                if objective_changed {
                    let item = objective_updated_steering_item(&protocol_goal_from_state(goal));
                    self.inject_active_turn_steering(item).await;
                }
                self.continue_if_idle().await?;
            }
            codex_state::ThreadGoalStatus::BudgetLimited => {
                if self.inner.accounting_state.current_turn_id().is_none() {
                    self.inner.accounting_state.clear_active_goal();
                }
            }
            codex_state::ThreadGoalStatus::Paused
            | codex_state::ThreadGoalStatus::Blocked
            | codex_state::ThreadGoalStatus::UsageLimited
            | codex_state::ThreadGoalStatus::Complete => {
                self.cancel_provider_continuation().await;
                self.inner.accounting_state.clear_active_goal();
            }
        }
        Ok(())
    }

    pub async fn apply_external_goal_clear(
        &self,
        goal: codex_state::ThreadGoal,
    ) -> Result<(), String> {
        if !self.is_enabled() {
            return Ok(());
        }

        self.inner.analytics.cleared(&goal);
        self.cancel_provider_continuation().await;
        self.inner.accounting_state.clear_active_goal();
        Ok(())
    }

    pub async fn usage_limit_active_goal_for_turn(&self, turn_id: &str) -> Result<(), String> {
        self.stop_active_goal_for_turn(turn_id, ActiveGoalStopReason::UsageLimit)
            .await
    }

    /// Preserves an active goal after an authoritative provider usage-limit
    /// response. A retry is only admitted at the provider-established delay.
    #[doc(hidden)]
    pub async fn preserve_active_goal_after_provider_limit(
        &self,
        turn_id: &str,
        rate_limit_domain: &RateLimitDomain,
    ) -> Result<bool, String> {
        if !self.is_enabled() {
            return Ok(false);
        }

        let _goal_state_permit = self.goal_state_permit().await?;
        let accounting = self.accounting_state();
        if !accounting.turn_is_current_active_goal(turn_id) {
            accounting.finish_turn(turn_id);
            return Ok(false);
        }

        self.account_active_goal_progress(
            turn_id,
            &format!("{turn_id}:provider-limit-progress"),
            codex_state::GoalAccountingMode::ActiveOnly,
            BudgetLimitedGoalDisposition::ClearActive,
        )
        .await?;

        let goal = self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
            .map_err(|err| err.to_string())?;

        accounting.finish_turn(turn_id);
        match goal {
            Some(goal) if goal.status == codex_state::ThreadGoalStatus::Active => {
                accounting.mark_idle_goal_active(goal.goal_id.clone());
                let retry_after = rate_limit_domain.provider_limit_evidence.retry_after;
                let deadline_at = retry_after
                    .and_then(|delay| chrono::Duration::from_std(delay).ok())
                    .map(|delay| Utc::now() + delay);
                let recognized = matches!(
                    rate_limit_domain.provider_limit_evidence.authority,
                    ProviderEvidenceAuthority::RecognizedHttpUsageLimit
                );
                let phase = if recognized && deadline_at.is_some() {
                    codex_state::GoalOwnerAdmissionPhase::Pending
                } else {
                    codex_state::GoalOwnerAdmissionPhase::Dormant
                };
                let record = self
                    .inner
                    .state_dbs
                    .goal_owner_admissions()
                    .observe_denial(&codex_state::GoalOwnerAdmissionObservation {
                        thread_id: self.thread_id(),
                        goal_id: goal.goal_id.clone(),
                        origin_turn_id: turn_id.to_string(),
                        origin_request_id: format!("{turn_id}:model-request"),
                        denial_class: codex_state::GoalOwnerAdmissionDenialClass::RateLimited,
                        configured_provider_key: rate_limit_domain
                            .local_request_identity
                            .configured_provider_key
                            .clone(),
                        requested_model: rate_limit_domain
                            .local_request_identity
                            .requested_model
                            .clone(),
                        effective_provider_id: rate_limit_domain
                            .local_request_identity
                            .effective_provider_id
                            .as_deref()
                            .or(rate_limit_domain
                                .local_request_identity
                                .configured_provider_key
                                .as_deref())
                            .map(codex_state::canonical_provider_id),
                        effective_model: rate_limit_domain
                            .local_request_identity
                            .resolved_model
                            .clone(),
                        intended_request_kind: "turn".to_string(),
                        successor_turn_id: Uuid::new_v4().to_string(),
                        logical_successor_request_id: Uuid::new_v4().to_string(),
                        decision_id: Uuid::now_v7(),
                        account_context_fingerprint: None,
                        deadline_at: deadline_at.unwrap_or_else(Utc::now),
                        max_attempts: 1,
                        requested_phase: phase,
                        phase,
                    })
                    .await
                    .map_err(|err| err.to_string())?;
                if record.phase != codex_state::GoalOwnerAdmissionPhase::Pending {
                    return Ok(false);
                }
                return self
                    .defer_provider_continuation(
                        turn_id.to_string(),
                        goal.goal_id,
                        retry_after.expect("pending admission has provider retry delay"),
                        GoalOwnerContinuation::new(record.continuation_authority()),
                    )
                    .await;
            }
            Some(_) | None => accounting.clear_active_goal(),
        }
        Ok(false)
    }

    async fn defer_provider_continuation(
        &self,
        turn_id: String,
        goal_id: String,
        retry_after: Duration,
        continuation: GoalOwnerContinuation,
    ) -> Result<bool, String> {
        let continuation = continuation.with_fence(
            Arc::clone(&self.inner.continuation.fence),
            self.inner.continuation.fence.current_epoch(),
        );
        enum ProviderContinuationAction {
            Blocked,
            Dormant,
            Scheduled {
                delay: Duration,
                cancellation: CancellationToken,
            },
        }

        let action = {
            let mut state = self
                .inner
                .continuation
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.goal_id.as_deref() != Some(goal_id.as_str()) {
                if let Some(pending) = state.pending.take() {
                    pending.cancellation.cancel();
                }
                state.goal_id = Some(goal_id.clone());
                state.attempts = 0;
                state.blocked = false;
            }
            if state.pending.is_some()
                || state.attempts >= MAX_PROVIDER_RATE_LIMIT_CONTINUATIONS_PER_GOAL
            {
                ProviderContinuationAction::Blocked
            } else if retry_after <= MAX_PROVIDER_RATE_LIMIT_CONTINUATION_DELAY {
                let delay = retry_after;
                state.attempts += 1;
                state.blocked = true;
                let eligible_at = Instant::now().checked_add(delay);
                let cancellation = CancellationToken::new();
                let authority = continuation.authority().authority.clone();
                state.pending = Some(DeferredProviderContinuation {
                    turn_id,
                    goal_id,
                    continuation,
                    eligible_at,
                    cancellation: cancellation.clone(),
                    enablement_epoch: self
                        .inner
                        .continuation
                        .enablement_epoch
                        .load(Ordering::Relaxed),
                });
                state.last_authority = Some(authority);
                ProviderContinuationAction::Scheduled {
                    delay,
                    cancellation,
                }
            } else {
                state.attempts = MAX_PROVIDER_RATE_LIMIT_CONTINUATIONS_PER_GOAL;
                state.blocked = true;
                state.pending = None;
                ProviderContinuationAction::Dormant
            }
        };
        if matches!(&action, &ProviderContinuationAction::Blocked) {
            self.block_provider_continuation();
            self.mark_provider_continuation_deferred().await?;
            return Ok(false);
        }
        let ProviderContinuationAction::Scheduled {
            delay,
            cancellation,
        } = action
        else {
            self.mark_provider_continuation_deferred().await?;
            tracing::info!(
                thread_id = %self.thread_id(),
                "provider rate-limit continuation remains dormant without a bounded eligible delay"
            );
            return Ok(false);
        };
        if let Err(err) = self.mark_provider_continuation_deferred().await {
            let mut state = self
                .inner
                .continuation
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.pending = None;
            state.attempts = MAX_PROVIDER_RATE_LIMIT_CONTINUATIONS_PER_GOAL;
            state.blocked = true;
            return Err(err);
        }
        let runtime = Arc::downgrade(&self.inner);
        drop(tokio::spawn(async move {
            tokio::select! {
                _ = cancellation.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
            let Some(inner) = runtime.upgrade() else {
                return;
            };
            let runtime = GoalRuntimeHandle { inner };
            if let Err(err) = runtime.continue_provider_preserved_goal().await {
                tracing::warn!(
                    thread_id = %runtime.thread_id(),
                    "failed to resume provider-preserved goal owner: {err}"
                );
            }
        }));
        Ok(true)
    }

    async fn mark_provider_continuation_deferred(&self) -> Result<(), String> {
        self.inner
            .state_dbs
            .thread_goals()
            .mark_thread_goal_continuation_deferred(self.thread_id())
            .await
            .map_err(|err| err.to_string())
    }

    fn block_provider_continuation(&self) {
        self.inner
            .continuation
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .blocked = true;
    }

    async fn cancel_and_retire_admission(
        &self,
        authority: &codex_state::GoalOwnerAdmissionAuthority,
    ) -> Result<(), String> {
        let admissions = self.inner.state_dbs.goal_owner_admissions();
        let cancelled = admissions
            .cancel(
                authority,
                codex_state::GoalOwnerAdmissionTerminalDisposition::AwaitUserTurn,
            )
            .await
            .map_err(|err| err.to_string())?;
        if let Some(cancelled) = cancelled
            && cancelled.phase == codex_state::GoalOwnerAdmissionPhase::Terminal
            && cancelled.terminal_outcome
                == codex_state::GoalOwnerAdmissionTerminalOutcome::Cancelled
        {
            self.retire_safe_terminal_admission(
                &cancelled.authority,
                /*clear_deferral*/ false,
                /*allow_exhausted*/ false,
            )
            .await?;
        } else {
            admissions
                .retire_cancelled_generation(
                    authority,
                    codex_state::GoalOwnerAdmissionRetirementReason::Superseded,
                )
                .await
                .map_err(|err| err.to_string())?;
        }
        Ok(())
    }

    /// Retire one exact terminal generation while preserving its provider outcome and history.
    /// Only definite pre-provider outcomes are safe to retire; uncertain and in-flight rows stay
    /// available for explicit recovery review.
    async fn retire_safe_terminal_admission(
        &self,
        authority: &codex_state::GoalOwnerAdmissionAuthority,
        clear_deferral: bool,
        allow_exhausted: bool,
    ) -> Result<bool, String> {
        let admissions = self.inner.state_dbs.goal_owner_admissions();
        let Some(record) = admissions
            .get_generation(authority)
            .await
            .map_err(|err| err.to_string())?
        else {
            return Ok(false);
        };
        if record.authority != *authority
            || record.phase != codex_state::GoalOwnerAdmissionPhase::Terminal
            || !matches!(
                record.terminal_outcome,
                codex_state::GoalOwnerAdmissionTerminalOutcome::Cancelled
                    | codex_state::GoalOwnerAdmissionTerminalOutcome::Succeeded
                    | codex_state::GoalOwnerAdmissionTerminalOutcome::Rejected
                    | codex_state::GoalOwnerAdmissionTerminalOutcome::Exhausted
            )
            || (record.terminal_outcome
                == codex_state::GoalOwnerAdmissionTerminalOutcome::Exhausted
                && !allow_exhausted)
        {
            return Ok(false);
        }
        admissions
            .retire(
                &record.authority,
                codex_state::GoalOwnerAdmissionRetirementReason::Superseded,
            )
            .await
            .map_err(|err| err.to_string())?;
        if clear_deferral {
            self.clear_deferral_if_retired(authority).await?;
        }
        Ok(true)
    }

    async fn clear_deferral_if_retired(
        &self,
        authority: &codex_state::GoalOwnerAdmissionAuthority,
    ) -> Result<(), String> {
        self.inner
            .state_dbs
            .goal_owner_admissions()
            .clear_deferral_if_retired(authority)
            .await
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    pub(crate) async fn cancel_provider_continuation(&self) {
        let Ok(_goal_state_permit) = self.goal_state_permit().await else {
            return;
        };
        self.cancel_provider_continuation_locked().await;
    }

    async fn cancel_provider_continuation_locked(&self) {
        let (pending, authority) = {
            let mut state = self
                .inner
                .continuation
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let pending = state.pending.take();
            let authority = pending
                .as_ref()
                .map(|pending| pending.continuation.authority().authority.clone())
                .or_else(|| state.last_authority.clone());
            (pending, authority)
        };
        if let Some(authority) = authority {
            if let Err(error) = self.cancel_and_retire_admission(&authority).await {
                tracing::warn!(
                    thread_id = %self.thread_id(),
                    error = %error,
                    "failed to durably cancel goal-owner continuation admission"
                );
            }
        }
        let Some(pending) = pending else {
            return;
        };
        pending.cancellation.cancel();
        let Some(thread_manager) = self.inner.thread_manager.upgrade() else {
            return;
        };
        let Ok(thread) = thread_manager.get_thread(self.inner.thread_id).await else {
            return;
        };
        thread
            .resolve_pending_owner_continuation(pending.turn_id.as_str())
            .await;
    }

    /// Retires the exact continuation generation after its successor turn has durably settled.
    /// The turn carries the immutable token, so a stale in-memory runtime authority can never
    /// retire a newer generation.
    pub(crate) async fn retire_settled_provider_continuation(
        &self,
        turn_store: &codex_extension_api::ExtensionData,
    ) -> Result<(), String> {
        let _goal_state_permit = self.goal_state_permit().await?;
        let Some(continuation) = turn_store.get::<GoalOwnerContinuation>() else {
            return Ok(());
        };
        let authority = continuation.authority().authority.clone();
        self.retire_safe_terminal_admission(
            &authority, /*clear_deferral*/ true, /*allow_exhausted*/ false,
        )
        .await?;
        Ok(())
    }

    /// Accounts the ending turn and stops its active goal after a terminal error.
    pub(crate) async fn stop_active_goal_for_turn(
        &self,
        turn_id: &str,
        reason: ActiveGoalStopReason,
    ) -> Result<(), String> {
        if !self.is_enabled() {
            return Ok(());
        }

        // Hold this through accounting and the status update so external goal
        // mutations and idle continuation cannot interleave between them.
        let _goal_state_permit = self.goal_state_permit().await?;
        if !self
            .inner
            .accounting_state
            .turn_is_current_active_goal(turn_id)
        {
            return Ok(());
        }

        let (event_name, status) = match reason {
            ActiveGoalStopReason::TurnError => {
                ("turn-error", codex_state::ThreadGoalStatus::Blocked)
            }
            ActiveGoalStopReason::UsageLimit => {
                ("usage-limit", codex_state::ThreadGoalStatus::UsageLimited)
            }
        };
        self.account_active_goal_progress(
            turn_id,
            &format!("{turn_id}:{event_name}-progress"),
            codex_state::GoalAccountingMode::ActiveOnly,
            BudgetLimitedGoalDisposition::ClearActive,
        )
        .await?;

        let Some(active_goal) = self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
            .map_err(|err| err.to_string())?
        else {
            self.inner.accounting_state.clear_active_goal();
            return Ok(());
        };
        let can_stop = active_goal.status == codex_state::ThreadGoalStatus::Active
            || (active_goal.status == codex_state::ThreadGoalStatus::BudgetLimited
                && status == codex_state::ThreadGoalStatus::UsageLimited);
        if !can_stop {
            self.inner.accounting_state.clear_active_goal();
            return Ok(());
        }
        let previous_status = Some(active_goal.status);
        let Some(goal) = self
            .inner
            .state_dbs
            .thread_goals()
            .update_thread_goal(
                self.thread_id(),
                codex_state::GoalUpdate {
                    objective: None,
                    status: Some(status),
                    token_budget: None,
                    expected_goal_id: Some(active_goal.goal_id),
                },
            )
            .await
            .map_err(|err| err.to_string())?
        else {
            return Ok(());
        };
        self.inner
            .metrics
            .record_terminal_if_status_changed(previous_status, &goal);
        self.inner.analytics.status_changed(
            &goal,
            previous_status,
            GoalEventAttribution::Turn(turn_id),
        );
        self.inner.accounting_state.clear_active_goal();
        let goal = protocol_goal_from_state(goal);
        self.inner.event_emitter.thread_goal_updated(
            format!("{turn_id}:{event_name}"),
            Some(turn_id.to_string()),
            goal,
        );
        Ok(())
    }

    pub async fn restore_after_resume(&self) -> Result<(), String> {
        let _goal_state_permit = self.goal_state_permit().await?;
        let admissions = self.inner.state_dbs.goal_owner_admissions();
        let persisted = admissions
            .get(self.thread_id())
            .await
            .map_err(|err| err.to_string())?;
        if let Some(record) = persisted.as_ref()
            && record.phase == codex_state::GoalOwnerAdmissionPhase::Terminal
        {
            if record.terminal_outcome == codex_state::GoalOwnerAdmissionTerminalOutcome::Exhausted
            {
                if let Some(goal) = self
                    .inner
                    .state_dbs
                    .thread_goals()
                    .get_thread_goal(self.thread_id())
                    .await
                    .map_err(|err| err.to_string())?
                    .filter(|goal| goal.goal_id == record.authority.goal_id)
                {
                    self.inner
                        .state_dbs
                        .thread_goals()
                        .update_thread_goal(
                            self.thread_id(),
                            codex_state::GoalUpdate {
                                objective: None,
                                status: Some(codex_state::ThreadGoalStatus::Blocked),
                                token_budget: None,
                                expected_goal_id: Some(goal.goal_id),
                            },
                        )
                        .await
                        .map_err(|err| err.to_string())?;
                }
                self.inner.accounting_state.clear_active_goal();
                return Ok(());
            }
            self.retire_safe_terminal_admission(
                &record.authority,
                /*clear_deferral*/ true,
                /*allow_exhausted*/ false,
            )
            .await?;
            // Uncertain terminal rows remain durable recovery evidence. They must not be
            // silently retired during resume because provider outcome is not knowable.
            return Ok(());
        }
        if !self.is_enabled() {
            if let Some(record) = persisted
                && record.phase == codex_state::GoalOwnerAdmissionPhase::Pending
            {
                self.cancel_and_retire_admission(&record.authority).await?;
                self.clear_deferral_if_retired(&record.authority).await?;
            }
            return Ok(());
        }

        let goal = self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
            .map_err(|err| err.to_string())?;
        match goal.as_ref() {
            Some(goal) if goal.status == codex_state::ThreadGoalStatus::Active => {
                self.inner
                    .accounting_state
                    .mark_idle_goal_active(goal.goal_id.clone());
                self.inner.metrics.record_resumed();
            }
            Some(_) | None => self.inner.accounting_state.clear_active_goal(),
        }

        // Rehydrate only the exact persisted pending generation. The continuation token,
        // successor IDs, and deadline all come from the durable record; no authority is
        // synthesized from the resumed thread or current configuration.
        let Some(record) = persisted else {
            return Ok(());
        };
        if record.phase != codex_state::GoalOwnerAdmissionPhase::Pending {
            return Ok(());
        }
        let Some(active_goal) = goal
            .as_ref()
            .filter(|goal| goal.status == codex_state::ThreadGoalStatus::Active)
        else {
            self.cancel_and_retire_admission(&record.authority).await?;
            self.clear_deferral_if_retired(&record.authority).await?;
            return Ok(());
        };
        if record.authority.goal_id != active_goal.goal_id {
            self.cancel_and_retire_admission(&record.authority).await?;
            self.clear_deferral_if_retired(&record.authority).await?;
            return Ok(());
        }
        if let Some(dispatch_claim_id) = record.dispatch_claim_id {
            // A claim cannot survive a process restart as an owner. Reopen the
            // exact pending generation for a fresh timer claim.
            admissions
                .release_dispatch_claim(&record.authority, dispatch_claim_id)
                .await
                .map_err(|err| err.to_string())?;
        }
        let retry_after = record
            .deadline_at
            .signed_duration_since(Utc::now())
            .to_std()
            .unwrap_or_default();
        if retry_after > MAX_PROVIDER_RATE_LIMIT_CONTINUATION_DELAY {
            tracing::warn!(
                thread_id = %self.thread_id(),
                "ignoring goal-owner admission with an unbounded persisted deadline"
            );
            return Ok(());
        }
        let continuation = GoalOwnerContinuation::new(record.continuation_authority()).with_fence(
            Arc::clone(&self.inner.continuation.fence),
            self.inner.continuation.fence.current_epoch(),
        );
        let cancellation = CancellationToken::new();
        let eligible_at = Instant::now().checked_add(retry_after);
        {
            let mut state = self
                .inner
                .continuation
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.pending.is_none() {
                state.goal_id = Some(active_goal.goal_id.clone());
                state.attempts = u8::try_from(record.attempts_started).unwrap_or(u8::MAX);
                state.blocked = true;
                state.pending = Some(DeferredProviderContinuation {
                    turn_id: record.origin_turn_id.clone(),
                    goal_id: active_goal.goal_id.clone(),
                    continuation,
                    eligible_at,
                    cancellation: cancellation.clone(),
                    enablement_epoch: self
                        .inner
                        .continuation
                        .enablement_epoch
                        .load(Ordering::Relaxed),
                });
                state.last_authority = Some(record.authority.clone());
            } else {
                return Ok(());
            }
        }
        self.mark_provider_continuation_deferred().await?;
        let runtime = Arc::downgrade(&self.inner);
        drop(tokio::spawn(async move {
            tokio::select! {
                _ = cancellation.cancelled() => return,
                _ = tokio::time::sleep(retry_after) => {}
            }
            let Some(inner) = runtime.upgrade() else {
                return;
            };
            let runtime = GoalRuntimeHandle { inner };
            if let Err(err) = runtime.continue_provider_preserved_goal().await {
                tracing::warn!(
                    thread_id = %runtime.thread_id(),
                    "failed to resume persisted provider continuation: {err}"
                );
            }
        }));
        Ok(())
    }

    pub(crate) async fn continue_if_idle(&self) -> Result<(), String> {
        if self.provider_continuation_pending() {
            self.continue_provider_preserved_goal().await?;
            return Ok(());
        }
        self.start_if_idle(
            |goal| vec![continuation_steering_item(&protocol_goal_from_state(goal))],
            /*goal_continuation*/ false,
        )
        .await?;
        Ok(())
    }

    fn provider_continuation_pending(&self) -> bool {
        self.inner
            .continuation
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .is_some()
    }

    async fn continue_provider_preserved_goal(&self) -> Result<(), String> {
        // Cancellation/takeover paths acquire the same gate. Holding it through the idle-start
        // reservation serializes the final authority check with the awaited start operation.
        let _goal_state_permit = self.goal_state_permit().await?;
        let (continuation_authority, cancellation, enablement_epoch) = {
            let state = self
                .inner
                .continuation
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(pending) = state.pending.as_ref() else {
                return Ok(());
            };
            if pending
                .eligible_at
                .is_some_and(|eligible_at| eligible_at > Instant::now())
            {
                return Ok(());
            }
            (
                pending.continuation.authority().clone(),
                pending.cancellation.clone(),
                pending.enablement_epoch,
            )
        };
        if !self
            .timer_is_current(
                &continuation_authority,
                &cancellation,
                enablement_epoch,
                None,
                /*require_pending*/ true,
            )
            .await?
        {
            return Ok(());
        }

        let Some(dispatch_claim_id) = self
            .inner
            .state_dbs
            .goal_owner_admissions()
            .claim_dispatch(&continuation_authority, Utc::now())
            .await
            .map_err(|err| err.to_string())?
        else {
            return Ok(());
        };
        let mut dispatch_claim_guard = DispatchClaimGuard::new(
            self.inner.state_dbs.goal_owner_admissions().clone(),
            continuation_authority.authority.clone(),
            dispatch_claim_id,
        );
        if !self
            .timer_is_current(
                &continuation_authority,
                &cancellation,
                enablement_epoch,
                Some(dispatch_claim_id),
                /*require_pending*/ true,
            )
            .await?
        {
            self.release_dispatch_claim_best_effort(
                &continuation_authority.authority,
                dispatch_claim_id,
            )
            .await;
            return Ok(());
        }
        let claimed_continuation = GoalOwnerContinuation::with_dispatch_claim(
            continuation_authority.clone(),
            dispatch_claim_id,
        );
        let Some(thread_manager) = self.inner.thread_manager.upgrade() else {
            self.release_dispatch_claim_best_effort(
                &continuation_authority.authority,
                dispatch_claim_id,
            )
            .await;
            return Ok(());
        };
        let Ok(thread) = thread_manager.get_thread(self.inner.thread_id).await else {
            self.release_dispatch_claim_best_effort(
                &continuation_authority.authority,
                dispatch_claim_id,
            )
            .await;
            return Ok(());
        };
        if !self
            .timer_is_current(
                &continuation_authority,
                &cancellation,
                enablement_epoch,
                Some(dispatch_claim_id),
                /*require_pending*/ true,
            )
            .await?
        {
            self.release_dispatch_claim_best_effort(
                &continuation_authority.authority,
                dispatch_claim_id,
            )
            .await;
            return Ok(());
        }
        // Keep this check immediately adjacent to the start call: cancellation and replacement
        // may retire this exact generation while the thread lookup is in flight.
        if !self
            .timer_is_current(
                &continuation_authority,
                &cancellation,
                enablement_epoch,
                Some(dispatch_claim_id),
                /*require_pending*/ true,
            )
            .await?
        {
            self.release_dispatch_claim_best_effort(
                &continuation_authority.authority,
                dispatch_claim_id,
            )
            .await;
            return Ok(());
        }
        let start_result = {
            let Some(_publication_guard) = claimed_continuation.enter_fence() else {
                self.release_dispatch_claim_best_effort(
                    &continuation_authority.authority,
                    dispatch_claim_id,
                )
                .await;
                return Ok(());
            };
            thread
                .try_start_goal_continuation_if_idle(Vec::new(), claimed_continuation)
                .await
        };
        if start_result.is_err() {
            self.release_dispatch_claim_best_effort(
                &continuation_authority.authority,
                dispatch_claim_id,
            )
            .await;
            return Ok(());
        }
        // The claim is now carried by the published turn token. Its abort
        // path owns exact cancellation; the wake-task guard must not race it.
        // Keep the guard armed until this publication call has definitely
        // succeeded so an aborted wake task cannot strand a pending claim.
        dispatch_claim_guard.disarm();
        let still_current = self
            .timer_is_current(
                &continuation_authority,
                &cancellation,
                enablement_epoch,
                Some(dispatch_claim_id),
                /*require_pending*/ false,
            )
            .await?;
        if !still_current {
            self.mark_provider_continuation_deferred().await?;
            let _ = self
                .timer_is_current(
                    &continuation_authority,
                    &cancellation,
                    enablement_epoch,
                    Some(dispatch_claim_id),
                    /*require_pending*/ false,
                )
                .await?;
            return Ok(());
        }
        self.inner
            .state_dbs
            .thread_goals()
            .clear_thread_goal_continuation_deferral(self.thread_id())
            .await
            .map_err(|err| err.to_string())?;
        if !self
            .timer_is_current(
                &continuation_authority,
                &cancellation,
                enablement_epoch,
                Some(dispatch_claim_id),
                /*require_pending*/ false,
            )
            .await?
        {
            // A newer generation may have been installed while the thread-scoped gate was being
            // cleared. Restore the conservative gate rather than clearing the newer generation.
            self.mark_provider_continuation_deferred().await?;
            let _ = self
                .timer_is_current(
                    &continuation_authority,
                    &cancellation,
                    enablement_epoch,
                    Some(dispatch_claim_id),
                    /*require_pending*/ false,
                )
                .await?;
            return Ok(());
        }
        let mut state = self
            .inner
            .continuation
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.pending.as_ref().is_some_and(|pending| {
            pending.enablement_epoch == enablement_epoch
                && pending_continuation_is_current(pending, &continuation_authority)
        }) {
            state.pending = None;
            state.blocked = false;
        }
        Ok(())
    }

    fn timer_cache_is_current(
        &self,
        continuation_authority: &codex_state::GoalOwnerAdmissionContinuationAuthority,
        cancellation: &CancellationToken,
        enablement_epoch: u64,
    ) -> bool {
        !cancellation.is_cancelled()
            && self.inner.continuation.enabled.load(Ordering::Relaxed)
            && self
                .inner
                .continuation
                .enablement_epoch
                .load(Ordering::Relaxed)
                == enablement_epoch
            && self
                .inner
                .continuation
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending
                .as_ref()
                .is_some_and(|pending| {
                    pending.enablement_epoch == enablement_epoch
                        && pending_continuation_is_current(pending, continuation_authority)
                })
    }

    async fn timer_is_current(
        &self,
        continuation_authority: &codex_state::GoalOwnerAdmissionContinuationAuthority,
        cancellation: &CancellationToken,
        enablement_epoch: u64,
        dispatch_claim_id: Option<Uuid>,
        require_pending: bool,
    ) -> Result<bool, String> {
        if !self.timer_cache_is_current(continuation_authority, cancellation, enablement_epoch) {
            return Ok(false);
        }
        let Some(record) = self
            .inner
            .state_dbs
            .goal_owner_admissions()
            .get_generation(&continuation_authority.authority)
            .await
            .map_err(|err| err.to_string())?
        else {
            return Ok(false);
        };
        if record.authority != continuation_authority.authority
            || record.continuation_authority() != *continuation_authority
            || record.retired_at.is_some()
            || (require_pending
                && (record.phase != codex_state::GoalOwnerAdmissionPhase::Pending
                    || record.dispatch_claim_id != dispatch_claim_id))
            || (!require_pending
                && record.phase == codex_state::GoalOwnerAdmissionPhase::Pending
                && record.dispatch_claim_id != dispatch_claim_id)
        {
            return Ok(false);
        }
        Ok(self.timer_cache_is_current(continuation_authority, cancellation, enablement_epoch))
    }

    async fn release_dispatch_claim_best_effort(
        &self,
        authority: &codex_state::GoalOwnerAdmissionAuthority,
        dispatch_claim_id: Uuid,
    ) {
        if let Err(error) = self
            .inner
            .state_dbs
            .goal_owner_admissions()
            .release_dispatch_claim(authority, dispatch_claim_id)
            .await
        {
            tracing::warn!(
                thread_id = %authority.thread_id,
                generation = authority.generation,
                dispatch_claim_id = %dispatch_claim_id,
                error = %error,
                "failed to release goal continuation dispatch claim; resume will retry exact cleanup"
            );
        }
    }

    async fn start_if_idle(
        &self,
        input_for_goal: impl FnOnce(codex_state::ThreadGoal) -> Vec<ResponseItem>,
        goal_continuation: bool,
    ) -> Result<bool, String> {
        if !self.tools_visible() {
            self.inner.accounting_state.clear_active_goal();
            return Ok(false);
        }
        // Hold this through the read/start window so external set/clear cannot
        // change the goal after we read it but before the continuation launches.
        let _goal_state_permit = self.goal_state_permit().await?;

        if !goal_continuation
            && self
                .inner
                .continuation
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .blocked
        {
            return Ok(false);
        }
        if !goal_continuation
            && self
                .inner
                .state_dbs
                .thread_goals()
                .has_thread_goal_continuation_deferral(self.thread_id())
                .await
                .map_err(|err| err.to_string())?
        {
            return Ok(false);
        }

        let Some(thread_manager) = self.inner.thread_manager.upgrade() else {
            tracing::debug!("skipping goal continuation because thread manager is unavailable");
            return Ok(false);
        };
        let Ok(thread) = thread_manager.get_thread(self.inner.thread_id).await else {
            tracing::debug!("skipping goal continuation because live thread is unavailable");
            return Ok(false);
        };

        let Some(goal) = self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
            .map_err(|err| err.to_string())?
        else {
            self.inner.accounting_state.clear_active_goal();
            return Ok(false);
        };
        if goal.status != codex_state::ThreadGoalStatus::Active {
            self.inner.accounting_state.clear_active_goal();
            return Ok(false);
        }
        // Provider-preserved continuations carry their exact authority through
        // `continue_provider_preserved_goal`; this ordinary idle path has no
        // continuation token and must never synthesize one for diagnostics.
        let start_result = thread.try_start_turn_if_idle(input_for_goal(goal)).await;
        match start_result {
            Ok(()) => {
                return Ok(true);
            }
            Err(err) => {
                let reason = err.reason();
                tracing::debug!(
                    ?reason,
                    "skipping goal continuation because automatic idle work was rejected"
                );
            }
        }
        Ok(false)
    }

    pub(crate) async fn inject_active_turn_steering(&self, item: ResponseItem) {
        let Some(thread_manager) = self.inner.thread_manager.upgrade() else {
            tracing::debug!("skipping goal steering because thread manager is unavailable");
            return;
        };
        let Ok(thread) = thread_manager.get_thread(self.inner.thread_id).await else {
            tracing::debug!("skipping goal steering because live thread is unavailable");
            return;
        };
        if thread.inject_if_running(vec![item]).await.is_err() {
            tracing::debug!("skipping goal steering because no turn is active");
        }
    }

    pub(crate) async fn account_active_goal_progress(
        &self,
        turn_id: &str,
        event_id: &str,
        mode: codex_state::GoalAccountingMode,
        budget_limited_goal_disposition: BudgetLimitedGoalDisposition,
    ) -> Result<Option<AccountedGoalProgress>, String> {
        let accounting = self.accounting_state();
        let _accounting_permit = accounting
            .progress_accounting_permit()
            .await
            .map_err(|err| err.to_string())?;
        let Some(snapshot) = accounting.progress_snapshot(turn_id) else {
            return Ok(None);
        };
        let previous_status = self
            .current_goal_status_for_metrics(Some(snapshot.expected_goal_id.as_str()))
            .await?;
        let outcome = self
            .inner
            .state_dbs
            .thread_goals()
            .account_thread_goal_usage(
                self.thread_id(),
                snapshot.time_delta_seconds,
                snapshot.token_delta,
                mode,
                Some(snapshot.expected_goal_id.as_str()),
            )
            .await
            .map_err(|err| err.to_string())?;
        Ok(match outcome {
            codex_state::GoalAccountingOutcome::Updated(goal) => {
                let goal_id = goal.goal_id.clone();
                self.inner
                    .metrics
                    .record_terminal_if_status_changed(previous_status, &goal);
                self.inner
                    .analytics
                    .usage_accounted(&goal, GoalEventAttribution::Turn(turn_id));
                self.inner.analytics.status_changed(
                    &goal,
                    previous_status,
                    GoalEventAttribution::Turn(turn_id),
                );
                accounting.mark_progress_accounted_for_status(
                    turn_id,
                    &snapshot,
                    goal.status,
                    budget_limited_goal_disposition,
                );
                let goal = protocol_goal_from_state(goal);
                self.inner.event_emitter.thread_goal_updated(
                    event_id.to_string(),
                    Some(turn_id.to_string()),
                    goal.clone(),
                );
                Some(AccountedGoalProgress { goal, goal_id })
            }
            codex_state::GoalAccountingOutcome::Unchanged(_) => None,
        })
    }

    async fn account_idle_goal_progress(
        &self,
        event_id: &str,
        mode: codex_state::GoalAccountingMode,
        budget_limited_goal_disposition: BudgetLimitedGoalDisposition,
    ) -> Result<Option<AccountedGoalProgress>, String> {
        let accounting = self.accounting_state();
        let _accounting_permit = accounting
            .progress_accounting_permit()
            .await
            .map_err(|err| err.to_string())?;
        let Some(snapshot) = accounting.idle_progress_snapshot() else {
            return Ok(None);
        };
        let previous_status = self
            .current_goal_status_for_metrics(Some(snapshot.expected_goal_id.as_str()))
            .await?;
        let outcome = self
            .inner
            .state_dbs
            .thread_goals()
            .account_thread_goal_usage(
                self.thread_id(),
                snapshot.time_delta_seconds,
                /*token_delta*/ 0,
                mode,
                Some(snapshot.expected_goal_id.as_str()),
            )
            .await
            .map_err(|err| err.to_string())?;
        Ok(match outcome {
            codex_state::GoalAccountingOutcome::Updated(goal) => {
                let goal_id = goal.goal_id.clone();
                self.inner
                    .metrics
                    .record_terminal_if_status_changed(previous_status, &goal);
                self.inner
                    .analytics
                    .usage_accounted(&goal, GoalEventAttribution::NoTurn);
                self.inner.analytics.status_changed(
                    &goal,
                    previous_status,
                    GoalEventAttribution::NoTurn,
                );
                accounting.mark_idle_progress_accounted_for_status(
                    &snapshot,
                    goal.status,
                    budget_limited_goal_disposition,
                );
                let goal = protocol_goal_from_state(goal);
                self.inner.event_emitter.thread_goal_updated(
                    event_id.to_string(),
                    /*turn_id*/ None,
                    goal.clone(),
                );
                Some(AccountedGoalProgress { goal, goal_id })
            }
            codex_state::GoalAccountingOutcome::Unchanged(_) => {
                accounting.reset_idle_progress_baseline_and_clear_active_goal();
                None
            }
        })
    }

    async fn current_goal_status_for_metrics(
        &self,
        expected_goal_id: Option<&str>,
    ) -> Result<Option<codex_state::ThreadGoalStatus>, String> {
        let goal = self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
            .map_err(|err| err.to_string())?;
        Ok(goal.and_then(|goal| {
            expected_goal_id
                .is_none_or(|expected_goal_id| goal.goal_id == expected_goal_id)
                .then_some(goal.status)
        }))
    }
}

fn pending_continuation_is_current(
    pending: &DeferredProviderContinuation,
    continuation_authority: &codex_state::GoalOwnerAdmissionContinuationAuthority,
) -> bool {
    !pending.cancellation.is_cancelled()
        && pending.continuation.authority() == continuation_authority
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authority(generation: i64) -> codex_state::GoalOwnerAdmissionAuthority {
        codex_state::GoalOwnerAdmissionAuthority {
            thread_id: ThreadId::new(),
            goal_id: "goal".to_string(),
            generation,
            cancellation_epoch: 0,
        }
    }

    fn pending(
        authority: codex_state::GoalOwnerAdmissionAuthority,
    ) -> DeferredProviderContinuation {
        DeferredProviderContinuation {
            turn_id: "turn".to_string(),
            goal_id: authority.goal_id.clone(),
            continuation: GoalOwnerContinuation::new(
                codex_state::GoalOwnerAdmissionContinuationAuthority {
                    authority,
                    intended_request_kind: "turn".to_string(),
                    successor_turn_id: "successor".to_string(),
                    logical_successor_request_id: "logical".to_string(),
                    decision_id: Uuid::nil(),
                },
            ),
            eligible_at: None,
            cancellation: CancellationToken::new(),
            enablement_epoch: 0,
        }
    }

    #[test]
    fn pending_continuation_match_requires_exact_live_authority() {
        let first_authority = authority(1);
        let mut first = pending(first_authority.clone());
        let first_continuation_authority = first.continuation.authority().clone();
        assert!(pending_continuation_is_current(
            &first,
            &first_continuation_authority,
        ));

        let newer_same_goal = codex_state::GoalOwnerAdmissionAuthority {
            generation: 2,
            ..first_authority.clone()
        };
        let newer_continuation_authority = codex_state::GoalOwnerAdmissionContinuationAuthority {
            authority: newer_same_goal,
            ..first_continuation_authority.clone()
        };
        assert!(!pending_continuation_is_current(
            &first,
            &newer_continuation_authority,
        ));

        first.cancellation.cancel();
        assert!(!pending_continuation_is_current(
            &first,
            &first_continuation_authority,
        ));
    }
}
