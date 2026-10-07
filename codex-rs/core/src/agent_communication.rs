use codex_protocol::ThreadId;
use codex_protocol::protocol::InterAgentCommunication;

pub(crate) static PENDING_MAILBOX_MESSAGES: codex_diagnostics::Gauge =
    codex_diagnostics::Gauge::new("core.mailbox.pending");

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
            content_kind = if communication.encrypted_content.is_some() {
                "encrypted"
            } else {
                "plaintext"
            },
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
    use tracing_test::internal::MockWriter;

    #[test]
    fn communication_telemetry_records_kind_without_payload() {
        let buffer: &'static std::sync::Mutex<Vec<u8>> =
            Box::leak(Box::new(std::sync::Mutex::new(Vec::new())));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(MockWriter::new(buffer))
            .finish();
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        let sender_thread_id = ThreadId::new();
        let receiver_thread_id = ThreadId::new();
        let context =
            AgentCommunicationContext::new(AgentCommunicationKind::Message, sender_thread_id);
        let plaintext = InterAgentCommunication::new(
            AgentPath::root().join("worker").expect("valid path"),
            AgentPath::root(),
            Vec::new(),
            "plaintext-payload-sentinel".to_string(),
            /*trigger_turn*/ false,
        );
        let encrypted = InterAgentCommunication::new_encrypted(
            AgentPath::root().join("worker").expect("valid path"),
            AgentPath::root(),
            Vec::new(),
            "ciphertext-payload-sentinel".to_string(),
            /*trigger_turn*/ false,
        );

        emit_agent_communication_send("plaintext-id", &context, &plaintext, receiver_thread_id);
        emit_agent_communication_send("encrypted-id", &context, &encrypted, receiver_thread_id);

        let logs = String::from_utf8(
            buffer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        )
        .expect("telemetry should be valid utf-8");
        assert!(!logs.contains("plaintext-payload-sentinel"));
        assert!(!logs.contains("ciphertext-payload-sentinel"));
        assert!(!logs.contains("content="));
        assert!(logs.contains("content_kind=\"plaintext\""));
        assert!(logs.contains("content_kind=\"encrypted\""));
    }
}
