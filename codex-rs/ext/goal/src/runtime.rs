use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

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
    continuation_blocked: AtomicBool,
    tools_available_for_thread: bool,
    goal_state_lock: Semaphore,
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
                continuation_blocked: AtomicBool::new(false),
                tools_available_for_thread: config.tools_available_for_thread,
                goal_state_lock: Semaphore::new(/*permits*/ 1),
            }),
        }
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.inner.enabled.store(enabled, Ordering::Relaxed);
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

    /// Stop automatic continuation when a persistence or accounting boundary
    /// is uncertain. The in-memory clear prevents the current process from
    /// immediately requesting work, while the durable deferral prevents a
    /// later idle callback or reload from doing so silently.
    pub(crate) async fn fail_closed_continuation_boundary(&self, turn_id: Option<&str>) {
        if let Some(turn_id) = turn_id {
            self.inner.accounting_state.finish_turn(turn_id);
        }
        self.inner.accounting_state.clear_active_goal();
        self.inner
            .continuation_blocked
            .store(true, Ordering::Release);
        if let Err(error) = self
            .inner
            .state_dbs
            .thread_goals()
            .mark_thread_goal_continuation_deferral(self.thread_id())
            .await
        {
            tracing::error!(
                thread_id = %self.thread_id(),
                %error,
                "failed to persist fail-closed goal continuation deferral"
            );
        }
    }

    pub(crate) fn clear_continuation_block(&self) {
        self.inner
            .continuation_blocked
            .store(false, Ordering::Release);
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
            if let Err(error) = self
                .account_active_goal_progress(
                    turn_id.as_str(),
                    &format!("{turn_id}:external-goal-mutation"),
                    codex_state::GoalAccountingMode::ActiveOnly,
                    BudgetLimitedGoalDisposition::ClearActive,
                )
                .await
            {
                self.fail_closed_continuation_boundary(Some(turn_id.as_str()))
                    .await;
                return Err(error);
            }
            return Ok(());
        }

        if let Err(error) = self
            .account_idle_goal_progress(
                &format!("{}:external-goal-mutation", self.inner.thread_id),
                codex_state::GoalAccountingMode::ActiveOnly,
                BudgetLimitedGoalDisposition::ClearActive,
            )
            .await
        {
            self.fail_closed_continuation_boundary(None).await;
            return Err(error);
        }
        Ok(())
    }

    pub async fn apply_external_goal_set(
        &self,
        goal: codex_state::ThreadGoal,
        previous_goal: Option<PreviousGoalSnapshot>,
    ) -> Result<(), String> {
        // A new explicit goal supersedes any persisted diagnostic continuation
        // marker from a previous goal, even if the extension was disabled by
        // the time the explicit mutation arrived.
        if let Err(err) = self
            .inner
            .state_dbs
            .thread_goals()
            .clear_thread_goal_continuity_research(self.thread_id())
            .await
        {
            let current_turn_id = self.inner.accounting_state.current_turn_id();
            self.fail_closed_continuation_boundary(current_turn_id.as_deref())
                .await;
            return Err(err.to_string());
        }
        self.clear_continuation_block();
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
                if let Err(error) = self.continue_if_idle().await {
                    self.fail_closed_continuation_boundary(None).await;
                    return Err(error);
                }
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
        self.inner.accounting_state.clear_active_goal();
        self.clear_continuation_block();
        Ok(())
    }

    pub async fn usage_limit_active_goal_for_turn(&self, turn_id: &str) -> Result<(), String> {
        self.stop_active_goal_for_turn(turn_id, ActiveGoalStopReason::UsageLimit)
            .await
    }

    pub(crate) async fn preserve_active_goal_after_turn_error(
        &self,
        turn_id: &str,
    ) -> Result<(), String> {
        if !self.is_enabled() {
            return Ok(());
        }

        let _goal_state_permit = match self.goal_state_permit().await {
            Ok(permit) => permit,
            Err(error) => {
                self.fail_closed_continuation_boundary(Some(turn_id)).await;
                return Err(error);
            }
        };
        let accounting = self.accounting_state();
        if !accounting.turn_is_current_active_goal(turn_id) {
            accounting.finish_turn(turn_id);
            return Ok(());
        }

        // A preserved turn still consumed real wall-clock/token progress. Charge
        // that delta before ending the turn, while retaining the idle goal for
        // the explicitly selected continuation path. The accounting snapshot is
        // marked here, so a later stop/finish hook cannot charge it twice.
        if let Err(error) = self
            .account_active_goal_progress(
                turn_id,
                &format!("{turn_id}:continuity-preserve-progress"),
                codex_state::GoalAccountingMode::ActiveOnly,
                BudgetLimitedGoalDisposition::KeepActive,
            )
            .await
        {
            self.fail_closed_continuation_boundary(Some(turn_id)).await;
            return Err(error);
        }

        let goal = match self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
        {
            Ok(goal) => goal,
            Err(error) => {
                self.fail_closed_continuation_boundary(Some(turn_id)).await;
                return Err(error.to_string());
            }
        };

        match goal {
            Some(goal) if goal.status == codex_state::ThreadGoalStatus::Active => {
                let marker_armed = match self
                    .inner
                    .state_dbs
                    .thread_goals()
                    .arm_thread_goal_continuity_research_if_active(self.thread_id())
                    .await
                {
                    Ok(armed) => armed,
                    Err(err) => {
                        // Do not leave an in-memory active goal behind when the
                        // durable recovery capability could not be committed.
                        // That would make the next idle boundary indistinguishable
                        // from an explicitly preserved turn.
                        self.fail_closed_continuation_boundary(Some(turn_id)).await;
                        return Err(err.to_string());
                    }
                };
                if !marker_armed {
                    self.fail_closed_continuation_boundary(Some(turn_id)).await;
                    return Err(
                        "active goal changed before continuity marker could be armed".into(),
                    );
                }
                accounting.finish_turn(turn_id);
                accounting.mark_idle_goal_active(goal.goal_id);
            }
            Some(_) | None => {
                accounting.finish_turn(turn_id);
                if self
                    .inner
                    .state_dbs
                    .thread_goals()
                    .clear_thread_goal_continuity_research(self.thread_id())
                    .await
                    .is_err()
                {
                    self.fail_closed_continuation_boundary(Some(turn_id)).await;
                    return Err("failed to clear continuity marker after goal state changed".into());
                }
                accounting.clear_active_goal();
            }
        }
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
        let _goal_state_permit = match self.goal_state_permit().await {
            Ok(permit) => permit,
            Err(error) => {
                self.fail_closed_continuation_boundary(Some(turn_id)).await;
                return Err(error);
            }
        };
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
        if let Err(error) = self
            .account_active_goal_progress(
                turn_id,
                &format!("{turn_id}:{event_name}-progress"),
                codex_state::GoalAccountingMode::ActiveOnly,
                BudgetLimitedGoalDisposition::ClearActive,
            )
            .await
        {
            self.fail_closed_continuation_boundary(Some(turn_id)).await;
            return Err(error);
        }

        let Some(active_goal) = (match self
            .inner
            .state_dbs
            .thread_goals()
            .get_thread_goal(self.thread_id())
            .await
        {
            Ok(goal) => goal,
            Err(error) => {
                self.fail_closed_continuation_boundary(Some(turn_id)).await;
                return Err(error.to_string());
            }
        }) else {
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
        let Some(goal) = (match self
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
        {
            Ok(goal) => goal,
            Err(error) => {
                self.fail_closed_continuation_boundary(Some(turn_id)).await;
                return Err(error.to_string());
            }
        }) else {
            self.fail_closed_continuation_boundary(Some(turn_id)).await;
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
        if self.inner.continuation_blocked.load(Ordering::Acquire) {
            self.inner.accounting_state.clear_active_goal();
            return Ok(());
        }
        if !self.tools_visible() {
            self.inner.accounting_state.clear_active_goal();
            return Ok(());
        }
        // Hold this through the read/start window so external set/clear cannot
        // change the goal after we read it but before the continuation launches.
        let _goal_state_permit = self.goal_state_permit().await?;

        let continuity_research_marker = self
            .inner
            .state_dbs
            .thread_goals()
            .has_thread_goal_continuity_research(self.thread_id())
            .await
            .map_err(|err| err.to_string())?;
        if continuity_research_marker {
            if !codex_core::diagnostic_flags::continuity_preserve_after_usage_limit_enabled() {
                // This goal was preserved by an earlier opt-in run. Do not
                // silently resume provider work after flags are removed or a
                // process restart restores the persisted goal.
                self.inner.accounting_state.clear_active_goal();
                tracing::debug!(
                    thread_id = %self.thread_id(),
                    "skipping preserved goal continuation because continuity research is disabled"
                );
                return Ok(());
            }
            self.inner
                .state_dbs
                .thread_goals()
                .clear_thread_goal_continuity_research(self.thread_id())
                .await
                .map_err(|err| err.to_string())?;
        }

        if self
            .inner
            .state_dbs
            .thread_goals()
            .has_thread_goal_continuation_deferral(self.thread_id())
            .await
            .map_err(|err| err.to_string())?
        {
            return Ok(());
        }

        let Some(thread_manager) = self.inner.thread_manager.upgrade() else {
            tracing::debug!("skipping goal continuation because thread manager is unavailable");
            return Ok(());
        };
        let Ok(thread) = thread_manager.get_thread(self.inner.thread_id).await else {
            tracing::debug!("skipping goal continuation because live thread is unavailable");
            return Ok(());
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
            return Ok(());
        };
        if goal.status != codex_state::ThreadGoalStatus::Active {
            self.inner.accounting_state.clear_active_goal();
            return Ok(());
        }
        let item = continuation_steering_item(&protocol_goal_from_state(goal));
        let continuity_observation = codex_core::diagnostic_flags::continuity_observation_enabled();
        let correlation_id = format!("thread:{}:continuation", self.thread_id());
        if continuity_observation {
            codex_core::diagnostic_flags::record_continuity_stage_with_context(
                &thread.session_telemetry(),
                "parent",
                "continuation_attempt",
                "direct_probe",
                Some(correlation_id.as_str()),
            );
        }

        match thread.try_start_turn_if_idle(vec![item]).await {
            Ok(()) => {
                if continuity_observation {
                    codex_core::diagnostic_flags::record_continuity_stage_with_context(
                        &thread.session_telemetry(),
                        "parent",
                        "continuation_started",
                        "direct_probe",
                        Some(correlation_id.as_str()),
                    );
                }
            }
            Err(err) => {
                if continuity_observation {
                    codex_core::diagnostic_flags::record_continuity_stage_with_context(
                        &thread.session_telemetry(),
                        "parent",
                        "continuation_rejected",
                        "direct_probe",
                        Some(correlation_id.as_str()),
                    );
                }
                let reason = err.reason();
                tracing::debug!(
                    ?reason,
                    "skipping goal continuation because automatic idle work was rejected"
                );
            }
        }

        let current_turn_is_goal_active = self
            .inner
            .accounting_state
            .current_turn_id()
            .is_some_and(|turn_id| {
                self.inner
                    .accounting_state
                    .turn_is_current_active_goal(turn_id.as_str())
            });
        if !current_turn_is_goal_active {
            self.inner.accounting_state.clear_active_goal();
        }
        Ok(())
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
