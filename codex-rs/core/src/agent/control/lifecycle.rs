use super::AgentControl;
use crate::agent::lifecycle::AgentLifecycleState;
use crate::agent::lifecycle::ColdMailboxItem;
use crate::agent::registry::AgentMetadata;
use crate::agent_communication::AgentCommunicationContext;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::config::Config;
use crate::session::new_submission_id;
use crate::thread_manager::ThreadManagerState;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
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
}

enum V2AgentPreparation {
    KeepCold,
    Reload(Config),
}

impl AgentControl {
    pub(crate) async fn prepare_v2_agent_delivery(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<PreparedV2AgentDelivery> {
        self.prepare_v2_agent_delivery_inner(agent_id, V2AgentPreparation::KeepCold)
            .await
    }

    pub(crate) async fn prepare_v2_agent_delivery_with_reload(
        &self,
        config: Config,
        agent_id: ThreadId,
    ) -> CodexResult<PreparedV2AgentDelivery> {
        self.prepare_v2_agent_delivery_inner(agent_id, V2AgentPreparation::Reload(config))
            .await
    }

    async fn prepare_v2_agent_delivery_inner(
        &self,
        agent_id: ThreadId,
        preparation: V2AgentPreparation,
    ) -> CodexResult<PreparedV2AgentDelivery> {
        let state = self.upgrade()?;
        let metadata = self
            .state
            .agent_metadata_for_thread(agent_id)
            .ok_or(CodexErr::ThreadNotFound(agent_id))?;
        let mut lifecycle = metadata.lifecycle.lock().await;
        if !self.state.metadata_is_current(agent_id, &metadata) {
            return Err(CodexErr::ThreadNotFound(agent_id));
        }
        if let V2AgentPreparation::Reload(config) = preparation {
            self.ensure_v2_agent_loaded_under_lifecycle(
                &state,
                config,
                agent_id,
                &metadata,
                &mut lifecycle,
            )
            .await?;
        } else if state.get_thread(agent_id).await.is_ok() {
            self.touch_loaded_v2_residency(&state, agent_id).await;
        }
        Ok(PreparedV2AgentDelivery {
            control: self.clone(),
            state,
            agent_id,
            metadata,
            lifecycle,
        })
    }

    pub(super) async fn uses_v2_lifecycle(
        &self,
        state: &Arc<ThreadManagerState>,
        agent_id: ThreadId,
    ) -> bool {
        match state.get_thread(agent_id).await {
            Ok(thread) => thread.multi_agent_version() == Some(MultiAgentVersion::V2),
            Err(_) => self.state.cold_status(agent_id, /*live_thread*/ None).is_some(),
        }
    }

    pub(super) async fn restore_cold_mail_to_loaded_thread(
        &self,
        state: &Arc<ThreadManagerState>,
        agent_id: ThreadId,
        lifecycle: &mut AgentLifecycleState,
    ) -> CodexResult<()> {
        let thread = state.get_thread(agent_id).await?;
        let cold_mail = lifecycle.take_cold_mail();
        if cold_mail.is_empty() {
            return Ok(());
        }
        let communications = cold_mail
            .iter()
            .map(|item| item.communication.clone())
            .collect();
        thread
            .session
            .input_queue
            .enqueue_mailbox_communications(communications)
            .await;
        for receive_id in cold_mail
            .iter()
            .filter_map(|item| item.receive_id.as_deref())
        {
            crate::agent_communication::emit_agent_communication_receive(receive_id);
        }
        Ok(())
    }
}

impl PreparedV2AgentDelivery {
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
            .ensure_execution_capacity_for_turn_start(
                self.agent_id,
                communication.trigger_turn,
            )
            .await?;
        self.send_after_capacity_check(communication, context, interrupt)
            .await
    }

    pub(super) async fn send_after_capacity_check(
        mut self,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        interrupt: bool,
    ) -> CodexResult<String> {
        if !self
            .control
            .state
            .metadata_is_current(self.agent_id, &self.metadata)
        {
            return Err(CodexErr::ThreadNotFound(self.agent_id));
        }
        if let Ok(thread) = self.state.get_thread(self.agent_id).await {
            if interrupt {
                self.state
                    .record_submitted_op(self.agent_id, &Op::Interrupt);
                thread.session.interrupt_task().await;
            }
            let submission_id = new_submission_id();
            self.state.record_submitted_op(
                self.agent_id,
                &Op::InterAgentCommunication {
                    communication: communication.clone(),
                },
            );
            if crate::agent_communication::logging_enabled() {
                crate::agent_communication::emit_agent_communication_send(
                    &submission_id,
                    &context,
                    &communication,
                    self.agent_id,
                );
            }
            Box::pin(crate::session::inter_agent_communication(
                &thread.session,
                submission_id.clone(),
                communication,
            ))
            .await;
            return Ok(submission_id);
        }
        if communication.trigger_turn {
            return Err(CodexErr::ThreadNotFound(self.agent_id));
        }

        let submission_id = new_submission_id();
        self.state.record_submitted_op(
            self.agent_id,
            &Op::InterAgentCommunication {
                communication: communication.clone(),
            },
        );
        if crate::agent_communication::logging_enabled() {
            crate::agent_communication::emit_agent_communication_send(
                &submission_id,
                &context,
                &communication,
                self.agent_id,
            );
        }
        self.lifecycle.push_cold_mail(ColdMailboxItem {
            receive_id: Some(submission_id.clone()),
            communication,
        });
        Ok(submission_id)
    }
}
