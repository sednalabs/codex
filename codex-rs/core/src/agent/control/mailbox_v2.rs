//! Durable custody for the native V2 adapter, independent of resident queues.
//!
//! The protected lifecycle gate still owns recipient delivery. SQLite owns accepted
//! identities; observing or unloading an in-memory queue never consumes that custody.

use super::AgentControl;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::config::Config;
use codex_agent_mailbox::DispatchDecision;
use codex_agent_mailbox::ActivationDecision;
use codex_agent_mailbox::ActivationDisposition;
use codex_agent_mailbox::Envelope;
use codex_agent_mailbox::Mailbox;
use codex_agent_mailbox::MessageDraft;
use codex_agent_mailbox::NativeWork;
use codex_agent_mailbox::WorkRef;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use std::collections::HashSet;
use std::sync::Arc;

pub(crate) fn storage_error(error: impl std::fmt::Display) -> CodexErr {
    CodexErr::Fatal(format!("agent mailbox storage failed: {error}"))
}

pub(crate) fn compatible(config: &Config) -> bool {
    config.multi_agent_version_from_features() == MultiAgentVersion::V2
        && config.features.enabled(Feature::AgentMailbox)
}

pub(super) struct AcceptedDelivery {
    pub(super) submission_id: String,
    pub(super) work: Option<WorkRef>,
    pub(super) already_dispatched: bool,
    pub(super) activation: Option<Envelope>,
}

impl AgentControl {
    /// Use the executing session's captured configuration, including during private
    /// startup or after its retained thread handle has left the manager registry.
    pub(crate) async fn mailbox_v2_open(&self, config: &Config) -> Result<Option<Arc<Mailbox>>> {
        if !compatible(config) {
            return Ok(None);
        }
        self.mailbox_v2
            .get_or_try_init(|| async {
                Mailbox::open(config.sqlite_config())
                    .await
                    .map(Arc::new)
                    .map_err(storage_error)
            })
            .await
            .map(|mailbox| Some(Arc::clone(mailbox)))
    }

    pub(crate) fn mailbox_v2(&self) -> Option<Arc<Mailbox>> {
        self.mailbox_v2.get().cloned()
    }

    pub(crate) async fn mailbox_v2_outcome_activation_requested(&self, recipient: ThreadId) -> bool {
        let Ok(state) = self.upgrade() else { return false; };
        let Ok(thread) = state.get_thread(recipient).await else { return false; };
        let config = thread.session.mailbox_v2_config_snapshot().await;
        compatible(&config) && config.features.enabled(Feature::AgentMailboxIdleActivation)
    }

    /// Called under the protected recipient lifecycle gate, before interrupt,
    /// telemetry publication, or submission to the recipient.
    pub(super) async fn mailbox_v2_accept_delivery(
        &self,
        recipient: ThreadId,
        config: &Config,
        communication: &mut InterAgentCommunication,
        context: &AgentCommunicationContext,
        submission_id: String,
    ) -> Result<AcceptedDelivery> {
        if let Some(id) = context.durable_message_id.as_deref() {
            let mailbox = self.mailbox_v2_open(config).await?.ok_or_else(|| storage_error("outcome recipient disabled its durable mailbox"))?;
            let envelope = mailbox.get_message(recipient, id).await.map_err(storage_error)?
                .ok_or_else(|| storage_error("accepted outcome response is missing"))?;
            if envelope.sender != context.sender_thread_id || communication.id.as_ref().map(|id| id.as_str()) != Some(id) {
                return Err(storage_error("accepted outcome delivery identity changed"));
            }
            *communication = envelope.communication.clone();
            return self.mailbox_v2_plan_message_delivery(recipient, communication, mailbox, envelope).await;
        }
        let Some(native_key) = context.native_key.as_deref() else {
            return Ok(AcceptedDelivery {
                submission_id,
                work: None,
                already_dispatched: false,
                activation: None,
            });
        };
        let mailbox = self.mailbox_v2_open(config).await?.ok_or_else(|| {
            CodexErr::UnsupportedOperation(
                "durable native delivery requires MultiAgentV2 and agent_mailbox on the recipient"
                    .to_string(),
            )
        })?;
        if context.is_native_work() {
            let original = mailbox
                .work_for_native_key(context.sender_thread_id, native_key)
                .await
                .map_err(storage_error)?;
            communication.id = None;
            let work = mailbox
                .accept_work(NativeWork {
                    native_key: native_key.to_string(),
                    issuer: context.sender_thread_id,
                    agent: recipient,
                    submission_id: original.map_or(submission_id, |work| work.submission_id),
                    input: communication.clone(),
                })
                .await
                .map_err(storage_error)?;
            *communication = mailbox.accepted_input(&work).await.map_err(storage_error)?;
            let decision = mailbox.begin_dispatch(&work).await.map_err(storage_error)?;
            if decision == DispatchDecision::RecoveryNeeded {
                return Err(CodexErr::UnsupportedOperation(format!(
                    "accepted work {} requires delivery reconciliation; its original input is retained",
                    work.id
                )));
            }
            return Ok(AcceptedDelivery {
                submission_id: work.submission_id.clone(),
                work: Some(work),
                already_dispatched: decision == DispatchDecision::AlreadyDispatched,
                activation: None,
            });
        }
        let kind = context.message_kind.unwrap_or_default();
        let envelope = mailbox
            .accept_message(MessageDraft {
                invocation_key: format!("{}:{recipient}:{native_key}", context.sender_thread_id),
                sender: context.sender_thread_id,
                recipient,
                communication: communication.clone(),
                kind,
                activate_if_idle: communication.resolved_intent().wakes_native_wait()
                    && config.features.enabled(Feature::AgentMailboxIdleActivation),
                work_ref: None,
                reply_to: context.reply_to.clone(),
                supersedes: None,
                legacy_attention: communication.resolved_intent().wakes_native_wait(),
            })
            .await
            .map_err(storage_error)?;
        *communication = envelope.communication.clone();
        self.mailbox_v2_plan_message_delivery(recipient, communication, mailbox, envelope).await
    }

    async fn mailbox_v2_plan_message_delivery(
        &self,
        recipient: ThreadId,
        communication: &mut InterAgentCommunication,
        mailbox: Arc<Mailbox>,
        envelope: Envelope,
    ) -> Result<AcceptedDelivery> {
        let activation = if envelope.activate_if_idle {
            match mailbox.begin_activation(envelope.sender, &envelope.id).await.map_err(storage_error)? {
                ActivationDecision::AlreadyDecided(ActivationDisposition::Activated) => {
                    return Ok(AcceptedDelivery {
                        submission_id: envelope.id,
                        work: None,
                        already_dispatched: true,
                        activation: None,
                    });
                }
                ActivationDecision::AlreadyDecided(_) => None,
                ActivationDecision::RecoveryNeeded => return Err(CodexErr::UnsupportedOperation(
                    "accepted mailbox activation requires reconciliation".to_string(),
                )),
                ActivationDecision::Activate => {
                    let thread = self.upgrade()?.get_thread(recipient).await?;
                    let is_child = self.ensure_agent_known(recipient)?.agent_path.as_ref()
                        .is_some_and(|path| !path.is_root());
                    let idle = thread.session.active_turn.lock().await.is_none();
                    let goal_allows_activation = match thread.session.services.state_db.as_ref() {
                        Some(state_db) => state_db.thread_goals().get_thread_goal(recipient).await
                            .map_err(storage_error)?
                            .is_none_or(|goal| goal.status == codex_state::ThreadGoalStatus::Active),
                        None => false,
                    };
                    if is_child && idle && goal_allows_activation
                        && self.ensure_execution_capacity_for_turn_start(recipient, /*starts_turn*/ true).await.is_ok()
                    {
                        communication.trigger_turn = true;
                        Some(envelope.clone())
                    } else {
                        mailbox.record_activation(envelope.sender, &envelope.id,
                            if !idle { ActivationDisposition::AlreadyActive } else { ActivationDisposition::Denied },
                        ).await.map_err(storage_error)?;
                        None
                    }
                }
            }
        } else {
            None
        };
        Ok(AcceptedDelivery {
            submission_id: envelope.id,
            work: None,
            already_dispatched: false,
            activation,
        })
    }

    pub(super) async fn mailbox_v2_finish_activation(&self, envelope: &Envelope) -> Result<()> {
        self.mailbox_v2().ok_or_else(|| storage_error("activation lost its mailbox"))?
            .record_activation(envelope.sender, &envelope.id, ActivationDisposition::Activated)
            .await.map_err(storage_error)
    }

    pub(super) async fn mailbox_v2_finish_dispatch(&self, work: &WorkRef) -> Result<()> {
        self.mailbox_v2()
            .ok_or_else(|| storage_error("accepted work lost its mailbox handle"))?
            .mark_dispatched(work)
            .await
            .map_err(storage_error)
    }

    /// Non-consuming bounded replay. Provider admission, not a queue drain,
    /// acknowledges native input. Canonical message bodies are retained for compaction.
    pub(crate) async fn mailbox_v2_pending_inputs(
        &self,
        recipient: ThreadId,
        config: &Config,
        include_canonical: bool,
    ) -> Result<Vec<InterAgentCommunication>> {
        let Some(mailbox) = self.mailbox_v2_open(config).await? else {
            return Ok(Vec::new());
        };
        let page = mailbox
            .pending_page(recipient, /*after_seq*/ None, /*limit*/ 100)
            .await
            .map_err(storage_error)?;
        if page.next_cursor.is_some() {
            return Err(storage_error("pending recipient inputs exceed bounded capacity"));
        }
        let mut messages = page
            .items
            .into_iter()
            .filter(|message| include_canonical || !message.canonical)
            .map(|message| message.communication)
            .collect::<Vec<_>>();
        for id in mailbox
            .unadmitted_native_input_ids(recipient, /*limit*/ 100)
            .await
            .map_err(storage_error)?
        {
            if let Some(work) = mailbox
                .work_for_input_id(recipient, &id)
                .await
                .map_err(storage_error)?
            {
                messages.push(mailbox.accepted_input(&work).await.map_err(storage_error)?);
            }
        }
        let mut seen = HashSet::new();
        messages.retain(|message| message.id.as_ref().is_some_and(|id| seen.insert(id.clone())));
        if messages.len() > 100 {
            return Err(storage_error("pending recipient inputs exceed bounded capacity"));
        }
        Ok(messages)
    }

    pub(crate) async fn mailbox_v2_input_is_durable(
        &self,
        recipient: ThreadId,
        id: &str,
        config: &Config,
    ) -> Result<bool> {
        let Some(mailbox) = self.mailbox_v2_open(config).await? else {
            return Ok(false);
        };
        if mailbox.get_message(recipient, id).await.map_err(storage_error)?.is_some() {
            return Ok(true);
        }
        mailbox
            .work_for_input_id(recipient, id)
            .await
            .map(|work| work.is_some())
            .map_err(storage_error)
    }

    pub(crate) async fn mailbox_v2_confirm_canonical(
        &self,
        recipient: ThreadId,
        id: &str,
        config: &Config,
    ) -> Result<()> {
        let Some(mailbox) = self.mailbox_v2_open(config).await? else {
            return Ok(());
        };
        if mailbox.get_message(recipient, id).await.map_err(storage_error)?.is_some() {
            mailbox.mark_canonical(recipient, id).await.map_err(storage_error)?;
        }
        Ok(())
    }

    pub(crate) async fn mailbox_v2_record_admission(
        &self,
        recipient: ThreadId,
        turn_id: &str,
        ids: &[String],
        config: &Config,
    ) -> Result<()> {
        let Some(mailbox) = self.mailbox_v2_open(config).await? else {
            return Ok(());
        };
        // A long history can contain unrelated IDs. Bound individual storage queries,
        // then atomically record only the exact mailbox-owned provider snapshot.
        let mut selected = Vec::new();
        for ids in ids.chunks(100) {
            selected.extend(mailbox.unadmitted_input_ids(recipient, ids).await.map_err(storage_error)?);
        }
        if !selected.is_empty() {
            mailbox.record_admission(recipient, turn_id, &selected).await.map_err(storage_error)?;
        }
        Ok(())
    }

    pub(crate) async fn mailbox_v2_native_work(
        &self,
        issuer: ThreadId,
        key: &str,
    ) -> Result<Option<WorkRef>> {
        match self.mailbox_v2() {
            Some(mailbox) => mailbox.work_for_native_key(issuer, key).await.map_err(storage_error),
            None => Ok(None),
        }
    }

    /// Queue the exact response already committed alongside its outcome. An unloaded
    /// parent keeps SQLite custody; its next turn reconciles the stable body.
    pub(crate) async fn mailbox_v2_deliver_outcomes(
        &self,
        recipient: ThreadId,
        message_ids: &[String],
    ) -> Result<()> {
        let mailbox = self.mailbox_v2().ok_or_else(|| storage_error("outcome lost its mailbox"))?;
        // Keep accepted bodies in SQLite when the parent is cold. A resident
        // parent is delivered under the same lifecycle/residency gates as tools.
        if self.upgrade()?.get_thread(recipient).await.is_err() {
            return Ok(());
        }
        for id in message_ids {
            let envelope = mailbox.get_message(recipient, id).await.map_err(storage_error)?
                .ok_or_else(|| storage_error("durable outcome response is missing"))?;
            let context = AgentCommunicationContext::new(AgentCommunicationKind::Result, envelope.sender)
                .with_durable_message_id(id.clone());
            self.prepare_v2_agent_delivery(recipient).await?
                .send(envelope.communication, context, /*interrupt*/ false).await?;
        }
        Ok(())
    }
}
