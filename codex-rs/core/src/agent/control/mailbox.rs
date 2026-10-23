//! The narrow native-runtime adapter for the independently stored V2 mailbox.
//!
//! The mailbox never owns agent membership, execution, or approval policy. Native lifecycle
//! boundaries supply accepted inputs and outcomes; canonical history owns observation custody.

use super::AgentControl;
use crate::agent::AgentStatus;
use crate::agent::goal_notifications::GoalNotificationPhase;
use crate::agent::goal_notifications::GoalNotificationSnapshot;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::config::Config;
use codex_agent_mailbox::Continuation;
use codex_agent_mailbox::ActivationDisposition;
use codex_agent_mailbox::DispatchDecision;
use codex_agent_mailbox::GoalBinding;
use codex_agent_mailbox::Envelope;
use codex_agent_mailbox::Mailbox;
use codex_agent_mailbox::NativeWork;
use codex_agent_mailbox::Outcome;
use codex_agent_mailbox::WorkRef;
use codex_features::Feature;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::models::ResponseItem;
use std::sync::Arc;
use std::collections::HashSet;

fn storage_error(error: impl std::fmt::Display) -> CodexErr {
    // The store does not log payloads. Keep its typed diagnostic in the error chain presented
    // to the native caller rather than treating failed acceptance as successful delivery.
    CodexErr::Fatal(format!("agent mailbox storage failed: {error}"))
}

impl AgentControl {
    pub(crate) async fn mailbox_initialize(&self, config: &Config) -> CodexResult<()> {
        if config.multi_agent_version_from_features() != MultiAgentVersion::V2
            || !config.features.enabled(Feature::AgentMailbox)
        {
            return Ok(());
        }
        self.mailbox
            .get_or_try_init(|| async {
                Mailbox::open(config.sqlite_config())
                    .await
                    .map(Arc::new)
                    .map_err(storage_error)
            })
            .await?;
        Ok(())
    }

    pub(crate) fn mailbox_enabled(&self) -> bool {
        self.mailbox.get().is_some()
    }

    pub(crate) fn mailbox(&self) -> Option<Arc<Mailbox>> {
        self.mailbox.get().cloned()
    }

    pub(crate) async fn mailbox_work_for_key(
        &self,
        issuer: ThreadId,
        native_key: &str,
    ) -> CodexResult<Option<WorkRef>> {
        let Some(mailbox) = self.mailbox() else {
            return Ok(None);
        };
        mailbox
            .work_for_native_key(issuer, native_key)
            .await
            .map_err(storage_error)
    }

    pub(super) async fn mailbox_accept_native_submission(
        &self,
        agent: ThreadId,
        context: &AgentCommunicationContext,
        communication: &mut InterAgentCommunication,
        submission_id: &mut String,
    ) -> CodexResult<Option<(WorkRef, DispatchDecision)>> {
        let Some(mailbox) = self.mailbox() else {
            return Ok(None);
        };
        if !matches!(
            context.kind,
            AgentCommunicationKind::Spawn | AgentCommunicationKind::Followup
        ) {
            return Ok(None);
        }
        if let Some(native_key) = context.native_key.as_deref()
            && let Some(existing) = mailbox.work_for_native_key(context.sender_thread_id, native_key)
                .await.map_err(storage_error)?
        {
            *submission_id = existing.submission_id;
        }
        let work = mailbox
            .accept_work(NativeWork {
                native_key: context
                    .native_key
                    .clone()
                    .unwrap_or_else(|| submission_id.clone()),
                issuer: context.sender_thread_id,
                agent,
                submission_id: submission_id.clone(),
                input: communication.clone(),
            })
            .await
            .map_err(storage_error)?;
        *communication = mailbox.accepted_input(&work).await.map_err(storage_error)?;
        *submission_id = work.submission_id.clone();
        let dispatch = mailbox.begin_dispatch(&work).await.map_err(storage_error)?;
        Ok(Some((work, dispatch)))
    }

    pub(super) async fn mailbox_dispatched(&self, work: &WorkRef) -> CodexResult<()> {
        if let Some(mailbox) = self.mailbox() {
            mailbox.mark_dispatched(work).await.map_err(storage_error)?;
        }
        Ok(())
    }

    pub(crate) async fn mailbox_activate_if_idle(
        &self,
        recipient: ThreadId,
        envelope: &Envelope,
    ) -> CodexResult<(ActivationDisposition, Option<String>)> {
        let denied = |reason: &str| (ActivationDisposition::Denied, Some(reason.to_string()));
        let metadata = self.ensure_agent_known(recipient)?;
        if metadata.agent_path.as_ref().is_none_or(|path| path.is_root()) {
            return Ok(denied("root_activation_is_not_allowed"));
        }
        let state = self.upgrade()?;
        // Reloading is not a side effect of accepting mail. An unloaded recipient keeps durable
        // custody until it is explicitly resumed through its normal native lifecycle.
        let Ok(thread) = state.get_thread(recipient).await else {
            return Ok(denied("recipient_is_not_live"));
        };
        let config = thread.session.get_config().await;
        if !config.features.enabled(Feature::AgentMailboxIdleActivation) {
            return Ok(denied("recipient_idle_activation_not_enabled"));
        }
        if !thread.is_running() {
            return Ok(denied("recipient_runtime_is_not_accepting_input"));
        }
        if thread.session.active_turn.lock().await.is_some() {
            return Ok((ActivationDisposition::AlreadyActive, None));
        }
        if thread.session.services.thread_extension_data
            .get::<crate::agent::goal_notifications::GoalNotificationStore>()
            .and_then(|store| store.snapshot())
            .is_some_and(|goal| goal.status != ThreadGoalStatus::Active)
        {
            return Ok(denied("recipient_goal_requires_native_control"));
        }
        let mut communication = envelope.communication.clone();
        communication.trigger_turn = true;
        self.send_inter_agent_communication(
            recipient,
            communication,
            AgentCommunicationContext::new(AgentCommunicationKind::Message, envelope.sender)
                .with_native_key(envelope.id.clone()),
        )
        .await?;
        Ok((ActivationDisposition::Activated, None))
    }

    pub(crate) async fn mailbox_mark_canonical(
        &self,
        recipient: ThreadId,
        message_ids: &[ResponseItemId],
    ) -> CodexResult<()> {
        let Some(mailbox) = self.mailbox() else {
            return Ok(());
        };
        for id in message_ids {
            if mailbox.get_message(recipient, id.as_str()).await.map_err(storage_error)?.is_some() {
                mailbox.mark_canonical(recipient, id.as_str()).await.map_err(storage_error)?;
            }
        }
        Ok(())
    }

    pub(crate) async fn mailbox_reconcile_canonical_ids(
        &self,
        recipient: ThreadId,
        canonical_ids: &HashSet<ResponseItemId>,
    ) -> CodexResult<()> {
        let Some(mailbox) = self.mailbox() else {
            return Ok(());
        };
        let mut cursor = None;
        loop {
            let page = mailbox.pending_page(recipient, cursor, 100).await.map_err(storage_error)?;
            for envelope in &page.items {
                if envelope.communication.id.as_ref().is_some_and(|id| canonical_ids.contains(id)) {
                    mailbox.mark_canonical(recipient, &envelope.id).await.map_err(storage_error)?;
                }
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(())
    }

    pub(crate) async fn mailbox_preserve_unadmitted(
        &self,
        recipient: ThreadId,
        current_active_history: &[ResponseItem],
    ) -> CodexResult<Vec<ResponseItem>> {
        let Some(mailbox) = self.mailbox() else {
            return Ok(Vec::new());
        };
        let unread = mailbox.unadmitted_canonical_ids(recipient, 4096)
            .await.map_err(storage_error)?.into_iter().collect::<HashSet<_>>();
        Ok(current_active_history.iter().filter(|item| {
            matches!(item, ResponseItem::AgentMessage { id: Some(id), .. } if unread.contains(id.as_str()))
        }).cloned().collect())
    }

    pub(crate) async fn mailbox_automatic_inputs(
        &self,
        recipient: ThreadId,
        source_turn_id: &str,
    ) -> CodexResult<Vec<InterAgentCommunication>> {
        let Some(mailbox) = self.mailbox() else {
            return Ok(Vec::new());
        };
        Ok(mailbox.automatic_inputs(recipient, source_turn_id, 2)
            .await.map_err(storage_error)?.into_iter()
            .map(|envelope| envelope.communication).collect())
    }

    pub(crate) async fn mailbox_record_admission(
        &self,
        recipient: ThreadId,
        source_turn_id: &str,
        message_ids: &[ResponseItemId],
    ) -> CodexResult<()> {
        let Some(mailbox) = self.mailbox() else {
            return Ok(());
        };
        let ids = message_ids.iter().map(ToString::to_string).collect::<Vec<_>>();
        for candidates in ids.chunks(100) {
            let admitted = mailbox.unadmitted_input_ids(recipient, candidates)
                .await.map_err(storage_error)?;
            if !admitted.is_empty() {
                mailbox.record_admission(recipient, source_turn_id, &admitted)
                    .await.map_err(storage_error)?;
            }
        }
        Ok(())
    }

    pub(crate) async fn mailbox_record_terminal(
        &self,
        recipient: ThreadId,
        source_turn_id: &str,
        status: AgentStatus,
        goal: Option<GoalNotificationSnapshot>,
        mut completion: Option<InterAgentCommunication>,
    ) -> CodexResult<()> {
        let Some(mailbox) = self.mailbox() else {
            return Ok(());
        };
        if let Some(communication) = completion.as_mut()
            && communication.encrypted_content.is_none()
            && communication.content.len() > 7600
        {
            let mut boundary = 7600;
            while !communication.content.is_char_boundary(boundary) {
                boundary -= 1;
            }
            communication.content.truncate(boundary);
            communication.content.push_str(&format!(
                "\n[Result preview truncated. Canonical source: agent={recipient}, turn={source_turn_id}.]"
            ));
        }
        let mut outcome = match status {
            AgentStatus::Completed(_) => Outcome::Completed,
            AgentStatus::Errored(_) => Outcome::Failed,
            AgentStatus::Interrupted | AgentStatus::Shutdown => Outcome::Cancelled,
            AgentStatus::PendingInit | AgentStatus::Running | AgentStatus::NotFound => {
                Outcome::NeedsAttention
            }
        };
        let continuation = match goal {
            Some(snapshot) => {
                let binding = GoalBinding {
                    goal_id: snapshot.binding.goal_id,
                    parent_thread_id: snapshot.binding.parent_thread_id,
                    child_thread_id: snapshot.binding.child_thread_id,
                    control_generation: snapshot.binding.control_generation,
                    revision: snapshot.revision,
                    source_turn_id: source_turn_id.to_string(),
                };
                match snapshot.phase {
                    GoalNotificationPhase::Running | GoalNotificationPhase::ContinuationPending => {
                        Continuation::PendingGoalBinding(binding)
                    }
                    GoalNotificationPhase::DeferredWithOwner => Continuation::Deferred(binding),
                    GoalNotificationPhase::ActionRequired => {
                        outcome = Outcome::NeedsAttention;
                        Continuation::Deferred(binding)
                    }
                    GoalNotificationPhase::ResultReady => Continuation::Terminal(binding),
                }
            }
            None => Continuation::None,
        };
        if let Err(error) = mailbox.record_turn_outcome(
            recipient, source_turn_id, outcome, completion, continuation,
        ).await {
            mailbox.notify_runtime_fault(recipient);
            return Err(storage_error(error));
        }
        Ok(())
    }
}
