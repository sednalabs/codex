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
    pub(crate) sender_thread_id: ThreadId,
    pub(crate) native_key: Option<String>,
    pub(crate) message_kind: Option<codex_agent_mailbox::MessageKind>,
    pub(crate) reply_to: Option<String>,
    /// An outcome already accepted atomically by the mailbox store.
    pub(crate) durable_message_id: Option<String>,
}

impl AgentCommunicationContext {
    pub(crate) fn new(kind: AgentCommunicationKind, sender_thread_id: ThreadId) -> Self {
        Self {
            kind,
            sender_thread_id,
            native_key: None,
            message_kind: None,
            reply_to: None,
            durable_message_id: None,
        }
    }

    pub(crate) fn with_native_key(mut self, key: Option<String>) -> Self {
        self.native_key = key.filter(|key| !key.trim().is_empty());
        self
    }

    pub(crate) fn with_mailbox_message(
        mut self,
        kind: Option<codex_agent_mailbox::MessageKind>,
        reply_to: Option<String>,
    ) -> Self {
        self.message_kind = kind;
        self.reply_to = reply_to;
        self
    }

    pub(crate) fn is_native_work(&self) -> bool {
        self.native_key.is_some()
            && matches!(self.kind, AgentCommunicationKind::Spawn | AgentCommunicationKind::Followup)
    }

    pub(crate) fn with_durable_message_id(mut self, id: String) -> Self {
        self.durable_message_id = Some(id);
        self
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
            /*trigger_turn*/ false,
        );

        assert_eq!(telemetry_content(&communication), "[encrypted]");
        assert!(!telemetry_content(&communication).contains(ciphertext));
    }
}
