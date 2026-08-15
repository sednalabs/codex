use codex_protocol::protocol::InterAgentCommunication;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Default)]
pub(super) struct AgentLifecycle {
    state: Arc<Mutex<AgentLifecycleState>>,
    reload_gate: Arc<Mutex<()>>,
}

#[derive(Debug, Default)]
pub(super) struct AgentLifecycleState {
    cold_mailbox: VecDeque<ColdMailboxItem>,
    terminal_idle_unload_generation: u64,
    terminal_idle_unload_watcher_generation: u64,
    terminal_idle_unload_watcher: Option<CancellationToken>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ColdMailboxItem {
    pub(super) receive_id: Option<String>,
    pub(super) communication: InterAgentCommunication,
}

impl AgentLifecycle {
    pub(super) async fn lock(&self) -> OwnedMutexGuard<AgentLifecycleState> {
        Arc::clone(&self.state).lock_owned().await
    }

    pub(super) async fn lock_reload(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.reload_gate).lock_owned().await
    }
}

impl AgentLifecycleState {
    pub(super) fn arm_terminal_idle_unload(&mut self) -> u64 {
        self.terminal_idle_unload_generation = self.terminal_idle_unload_generation.wrapping_add(1);
        self.terminal_idle_unload_generation
    }

    pub(super) fn invalidate_terminal_idle_unload(&mut self) {
        self.terminal_idle_unload_generation = self.terminal_idle_unload_generation.wrapping_add(1);
    }

    pub(super) fn terminal_idle_unload_is_current(&self, generation: u64) -> bool {
        self.terminal_idle_unload_generation == generation
    }

    #[cfg(test)]
    pub(super) fn terminal_idle_unload_generation(&self) -> u64 {
        self.terminal_idle_unload_generation
    }

    /// Replaces the terminal-idle watcher owner for this lifecycle. A reloaded runtime must not
    /// leave a prior runtime's watcher eligible to act after the replacement is published.
    pub(super) fn replace_terminal_idle_unload_watcher(&mut self) -> (u64, CancellationToken) {
        let owner = CancellationToken::new();
        if let Some(previous_owner) = self.terminal_idle_unload_watcher.replace(owner.clone()) {
            previous_owner.cancel();
        }
        self.terminal_idle_unload_watcher_generation =
            self.terminal_idle_unload_watcher_generation.wrapping_add(1);
        (self.terminal_idle_unload_watcher_generation, owner)
    }

    pub(super) fn terminal_idle_unload_watcher_is_current(&self, watcher_generation: u64) -> bool {
        self.terminal_idle_unload_watcher_generation == watcher_generation
            && self
                .terminal_idle_unload_watcher
                .as_ref()
                .is_some_and(|owner| !owner.is_cancelled())
    }

    pub(super) fn push_cold_mail(&mut self, item: ColdMailboxItem) {
        self.cold_mailbox.push_back(item);
    }

    pub(super) fn extend_cold_mail(&mut self, items: impl IntoIterator<Item = ColdMailboxItem>) {
        self.cold_mailbox.extend(items);
    }

    pub(super) fn take_cold_mail(&mut self) -> Vec<ColdMailboxItem> {
        self.cold_mailbox.drain(..).collect()
    }

    pub(super) fn discard_cold_mail(&mut self) {
        self.cold_mailbox.clear();
    }

    #[cfg(test)]
    pub(super) fn cold_mail_len(&self) -> usize {
        self.cold_mailbox.len()
    }
}

#[cfg(test)]
#[tokio::test]
async fn reload_gate_does_not_hold_lifecycle_state() {
    let lifecycle = AgentLifecycle::default();
    let _reload = lifecycle.lock_reload().await;
    assert!(lifecycle.state.try_lock().is_ok());
}

#[tokio::test]
async fn replacing_terminal_idle_watcher_cancels_prior_owner() {
    let lifecycle = AgentLifecycle::default();
    let (first_generation, first_owner) = lifecycle
        .lock()
        .await
        .replace_terminal_idle_unload_watcher();
    let (second_generation, second_owner) = lifecycle
        .lock()
        .await
        .replace_terminal_idle_unload_watcher();

    assert!(first_owner.is_cancelled());
    assert!(!second_owner.is_cancelled());
    assert_ne!(first_generation, second_generation);
    assert!(
        lifecycle
            .lock()
            .await
            .terminal_idle_unload_watcher_is_current(second_generation)
    );
}
