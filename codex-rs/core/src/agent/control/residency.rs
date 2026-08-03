use super::AgentControl;
use crate::agent::AgentStatus;
use crate::agent::lifecycle::ColdMailboxItem;
use crate::agent::registry::AgentRegistry;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use crate::thread_manager::RemoveThreadIfSameResult;
use crate::thread_manager::ThreadManagerState;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tracing::warn;

#[derive(Default)]
pub(super) struct V2Residency {
    state: Mutex<V2ResidencyState>,
}

#[derive(Default)]
struct V2ResidencyState {
    residents: VecDeque<ThreadId>,
    pending_slots: usize,
}

pub(super) struct V2ResidencySlot {
    residency: Arc<V2Residency>,
    active: bool,
}

impl V2ResidencySlot {
    pub(super) fn commit(mut self, thread_id: ThreadId) {
        self.residency.commit_slot(thread_id);
        self.active = false;
    }
}

impl Drop for V2ResidencySlot {
    fn drop(&mut self) {
        if self.active {
            self.residency.release_pending_slot();
        }
    }
}

impl AgentControl {
    pub(super) async fn reserve_v2_residency_slot(
        &self,
        state: &Arc<ThreadManagerState>,
        config: &Config,
        protected_thread_id: Option<ThreadId>,
    ) -> CodexResult<V2ResidencySlot> {
        let capacity = config
            .effective_agent_max_threads(MultiAgentVersion::V2)
            .unwrap_or(usize::MAX);
        Arc::clone(&self.v2_residency)
            .reserve_slot(state, self.state.as_ref(), capacity, protected_thread_id)
            .await
    }

    pub(super) async fn touch_loaded_v2_residency(
        &self,
        state: &Arc<ThreadManagerState>,
        thread_id: ThreadId,
    ) {
        if let Ok(thread) = state.get_thread(thread_id).await
            && is_resident_candidate(thread.as_ref())
        {
            self.v2_residency.touch(thread_id);
        }
    }

    pub(super) fn forget_v2_residency(&self, thread_id: ThreadId) {
        self.v2_residency.remove(thread_id);
    }

    pub(super) fn start_terminal_idle_unload_watcher(
        &self,
        thread: Arc<CodexThread>,
        metadata: crate::agent::registry::AgentMetadata,
        timeout_ms: u64,
    ) {
        if timeout_ms == 0
            || !is_resident_candidate(thread.as_ref())
            || thread.session.live_thread().is_none()
        {
            return;
        }

        let control = self.clone();
        tokio::spawn(async move {
            let thread_id = thread.thread_id;
            let mut status_rx = thread.subscribe_status();
            loop {
                let status = status_rx.borrow().clone();
                match status {
                    AgentStatus::Completed(_)
                    | AgentStatus::Errored(_)
                    | AgentStatus::Interrupted => {
                        let timer_generation = {
                            let mut lifecycle = metadata.lifecycle.lock().await;
                            if !control.state.metadata_is_current(thread_id, &metadata) {
                                return;
                            }
                            lifecycle.arm_terminal_idle_unload()
                        };
                        let runtime_activity_generation = thread
                            .session
                            .input_queue
                            .residency_activity_generation();
                        tokio::select! {
                            () = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {}
                            changed = status_rx.changed() => {
                                if changed.is_err() {
                                    return;
                                }
                                continue;
                            }
                        }
                        let Ok(manager) = control.upgrade() else {
                            return;
                        };
                        if control
                            .v2_residency
                            .try_unload_terminal_idle(
                                &manager,
                                control.state.as_ref(),
                                &metadata,
                                &thread,
                                timer_generation,
                                runtime_activity_generation,
                            )
                            .await
                        {
                            control.forget_v2_residency(thread_id);
                            return;
                        }
                    }
                    AgentStatus::PendingInit | AgentStatus::Running => {
                        if status_rx.changed().await.is_err() {
                            return;
                        }
                    }
                    AgentStatus::Shutdown | AgentStatus::NotFound => return,
                }
            }
        });
    }
}

impl V2Residency {
    async fn reserve_slot(
        self: Arc<Self>,
        manager: &Arc<ThreadManagerState>,
        registry: &AgentRegistry,
        capacity: usize,
        protected_thread_id: Option<ThreadId>,
    ) -> CodexResult<V2ResidencySlot> {
        loop {
            if self.try_reserve_pending_slot(capacity) {
                return Ok(V2ResidencySlot {
                    residency: self,
                    active: true,
                });
            }
            if !self
                .try_unload_one_resident(manager, registry, protected_thread_id)
                .await
            {
                return Err(CodexErr::new(CodexErrorDetails::AgentLimitReached {
                    max_threads: capacity,
                }));
            }
        }
    }

    fn try_reserve_pending_slot(&self, capacity: usize) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.residents.len().saturating_add(state.pending_slots) >= capacity {
            return false;
        }
        state.pending_slots += 1;
        true
    }

    async fn try_unload_one_resident(
        &self,
        manager: &Arc<ThreadManagerState>,
        registry: &AgentRegistry,
        protected_thread_id: Option<ThreadId>,
    ) -> bool {
        let candidates_to_scan = self.resident_count();
        for _ in 0..candidates_to_scan {
            let Some(candidate_thread_id) = self.pop_lru_candidate(protected_thread_id) else {
                return false;
            };
            let metadata = registry.agent_metadata_for_thread(candidate_thread_id);
            let mut lifecycle = match metadata.as_ref() {
                Some(metadata) => {
                    let lifecycle = metadata.lifecycle.lock().await;
                    if !registry.metadata_is_current(candidate_thread_id, metadata) {
                        continue;
                    }
                    Some(lifecycle)
                }
                None => None,
            };
            let Some(candidate_thread) = manager
                .get_thread(candidate_thread_id)
                .await
                .ok()
                .filter(|thread| is_resident_candidate(thread))
            else {
                continue;
            };
            let _residency_transition = candidate_thread
                .session
                .input_queue
                .lock_residency_transition()
                .await;
            if self
                .try_unload_candidate(
                    manager,
                    registry,
                    metadata.as_ref(),
                    lifecycle.as_mut().map(|guard| &mut **guard),
                    candidate_thread,
                )
                .await
            {
                return true;
            }
            self.touch(candidate_thread_id);
        }
        false
    }

    async fn try_unload_terminal_idle(
        &self,
        manager: &Arc<ThreadManagerState>,
        registry: &AgentRegistry,
        metadata: &crate::agent::registry::AgentMetadata,
        expected_thread: &Arc<CodexThread>,
        timer_generation: u64,
        runtime_activity_generation: u64,
    ) -> bool {
        let _reload = metadata.lifecycle.lock_reload().await;
        let mut lifecycle = metadata.lifecycle.lock().await;
        if !registry.metadata_is_current(expected_thread.thread_id, metadata)
            || !lifecycle.terminal_idle_unload_is_current(timer_generation)
        {
            return false;
        }
        let Ok(thread) = manager.get_thread(expected_thread.thread_id).await else {
            return false;
        };
        if !Arc::ptr_eq(&thread, expected_thread) || !is_resident_candidate(thread.as_ref()) {
            return false;
        }
        let _residency_transition = thread
            .session
            .input_queue
            .lock_residency_transition()
            .await;
        if thread
            .session
            .input_queue
            .residency_activity_generation()
            != runtime_activity_generation
        {
            return false;
        }
        self.try_unload_candidate(
            manager,
            registry,
            Some(metadata),
            Some(&mut lifecycle),
            thread,
        )
        .await
    }

    async fn try_unload_candidate(
        &self,
        manager: &Arc<ThreadManagerState>,
        registry: &AgentRegistry,
        metadata: Option<&crate::agent::registry::AgentMetadata>,
        mut lifecycle: Option<&mut crate::agent::lifecycle::AgentLifecycleState>,
        candidate_thread: Arc<CodexThread>,
    ) -> bool {
        let candidate_thread_id = candidate_thread.thread_id;
        // Cold identities are reloadable only when the session has durable history.
        if candidate_thread.session.live_thread().is_none() {
            return false;
        }
        let status = candidate_thread.agent_status().await;
        if !is_unloadable(candidate_thread.as_ref(), &status).await
            || candidate_thread
                .session
                .input_queue
                .has_pending_terminal_completions()
                .await
            || candidate_thread
                .session
                .input_queue
                .has_pending_terminal_finalizers()
            || candidate_thread
                .session
                .input_queue
                .has_pending_residency_submissions()
        {
            return false;
        }
        let cold_status = match status {
            AgentStatus::Completed(_) | AgentStatus::Errored(_) | AgentStatus::Interrupted => {
                Some(status)
            }
            AgentStatus::PendingInit
            | AgentStatus::Running
            | AgentStatus::Shutdown
            | AgentStatus::NotFound => return false,
        };
        if let Err(err) = candidate_thread
            .session
            .try_ensure_rollout_materialized()
            .await
        {
            warn!(
                "failed to materialize v2 resident thread before unloading {candidate_thread_id}: {err}"
            );
            return false;
        }
        if let Err(err) = candidate_thread.flush_rollout().await {
            warn!(
                "failed to flush v2 resident thread before unloading {candidate_thread_id}: {err}"
            );
            return false;
        }
        let pending_mail = candidate_thread
            .session
            .input_queue
            .drain_mailbox_communications()
            .await;
        if pending_mail.iter().any(|mail| mail.trigger_turn)
            || (metadata.is_none() && !pending_mail.is_empty())
        {
            candidate_thread
                .session
                .input_queue
                .prepend_mailbox_communications(pending_mail)
                .await;
            return false;
        }
        if metadata.is_some_and(|metadata| {
            !registry.metadata_is_current(candidate_thread_id, metadata)
        }) {
            candidate_thread
                .session
                .input_queue
                .prepend_mailbox_communications(pending_mail)
                .await;
            return false;
        }
        if let Err(err) = candidate_thread.shutdown_and_wait().await {
            warn!(
                "failed to shut down v2 resident thread before unloading {candidate_thread_id}: {err}"
            );
            candidate_thread
                .session
                .input_queue
                .prepend_mailbox_communications(pending_mail)
                .await;
            return false;
        }
        let removal = manager
            .remove_thread_if_same(&candidate_thread_id, &candidate_thread, || {
                if let (Some(metadata), Some(status)) = (metadata, cold_status) {
                    registry.publish_cold_status_if_current(
                        candidate_thread_id,
                        metadata,
                        &candidate_thread,
                        status,
                    );
                }
            })
            .await;
        match removal {
            RemoveThreadIfSameResult::Removed | RemoveThreadIfSameResult::Missing => {
                if let Some(lifecycle) = lifecycle.as_mut() {
                    lifecycle.extend_cold_mail(pending_mail.into_iter().map(|communication| {
                        ColdMailboxItem {
                            receive_id: None,
                            communication,
                        }
                    }));
                }
                true
            }
            RemoveThreadIfSameResult::Replaced => {
                if let Ok(replacement) = manager.get_thread(candidate_thread_id).await {
                    replacement
                        .session
                        .input_queue
                        .prepend_mailbox_communications(pending_mail)
                        .await;
                } else if let Some(lifecycle) = lifecycle.as_mut() {
                    lifecycle.extend_cold_mail(pending_mail.into_iter().map(|communication| {
                        ColdMailboxItem {
                            receive_id: None,
                            communication,
                        }
                    }));
                }
                false
            }
        }
    }

    fn resident_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .residents
            .len()
    }

    fn pop_lru_candidate(&self, protected_thread_id: Option<ThreadId>) -> Option<ThreadId> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let candidates_to_scan = state.residents.len();
        for _ in 0..candidates_to_scan {
            let candidate_thread_id = state.residents.pop_front()?;
            if Some(candidate_thread_id) == protected_thread_id {
                state.residents.push_back(candidate_thread_id);
                continue;
            }
            return Some(candidate_thread_id);
        }
        None
    }

    fn touch(&self, thread_id: ThreadId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        touch_resident(&mut state.residents, thread_id);
    }

    fn remove(&self, thread_id: ThreadId) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .residents
            .retain(|resident_thread_id| *resident_thread_id != thread_id);
    }

    fn commit_slot(&self, thread_id: ThreadId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_slots = state.pending_slots.saturating_sub(1);
        touch_resident(&mut state.residents, thread_id);
    }

    fn release_pending_slot(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_slots = state.pending_slots.saturating_sub(1);
    }
}

fn touch_resident(residents: &mut VecDeque<ThreadId>, thread_id: ThreadId) {
    residents.retain(|resident_thread_id| *resident_thread_id != thread_id);
    residents.push_back(thread_id);
}

fn is_resident_candidate(thread: &CodexThread) -> bool {
    thread.multi_agent_version() == Some(MultiAgentVersion::V2)
        && is_v2_resident_session_source(&thread.session_source)
}

pub(super) fn is_v2_resident_session_source(session_source: &SessionSource) -> bool {
    matches!(session_source, SessionSource::SubAgent(_))
}

async fn is_unloadable(thread: &CodexThread, status: &AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Completed(_) | AgentStatus::Errored(_) | AgentStatus::Interrupted
    ) && thread.session.active_turn.lock().await.is_none()
        && thread.list_background_terminals().await.is_empty()
}

#[cfg(test)]
#[path = "residency_tests.rs"]
mod tests;
