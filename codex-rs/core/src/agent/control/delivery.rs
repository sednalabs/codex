//! Converts agent messages into attributed input while preserving their wake mode.

use super::LocalAgentControl;
use super::is_final;
use crate::agent::api::ExplicitInterAgentUpdate;
use crate::agent::types::AgentMessage;
use crate::agent::types::MessageDeliveryMode;
use crate::agent_communication::AgentCommunicationContext;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use crate::context::ContextualUserFragment;
use crate::context::InterAgentMessage;
use crate::context::InterAgentMessageType;
use codex_history::InterAgentCommunicationAdmission;
use codex_history::InterAgentCommunicationIntent;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_thread_store::StoredThreadHistory;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

impl AgentMessage {
    pub(crate) fn into_communication(
        self,
        author: AgentPath,
        recipient: AgentPath,
        mode: MessageDeliveryMode,
    ) -> InterAgentCommunication {
        let trigger_turn = mode == MessageDeliveryMode::TriggerTurn;
        match self {
            Self::Encrypted(message) => InterAgentCommunication::new_encrypted(
                author,
                recipient,
                Vec::new(),
                message,
                trigger_turn,
            ),
            Self::Plaintext(message) => {
                let message_type = match mode {
                    MessageDeliveryMode::QueueOnly => InterAgentMessageType::Message,
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
        }
    }
}

type ExplicitAdmissionScopeKey = (ThreadId, ThreadId, ThreadId, String);

#[derive(Default)]
struct ExplicitAdmissionIndex {
    ordered: Vec<InterAgentCommunicationAdmission>,
    by_update_id: HashMap<ResponseItemId, InterAgentCommunicationAdmission>,
    by_scope_key: HashMap<ExplicitAdmissionScopeKey, ResponseItemId>,
    represented_update_ids: HashSet<ResponseItemId>,
}

impl ExplicitAdmissionIndex {
    fn from_history(
        history: &StoredThreadHistory,
        root_thread_id: ThreadId,
        recipient_thread_id: ThreadId,
    ) -> CodexResult<Self> {
        let mut index = Self::default();
        let mut represented_response_ids = HashMap::<ResponseItemId, ResponseItemId>::new();

        for item in &history.items {
            match item {
                RolloutItem::InterAgentCommunicationMetadata {
                    trigger_turn,
                    admission: Some(admission),
                } => {
                    if *trigger_turn
                        || admission.original_communication.trigger_turn
                        || !admission.original_communication.other_recipients.is_empty()
                        || admission.original_communication.id.as_ref()
                            != Some(&admission.update_id)
                        || admission.original_communication.author != admission.sender_path
                        || admission.original_communication.recipient != admission.recipient_path
                        || !admission.recipient_path.is_root()
                        || admission.root_thread_id != root_thread_id
                        || admission.recipient_thread_id != recipient_thread_id
                        || admission.idempotency_key.trim().is_empty()
                        || admission.idempotency_key.trim() != admission.idempotency_key.as_str()
                    {
                        return Err(CodexErr::InvalidRequest(
                            "stored explicit inter-agent admission has invalid identity or scope"
                                .to_string(),
                        ));
                    }

                    let mut superseded_ids = HashSet::new();
                    for superseded_id in &admission.supersedes {
                        if !superseded_ids.insert(superseded_id) {
                            return Err(CodexErr::InvalidRequest(
                                "stored explicit inter-agent admission has duplicate supersession IDs"
                                    .to_string(),
                            ));
                        }
                        let Some(previous) = index.by_update_id.get(superseded_id) else {
                            return Err(CodexErr::InvalidRequest(
                                "stored explicit inter-agent admission supersedes a missing or later update"
                                    .to_string(),
                            ));
                        };
                        if previous.intent != InterAgentCommunicationIntent::Progress
                            || previous.root_thread_id != admission.root_thread_id
                            || previous.sender_thread_id != admission.sender_thread_id
                            || previous.recipient_thread_id != admission.recipient_thread_id
                        {
                            return Err(CodexErr::InvalidRequest(
                                "stored explicit inter-agent admission supersedes an update outside its progress scope"
                                    .to_string(),
                            ));
                        }
                    }

                    let scope_key = explicit_scope_key(
                        &admission.root_thread_id,
                        &admission.sender_thread_id,
                        &admission.recipient_thread_id,
                        &admission.idempotency_key,
                    );
                    if index
                        .by_update_id
                        .insert(admission.update_id.clone(), admission.clone())
                        .is_some()
                    {
                        return Err(CodexErr::InvalidRequest(
                            "stored explicit inter-agent admission IDs are duplicated".to_string(),
                        ));
                    }
                    if index
                        .by_scope_key
                        .insert(scope_key, admission.update_id.clone())
                        .is_some()
                    {
                        return Err(CodexErr::InvalidRequest(
                            "stored explicit inter-agent idempotency keys are duplicated in scope"
                                .to_string(),
                        ));
                    }
                    if admission.represented_response_id.is_some() {
                        index
                            .represented_update_ids
                            .insert(admission.update_id.clone());
                    }
                    index.ordered.push(admission.clone());
                }
                RolloutItem::ResponseItem(response) => {
                    record_explicit_representation(response, &mut represented_response_ids)?;
                }
                RolloutItem::Compacted(compacted) => {
                    for response in compacted.replacement_history.iter().flatten() {
                        record_explicit_representation(response, &mut represented_response_ids)?;
                    }
                }
                _ => {}
            }
        }

        for (update_id, response_id) in represented_response_ids {
            let admission = index.by_update_id.get(&update_id).ok_or_else(|| {
                CodexErr::InvalidRequest(
                    "stored inter-agent representation has no admission".to_string(),
                )
            })?;
            if admission
                .represented_response_id
                .as_ref()
                .is_some_and(|recorded| recorded != &response_id)
            {
                return Err(CodexErr::InvalidRequest(
                    "stored inter-agent representation conflicts with its admission".to_string(),
                ));
            }
            index.represented_update_ids.insert(update_id);
        }

        Ok(index)
    }
}

fn record_explicit_representation(
    response: &ResponseItemEnvelope,
    represented_response_ids: &mut HashMap<ResponseItemId, ResponseItemId>,
) -> CodexResult<()> {
    let Some(update_id) = response
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.inter_agent_update_id.as_ref())
    else {
        return Ok(());
    };
    if !matches!(&response.item, ResponseItem::AgentMessage { .. }) {
        return Err(CodexErr::InvalidRequest(
            "inter-agent update identity is attached to a non-agent response".to_string(),
        ));
    }
    let response_id = response.item.id().cloned().ok_or_else(|| {
        CodexErr::InvalidRequest(
            "inter-agent representation is missing its response ID".to_string(),
        )
    })?;
    if represented_response_ids
        .insert(update_id.clone(), response_id.clone())
        .is_some_and(|previous| previous != response_id)
    {
        return Err(CodexErr::InvalidRequest(
            "inter-agent update has conflicting response identities".to_string(),
        ));
    }
    Ok(())
}

fn explicit_scope_key(
    root_thread_id: &ThreadId,
    sender_thread_id: &ThreadId,
    recipient_thread_id: &ThreadId,
    idempotency_key: &str,
) -> ExplicitAdmissionScopeKey {
    (
        root_thread_id.clone(),
        sender_thread_id.clone(),
        recipient_thread_id.clone(),
        idempotency_key.to_string(),
    )
}

fn same_explicit_request(
    admission: &InterAgentCommunicationAdmission,
    communication: &InterAgentCommunication,
    update: &ExplicitInterAgentUpdate,
    root_thread_id: &ThreadId,
    sender_thread_id: &ThreadId,
    recipient_thread_id: &ThreadId,
    sender_path: &AgentPath,
    recipient_path: &AgentPath,
) -> bool {
    let mut original_communication = admission.original_communication.clone();
    original_communication.id = None;
    let mut requested_communication = communication.clone();
    requested_communication.id = None;
    admission.intent == update.intent
        && admission.idempotency_key == update.idempotency_key
        && admission.supersedes == update.supersedes
        && admission.root_thread_id == *root_thread_id
        && admission.sender_thread_id == *sender_thread_id
        && admission.recipient_thread_id == *recipient_thread_id
        && admission.sender_path == *sender_path
        && admission.recipient_path == *recipient_path
        && original_communication == requested_communication
}

impl LocalAgentControl {
    /// Durably admits a root-directed update before making it visible on the quiet mailbox path.
    pub(super) async fn send_explicit_inter_agent_update(
        &self,
        caller: ThreadId,
        recipient: ThreadId,
        resume_config: Config,
        message: AgentMessage,
        update: ExplicitInterAgentUpdate,
        context: AgentCommunicationContext,
    ) -> CodexResult<String> {
        if update.idempotency_key.trim().is_empty()
            || update.idempotency_key.trim() != update.idempotency_key
        {
            return Err(CodexErr::InvalidRequest(
                "explicit inter-agent idempotency_key must be non-empty and canonical".to_string(),
            ));
        }

        let root_path = AgentPath::root();
        let root_thread_id = self
            .runtime
            .registry
            .agent_id_for_path(&root_path)
            .ok_or_else(|| CodexErr::ThreadNotFound(recipient))?;
        if recipient != root_thread_id {
            return Err(CodexErr::UnsupportedOperation(
                "explicit inter-agent updates must target the registered root agent".to_string(),
            ));
        }

        let sender_metadata = self.runtime.ensure_agent_known(caller)?;
        let sender_path = sender_metadata.agent_path.ok_or_else(|| {
            CodexErr::InvalidRequest("explicit update sender has no registered agent path".into())
        })?;
        if self.runtime.registry.agent_id_for_path(&sender_path) != Some(caller) {
            return Err(CodexErr::InvalidRequest(
                "explicit update sender identity does not match its registered path".to_string(),
            ));
        }
        let recipient_metadata = self.runtime.ensure_agent_known(recipient)?;
        if recipient_metadata.agent_path.as_ref() != Some(&root_path) {
            return Err(CodexErr::InvalidRequest(
                "explicit update recipient is not the registered root agent".to_string(),
            ));
        }

        let state = self.runtime.upgrade()?;
        let sender_thread = state.get_thread(caller).await?;
        if !sender_thread.is_running()
            || sender_thread.multi_agent_version()
                != Some(codex_protocol::protocol::MultiAgentVersion::V2)
            || !Arc::ptr_eq(
                &sender_thread.session.services.local_agent_runtime.registry,
                &self.runtime.registry,
            )
            || is_final(&sender_thread.agent_status().await)
        {
            return Err(CodexErr::InvalidRequest(
                "explicit update sender is not a live member of this V2 agent tree".to_string(),
            ));
        }

        let _residency_pin = self
            .ensure_v2_agent_loaded(resume_config, recipient, /*parent*/ None)
            .await?;
        let recipient_thread = state.get_thread(recipient).await?;
        if !recipient_thread.is_running()
            || recipient_thread.multi_agent_version()
                != Some(codex_protocol::protocol::MultiAgentVersion::V2)
            || !Arc::ptr_eq(
                &recipient_thread
                    .session
                    .services
                    .local_agent_runtime
                    .registry,
                &self.runtime.registry,
            )
            || is_final(&recipient_thread.agent_status().await)
        {
            return Err(CodexErr::InvalidRequest(
                "explicit update root recipient is not a live member of this V2 agent tree"
                    .to_string(),
            ));
        }

        let mut communication = message.into_communication(
            sender_path.clone(),
            root_path.clone(),
            MessageDeliveryMode::QueueOnly,
        );
        if communication.trigger_turn || !communication.other_recipients.is_empty() {
            return Err(CodexErr::InvalidRequest(
                "explicit inter-agent updates must be single-target and quiet".to_string(),
            ));
        }

        let _admission_guard = recipient_thread
            .session
            .explicit_inter_agent_admission
            .lock()
            .await;
        let history = recipient_thread
            .load_history(/*include_archived*/ false)
            .await
            .map_err(|_| {
                CodexErr::Fatal("failed to read explicit inter-agent admission history".to_string())
            })?;
        let mut index = ExplicitAdmissionIndex::from_history(&history, root_thread_id, recipient)?;
        let scope_key = explicit_scope_key(
            &root_thread_id,
            &caller,
            &recipient,
            &update.idempotency_key,
        );
        let (update_id, is_new) = if let Some(existing_id) = index.by_scope_key.get(&scope_key) {
            let existing = index.by_update_id.get(existing_id).ok_or_else(|| {
                CodexErr::InvalidRequest(
                    "stored explicit inter-agent idempotency index is inconsistent".to_string(),
                )
            })?;
            if !same_explicit_request(
                existing,
                &communication,
                &update,
                &root_thread_id,
                &caller,
                &recipient,
                &sender_path,
                &root_path,
            ) {
                return Err(CodexErr::InvalidRequest(
                    "explicit inter-agent idempotency_key was reused for a changed request"
                        .to_string(),
                ));
            }
            (existing_id.clone(), false)
        } else {
            validate_explicit_supersession(&index, &update, &root_thread_id, &caller, &recipient)?;
            let update_id = ResponseItemId::new("inter_agent_update");
            communication.id = Some(update_id.clone());
            let admission = InterAgentCommunicationAdmission {
                update_id: update_id.clone(),
                idempotency_key: update.idempotency_key.clone(),
                intent: update.intent,
                original_communication: communication.clone(),
                root_thread_id,
                sender_thread_id: caller,
                recipient_thread_id: recipient,
                sender_path,
                recipient_path: root_path,
                supersedes: update.supersedes.clone(),
                represented_response_id: None,
            };
            let rollout_item = RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn: false,
                admission: Some(admission.clone()),
            };
            recipient_thread
                .append_rollout_items(&[rollout_item])
                .await
                .map_err(|_| {
                    CodexErr::Fatal("failed to append explicit inter-agent admission".to_string())
                })?;
            recipient_thread
                .session
                .flush_rollout()
                .await
                .map_err(|_| {
                    CodexErr::Fatal("failed to flush explicit inter-agent admission".to_string())
                })?;
            index.by_scope_key.insert(scope_key, update_id.clone());
            index
                .by_update_id
                .insert(update_id.clone(), admission.clone());
            index.ordered.push(admission);
            (update_id, true)
        };

        if is_new {
            crate::agent_communication::emit_agent_communication_send(
                update_id.as_str(),
                &context,
                &communication,
                recipient,
            );
        }
        enqueue_pending_explicit_admissions(&recipient_thread, &index).await;
        Ok(update_id.to_string())
    }
}

fn validate_explicit_supersession(
    index: &ExplicitAdmissionIndex,
    update: &ExplicitInterAgentUpdate,
    root_thread_id: &ThreadId,
    sender_thread_id: &ThreadId,
    recipient_thread_id: &ThreadId,
) -> CodexResult<()> {
    let mut seen = HashSet::new();
    for update_id in &update.supersedes {
        if !seen.insert(update_id) {
            return Err(CodexErr::InvalidRequest(
                "explicit inter-agent supersedes contains a duplicate update ID".to_string(),
            ));
        }
        let Some(previous) = index.by_update_id.get(update_id) else {
            return Err(CodexErr::InvalidRequest(
                "explicit inter-agent supersedes must name a stored progress update".to_string(),
            ));
        };
        if previous.intent != InterAgentCommunicationIntent::Progress
            || previous.root_thread_id != *root_thread_id
            || previous.sender_thread_id != *sender_thread_id
            || previous.recipient_thread_id != *recipient_thread_id
        {
            return Err(CodexErr::InvalidRequest(
                "explicit inter-agent supersedes must name same-scope progress updates".to_string(),
            ));
        }
    }
    Ok(())
}

async fn enqueue_pending_explicit_admissions(
    recipient_thread: &Arc<CodexThread>,
    index: &ExplicitAdmissionIndex,
) {
    let pending = {
        let mut delivered = recipient_thread
            .session
            .delivered_explicit_inter_agent_updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        delivered.retain(|update_id| !index.represented_update_ids.contains(update_id));
        index
            .ordered
            .iter()
            .filter(|admission| {
                !index.represented_update_ids.contains(&admission.update_id)
                    && !delivered.contains(&admission.update_id)
            })
            .cloned()
            .collect::<Vec<_>>()
    };

    for admission in pending {
        recipient_thread
            .session
            .input_queue
            .enqueue_mailbox_communication(
                admission.original_communication.clone(),
                Default::default(),
            )
            .await;
        recipient_thread
            .session
            .delivered_explicit_inter_agent_updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(admission.update_id.clone());
        crate::agent_communication::emit_agent_communication_receive(admission.update_id.as_str());
    }
}
