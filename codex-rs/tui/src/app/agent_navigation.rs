//! Multi-agent picker navigation and labeling state for the TUI app.
//!
//! This module exists to keep the pure parts of multi-agent navigation out of [`crate::app::App`].
//! It owns the stable spawn-order cache used by the `/agent` picker, keyboard next/previous
//! navigation, and the contextual footer label for the thread currently being watched.
//!
//! Responsibilities here are intentionally narrow:
//! - remember picker entries and their first-seen order
//! - answer traversal questions like "what is the next thread?"
//! - derive user-facing picker/footer text from cached thread metadata
//!
//! Responsibilities that stay in `App`:
//! - discovering threads from the backend
//! - deciding which thread is currently displayed
//! - mutating UI state such as switching threads or updating the footer widget
//!
//! The key invariant is that traversal follows first-seen spawn order rather than thread-id sort
//! order. Once a thread id is observed it keeps its place in the cycle even if the entry is later
//! updated or marked closed.

use crate::multi_agents::AgentPickerThreadEntry;
use crate::multi_agents::format_agent_picker_item_name;
use crate::multi_agents::next_agent_shortcut;
use crate::multi_agents::previous_agent_shortcut;
use codex_protocol::ThreadId;
use ratatui::text::Span;
use std::collections::HashMap;

/// Small state container for multi-agent picker ordering and labeling.
///
/// `App` owns thread lifecycle and UI side effects. This type keeps the pure rules for stable
/// spawn-order traversal, picker copy, and active-agent labels together and separately testable.
///
/// The core invariant is that `order` records first-seen thread ids exactly once, while `threads`
/// stores the latest metadata for those ids. Mutation is intentionally funneled through `upsert`,
/// `mark_closed`, and `clear` so those two collections do not drift semantically even if they are
/// temporarily out of sync during teardown races.
#[derive(Debug, Default)]
pub(crate) struct AgentNavigationState {
    /// Latest picker metadata for each tracked thread id.
    threads: HashMap<ThreadId, AgentPickerThreadEntry>,
    /// Stable first-seen traversal order for picker rows and keyboard cycling.
    order: Vec<ThreadId>,
}

/// Direction of keyboard traversal through the stable picker order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentNavigationDirection {
    /// Move toward the entry that was seen earlier in spawn order, wrapping at the front.
    Previous,
    /// Move toward the entry that was seen later in spawn order, wrapping at the end.
    Next,
}

impl AgentNavigationState {
    /// Returns the cached picker entry for a specific thread id.
    ///
    /// Callers use this when they already know which thread they care about and need the last
    /// metadata captured for picker or footer rendering. If a caller assumes every tracked thread
    /// must be present here, shutdown races can turn that assumption into a panic elsewhere, so
    /// this stays optional.
    pub(crate) fn get(&self, thread_id: &ThreadId) -> Option<&AgentPickerThreadEntry> {
        self.threads.get(thread_id)
    }

    /// Returns whether the picker cache currently knows about any threads.
    ///
    /// This is the cheapest way for `App` to decide whether opening the picker should show "No
    /// agents available yet." rather than constructing picker rows from an empty state.
    pub(crate) fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }

    /// Inserts or updates a picker entry while preserving first-seen traversal order.
    ///
    /// The key invariant of this module is enforced here: a thread id is appended to `order` only
    /// the first time it is seen. Later updates may change nickname, role, or closed state, but
    /// they must not move the thread in the cycle or keyboard navigation would feel unstable.
    pub(crate) fn upsert(
        &mut self,
        thread_id: ThreadId,
        agent_nickname: Option<String>,
        agent_role: Option<String>,
        is_closed: bool,
        created_at: Option<i64>,
        updated_at: Option<i64>,
    ) {
        self.upsert_with_path(
            thread_id,
            agent_nickname,
            agent_role,
            /*agent_path*/ None,
            is_closed,
            created_at,
            updated_at,
        );
    }

    pub(crate) fn upsert_with_path(
        &mut self,
        thread_id: ThreadId,
        agent_nickname: Option<String>,
        agent_role: Option<String>,
        agent_path: Option<String>,
        is_closed: bool,
        created_at: Option<i64>,
        updated_at: Option<i64>,
    ) {
        let existing = self.threads.get(&thread_id).cloned();
        if !self.threads.contains_key(&thread_id) {
            self.order.push(thread_id);
        }
        self.threads.insert(
            thread_id,
            AgentPickerThreadEntry {
                agent_nickname,
                agent_role,
                agent_path: agent_path
                    .or(existing.as_ref().and_then(|entry| entry.agent_path.clone())),
                is_closed,
                created_at: created_at.or(existing.as_ref().and_then(|entry| entry.created_at)),
                updated_at: updated_at.or(existing.as_ref().and_then(|entry| entry.updated_at)),
            },
        );
    }

    /// Marks a thread as closed without removing it from the traversal cache.
    ///
    /// Closed threads stay in the picker and in spawn order so users can still review them and so
    /// next/previous navigation does not reshuffle around disappearing entries. If a caller "cleans
    /// this up" by deleting the entry instead, wraparound navigation will silently change shape
    /// mid-session.
    pub(crate) fn mark_closed(&mut self, thread_id: ThreadId) {
        if let Some(entry) = self.threads.get_mut(&thread_id) {
            entry.is_closed = true;
        } else {
            self.upsert(
                thread_id, /*agent_nickname*/ None, /*agent_role*/ None,
                /*is_closed*/ true, /*created_at*/ None, /*updated_at*/ None,
            );
        }
    }

    /// Drops all cached picker state.
    ///
    /// This is used when `App` tears down thread event state and needs the picker cache to return
    /// to a pristine single-session state.
    pub(crate) fn clear(&mut self) {
        self.threads.clear();
        self.order.clear();
    }

    /// Removes a tracked thread entirely from picker metadata and traversal order.
    ///
    /// This is reserved for entries that were only discovered opportunistically and never became
    /// replayable local threads. Keeping those around after the backend confirms they are gone
    /// would leave ghost rows in `/agent`.
    pub(crate) fn remove(&mut self, thread_id: ThreadId) {
        self.threads.remove(&thread_id);
        self.order.retain(|candidate| *candidate != thread_id);
    }

    /// Returns whether there is at least one tracked thread other than the primary one.
    ///
    /// `App` uses this to decide whether the picker should be available even when the collaboration
    /// feature flag is currently disabled, because already-existing sub-agent threads should remain
    /// inspectable.
    pub(crate) fn has_non_primary_thread(&self, primary_thread_id: Option<ThreadId>) -> bool {
        self.threads
            .keys()
            .any(|thread_id| Some(*thread_id) != primary_thread_id)
    }

    /// Returns live picker rows in the same order users cycle through them.
    ///
    /// The `order` vector is intentionally historical and may briefly contain thread ids that no
    /// longer have cached metadata, so this filters through the map instead of assuming both
    /// collections are perfectly synchronized.
    pub(crate) fn ordered_threads(&self) -> Vec<(ThreadId, &AgentPickerThreadEntry)> {
        self.order
            .iter()
            .filter_map(|thread_id| self.threads.get(thread_id).map(|entry| (*thread_id, entry)))
            .collect()
    }

    /// Returns tree connector prefixes keyed by thread id for visible picker rows.
    ///
    /// Prefixes are derived from thread-spawn agent paths when available. The primary thread is
    /// treated as `/root` only when no explicit agent path is available; otherwise we preserve the
    /// primary thread's actual path so descendants are attached to the real parent.
    pub(crate) fn picker_tree_prefixes(
        &self,
        primary_thread_id: Option<ThreadId>,
    ) -> HashMap<ThreadId, String> {
        self.picker_tree_layout(primary_thread_id).0
    }

    /// Returns visible picker thread ids in parent-first tree order, preserving existing spawn-order
    /// within sibling sets.
    pub(crate) fn picker_tree_thread_ids(
        &self,
        primary_thread_id: Option<ThreadId>,
    ) -> Vec<ThreadId> {
        self.picker_tree_layout(primary_thread_id).1
    }

    fn picker_tree_layout(
        &self,
        primary_thread_id: Option<ThreadId>,
    ) -> (HashMap<ThreadId, String>, Vec<ThreadId>) {
        let ordered_threads = self.ordered_threads();
        if ordered_threads.is_empty() {
            return (HashMap::new(), Vec::new());
        }

        let ordered_ids = ordered_threads
            .iter()
            .map(|(thread_id, _)| *thread_id)
            .collect::<Vec<_>>();
        let path_by_thread_id = ordered_threads
            .into_iter()
            .map(|(thread_id, entry)| {
                let path = entry.agent_path.clone().or_else(|| {
                    (Some(thread_id) == primary_thread_id).then_some("/root".to_string())
                });
                (thread_id, path)
            })
            .collect::<HashMap<_, _>>();

        let path_owner = ordered_ids
            .iter()
            .filter_map(|thread_id| {
                path_by_thread_id
                    .get(thread_id)
                    .and_then(|path| path.as_ref().map(|path| (path.clone(), *thread_id)))
            })
            .collect::<HashMap<_, _>>();

        let mut children_by_parent = HashMap::<ThreadId, Vec<ThreadId>>::new();
        let mut roots = Vec::<ThreadId>::new();
        for thread_id in ordered_ids {
            let Some(path) = path_by_thread_id.get(&thread_id).and_then(Option::as_deref) else {
                roots.push(thread_id);
                continue;
            };
            let Some(parent_path) = parent_agent_path(path) else {
                roots.push(thread_id);
                continue;
            };
            if let Some(parent_thread_id) = path_owner.get(parent_path).copied() {
                children_by_parent
                    .entry(parent_thread_id)
                    .or_default()
                    .push(thread_id);
            } else {
                roots.push(thread_id);
            }
        }

        fn visit(
            thread_id: ThreadId,
            continuation_columns: &[bool],
            children_by_parent: &HashMap<ThreadId, Vec<ThreadId>>,
            prefixes: &mut HashMap<ThreadId, String>,
            ordered_thread_ids: &mut Vec<ThreadId>,
        ) {
            prefixes.insert(thread_id, format_tree_prefix(continuation_columns));
            ordered_thread_ids.push(thread_id);
            let Some(children) = children_by_parent.get(&thread_id) else {
                return;
            };
            for (index, child_thread_id) in children.iter().enumerate() {
                let mut child_columns = continuation_columns.to_vec();
                child_columns.push(index + 1 < children.len());
                visit(
                    *child_thread_id,
                    child_columns.as_slice(),
                    children_by_parent,
                    prefixes,
                    ordered_thread_ids,
                );
            }
        }

        let mut prefixes = HashMap::new();
        let mut ordered_thread_ids = Vec::new();
        for root_thread_id in roots {
            visit(
                root_thread_id,
                &[],
                &children_by_parent,
                &mut prefixes,
                &mut ordered_thread_ids,
            );
        }
        (prefixes, ordered_thread_ids)
    }

    /// Returns tracked thread ids in the same stable order used by the picker.
    pub(crate) fn tracked_thread_ids(&self) -> Vec<ThreadId> {
        self.ordered_threads()
            .into_iter()
            .map(|(thread_id, _)| thread_id)
            .collect()
    }

    /// Returns the adjacent thread id for keyboard navigation in stable spawn order.
    ///
    /// The caller must pass the thread whose transcript is actually being shown to the user, not
    /// just whichever thread bookkeeping most recently marked active. If the wrong current thread
    /// is supplied, next/previous navigation will jump in a way that feels nondeterministic even
    /// though the cache itself is correct.
    pub(crate) fn adjacent_thread_id(
        &self,
        current_displayed_thread_id: Option<ThreadId>,
        direction: AgentNavigationDirection,
    ) -> Option<ThreadId> {
        let ordered_threads = self.ordered_threads();
        if ordered_threads.len() < 2 {
            return None;
        }

        let current_thread_id = current_displayed_thread_id?;
        let current_idx = ordered_threads
            .iter()
            .position(|(thread_id, _)| *thread_id == current_thread_id)?;
        let next_idx = match direction {
            AgentNavigationDirection::Next => (current_idx + 1) % ordered_threads.len(),
            AgentNavigationDirection::Previous => {
                if current_idx == 0 {
                    ordered_threads.len() - 1
                } else {
                    current_idx - 1
                }
            }
        };
        Some(ordered_threads[next_idx].0)
    }

    /// Derives the contextual footer label for the currently displayed thread.
    ///
    /// This intentionally returns `None` until there is more than one tracked thread so
    /// single-thread sessions do not waste footer space restating the obvious. When metadata for
    /// the displayed thread is missing, the label falls back to the same generic naming rules used
    /// by the picker.
    pub(crate) fn active_agent_label(
        &self,
        current_displayed_thread_id: Option<ThreadId>,
        primary_thread_id: Option<ThreadId>,
    ) -> Option<String> {
        if self.threads.len() <= 1 {
            return None;
        }

        let thread_id = current_displayed_thread_id?;
        let is_primary = primary_thread_id == Some(thread_id);
        Some(
            self.threads
                .get(&thread_id)
                .map(|entry| {
                    format_agent_picker_item_name(
                        entry.agent_nickname.as_deref(),
                        entry.agent_role.as_deref(),
                        is_primary,
                    )
                })
                .unwrap_or_else(|| {
                    format_agent_picker_item_name(
                        /*agent_nickname*/ None, /*agent_role*/ None, is_primary,
                    )
                }),
        )
    }

    /// Builds the `/agent` picker subtitle from the same canonical bindings used by key handling.
    ///
    /// Keeping this text derived from the actual shortcut helpers prevents the picker copy from
    /// drifting if the bindings ever change on one platform.
    pub(crate) fn picker_subtitle() -> String {
        let previous: Span<'static> = previous_agent_shortcut().into();
        let next: Span<'static> = next_agent_shortcut().into();
        format!(
            "Select an agent to watch. Type to filter; search 'closed' for stale sessions. {} previous, {} next.",
            previous.content, next.content
        )
    }

    #[cfg(test)]
    /// Returns only the ordered thread ids for focused tests of traversal invariants.
    ///
    /// This helper exists so tests can assert on ordering without embedding the full picker entry
    /// payload in every expectation.
    pub(crate) fn ordered_thread_ids(&self) -> Vec<ThreadId> {
        self.ordered_threads()
            .into_iter()
            .map(|(thread_id, _)| thread_id)
            .collect()
    }
}

fn parent_agent_path(path: &str) -> Option<&str> {
    if path.is_empty() || !path.starts_with('/') {
        return None;
    }
    let slash_index = path.rfind('/')?;
    if slash_index == 0 {
        if path.len() == 1 {
            return None;
        }
        return Some("/");
    }
    if slash_index == path.len() - 1 {
        return Some(&path[..slash_index]);
    }
    Some(&path[..slash_index])
}

fn format_tree_prefix(continuation_columns: &[bool]) -> String {
    if continuation_columns.is_empty() {
        return String::new();
    }
    let mut prefix = String::new();
    for has_more_siblings in &continuation_columns[..continuation_columns.len().saturating_sub(1)] {
        if *has_more_siblings {
            prefix.push_str("│  ");
        } else {
            prefix.push_str("   ");
        }
    }
    if continuation_columns.last().copied().unwrap_or(false) {
        prefix.push_str("├─ ");
    } else {
        prefix.push_str("└─ ");
    }
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn populated_state() -> (AgentNavigationState, ThreadId, ThreadId, ThreadId) {
        let mut state = AgentNavigationState::default();
        let main_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000101").expect("valid thread");
        let first_agent_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000102").expect("valid thread");
        let second_agent_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000103").expect("valid thread");

        state.upsert(
            main_thread_id,
            /*agent_nickname*/ None,
            /*agent_role*/ None,
            /*is_closed*/ false,
            /*created_at*/ None,
            /*updated_at*/ None,
        );
        state.upsert(
            first_agent_id,
            Some("Robie".to_string()),
            Some("explorer".to_string()),
            /*is_closed*/ false,
            /*created_at*/ None,
            /*updated_at*/ None,
        );
        state.upsert(
            second_agent_id,
            Some("Bob".to_string()),
            Some("worker".to_string()),
            /*is_closed*/ false,
            /*created_at*/ None,
            /*updated_at*/ None,
        );

        (state, main_thread_id, first_agent_id, second_agent_id)
    }

    #[test]
    fn upsert_preserves_first_seen_order() {
        let (mut state, main_thread_id, first_agent_id, second_agent_id) = populated_state();

        state.upsert(
            first_agent_id,
            Some("Robie".to_string()),
            Some("worker".to_string()),
            /*is_closed*/ true,
            /*created_at*/ None,
            /*updated_at*/ None,
        );

        assert_eq!(
            state.ordered_thread_ids(),
            vec![main_thread_id, first_agent_id, second_agent_id]
        );
    }

    #[test]
    fn adjacent_thread_id_wraps_in_spawn_order() {
        let (state, main_thread_id, first_agent_id, second_agent_id) = populated_state();

        assert_eq!(
            state.adjacent_thread_id(Some(second_agent_id), AgentNavigationDirection::Next),
            Some(main_thread_id)
        );
        assert_eq!(
            state.adjacent_thread_id(Some(second_agent_id), AgentNavigationDirection::Previous),
            Some(first_agent_id)
        );
        assert_eq!(
            state.adjacent_thread_id(Some(main_thread_id), AgentNavigationDirection::Previous),
            Some(second_agent_id)
        );
    }

    #[test]
    fn picker_subtitle_mentions_shortcuts() {
        let previous: Span<'static> = previous_agent_shortcut().into();
        let next: Span<'static> = next_agent_shortcut().into();
        let subtitle = AgentNavigationState::picker_subtitle();

        assert!(subtitle.contains(previous.content.as_ref()));
        assert!(subtitle.contains(next.content.as_ref()));
        assert!(subtitle.contains("closed"));
    }

    #[test]
    fn active_agent_label_tracks_current_thread() {
        let (state, main_thread_id, first_agent_id, _) = populated_state();

        assert_eq!(
            state.active_agent_label(Some(first_agent_id), Some(main_thread_id)),
            Some("Subagent: Robie [explorer]".to_string())
        );
        assert_eq!(
            state.active_agent_label(Some(main_thread_id), Some(main_thread_id)),
            Some("Main [default]".to_string())
        );
    }

    #[test]
    fn picker_tree_prefixes_reflect_nested_agent_paths() {
        let mut state = AgentNavigationState::default();
        let main_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000201").expect("valid thread");
        let researcher_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000202").expect("valid thread");
        let worker_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000203").expect("valid thread");
        let reviewer_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000204").expect("valid thread");

        state.upsert_with_path(
            main_thread_id,
            /*agent_nickname*/ None,
            /*agent_role*/ None,
            Some("/root".to_string()),
            /*is_closed*/ false,
            /*created_at*/ None,
            /*updated_at*/ None,
        );
        state.upsert_with_path(
            researcher_thread_id,
            Some("Scout".to_string()),
            Some("researcher".to_string()),
            Some("/root/researcher".to_string()),
            /*is_closed*/ false,
            /*created_at*/ None,
            /*updated_at*/ None,
        );
        state.upsert_with_path(
            worker_thread_id,
            Some("Builder".to_string()),
            Some("worker".to_string()),
            Some("/root/researcher/worker".to_string()),
            /*is_closed*/ false,
            /*created_at*/ None,
            /*updated_at*/ None,
        );
        state.upsert_with_path(
            reviewer_thread_id,
            Some("Critic".to_string()),
            Some("reviewer".to_string()),
            Some("/root/reviewer".to_string()),
            /*is_closed*/ false,
            /*created_at*/ None,
            /*updated_at*/ None,
        );

        let prefixes = state.picker_tree_prefixes(Some(main_thread_id));
        assert_eq!(prefixes.get(&main_thread_id), Some(&String::new()));
        assert_eq!(
            prefixes.get(&researcher_thread_id),
            Some(&"├─ ".to_string())
        );
        assert_eq!(prefixes.get(&worker_thread_id), Some(&"│  └─ ".to_string()));
        assert_eq!(prefixes.get(&reviewer_thread_id), Some(&"└─ ".to_string()));
    }

    #[test]
    fn picker_tree_respects_parent_first_for_hierarchy_rows() {
        let mut state = AgentNavigationState::default();
        let main_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000301").expect("valid thread");
        let worker_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000302").expect("valid thread");
        let critic_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000303").expect("valid thread");

        state.upsert_with_path(
            worker_thread_id,
            Some("Worker".to_string()),
            Some("worker".to_string()),
            Some("/root/primary/worker".to_string()),
            /*is_closed*/ false,
            /*created_at*/ None,
            /*updated_at*/ None,
        );
        state.upsert_with_path(
            main_thread_id,
            None,
            None,
            Some("/root/primary".to_string()),
            /*is_closed*/ false,
            /*created_at*/ None,
            /*updated_at*/ None,
        );
        state.upsert_with_path(
            critic_thread_id,
            Some("Critic".to_string()),
            Some("reviewer".to_string()),
            Some("/root/primary/reviewer".to_string()),
            /*is_closed*/ false,
            /*created_at*/ None,
            /*updated_at*/ None,
        );

        let tree_order = state.picker_tree_thread_ids(Some(main_thread_id));
        assert_eq!(
            tree_order,
            vec![main_thread_id, worker_thread_id, critic_thread_id]
        );

        let prefixes = state.picker_tree_prefixes(Some(main_thread_id));
        assert_eq!(prefixes.get(&main_thread_id), Some(&String::new()));
        assert_eq!(prefixes.get(&worker_thread_id), Some(&"├─ ".to_string()));
        assert_eq!(prefixes.get(&critic_thread_id), Some(&"└─ ".to_string()));
    }

    #[test]
    fn picker_tree_preserves_primary_path_when_available() {
        let mut state = AgentNavigationState::default();
        let main_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000401").expect("valid thread");
        let child_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000402").expect("valid thread");

        state.upsert_with_path(
            main_thread_id,
            None,
            None,
            Some("/root/main".to_string()),
            /*is_closed*/ false,
            /*created_at*/ None,
            /*updated_at*/ None,
        );
        state.upsert_with_path(
            child_thread_id,
            Some("Child".to_string()),
            Some("child".to_string()),
            Some("/root/main/child".to_string()),
            /*is_closed*/ false,
            /*created_at*/ None,
            /*updated_at*/ None,
        );

        let prefixes = state.picker_tree_prefixes(Some(main_thread_id));
        assert_eq!(prefixes.get(&main_thread_id), Some(&String::new()));
        assert_eq!(prefixes.get(&child_thread_id), Some(&"└─ ".to_string()));
    }

    #[test]
    fn parent_agent_path_honors_root_and_invalid_inputs() {
        assert_eq!(parent_agent_path("/"), None);
        assert_eq!(parent_agent_path("/root"), Some("/"));
        assert_eq!(parent_agent_path("root/child"), None);
        assert_eq!(parent_agent_path(""), None);
    }
}
