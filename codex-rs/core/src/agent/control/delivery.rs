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
use crate::session::ExplicitMailboxActivity;
use crate::session::session::Session;
use codex_history::InterAgentCommunicationAdmission;
use codex_history::InterAgentCommunicationIntent;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::InterAgentCommunication;
use codex_rollout::RolloutRecorder;
use codex_thread_store::PersistContext;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
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

#[derive(Clone, Copy)]
pub(super) struct ExplicitUpdateScope<'a> {
    pub(super) root_thread_id: &'a ThreadId,
    pub(super) sender_thread_id: &'a ThreadId,
    pub(super) recipient_thread_id: &'a ThreadId,
    pub(super) sender_path: &'a AgentPath,
    pub(super) recipient_path: &'a AgentPath,
}

#[derive(Default)]
struct ExplicitAdmissionIndex {
    ordered: Vec<InterAgentCommunicationAdmission>,
    by_update_id: HashMap<ResponseItemId, InterAgentCommunicationAdmission>,
    by_scope_key: HashMap<ExplicitAdmissionScopeKey, ResponseItemId>,
    represented_response_ids: HashMap<ResponseItemId, ResponseItemId>,
}

impl ExplicitAdmissionIndex {
    fn from_rollout_items(
        items: &[RolloutItem],
        root_thread_id: ThreadId,
        recipient_thread_id: ThreadId,
        recipient_path: &AgentPath,
    ) -> CodexResult<Self> {
        let mut index = Self::default();
        let mut represented_response_ids = HashMap::<ResponseItemId, ResponseItemId>::new();

        for item in items {
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
                        || admission.sender_thread_id == admission.recipient_thread_id
                        || (admission.sender_thread_id == root_thread_id)
                            != admission.sender_path.is_root()
                        || (admission.recipient_thread_id == root_thread_id)
                            != admission.recipient_path.is_root()
                        || admission.root_thread_id != root_thread_id
                        || admission.recipient_thread_id != recipient_thread_id
                        || &admission.recipient_path != recipient_path
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
            validate_admission_response_id(admission)?;
            if admission
                .represented_response_id
                .as_ref()
                .is_some_and(|recorded| recorded != &response_id)
            {
                return Err(CodexErr::InvalidRequest(
                    "stored inter-agent representation conflicts with its admission".to_string(),
                ));
            }
            index
                .represented_response_ids
                .insert(update_id, response_id);
        }

        for item in items {
            match item {
                RolloutItem::ResponseItem(response) => {
                    validate_explicit_representation(response, &index.by_update_id)?;
                }
                RolloutItem::Compacted(compacted) => {
                    for response in compacted.replacement_history.iter().flatten() {
                        validate_explicit_representation(response, &index.by_update_id)?;
                    }
                }
                _ => {}
            }
        }

        for admission in &index.ordered {
            validate_admission_response_id(admission)?;
            if let Some(response_id) = &admission.represented_response_id {
                if index
                    .represented_response_ids
                    .get(&admission.update_id)
                    .is_some_and(|recorded| recorded != response_id)
                {
                    return Err(CodexErr::InvalidRequest(
                        "stored inter-agent representation conflicts with its admission"
                            .to_string(),
                    ));
                }
                index
                    .represented_response_ids
                    .entry(admission.update_id.clone())
                    .or_insert_with(|| response_id.clone());
            }
        }

        Ok(index)
    }
}

async fn load_explicit_admission_index(
    rollout_path: &Path,
    root_thread_id: ThreadId,
    recipient_thread_id: ThreadId,
    recipient_path: &AgentPath,
) -> CodexResult<(ExplicitAdmissionIndex, Vec<RolloutItem>)> {
    let (items, stored_thread_id, parse_errors) = RolloutRecorder::load_rollout_items(rollout_path)
        .await
        .map_err(|_| CodexErr::Fatal("failed to read explicit inter-agent rollout".into()))?;
    if stored_thread_id != Some(recipient_thread_id) || parse_errors != 0 {
        return Err(CodexErr::Fatal(
            "explicit inter-agent rollout identity or contents could not be verified".into(),
        ));
    }
    let index = ExplicitAdmissionIndex::from_rollout_items(
        &items,
        root_thread_id,
        recipient_thread_id,
        recipient_path,
    )?;
    Ok((index, items))
}

pub(crate) async fn explicit_admission_for_communication(
    rollout_path: &Path,
    root_thread_id: ThreadId,
    recipient_thread_id: ThreadId,
    recipient_path: &AgentPath,
    communication: &InterAgentCommunication,
) -> CodexResult<
    Option<(
        InterAgentCommunicationAdmission,
        Option<ResponseItemId>,
        Vec<RolloutItem>,
        bool,
    )>,
> {
    let Some(update_id) = communication.id.as_ref() else {
        return Ok(None);
    };
    let (index, rollout_items) = load_explicit_admission_index(
        rollout_path,
        root_thread_id,
        recipient_thread_id,
        recipient_path,
    )
    .await?;
    let Some(admission) = index.by_update_id.get(update_id) else {
        return Ok(None);
    };
    if admission.root_thread_id != root_thread_id
        || admission.recipient_thread_id != recipient_thread_id
        || &admission.recipient_path != recipient_path
        || admission.original_communication != *communication
    {
        return Ok(None);
    }
    let superseded = index
        .ordered
        .iter()
        .any(|candidate| candidate.supersedes.contains(update_id));
    Ok(Some((
        admission.clone(),
        index.represented_response_ids.get(update_id).cloned(),
        rollout_items,
        superseded,
    )))
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

fn validate_explicit_representation(
    response: &ResponseItemEnvelope,
    admissions: &HashMap<ResponseItemId, InterAgentCommunicationAdmission>,
) -> CodexResult<()> {
    let Some(update_id) = response
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.inter_agent_update_id.as_ref())
    else {
        return Ok(());
    };
    let admission = admissions.get(update_id).ok_or_else(|| {
        CodexErr::InvalidRequest("stored inter-agent representation has no admission".to_string())
    })?;
    if !explicit_response_matches_communication(&response.item, &admission.original_communication) {
        return Err(CodexErr::InvalidRequest(
            "stored inter-agent representation conflicts with its admitted message".to_string(),
        ));
    }
    Ok(())
}

fn explicit_response_matches_communication(
    response: &ResponseItem,
    communication: &InterAgentCommunication,
) -> bool {
    let expected = communication.to_model_input_item();
    let (
        ResponseItem::AgentMessage {
            id: expected_id,
            author: expected_author,
            recipient: expected_recipient,
            content: expected_content,
            ..
        },
        ResponseItem::AgentMessage {
            id,
            author,
            recipient,
            content,
            ..
        },
    ) = (&expected, response)
    else {
        return false;
    };
    expected_id.as_ref() == id.as_ref()
        && expected_author == author
        && expected_recipient == recipient
        && expected_content == content
}

fn validate_admission_response_id(admission: &InterAgentCommunicationAdmission) -> CodexResult<()> {
    let expected_response_id = admission
        .original_communication
        .to_model_input_item()
        .id()
        .cloned();
    if expected_response_id.as_ref() != Some(&admission.update_id)
        || admission
            .represented_response_id
            .as_ref()
            .is_some_and(|response_id| Some(response_id) != expected_response_id.as_ref())
    {
        return Err(CodexErr::InvalidRequest(
            "stored inter-agent receipt conflicts with its admitted message identity".to_string(),
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
        *root_thread_id,
        *sender_thread_id,
        *recipient_thread_id,
        idempotency_key.to_string(),
    )
}

fn same_explicit_request(
    admission: &InterAgentCommunicationAdmission,
    communication: &InterAgentCommunication,
    update: &ExplicitInterAgentUpdate,
    scope: &ExplicitUpdateScope<'_>,
) -> bool {
    let mut original_communication = admission.original_communication.clone();
    original_communication.id = None;
    let mut requested_communication = communication.clone();
    requested_communication.id = None;
    admission.intent == update.intent
        && admission.idempotency_key == update.idempotency_key
        && admission.supersedes == update.supersedes
        && admission.root_thread_id == *scope.root_thread_id
        && admission.sender_thread_id == *scope.sender_thread_id
        && admission.recipient_thread_id == *scope.recipient_thread_id
        && admission.sender_path == *scope.sender_path
        && admission.recipient_path == *scope.recipient_path
        && original_communication == requested_communication
}

impl LocalAgentControl {
    /// Durably admits a directly addressed update before making it visible on the quiet mailbox path.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "The recipient admission mutex serializes membership, durable ledger, and quiet enqueue operations."
    )]
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
        if caller == recipient {
            return Err(CodexErr::InvalidRequest(
                "explicit inter-agent updates cannot be sent to the sender itself".to_string(),
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
        let recipient_path = recipient_metadata.agent_path.ok_or_else(|| {
            CodexErr::InvalidRequest(
                "explicit update recipient has no registered agent path".into(),
            )
        })?;
        if self.runtime.registry.agent_id_for_path(&recipient_path) != Some(recipient) {
            return Err(CodexErr::InvalidRequest(
                "explicit update recipient identity does not match its registered path".to_string(),
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

        // Only a root recipient may use the narrow Completed-root exception. Its Arc and
        // open-channel state are captured before loading, then carried ephemerally to the
        // recipient loop for cancellation recovery; restored admissions never gain this witness.
        let preloaded_root = if recipient == root_thread_id {
            match state.get_thread(recipient).await {
                Ok(root) => Some((Arc::clone(&root), root.is_running())),
                Err(error) if matches!(error.details(), CodexErrorDetails::ThreadNotFound(_)) => {
                    None
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };

        let _residency_pin = self
            .ensure_v2_agent_loaded(resume_config, recipient, /*parent*/ None)
            .await?;
        let recipient_thread = state.get_thread(recipient).await?;
        let completed_root_witness = preloaded_root
            .as_ref()
            .filter(|(preloaded, channel_was_open)| {
                *channel_was_open && Arc::ptr_eq(preloaded, &recipient_thread)
            })
            .map(|(preloaded, _)| preloaded);
        let scope = ExplicitUpdateScope {
            root_thread_id: &root_thread_id,
            sender_thread_id: &caller,
            recipient_thread_id: &recipient,
            sender_path: &sender_path,
            recipient_path: &recipient_path,
        };
        validate_current_explicit_membership(
            state.as_ref(),
            &self.runtime.registry,
            &sender_thread,
            &recipient_thread,
            completed_root_witness,
            &scope,
        )
        .await?;

        let mut communication = message.into_communication(
            sender_path.clone(),
            recipient_path.clone(),
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
        validate_current_explicit_membership(
            state.as_ref(),
            &self.runtime.registry,
            &sender_thread,
            &recipient_thread,
            completed_root_witness,
            &scope,
        )
        .await?;

        let rollout_path = recipient_thread.rollout_path().ok_or_else(|| {
            CodexErr::Fatal("explicit inter-agent recipient has no retained rollout".to_string())
        })?;
        // A new idle root has a live recorder but no rollout file until its first persist.
        // Materialize that existing recorder before scanning the durable admission ledger;
        // this does not start a model turn or add model-visible context.
        if codex_rollout::existing_rollout_path(&rollout_path)
            .await
            .is_none()
        {
            recipient_thread
                .session
                .try_ensure_rollout_materialized(PersistContext::Standard)
                .await
                .map_err(|_| {
                    CodexErr::Fatal(
                        "failed to materialize explicit inter-agent recipient rollout".to_string(),
                    )
                })?;
        }
        recipient_thread
            .session
            .flush_rollout()
            .await
            .map_err(|_| {
                CodexErr::Fatal("failed to flush recipient before admission readback".to_string())
            })?;
        let (mut index, _retained_rollout_items) = load_explicit_admission_index(
            &rollout_path,
            root_thread_id,
            recipient,
            &recipient_path,
        )
        .await?;
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
            if !same_explicit_request(existing, &communication, &update, &scope) {
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
                sender_path: sender_path.clone(),
                recipient_path: recipient_path.clone(),
                supersedes: update.supersedes.clone(),
                represented_response_id: None,
            };
            let rollout_item = RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn: false,
                admission: Some(admission.clone()),
            };
            validate_current_explicit_membership(
                state.as_ref(),
                &self.runtime.registry,
                &sender_thread,
                &recipient_thread,
                completed_root_witness,
                &scope,
            )
            .await?;
            recipient_thread
                .session
                .input_queue
                .begin_explicit_admission_hint(update_id.clone(), completed_root_witness)
                .await;
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
        enqueue_pending_explicit_admissions(&recipient_thread.session, &index).await;
        Ok(update_id.to_string())
    }

    pub(crate) async fn restore_explicit_inter_agent_admissions(
        &self,
        session: &Session,
    ) -> CodexResult<()> {
        let root_path = AgentPath::root();
        if session.multi_agent_version() != Some(codex_protocol::protocol::MultiAgentVersion::V2) {
            return Ok(());
        }
        let Some(root_thread_id) = self.runtime.registry.agent_id_for_path(&root_path) else {
            return Ok(());
        };
        let Some(recipient_path) = self
            .runtime
            .registry
            .agent_metadata_for_thread(session.thread_id)
            .and_then(|metadata| metadata.agent_path)
        else {
            return Ok(());
        };
        if self.runtime.registry.agent_id_for_path(&recipient_path) != Some(session.thread_id) {
            return Ok(());
        }
        let Some(rollout_path) = session
            .current_rollout_path()
            .await
            .map_err(|_| CodexErr::Fatal("failed to locate root inter-agent rollout".into()))?
        else {
            return Ok(());
        };
        session.flush_rollout().await.map_err(|_| {
            CodexErr::Fatal("failed to flush recipient before restoring explicit admissions".into())
        })?;
        let (index, _retained_rollout_items) = load_explicit_admission_index(
            &rollout_path,
            root_thread_id,
            session.thread_id,
            &recipient_path,
        )
        .await?;
        enqueue_pending_explicit_admissions(session, &index).await;
        Ok(())
    }

    /// Reconciles a live registered recipient after an explicit activity hint. The sender may
    /// have been cancelled while a recorder append was in flight, so the recipient first waits
    /// for the writer's FIFO flush acknowledgement before deciding whether the hint committed.
    pub(crate) async fn reconcile_explicit_inter_agent_admissions(
        &self,
        session: &Arc<Session>,
    ) -> CodexResult<()> {
        if session.multi_agent_version() != Some(codex_protocol::protocol::MultiAgentVersion::V2) {
            return Ok(());
        }
        let root_path = AgentPath::root();
        let Some(root_thread_id) = self.runtime.registry.agent_id_for_path(&root_path) else {
            return Ok(());
        };
        let Some(recipient_path) = self
            .runtime
            .registry
            .agent_metadata_for_thread(session.thread_id)
            .and_then(|metadata| metadata.agent_path)
        else {
            return Ok(());
        };
        if self.runtime.registry.agent_id_for_path(&recipient_path) != Some(session.thread_id) {
            return Ok(());
        }
        let state = self.runtime.upgrade()?;
        let current_recipient = match state.get_thread(session.thread_id).await {
            Ok(thread) => thread,
            Err(_) => return Ok(()),
        };
        if !Arc::ptr_eq(&current_recipient.session, session)
            || !current_recipient.is_running()
            || current_recipient.multi_agent_version()
                != Some(codex_protocol::protocol::MultiAgentVersion::V2)
            || !Arc::ptr_eq(
                &current_recipient
                    .session
                    .services
                    .local_agent_runtime
                    .registry,
                &self.runtime.registry,
            )
        {
            return Ok(());
        }

        let _admission_guard = session.explicit_inter_agent_admission.lock().await;
        let still_current = state.get_thread(session.thread_id).await.ok();
        if !still_current.is_some_and(|thread| {
            Arc::ptr_eq(&thread, &current_recipient)
                && Arc::ptr_eq(&thread.session, session)
                && thread.is_running()
                && thread.multi_agent_version()
                    == Some(codex_protocol::protocol::MultiAgentVersion::V2)
        }) {
            return Ok(());
        }
        session.flush_rollout().await.map_err(|_| {
            CodexErr::Fatal(
                "failed to flush recipient before reconciling explicit admissions".into(),
            )
        })?;
        let Some(rollout_path) = session
            .current_rollout_path()
            .await
            .map_err(|_| CodexErr::Fatal("failed to locate recipient rollout".into()))?
        else {
            return Ok(());
        };
        let (index, _retained_rollout_items) = load_explicit_admission_index(
            &rollout_path,
            root_thread_id,
            session.thread_id,
            &recipient_path,
        )
        .await?;
        enqueue_pending_explicit_admissions(session, &index).await;
        Ok(())
    }

    pub(crate) async fn explicit_action_recipient_is_current(
        &self,
        session: &Arc<Session>,
        update_ids: &[ResponseItemId],
    ) -> bool {
        let Ok(state) = self.runtime.upgrade() else {
            return false;
        };
        let root_path = AgentPath::root();
        let Some(root_thread_id) = self.runtime.registry.agent_id_for_path(&root_path) else {
            return false;
        };
        let Some(recipient_metadata) = self
            .runtime
            .registry
            .agent_metadata_for_thread(self.session_id)
        else {
            return false;
        };
        let Some(recipient_path) = recipient_metadata.agent_path else {
            return false;
        };
        if recipient_metadata.agent_id != Some(self.session_id)
            || self.runtime.registry.agent_id_for_path(&root_path) != Some(root_thread_id)
            || self.runtime.registry.agent_id_for_path(&recipient_path) != Some(self.session_id)
            || !Arc::ptr_eq(
                &session.services.local_agent_runtime.registry,
                &self.runtime.registry,
            )
            || session.thread_id != self.session_id
            || session.multi_agent_version()
                != Some(codex_protocol::protocol::MultiAgentVersion::V2)
        {
            return false;
        }
        let Ok(current_recipient) = state.get_thread(self.session_id).await else {
            return false;
        };
        if !Arc::ptr_eq(&current_recipient.session, session)
            || !current_recipient.is_running()
            || current_recipient.multi_agent_version()
                != Some(codex_protocol::protocol::MultiAgentVersion::V2)
            || !Arc::ptr_eq(
                &current_recipient
                    .session
                    .services
                    .local_agent_runtime
                    .registry,
                &self.runtime.registry,
            )
        {
            return false;
        }

        let status = current_recipient.agent_status().await;
        if matches!(status, AgentStatus::Interrupted) {
            return false;
        }
        if is_final(&status) {
            if self.session_id != root_thread_id
                || !recipient_path.is_root()
                || !matches!(status, AgentStatus::Completed(_))
            {
                return false;
            }
            for update_id in update_ids {
                let Some(witness) = session.input_queue.completed_root_witness(update_id).await
                else {
                    return false;
                };
                if !Arc::ptr_eq(&current_recipient, &witness) {
                    return false;
                }
            }
        }
        let path_matched_update_ids = session
            .input_queue
            .pending_actionable_explicit_update_ids(self.session_id, Some(&recipient_path))
            .await;
        update_ids
            .iter()
            .all(|update_id| path_matched_update_ids.contains(update_id))
    }
}

async fn valid_explicit_member(
    thread: &Arc<CodexThread>,
    registry: &Arc<crate::agent::registry::AgentRegistry>,
    thread_id: ThreadId,
    path: &AgentPath,
) -> bool {
    thread.session.thread_id == thread_id
        && thread.is_running()
        && thread.multi_agent_version() == Some(codex_protocol::protocol::MultiAgentVersion::V2)
        && Arc::ptr_eq(
            &thread.session.services.local_agent_runtime.registry,
            registry,
        )
        && registry.agent_id_for_path(path) == Some(thread_id)
        && registry
            .agent_metadata_for_thread(thread_id)
            .and_then(|metadata| metadata.agent_path)
            .as_ref()
            == Some(path)
        && !is_final(&thread.agent_status().await)
}

async fn valid_explicit_recipient(
    thread: &Arc<CodexThread>,
    registry: &Arc<crate::agent::registry::AgentRegistry>,
    completed_root_witness: Option<&Arc<CodexThread>>,
    root_thread_id: &ThreadId,
    recipient_thread_id: &ThreadId,
    recipient_path: &AgentPath,
) -> bool {
    let status = thread.agent_status().await;
    let root_path = AgentPath::root();
    let completed_root_is_original_live_session = *root_thread_id == *recipient_thread_id
        && recipient_path.is_root()
        && matches!(&status, AgentStatus::Completed(_))
        && completed_root_witness.is_some_and(|captured_root| Arc::ptr_eq(thread, captured_root));

    thread.session.thread_id == *recipient_thread_id
        && thread.is_running()
        && thread.multi_agent_version() == Some(codex_protocol::protocol::MultiAgentVersion::V2)
        && Arc::ptr_eq(
            &thread.session.services.local_agent_runtime.registry,
            registry,
        )
        && registry.agent_id_for_path(&root_path) == Some(*root_thread_id)
        && registry.agent_id_for_path(recipient_path) == Some(*recipient_thread_id)
        && registry
            .agent_metadata_for_thread(*recipient_thread_id)
            .and_then(|metadata| metadata.agent_path)
            .as_ref()
            == Some(recipient_path)
        && (!is_final(&status) || completed_root_is_original_live_session)
}

pub(super) async fn validate_current_explicit_membership(
    state: &crate::thread_manager::ThreadManagerState,
    registry: &Arc<crate::agent::registry::AgentRegistry>,
    original_sender: &Arc<CodexThread>,
    original_recipient: &Arc<CodexThread>,
    completed_root_witness: Option<&Arc<CodexThread>>,
    scope: &ExplicitUpdateScope<'_>,
) -> CodexResult<()> {
    let root_path = AgentPath::root();
    let current_root_id = registry
        .agent_id_for_path(&root_path)
        .ok_or_else(|| CodexErr::ThreadNotFound(*scope.root_thread_id))?;
    let sender_metadata = registry
        .agent_metadata_for_thread(*scope.sender_thread_id)
        .ok_or_else(|| {
            CodexErr::InvalidRequest("explicit update sender is no longer registered".into())
        })?;
    let recipient_metadata = registry
        .agent_metadata_for_thread(*scope.recipient_thread_id)
        .ok_or_else(|| {
            CodexErr::InvalidRequest("explicit update recipient is no longer registered".into())
        })?;
    let current_sender = state.get_thread(*scope.sender_thread_id).await?;
    let current_recipient = state.get_thread(*scope.recipient_thread_id).await?;
    if &current_root_id != scope.root_thread_id
        || sender_metadata.agent_path.as_ref() != Some(scope.sender_path)
        || recipient_metadata.agent_path.as_ref() != Some(scope.recipient_path)
        || registry.agent_id_for_path(scope.recipient_path) != Some(*scope.recipient_thread_id)
        || registry.agent_id_for_path(scope.sender_path) != Some(*scope.sender_thread_id)
        || !Arc::ptr_eq(original_sender, &current_sender)
        || !Arc::ptr_eq(original_recipient, &current_recipient)
        || !valid_explicit_member(
            &current_sender,
            registry,
            *scope.sender_thread_id,
            scope.sender_path,
        )
        .await
        || !valid_explicit_recipient(
            &current_recipient,
            registry,
            completed_root_witness,
            scope.root_thread_id,
            scope.recipient_thread_id,
            scope.recipient_path,
        )
        .await
    {
        return Err(CodexErr::InvalidRequest(
            "explicit update membership changed before durable admission".to_string(),
        ));
    }
    Ok(())
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
    recipient_session: &Session,
    index: &ExplicitAdmissionIndex,
) {
    let superseded_update_ids = index
        .ordered
        .iter()
        .flat_map(|admission| admission.supersedes.iter().cloned())
        .collect::<HashSet<_>>();
    let represented_update_ids = index
        .represented_response_ids
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    let unavailable_update_ids = superseded_update_ids
        .union(&represented_update_ids)
        .cloned()
        .collect::<HashSet<_>>();
    let committed_update_ids = index.by_update_id.keys().cloned().collect::<HashSet<_>>();
    recipient_session
        .input_queue
        .finish_explicit_admission_reconciliation(&committed_update_ids, &unavailable_update_ids)
        .await;
    let removed_pending_update_ids = recipient_session
        .input_queue
        .remove_pending_explicit_updates(&unavailable_update_ids)
        .await;
    let pending = {
        let mut delivered = recipient_session
            .delivered_explicit_inter_agent_updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        delivered.retain(|update_id| {
            !represented_update_ids.contains(update_id)
                && (!superseded_update_ids.contains(update_id)
                    || !removed_pending_update_ids.contains(update_id))
        });
        index
            .ordered
            .iter()
            .filter(|admission| {
                !unavailable_update_ids.contains(&admission.update_id)
                    && !delivered.contains(&admission.update_id)
            })
            .cloned()
            .collect::<Vec<_>>()
    };

    for admission in pending {
        let explicit_activity = ExplicitMailboxActivity {
            update_id: admission.update_id.clone(),
            intent: admission.intent,
            sender_thread_id: admission.sender_thread_id,
            recipient_thread_id: admission.recipient_thread_id,
            sender_path: admission.sender_path.clone(),
            recipient_path: admission.recipient_path.clone(),
            generation: None,
        };
        recipient_session
            .input_queue
            .enqueue_explicit_mailbox_communication(
                admission.original_communication.clone(),
                Default::default(),
                explicit_activity,
            )
            .await;
        recipient_session
            .delivered_explicit_inter_agent_updates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(admission.update_id.clone());
        crate::agent_communication::emit_agent_communication_receive(admission.update_id.as_str());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_admission_replay_rejects_root_self_send() {
        let root_thread_id = ThreadId::new();
        let update_id = ResponseItemId::new("invalid-root-sender");
        let root_path = AgentPath::root();
        let mut original_communication = InterAgentCommunication::new(
            root_path.clone(),
            root_path.clone(),
            Vec::new(),
            "invalid root self-message".to_string(),
            /*trigger_turn*/ false,
        );
        original_communication.id = Some(update_id.clone());
        let admission = InterAgentCommunicationAdmission {
            update_id,
            idempotency_key: "invalid-root-sender-key".to_string(),
            intent: InterAgentCommunicationIntent::Progress,
            original_communication,
            root_thread_id,
            sender_thread_id: root_thread_id,
            recipient_thread_id: root_thread_id,
            sender_path: root_path.clone(),
            recipient_path: root_path,
            supersedes: Vec::new(),
            represented_response_id: None,
        };
        let items = [RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
            admission: Some(admission),
        }];

        assert!(matches!(
            ExplicitAdmissionIndex::from_rollout_items(
                &items,
                root_thread_id,
                root_thread_id,
                &AgentPath::root(),
            ),
            Err(err)
                if matches!(
                    err.details(),
                    codex_protocol::error::CodexErrorDetails::InvalidRequest(_)
                )
        ));
    }

    #[test]
    fn explicit_admission_replay_rejects_a_stale_recipient_path() {
        let root_thread_id = ThreadId::new();
        let sender_thread_id = root_thread_id;
        let recipient_thread_id = ThreadId::new();
        let update_id = ResponseItemId::new("recipient-path-mismatch");
        let root_path = AgentPath::root();
        let stored_recipient_path = AgentPath::try_from("/root/worker").expect("worker path");
        let current_recipient_path = AgentPath::try_from("/root/replacement")
            .expect("replacement path");
        let mut original_communication = InterAgentCommunication::new(
            root_path.clone(),
            stored_recipient_path.clone(),
            Vec::new(),
            "scoped explicit message".to_string(),
            /*trigger_turn*/ false,
        );
        original_communication.id = Some(update_id.clone());
        let admission = InterAgentCommunicationAdmission {
            update_id,
            idempotency_key: "recipient-path-mismatch-key".to_string(),
            intent: InterAgentCommunicationIntent::Progress,
            original_communication,
            root_thread_id,
            sender_thread_id,
            recipient_thread_id,
            sender_path: root_path,
            recipient_path: stored_recipient_path,
            supersedes: Vec::new(),
            represented_response_id: None,
        };
        let items = [RolloutItem::InterAgentCommunicationMetadata {
            trigger_turn: false,
            admission: Some(admission),
        }];

        assert!(matches!(
            ExplicitAdmissionIndex::from_rollout_items(
                &items,
                root_thread_id,
                recipient_thread_id,
                &current_recipient_path,
            ),
            Err(err)
                if matches!(
                    err.details(),
                    codex_protocol::error::CodexErrorDetails::InvalidRequest(_)
                )
        ));
    }
}
