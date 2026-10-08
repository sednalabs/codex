//! Delivers terminal child results and completion activity to the agent tree.
//!
//! Sessions capture terminal state; the controller owns routing and queue-only delivery.
//! Delivery remains best effort; metrics distinguish queue acceptance from delivery failure.

use super::LocalAgentControl;
use crate::TurnStartOptions;
use crate::agent::api::AgentReadiness;
use crate::agent::api::AgentTurnOutcome;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::session_prefix::format_guardian_interruption_message;
use crate::session_prefix::format_inter_agent_completion_message;
use codex_protocol::AgentPath;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::InterAgentCommunication;
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
        if is_quiet_goal_continuation(&outcome.status, &outcome.readiness) {
            return;
        }
        let parent_thread_id = *parent_thread_id;
        let status = outcome.status;
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

        let message = format_parent_outcome_message(
            &parent_agent_path,
            &child_agent_path,
            &status,
            &outcome.readiness,
            outcome.error_info.as_ref(),
        );
        let Some(message) = message else {
            return;
        };
        // `communication` owns the message. Keep a second copy only when the
        // recorder will actually need it after parent delivery succeeds.
        let trace_message = trace.is_enabled().then(|| message.clone());
        let communication = InterAgentCommunication::new(
            child_agent_path.clone(),
            parent_agent_path,
            Vec::new(),
            message,
            /*trigger_turn*/ false,
        );
        let context =
            AgentCommunicationContext::new(AgentCommunicationKind::Result, outcome.thread_id);
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

fn format_parent_outcome_message(
    parent_agent_path: &AgentPath,
    child_agent_path: &AgentPath,
    status: &AgentStatus,
    readiness: &AgentReadiness,
    error_info: Option<&CodexErrorInfo>,
) -> Option<String> {
    match (status, readiness, error_info) {
        (AgentStatus::Completed(_), AgentReadiness::GoalContinuing { .. }, _) => None,
        (AgentStatus::Interrupted, AgentReadiness::ActionRequired, _) => Some(format!(
            "Agent {child_agent_path} was interrupted and requires action before it can continue."
        )),
        (
            AgentStatus::Errored(error),
            _,
            Some(CodexErrorInfo::TooManyDenials),
        ) => Some(format_guardian_interruption_message(
            parent_agent_path.clone(),
            child_agent_path.clone(),
            error,
        )),
        _ => format_inter_agent_completion_message(
            parent_agent_path.clone(),
            child_agent_path.clone(),
            status,
        ),
    }
}

fn is_quiet_goal_continuation(status: &AgentStatus, readiness: &AgentReadiness) -> bool {
    matches!(status, AgentStatus::Completed(_))
        && matches!(readiness, AgentReadiness::GoalContinuing { .. })
}

#[cfg(test)]
mod tests {
    use super::format_parent_outcome_message;
    use super::is_quiet_goal_continuation;
    use crate::agent::api::AgentReadiness;
    use codex_protocol::AgentPath;
    use codex_protocol::protocol::AgentStatus;

    fn paths() -> (AgentPath, AgentPath) {
        (
            AgentPath::try_from("/root").expect("valid parent path"),
            AgentPath::try_from("/root/child").expect("valid child path"),
        )
    }

    #[test]
    fn action_required_interruption_is_handed_back_but_quiet_states_are_not() {
        let (parent, child) = paths();
        let message = format_parent_outcome_message(
            &parent,
            &child,
            &AgentStatus::Interrupted,
            &AgentReadiness::ActionRequired,
            None,
        );
        assert!(message.is_some());

        assert_eq!(
            format_parent_outcome_message(
                &parent,
                &child,
                &AgentStatus::Interrupted,
                &AgentReadiness::Pending,
                None,
            ),
            None
        );
        assert_eq!(
            format_parent_outcome_message(
                &parent,
                &child,
                &AgentStatus::Interrupted,
                &AgentReadiness::GoalContinuing {
                    goal_id: "goal-1".to_owned(),
                    turn_id: "turn-1".to_owned(),
                },
                None,
            ),
            None
        );
        assert_eq!(
            format_parent_outcome_message(
                &parent,
                &child,
                &AgentStatus::Completed(Some("intermediate goal output".to_owned())),
                &AgentReadiness::GoalContinuing {
                    goal_id: "goal-1".to_owned(),
                    turn_id: "turn-2".to_owned(),
                },
                None,
            ),
            None,
            "intermediate goal completion must not become parent mailbox activity"
        );
        assert!(is_quiet_goal_continuation(
            &AgentStatus::Completed(Some("intermediate goal output".to_owned())),
            &AgentReadiness::GoalContinuing {
                goal_id: "goal-1".to_owned(),
                turn_id: "turn-2".to_owned(),
            }
        ));
    }
}
