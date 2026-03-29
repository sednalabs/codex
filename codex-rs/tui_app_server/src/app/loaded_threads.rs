//! Discovers subagent threads that belong to a primary thread by walking spawn-tree edges.
//!
//! When the TUI resumes or switches to an existing thread, it needs to populate
//! `AgentNavigationState` and `ChatWidget` metadata for every subagent that was spawned during
//! that thread's lifetime. The app server exposes a flat list of currently loaded threads via
//! `thread/loaded/list`, but the TUI must figure out which of those are descendants of the
//! primary thread.
//!
//! This module provides the pure, synchronous tree-walk that turns that flat list into the filtered
//! set of descendants. It intentionally has no async, no I/O, and no side effects so it can be
//! unit-tested in isolation.
//!
//! The walk starts from `primary_thread_id` and repeatedly follows
//! `SessionSource::SubAgent(ThreadSpawn { parent_thread_id, .. })` edges until no new children are
//! found. The primary thread itself is never included in the output.

use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::Thread;
use codex_protocol::ThreadId;
use codex_protocol::protocol::SubAgentSource;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

/// A subagent thread discovered by the spawn-tree walk, carrying just enough metadata for the
/// TUI to register it in the navigation cache and rendering metadata map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadedSubagentThread {
    pub(crate) thread_id: ThreadId,
    pub(crate) agent_nickname: Option<String>,
    pub(crate) agent_role: Option<String>,
}

/// Walks the spawn tree rooted at `primary_thread_id` and returns every descendant subagent.
///
/// The walk is breadth-first over `SessionSource::SubAgent(ThreadSpawn { parent_thread_id })`
/// edges. Threads whose `source` is not a `ThreadSpawn`, or whose `parent_thread_id` does not
/// chain back to `primary_thread_id`, are excluded. The primary thread itself is never included.
/// Results are returned in thread creation order, with the UUIDv7 thread id as the tiebreaker.
/// This keeps restored navigation aligned with live spawn order even though
/// `thread/loaded/list` itself is currently UUID-sorted and `created_at` only has second
/// precision.
///
/// If two threads claim the same parent, both are included. Cycles in the parent chain are not
/// possible because `ThreadId`s are server-assigned UUIDs and the server enforces acyclicity, but
/// the `included` set guards against re-visiting regardless.
pub(crate) fn find_loaded_subagent_threads_for_primary(
    threads: Vec<Thread>,
    primary_thread_id: ThreadId,
) -> Vec<LoadedSubagentThread> {
    let mut threads_by_id = HashMap::new();
    let mut children_by_parent = HashMap::new();
    for thread in threads {
        let Ok(thread_id) = ThreadId::from_string(&thread.id) else {
            continue;
        };

        if let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) = &thread.source
        {
            children_by_parent
                .entry(*parent_thread_id)
                .or_insert_with(Vec::new)
                .push(thread_id.clone());
        }

        threads_by_id.insert(thread_id, thread);
    }

    let mut included = HashSet::new();
    let mut pending = VecDeque::from([primary_thread_id]);
    let mut discovery_order = Vec::new();
    while let Some(parent_thread_id) = pending.pop_front() {
        let Some(child_thread_ids) = children_by_parent.get(&parent_thread_id) else {
            continue;
        };

        for child_thread_id in child_thread_ids {
            let child_thread_id = child_thread_id.clone();
            if included.insert(child_thread_id.clone()) {
                discovery_order.push(child_thread_id.clone());
                pending.push_back(child_thread_id);
            }
        }
    }

    let mut loaded_threads: Vec<(i64, String, LoadedSubagentThread)> = discovery_order
        .into_iter()
        .filter_map(|thread_id| {
            threads_by_id.remove(&thread_id).map(|thread| {
                (
                    thread.created_at,
                    thread_id.to_string(),
                    LoadedSubagentThread {
                        thread_id,
                        agent_nickname: thread.agent_nickname,
                        agent_role: thread.agent_role,
                    },
                )
            })
        })
        .collect();
    loaded_threads.sort_by_key(|(created_at, thread_id, _)| (*created_at, thread_id.clone()));
    loaded_threads
        .into_iter()
        .map(|(_, _, thread)| thread)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::LoadedSubagentThread;
    use super::find_loaded_subagent_threads_for_primary;
    use codex_app_server_protocol::SessionSource;
    use codex_app_server_protocol::Thread;
    use codex_app_server_protocol::ThreadStatus;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::SubAgentSource;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    fn test_thread(thread_id: ThreadId, source: SessionSource) -> Thread {
        Thread {
            id: thread_id.to_string(),
            preview: String::new(),
            ephemeral: false,
            model_provider: "openai".to_string(),
            created_at: 0,
            updated_at: 0,
            status: ThreadStatus::Idle,
            path: None,
            cwd: PathBuf::from("/tmp"),
            cli_version: "0.0.0".to_string(),
            source,
            agent_nickname: None,
            agent_role: None,
            git_info: None,
            name: None,
            turns: Vec::new(),
        }
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
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: primary_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: Some("Scout".to_string()),
                agent_role: Some("explorer".to_string()),
            }),
        );
        child.agent_nickname = Some("Scout".to_string());
        child.agent_role = Some("explorer".to_string());

        let mut grandchild = test_thread(
            grandchild_thread_id,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: Some("Atlas".to_string()),
                agent_role: Some("worker".to_string()),
            }),
        );
        grandchild.agent_nickname = Some("Atlas".to_string());
        grandchild.agent_role = Some("worker".to_string());

        let unrelated_child = test_thread(
            unrelated_child_id,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: unrelated_parent_id,
                depth: 1,
                agent_path: None,
                agent_nickname: Some("Other".to_string()),
                agent_role: Some("researcher".to_string()),
            }),
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
                    thread_id: child_thread_id,
                    agent_nickname: Some("Scout".to_string()),
                    agent_role: Some("explorer".to_string()),
                },
                LoadedSubagentThread {
                    thread_id: grandchild_thread_id,
                    agent_nickname: Some("Atlas".to_string()),
                    agent_role: Some("worker".to_string()),
                },
            ]
        );
    }

    #[test]
    fn preserves_created_order_for_loaded_threads() {
        let primary_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000010").expect("valid thread");
        let first_child_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000040").expect("valid thread");
        let second_child_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000030").expect("valid thread");

        let mut first_child = test_thread(
            first_child_id,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: primary_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: Some("Alpha".to_string()),
                agent_role: Some("lead".to_string()),
            }),
        );
        first_child.agent_nickname = Some("Alpha".to_string());
        first_child.agent_role = Some("lead".to_string());
        first_child.created_at = 10;

        let mut second_child = test_thread(
            second_child_id,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: primary_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: Some("Beta".to_string()),
                agent_role: Some("support".to_string()),
            }),
        );
        second_child.agent_nickname = Some("Beta".to_string());
        second_child.agent_role = Some("support".to_string());
        second_child.created_at = 20;

        let loaded = find_loaded_subagent_threads_for_primary(
            vec![
                test_thread(primary_thread_id, SessionSource::Cli),
                second_child,
                first_child,
            ],
            primary_thread_id,
        );

        assert_eq!(
            loaded,
            vec![
                LoadedSubagentThread {
                    thread_id: first_child_id,
                    agent_nickname: Some("Alpha".to_string()),
                    agent_role: Some("lead".to_string()),
                },
                LoadedSubagentThread {
                    thread_id: second_child_id,
                    agent_nickname: Some("Beta".to_string()),
                    agent_role: Some("support".to_string()),
                },
            ]
        );
    }

    #[test]
    fn created_at_ties_break_by_thread_id_order() {
        let primary_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000010").expect("valid thread");
        let earlier_child_id =
            ThreadId::from_string("019d0000-0000-7000-8000-000000000010").expect("valid thread");
        let later_child_id =
            ThreadId::from_string("019d0000-0000-7000-8000-000000000020").expect("valid thread");

        let mut later_child = test_thread(
            later_child_id,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: primary_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: Some("Later".to_string()),
                agent_role: Some("support".to_string()),
            }),
        );
        later_child.agent_nickname = Some("Later".to_string());
        later_child.agent_role = Some("support".to_string());
        later_child.created_at = 10;

        let mut earlier_child = test_thread(
            earlier_child_id,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: primary_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: Some("Earlier".to_string()),
                agent_role: Some("lead".to_string()),
            }),
        );
        earlier_child.agent_nickname = Some("Earlier".to_string());
        earlier_child.agent_role = Some("lead".to_string());
        earlier_child.created_at = 10;

        let loaded = find_loaded_subagent_threads_for_primary(
            vec![
                test_thread(primary_thread_id, SessionSource::Cli),
                later_child,
                earlier_child,
            ],
            primary_thread_id,
        );

        assert_eq!(
            loaded,
            vec![
                LoadedSubagentThread {
                    thread_id: earlier_child_id,
                    agent_nickname: Some("Earlier".to_string()),
                    agent_role: Some("lead".to_string()),
                },
                LoadedSubagentThread {
                    thread_id: later_child_id,
                    agent_nickname: Some("Later".to_string()),
                    agent_role: Some("support".to_string()),
                },
            ]
        );
    }
}
