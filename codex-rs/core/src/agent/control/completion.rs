//! Delivers terminal child results and completion activity to the agent tree.
//!
//! Sessions capture terminal state; the controller owns routing and queue-only delivery.
//! Delivery remains best effort; metrics distinguish queue acceptance from delivery failure.

use super::LocalAgentControl;
use crate::TurnStartOptions;
use crate::agent::api::AgentGoalControlSnapshot;
use crate::agent::api::AgentLogicalGoal;
use crate::agent::api::AgentTurnOutcome;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::session_prefix::format_guardian_interruption_message;
use crate::session_prefix::format_inter_agent_completion_message;
use codex_agent_mailbox::Continuation;
use codex_agent_mailbox::GoalBinding;
use codex_agent_mailbox::Outcome;
use codex_features::Feature;
use codex_protocol::AgentPath;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentActivityKind;
use codex_protocol::protocol::SubAgentSource;
use codex_rollout_trace::AgentResultTracePayload;
use codex_rollout_trace::ThreadTraceContext;
use tracing::debug;

impl LocalAgentControl {
    /// Routes a captured terminal outcome without retaining the child's live turn context.
    pub(crate) async fn notify_parent_of_terminal_turn(
        &self,
        outcome: AgentTurnOutcome,
        trace: &ThreadTraceContext,
    ) {
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            agent_path: Some(child_agent_path),
            ..
        }) = &outcome.source
        else {
            return;
        };
        let parent_thread_id = *parent_thread_id;
        let goal_control_superseded = match (
            &outcome.goal_control_snapshot_at_start,
            &outcome.logical_goal,
        ) {
            (Ok(start), Ok(current)) => !start.matches(current.as_ref()),
            _ => false,
        };
        let status = match &outcome.status {
            AgentStatus::Interrupted | AgentStatus::Shutdown => outcome.status.clone(),
            _ if goal_control_superseded => AgentStatus::Interrupted,
            _ => outcome.status.clone(),
        };
        let Some(parent_agent_path) = child_agent_path
            .as_str()
            .rsplit_once('/')
            .and_then(|(parent, _)| AgentPath::try_from(parent).ok())
        else {
            return;
        };

        if matches!(status, AgentStatus::Completed(_))
            && let Some(parent_turn_id) = outcome.parent_turn_id
        {
            let initiating_thread_id = match outcome.initiating_agent_path.as_ref() {
                Some(initiating_agent_path) if initiating_agent_path != &parent_agent_path => {
                    self.runtime.resolve_agent_reference(
                        outcome.thread_id,
                        &outcome.source,
                        initiating_agent_path.as_str(),
                    )
                    .await
                    .inspect_err(|err| {
                        debug!(
                            "failed to resolve completed activity initiator {initiating_agent_path}: {err}"
                        );
                    })
                    .ok()
                }
                _ => Some(parent_thread_id),
            };
            if let Some(initiating_thread_id) = initiating_thread_id
                && let Err(err) = self
                    .emit_sub_agent_activity(
                        initiating_thread_id,
                        parent_turn_id,
                        SubAgentActivityItem {
                            model: None,
                            reasoning_effort: None,
                            id: format!("subagent-completed-{}", outcome.turn_id),
                            kind: SubAgentActivityKind::Completed,
                            agent_thread_id: outcome.thread_id,
                            agent_path: child_agent_path.clone(),
                        },
                    )
                    .await
            {
                debug!(
                    "failed to emit completed activity to initiating thread {initiating_thread_id}: {err}"
                );
            }
        }

        let message = match (&status, outcome.error_info) {
            (AgentStatus::Errored(error), Some(CodexErrorInfo::TooManyDenials)) => {
                Some(format_guardian_interruption_message(
                    parent_agent_path.clone(),
                    child_agent_path.clone(),
                    error,
                ))
            }
            _ => format_inter_agent_completion_message(
                parent_agent_path.clone(),
                child_agent_path.clone(),
                &status,
            ),
        };
        let Some(message) = message else {
            return;
        };
        let trace_message = trace.is_enabled().then(|| message.clone());

        let parent_config = match self.runtime.upgrade() {
            Ok(state) => match state.get_thread(parent_thread_id).await {
                Ok(parent_thread) => Some(parent_thread.session.get_config().await),
                Err(_) => None,
            },
            Err(_) => None,
        };
        let parent_supports_native_mailbox = parent_config.as_ref().is_some_and(|config| {
            config.multi_agent_version_from_features() == MultiAgentVersion::V2
                && config.features.enabled(Feature::AgentMailbox)
        });

        if parent_supports_native_mailbox && let Some(mailbox) = self.mailbox_v2() {
            let logical_goal = match outcome.logical_goal.as_ref() {
                Ok(goal) => goal,
                Err(error) => {
                    debug!(
                        "failed to capture native logical goal for terminal work outcome: {error}"
                    );
                    return;
                }
            };
            let goal_control_snapshot_at_start = match outcome.goal_control_snapshot_at_start.as_ref()
            {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    debug!("failed to capture goal control snapshot at turn start: {error}");
                    return;
                }
            };
            let (native_outcome, continuation) = goal_continuation_for_terminal(
                &outcome.status,
                goal_control_superseded,
                goal_control_snapshot_at_start,
                logical_goal.as_ref(),
                parent_thread_id,
                outcome.thread_id,
            );
            let activate_if_idle = parent_config
                .as_ref()
                .is_some_and(|config| config.features.enabled(Feature::AgentMailboxIdleActivation));
            let response = (message.len() <= 8_192).then(|| {
                InterAgentCommunication::new(
                    child_agent_path.clone(),
                    parent_agent_path.clone(),
                    Vec::new(),
                    message.clone(),
                    /*trigger_turn*/ false,
                )
            });
            let receipt = match mailbox
                .record_turn_outcome(
                    outcome.thread_id,
                    &outcome.turn_id,
                    native_outcome,
                    response,
                    continuation,
                    activate_if_idle,
                )
                .await
            {
                Ok(receipt) => receipt,
                Err(error) => {
                    debug!("failed to persist native terminal work outcome: {error}");
                    return;
                }
            };
            if receipt.stale_turn {
                return;
            }
            if !receipt.no_matching_work {
                if let Err(error) = self
                    .mailbox_v2_deliver_outcome_messages(
                        parent_thread_id,
                        &receipt.response_message_ids,
                    )
                    .await
                {
                    debug!(
                        "failed to admit durable terminal response to parent input queue: {error}"
                    );
                    return;
                }
                if let Some(message) = trace_message.as_ref() {
                    trace.record_agent_result_interaction(
                        outcome.turn_id.as_str(),
                        parent_thread_id,
                        &AgentResultTracePayload {
                            child_agent_path: child_agent_path.as_str(),
                            message,
                            status: &status,
                        },
                    );
                }
                return;
            }
        }
        // `communication` owns the message. Keep a second copy only when the
        // recorder will actually need it after parent delivery succeeds.
        let communication = InterAgentCommunication::new(
            child_agent_path.clone(),
            parent_agent_path,
            Vec::new(),
            message,
            /*trigger_turn*/ false,
        );
        let context =
            AgentCommunicationContext::new(AgentCommunicationKind::Result, outcome.thread_id)
                .with_native_key(parent_supports_native_mailbox.then(|| outcome.turn_id.clone()));
        let delivery = self
            .send_inter_agent_communication(
                parent_thread_id,
                communication,
                context,
                TurnStartOptions::default(),
            )
            .await;
        if let Some(metrics) = codex_otel::global() {
            let _ = metrics.counter(
                "codex.multi_agent.result_delivery",
                /*inc*/ 1,
                &[(
                    "outcome",
                    if delivery.is_ok() { "queued" } else { "failed" },
                )],
            );
        }
        if let Err(err) = delivery {
            debug!("failed to notify parent thread {parent_thread_id}: {err}");
            return;
        }
        if let Some(message) = trace_message {
            trace.record_agent_result_interaction(
                outcome.turn_id.as_str(),
                parent_thread_id,
                &AgentResultTracePayload {
                    child_agent_path: child_agent_path.as_str(),
                    message: &message,
                    status: &status,
                },
            );
        }
    }
}

fn goal_continuation_for_terminal(
    status: &AgentStatus,
    goal_control_superseded: bool,
    start: &AgentGoalControlSnapshot,
    current: Option<&AgentLogicalGoal>,
    parent_thread_id: codex_protocol::ThreadId,
    child_thread_id: codex_protocol::ThreadId,
) -> (Outcome, Continuation) {
    let binding = |goal_id: String, control_generation| GoalBinding {
        goal_id,
        parent_thread_id,
        child_thread_id,
        control_generation,
        incarnation: Some(child_thread_id),
    };

    if matches!(status, AgentStatus::Interrupted | AgentStatus::Shutdown) {
        // Cancellation is a stop, not a continuation inferred from the latest goal status.
        return (Outcome::Cancelled, Continuation::None);
    }

    if goal_control_superseded {
        // Cancel only work bound to the old control snapshot. A later same-ID control revision
        // has a different binding and cannot be settled by this terminal turn.
        return (
            Outcome::Cancelled,
            start.goal_id.as_ref().map_or(Continuation::None, |goal_id| {
                Continuation::Terminal(binding(
                    goal_id.clone(),
                    start.control_generation,
                ))
            }),
        );
    }

    let outcome = match status {
        AgentStatus::Completed(_) => Outcome::Completed,
        AgentStatus::Errored(_) => Outcome::NeedsAttention,
        AgentStatus::Interrupted | AgentStatus::Shutdown => unreachable!(),
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::NotFound => {
            return (Outcome::Cancelled, Continuation::None);
        }
    };
    let continuation = current.map_or(Continuation::None, |goal| {
        let binding = binding(goal.goal_id.clone(), goal.control_generation);
        match goal.status {
            codex_protocol::protocol::ThreadGoalStatus::Active => {
                Continuation::PendingGoalBinding(binding)
            }
            codex_protocol::protocol::ThreadGoalStatus::Paused
            | codex_protocol::protocol::ThreadGoalStatus::Blocked
            | codex_protocol::protocol::ThreadGoalStatus::UsageLimited => {
                Continuation::Deferred(binding)
            }
            codex_protocol::protocol::ThreadGoalStatus::BudgetLimited
            | codex_protocol::protocol::ThreadGoalStatus::Complete => {
                Continuation::Terminal(binding)
            }
        }
    });
    (outcome, continuation)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start_goal() -> AgentGoalControlSnapshot {
        AgentGoalControlSnapshot {
            goal_id: Some("goal-1".to_string()),
            control_generation: 4,
        }
    }

    fn logical_goal(status: codex_protocol::protocol::ThreadGoalStatus) -> AgentLogicalGoal {
        AgentLogicalGoal {
            goal_id: "goal-1".to_string(),
            status,
            control_generation: 4,
        }
    }

    #[test]
    fn same_id_external_control_change_cancels_old_binding() {
        let mut externally_updated = logical_goal(
            codex_protocol::protocol::ThreadGoalStatus::Active,
        );
        externally_updated.control_generation += 1;
        let start = start_goal();
        let goal_control_superseded = !start.matches(Some(&externally_updated));
        let parent = codex_protocol::ThreadId::default();
        let child = codex_protocol::ThreadId::default();

        let (outcome, continuation) = goal_continuation_for_terminal(
            &AgentStatus::Completed(Some("stale result".to_string())),
            goal_control_superseded,
            &start,
            Some(&externally_updated),
            parent,
            child,
        );

        assert_eq!(Outcome::Cancelled, outcome);
        assert_eq!(
            Continuation::Terminal(GoalBinding {
                goal_id: "goal-1".to_string(),
                parent_thread_id: parent,
                child_thread_id: child,
                control_generation: 4,
                incarnation: Some(child),
            }),
            continuation
        );
    }

    #[test]
    fn active_turn_goal_update_and_quiet_continuation_are_preserved() {
        let parent = codex_protocol::ThreadId::default();
        let child = codex_protocol::ThreadId::default();
        let active = logical_goal(codex_protocol::protocol::ThreadGoalStatus::Active);
        let (active_outcome, active_continuation) = goal_continuation_for_terminal(
            &AgentStatus::Completed(None),
            /*goal_control_superseded*/ false,
            &start_goal(),
            Some(&active),
            parent,
            child,
        );
        assert_eq!(Outcome::Completed, active_outcome);
        assert_eq!(
            Continuation::PendingGoalBinding(GoalBinding {
                goal_id: "goal-1".to_string(),
                parent_thread_id: parent,
                child_thread_id: child,
                control_generation: 4,
                incarnation: Some(child),
            }),
            active_continuation
        );

        let complete = logical_goal(codex_protocol::protocol::ThreadGoalStatus::Complete);
        let (complete_outcome, complete_continuation) = goal_continuation_for_terminal(
            &AgentStatus::Completed(None),
            /*goal_control_superseded*/ false,
            &start_goal(),
            Some(&complete),
            parent,
            child,
        );
        assert_eq!(Outcome::Completed, complete_outcome);
        assert!(matches!(complete_continuation, Continuation::Terminal(_)));
    }

    #[test]
    fn explicit_cancellation_never_infers_goal_continuation() {
        let active = logical_goal(codex_protocol::protocol::ThreadGoalStatus::Active);
        let (outcome, continuation) = goal_continuation_for_terminal(
            &AgentStatus::Interrupted,
            /*goal_control_superseded*/ false,
            &start_goal(),
            Some(&active),
            codex_protocol::ThreadId::default(),
            codex_protocol::ThreadId::default(),
        );
        assert_eq!(Outcome::Cancelled, outcome);
        assert_eq!(Continuation::None, continuation);
    }
}
