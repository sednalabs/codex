//! Converts agent messages into attributed input while preserving their wake mode.

use crate::agent::types::AgentMessage;
use crate::agent::types::MessageDeliveryMode;
use crate::context::ContextualUserFragment;
use crate::context::InterAgentMessage;
use crate::context::InterAgentMessageType;
use codex_protocol::AgentPath;
use codex_protocol::protocol::AgentCommunicationOrigin;
use codex_protocol::protocol::InterAgentCommunication;

impl AgentMessage {
    pub(crate) fn into_communication(
        self,
        author: AgentPath,
        recipient: AgentPath,
        mode: MessageDeliveryMode,
    ) -> InterAgentCommunication {
        let trigger_turn = mode == MessageDeliveryMode::TriggerTurn;
        let mut communication = match self {
            Self::Encrypted(message) => InterAgentCommunication::new_encrypted(
                author,
                recipient,
                Vec::new(),
                message,
                trigger_turn,
            ),
            Self::Plaintext(message) => {
                let message_type = match mode {
                    MessageDeliveryMode::QueueOnly | MessageDeliveryMode::WakeWait => {
                        InterAgentMessageType::Message
                    }
                    MessageDeliveryMode::TriggerTurn => InterAgentMessageType::NewTask,
                };
                let content = InterAgentMessage::new(
                    message_type,
                    recipient.clone(),
                    author.clone(),
                    message,
                )
                .render();
                InterAgentCommunication::new(author, recipient, Vec::new(), content, trigger_turn)
            }
        };
        communication.origin = Some(match mode {
            MessageDeliveryMode::QueueOnly => AgentCommunicationOrigin::Informational,
            MessageDeliveryMode::WakeWait | MessageDeliveryMode::TriggerTurn => {
                AgentCommunicationOrigin::Actionable
            }
        });
        communication
    }
}

#[cfg(test)]
#[path = "delivery_tests.rs"]
mod tests;
