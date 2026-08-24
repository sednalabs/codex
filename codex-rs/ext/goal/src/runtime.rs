use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use codex_core::ThreadManager;
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
    enabled: AtomicBool,
    provider_continuation: Mutex<ProviderContinuationState>,
    tools_available_for_thread: bool,
    goal_state_lock: Semaphore,
}

struct DeferredProviderContinuation {
    turn_id: String,
    goal_id: String,
    eligible_at: Option<Instant>,
    cancellation: CancellationToken,
}

#[derive(Default)]
struct ProviderContinuationState {
    goal_id: Option<String>,
    attempts: u8,
    /// In-memory fail-closed gate for this runtime. The durable thread-goal gate below
    /// covers resume; this bit also covers a persistence failure or an exhausted attempt.
    blocked: bool,
    pending: Option<DeferredProviderContinuation>,
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
                enabled: AtomicBool::new(config.enabled),
                provider_continuation: Mutex::new(ProviderContinuationState::default()),
                tools_available_for_thread: config.tools_available_for_thread,
                goal_state_lock: Semaphore::new(/*permits*/ 1),
            }),
        }
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.inner.enabled.store(enabled, Ordering::Relaxed);
        if !enabled {
            let mut state = self
                .inner
                .provider_continuation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(pending) = state.pending.take() {
                pending.cancellation.cancel();
            }
            state.attempts = MAX_PROVIDER_RATE_LIMIT_CONTINUATIONS_PER_GOAL;
            state.blocked = true;
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Relaxed)
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
            .goal_state_lock
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
            let pending = {
                let mut state = self
                    .inner
                    .provider_continuation
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
        retry_after: Option<Duration>,
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
                return self
                    .defer_provider_continuation(turn_id.to_string(), goal.goal_id, retry_after)
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
        retry_after: Option<Duration>,
    ) -> Result<bool, String> {
        let mut state = self
            .inner
            .provider_continuation
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
            drop(state);
            self.block_provider_continuation();
            self.mark_provider_continuation_deferred().await?;
            return Ok(false);
        }
        let Some(delay) =
            retry_after.filter(|delay| *delay <= MAX_PROVIDER_RATE_LIMIT_CONTINUATION_DELAY)
        else {
            state.attempts = MAX_PROVIDER_RATE_LIMIT_CONTINUATIONS_PER_GOAL;
            state.blocked = true;
            state.pending = None;
            drop(state);
            self.mark_provider_continuation_deferred().await?;
            tracing::info!(
                thread_id = %self.thread_id(),
                "provider rate-limit continuation remains dormant without a bounded eligible delay"
            );
            return Ok(false);
        };
        state.attempts += 1;
        state.blocked = true;
        let eligible_at = Instant::now().checked_add(delay);
        let cancellation = CancellationToken::new();
        state.pending = Some(DeferredProviderContinuation {
            turn_id,
            goal_id,
            eligible_at,
            cancellation: cancellation.clone(),
        });
        drop(state);
        if let Err(err) = self.mark_provider_continuation_deferred().await {
            let mut state = self
                .inner
                .provider_continuation
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
            .provider_continuation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .blocked = true;
    }

    pub(crate) async fn cancel_provider_continuation(&self) {
        let pending = self
            .inner
            .provider_continuation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .take();
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
        if !self.is_enabled() {
            return Ok(());
        }

        let goal = self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
            .map_err(|err| err.to_string())?;
        match goal {
            Some(goal) if goal.status == codex_state::ThreadGoalStatus::Active => {
                self.inner
                    .accounting_state
                    .mark_idle_goal_active(goal.goal_id);
                self.inner.metrics.record_resumed();
            }
            Some(_) | None => self.inner.accounting_state.clear_active_goal(),
        }
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
            .provider_continuation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .is_some()
    }

    async fn continue_provider_preserved_goal(&self) -> Result<(), String> {
        let eligible_goal_id = {
            let state = self
                .inner
                .provider_continuation
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
            pending.goal_id.clone()
        };

        if self
            .start_if_idle(|_| Vec::new(), /*goal_continuation*/ true)
            .await?
        {
            let mut state = self
                .inner
                .provider_continuation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state
                .pending
                .as_ref()
                .is_some_and(|pending| pending.goal_id == eligible_goal_id)
            {
                state.pending = None;
            }
            state.blocked = false;
            drop(state);
            self.inner
                .state_dbs
                .thread_goals()
                .clear_thread_goal_continuation_deferral(self.thread_id())
                .await
                .map_err(|err| err.to_string())?;
        }
        Ok(())
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
                .provider_continuation
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
        let start_result = if goal_continuation
            && codex_core::diagnostic_flags::goal_continuation_health_check_enabled()
        {
            thread
                .try_start_goal_continuation_if_idle(input_for_goal(goal))
                .await
        } else {
            thread.try_start_turn_if_idle(input_for_goal(goal)).await
        };
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
