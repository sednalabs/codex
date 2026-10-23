//! The narrow native-runtime adapter for the independently stored V2 mailbox.
//!
//! The mailbox never owns agent membership, execution, or approval policy. Native lifecycle
//! boundaries supply accepted inputs and outcomes; canonical history owns observation custody.

use super::AgentControl;
use crate::agent::AgentStatus;
use crate::agent::goal_notifications::GoalNotificationPhase;
use crate::agent::goal_notifications::GoalNotificationSnapshot;
use crate::agent::role::apply_role_to_config;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::config::Config;
use codex_agent_mailbox::ActivationDisposition;
use codex_agent_mailbox::Continuation;
use codex_agent_mailbox::ContinuationState;
use codex_agent_mailbox::DispatchDecision;
use codex_agent_mailbox::DispatchState;
use codex_agent_mailbox::Envelope;
use codex_agent_mailbox::GoalBinding;
use codex_agent_mailbox::Mailbox;
use codex_agent_mailbox::NativeWork;
use codex_agent_mailbox::Outcome;
use codex_agent_mailbox::RecoveryEvidence;
use codex_agent_mailbox::WorkRef;
use codex_agent_mailbox::WorkSnapshot;
use codex_features::Feature;
use codex_protocol::ResponseItemId;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::ThreadGoalStatus;
use std::collections::HashSet;
use std::sync::Arc;

fn storage_error(error: impl std::fmt::Display) -> CodexErr {
    // The store does not log payloads. Keep its typed diagnostic in the error chain presented
    // to the native caller rather than treating failed acceptance as successful delivery.
    CodexErr::Fatal(format!("agent mailbox storage failed: {error}"))
}

fn goal_binding_matches_native(
    binding: &GoalBinding,
    incarnation: ThreadId,
    snapshot: &GoalNotificationSnapshot,
) -> bool {
    binding.incarnation == Some(incarnation)
        && binding.goal_id == snapshot.binding.goal_id
        && binding.parent_thread_id == snapshot.binding.parent_thread_id
        && binding.child_thread_id == snapshot.binding.child_thread_id
        && binding.control_generation == snapshot.binding.control_generation
}

async fn recover_exact_unadmitted_submission(
    mailbox: &Mailbox,
    recipient: ThreadId,
    submission_id: &str,
) -> CodexResult<bool> {
    let Some(work) = mailbox
        .work_for_submission(recipient, submission_id)
        .await
        .map_err(storage_error)?
    else {
        return Ok(false);
    };
    let input = mailbox.accepted_input(&work).await.map_err(storage_error)?;
    let input_id = input
        .id
        .as_ref()
        .ok_or_else(|| storage_error("accepted mailbox submission has no stable input ID"))?
        .to_string();
    if mailbox
        .unadmitted_input_ids(recipient, std::slice::from_ref(&input_id))
        .await
        .map_err(storage_error)?
        .is_empty()
    {
        return Ok(false);
    }

    mailbox
        .mark_recovery_needed(&work, RecoveryEvidence::DeliveryUnavailable)
        .await
        .map_err(storage_error)?;
    let dispatch_state = mailbox.dispatch_state(&work).await.map_err(storage_error)?;
    if dispatch_state != DispatchState::RecoveryNeeded {
        return Err(storage_error(
            "exact unadmitted mailbox submission could not enter recovery-needed state",
        ));
    }
    Ok(true)
}

impl AgentControl {
    pub(crate) fn mailbox_config_compatible(config: &Config) -> bool {
        config.multi_agent_version_from_features() == MultiAgentVersion::V2
            && config.features.enabled(Feature::AgentMailbox)
    }

    pub(super) fn mailbox_config_incompatibility_error(
        recipient: &str,
        role: Option<&str>,
    ) -> CodexErr {
        let target = role.map_or_else(
            || recipient.to_string(),
            |role| format!("role `{role}` ({recipient})"),
        );
        CodexErr::UnsupportedOperation(format!(
            "tracked agent delivery to {target} requires MultiAgentV2 and agent_mailbox to be enabled"
        ))
    }

    pub(crate) async fn mailbox_initialize(&self, config: &Config) -> CodexResult<()> {
        if !Self::mailbox_config_compatible(config) {
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

    /// Resolve compatibility without reloading a cold recipient or restoring its queued input.
    /// The retained role is the authoritative per-agent config selector while the runtime is cold.
    pub(crate) async fn mailbox_recipient_config_compatible(
        &self,
        recipient: ThreadId,
        fallback_config: &Config,
    ) -> CodexResult<bool> {
        let state = self.upgrade()?;
        if let Ok(thread) = state.get_thread(recipient).await {
            let config = thread.session.get_config().await;
            return Ok(Self::mailbox_config_compatible(&config));
        }

        let metadata = self
            .state
            .agent_metadata_for_thread(recipient)
            .ok_or(CodexErr::ThreadNotFound(recipient))?;
        let mut resolved_config = fallback_config.clone();
        apply_role_to_config(&mut resolved_config, metadata.agent_role.as_deref())
            .await
            .map_err(CodexErr::InvalidRequest)?;
        Ok(Self::mailbox_config_compatible(&resolved_config))
    }

    pub(crate) fn mailbox(&self) -> Option<Arc<Mailbox>> {
        self.mailbox.get().cloned()
    }

    pub(crate) async fn mailbox_interrupt_resumed_dispatches(
        &self,
        issuer: ThreadId,
    ) -> CodexResult<()> {
        let Some(mailbox) = self.mailbox() else {
            return Ok(());
        };
        // Indexed bounded batches, scoped to the exact native session being resumed.
        // Other loaded issuers and producer continuation custody are untouched.
        while mailbox
            .interrupt_issuer_dispatches(issuer, 100)
            .await
            .map_err(storage_error)?
            != 0
        {}
        Ok(())
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
        recipient_config: &Config,
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
        // Only a mailbox-enabled invocation supplies this frozen native identity. A shared
        // tree may contain a feature-disabled session; opening the store elsewhere does not
        // opt its stock tools into mailbox capture.
        let Some(native_key) = context.native_key.as_deref() else {
            return Ok(None);
        };
        self.mailbox_require_native_recipient_compatible(
            agent,
            context,
            communication,
            recipient_config,
            /*recipient_role*/ None,
        )
        .await?;
        if let Some(existing) = mailbox
            .work_for_native_key(context.sender_thread_id, native_key)
            .await
            .map_err(storage_error)?
        {
            *submission_id = existing.submission_id;
        }
        let work = mailbox
            .accept_work(NativeWork {
                native_key: native_key.to_string(),
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

    pub(super) async fn mailbox_require_native_recipient_compatible(
        &self,
        recipient: ThreadId,
        context: &AgentCommunicationContext,
        communication: &InterAgentCommunication,
        recipient_config: &Config,
        recipient_role: Option<&str>,
    ) -> CodexResult<()> {
        if !matches!(
            context.kind,
            AgentCommunicationKind::Spawn | AgentCommunicationKind::Followup
        ) || context.native_key.is_none()
            || Self::mailbox_config_compatible(recipient_config)
        {
            return Ok(());
        }

        let incompatibility = Self::mailbox_config_incompatibility_error(
            &format!("recipient {recipient}"),
            recipient_role,
        );
        let Some(mailbox) = self.mailbox() else {
            return Err(incompatibility);
        };
        if let Some(native_key) = context.native_key.as_deref()
            && let Some(work) = mailbox
                .work_for_native_key(context.sender_thread_id, native_key)
                .await
                .map_err(storage_error)?
        {
            let mut accepted = mailbox.accepted_input(&work).await.map_err(storage_error)?;
            let mut requested = communication.clone();
            accepted.id = None;
            requested.id = None;
            if work.agent != recipient || accepted != requested {
                return Err(CodexErr::InvalidRequest(format!(
                    "native mailbox key `{native_key}` was reused with a different recipient or input"
                )));
            }
            mailbox
                .mark_recovery_needed(&work, RecoveryEvidence::RecipientIncompatible)
                .await
                .map_err(storage_error)?;
            return Err(CodexErr::UnsupportedOperation(format!(
                "accepted work {} for {recipient} is incompatible with the current recipient configuration; its original input is retained and recovery is required",
                work.id
            )));
        }
        Err(incompatibility)
    }

    pub(super) async fn mailbox_dispatched(&self, work: &WorkRef) -> CodexResult<()> {
        if let Some(mailbox) = self.mailbox()
            && let Err(mark_error) = mailbox.mark_dispatched(work).await
        {
            // Native input is already queued. A producer may have durably captured its
            // pre-admission failure while this fast sender was finishing; preserve that
            // accepted recovery receipt instead of making spawn roll the child back.
            match mailbox.dispatch_state(work).await {
                Ok(DispatchState::Dispatched | DispatchState::RecoveryNeeded) => return Ok(()),
                Ok(state) => {
                    return Err(storage_error(format!(
                        "accepted work {} could not be marked dispatched (state {state:?}): {mark_error}",
                        work.id
                    )));
                }
                Err(state_error) => {
                    return Err(storage_error(format!(
                        "accepted work {} could not be marked dispatched and its retained state could not be verified: {mark_error}; {state_error}",
                        work.id
                    )));
                }
            }
        }
        Ok(())
    }

    pub(super) async fn mailbox_mark_recipient_incompatible(
        &self,
        work: &WorkRef,
    ) -> CodexResult<()> {
        if let Some(mailbox) = self.mailbox() {
            mailbox
                .mark_recovery_needed(work, RecoveryEvidence::RecipientIncompatible)
                .await
                .map_err(storage_error)?;
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
        if metadata
            .agent_path
            .as_ref()
            .is_none_or(|path| path.is_root())
        {
            return Ok(denied("root_activation_is_not_allowed"));
        }
        let state = self.upgrade()?;
        // Reloading is not a side effect of accepting mail. An unloaded recipient keeps durable
        // custody until it is explicitly resumed through its normal native lifecycle.
        let Ok(thread) = state.get_thread(recipient).await else {
            return Ok(denied("recipient_is_not_live"));
        };
        let config = thread.session.get_config().await;
        if config.multi_agent_version_from_features() != MultiAgentVersion::V2
            || !config.features.enabled(Feature::AgentMailbox)
        {
            return Ok(denied("recipient_mailbox_not_enabled"));
        }
        if !config.features.enabled(Feature::AgentMailboxIdleActivation) {
            return Ok(denied("recipient_idle_activation_not_enabled"));
        }
        if !thread.is_running() {
            return Ok(denied("recipient_runtime_is_not_accepting_input"));
        }
        if thread.session.active_turn.lock().await.is_some() {
            return Ok((ActivationDisposition::AlreadyActive, None));
        }
        if thread
            .session
            .services
            .thread_extension_data
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
            if mailbox
                .get_message(recipient, id.as_str())
                .await
                .map_err(storage_error)?
                .is_some()
            {
                mailbox
                    .mark_canonical(recipient, id.as_str())
                    .await
                    .map_err(storage_error)?;
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
            let page = mailbox
                .pending_page(recipient, cursor, 100)
                .await
                .map_err(storage_error)?;
            for envelope in &page.items {
                if envelope
                    .communication
                    .id
                    .as_ref()
                    .is_some_and(|id| canonical_ids.contains(id))
                {
                    mailbox
                        .mark_canonical(recipient, &envelope.id)
                        .await
                        .map_err(storage_error)?;
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
        let unread = mailbox
            .unadmitted_canonical_ids(recipient, 4096)
            .await
            .map_err(storage_error)?
            .into_iter()
            .collect::<HashSet<_>>();
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
        Ok(mailbox
            .automatic_inputs(recipient, source_turn_id, 2)
            .await
            .map_err(storage_error)?
            .into_iter()
            .map(|envelope| envelope.communication)
            .collect())
    }

    pub(crate) async fn mailbox_reconcile_pending_work(
        &self,
        work: &WorkSnapshot,
    ) -> CodexResult<bool> {
        let Some(mailbox) = self.mailbox() else {
            return Ok(false);
        };
        let state = self.upgrade()?;
        let (can_proceed, goal_owner) = match state.get_thread(work.work_ref.agent).await {
            Ok(thread) if thread.is_running() => {
                let active = { thread.session.active_turn.lock().await.is_some() };
                let can_proceed = active
                    || thread.session.input_queue.has_pending_mailbox_items().await
                    || thread.session.input_queue.has_pending_terminal_finalizers()
                    || thread.session.has_outstanding_durable_sleep();
                let goal_owner = thread
                    .session
                    .services
                    .thread_extension_data
                    .get::<crate::agent::goal_notifications::GoalNotificationStore>()
                    .and_then(|store| {
                        store
                            .snapshot()
                            .map(|snapshot| (store.incarnation(), snapshot))
                    });
                (can_proceed, goal_owner)
            }
            Ok(_) | Err(_) => (false, None),
        };
        let unresolved = work.continuations.iter().find(|continuation| {
            matches!(
                continuation.state,
                ContinuationState::PendingGoalBinding | ContinuationState::Deferred
            )
        });
        let evidence = if let Some(continuation) = unresolved {
            if goal_owner.as_ref().is_some_and(|(incarnation, snapshot)| {
                goal_binding_matches_native(&continuation.binding, *incarnation, snapshot)
                    && ((snapshot.status == ThreadGoalStatus::Active
                        && matches!(
                            snapshot.phase,
                            GoalNotificationPhase::Running
                                | GoalNotificationPhase::ContinuationPending
                                | GoalNotificationPhase::DeferredWithOwner
                        ))
                        || (snapshot.phase == GoalNotificationPhase::ResultReady && can_proceed))
            }) {
                return Ok(false);
            }
            // A cold, restarted, replaced, or action-required producer cannot prove custody
            // of this exact old logical continuation. Keep its binding and original refs;
            // return recovery-needed instead of waiting for a producer that cannot advance it.
            RecoveryEvidence::ContinuationUnowned {
                binding: continuation.binding.clone(),
            }
        } else if can_proceed {
            return Ok(false);
        } else {
            RecoveryEvidence::DeliveryUnavailable
        };
        // This only changes recovery disposition, never replays input or invents a result.
        // The store rechecks terminal/continuation custody atomically against this native read.
        mailbox
            .mark_recovery_needed(&work.work_ref, evidence)
            .await
            .map_err(storage_error)
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
        let ids = message_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        for candidates in ids.chunks(100) {
            let admitted = mailbox
                .unadmitted_input_ids(recipient, candidates)
                .await
                .map_err(storage_error)?;
            if !admitted.is_empty() {
                mailbox
                    .record_admission(recipient, source_turn_id, &admitted)
                    .await
                    .map_err(storage_error)?;
            }
        }
        Ok(())
    }

    pub(crate) async fn mailbox_record_terminal(
        &self,
        recipient: ThreadId,
        source_turn_id: &str,
        status: AgentStatus,
        goal: Option<(ThreadId, GoalNotificationSnapshot)>,
        mut completion: Option<InterAgentCommunication>,
    ) -> CodexResult<bool> {
        let Some(mailbox) = self.mailbox() else {
            return Ok(false);
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
            Some((incarnation, snapshot)) => {
                let binding = GoalBinding {
                    goal_id: snapshot.binding.goal_id,
                    parent_thread_id: snapshot.binding.parent_thread_id,
                    child_thread_id: snapshot.binding.child_thread_id,
                    control_generation: snapshot.binding.control_generation,
                    incarnation: Some(incarnation),
                };
                if matches!(outcome, Outcome::Failed | Outcome::Cancelled) {
                    Continuation::Terminal(binding)
                } else {
                    match snapshot.phase {
                        GoalNotificationPhase::Running
                        | GoalNotificationPhase::ContinuationPending => {
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
            }
            None => Continuation::None,
        };
        let receipt = match mailbox
            .record_turn_outcome(recipient, source_turn_id, outcome, completion, continuation)
            .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                mailbox.notify_runtime_fault(recipient);
                return Err(storage_error(error));
            }
        };
        if receipt.no_matching_work {
            match recover_exact_unadmitted_submission(&mailbox, recipient, source_turn_id).await {
                Ok(captured) => Ok(captured),
                Err(error) => {
                    mailbox.notify_runtime_fault(recipient);
                    Err(error)
                }
            }
        } else {
            Ok(true)
        }
    }
}

#[cfg(test)]
#[path = "mailbox_tests.rs"]
mod tests;
