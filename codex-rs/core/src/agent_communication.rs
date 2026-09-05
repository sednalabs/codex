use codex_protocol::ThreadId;
use codex_protocol::protocol::InterAgentCommunication;

const AGENT_COMMUNICATION_TARGET: &str = "codex_otel.agent_communication";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentCommunicationKind {
    Spawn,
    Message,
    Followup,
    Result,
}

impl AgentCommunicationKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Message => "message",
            Self::Followup => "followup",
            Self::Result => "result",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentCommunicationContext {
    kind: AgentCommunicationKind,
    sender_thread_id: ThreadId,
}

impl AgentCommunicationContext {
    pub(crate) fn new(kind: AgentCommunicationKind, sender_thread_id: ThreadId) -> Self {
        Self {
            kind,
            sender_thread_id,
        }
    }
}

pub(crate) fn logging_enabled() -> bool {
    tracing::enabled!(target: AGENT_COMMUNICATION_TARGET, tracing::Level::INFO)
}

fn telemetry_content(communication: &InterAgentCommunication) -> &str {
    if communication.content.is_empty() {
        // Encrypted payloads are opaque and must never be copied into telemetry.
        if communication.encrypted_content.is_some() {
            "[encrypted]"
        } else {
            ""
        }
    } else {
        communication.content.as_str()
    }
}

pub(crate) fn emit_agent_communication_send(
    communication_id: &str,
    context: &AgentCommunicationContext,
    communication: &InterAgentCommunication,
    receiver_thread_id: ThreadId,
) {
    tracing::info!(
        target: AGENT_COMMUNICATION_TARGET,
        {
            event.name = "codex.agent_communication",
            communication_id,
            kind = context.kind.as_str(),
            state = "send",
            sender_thread_id = %context.sender_thread_id,
            receiver_thread_id = %receiver_thread_id,
            content = telemetry_content(communication),
        },
        "agent communication"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::AgentPath;

    #[test]
    fn telemetry_redacts_encrypted_content() {
        let ciphertext = "gAAAA-secret-ciphertext";
        let communication = InterAgentCommunication::new_encrypted(
            AgentPath::root(),
            AgentPath::root(),
            Vec::new(),
            ciphertext.to_string(),
            false,
        );

        assert_eq!(telemetry_content(&communication), "[encrypted]");
        assert!(!telemetry_content(&communication).contains(ciphertext));
    }
}

pub(crate) fn emit_agent_communication_receive(communication_id: &str) {
    tracing::info!(
        target: AGENT_COMMUNICATION_TARGET,
        {
            event.name = "codex.agent_communication",
            communication_id,
            state = "receive",
        },
        "agent communication"
    );
}
