//! Direct-message diagnostics and delivery ownership across queues and eviction.
//! Only history commitment consumes mail; dropping its last owner releases its attachments.

use codex_agent_mailbox::MessageKind;
use codex_extension_api::ExtensionData;
use codex_extension_api::MessageLifecycleContributor;
use codex_extension_api::MessageLifecycleInput;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::protocol::InterAgentCommunication;
use serde::Deserialize;
use serde::Serialize;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

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
    pub(crate) sender_thread_id: ThreadId,
    pub(crate) native_key: Option<String>,
    pub(crate) message_kind: Option<MessageKind>,
    pub(crate) reply_to: Option<String>,
}

impl AgentCommunicationContext {
    pub(crate) fn new(kind: AgentCommunicationKind, sender_thread_id: ThreadId) -> Self {
        Self {
            kind,
            sender_thread_id,
            native_key: None,
            message_kind: None,
            reply_to: None,
        }
    }

    pub(crate) fn with_native_key(mut self, native_key: Option<String>) -> Self {
        self.native_key = native_key.filter(|key| !key.trim().is_empty());
        self
    }

    pub(crate) fn with_mailbox_message(
        mut self,
        message_kind: Option<MessageKind>,
        reply_to: Option<String>,
    ) -> Self {
        self.message_kind = message_kind;
        self.reply_to = reply_to;
        self
    }

    pub(crate) fn is_native_work(&self) -> bool {
        self.native_key.is_some()
            && matches!(
                self.kind,
                AgentCommunicationKind::Spawn | AgentCommunicationKind::Followup
            )
    }

    pub(crate) fn is_native_message(&self) -> bool {
        self.native_key.is_some()
            && matches!(
                self.kind,
                AgentCommunicationKind::Message | AgentCommunicationKind::Result
            )
    }

    pub(crate) fn is_completion_result(&self) -> bool {
        self.kind == AgentCommunicationKind::Result
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
            content = communication
                .encrypted_content
                .as_deref()
                .unwrap_or("[plaintext]"),
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

/// Pending mail with shared ownership of its captured publication state.
/// Serialization preserves the protocol shape; restored historical mail is not republished.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PendingAgentMessage {
    communication: InterAgentCommunication,
    #[serde(skip)]
    pub(crate) delivery: Option<MessageDelivery>,
}

impl From<InterAgentCommunication> for PendingAgentMessage {
    fn from(communication: InterAgentCommunication) -> Self {
        Self::new(communication, /*delivery*/ None)
    }
}

impl Deref for PendingAgentMessage {
    type Target = InterAgentCommunication;
    fn deref(&self) -> &Self::Target {
        &self.communication
    }
}

impl PartialEq for PendingAgentMessage {
    fn eq(&self, other: &Self) -> bool {
        self.communication == other.communication
    }
}

impl PendingAgentMessage {
    pub(crate) fn new(
        communication: InterAgentCommunication,
        delivery: Option<MessageDelivery>,
    ) -> Self {
        Self {
            communication,
            delivery,
        }
    }

    pub(crate) fn durable_input_id(&self) -> Option<&str> {
        self.delivery
            .as_ref()
            .and_then(MessageDelivery::durable_input_id)
    }

    pub(crate) fn preempts_active_turn(&self) -> bool {
        self.delivery
            .as_ref()
            .is_none_or(MessageDelivery::preempts_active_turn)
    }
}

/// The publication state retained until this delivery is consumed or abandoned.
struct MessagePublication {
    input: MessageLifecycleInput,
    observers: Box<[Arc<dyn MessageLifecycleContributor>]>,
}

/// Clones follow one logical delivery through queues and share its consumption state.
#[derive(Clone)]
pub(crate) struct MessageDelivery {
    publication: Arc<Mutex<Option<MessagePublication>>>,
    durable_input_id: Option<String>,
    durable_message_id: Option<String>,
    preempts_active_turn: bool,
}

impl MessageDelivery {
    pub(crate) fn publish(
        sender: ThreadId,
        message_id: ResponseItemId,
        observers: &[Arc<dyn MessageLifecycleContributor>],
        durable_input_id: Option<String>,
        durable_message_id: Option<String>,
        preempts_active_turn: bool,
    ) -> Self {
        let input = MessageLifecycleInput {
            sender,
            data: ExtensionData::new(message_id.to_string()),
            message_id,
        };
        for observer in observers {
            observer.on_publish(&input);
        }
        let publication = MessagePublication {
            input,
            observers: observers.into(),
        };
        Self {
            publication: Arc::new(Mutex::new(Some(publication))),
            durable_input_id,
            durable_message_id,
            preempts_active_turn,
        }
    }

    /// Restored durable mail is not republished to lifecycle contributors, but still
    /// carries its stable input identity through canonical history settlement.
    pub(crate) fn restored_mailbox_input(
        durable_input_id: String,
        durable_message_id: Option<String>,
    ) -> Self {
        Self::restored_mailbox_message_input(
            durable_input_id,
            durable_message_id,
            /*preempts_active_turn*/ true,
        )
    }

    /// Restored durable messages retain their attention class without republishing lifecycle
    /// state. Work inputs use `restored_mailbox_input`, which remains actionable by default.
    pub(crate) fn restored_mailbox_message_input(
        durable_input_id: String,
        durable_message_id: Option<String>,
        preempts_active_turn: bool,
    ) -> Self {
        Self {
            publication: Arc::new(Mutex::new(None)),
            durable_input_id: Some(durable_input_id),
            durable_message_id,
            preempts_active_turn,
        }
    }

    pub(crate) fn durable_input_id(&self) -> Option<&str> {
        self.durable_input_id.as_deref()
    }

    pub(crate) fn durable_message_id(&self) -> Option<&str> {
        self.durable_message_id.as_deref()
    }

    pub(crate) fn preempts_active_turn(&self) -> bool {
        self.preempts_active_turn
    }

    /// Commit this delivery and release its publication state, even if clones remain.
    pub(crate) fn consumed(&self, recipient: ThreadId) {
        let publication = self
            .publication
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(publication) = publication {
            for observer in &publication.observers {
                observer.on_consumed(&publication.input, recipient);
            }
        }
    }
}

impl fmt::Debug for MessageDelivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageDelivery").finish_non_exhaustive()
    }
}
