//! Delivers terminal child results and completion activity to the agent tree.
//!
//! Sessions capture terminal state; the controller owns routing and queue-only delivery.
//! Delivery remains best effort, with tracing recorded only after the parent accepts it.

use super::LocalAgentControl;
use crate::TurnStartOptions;
use crate::agent::api::AgentTurnOutcome;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::session_prefix::format_inter_agent_completion_message;
use codex_protocol::AgentPath;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::protocol::AgentStatus;
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
        if !self.claim_terminal_publication(outcome.thread_id, &outcome.turn_id) {
            return;
        }
        let is_quiet_continuation = is_active_goal_continuation(&outcome);
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
                    self.resolve_agent_reference(
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
                            id: format!("subagent-completed-{}", outcome.turn_id),
                            kind: SubAgentActivityKind::Completed,
                            agent_thread_id: outcome.thread_id,
                            agent_path: child_agent_path.clone(),
                            model: None,
                            reasoning_effort: None,
                        },
                    )
                    .await
            {
                debug!(
                    "failed to emit completed activity to initiating thread {initiating_thread_id}: {err}"
                );
            }
        }

        let Some(message) = format_inter_agent_completion_message(
            parent_agent_path.clone(),
            child_agent_path.clone(),
            &status,
        ) else {
            return;
        };
        if is_quiet_continuation {
            return;
        }
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
        if let Err(err) = self
            .send_inter_agent_communication(
                parent_thread_id,
                communication,
                context,
                TurnStartOptions::default(),
            )
            .await
        {
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

fn is_active_goal_continuation(outcome: &AgentTurnOutcome) -> bool {
    matches!(outcome.status, AgentStatus::Completed(_))
        && outcome
            .logical_terminality
            .as_ref()
            .is_some_and(|terminality| {
                !terminality.goal_id.is_empty()
                    && terminality.turn_id == outcome.turn_id
                    && !terminality.turn_id.is_empty()
            })
}

#[cfg(test)]
mod tests {
    use super::is_active_goal_continuation;
    use crate::agent::api::AgentTurnLogicalTerminality;
    use crate::agent::api::AgentTurnOutcome;
    use crate::agent::control::LocalAgentControl;
    use codex_protocol::AgentPath;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::AgentStatus;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::SubAgentSource;

    fn outcome(
        status: AgentStatus,
        terminality: Option<AgentTurnLogicalTerminality>,
    ) -> AgentTurnOutcome {
        AgentTurnOutcome {
            thread_id: ThreadId::default(),
            turn_id: "turn-1".to_string(),
            source: SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: ThreadId::default(),
                depth: 1,
                agent_path: Some(AgentPath::root()),
                agent_nickname: None,
                agent_role: None,
            }),
            parent_turn_id: None,
            initiating_agent_path: None,
            status,
            logical_terminality: terminality,
        }
    }

    #[test]
    fn only_matching_successful_goal_turn_is_quiet() {
        let matching = AgentTurnLogicalTerminality {
            turn_id: "turn-1".to_string(),
            goal_id: "goal-1".to_string(),
        };
        assert!(is_active_goal_continuation(&outcome(
            AgentStatus::Completed(Some("intermediate".to_string())),
            Some(matching.clone()),
        )));
        assert!(!is_active_goal_continuation(&outcome(
            AgentStatus::Completed(Some("final".to_string())),
            None,
        )));
        assert!(!is_active_goal_continuation(&outcome(
            AgentStatus::Completed(Some("stale".to_string())),
            Some(AgentTurnLogicalTerminality {
                turn_id: "old-turn".to_string(),
                ..matching.clone()
            }),
        )));
        assert!(!is_active_goal_continuation(&outcome(
            AgentStatus::Errored("error".to_string()),
            Some(matching),
        )));
    }

    #[test]
    fn duplicate_terminal_publication_is_idempotent_per_latest_turn() {
        let control = LocalAgentControl::default();
        let thread_id = ThreadId::default();
        assert!(control.claim_terminal_publication(thread_id, "turn-1"));
        assert!(!control.claim_terminal_publication(thread_id, "turn-1"));
        assert!(control.claim_terminal_publication(thread_id, "turn-2"));
        assert!(!control.claim_terminal_publication(thread_id, "turn-2"));
    }
}
