use super::AgentControl;
use crate::agent::lifecycle::AgentLifecycleState;
use crate::agent::lifecycle::ColdMailboxItem;
use crate::agent::registry::AgentMetadata;
use crate::agent_communication::AgentCommunicationContext;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::config::Config;
use crate::session::new_submission_id;
use crate::thread_manager::ThreadManagerState;
use codex_agent_mailbox::DispatchDecision;
use codex_agent_mailbox::WorkRef;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::Op;
use std::sync::Arc;
use tokio::sync::OwnedMutexGuard;

pub(crate) struct PreparedV2AgentDelivery {
    control: AgentControl,
    state: Arc<ThreadManagerState>,
    agent_id: ThreadId,
    metadata: AgentMetadata,
    lifecycle: OwnedMutexGuard<AgentLifecycleState>,
    /// Initial spawn delivery temporarily owns exact-child metadata before the publication CAS
    /// makes that metadata parent-visible through the registry.
    metadata_is_published: bool,
}

struct RecordedSubmission {
    id: String,
    work: Option<WorkRef>,
    dispatch: DispatchDecision,
}

impl AgentControl {
    pub(crate) async fn prepare_v2_agent_delivery(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<PreparedV2AgentDelivery> {
        let state = self.upgrade()?;
        let metadata = self
            .state
            .agent_metadata_for_thread(agent_id)
            .ok_or(CodexErr::ThreadNotFound(agent_id))?;
        let lifecycle = metadata.lifecycle.lock().await;
        if !self.state.metadata_is_current(agent_id, &metadata) {
            return Err(CodexErr::ThreadNotFound(agent_id));
        }
        if state.get_thread(agent_id).await.is_ok() {
            self.touch_loaded_v2_residency(&state, agent_id).await;
        }
        Ok(PreparedV2AgentDelivery {
            control: self.clone(),
            state,
            agent_id,
            metadata,
            lifecycle,
            metadata_is_published: true,
        })
    }

    /// Prepare the first V2 delivery for a just-created child before its spawn publication CAS.
    ///
    /// The metadata is owned by `spawn_agent_internal` and deliberately has no registry entry
    /// yet. Keeping it private prevents cancellation-owned children from entering normal agent
    /// lookup, inventory, TUI, or analytics paths, while the lifecycle gate still serializes the
    /// initial delivery with the child metadata that will become public only after publication.
    pub(super) async fn prepare_provisional_v2_agent_delivery(
        &self,
        agent_id: ThreadId,
        metadata: AgentMetadata,
    ) -> CodexResult<PreparedV2AgentDelivery> {
        if metadata.agent_id != Some(agent_id) {
            return Err(CodexErr::Fatal(format!(
                "provisional V2 metadata does not belong to child {agent_id}"
            )));
        }
        let state = self.upgrade()?;
        let lifecycle = metadata.lifecycle.lock().await;
        if state.get_thread(agent_id).await.is_err() {
            return Err(CodexErr::ThreadNotFound(agent_id));
        }
        Ok(PreparedV2AgentDelivery {
            control: self.clone(),
            state,
            agent_id,
            metadata,
            lifecycle,
            metadata_is_published: false,
        })
    }

    pub(crate) async fn prepare_v2_agent_delivery_with_reload(
        &self,
        config: Config,
        agent_id: ThreadId,
    ) -> CodexResult<PreparedV2AgentDelivery> {
        let state = self.upgrade()?;
        let metadata = self
            .state
            .agent_metadata_for_thread(agent_id)
            .ok_or(CodexErr::ThreadNotFound(agent_id))?;
        // Capacity eviction never acquires this gate. Timed idle eviction follows this same
        // reload-then-lifecycle order, so residency work cannot invert lifecycle locks.
        let _reload = metadata.lifecycle.lock_reload().await;
        let mut lifecycle = metadata.lifecycle.lock().await;
        if !self.state.metadata_is_current(agent_id, &metadata) {
            return Err(CodexErr::ThreadNotFound(agent_id));
        }
        if state.get_thread(agent_id).await.is_err() {
            drop(lifecycle);
            let residency_slot = self
                .reserve_v2_residency_slot(&state, &config, Some(agent_id))
                .await?;
            lifecycle = metadata.lifecycle.lock().await;
            if !self.state.metadata_is_current(agent_id, &metadata) {
                return Err(CodexErr::ThreadNotFound(agent_id));
            }
            self.ensure_v2_agent_loaded_under_lifecycle(
                &state,
                config,
                agent_id,
                &metadata,
                residency_slot,
                &mut lifecycle,
            )
            .await?;
        } else {
            metadata.clear_cold_status();
            self.touch_loaded_v2_residency(&state, agent_id).await;
            self.restore_cold_mail_to_loaded_thread(&state, agent_id, &mut lifecycle)
                .await?;
        }
        Ok(PreparedV2AgentDelivery {
            control: self.clone(),
            state,
            agent_id,
            metadata,
            lifecycle,
            metadata_is_published: true,
        })
    }

    pub(crate) async fn uses_v2_lifecycle(
        &self,
        state: &Arc<ThreadManagerState>,
        agent_id: ThreadId,
    ) -> bool {
        match state.get_thread(agent_id).await {
            Ok(thread) => thread.multi_agent_version() == Some(MultiAgentVersion::V2),
            Err(_) => self
                .state
                .cold_status(agent_id, /*live_thread*/ None)
                .is_some(),
        }
    }

    pub(super) async fn restore_cold_mail_to_loaded_thread(
        &self,
        state: &Arc<ThreadManagerState>,
        agent_id: ThreadId,
        lifecycle: &mut AgentLifecycleState,
    ) -> CodexResult<()> {
        let thread = state.get_thread(agent_id).await?;
        let mut entries = Vec::new();
        let mut receive_ids = Vec::new();
        for item in lifecycle.take_cold_mail() {
            receive_ids.push(item.receive_id);
            entries.push(thread.session.input_queue.mailbox_entry_with_metadata(
                item.communication,
                item.sequence,
                item.enqueued_at_ms,
            ));
        }
        if entries.is_empty() {
            return Ok(());
        }
        thread
            .session
            .input_queue
            .enqueue_mailbox_entries(entries)
            .await;
        for receive_id in receive_ids.into_iter().flatten() {
            crate::agent_communication::emit_agent_communication_receive(&receive_id);
        }
        Ok(())
    }
}

impl PreparedV2AgentDelivery {
    pub(crate) async fn inject_response_items(self, items: Vec<ResponseItem>) -> CodexResult<()> {
        if !self
            .control
            .state
            .metadata_is_current(self.agent_id, &self.metadata)
        {
            return Err(CodexErr::ThreadNotFound(self.agent_id));
        }
        let thread = self.state.get_thread(self.agent_id).await?;
        let _residency_transition = thread.session.input_queue.begin_residency_activity().await;
        let thread_is_current = self
            .state
            .get_thread(self.agent_id)
            .await
            .is_ok_and(|current| Arc::ptr_eq(&current, &thread));
        if !thread_is_current
            || !self
                .control
                .state
                .metadata_is_current(self.agent_id, &self.metadata)
        {
            return Err(CodexErr::ThreadNotFound(self.agent_id));
        }
        if !thread.is_running() {
            return Err(CodexErr::InternalAgentDied);
        }
        thread
            .inject_response_items_with_residency_transition_held(items)
            .await
    }

    async fn record_submission(
        &self,
        communication: &mut InterAgentCommunication,
        context: &AgentCommunicationContext,
        recipient_config: Option<&Config>,
    ) -> CodexResult<RecordedSubmission> {
        let mut submission_id = new_submission_id();
        let accepted = if let Some(recipient_config) = recipient_config {
            self.control
                .mailbox_accept_native_submission(
                self.agent_id,
                context,
                communication,
                &mut submission_id,
                recipient_config,
            )
                .await?
        } else if context.native_key.is_some() {
            return Err(CodexErr::UnsupportedOperation(format!(
                "cannot verify the resolved configuration for mailbox recipient {}; the original input was not submitted",
                self.agent_id
            )));
        } else {
            None
        };
        let (work, dispatch) = accepted
            .map_or((None, DispatchDecision::Dispatch), |(work, decision)| {
                (Some(work), decision)
            });
        if dispatch != DispatchDecision::Dispatch {
            return Ok(RecordedSubmission {
                id: submission_id,
                work,
                dispatch,
            });
        }
        let op = Op::InterAgentCommunication {
            communication: communication.clone(),
        };
        self.state.record_submitted_op(self.agent_id, &op);
        if crate::agent_communication::logging_enabled() {
            let emit = crate::agent_communication::emit_agent_communication_send;
            emit(&submission_id, context, communication, self.agent_id);
        }
        Ok(RecordedSubmission {
            id: submission_id,
            work,
            dispatch,
        })
    }

    pub(crate) async fn config_snapshot(&self) -> CodexResult<ThreadConfigSnapshot> {
        Ok(self
            .state
            .get_thread(self.agent_id)
            .await?
            .config_snapshot()
            .await)
    }

    pub(crate) async fn send(
        self,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        interrupt: bool,
    ) -> CodexResult<String> {
        self.control
            .ensure_execution_capacity_for_turn_start(self.agent_id, communication.trigger_turn)
            .await?;
        self.send_after_capacity_check(communication, context, interrupt)
            .await
    }

    pub(super) fn send_after_capacity_check(
        mut self,
        mut communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        interrupt: bool,
    ) -> futures::future::BoxFuture<'static, CodexResult<String>> {
        Box::pin(async move {
            if self.metadata_is_published
                && !self
                    .control
                    .state
                    .metadata_is_current(self.agent_id, &self.metadata)
            {
                return Err(CodexErr::ThreadNotFound(self.agent_id));
            }
            if interrupt || communication.trigger_turn {
                self.lifecycle.invalidate_terminal_idle_unload();
            }
            if let Ok(thread) = self.state.get_thread(self.agent_id).await {
                let _residency_transition =
                    thread.session.input_queue.begin_residency_activity().await;
                let thread_is_current = self
                    .state
                    .get_thread(self.agent_id)
                    .await
                    .is_ok_and(|current| Arc::ptr_eq(&current, &thread));
                if thread_is_current {
                    if !thread.is_running() {
                        return Err(CodexErr::InternalAgentDied);
                    }
                    let recipient_config = thread.session.get_config().await;
                    self.control
                        .mailbox_require_native_recipient_compatible(
                            self.agent_id,
                            &context,
                            &communication,
                            &recipient_config,
                            self.metadata.agent_role.as_deref(),
                        )
                        .await?;
                    let submission = self
                        .record_submission(&mut communication, &context, Some(&recipient_config))
                        .await?;
                    match submission.dispatch {
                        DispatchDecision::AlreadyDispatched => return Ok(submission.id),
                        DispatchDecision::RecoveryNeeded => {
                            return Err(CodexErr::UnsupportedOperation(format!(
                                "accepted work {} requires native recovery; its original input is retained and was not resent",
                                submission
                                    .work
                                    .as_ref()
                                    .map(|work| work.id.as_str())
                                    .unwrap_or("unknown")
                            )));
                        }
                        DispatchDecision::Dispatch => {}
                    }
                    if interrupt {
                        self.state
                            .record_submitted_op(self.agent_id, &Op::Interrupt);
                        thread.session.interrupt_task().await;
                    }
                    let send = crate::session::inter_agent_communication;
                    send(&thread.session, submission.id.clone(), communication).await;
                    if let Some(work) = &submission.work {
                        self.control.mailbox_dispatched(work).await?;
                    }
                    return Ok(submission.id);
                }
            }
            if communication.trigger_turn {
                return Err(CodexErr::ThreadNotFound(self.agent_id));
            }
            if context.native_key.is_some() {
                return Err(CodexErr::UnsupportedOperation(format!(
                    "cannot verify the resolved configuration for mailbox recipient {}; the original input was not submitted",
                    self.agent_id
                )));
            }
            let submission = self
                .record_submission(&mut communication, &context, None)
                .await?;
            match submission.dispatch {
                DispatchDecision::AlreadyDispatched => return Ok(submission.id),
                DispatchDecision::RecoveryNeeded => {
                    return Err(CodexErr::UnsupportedOperation(format!(
                        "accepted work {} requires native recovery; its original input is retained and was not resent",
                        submission
                            .work
                            .as_ref()
                            .map(|work| work.id.as_str())
                            .unwrap_or("unknown")
                    )));
                }
                DispatchDecision::Dispatch => {}
            }
            let sequence = self.lifecycle.next_cold_mailbox_sequence();
            self.lifecycle.push_cold_mail(ColdMailboxItem {
                receive_id: Some(submission.id.clone()),
                communication,
                sequence: Some(sequence),
                enqueued_at_ms: Some(crate::session::input_queue::current_time_ms()),
            });
            if let Some(work) = &submission.work {
                self.control.mailbox_dispatched(work).await?;
            }
            Ok(submission.id)
        })
    }
}
