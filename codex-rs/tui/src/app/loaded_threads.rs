//! Discovers subagent threads that belong to a primary thread by walking spawn-tree edges.
//!
//! When the TUI resumes or switches to an existing thread, it needs to populate
//! `AgentNavigationState` and `ChatWidget` metadata for every subagent that was spawned during
//! that thread's lifetime. The app server returns ancestor-filtered lineage pages, and the TUI
//! validates their spawn edges before adding them to the selected primary thread's navigation.
//!
//! This module provides the pure, synchronous tree-walk that turns that flat list into the filtered
//! set of descendants. It intentionally has no async, no I/O, and no side effects so it can be
//! unit-tested in isolation.
//!
//! The walk starts from `primary_thread_id` and repeatedly follows
//! `SessionSource::SubAgent(ThreadSpawn { parent_thread_id, .. })` edges until no new children are
//! found. The primary thread itself is never included in the output.

use crate::app_server_session::thread_blocks_direct_input;
use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadStatus;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SubAgentSource;
/// Descendant capacity after reserving one slot for the retained primary thread.
pub(crate) const MAX_RETAINED_SUBAGENT_LINEAGE: usize =
    codex_state::MAX_THREAD_RELATION_DESCENDANTS.saturating_sub(1);
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

pub(crate) const SUBAGENT_BACKFILL_PAGES_PER_ATTEMPT: usize = 32;
pub(crate) const AGENT_PICKER_ROWS_PER_OPEN: usize = 200;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LineagePageAdvance {
    Complete,
    Continue(String),
    Pause(String),
    CursorCycle(String),
    Truncated,
}

pub(crate) struct LineagePageBudget {
    pages_fetched: usize,
    seen_cursors: HashSet<String>,
}

/// Incrementally validates ancestor-filtered pages without retaining or re-walking accepted rows.
pub(crate) struct LoadedSubagentAccumulator {
    primary_thread_id: ThreadId,
    accepted_thread_ids: HashSet<ThreadId>,
    seen_thread_ids: HashSet<ThreadId>,
    pending_by_parent: HashMap<ThreadId, Vec<Thread>>,
}

impl LoadedSubagentAccumulator {
    pub(crate) fn new(primary_thread_id: ThreadId) -> Self {
        Self {
            primary_thread_id,
            accepted_thread_ids: HashSet::from([primary_thread_id]),
            seen_thread_ids: HashSet::new(),
            pending_by_parent: HashMap::new(),
        }
    }

    /// Adds one page and returns only newly validated descendants.
    ///
    /// Rows whose parents have not arrived yet remain indexed by parent. Once an ancestor is
    /// accepted, every now-reachable pending child is drained exactly once, so total work is
    /// linear in the number of rows observed across the attempt.
    pub(crate) fn ingest(&mut self, threads: Vec<Thread>) -> Vec<LoadedSubagentThread> {
        let mut ready_parents = HashSet::new();
        for thread in threads {
            let Ok(thread_id) = ThreadId::from_string(&thread.id) else {
                continue;
            };
            if thread_id == self.primary_thread_id || !self.seen_thread_ids.insert(thread_id) {
                continue;
            }
            let Some(parent_thread_id) = thread_spawn_parent_thread_id(&thread.source) else {
                continue;
            };
            if self.accepted_thread_ids.contains(&parent_thread_id) {
                ready_parents.insert(parent_thread_id);
            }
            self.pending_by_parent
                .entry(parent_thread_id)
                .or_default()
                .push(thread);
        }

        let mut pending_parents = VecDeque::from_iter(ready_parents);
        let mut loaded = Vec::new();
        while let Some(parent_thread_id) = pending_parents.pop_front() {
            let Some(children) = self.pending_by_parent.remove(&parent_thread_id) else {
                continue;
            };
            for thread in children {
                let Ok(thread_id) = ThreadId::from_string(&thread.id) else {
                    continue;
                };
                if !self.accepted_thread_ids.insert(thread_id) {
                    continue;
                }
                pending_parents.push_back(thread_id);
                loaded.push(loaded_subagent_thread(thread_id, thread));
            }
        }
        loaded.sort_by_key(|thread| thread.thread_id.to_string());
        loaded
    }

    /// Seeds descendants already established by an authoritative ancestor-filtered page.
    pub(crate) fn seed_accepted(&mut self, thread_ids: impl IntoIterator<Item = ThreadId>) {
        self.accepted_thread_ids.extend(thread_ids);
    }

    /// Admits rows whose immediate parents were filtered from a complete authoritative listing.
    ///
    /// `thread/list` has already constrained every returned row to the primary's descendant set.
    /// Once the final page arrives, any remaining parent gaps therefore represent filtered
    /// connectors rather than unrelated rows. Draining the parent buckets visits each pending row
    /// once and leaves retries idempotent.
    pub(crate) fn finish(&mut self) -> Vec<LoadedSubagentThread> {
        let accepted_thread_ids = &mut self.accepted_thread_ids;
        let mut loaded = self
            .pending_by_parent
            .drain()
            .flat_map(|(_, threads)| threads)
            .filter_map(|thread| {
                let thread_id = ThreadId::from_string(&thread.id).ok()?;
                accepted_thread_ids
                    .insert(thread_id)
                    .then(|| loaded_subagent_thread(thread_id, thread))
            })
            .collect::<Vec<_>>();
        loaded.sort_by_key(|thread| thread.thread_id.to_string());
        loaded
    }

    pub(crate) fn contains_accepted(&self, thread_id: ThreadId) -> bool {
        self.accepted_thread_ids.contains(&thread_id)
    }

    #[cfg(test)]
    fn pending_thread_count(&self) -> usize {
        self.pending_by_parent.values().map(Vec::len).sum()
    }
}

impl LineagePageBudget {
    pub(crate) fn new(seen_cursors: HashSet<String>) -> Self {
        Self {
            pages_fetched: 0,
            seen_cursors,
        }
    }

    pub(crate) fn observe_page(&mut self, next_cursor: Option<String>) -> LineagePageAdvance {
        self.pages_fetched += 1;
        let Some(next_cursor) = next_cursor else {
            return LineagePageAdvance::Complete;
        };
        if self.seen_cursors.contains(&next_cursor) {
            return LineagePageAdvance::CursorCycle(next_cursor);
        }
        if self.seen_cursors.len() >= MAX_RETAINED_SUBAGENT_LINEAGE {
            return LineagePageAdvance::Truncated;
        }
        self.seen_cursors.insert(next_cursor.clone());
        if self.pages_fetched >= SUBAGENT_BACKFILL_PAGES_PER_ATTEMPT {
            LineagePageAdvance::Pause(next_cursor)
        } else {
            LineagePageAdvance::Continue(next_cursor)
        }
    }

    pub(crate) fn into_seen_cursors(self) -> HashSet<String> {
        self.seen_cursors
    }

    #[cfg(test)]
    pub(crate) fn seen_cursor_count(&self) -> usize {
        self.seen_cursors.len()
    }
}

/// A subagent thread discovered by the spawn-tree walk, carrying just enough metadata for the
/// TUI to register it in the navigation cache and rendering metadata map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadedSubagentThread {
    pub(crate) thread_id: ThreadId,
    pub(crate) agent_nickname: Option<String>,
    pub(crate) agent_role: Option<String>,
    pub(crate) agent_path: Option<String>,
    pub(crate) blocks_direct_input: bool,
    pub(crate) has_authoritative_input_capability: bool,
    pub(crate) is_running: bool,
    pub(crate) is_closed: bool,
}

/// Walks the spawn tree rooted at `primary_thread_id` and returns every descendant subagent.
///
/// The walk is breadth-first over `SessionSource::SubAgent(ThreadSpawn { parent_thread_id })` edges.
/// Threads whose `source` is not a `ThreadSpawn`, or whose `parent_thread_id` does not chain back
/// to `primary_thread_id`, are excluded. The primary thread itself is never included.
///
/// Results are sorted by stringified thread id for deterministic output in tests and in the
/// navigation cache. Callers should not rely on this ordering for anything semantic; it exists
/// purely to make snapshot assertions stable.
///
/// If two threads claim the same parent, both are included. Cycles in the parent chain are not
/// possible because `ThreadId`s are server-assigned UUIDs and the server enforces acyclicity, but
/// the `included` set guards against re-visiting regardless.
#[cfg(test)]
pub(crate) fn find_loaded_subagent_threads_for_primary(
    threads: Vec<Thread>,
    primary_thread_id: ThreadId,
) -> Vec<LoadedSubagentThread> {
    find_loaded_subagent_threads_for_primary_with_counts(threads, primary_thread_id).0
}

#[cfg(test)]
fn find_loaded_subagent_threads_for_primary_with_counts(
    threads: Vec<Thread>,
    primary_thread_id: ThreadId,
) -> (Vec<LoadedSubagentThread>, usize, usize) {
    let mut threads_by_id = HashMap::new();
    let mut children_by_parent = HashMap::<ThreadId, Vec<ThreadId>>::new();
    let mut indexed_edges = 0;
    for thread in threads {
        let Ok(thread_id) = ThreadId::from_string(&thread.id) else {
            continue;
        };
        if let Some(parent_thread_id) = thread_spawn_parent_thread_id(&thread.source) {
            indexed_edges += 1;
            children_by_parent
                .entry(parent_thread_id)
                .or_default()
                .push(thread_id);
        }
        threads_by_id.insert(thread_id, thread);
    }

    let mut included = HashSet::new();
    let mut pending = vec![primary_thread_id];
    let mut followed_edges = 0;
    while let Some(parent_thread_id) = pending.pop() {
        let Some(children) = children_by_parent.get(&parent_thread_id) else {
            continue;
        };
        for thread_id in children {
            followed_edges += 1;
            if included.insert(*thread_id) {
                pending.push(*thread_id);
            }
        }
    }

    let mut loaded_threads: Vec<LoadedSubagentThread> = included
        .into_iter()
        .filter_map(|thread_id| {
            threads_by_id
                .remove(&thread_id)
                .map(|thread| loaded_subagent_thread(thread_id, thread))
        })
        .collect();
    loaded_threads.sort_by_key(|thread| thread.thread_id.to_string());
    (loaded_threads, indexed_edges, followed_edges)
}

pub(crate) fn loaded_subagent_thread(thread_id: ThreadId, thread: Thread) -> LoadedSubagentThread {
    LoadedSubagentThread {
        blocks_direct_input: thread_blocks_direct_input(&thread),
        has_authoritative_input_capability: thread.can_accept_direct_input.is_some(),
        is_running: matches!(&thread.status, ThreadStatus::Active { .. }),
        is_closed: matches!(&thread.status, ThreadStatus::NotLoaded),
        thread_id,
        agent_nickname: thread.agent_nickname,
        agent_role: thread.agent_role,
        agent_path: thread_spawn_agent_path(&thread.source),
    }
}

fn thread_spawn_agent_path(source: &SessionSource) -> Option<String> {
    match source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_path, .. }) => {
            agent_path.clone().map(String::from)
        }
        _ => None,
    }
}

fn thread_spawn_parent_thread_id(source: &SessionSource) -> Option<ThreadId> {
    match source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) => Some(*parent_thread_id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::LineagePageAdvance;
    use super::LineagePageBudget;
    use super::LoadedSubagentAccumulator;
    use super::LoadedSubagentThread;
    use super::SUBAGENT_BACKFILL_PAGES_PER_ATTEMPT;
    use super::find_loaded_subagent_threads_for_primary;
    use super::find_loaded_subagent_threads_for_primary_with_counts;
    use codex_app_server_protocol::SessionSource;
    use codex_app_server_protocol::Thread;
    use codex_app_server_protocol::ThreadStatus;
    use codex_protocol::ThreadId;
    use codex_utils_absolute_path::test_support::PathBufExt;
    use codex_utils_absolute_path::test_support::test_path_buf;
    use pretty_assertions::assert_eq;
    use std::collections::HashSet;

    fn test_thread(thread_id: ThreadId, source: SessionSource) -> Thread {
        Thread {
            id: thread_id.to_string(),
            extra: None,
            session_id: thread_id.to_string(),
            forked_from_id: None,
            parent_thread_id: None,
            preview: String::new(),
            ephemeral: false,
            is_pinned: false,
            history_mode: Default::default(),
            model_provider: "openai".to_string(),
            model: None,
            reasoning_effort: None,
            created_at: 0,
            updated_at: 0,
            recency_at: Some(0),
            status: ThreadStatus::Idle,
            path: None,
            cwd: test_path_buf("/tmp").abs(),
            cli_version: "0.0.0".to_string(),
            source,
            can_accept_direct_input: None,
            thread_source: None,
            agent_nickname: None,
            agent_role: None,
            git_info: None,
            name: None,
            turns: Vec::new(),
        }
    }

    fn thread_spawn_source(
        parent_thread_id: ThreadId,
        depth: i32,
        agent_nickname: &str,
        agent_role: &str,
    ) -> SessionSource {
        serde_json::from_value(serde_json::json!({
            "subAgent": {
                "thread_spawn": {
                    "parent_thread_id": parent_thread_id.to_string(),
                    "depth": depth,
                    "agent_nickname": agent_nickname,
                    "agent_role": agent_role,
                }
            }
        }))
        .expect("valid subagent source")
    }

    #[test]
    fn finds_loaded_subagent_tree_for_primary_thread() {
        let primary_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("valid thread");
        let child_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000002").expect("valid thread");
        let grandchild_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000003").expect("valid thread");
        let unrelated_parent_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000004").expect("valid thread");
        let unrelated_child_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000005").expect("valid thread");

        let mut child = test_thread(
            child_thread_id,
            thread_spawn_source(primary_thread_id, /*depth*/ 1, "Scout", "explorer"),
        );
        child.agent_nickname = Some("Scout".to_string());
        child.agent_role = Some("explorer".to_string());
        child.can_accept_direct_input = Some(true);
        child.status = ThreadStatus::Active {
            active_flags: Vec::new(),
        };

        let mut grandchild = test_thread(
            grandchild_thread_id,
            thread_spawn_source(child_thread_id, /*depth*/ 2, "Atlas", "worker"),
        );
        grandchild.agent_nickname = Some("Atlas".to_string());
        grandchild.agent_role = Some("worker".to_string());
        grandchild.can_accept_direct_input = Some(false);
        grandchild.status = ThreadStatus::NotLoaded;
        let unrelated_child = test_thread(
            unrelated_child_id,
            thread_spawn_source(unrelated_parent_id, /*depth*/ 1, "Other", "researcher"),
        );

        let loaded = find_loaded_subagent_threads_for_primary(
            vec![
                test_thread(primary_thread_id, SessionSource::Cli),
                child,
                grandchild,
                unrelated_child,
            ],
            primary_thread_id,
        );

        assert_eq!(
            loaded,
            vec![
                LoadedSubagentThread {
                    blocks_direct_input: false,
                    has_authoritative_input_capability: true,
                    thread_id: child_thread_id,
                    agent_nickname: Some("Scout".to_string()),
                    agent_role: Some("explorer".to_string()),
                    agent_path: None,
                    is_running: true,
                    is_closed: false,
                },
                LoadedSubagentThread {
                    blocks_direct_input: true,
                    has_authoritative_input_capability: true,
                    thread_id: grandchild_thread_id,
                    agent_nickname: Some("Atlas".to_string()),
                    agent_role: Some("worker".to_string()),
                    agent_path: None,
                    is_running: false,
                    is_closed: true,
                },
            ]
        );
    }

    #[test]
    fn lineage_page_budget_rejects_cursor_cycles() {
        let mut budget = LineagePageBudget::new(HashSet::new());

        assert_eq!(
            budget.observe_page(Some("cursor-a".to_string())),
            LineagePageAdvance::Continue("cursor-a".to_string())
        );
        assert_eq!(
            budget.observe_page(Some("cursor-b".to_string())),
            LineagePageAdvance::Continue("cursor-b".to_string())
        );
        assert_eq!(
            budget.observe_page(Some("cursor-a".to_string())),
            LineagePageAdvance::CursorCycle("cursor-a".to_string())
        );
    }

    #[test]
    fn lineage_page_budget_pauses_with_continuation_after_limit() {
        let mut budget = LineagePageBudget::new(HashSet::new());

        for page in 1..SUBAGENT_BACKFILL_PAGES_PER_ATTEMPT {
            assert_eq!(
                budget.observe_page(Some(format!("cursor-{page}"))),
                LineagePageAdvance::Continue(format!("cursor-{page}"))
            );
        }
        assert_eq!(
            budget.observe_page(Some("continuation-cursor".to_string())),
            LineagePageAdvance::Pause("continuation-cursor".to_string())
        );
    }

    #[test]
    fn lineage_page_budget_rejects_unique_cursor_beyond_retained_limit() {
        let seen_cursors = (0..super::MAX_RETAINED_SUBAGENT_LINEAGE)
            .map(|index| format!("cursor-{index}"))
            .collect();
        let mut budget = LineagePageBudget::new(seen_cursors);

        assert_eq!(
            budget.observe_page(Some("over-limit".to_string())),
            LineagePageAdvance::Truncated
        );
        assert_eq!(
            budget.seen_cursor_count(),
            super::MAX_RETAINED_SUBAGENT_LINEAGE
        );
        assert_eq!(
            budget.observe_page(Some("cursor-0".to_string())),
            LineagePageAdvance::CursorCycle("cursor-0".to_string())
        );
        assert_eq!(
            budget.seen_cursor_count(),
            super::MAX_RETAINED_SUBAGENT_LINEAGE
        );
    }

    #[test]
    fn incremental_lineage_accepts_descendants_across_pages() {
        let primary_thread_id = ThreadId::new();
        let child_thread_id = ThreadId::new();
        let grandchild_thread_id = ThreadId::new();
        let mut accumulator = LoadedSubagentAccumulator::new(primary_thread_id);

        assert_eq!(
            accumulator
                .ingest(vec![test_thread(
                    grandchild_thread_id,
                    thread_spawn_source(child_thread_id, /*depth*/ 2, "grandchild", "worker"),
                )])
                .len(),
            0
        );
        assert_eq!(accumulator.pending_thread_count(), 1);

        let loaded = accumulator.ingest(vec![test_thread(
            child_thread_id,
            thread_spawn_source(primary_thread_id, /*depth*/ 1, "child", "worker"),
        )]);
        assert_eq!(
            loaded
                .into_iter()
                .map(|thread| thread.thread_id)
                .collect::<HashSet<_>>(),
            HashSet::from([child_thread_id, grandchild_thread_id])
        );
        assert_eq!(accumulator.pending_thread_count(), 0);
    }

    #[test]
    fn completed_lineage_accepts_descendant_behind_hidden_connector_once() {
        let primary_thread_id = ThreadId::new();
        let hidden_connector_id = ThreadId::new();
        let grandchild_thread_id = ThreadId::new();
        let mut accumulator = LoadedSubagentAccumulator::new(primary_thread_id);
        let mut grandchild = test_thread(
            grandchild_thread_id,
            thread_spawn_source(
                hidden_connector_id,
                /*depth*/ 2,
                "grandchild",
                "worker",
            ),
        );
        assert!(accumulator.ingest(vec![grandchild]).is_empty());
        assert_eq!(accumulator.pending_thread_count(), 1);

        let loaded = accumulator.finish();

        assert_eq!(
            loaded
                .into_iter()
                .map(|thread| thread.thread_id)
                .collect::<Vec<_>>(),
            vec![grandchild_thread_id]
        );
        assert_eq!(accumulator.pending_thread_count(), 0);
        assert!(accumulator.finish().is_empty());
    }

    #[test]
    fn completed_lineage_accepts_pending_cycle_once() {
        let primary_thread_id = ThreadId::new();
        let first_thread_id = ThreadId::new();
        let second_thread_id = ThreadId::new();
        let mut accumulator = LoadedSubagentAccumulator::new(primary_thread_id);

        assert!(
            accumulator
                .ingest(vec![
                    test_thread(
                        first_thread_id,
                        thread_spawn_source(second_thread_id, /*depth*/ 2, "first", "worker"),
                    ),
                    test_thread(
                        second_thread_id,
                        thread_spawn_source(first_thread_id, /*depth*/ 3, "second", "worker"),
                    ),
                ])
                .is_empty()
        );

        assert_eq!(
            accumulator
                .finish()
                .into_iter()
                .map(|thread| thread.thread_id)
                .collect::<HashSet<_>>(),
            HashSet::from([first_thread_id, second_thread_id])
        );
        assert_eq!(accumulator.pending_thread_count(), 0);
        assert!(accumulator.finish().is_empty());
    }

    #[test]
    fn wide_and_deep_lineage_walk_follows_each_indexed_edge_once() {
        const WIDTH: usize = 256;
        const DEPTH: usize = 256;
        let primary_thread_id = ThreadId::new();
        let mut threads = Vec::with_capacity(WIDTH + DEPTH);
        for index in 0..WIDTH {
            threads.push(test_thread(
                ThreadId::new(),
                thread_spawn_source(
                    primary_thread_id,
                    /*depth*/ 1,
                    &format!("wide-{index}"),
                    "worker",
                ),
            ));
        }
        let mut parent_thread_id = primary_thread_id;
        for depth in 1..=DEPTH {
            let thread_id = ThreadId::new();
            threads.push(test_thread(
                thread_id,
                thread_spawn_source(parent_thread_id, depth as i32, "deep", "worker"),
            ));
            parent_thread_id = thread_id;
        }

        let (loaded, indexed_edges, followed_edges) =
            find_loaded_subagent_threads_for_primary_with_counts(threads, primary_thread_id);
        assert_eq!(loaded.len(), WIDTH + DEPTH);
        assert_eq!(indexed_edges, WIDTH + DEPTH);
        assert_eq!(followed_edges, WIDTH + DEPTH);
    }
}
