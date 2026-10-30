//! Native V2 durable-mailbox lifecycle hooks.
//!
//! This adapter is intentionally separate from `mailbox`, which remains the
//! synchronous in-memory V1 `take_mailbox` implementation. Durable work is
//! captured before the native operation is published and is never drained by
//! the V1 interface.

use super::LocalAgentControl;
use crate::PendingAgentMessage;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::MessageDelivery;
use crate::config::Config;
use codex_agent_mailbox::ActivationDecision;
use codex_agent_mailbox::ActivationDisposition;
use codex_agent_mailbox::DispatchDecision;
use codex_agent_mailbox::Envelope;
use codex_agent_mailbox::Mailbox;
use codex_agent_mailbox::MessageDraft;
use codex_agent_mailbox::MessageKind;
use codex_agent_mailbox::NativeWork;
use codex_agent_mailbox::RecoveryEvidence;
use codex_agent_mailbox::WorkRef;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::Op;
use codex_protocol::turn_input::TurnStartOptions;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MailboxActivationPlan {
    QueueOnly,
    StartIdleRecipient,
    AlreadyActivated,
}

fn storage_error(error: impl std::fmt::Display) -> CodexErr {
    CodexErr::Fatal(format!("agent mailbox storage failed: {error}"))
}

fn compatible(config: &Config) -> bool {
    config.multi_agent_version_from_features() == MultiAgentVersion::V2
        && config.features.enabled(Feature::AgentMailbox)
}

pub(super) fn envelope_preempts_active_turn(envelope: &Envelope) -> bool {
    envelope.legacy_attention
        || envelope.reply_to.is_some()
        || matches!(envelope.kind, MessageKind::Request | MessageKind::Alert)
}

fn dedupe_pending_inputs(inputs: Vec<PendingAgentMessage>) -> Vec<PendingAgentMessage> {
    let mut seen = HashSet::new();
    inputs
        .into_iter()
        .filter(|input| match input.durable_input_id() {
            Some(id) => seen.insert(id.to_string()),
            None => true,
        })
        .collect()
}

impl LocalAgentControl {
    /// Lazily opens the separate durable store only for an explicitly enabled V2 tree.
    pub(crate) async fn mailbox_v2_initialize(&self, config: &Config) -> CodexResult<()> {
        if !compatible(config) {
            return Err(CodexErr::UnsupportedOperation(
                "durable native agent work requires MultiAgentV2 and agent_mailbox to be enabled"
                    .to_string(),
            ));
        }
        self.runtime
            .mailbox_v2
            .get_or_try_init(|| async {
                Mailbox::open(config.sqlite_config())
                    .await
                    .map(Arc::new)
                    .map_err(storage_error)
            })
            .await?;
        Ok(())
    }

    pub(crate) fn mailbox_v2(&self) -> Option<Arc<Mailbox>> {
        self.runtime.mailbox_v2.get().cloned()
    }

    /// A resumed recipient can reach its first turn before any native send or
    /// wait operation has opened this tree's durable store. Initialize it from
    /// that live recipient's captured config so accepted work is replayed on
    /// restart rather than mistaken for an empty mailbox.
    async fn mailbox_v2_for_recipient(
        &self,
        recipient: ThreadId,
    ) -> CodexResult<Option<Arc<Mailbox>>> {
        if let Some(mailbox) = self.mailbox_v2() {
            return Ok(Some(mailbox));
        }
        let state = self.runtime.upgrade()?;
        let thread = state.get_thread(recipient).await?;
        let config = thread.session.get_config().await;
        if !compatible(&config) {
            return Ok(None);
        }
        self.mailbox_v2_initialize(&config).await?;
        Ok(self.mailbox_v2())
    }

    /// Claims an accepted message's optional idle activation. Acceptance is
    /// durable first; only the recipient's captured feature/config and live
    /// idle state can authorize a real turn start.
    pub(crate) async fn mailbox_v2_activation_plan(
        &self,
        recipient: ThreadId,
        envelope: &Envelope,
    ) -> CodexResult<MailboxActivationPlan> {
        if !envelope.activate_if_idle {
            return Ok(MailboxActivationPlan::QueueOnly);
        }
        let mailbox = self.mailbox_v2().ok_or_else(|| {
            CodexErr::Fatal("accepted mailbox activation lost its durable store".into())
        })?;
        match mailbox
            .begin_activation(envelope.sender, &envelope.id)
            .await
            .map_err(storage_error)?
        {
            ActivationDecision::AlreadyDecided(ActivationDisposition::Activated) => {
                return Ok(MailboxActivationPlan::AlreadyActivated);
            }
            ActivationDecision::AlreadyDecided(_) => {
                return Ok(MailboxActivationPlan::QueueOnly);
            }
            ActivationDecision::RecoveryNeeded => {
                return Err(CodexErr::UnsupportedOperation(format!(
                    "mailbox activation for {} has an unresolved prior attempt",
                    envelope.id
                )));
            }
            ActivationDecision::Activate => {}
        }

        let deny = |reason: &'static str| {
            let mailbox = Arc::clone(&mailbox);
            let sender = envelope.sender;
            let message_id = envelope.id.clone();
            async move {
                mailbox
                    .record_activation(sender, &message_id, ActivationDisposition::Denied)
                    .await
                    .map_err(storage_error)?;
                tracing::debug!(message_id = %message_id, %reason, "native mailbox idle activation denied");
                Ok::<_, CodexErr>(MailboxActivationPlan::QueueOnly)
            }
        };
        let metadata = match self.runtime.ensure_agent_known(recipient) {
            Ok(metadata) => metadata,
            Err(_) => return deny("recipient_is_not_a_known_native_agent").await,
        };
        if metadata
            .agent_path
            .as_ref()
            .is_none_or(codex_protocol::AgentPath::is_root)
        {
            return deny("root_activation_is_not_allowed").await;
        }
        let state = self.runtime.upgrade()?;
        let thread = match state.get_thread(recipient).await {
            Ok(thread) if thread.is_running() => thread,
            _ => return deny("recipient_is_not_live").await,
        };
        let config = thread.session.get_config().await;
        if !compatible(&config) || !config.features.enabled(Feature::AgentMailboxIdleActivation) {
            return deny("recipient_idle_activation_is_not_enabled").await;
        }
        if thread.session.active_turn.lock().await.is_some() {
            mailbox
                .record_activation(
                    envelope.sender,
                    &envelope.id,
                    ActivationDisposition::AlreadyActive,
                )
                .await
                .map_err(storage_error)?;
            return Ok(MailboxActivationPlan::QueueOnly);
        }
        if let Some(state_db) = thread.session.services.state_db.as_ref() {
            if state_db
                .thread_goals()
                .get_thread_goal(recipient)
                .await
                .map_err(storage_error)?
                .is_some_and(|goal| goal.status != codex_state::ThreadGoalStatus::Active)
            {
                return deny("recipient_goal_requires_native_control").await;
            }
        } else {
            return deny("recipient_goal_state_is_unavailable").await;
        }
        if thread
            .ensure_execution_capacity_for_turn_start(self)
            .await
            .is_err()
        {
            return deny("recipient_has_no_execution_capacity").await;
        }
        Ok(MailboxActivationPlan::StartIdleRecipient)
    }

    pub(crate) async fn mailbox_v2_finish_activation(
        &self,
        envelope: &Envelope,
        disposition: ActivationDisposition,
    ) -> CodexResult<()> {
        let Some(mailbox) = self.mailbox_v2() else {
            return Err(CodexErr::Fatal(
                "mailbox activation lost its durable store before settlement".into(),
            ));
        };
        mailbox
            .record_activation(envelope.sender, &envelope.id, disposition)
            .await
            .map_err(storage_error)
    }

    /// Deliver a response created atomically with a terminal mailbox outcome.
    /// Unloaded recipients keep SQLite custody; live recipients receive the
    /// same stable ID through their asynchronous InputQueue path.
    pub(crate) async fn mailbox_v2_deliver_outcome_messages(
        &self,
        recipient: ThreadId,
        message_ids: &[String],
    ) -> CodexResult<()> {
        let Some(mailbox) = self.mailbox_v2() else {
            return Err(CodexErr::Fatal(
                "recorded mailbox outcomes lost their durable store".into(),
            ));
        };
        let state = self.runtime.upgrade()?;
        for id in message_ids {
            let Some(envelope) = mailbox
                .get_message(recipient, id)
                .await
                .map_err(storage_error)?
            else {
                return Err(storage_error(format!(
                    "recorded mailbox response {id} is missing"
                )));
            };
            match state.get_thread(recipient).await {
                Ok(_) => {}
                Err(_) => continue,
            }
            let plan = self
                .mailbox_v2_activation_plan(recipient, &envelope)
                .await?;
            if plan == MailboxActivationPlan::AlreadyActivated {
                continue;
            }
            let mut communication = envelope.communication.clone();
            if plan == MailboxActivationPlan::StartIdleRecipient {
                communication.trigger_turn = true;
            }
            let delivery = MessageDelivery::restored_mailbox_message_input(
                envelope.id.clone(),
                Some(envelope.id.clone()),
                envelope_preempts_active_turn(&envelope),
            );
            let result = state
                .send_op(
                    recipient,
                    Op::InterAgentCommunication {
                        communication,
                        start_options: TurnStartOptions::default(),
                    },
                    /*parent_turn_id*/ None,
                    /*root_turn_id*/ None,
                    Some(delivery),
                )
                .await;
            let result = self
                .handle_thread_request_result(recipient, &state, result)
                .await;
            if plan == MailboxActivationPlan::StartIdleRecipient {
                self.mailbox_v2_finish_activation(
                    &envelope,
                    if result.is_ok() {
                        ActivationDisposition::Activated
                    } else {
                        ActivationDisposition::Unknown
                    },
                )
                .await?;
            }
            result?;
        }
        Ok(())
    }

    pub(crate) async fn mailbox_v2_accept_native_message(
        &self,
        recipient: ThreadId,
        context: &AgentCommunicationContext,
        communication: &InterAgentCommunication,
    ) -> CodexResult<Option<Envelope>> {
        if !context.is_native_message() {
            return Ok(None);
        }
        let native_key = context.native_key.as_deref().ok_or_else(|| {
            CodexErr::InvalidRequest("native durable message is missing its invocation key".into())
        })?;
        let state = self.runtime.upgrade()?;
        let thread = state.get_thread(recipient).await?;
        let recipient_config = thread.session.get_config().await;
        if !compatible(&recipient_config) {
            return Err(CodexErr::UnsupportedOperation(format!(
                "durable native messages to {recipient} require MultiAgentV2 and agent_mailbox to be enabled"
            )));
        }
        let mailbox = self.mailbox_v2().ok_or_else(|| {
            CodexErr::Fatal(
                "agent mailbox was enabled for native messages but its durable store is unavailable"
                    .to_string(),
            )
        })?;
        let kind = if context.is_completion_result() {
            MessageKind::Alert
        } else {
            context.message_kind.unwrap_or_default()
        };
        let reply_to = if context.is_completion_result() {
            None
        } else {
            context.reply_to.clone()
        };
        let activate_if_idle = (context.is_completion_result()
            || matches!(kind, MessageKind::Request | MessageKind::Alert)
            || reply_to.is_some())
            && recipient_config
                .features
                .enabled(Feature::AgentMailboxIdleActivation);
        let envelope = mailbox
            .accept_message(MessageDraft {
                invocation_key: format!("{}:{}:{native_key}", context.sender_thread_id, recipient),
                sender: context.sender_thread_id,
                recipient,
                communication: communication.clone(),
                kind,
                activate_if_idle,
                work_ref: None,
                reply_to,
                supersedes: None,
                legacy_attention: false,
            })
            .await
            .map_err(storage_error)?;
        Ok(Some(envelope))
    }

    pub(crate) async fn mailbox_v2_pending_inputs(
        &self,
        recipient: ThreadId,
        config: &crate::config::Config,
    ) -> CodexResult<Vec<PendingAgentMessage>> {
        // The active session already owns its captured config. Do not resolve it again through
        // ThreadManager: an accepted child may have left the registry while its initial input
        // is still being submitted through the retained thread handle.
        if self.mailbox_v2().is_none() {
            self.mailbox_v2_initialize(config).await?;
        }
        let Some(mailbox) = self.mailbox_v2() else {
            return Ok(Vec::new());
        };
        let page = mailbox
            .pending_page(recipient, /*after_seq*/ None, /*limit*/ 100)
            .await
            .map_err(storage_error)?;
        if page.next_cursor.is_some() {
            return Err(storage_error(
                "pending mailbox input page exceeds bounded recipient capacity",
            ));
        }
        let mut inputs = page
            .items
            .into_iter()
            .filter(|envelope| !envelope.canonical)
            .map(|envelope| {
                let preempts_active_turn = envelope_preempts_active_turn(&envelope);
                let id = envelope.id.clone();
                PendingAgentMessage::new(
                    envelope.communication,
                    Some(MessageDelivery::restored_mailbox_message_input(
                        id.clone(),
                        Some(id),
                        preempts_active_turn,
                    )),
                )
            })
            .collect::<Vec<_>>();
        for id in mailbox
            .unadmitted_native_input_ids(recipient, /*limit*/ 101)
            .await
            .map_err(storage_error)?
        {
            let Some(work) = mailbox
                .work_for_input_id(recipient, &id)
                .await
                .map_err(storage_error)?
            else {
                continue;
            };
            let communication = mailbox.accepted_input(&work).await.map_err(storage_error)?;
            inputs.push(PendingAgentMessage::new(
                communication,
                Some(MessageDelivery::restored_mailbox_input(
                    id, /*durable_message_id*/ None,
                )),
            ));
        }
        inputs = dedupe_pending_inputs(inputs);
        if inputs.len() > 100 {
            return Err(storage_error(
                "pending durable mailbox inputs exceed bounded recipient capacity",
            ));
        }
        Ok(inputs)
    }

    pub(crate) async fn mailbox_v2_inputs_for_ids(
        &self,
        recipient: ThreadId,
        ids: &[String],
    ) -> CodexResult<Vec<PendingAgentMessage>> {
        let Some(mailbox) = self.mailbox_v2_for_recipient(recipient).await? else {
            return Ok(Vec::new());
        };
        let mut messages = Vec::new();
        for id in mailbox
            .unadmitted_input_ids(recipient, ids)
            .await
            .map_err(storage_error)?
        {
            if let Some(envelope) = mailbox
                .get_message(recipient, &id)
                .await
                .map_err(storage_error)?
            {
                let preempts_active_turn = envelope_preempts_active_turn(&envelope);
                messages.push(PendingAgentMessage::new(
                    envelope.communication,
                    Some(MessageDelivery::restored_mailbox_message_input(
                        id.clone(),
                        Some(id),
                        preempts_active_turn,
                    )),
                ));
                continue;
            }
            if let Some(work) = mailbox
                .work_for_input_id(recipient, &id)
                .await
                .map_err(storage_error)?
            {
                let communication = mailbox.accepted_input(&work).await.map_err(storage_error)?;
                messages.push(PendingAgentMessage::new(
                    communication,
                    Some(MessageDelivery::restored_mailbox_input(
                        id, /*durable_message_id*/ None,
                    )),
                ));
            }
        }
        Ok(messages)
    }

    pub(crate) async fn mailbox_v2_unadmitted_inputs(
        &self,
        recipient: ThreadId,
    ) -> CodexResult<Vec<PendingAgentMessage>> {
        let Some(mailbox) = self.mailbox_v2_for_recipient(recipient).await? else {
            return Ok(Vec::new());
        };
        let page = mailbox
            .pending_page(recipient, /*after_seq*/ None, /*limit*/ 100)
            .await
            .map_err(storage_error)?;
        if page.next_cursor.is_some() {
            return Err(storage_error(
                "unadmitted mailbox inputs exceed bounded compaction capacity",
            ));
        }
        let mut inputs = page
            .items
            .into_iter()
            .map(|envelope| {
                let preempts_active_turn = envelope_preempts_active_turn(&envelope);
                let id = envelope.id;
                PendingAgentMessage::new(
                    envelope.communication,
                    Some(MessageDelivery::restored_mailbox_message_input(
                        id.clone(),
                        Some(id),
                        preempts_active_turn,
                    )),
                )
            })
            .collect::<Vec<_>>();
        for id in mailbox
            .unadmitted_native_input_ids(recipient, /*limit*/ 100)
            .await
            .map_err(storage_error)?
        {
            let Some(work) = mailbox
                .work_for_input_id(recipient, &id)
                .await
                .map_err(storage_error)?
            else {
                continue;
            };
            let communication = mailbox.accepted_input(&work).await.map_err(storage_error)?;
            inputs.push(PendingAgentMessage::new(
                communication,
                Some(MessageDelivery::restored_mailbox_input(
                    id, /*durable_message_id*/ None,
                )),
            ));
        }
        inputs = dedupe_pending_inputs(inputs);
        if inputs.len() > 100 {
            return Err(storage_error(
                "unadmitted durable mailbox inputs exceed bounded compaction capacity",
            ));
        }
        Ok(inputs)
    }

    pub(crate) async fn mailbox_v2_confirm_input_canonical(
        &self,
        recipient: ThreadId,
        id: &str,
    ) -> CodexResult<()> {
        let Some(mailbox) = self.mailbox_v2_for_recipient(recipient).await? else {
            return Ok(());
        };
        if mailbox
            .get_message(recipient, id)
            .await
            .map_err(storage_error)?
            .is_some()
        {
            mailbox
                .mark_canonical(recipient, id)
                .await
                .map_err(storage_error)?;
        }
        Ok(())
    }

    pub(crate) async fn mailbox_v2_record_admission(
        &self,
        recipient: ThreadId,
        turn_id: &str,
        ids: &[String],
    ) -> CodexResult<()> {
        let Some(mailbox) = self.mailbox_v2_for_recipient(recipient).await? else {
            return Ok(());
        };
        let ids = mailbox
            .unadmitted_input_ids(recipient, ids)
            .await
            .map_err(storage_error)?;
        if !ids.is_empty() {
            mailbox
                .record_admission(recipient, turn_id, &ids)
                .await
                .map_err(storage_error)?;
        }
        Ok(())
    }

    /// Capture immutable accepted native work before observers or the recipient
    /// can see it. Replays resolve the original submission and input reference.
    pub(crate) async fn mailbox_v2_accept_native_submission(
        &self,
        recipient: ThreadId,
        context: &AgentCommunicationContext,
        communication: &mut InterAgentCommunication,
        submission_id: &mut String,
    ) -> CodexResult<Option<(WorkRef, DispatchDecision)>> {
        if !context.is_native_work() {
            return Ok(None);
        }
        let Some(native_key) = context.native_key.as_deref() else {
            return Ok(None);
        };
        self.runtime.ensure_agent_known(context.sender_thread_id)?;

        let state = self.runtime.upgrade()?;
        let thread = state.get_thread(recipient).await?;
        let recipient_config = thread.session.get_config().await;
        if !compatible(&recipient_config) {
            if let Some(mailbox) = self.mailbox_v2()
                && let Some(existing) = mailbox
                    .work_for_native_key(context.sender_thread_id, native_key)
                    .await
                    .map_err(storage_error)?
            {
                mailbox
                    .mark_recovery_needed(&existing, RecoveryEvidence::RecipientIncompatible)
                    .await
                    .map_err(storage_error)?;
                return Err(CodexErr::UnsupportedOperation(format!(
                    "accepted work {} for {recipient} is incompatible with the current recipient configuration; its original input is retained",
                    existing.id
                )));
            }
            return Err(CodexErr::UnsupportedOperation(format!(
                "durable native work for {recipient} requires MultiAgentV2 and agent_mailbox to be enabled"
            )));
        }
        let mailbox = self.mailbox_v2().ok_or_else(|| {
            CodexErr::Fatal(
                "agent mailbox was enabled for native work but its durable store is unavailable"
                    .to_string(),
            )
        })?;

        let existing = mailbox
            .work_for_native_key(context.sender_thread_id, native_key)
            .await
            .map_err(storage_error)?;
        if let Some(existing) = existing.as_ref() {
            *submission_id = existing.submission_id.clone();
        }
        communication.id = None;
        let work = mailbox
            .accept_work(NativeWork {
                native_key: native_key.to_string(),
                issuer: context.sender_thread_id,
                agent: recipient,
                submission_id: submission_id.clone(),
                input: communication.clone(),
            })
            .await
            .map_err(storage_error)?;
        *communication = mailbox.accepted_input(&work).await.map_err(storage_error)?;
        *submission_id = work.submission_id.clone();
        let decision = mailbox.begin_dispatch(&work).await.map_err(storage_error)?;
        Ok(Some((work, decision)))
    }

    pub(crate) async fn mailbox_v2_mark_dispatched(&self, work: &WorkRef) -> CodexResult<()> {
        let Some(mailbox) = self.mailbox_v2() else {
            return Err(CodexErr::Fatal(format!(
                "accepted work {} lost its durable mailbox handle",
                work.id
            )));
        };
        mailbox.mark_dispatched(work).await.map_err(storage_error)
    }
}
