use crate::agent::api::AgentWaitStatus;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;

/// Derive the next agent status from a single emitted event.
/// Returns `None` when the event does not affect status tracking.
pub(crate) fn agent_status_from_event(msg: &EventMsg) -> Option<AgentStatus> {
    match msg {
        EventMsg::TurnStarted(_) => Some(AgentStatus::Running),
        EventMsg::TurnComplete(ev) => Some(match &ev.error {
            Some(error) => AgentStatus::Errored(error.message.clone()),
            None => AgentStatus::Completed(ev.last_agent_message.clone()),
        }),
        EventMsg::TurnAborted(ev) => match ev.reason {
            codex_protocol::protocol::TurnAbortReason::Interrupted
            | codex_protocol::protocol::TurnAbortReason::BudgetLimited => {
                Some(AgentStatus::Interrupted)
            }
            _ => Some(AgentStatus::Errored(format!("{:?}", ev.reason))),
        },
        EventMsg::Error(ev) => Some(AgentStatus::Errored(ev.message.clone())),
        EventMsg::ShutdownComplete => Some(AgentStatus::Shutdown),
        _ => None,
    }
}

pub(crate) fn is_final(status: &AgentStatus) -> bool {
    !matches!(
        status,
        AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::Interrupted
    )
}

/// Whether a status update may complete a native wait.
///
/// Only a successful terminal status paired with a matching producer snapshot
/// for the same turn remains logically active. Missing or mismatched snapshots
/// preserve the ordinary fail-open status behavior.
pub(crate) fn is_final_for_wait(status: &AgentWaitStatus) -> bool {
    is_final(&status.status)
        && !status
            .logical_terminality
            .as_ref()
            .is_some_and(|terminality| {
                !terminality.turn_id.is_empty()
                    && !terminality.goal_id.is_empty()
                    && status.turn_id.as_deref() == Some(terminality.turn_id.as_str())
            })
}

#[cfg(test)]
mod tests {
    use super::is_final_for_wait;
    use crate::agent::api::AgentTurnLogicalTerminality;
    use crate::agent::api::AgentWaitStatus;
    use codex_protocol::protocol::AgentStatus;

    fn completed_wait_status(
        turn_id: Option<&str>,
        logical_turn_id: Option<&str>,
    ) -> AgentWaitStatus {
        AgentWaitStatus {
            status: AgentStatus::Completed("turn result".to_string()),
            turn_id: turn_id.map(str::to_owned),
            logical_terminality: logical_turn_id.map(|logical_turn_id| {
                AgentTurnLogicalTerminality {
                    turn_id: logical_turn_id.to_string(),
                    goal_id: "goal-generation-1".to_string(),
                }
            }),
        }
    }

    #[test]
    fn only_exact_turn_bound_active_goal_remains_nonterminal() {
        assert!(!is_final_for_wait(&completed_wait_status(
            Some("turn-1"),
            Some("turn-1")
        )));
        assert!(is_final_for_wait(&completed_wait_status(
            Some("turn-2"),
            Some("turn-1")
        )));
        assert!(is_final_for_wait(&completed_wait_status(
            Some("turn-1"),
            None
        )));
        assert!(is_final_for_wait(&completed_wait_status(
            None,
            Some("turn-1")
        )));
        assert!(is_final_for_wait(&AgentWaitStatus {
            status: AgentStatus::Completed("turn result".to_string()),
            turn_id: Some(String::new()),
            logical_terminality: Some(AgentTurnLogicalTerminality {
                turn_id: String::new(),
                goal_id: "goal-generation-1".to_string(),
            }),
        }));
        assert!(!is_final_for_wait(&AgentWaitStatus {
            status: AgentStatus::Running,
            turn_id: Some("turn-1".to_string()),
            logical_terminality: Some(AgentTurnLogicalTerminality {
                turn_id: "turn-1".to_string(),
                goal_id: "goal-generation-1".to_string(),
            }),
        }));
    }
}
