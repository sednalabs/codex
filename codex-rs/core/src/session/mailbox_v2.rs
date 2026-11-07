//! Bridges protected native turn/goal identities to durable mailbox custody.

use super::session::Session;
use super::turn_context::TurnContext;
use crate::agent::AgentStatus;
use crate::agent::control::mailbox_v2::storage_error;
use crate::agent::goal_notifications::GoalNotificationStore;
use crate::agent::goal_notifications::GoalNotificationTurnToken;
use codex_agent_mailbox::Continuation;
use codex_agent_mailbox::GoalBinding;
use codex_agent_mailbox::Outcome;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::protocol::AgentCommunicationOrigin;
use codex_protocol::protocol::AgentMessageIntent;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadGoalStatus;

pub(super) fn public_recipient(source: &SessionSource) -> bool {
    match source {
        SessionSource::Internal(_) => false,
        SessionSource::SubAgent(source) => matches!(source, SubAgentSource::ThreadSpawn { .. }),
        _ => true,
    }
}

impl Session {
    /// Returns true only when the exact durable work took ownership of the
    /// terminal handback. Legacy producers remain unchanged otherwise.
    pub(super) async fn settle_mailbox_terminal_turn(
        &self,
        turn: &TurnContext,
        parent: ThreadId,
        child_path: &codex_protocol::AgentPath,
        status: &AgentStatus,
    ) -> Result<bool> {
        let Some(mailbox) = self.services.agent_control.mailbox_v2_open(&turn.config).await? else {
            return Ok(false);
        };
        let token = turn.extension_data.get::<GoalNotificationTurnToken>();
        let store = self.services.thread_extension_data.get::<GoalNotificationStore>();
        let (outcome, continuation) = terminal_continuation(status, &turn.sub_id, token.as_deref(), store.as_deref())?;
        let terminal = matches!(continuation, Continuation::None | Continuation::Terminal(_));
        let Some(parent_path) = child_path.as_str().rsplit_once('/')
            .and_then(|(path, _)| codex_protocol::AgentPath::try_from(path).ok())
        else {
            return Err(CodexErr::InvalidRequest("child has no canonical parent path".to_string()));
        };
        let response = terminal.then(|| crate::session_prefix::format_inter_agent_completion_message(
            parent_path.clone(), child_path.clone(), status,
        )).flatten().filter(|body| body.len() <= 8_192).map(|body| {
            let mut message = InterAgentCommunication::new(
                child_path.clone(), parent_path, Vec::new(), body, /*trigger_turn*/ false,
            );
            message.intent = Some(AgentMessageIntent::ResultReady);
            message.origin = Some(AgentCommunicationOrigin::Result);
            message
        });
        let activate_if_idle = terminal && self.services.agent_control
            .mailbox_v2_outcome_activation_requested(parent).await;
        let receipt = mailbox.record_turn_outcome(
            self.thread_id, &turn.sub_id, outcome, response, continuation,
            activate_if_idle,
        ).await.map_err(storage_error)?;
        if receipt.stale_turn {
            return Ok(true);
        }
        if receipt.no_matching_work {
            return Ok(false);
        }
        self.services.agent_control.mailbox_v2_deliver_outcomes(parent, &receipt.response_message_ids).await?;
        Ok(true)
    }
}

fn terminal_continuation(
    status: &AgentStatus,
    source_turn_id: &str,
    token: Option<&GoalNotificationTurnToken>,
    store: Option<&GoalNotificationStore>,
) -> Result<(Outcome, Continuation)> {
    let outcome = match status {
        AgentStatus::Completed(_) => Outcome::Completed,
        AgentStatus::Errored(_) => Outcome::NeedsAttention,
        AgentStatus::Interrupted | AgentStatus::Shutdown => Outcome::Cancelled,
        _ => return Err(CodexErr::InvalidRequest("mailbox settlement requires a terminal turn".to_string())),
    };
    let snapshot = store.and_then(GoalNotificationStore::snapshot);
    match (token, store, snapshot) {
        (None, _, None) => Ok((outcome, Continuation::None)),
        (Some(token), Some(store), Some(snapshot)) => {
            let binding = GoalBinding {
                goal_id: token.binding.goal_id.clone(),
                parent_thread_id: token.binding.parent_thread_id,
                child_thread_id: token.binding.child_thread_id,
                control_generation: token.binding.control_generation,
                incarnation: Some(token.incarnation),
            };
            if token.incarnation != store.incarnation() || token.binding != snapshot.binding
                || token.generation != token.binding.control_generation
                || token.source_turn_id != source_turn_id
            {
                // record_turn_outcome also selects this turn's admissions. Do
                // not attach those admissions to an old goal token merely to
                // settle linked work from the old binding.
                return Err(CodexErr::Fatal("stale native goal token cannot settle durable work".to_string()));
            }
            if !matches!(status, AgentStatus::Completed(_)) {
                return Ok((outcome, Continuation::Terminal(binding)));
            }
            let continuation = match snapshot.status {
                ThreadGoalStatus::Active => Continuation::PendingGoalBinding(binding),
                ThreadGoalStatus::Paused | ThreadGoalStatus::Blocked | ThreadGoalStatus::UsageLimited => Continuation::Deferred(binding),
                ThreadGoalStatus::BudgetLimited | ThreadGoalStatus::Complete => Continuation::Terminal(binding),
            };
            Ok((outcome, continuation))
        }
        _ => Err(CodexErr::Fatal("native goal custody is unavailable for exact mailbox settlement".to_string())),
    }
}

#[cfg(test)]
#[path = "mailbox_v2_tests.rs"]
mod tests;
