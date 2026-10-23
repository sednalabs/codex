use crate::store::Mailbox;
use codex_protocol::ThreadId;
use std::collections::HashSet;
use std::sync::Mutex;

const MAX_RUNTIME_FAULT_AGENTS: usize = 4_096;

#[derive(Default)]
pub(crate) struct RuntimeFaults {
    agents: HashSet<String>,
    overflowed: bool,
}

impl Mailbox {
    /// Signal an in-memory observation that terminal custody could not settle.
    /// This does not create or fabricate a durable work outcome.
    pub fn notify_runtime_fault(&self, agent: ThreadId) {
        let _input_arrival = self.begin_input_arrival();
        let mut faults = lock(&self.runtime_faults);
        let agent = agent.to_string();
        let changed = if faults.agents.contains(&agent) {
            false
        } else if faults.agents.len() < MAX_RUNTIME_FAULT_AGENTS {
            faults.agents.insert(agent)
        } else if !faults.overflowed {
            // Preserve an honest wake if the bounded exact set fills. The
            // conservative overflow indication remains until process restart.
            faults.overflowed = true;
            true
        } else {
            false
        };
        drop(faults);
        if changed {
            self.notify_changed();
        }
    }

    /// Check only the caller's bounded selected agent set for a reported fault.
    /// Oversized selections conservatively wake rather than risk a silent hang.
    pub fn has_runtime_fault(&self, agents: &[ThreadId]) -> bool {
        if agents.len() > 100 {
            return true;
        }
        let faults = lock(&self.runtime_faults);
        faults.overflowed
            || agents
                .iter()
                .any(|agent| faults.agents.contains(&agent.to_string()))
    }

    /// Clear one exact agent's fault only after explicit adapter recovery.
    pub fn clear_runtime_fault(&self, agent: ThreadId) -> bool {
        let cleared = lock(&self.runtime_faults).agents.remove(&agent.to_string());
        if cleared {
            self.notify_changed();
        }
        cleared
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
