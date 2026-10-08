use crate::agent::types::AgentMetadata;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::AgentErrorContext;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnEnvironmentSelection;
use rand::prelude::IndexedRandom;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

/// This structure is used to add some limits on the multi-agent capabilities for Codex. In
/// the current implementation, it limits:
/// * Total number of sub-agents (i.e. threads) per user session
///
/// This structure is shared by all agents in the same user session (because the `LocalAgentControl`
/// is).
#[derive(Default)]
pub(crate) struct AgentRegistry {
    active_agents: Mutex<ActiveAgents>,
    total_count: AtomicUsize,
}

#[derive(Default)]
struct ActiveAgents {
    agent_tree: HashMap<String, AgentMetadata>,
    thread_paths: HashMap<ThreadId, RegisteredAgent>,
    reserved_thread_ids: HashSet<ThreadId>,
    used_agent_nicknames: HashSet<String>,
    nickname_reset_count: usize,
}

pub(crate) struct RestoreAgentMetadata {
    pub(crate) thread_id: ThreadId,
    pub(crate) agent_path: Option<AgentPath>,
    pub(crate) agent_role: Option<String>,
    pub(crate) preferred_nickname: Option<String>,
    pub(crate) nickname_candidates: Vec<String>,
}

struct RegisteredAgent {
    path: String,
    evicted_environments: Option<Vec<TurnEnvironmentSelection>>,
}

impl RegisteredAgent {
    fn new(path: String) -> Self {
        Self {
            path,
            evicted_environments: None,
        }
    }
}

fn format_agent_nickname(name: &str, nickname_reset_count: usize) -> String {
    match nickname_reset_count {
        0 => name.to_string(),
        reset_count => {
            let value = reset_count + 1;
            let suffix = match value % 100 {
                11..=13 => "th",
                _ => match value % 10 {
                    1 => "st", // codespell:ignore
                    2 => "nd", // codespell:ignore
                    3 => "rd", // codespell:ignore
                    _ => "th", // codespell:ignore
                },
            };
            format!("{name} the {value}{suffix}")
        }
    }
}

fn session_depth(session_source: &SessionSource) -> i32 {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { depth, .. }) => *depth,
        SessionSource::SubAgent(_) => 0,
        _ => 0,
    }
}

pub(crate) fn next_thread_spawn_depth(session_source: &SessionSource) -> i32 {
    session_depth(session_source).saturating_add(1)
}

pub(crate) fn exceeds_thread_spawn_depth_limit(depth: i32, max_depth: i32) -> bool {
    depth > max_depth
}

impl AgentRegistry {
    pub(crate) fn reserve_spawn_slot(
        self: &Arc<Self>,
        max_threads: Option<usize>,
    ) -> Result<SpawnReservation> {
        if let Some(max_threads) = max_threads {
            if !self.try_increment_spawned(max_threads) {
                return Err(
                    CodexErr::new(CodexErrorDetails::AgentLimitReached { max_threads })
                        .with_agent_context(AgentErrorContext::RegistryCapacity),
                );
            }
        } else {
            self.total_count.fetch_add(1, Ordering::AcqRel);
        }
        Ok(SpawnReservation {
            state: Arc::clone(self),
            active: true,
            reserved_agent_nickname: None,
            reserved_agent_path: None,
        })
    }

    /// Restore persisted agent identities as one registry transaction.
    ///
    /// All identifiers, paths, and slots are reserved before nicknames are consumed. The mutex
    /// remains held through the single metadata commit, so readers cannot observe a partial tree.
    pub(crate) fn restore_agent_metadata_batch(
        &self,
        entries: Vec<RestoreAgentMetadata>,
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut thread_ids = HashSet::new();
        let mut paths = HashSet::new();
        for entry in &entries {
            if !thread_ids.insert(entry.thread_id)
                || active_agents.thread_paths.contains_key(&entry.thread_id)
                || active_agents.reserved_thread_ids.contains(&entry.thread_id)
            {
                return Err(CodexErr::InvalidRequest(format!(
                    "agent thread `{}` is already registered or reserved",
                    entry.thread_id
                )));
            }
            if entry.preferred_nickname.is_none() && entry.nickname_candidates.is_empty() {
                return Err(CodexErr::InvalidRequest(format!(
                    "no nickname candidates for restored agent {}",
                    entry.thread_id
                )));
            }
            if let Some(agent_path) = &entry.agent_path
                && (!paths.insert(agent_path.to_string())
                    || active_agents.agent_tree.contains_key(agent_path.as_str()))
            {
                return Err(CodexErr::InvalidRequest(format!(
                    "stored agent path {agent_path} is duplicated or already registered"
                )));
            }
        }

        self.total_count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(entries.len())
            })
            .map_err(|_| CodexErr::InvalidRequest("agent registry slot count overflow".into()))?;

        let mut reservation = RestoreBatchReservation::new(
            &mut active_agents,
            &self.total_count,
            entries,
        );
        reservation.reserve_all();
        reservation.allocate_nicknames_and_commit();
        Ok(())
    }

    pub(crate) fn release_spawned_thread(&self, thread_id: ThreadId) {
        let removed_counted_agent = {
            let mut active_agents = self
                .active_agents
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            active_agents
                .thread_paths
                .remove(&thread_id)
                .and_then(|agent| active_agents.agent_tree.remove(agent.path.as_str()))
                .is_some_and(|metadata| {
                    !metadata.agent_path.as_ref().is_some_and(AgentPath::is_root)
                })
        };
        if removed_counted_agent {
            self.total_count.fetch_sub(1, Ordering::AcqRel);
        }
    }

    pub(crate) fn register_root_thread(&self, thread_id: ThreadId) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root_path = AgentPath::ROOT.to_string();
        let root_thread_id = active_agents
            .agent_tree
            .entry(root_path.clone())
            .or_insert_with(|| AgentMetadata {
                agent_id: Some(thread_id),
                agent_path: Some(AgentPath::root()),
                ..Default::default()
            })
            .agent_id;
        if let Some(root_thread_id) = root_thread_id {
            active_agents
                .thread_paths
                .insert(root_thread_id, RegisteredAgent::new(root_path));
        }
    }

    pub(crate) fn agent_id_for_path(&self, agent_path: &AgentPath) -> Option<ThreadId> {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent_tree
            .get(agent_path.as_str())
            .and_then(|metadata| metadata.agent_id)
    }

    pub(crate) fn agent_metadata_for_thread(&self, thread_id: ThreadId) -> Option<AgentMetadata> {
        let active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active_agents
            .thread_paths
            .get(&thread_id)
            .and_then(|agent| active_agents.agent_tree.get(&agent.path))
            .cloned()
    }

    pub(crate) fn save_evicted_environments(
        &self,
        thread_id: ThreadId,
        environments: Vec<TurnEnvironmentSelection>,
    ) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(agent) = active_agents.thread_paths.get_mut(&thread_id) {
            agent.evicted_environments = Some(environments);
        }
    }

    pub(crate) fn evicted_environments(
        &self,
        thread_id: ThreadId,
    ) -> Option<Vec<TurnEnvironmentSelection>> {
        let active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active_agents
            .thread_paths
            .get(&thread_id)
            .and_then(|agent| agent.evicted_environments.clone())
    }

    pub(crate) fn clear_evicted_environments(&self, thread_id: ThreadId) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(agent) = active_agents.thread_paths.get_mut(&thread_id) {
            agent.evicted_environments = None;
        }
    }

    pub(crate) fn live_agents(&self) -> Vec<AgentMetadata> {
        self.active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .agent_tree
            .values()
            .filter(|metadata| {
                metadata.agent_id.is_some()
                    && !metadata.agent_path.as_ref().is_some_and(AgentPath::is_root)
            })
            .cloned()
            .collect()
    }

    fn register_spawned_thread(&self, agent_metadata: AgentMetadata) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::insert_spawned_thread(&mut active_agents, agent_metadata);
    }

    fn insert_spawned_thread(
        active_agents: &mut ActiveAgents,
        agent_metadata: AgentMetadata,
    ) {
        let Some(thread_id) = agent_metadata.agent_id else {
            return;
        };
        let key = agent_metadata
            .agent_path
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("thread:{thread_id}"));
        if let Some(agent_nickname) = agent_metadata.agent_nickname.clone() {
            active_agents.used_agent_nicknames.insert(agent_nickname);
        }
        if let Some(previous_agent) = active_agents
            .thread_paths
            .insert(thread_id, RegisteredAgent::new(key.clone()))
            && previous_agent.path != key
        {
            active_agents
                .agent_tree
                .remove(previous_agent.path.as_str());
        }
        if let Some(previous_metadata) = active_agents.agent_tree.insert(key, agent_metadata)
            && let Some(previous_thread_id) = previous_metadata.agent_id
            && previous_thread_id != thread_id
        {
            active_agents.thread_paths.remove(&previous_thread_id);
        }
    }

    fn reserve_agent_nickname(&self, names: &[&str], preferred: Option<&str>) -> Option<String> {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::reserve_agent_nickname_locked(&mut active_agents, names, preferred)
    }

    fn reserve_agent_nickname_locked(
        active_agents: &mut ActiveAgents,
        names: &[&str],
        preferred: Option<&str>,
    ) -> Option<String> {
        let agent_nickname = if let Some(preferred) = preferred {
            preferred.to_string()
        } else {
            if names.is_empty() {
                return None;
            }
            let available_names: Vec<String> = names
                .iter()
                .map(|name| format_agent_nickname(name, active_agents.nickname_reset_count))
                .filter(|name| !active_agents.used_agent_nicknames.contains(name))
                .collect();
            if let Some(name) = available_names.choose(&mut rand::rng()) {
                name.clone()
            } else {
                active_agents.used_agent_nicknames.clear();
                active_agents.nickname_reset_count += 1;
                if let Some(metrics) = codex_otel::global() {
                    let _ = metrics.counter(
                        "codex.multi_agent.nickname_pool_reset",
                        /*inc*/ 1,
                        &[],
                    );
                }
                format_agent_nickname(
                    names.choose(&mut rand::rng())?,
                    active_agents.nickname_reset_count,
                )
            }
        };
        active_agents
            .used_agent_nicknames
            .insert(agent_nickname.clone());
        Some(agent_nickname)
    }

    fn reserve_agent_path(&self, agent_path: &AgentPath) -> Result<()> {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match active_agents.agent_tree.entry(agent_path.to_string()) {
            Entry::Occupied(_) => Err(CodexErr::UnsupportedOperation(format!(
                "agent path `{agent_path}` already exists"
            ))
            .with_agent_context(AgentErrorContext::DuplicatePath)),
            Entry::Vacant(entry) => {
                entry.insert(AgentMetadata {
                    agent_path: Some(agent_path.clone()),
                    ..Default::default()
                });
                Ok(())
            }
        }
    }

    fn release_reserved_agent_path(&self, agent_path: &AgentPath) {
        let mut active_agents = self
            .active_agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active_agents
            .agent_tree
            .get(agent_path.as_str())
            .is_some_and(|metadata| metadata.agent_id.is_none())
        {
            active_agents.agent_tree.remove(agent_path.as_str());
        }
    }

    fn try_increment_spawned(&self, max_threads: usize) -> bool {
        let mut current = self.total_count.load(Ordering::Acquire);
        loop {
            if current >= max_threads {
                return false;
            }
            match self.total_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(updated) => current = updated,
            }
        }
    }
}

struct RestoreBatchReservation<'a> {
    active_agents: &'a mut ActiveAgents,
    total_count: &'a AtomicUsize,
    entries: Vec<RestoreAgentMetadata>,
    committed: bool,
}

impl<'a> RestoreBatchReservation<'a> {
    fn new(
        active_agents: &'a mut ActiveAgents,
        total_count: &'a AtomicUsize,
        entries: Vec<RestoreAgentMetadata>,
    ) -> Self {
        Self {
            active_agents,
            total_count,
            entries,
            committed: false,
        }
    }

    fn reserve_all(&mut self) {
        for entry in &self.entries {
            let inserted = self.active_agents.reserved_thread_ids.insert(entry.thread_id);
            debug_assert!(inserted);
            if let Some(agent_path) = &entry.agent_path {
                let previous = self.active_agents.agent_tree.insert(
                    agent_path.to_string(),
                    AgentMetadata {
                        agent_path: Some(agent_path.clone()),
                        ..Default::default()
                    },
                );
                debug_assert!(previous.is_none());
            }
        }
    }

    fn allocate_nicknames_and_commit(&mut self) {
        let metadata = self
            .entries
            .iter()
            .map(|entry| {
                let candidate_names: Vec<&str> = entry
                    .nickname_candidates
                    .iter()
                    .map(String::as_str)
                    .collect();
                let agent_nickname = AgentRegistry::reserve_agent_nickname_locked(
                    self.active_agents,
                    &candidate_names,
                    entry.preferred_nickname.as_deref(),
                )
                .expect("restore batch nickname candidates were preflighted");
                AgentMetadata {
                    agent_id: Some(entry.thread_id),
                    agent_path: entry.agent_path.clone(),
                    agent_nickname: Some(agent_nickname),
                    agent_role: entry.agent_role.clone(),
                }
            })
            .collect::<Vec<_>>();

        for entry in &self.entries {
            self.active_agents
                .reserved_thread_ids
                .remove(&entry.thread_id);
        }
        for agent_metadata in metadata {
            AgentRegistry::insert_spawned_thread(self.active_agents, agent_metadata);
        }
        self.committed = true;
    }
}

impl Drop for RestoreBatchReservation<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for entry in &self.entries {
            self.active_agents
                .reserved_thread_ids
                .remove(&entry.thread_id);
            if let Some(agent_path) = &entry.agent_path
                && self
                    .active_agents
                    .agent_tree
                    .get(agent_path.as_str())
                    .is_some_and(|metadata| metadata.agent_id.is_none())
            {
                self.active_agents
                    .agent_tree
                    .remove(agent_path.as_str());
            }
        }
        self.total_count
            .fetch_sub(self.entries.len(), Ordering::AcqRel);
    }
}

pub(crate) struct SpawnReservation {
    state: Arc<AgentRegistry>,
    active: bool,
    reserved_agent_nickname: Option<String>,
    reserved_agent_path: Option<AgentPath>,
}

impl SpawnReservation {
    pub(crate) fn reserve_agent_nickname_with_preference(
        &mut self,
        names: &[&str],
        preferred: Option<&str>,
    ) -> Result<String> {
        let agent_nickname = self
            .state
            .reserve_agent_nickname(names, preferred)
            .ok_or_else(|| {
                CodexErr::UnsupportedOperation("no available agent nicknames".to_string())
                    .with_agent_context(AgentErrorContext::NicknameUnavailable)
            })?;
        self.reserved_agent_nickname = Some(agent_nickname.clone());
        Ok(agent_nickname)
    }

    pub(crate) fn reserve_agent_path(&mut self, agent_path: &AgentPath) -> Result<()> {
        self.state.reserve_agent_path(agent_path)?;
        self.reserved_agent_path = Some(agent_path.clone());
        Ok(())
    }

    pub(crate) fn commit(mut self, agent_metadata: AgentMetadata) {
        self.reserved_agent_nickname = None;
        self.reserved_agent_path = None;
        self.state.register_spawned_thread(agent_metadata);
        self.active = false;
    }
}

impl Drop for SpawnReservation {
    fn drop(&mut self) {
        if self.active {
            if let Some(agent_path) = self.reserved_agent_path.take() {
                self.state.release_reserved_agent_path(&agent_path);
            }
            self.state.total_count.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
