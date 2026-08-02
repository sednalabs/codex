//! Multi-agent picker navigation and labeling state for the TUI app.
//!
//! This module exists to keep the pure parts of multi-agent navigation out of [`crate::app::App`].
//! It owns the stable spawn-order cache used by the `/agent` picker, keyboard next/previous
//! navigation, and the contextual footer label for the thread currently being watched.
//!
//! Responsibilities here are intentionally narrow:
//! - remember picker entries and their first-seen order
//! - remember which V2 child threads are owned by their parent agent
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
use crate::multi_agents::SubAgentActivityDisplay;
use crate::multi_agents::format_agent_picker_item_label;
use crate::multi_agents::next_agent_shortcut;
use crate::multi_agents::previous_agent_shortcut;
use codex_protocol::ThreadId;
use ratatui::text::Span;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

/// Retain only recent close notifications that had no corresponding picker row. The picker still
/// preserves tracked closed rows separately; this cap only bounds unmatched notification state.
const CLOSED_THREAD_TOMBSTONE_LIMIT: usize = 256;
/// Retain only recent accepted nonterminal statuses that did not create a picker row. Terminal
/// status provenance is instead owned by [`Self::terminal_lifecycle_watermarks`], which keeps
/// the status revision and activity boundary together through a close/recovery lifecycle.
const UNKNOWN_THREAD_STATUS_PROVENANCE_LIMIT: usize = 256;
/// Terminal lifecycle evidence also protects rows after cache removal. It has a larger, separate
/// bound so tracked close notifications cannot evict an unrelated unmatched-close tombstone.
const TERMINAL_LIFECYCLE_WATERMARK_LIMIT: usize = 1024;
/// Retain every activity identity from a normal child lifecycle, while still bounding the causal
/// history held by each terminal watermark.
const TERMINAL_LIFECYCLE_ACTIVITY_ID_LIMIT: usize = 16;

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
    /// Threads with observed terminal liveness that must not be revived by delayed activity.
    stopped_threads: HashSet<ThreadId>,
    /// Error activities that must see their matching status before a later status can recover
    /// their picker row. Activity and status notifications race across independent streams.
    system_error_epochs: HashMap<ThreadId, SystemErrorEpoch>,
    /// Monotonic error observations per thread. A successful asynchronous `thread/read` may
    /// clear an error only when no newer activity or status observed error evidence in flight.
    system_error_observation_generations: HashMap<ThreadId, u64>,
    /// Latest accepted status revision per thread. Revisions are scoped to one app-server session
    /// and let the picker reject a status watcher message delivered out of order.
    last_status_revisions: HashMap<ThreadId, u64>,
    /// Kind and revision of the latest accepted status. The kind lets a terminal activity that
    /// arrives after an already-observed `SystemError` enter a confirmed error epoch directly.
    last_accepted_statuses: HashMap<ThreadId, AcceptedThreadStatus>,
    /// FIFO ownership for status provenance that did not materialize a picker row and has no
    /// terminal lifecycle watermark. Without this, a stream of unique status-only thread ids
    /// would retain revision state for the lifetime of the TUI session.
    unknown_thread_status_provenance_order: VecDeque<ThreadId>,
    /// Recent terminal notifications for threads that may not have created picker metadata yet.
    /// A late parent activity must not recreate an open row after the child was already closed.
    closed_thread_tombstones: HashMap<ThreadId, ClosedThreadTombstone>,
    /// FIFO eviction order for [`Self::closed_thread_tombstones`].
    closed_thread_tombstone_order: VecDeque<ThreadId>,
    /// Bounded terminal lifecycle evidence retained across picker-row removal. It distinguishes a
    /// stale terminal activity from a new child lifecycle after a status-only recovery.
    terminal_lifecycle_watermarks: HashMap<ThreadId, TerminalLifecycleWatermark>,
    /// FIFO eviction order for [`Self::terminal_lifecycle_watermarks`].
    terminal_lifecycle_watermark_order: VecDeque<ThreadId>,
    /// Bounded activity identities from the currently known lifecycle for each child. They are
    /// transferred into a terminal watermark before a close or explicit cache removal.
    active_lifecycle_activity_ids: HashMap<ThreadId, ActivityIdHistory>,
    /// Spawned child threads whose instructions are owned by their parent agent.
    parent_owned_threads: HashSet<ThreadId>,
    /// Opaque continuation for the next bounded persisted-subagent page.
    next_picker_page_cursor: Option<String>,
    /// Whether this session has completed the bounded legacy relation repair fallback.
    legacy_relation_fallback_checked: bool,
    /// Coalesces picker refreshes and binds their eventual reply to one root lifecycle.
    ///
    /// A refresh request deliberately outlives the picker view itself, so the request number is
    /// monotonic across [`Self::clear`]. That makes an old reply fail closed after a same-root
    /// resume has installed a new lifecycle.
    picker_refresh: Option<PickerRefreshTicket>,
    /// Source for the monotonic request identity in [`Self::picker_refresh`].
    next_picker_refresh_generation: u64,
}

/// Correlates one background picker refresh with the root session that requested it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PickerRefreshTicket {
    root_thread_id: ThreadId,
    lifecycle_generation: u64,
    request_generation: u64,
}

/// The causal state for one activity-derived system error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SystemErrorEpoch {
    /// A terminal activity was observed before its child status watcher reached `SystemError`.
    AwaitingSystemError,
    /// The watcher has confirmed the activity-derived failure; a later status may recover it.
    ConfirmedSystemError {
        /// Revision assigned to the matching `SystemError`, when the server supports it.
        status_revision: Option<u64>,
    },
}

/// The status observation most recently accepted for a picker thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AcceptedThreadStatus {
    has_system_error: bool,
    status_revision: Option<u64>,
}

/// Revision evidence retained for a terminal notification when it carries a status revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ClosedThreadTombstone {
    status_revision: Option<u64>,
}

/// Terminal evidence that survives removal of the corresponding picker row.
#[derive(Clone, Debug, Eq, PartialEq)]
enum TerminalLifecycleWatermark {
    Closed {
        status_revision: Option<u64>,
        activity_ids: ActivityIdHistory,
    },
    Recovered {
        status_revision: u64,
        activity_ids: ActivityIdHistory,
    },
}

impl TerminalLifecycleWatermark {
    fn status_revision(&self) -> Option<u64> {
        match self {
            Self::Closed {
                status_revision, ..
            } => *status_revision,
            Self::Recovered {
                status_revision, ..
            } => Some(*status_revision),
        }
    }

    fn activity_ids(&self) -> &ActivityIdHistory {
        match self {
            Self::Closed { activity_ids, .. } | Self::Recovered { activity_ids, .. } => {
                activity_ids
            }
        }
    }

    fn activity_ids_mut(&mut self) -> &mut ActivityIdHistory {
        match self {
            Self::Closed { activity_ids, .. } | Self::Recovered { activity_ids, .. } => {
                activity_ids
            }
        }
    }
}

/// Bounded activity identities observed during one child lifecycle.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ActivityIdHistory {
    ids: VecDeque<String>,
}

impl ActivityIdHistory {
    fn record(&mut self, activity_id: String) {
        self.ids.retain(|candidate| candidate != &activity_id);
        self.ids.push_back(activity_id);
        while self.ids.len() > TERMINAL_LIFECYCLE_ACTIVITY_ID_LIMIT {
            self.ids.pop_front();
        }
    }

    fn extend(&mut self, other: &Self) {
        for activity_id in &other.ids {
            self.record(activity_id.clone());
        }
    }

    fn contains(&self, activity_id: &str) -> bool {
        self.ids.iter().any(|candidate| candidate == activity_id)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.ids.len()
    }
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
    /// Starts one root-scoped picker refresh, returning `None` when an equivalent refresh is
    /// already in flight. The caller must send all three returned identifiers back with the
    /// completion event so stale work cannot update a later session.
    pub(crate) fn begin_picker_refresh(
        &mut self,
        root_thread_id: ThreadId,
        lifecycle_generation: u64,
    ) -> Option<u64> {
        if self.picker_refresh.is_some() {
            return None;
        }
        self.next_picker_refresh_generation = self.next_picker_refresh_generation.wrapping_add(1);
        let request_generation = self.next_picker_refresh_generation;
        self.picker_refresh = Some(PickerRefreshTicket {
            root_thread_id,
            lifecycle_generation,
            request_generation,
        });
        Some(request_generation)
    }

    /// Consumes the matching in-flight picker refresh. A mismatch is an old response or a reply
    /// for a previous root lifecycle and must not change navigation or picker UI state.
    pub(crate) fn finish_picker_refresh(
        &mut self,
        root_thread_id: ThreadId,
        lifecycle_generation: u64,
        request_generation: u64,
    ) -> bool {
        let ticket = PickerRefreshTicket {
            root_thread_id,
            lifecycle_generation,
            request_generation,
        };
        if self.picker_refresh != Some(ticket) {
            return false;
        }
        self.picker_refresh = None;
        true
    }

    #[cfg(test)]
    pub(crate) fn picker_refresh_ticket_for_test(&self) -> Option<(ThreadId, u64, u64)> {
        self.picker_refresh.map(|ticket| {
            (
                ticket.root_thread_id,
                ticket.lifecycle_generation,
                ticket.request_generation,
            )
        })
    }

    /// Returns the cached picker entry for a specific thread id.
    ///
    /// Callers use this when they already know which thread they care about and need the last
    /// metadata captured for picker or footer rendering. If a caller assumes every tracked thread
    /// must be present here, shutdown races can turn that assumption into a panic elsewhere, so
    /// this stays optional.
    pub(crate) fn get(&self, thread_id: &ThreadId) -> Option<&AgentPickerThreadEntry> {
        self.threads.get(thread_id)
    }

    pub(crate) fn is_parent_owned(&self, thread_id: ThreadId) -> bool {
        self.parent_owned_threads.contains(&thread_id)
    }

    /// Marks a spawned child thread as view-only for direct user instructions.
    pub(crate) fn mark_parent_owned(&mut self, thread_id: ThreadId) {
        self.parent_owned_threads.insert(thread_id);
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
        let stale_terminal_metadata = is_closed
            && self
                .terminal_lifecycle_watermarks
                .get(&thread_id)
                .is_some_and(|watermark| {
                    matches!(watermark, TerminalLifecycleWatermark::Recovered { .. })
                });
        let is_closed = is_closed && !stale_terminal_metadata;
        if is_closed {
            self.record_terminal_lifecycle_closed(thread_id, /*status_revision*/ None);
            self.system_error_epochs.remove(&thread_id);
            self.last_status_revisions.remove(&thread_id);
            self.last_accepted_statuses.remove(&thread_id);
        }
        let previous_entry = self.threads.get(&thread_id);
        let previous_is_running = previous_entry.is_some_and(|entry| entry.is_running);
        let previous_has_system_error = previous_entry.is_some_and(|entry| entry.has_system_error);
        self.upsert_with_path(
            thread_id,
            AgentPickerThreadEntry {
                agent_nickname,
                agent_role,
                agent_path: None,
                model: None,
                reasoning_effort: None,
                model_provider: None,
                task_name: None,
                is_running: previous_is_running && !is_closed,
                is_closed,
                // A backend metadata refresh can lag the child status watch. Keep an activity-
                // derived error until a direct recovery event or terminal closure supersedes it.
                has_system_error: previous_has_system_error && !is_closed,
                created_at,
                updated_at,
            },
        );
    }

    pub(crate) fn upsert_with_path(
        &mut self,
        thread_id: ThreadId,
        mut entry: AgentPickerThreadEntry,
    ) {
        self.remove_unknown_thread_status_provenance_tracking(thread_id);
        let existing = self.threads.get(&thread_id).cloned();
        let stale_terminal_metadata = entry.is_closed
            && self
                .terminal_lifecycle_watermarks
                .get(&thread_id)
                .is_some_and(|watermark| {
                    matches!(watermark, TerminalLifecycleWatermark::Recovered { .. })
                });
        if stale_terminal_metadata {
            // A backfill has no watcher revision and cannot supersede a known recovery. Retain
            // the visible liveness as well as the recovered watermark until a newer terminal
            // status is accepted through the status-watch path.
            entry.is_closed = false;
            entry.is_running = existing.as_ref().is_some_and(|entry| entry.is_running);
            entry.has_system_error = existing
                .as_ref()
                .is_some_and(|entry| entry.has_system_error);
        }
        let preserves_terminal_closure = existing.as_ref().is_some_and(|entry| entry.is_closed)
            || self
                .terminal_lifecycle_watermarks
                .get(&thread_id)
                .is_some_and(|watermark| {
                    matches!(watermark, TerminalLifecycleWatermark::Closed { .. })
                });
        if preserves_terminal_closure {
            // Metadata and backfill reads carry no status revision, so they cannot establish a
            // new lifecycle after a terminal close. Only `reopen_after_newer_status`, after the
            // watcher has accepted a strictly newer nonterminal status, may clear this state.
            entry.is_closed = true;
            entry.is_running = false;
            entry.has_system_error = false;
        }
        if !self.threads.contains_key(&thread_id) {
            self.order.push(thread_id);
        }
        self.threads.insert(
            thread_id,
            AgentPickerThreadEntry {
                agent_path: entry
                    .agent_path
                    .or_else(|| existing.as_ref().and_then(|entry| entry.agent_path.clone())),
                model: entry
                    .model
                    .or_else(|| existing.as_ref().and_then(|entry| entry.model.clone())),
                reasoning_effort: entry.reasoning_effort.or_else(|| {
                    existing
                        .as_ref()
                        .and_then(|entry| entry.reasoning_effort.clone())
                }),
                model_provider: entry.model_provider.or_else(|| {
                    existing
                        .as_ref()
                        .and_then(|entry| entry.model_provider.clone())
                }),
                task_name: entry
                    .task_name
                    .or_else(|| existing.as_ref().and_then(|entry| entry.task_name.clone())),
                created_at: entry
                    .created_at
                    .or(existing.as_ref().and_then(|entry| entry.created_at)),
                updated_at: entry
                    .updated_at
                    .or(existing.as_ref().and_then(|entry| entry.updated_at)),
                ..entry
            },
        );
    }

    pub(crate) fn record_sub_agent_activity(&mut self, activity: SubAgentActivityDisplay) {
        let thread_id = activity.thread_id;
        if self.activity_is_retired_lifecycle_replay(&activity) {
            // A recovered child can have a fresh row by the time the parent replays an older
            // terminal item. Preserve the new row rather than letting that old item recolor it.
            return;
        }
        if !self.threads.contains_key(&thread_id)
            && !self.activity_can_create_missing_row(&activity)
        {
            // A child close/recovery lifecycle remains authoritative after its picker row was
            // removed. Only a distinct `Started` activity can positively establish a new one.
            return;
        }
        let is_errored_activity = activity.has_system_error;
        let accepted_system_error = self
            .last_accepted_statuses
            .get(&thread_id)
            .is_some_and(|status| status.has_system_error);
        let error_epoch = is_errored_activity.then(|| {
            self.last_accepted_statuses
                .get(&thread_id)
                .filter(|status| status.has_system_error)
                .map_or(SystemErrorEpoch::AwaitingSystemError, |status| {
                    SystemErrorEpoch::ConfirmedSystemError {
                        status_revision: status.status_revision,
                    }
                })
        });
        if !self.threads.contains_key(&activity.thread_id) {
            self.remove_unknown_thread_status_provenance_tracking(activity.thread_id);
            self.order.push(activity.thread_id);
        }
        let entry =
            self.threads
                .entry(activity.thread_id)
                .or_insert_with(|| AgentPickerThreadEntry {
                    agent_nickname: None,
                    agent_role: None,
                    agent_path: None,
                    model: None,
                    reasoning_effort: None,
                    model_provider: None,
                    task_name: None,
                    is_running: false,
                    is_closed: false,
                    // A status watcher can report SystemError before any parent activity creates
                    // this picker row. Preserve that accepted status when the row finally
                    // materializes so a delayed Started item cannot invent a green running row.
                    has_system_error: accepted_system_error,
                    created_at: None,
                    updated_at: None,
                });
        entry.agent_path = Some(activity.agent_path);
        if activity.model.is_some() {
            entry.model = activity.model;
        }
        if activity.reasoning_effort.is_some() {
            entry.reasoning_effort = activity.reasoning_effort;
        }
        if entry.is_closed {
            // A `ThreadClosed` transition is terminal for picker liveness. Parent activity can
            // arrive after that transition on an independent notification stream; retain its
            // descriptive metadata above and its identity below, but never let it recolor or
            // revive a closed row.
            self.record_terminal_activity_id(thread_id, activity.activity_id);
            return;
        }
        // A delayed non-error activity must not make a terminal `Errored` activity look like a
        // recovery. Only a direct status transition or closure may clear this state.
        entry.has_system_error |= activity.has_system_error;
        if activity.is_running_hint
            && !entry.is_closed
            && !entry.has_system_error
            && !self.stopped_threads.contains(&activity.thread_id)
        {
            entry.is_running = true;
        } else {
            entry.is_running = false;
            self.stopped_threads.insert(activity.thread_id);
        }
        if let Some(error_epoch) = error_epoch {
            self.system_error_epochs
                .entry(thread_id)
                .or_insert(error_epoch);
        }
        if is_errored_activity {
            self.record_system_error_observation(thread_id);
        }
        self.record_active_activity_id(thread_id, activity.activity_id);
    }

    pub(crate) fn update_identity(
        &mut self,
        thread_id: ThreadId,
        model: Option<String>,
        reasoning_effort: Option<codex_protocol::openai_models::ReasoningEffort>,
        model_provider: Option<String>,
        task_name: Option<String>,
    ) {
        let Some(entry) = self.threads.get_mut(&thread_id) else {
            return;
        };
        if model.is_some() {
            entry.model = model;
        }
        if reasoning_effort.is_some() {
            entry.reasoning_effort = reasoning_effort;
        }
        if model_provider.is_some() {
            entry.model_provider = model_provider;
        }
        if task_name.is_some() {
            entry.task_name = task_name;
        }
    }

    pub(crate) fn mark_running(&mut self, thread_id: ThreadId) {
        let can_mark_running = self
            .threads
            .get(&thread_id)
            .is_some_and(|entry| !entry.is_closed && !entry.has_system_error);
        if !can_mark_running {
            return;
        }
        self.stopped_threads.remove(&thread_id);
        self.set_running(thread_id, /*is_running*/ true);
    }

    pub(crate) fn mark_stopped(&mut self, thread_id: ThreadId) {
        self.stopped_threads.insert(thread_id);
        self.set_running(thread_id, /*is_running*/ false);
    }

    pub(crate) fn set_running(&mut self, thread_id: ThreadId, is_running: bool) {
        if let Some(entry) = self.threads.get_mut(&thread_id) {
            entry.is_running = is_running;
        }
    }

    /// Records the app-server's current error state without hiding a replayable saved thread.
    pub(crate) fn set_system_error(&mut self, thread_id: ThreadId, has_system_error: bool) {
        let mut observed_system_error = false;
        if let Some(entry) = self.threads.get_mut(&thread_id) {
            if entry.is_closed {
                return;
            }
            entry.has_system_error = has_system_error;
            if has_system_error {
                entry.is_running = false;
                self.stopped_threads.insert(thread_id);
                observed_system_error = true;
            }
        }
        if observed_system_error {
            self.record_system_error_observation(thread_id);
        }
    }

    /// Captures the error-observation generation before an asynchronous authoritative liveness
    /// read begins.
    pub(crate) fn system_error_observation_generation(&self, thread_id: ThreadId) -> u64 {
        self.system_error_observation_generations
            .get(&thread_id)
            .copied()
            .unwrap_or_default()
    }

    /// Clears stale error display state from a successful authoritative read only when no newer
    /// activity or status error observation arrived while that read was in flight.
    pub(crate) fn clear_system_error_from_authoritative_read(
        &mut self,
        thread_id: ThreadId,
        observed_generation: u64,
    ) -> bool {
        if self.system_error_observation_generation(thread_id) != observed_generation {
            return false;
        }
        self.system_error_epochs.remove(&thread_id);
        self.set_system_error(thread_id, /*has_system_error*/ false);
        true
    }

    fn record_system_error_observation(&mut self, thread_id: ThreadId) {
        let generation = self
            .system_error_observation_generations
            .entry(thread_id)
            .or_default();
        *generation = generation.saturating_add(1);
    }

    /// Records an authoritative `SystemError` obtained during thread/read or persisted-thread
    /// backfill. Those snapshots do not currently carry a status revision, but they do confirm
    /// an activity-derived error epoch so a later revisioned status notification can recover it.
    pub(crate) fn confirm_system_error_from_authoritative_status(
        &mut self,
        thread_id: ThreadId,
        status_revision: Option<u64>,
    ) {
        if self
            .threads
            .get(&thread_id)
            .is_none_or(|entry| entry.is_closed)
        {
            return;
        }

        self.record_accepted_status(thread_id, /*has_system_error*/ true, status_revision);
        if let Some(epoch) = self.system_error_epochs.get_mut(&thread_id) {
            match epoch {
                SystemErrorEpoch::AwaitingSystemError => {
                    *epoch = SystemErrorEpoch::ConfirmedSystemError { status_revision };
                }
                SystemErrorEpoch::ConfirmedSystemError {
                    status_revision: error_revision,
                } if error_revision.is_none() && status_revision.is_some() => {
                    *error_revision = status_revision;
                }
                SystemErrorEpoch::ConfirmedSystemError { .. } => {}
            }
        }
        self.set_system_error(thread_id, /*has_system_error*/ true);
    }

    /// Returns whether a status-watch update can change the picker liveness for this thread.
    ///
    /// The V2 activity stream and the child status watcher are independent. An `Errored` activity
    /// therefore starts an epoch which ignores a delayed `Idle` or `Active` status until the
    /// watcher reports the matching `SystemError`. Once confirmed, only a strictly newer status
    /// revision may recover the row. A tracked close moves its causal revision into a terminal
    /// lifecycle watermark, so revisionless older servers also fail closed after terminal state.
    /// In particular, there is intentionally no inferred `NotLoaded`-to-`Active` recovery for a
    /// revisionless watcher: independent streams provide no positive correlation proving that the
    /// `Active` observation belongs to a lifecycle newer than the terminal one.
    pub(crate) fn accepts_thread_status_change(
        &mut self,
        thread_id: ThreadId,
        has_system_error: bool,
        status_revision: Option<u64>,
        is_closed: bool,
    ) -> bool {
        if is_closed
            && let Some(TerminalLifecycleWatermark::Recovered {
                status_revision: recovered_revision,
                ..
            }) = self.terminal_lifecycle_watermarks.get(&thread_id)
            && status_revision.is_none_or(|status_revision| status_revision <= *recovered_revision)
        {
            // A recovered watermark is a stronger causal boundary than a delayed or legacy
            // terminal notification. Only a strictly newer terminal revision may close this row.
            return false;
        }

        if let Some(status_revision) = status_revision
            && let Some(latest_revision) = self.last_status_revisions.get(&thread_id).copied()
            && status_revision <= latest_revision
        {
            if is_closed
                && status_revision == latest_revision
                && !self.threads.contains_key(&thread_id)
                && self
                    .closed_thread_tombstones
                    .get(&thread_id)
                    .is_some_and(|tombstone| tombstone.status_revision == Some(status_revision))
            {
                // A duplicate close notification carries no newer lifecycle information, but it
                // does prove this unknown row remains recently relevant for FIFO retention.
                self.record_closed_tombstone(thread_id, Some(status_revision));
            }
            return false;
        }

        if !is_closed && let Some(watermark) = self.terminal_lifecycle_watermarks.get(&thread_id) {
            let Some(status_revision) = status_revision else {
                // A picker row may clear its ordinary revision cache when it closes, but the
                // terminal watermark still owns the causal boundary. Do not let an older server's
                // revisionless status revive that row without positive newer evidence.
                return false;
            };
            if watermark
                .status_revision()
                .is_some_and(|terminal_revision| status_revision <= terminal_revision)
            {
                return false;
            }
        }

        if is_closed {
            let is_unmatched_close = !self.threads.contains_key(&thread_id);
            self.record_accepted_status(
                thread_id,
                /*has_system_error*/ false,
                status_revision,
            );
            self.record_terminal_lifecycle_closed(thread_id, status_revision);
            if is_unmatched_close {
                self.record_closed_tombstone(thread_id, status_revision);
            }
            return true;
        }

        // A status watcher can observe `SystemError` and its newer recovery before a parent
        // activity has materialized a picker row. Retire that status-only error lifecycle now so
        // a delayed old `Errored` activity cannot create an unconfirmable AwaitingSystemError
        // epoch after the child is already healthy again.
        let recovered_status_first_system_error = !has_system_error
            && self.system_error_epochs.get(&thread_id).is_none()
            && self
                .last_accepted_statuses
                .get(&thread_id)
                .is_some_and(|status| status.has_system_error);
        let mut recovered_system_error_lifecycle = false;
        let accepts = match self.system_error_epochs.get(&thread_id).copied() {
            Some(SystemErrorEpoch::AwaitingSystemError) if !has_system_error => false,
            Some(SystemErrorEpoch::AwaitingSystemError) => {
                self.system_error_epochs.insert(
                    thread_id,
                    SystemErrorEpoch::ConfirmedSystemError { status_revision },
                );
                true
            }
            Some(SystemErrorEpoch::ConfirmedSystemError {
                status_revision: error_revision,
            }) if !has_system_error => {
                if match (error_revision, status_revision) {
                    (Some(error_revision), Some(status_revision)) => {
                        status_revision > error_revision
                    }
                    // A `thread/read` or backfill `SystemError` has no revision to compare, but
                    // a subsequently delivered revisioned status is still explicit recovery
                    // evidence. Revisionless status messages continue to fail closed.
                    (None, Some(_)) => true,
                    _ => false,
                } {
                    self.system_error_epochs.remove(&thread_id);
                    recovered_system_error_lifecycle = true;
                    true
                } else {
                    false
                }
            }
            Some(SystemErrorEpoch::ConfirmedSystemError {
                status_revision: error_revision,
            }) => {
                if error_revision.is_none() && status_revision.is_some() {
                    self.system_error_epochs.insert(
                        thread_id,
                        SystemErrorEpoch::ConfirmedSystemError { status_revision },
                    );
                }
                true
            }
            None => true,
        };

        if accepts {
            self.record_accepted_status(thread_id, has_system_error, status_revision);
            self.advance_terminal_lifecycle_for_newer_status(thread_id, status_revision);
            if recovered_system_error_lifecycle || recovered_status_first_system_error {
                self.record_recovered_system_error_lifecycle(thread_id, status_revision);
            }
            self.record_unknown_nonterminal_status_provenance(thread_id);
        }
        accepts
    }

    fn record_accepted_status(
        &mut self,
        thread_id: ThreadId,
        has_system_error: bool,
        status_revision: Option<u64>,
    ) {
        self.last_accepted_statuses.insert(
            thread_id,
            AcceptedThreadStatus {
                has_system_error,
                status_revision,
            },
        );
        if let Some(status_revision) = status_revision {
            self.last_status_revisions
                .insert(thread_id, status_revision);
        }
    }

    fn record_closed_tombstone(&mut self, thread_id: ThreadId, status_revision: Option<u64>) {
        self.closed_thread_tombstone_order
            .retain(|candidate| *candidate != thread_id);
        self.closed_thread_tombstone_order.push_back(thread_id);
        self.closed_thread_tombstones
            .insert(thread_id, ClosedThreadTombstone { status_revision });
        while self.closed_thread_tombstones.len() > CLOSED_THREAD_TOMBSTONE_LIMIT {
            let expired_thread_id = self
                .closed_thread_tombstone_order
                .pop_front()
                .expect("tombstone order must contain every tombstone");
            self.closed_thread_tombstones.remove(&expired_thread_id);
        }
    }

    /// Retains terminal liveness for a child whose `ThreadClosed` notification arrived before
    /// picker metadata. This deliberately does not create a visible closed row: only a later
    /// revisioned recovery plus a distinct Started activity may establish a new lifecycle.
    pub(crate) fn record_unmatched_thread_closed(&mut self, thread_id: ThreadId) {
        if self.threads.contains_key(&thread_id) {
            return;
        }
        self.record_terminal_lifecycle_closed(thread_id, /*status_revision*/ None);
        self.record_closed_tombstone(thread_id, /*status_revision*/ None);
    }

    /// Returns whether a revisionless `ThreadClosed` notification can close this child.
    ///
    /// The app-server protocol supplies only a thread id for `ThreadClosed`, so it cannot prove
    /// that the notification belongs to a lifecycle recovered by a newer watcher status. Keep a
    /// recovered watermark authoritative until a terminal status with newer revision evidence
    /// arrives; otherwise a delayed teardown notification could close the reopened picker row.
    pub(crate) fn accepts_unrevisioned_thread_closed(&self, thread_id: ThreadId) -> bool {
        !matches!(
            self.terminal_lifecycle_watermarks.get(&thread_id),
            Some(TerminalLifecycleWatermark::Recovered { .. })
        )
    }

    /// Returns whether accepted lifecycle evidence says this child is terminal. This includes
    /// close notifications that preceded picker metadata, so teardown does not mistake a stale
    /// local live channel for an app-server session that still needs interruption.
    pub(crate) fn is_terminally_closed(&self, thread_id: ThreadId) -> bool {
        self.threads
            .get(&thread_id)
            .is_some_and(|entry| entry.is_closed)
            || matches!(
                self.terminal_lifecycle_watermarks.get(&thread_id),
                Some(TerminalLifecycleWatermark::Closed { .. })
            )
    }

    /// Records revision provenance for an accepted status-only thread while there is no picker
    /// row or terminal lifecycle watermark to own it. A terminal status moves this provenance to
    /// [`Self::terminal_lifecycle_watermarks`]; a later activity or metadata update moves it to
    /// the tracked picker row. This leaves every unknown-thread path bounded.
    fn record_unknown_nonterminal_status_provenance(&mut self, thread_id: ThreadId) {
        if self.threads.contains_key(&thread_id)
            || self.terminal_lifecycle_watermarks.contains_key(&thread_id)
        {
            return;
        }

        self.unknown_thread_status_provenance_order
            .retain(|candidate| *candidate != thread_id);
        self.unknown_thread_status_provenance_order
            .push_back(thread_id);
        while self.unknown_thread_status_provenance_order.len()
            > UNKNOWN_THREAD_STATUS_PROVENANCE_LIMIT
        {
            let expired_thread_id = self
                .unknown_thread_status_provenance_order
                .pop_front()
                .expect("unknown status provenance order must contain every tracked thread");
            self.clear_unknown_thread_status_provenance(expired_thread_id);
        }
    }

    fn remove_unknown_thread_status_provenance_tracking(&mut self, thread_id: ThreadId) {
        self.unknown_thread_status_provenance_order
            .retain(|candidate| *candidate != thread_id);
    }

    /// Removes status provenance only after the lifecycle evidence that owned an unknown thread
    /// has expired. Never clear a visible row's revision state: it still rejects out-of-order
    /// watcher updates for that live picker entry.
    fn clear_unknown_thread_status_provenance(&mut self, thread_id: ThreadId) {
        if self.threads.contains_key(&thread_id) {
            return;
        }
        self.last_status_revisions.remove(&thread_id);
        self.last_accepted_statuses.remove(&thread_id);
    }

    fn advance_terminal_lifecycle_for_newer_status(
        &mut self,
        thread_id: ThreadId,
        status_revision: Option<u64>,
    ) {
        let Some(status_revision) = status_revision else {
            return;
        };
        let Some(watermark) = self.terminal_lifecycle_watermarks.get(&thread_id).cloned() else {
            return;
        };
        if watermark
            .status_revision()
            .is_some_and(|terminal_revision| status_revision <= terminal_revision)
        {
            return;
        }

        self.record_terminal_lifecycle_watermark(
            thread_id,
            TerminalLifecycleWatermark::Recovered {
                status_revision,
                activity_ids: watermark.activity_ids().clone(),
            },
        );
        // A direct revisioned status has recovered the unmatched close, but retain the lifecycle
        // watermark so an old parent activity cannot recreate the row after this point.
        if self.closed_thread_tombstones.contains_key(&thread_id) {
            self.clear_closed_tombstone(thread_id);
        }
    }

    /// A `SystemError`-to-nonterminal status transition retires the activity epoch that caused
    /// the failure even though the row itself remains open. Preserve that epoch's activity ids
    /// behind the recovered watermark so independently delivered old Errored/Interrupted events
    /// cannot overwrite the newly active row.
    fn record_recovered_system_error_lifecycle(
        &mut self,
        thread_id: ThreadId,
        status_revision: Option<u64>,
    ) {
        let Some(status_revision) = status_revision else {
            return;
        };
        self.remove_unknown_thread_status_provenance_tracking(thread_id);
        let existing = self.terminal_lifecycle_watermarks.get(&thread_id).cloned();
        let mut activity_ids = existing
            .as_ref()
            .map(|watermark| watermark.activity_ids().clone())
            .unwrap_or_default();
        if let Some(active_activity_ids) = self.active_lifecycle_activity_ids.get(&thread_id) {
            activity_ids.extend(active_activity_ids);
        }
        self.active_lifecycle_activity_ids.remove(&thread_id);
        self.record_terminal_lifecycle_watermark(
            thread_id,
            TerminalLifecycleWatermark::Recovered {
                status_revision,
                activity_ids,
            },
        );
    }

    fn record_terminal_lifecycle_closed(
        &mut self,
        thread_id: ThreadId,
        status_revision: Option<u64>,
    ) {
        self.remove_unknown_thread_status_provenance_tracking(thread_id);
        let existing = self.terminal_lifecycle_watermarks.get(&thread_id).cloned();
        let mut activity_ids = existing
            .as_ref()
            .map(|watermark| watermark.activity_ids().clone())
            .unwrap_or_default();
        if let Some(active_activity_ids) = self.active_lifecycle_activity_ids.get(&thread_id) {
            activity_ids.extend(active_activity_ids);
        }
        self.active_lifecycle_activity_ids.remove(&thread_id);
        self.system_error_observation_generations.remove(&thread_id);
        // `mark_closed` and `remove` do not carry a watcher revision, but they
        // may run after we accepted one. Preserve that accepted causal floor
        // before the ordinary cache is cleared below; otherwise an older
        // watcher status could reopen the terminal lifecycle.
        let latest_accepted_revision = self
            .last_status_revisions
            .get(&thread_id)
            .copied()
            .into_iter()
            .chain(
                self.last_accepted_statuses
                    .get(&thread_id)
                    .and_then(|status| status.status_revision),
            )
            .max();
        let status_revision = [
            existing
                .as_ref()
                .and_then(TerminalLifecycleWatermark::status_revision),
            status_revision,
            latest_accepted_revision,
        ]
        .into_iter()
        .flatten()
        .max();
        self.record_terminal_lifecycle_watermark(
            thread_id,
            TerminalLifecycleWatermark::Closed {
                status_revision,
                activity_ids,
            },
        );
    }

    fn record_active_activity_id(&mut self, thread_id: ThreadId, activity_id: String) {
        self.active_lifecycle_activity_ids
            .entry(thread_id)
            .or_default()
            .record(activity_id);
    }

    fn record_terminal_activity_id(&mut self, thread_id: ThreadId, activity_id: String) {
        let Some(mut watermark) = self.terminal_lifecycle_watermarks.get(&thread_id).cloned()
        else {
            return;
        };
        watermark.activity_ids_mut().record(activity_id);
        self.record_terminal_lifecycle_watermark(thread_id, watermark);
    }

    fn record_terminal_lifecycle_watermark(
        &mut self,
        thread_id: ThreadId,
        watermark: TerminalLifecycleWatermark,
    ) {
        self.terminal_lifecycle_watermark_order
            .retain(|candidate| *candidate != thread_id);
        self.terminal_lifecycle_watermark_order.push_back(thread_id);
        self.terminal_lifecycle_watermarks
            .insert(thread_id, watermark);
        while self.terminal_lifecycle_watermarks.len() > TERMINAL_LIFECYCLE_WATERMARK_LIMIT {
            let expired_thread_id = self
                .terminal_lifecycle_watermark_order
                .pop_front()
                .expect("terminal lifecycle watermark order must contain every watermark");
            self.terminal_lifecycle_watermarks
                .remove(&expired_thread_id);
            self.active_lifecycle_activity_ids
                .remove(&expired_thread_id);
            self.system_error_observation_generations
                .remove(&expired_thread_id);
            self.clear_unknown_thread_status_provenance(expired_thread_id);
        }
    }

    fn activity_is_retired_lifecycle_replay(&self, activity: &SubAgentActivityDisplay) -> bool {
        self.terminal_lifecycle_watermarks
            .get(&activity.thread_id)
            .is_some_and(|watermark| watermark.activity_ids().contains(&activity.activity_id))
    }

    fn activity_can_create_missing_row(&mut self, activity: &SubAgentActivityDisplay) -> bool {
        let Some(watermark) = self
            .terminal_lifecycle_watermarks
            .get(&activity.thread_id)
            .cloned()
        else {
            return true;
        };

        match watermark {
            TerminalLifecycleWatermark::Closed { .. } => {
                self.record_terminal_activity_id(activity.thread_id, activity.activity_id.clone());
                false
            }
            TerminalLifecycleWatermark::Recovered { .. } if activity.is_running_hint => {
                // A distinct Started item is the only positive evidence of a new lifecycle. Keep
                // the recovered watermark itself so late identities from the retired lifecycle
                // remain blocked even after this fresh row is created.
                self.active_lifecycle_activity_ids
                    .remove(&activity.thread_id);
                true
            }
            TerminalLifecycleWatermark::Recovered { .. } => {
                self.record_terminal_activity_id(activity.thread_id, activity.activity_id.clone());
                false
            }
        }
    }

    fn clear_closed_tombstone(&mut self, thread_id: ThreadId) {
        if self.closed_thread_tombstones.remove(&thread_id).is_some() {
            self.closed_thread_tombstone_order
                .retain(|candidate| *candidate != thread_id);
        }
    }

    pub(crate) fn set_agent_path(&mut self, thread_id: ThreadId, agent_path: Option<String>) {
        if let Some(agent_path) = agent_path
            && let Some(entry) = self.threads.get_mut(&thread_id)
        {
            entry.agent_path = Some(agent_path);
        }
    }

    pub(crate) fn set_timestamps(
        &mut self,
        thread_id: ThreadId,
        created_at: Option<i64>,
        updated_at: Option<i64>,
    ) {
        if let Some(entry) = self.threads.get_mut(&thread_id) {
            entry.created_at = created_at.or(entry.created_at);
            entry.updated_at = updated_at.or(entry.updated_at);
        }
    }

    /// Marks a thread as closed without removing it from the traversal cache.
    ///
    /// Closed threads stay in the picker and in spawn order so users can still review them and so
    /// next/previous navigation does not reshuffle around disappearing entries. If a caller "cleans
    /// this up" by deleting the entry instead, wraparound navigation will silently change shape
    /// mid-session.
    pub(crate) fn mark_closed(&mut self, thread_id: ThreadId) {
        self.record_terminal_lifecycle_closed(thread_id, /*status_revision*/ None);
        self.system_error_epochs.remove(&thread_id);
        self.last_status_revisions.remove(&thread_id);
        self.last_accepted_statuses.remove(&thread_id);
        if let Some(entry) = self.threads.get_mut(&thread_id) {
            entry.is_closed = true;
            entry.is_running = false;
            entry.has_system_error = false;
        } else {
            self.upsert(
                thread_id, /*agent_nickname*/ None, /*agent_role*/ None,
                /*is_closed*/ true, /*created_at*/ None, /*updated_at*/ None,
            );
        }
    }

    /// Reopens a tracked picker row after [`Self::accepts_thread_status_change`] has accepted a
    /// strictly newer nonterminal status. The caller owns that causal check; this helper only
    /// resets the row so the subsequent status can establish its current running/error state.
    pub(crate) fn reopen_after_newer_status(&mut self, thread_id: ThreadId) {
        let Some(entry) = self.threads.get_mut(&thread_id) else {
            return;
        };
        if !entry.is_closed {
            return;
        }

        entry.is_closed = false;
        entry.is_running = false;
        entry.has_system_error = false;
        self.stopped_threads.remove(&thread_id);
    }

    /// Drops all cached picker state.
    ///
    /// This is used when `App` tears down thread event state and needs the picker cache to return
    /// to a pristine single-session state.
    pub(crate) fn clear(&mut self) {
        self.threads.clear();
        self.order.clear();
        self.stopped_threads.clear();
        self.system_error_epochs.clear();
        self.system_error_observation_generations.clear();
        self.last_status_revisions.clear();
        self.last_accepted_statuses.clear();
        self.unknown_thread_status_provenance_order.clear();
        self.closed_thread_tombstones.clear();
        self.closed_thread_tombstone_order.clear();
        self.terminal_lifecycle_watermarks.clear();
        self.terminal_lifecycle_watermark_order.clear();
        self.active_lifecycle_activity_ids.clear();
        self.parent_owned_threads.clear();
        self.next_picker_page_cursor = None;
        self.legacy_relation_fallback_checked = false;
        self.picker_refresh = None;
    }

    /// Stores the server continuation after a bounded `/agent` backfill.
    pub(crate) fn set_next_picker_page_cursor(&mut self, next_cursor: Option<String>) {
        self.next_picker_page_cursor = next_cursor;
    }

    /// Returns the next persisted-subagent page when one exists.
    pub(crate) fn next_picker_page_cursor(&self) -> Option<String> {
        self.next_picker_page_cursor.clone()
    }

    /// Returns whether the bounded legacy relation fallback still needs a successful pass.
    ///
    /// Callers must mark it complete only after both compatibility probes and every listed
    /// loaded-thread metadata read have returned successfully. This keeps a temporary app-server
    /// failure from making legacy descendants permanently undiscoverable for the rest of the
    /// picker session.
    pub(crate) fn needs_legacy_relation_fallback_check(&self) -> bool {
        !self.legacy_relation_fallback_checked
    }

    /// Records a successful bounded legacy relation fallback pass.
    pub(crate) fn mark_legacy_relation_fallback_checked(&mut self) {
        self.legacy_relation_fallback_checked = true;
    }

    /// Removes a tracked thread entirely from picker metadata and traversal order.
    ///
    /// This is reserved for terminal liveness pruning and explicit side-thread discard. Capture
    /// terminal evidence before removing the row so delayed parent activity cannot recreate a
    /// ghost after either cleanup path.
    pub(crate) fn remove(&mut self, thread_id: ThreadId) {
        self.record_terminal_lifecycle_closed(thread_id, /*status_revision*/ None);
        self.threads.remove(&thread_id);
        self.order.retain(|candidate| *candidate != thread_id);
        self.stopped_threads.remove(&thread_id);
        self.system_error_epochs.remove(&thread_id);
        self.last_status_revisions.remove(&thread_id);
        self.last_accepted_statuses.remove(&thread_id);
        self.clear_closed_tombstone(thread_id);
        // Keep the terminal lifecycle watermark: delayed parent activity can still arrive after
        // this cleanup boundary, while `clear` bounds it to the current session.
        self.active_lifecycle_activity_ids.remove(&thread_id);
        self.parent_owned_threads.remove(&thread_id);
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

    pub(crate) fn ordered_path_backed_subagent_threads(
        &self,
        primary_thread_id: Option<ThreadId>,
    ) -> Vec<(ThreadId, &AgentPickerThreadEntry)> {
        self.ordered_threads()
            .into_iter()
            .filter(|(thread_id, entry)| {
                Some(*thread_id) != primary_thread_id
                    && entry
                        .agent_path
                        .as_deref()
                        .is_some_and(|agent_path| !agent_path.trim().is_empty())
            })
            .collect()
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
                    format_agent_picker_item_label(
                        entry.agent_nickname.as_deref(),
                        entry.agent_role.as_deref(),
                        entry.agent_path.as_deref(),
                        is_primary,
                    )
                })
                .unwrap_or_else(|| {
                    format_agent_picker_item_label(
                        /*agent_nickname*/ None, /*agent_role*/ None,
                        /*agent_path*/ None, is_primary,
                    )
                }),
        )
    }

    #[cfg(test)]
    /// Returns visible picker prefixes for nested agent paths.
    fn picker_tree_prefixes(
        &self,
        primary_thread_id: Option<ThreadId>,
    ) -> HashMap<ThreadId, String> {
        self.picker_tree_layout(primary_thread_id).0
    }

    #[cfg(test)]
    /// Returns visible picker thread ids in parent-first tree order, preserving existing spawn-order
    /// within sibling sets.
    fn picker_tree_thread_ids(&self, primary_thread_id: Option<ThreadId>) -> Vec<ThreadId> {
        self.picker_tree_layout(primary_thread_id).1
    }

    #[cfg(test)]
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

#[cfg(test)]
fn parent_agent_path(path: &str) -> Option<&str> {
    if path.is_empty() || !path.starts_with('/') {
        return None;
    }
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return None;
    }
    let slash_index = path.rfind('/')?;
    if slash_index == 0 {
        return Some("/");
    }
    Some(&path[..slash_index])
}

#[cfg(test)]
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
    fn upsert_preserves_running_state_until_closed() {
        let mut state = AgentNavigationState::default();
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000104").expect("valid thread");
        state.upsert(
            thread_id,
            Some("Scout".to_string()),
            Some("explorer".to_string()),
            /*is_closed*/ false,
            /*created_at*/ Some(1),
            /*updated_at*/ Some(2),
        );
        state.mark_running(thread_id);

        state.upsert(
            thread_id,
            Some("Scout renamed".to_string()),
            Some("worker".to_string()),
            /*is_closed*/ false,
            /*created_at*/ Some(1),
            /*updated_at*/ Some(3),
        );
        assert_eq!(
            state.get(&thread_id),
            Some(&AgentPickerThreadEntry {
                agent_nickname: Some("Scout renamed".to_string()),
                agent_role: Some("worker".to_string()),
                agent_path: None,
                model: None,
                reasoning_effort: None,
                model_provider: None,
                task_name: None,
                is_running: true,
                is_closed: false,
                has_system_error: false,
                created_at: Some(1),
                updated_at: Some(3),
            })
        );

        state.upsert(
            thread_id,
            Some("Scout renamed".to_string()),
            Some("worker".to_string()),
            /*is_closed*/ true,
            /*created_at*/ Some(1),
            /*updated_at*/ Some(4),
        );
        assert_eq!(
            state.get(&thread_id),
            Some(&AgentPickerThreadEntry {
                agent_nickname: Some("Scout renamed".to_string()),
                agent_role: Some("worker".to_string()),
                agent_path: None,
                model: None,
                reasoning_effort: None,
                model_provider: None,
                task_name: None,
                is_running: false,
                is_closed: true,
                has_system_error: false,
                created_at: Some(1),
                updated_at: Some(4),
            })
        );
    }

    #[test]
    fn metadata_upserts_preserve_terminal_closure_until_newer_status_recovers_it() {
        let mut state = AgentNavigationState::default();
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000116").expect("valid thread");
        state.upsert(
            thread_id,
            Some("Finished child".to_string()),
            Some("worker".to_string()),
            /*is_closed*/ false,
            /*created_at*/ Some(1),
            /*updated_at*/ Some(2),
        );
        state.mark_running(thread_id);
        state.mark_closed(thread_id);

        state.upsert(
            thread_id,
            Some("Stale idle metadata".to_string()),
            Some("worker".to_string()),
            /*is_closed*/ false,
            /*created_at*/ Some(1),
            /*updated_at*/ Some(3),
        );
        state.upsert_with_path(
            thread_id,
            AgentPickerThreadEntry {
                agent_path: Some("/root/finished-child".to_string()),
                is_running: true,
                is_closed: false,
                has_system_error: true,
                ..AgentPickerThreadEntry::default()
            },
        );
        assert!(state.get(&thread_id).is_some_and(|entry| {
            entry.is_closed
                && !entry.is_running
                && !entry.has_system_error
                && entry.agent_nickname.as_deref() == Some("Stale idle metadata")
                && entry.agent_path.as_deref() == Some("/root/finished-child")
        }));

        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(8),
            /*is_closed*/ false,
        ));
        state.reopen_after_newer_status(thread_id);
        state.mark_running(thread_id);
        state.upsert(
            thread_id,
            Some("Recovered child".to_string()),
            Some("worker".to_string()),
            /*is_closed*/ false,
            /*created_at*/ Some(1),
            /*updated_at*/ Some(4),
        );
        assert!(
            state
                .get(&thread_id)
                .is_some_and(|entry| !entry.is_closed && entry.is_running)
        );
    }

    #[test]
    fn terminal_metadata_closes_a_row_without_recovered_status_authority() {
        let mut state = AgentNavigationState::default();
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000117").expect("valid thread");
        state.upsert(
            thread_id,
            Some("Current child".to_string()),
            Some("worker".to_string()),
            /*is_closed*/ false,
            /*created_at*/ None,
            /*updated_at*/ None,
        );
        state.mark_running(thread_id);

        state.upsert(
            thread_id,
            Some("Current child".to_string()),
            Some("worker".to_string()),
            /*is_closed*/ true,
            /*created_at*/ None,
            /*updated_at*/ None,
        );

        assert!(state.get(&thread_id).is_some_and(|entry| {
            entry.is_closed && !entry.is_running && !entry.has_system_error
        }));
        assert!(matches!(
            state.terminal_lifecycle_watermarks.get(&thread_id),
            Some(TerminalLifecycleWatermark::Closed { .. })
        ));
    }

    #[test]
    fn authoritative_read_error_clear_rejects_newer_error_observation() {
        let mut state = AgentNavigationState::default();
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000119").expect("valid thread");
        state.upsert(
            thread_id,
            Some("Current child".to_string()),
            Some("worker".to_string()),
            /*is_closed*/ false,
            /*created_at*/ None,
            /*updated_at*/ None,
        );
        state.set_system_error(thread_id, /*has_system_error*/ true);
        let read_generation = state.system_error_observation_generation(thread_id);

        // This activity arrives while the read is in flight. Its newer causal observation must
        // keep the row failed even though the older authoritative response says Idle/Active.
        state.record_sub_agent_activity(SubAgentActivityDisplay {
            activity_id: "activity-119-newer-error".to_string(),
            thread_id,
            agent_path: "/root/current-child".to_string(),
            model: None,
            reasoning_effort: None,
            has_system_error: true,
            is_running_hint: false,
        });
        assert!(!state.clear_system_error_from_authoritative_read(thread_id, read_generation));
        assert!(
            state
                .get(&thread_id)
                .is_some_and(|entry| entry.has_system_error)
        );

        // A subsequent read that starts after the latest error observation is current enough to
        // clear the stale error epoch and allow its non-error status to set liveness.
        let current_generation = state.system_error_observation_generation(thread_id);
        assert!(state.clear_system_error_from_authoritative_read(thread_id, current_generation));
        assert!(
            state
                .get(&thread_id)
                .is_some_and(|entry| !entry.has_system_error)
        );
        assert!(!state.system_error_epochs.contains_key(&thread_id));
    }

    #[test]
    fn terminal_transfer_releases_active_activity_history_with_bounded_watermarks() {
        let mut state = AgentNavigationState::default();
        let first_thread_id = ThreadId::new();
        state.record_active_activity_id(first_thread_id, "activity-first".to_string());
        state.record_terminal_lifecycle_closed(first_thread_id, /*status_revision*/ None);
        assert!(
            !state
                .active_lifecycle_activity_ids
                .contains_key(&first_thread_id),
            "terminal transfer must move activity identities out of the active-lifecycle map"
        );
        assert!(
            state
                .terminal_lifecycle_watermarks
                .get(&first_thread_id)
                .is_some_and(|watermark| watermark.activity_ids().contains("activity-first"))
        );

        for _ in 0..TERMINAL_LIFECYCLE_WATERMARK_LIMIT {
            let thread_id = ThreadId::new();
            state.record_active_activity_id(thread_id, "activity-bounded".to_string());
            state.record_terminal_lifecycle_closed(thread_id, /*status_revision*/ None);
        }
        assert!(state.active_lifecycle_activity_ids.is_empty());
        assert_eq!(
            state.terminal_lifecycle_watermarks.len(),
            TERMINAL_LIFECYCLE_WATERMARK_LIMIT
        );
        assert!(
            !state
                .terminal_lifecycle_watermarks
                .contains_key(&first_thread_id),
            "watermark eviction must leave no parallel active-lifecycle history behind"
        );
    }

    #[test]
    fn errored_activity_stays_failed_across_stale_upserts_until_recovery_or_closure() {
        let mut state = AgentNavigationState::default();
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000105").expect("valid thread");
        state.record_sub_agent_activity(SubAgentActivityDisplay {
            activity_id: "activity-105".to_string(),
            thread_id,
            agent_path: "/root/failed-child".to_string(),
            model: None,
            reasoning_effort: None,
            has_system_error: true,
            is_running_hint: false,
        });

        // A delayed backend upsert only carries metadata and must not overwrite failure liveness.
        state.upsert(
            thread_id,
            Some("Failed child".to_string()),
            Some("worker".to_string()),
            /*is_closed*/ false,
            /*created_at*/ Some(1),
            /*updated_at*/ Some(2),
        );
        assert!(
            state
                .get(&thread_id)
                .is_some_and(|entry| entry.has_system_error && !entry.is_running)
        );

        // TurnStarted and active snapshots are not ordered with the terminal activity and cannot
        // clear it on their own.
        state.mark_running(thread_id);
        assert!(
            state
                .get(&thread_id)
                .is_some_and(|entry| entry.has_system_error && !entry.is_running)
        );

        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ true,
            /*status_revision*/ Some(3),
            /*is_closed*/ false,
        ));
        state.set_system_error(thread_id, /*has_system_error*/ true);
        assert!(!state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(2),
            /*is_closed*/ false,
        ));
        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(4),
            /*is_closed*/ false,
        ));
        state.set_system_error(thread_id, /*has_system_error*/ false);
        state.mark_running(thread_id);
        assert!(
            state
                .get(&thread_id)
                .is_some_and(|entry| !entry.has_system_error && entry.is_running)
        );

        state.mark_closed(thread_id);
        assert!(
            state.get(&thread_id).is_some_and(|entry| entry.is_closed
                && !entry.has_system_error
                && !entry.is_running)
        );
    }

    #[test]
    fn system_error_before_errored_activity_uses_its_revision_for_recovery() {
        let mut state = AgentNavigationState::default();
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000106").expect("valid thread");

        // Status notifications can win the race with a terminal parent activity. Retain the
        // SystemError revision so that the activity enters a confirmed, recoverable epoch.
        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ true,
            /*status_revision*/ Some(3),
            /*is_closed*/ false,
        ));
        state.set_system_error(thread_id, /*has_system_error*/ true);
        state.record_sub_agent_activity(SubAgentActivityDisplay {
            activity_id: "activity-106".to_string(),
            thread_id,
            agent_path: "/root/status-first-failed-child".to_string(),
            model: None,
            reasoning_effort: None,
            has_system_error: true,
            is_running_hint: false,
        });

        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(4),
            /*is_closed*/ false,
        ));
        state.set_system_error(thread_id, /*has_system_error*/ false);
        state.mark_running(thread_id);
        assert!(
            state
                .get(&thread_id)
                .is_some_and(|entry| !entry.has_system_error && entry.is_running)
        );
    }

    #[test]
    fn status_first_system_error_seeds_a_late_started_picker_row() {
        let mut state = AgentNavigationState::default();
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000114").expect("valid thread");

        // A status watcher can win the race before picker metadata or parent activity exists.
        // The later Started item must inherit that accepted SystemError instead of creating a
        // green running row with no error provenance.
        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ true,
            /*status_revision*/ Some(1),
            /*is_closed*/ false,
        ));
        state.record_sub_agent_activity(SubAgentActivityDisplay {
            activity_id: "activity-114-started".to_string(),
            thread_id,
            agent_path: "/root/status-first-started".to_string(),
            model: None,
            reasoning_effort: None,
            has_system_error: false,
            is_running_hint: true,
        });

        assert!(
            state
                .get(&thread_id)
                .is_some_and(|entry| entry.has_system_error && !entry.is_running)
        );
    }

    #[test]
    fn status_first_system_error_recovery_retires_late_errored_activity() {
        let mut state = AgentNavigationState::default();
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000118").expect("valid thread");

        // Both status changes arrive before a parent activity creates a picker row. The newer
        // Active is the recovery boundary for the earlier SystemError, even though no activity
        // epoch exists yet.
        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ true,
            /*status_revision*/ Some(1),
            /*is_closed*/ false,
        ));
        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(2),
            /*is_closed*/ false,
        ));
        assert!(state.is_empty());
        assert!(matches!(
            state.terminal_lifecycle_watermarks.get(&thread_id),
            Some(TerminalLifecycleWatermark::Recovered {
                status_revision: 2,
                ..
            })
        ));

        // The independently delivered old Errored activity cannot create an AwaitingSystemError
        // epoch after that recovery; otherwise every subsequent non-error status would be
        // rejected forever.
        state.record_sub_agent_activity(SubAgentActivityDisplay {
            activity_id: "activity-118-late-errored".to_string(),
            thread_id,
            agent_path: "/root/status-first-recovered-child".to_string(),
            model: None,
            reasoning_effort: None,
            has_system_error: true,
            is_running_hint: false,
        });
        assert!(state.is_empty());
        assert!(!state.system_error_epochs.contains_key(&thread_id));

        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(3),
            /*is_closed*/ false,
        ));
        assert!(state.is_empty());
        assert!(!state.system_error_epochs.contains_key(&thread_id));
        assert!(matches!(
            state.terminal_lifecycle_watermarks.get(&thread_id),
            Some(TerminalLifecycleWatermark::Recovered {
                status_revision: 3,
                ..
            })
        ));

        // A distinct Started activity after the newer status is a genuine new lifecycle and may
        // create the picker row.
        state.record_sub_agent_activity(SubAgentActivityDisplay {
            activity_id: "activity-118-fresh-start".to_string(),
            thread_id,
            agent_path: "/root/status-first-fresh-child".to_string(),
            model: None,
            reasoning_effort: None,
            has_system_error: false,
            is_running_hint: true,
        });
        assert!(state.get(&thread_id).is_some_and(|entry| {
            entry.is_running
                && !entry.is_closed
                && !entry.has_system_error
                && entry.agent_path.as_deref() == Some("/root/status-first-fresh-child")
        }));
    }

    #[test]
    fn system_error_recovery_retires_prior_activity_epoch_before_late_replay() {
        let mut state = AgentNavigationState::default();
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000115").expect("valid thread");

        for (activity_id, has_system_error, is_running_hint) in [
            ("activity-115-started", false, true),
            ("activity-115-interrupted", false, false),
            ("activity-115-errored", true, false),
        ] {
            state.record_sub_agent_activity(SubAgentActivityDisplay {
                activity_id: activity_id.to_string(),
                thread_id,
                agent_path: "/root/recovering-child".to_string(),
                model: None,
                reasoning_effort: None,
                has_system_error,
                is_running_hint,
            });
        }

        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ true,
            /*status_revision*/ Some(3),
            /*is_closed*/ false,
        ));
        state.set_system_error(thread_id, /*has_system_error*/ true);
        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(4),
            /*is_closed*/ false,
        ));
        state.set_system_error(thread_id, /*has_system_error*/ false);
        state.mark_running(thread_id);

        // The rev-4 recovery stays active even when the rev-3 epoch is replayed afterward.
        for (activity_id, has_system_error, is_running_hint) in [
            ("activity-115-errored", true, false),
            ("activity-115-interrupted", false, false),
        ] {
            state.record_sub_agent_activity(SubAgentActivityDisplay {
                activity_id: activity_id.to_string(),
                thread_id,
                agent_path: "/root/recovering-child".to_string(),
                model: None,
                reasoning_effort: None,
                has_system_error,
                is_running_hint,
            });
        }
        assert!(
            state
                .get(&thread_id)
                .is_some_and(|entry| !entry.has_system_error && entry.is_running)
        );

        // A genuinely new lifecycle activity remains valid after the old epoch is retired.
        state.record_sub_agent_activity(SubAgentActivityDisplay {
            activity_id: "activity-115-fresh-start".to_string(),
            thread_id,
            agent_path: "/root/fresh-child".to_string(),
            model: None,
            reasoning_effort: None,
            has_system_error: false,
            is_running_hint: true,
        });
        assert!(state.get(&thread_id).is_some_and(|entry| entry.is_running
            && !entry.has_system_error
            && entry.agent_path.as_deref() == Some("/root/fresh-child")));
    }

    #[test]
    fn hydrated_system_error_confirms_errored_activity_for_revisioned_recovery() {
        let mut state = AgentNavigationState::default();
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000107").expect("valid thread");

        state.record_sub_agent_activity(SubAgentActivityDisplay {
            activity_id: "activity-107".to_string(),
            thread_id,
            agent_path: "/root/hydrated-failed-child".to_string(),
            model: None,
            reasoning_effort: None,
            has_system_error: true,
            is_running_hint: false,
        });
        // `thread/read` and persisted-thread backfill currently provide this authoritative error
        // state without a comparable watcher revision.
        state.confirm_system_error_from_authoritative_status(
            thread_id, /*status_revision*/ None,
        );

        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(4),
            /*is_closed*/ false,
        ));
        state.set_system_error(thread_id, /*has_system_error*/ false);
        state.mark_running(thread_id);
        assert!(
            state
                .get(&thread_id)
                .is_some_and(|entry| !entry.has_system_error && entry.is_running)
        );
    }

    #[test]
    fn closed_status_tombstone_blocks_late_activity_until_newer_status() {
        let mut state = AgentNavigationState::default();
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000108").expect("valid thread");

        // A close notification is meaningful even when no child activity has created a picker
        // row. It must not materialize an unrelated closed row by itself.
        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(3),
            /*is_closed*/ true,
        ));
        assert!(state.is_empty());
        assert!(state.closed_thread_tombstones.contains_key(&thread_id));

        // The parent can receive terminal activity after the child has already closed. Keep the
        // picker empty rather than resurrecting an open, failed row.
        state.record_sub_agent_activity(SubAgentActivityDisplay {
            activity_id: "activity-108".to_string(),
            thread_id,
            agent_path: "/root/closed-before-activity".to_string(),
            model: None,
            reasoning_effort: None,
            has_system_error: true,
            is_running_hint: false,
        });
        assert!(state.is_empty());

        // Stale watcher state cannot remove the tombstone, while a newer direct status can.
        assert!(!state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(2),
            /*is_closed*/ false,
        ));
        assert!(state.closed_thread_tombstones.contains_key(&thread_id));
        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(4),
            /*is_closed*/ false,
        ));
        assert!(!state.closed_thread_tombstones.contains_key(&thread_id));

        state.record_sub_agent_activity(SubAgentActivityDisplay {
            activity_id: "activity-109".to_string(),
            thread_id,
            agent_path: "/root/recovered-child".to_string(),
            model: None,
            reasoning_effort: None,
            has_system_error: false,
            is_running_hint: true,
        });
        assert!(
            state.get(&thread_id).is_some_and(|entry| !entry.is_closed
                && !entry.has_system_error
                && entry.is_running)
        );
    }

    #[test]
    fn recovered_lifecycle_suppresses_stale_errored_activity() {
        let mut state = AgentNavigationState::default();
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000110").expect("valid thread");

        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(3),
            /*is_closed*/ true,
        ));
        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(4),
            /*is_closed*/ false,
        ));
        assert!(!state.closed_thread_tombstones.contains_key(&thread_id));

        // Parent activity is independently delivered and may still describe the closed rev-3
        // lifecycle. The rev-4 recovery removes the public tombstone but keeps this causal guard.
        state.record_sub_agent_activity(SubAgentActivityDisplay {
            activity_id: "activity-110-stale-error".to_string(),
            thread_id,
            agent_path: "/root/recovered-then-stale-error".to_string(),
            model: None,
            reasoning_effort: None,
            has_system_error: true,
            is_running_hint: false,
        });
        assert!(state.is_empty());
    }

    #[test]
    fn remove_retains_closed_lifecycle_guard_against_late_errored_activity() {
        let mut state = AgentNavigationState::default();
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000111").expect("valid thread");

        state.upsert(
            thread_id,
            Some("Finished child".to_string()),
            Some("worker".to_string()),
            /*is_closed*/ false,
            /*created_at*/ None,
            /*updated_at*/ None,
        );
        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(3),
            /*is_closed*/ true,
        ));
        state.mark_closed(thread_id);
        state.remove(thread_id);
        assert!(state.is_empty());
        assert!(state.terminal_lifecycle_watermarks.contains_key(&thread_id));

        state.record_sub_agent_activity(SubAgentActivityDisplay {
            activity_id: "activity-111-late-error".to_string(),
            thread_id,
            agent_path: "/root/removed-then-stale-error".to_string(),
            model: None,
            reasoning_effort: None,
            has_system_error: true,
            is_running_hint: false,
        });
        assert!(state.is_empty());
    }

    #[test]
    fn revisionless_mark_closed_preserves_last_watcher_revision_floor() {
        let mut state = AgentNavigationState::default();
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000120").expect("valid thread");

        state.upsert(
            thread_id,
            Some("Recoverable child".to_string()),
            Some("worker".to_string()),
            /*is_closed*/ false,
            /*created_at*/ None,
            /*updated_at*/ None,
        );
        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(3),
            /*is_closed*/ false,
        ));

        // This cleanup path has no watcher revision of its own, so it must
        // retain the accepted rev-3 floor before clearing the ordinary cache.
        state.mark_closed(thread_id);
        assert!(!state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(1),
            /*is_closed*/ false,
        ));
        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(4),
            /*is_closed*/ false,
        ));

        state.reopen_after_newer_status(thread_id);
        assert!(state.get(&thread_id).is_some_and(|entry| !entry.is_closed));
    }

    #[test]
    fn revisionless_remove_preserves_last_watcher_revision_floor() {
        let mut state = AgentNavigationState::default();
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000121").expect("valid thread");

        state.upsert(
            thread_id,
            Some("Pruned child".to_string()),
            Some("worker".to_string()),
            /*is_closed*/ false,
            /*created_at*/ None,
            /*updated_at*/ None,
        );
        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(3),
            /*is_closed*/ false,
        ));

        state.remove(thread_id);
        assert!(!state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(1),
            /*is_closed*/ false,
        ));
        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(4),
            /*is_closed*/ false,
        ));
    }

    #[test]
    fn remove_records_terminal_guard_for_every_cleanup_path() {
        let mut state = AgentNavigationState::default();
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000112").expect("valid thread");

        state.record_sub_agent_activity(SubAgentActivityDisplay {
            activity_id: "activity-112-started".to_string(),
            thread_id,
            agent_path: "/root/pruned-child".to_string(),
            model: None,
            reasoning_effort: None,
            has_system_error: false,
            is_running_hint: true,
        });

        // `remove` is the common state boundary for terminal thread/read pruning and explicit
        // side-thread discard. It must establish terminal evidence even without a prior close.
        state.remove(thread_id);
        assert!(state.is_empty());
        assert!(state.terminal_lifecycle_watermarks.contains_key(&thread_id));

        for (activity_id, has_system_error, is_running_hint) in [
            ("activity-112-started", false, true),
            ("activity-112-interrupted", false, false),
            ("activity-112-errored", true, false),
        ] {
            state.record_sub_agent_activity(SubAgentActivityDisplay {
                activity_id: activity_id.to_string(),
                thread_id,
                agent_path: "/root/pruned-child".to_string(),
                model: None,
                reasoning_effort: None,
                has_system_error,
                is_running_hint,
            });
            assert!(state.is_empty());
        }
    }

    #[test]
    fn recovered_lifecycle_blocks_all_prior_activity_ids_but_allows_a_fresh_start() {
        let mut state = AgentNavigationState::default();
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000113").expect("valid thread");

        state.record_sub_agent_activity(SubAgentActivityDisplay {
            activity_id: "activity-113-started".to_string(),
            thread_id,
            agent_path: "/root/previous-child".to_string(),
            model: None,
            reasoning_effort: None,
            has_system_error: false,
            is_running_hint: true,
        });
        state.record_sub_agent_activity(SubAgentActivityDisplay {
            activity_id: "activity-113-interrupted".to_string(),
            thread_id,
            agent_path: "/root/previous-child".to_string(),
            model: None,
            reasoning_effort: None,
            has_system_error: false,
            is_running_hint: false,
        });
        state.record_sub_agent_activity(SubAgentActivityDisplay {
            activity_id: "activity-113-errored".to_string(),
            thread_id,
            agent_path: "/root/previous-child".to_string(),
            model: None,
            reasoning_effort: None,
            has_system_error: true,
            is_running_hint: false,
        });
        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(3),
            /*is_closed*/ true,
        ));
        state.mark_closed(thread_id);
        state.remove(thread_id);
        assert!(state.accepts_thread_status_change(
            thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(4),
            /*is_closed*/ false,
        ));

        // Every item observed for the retired lifecycle remains blocked after recovery, not just
        // the last terminal item. None of them can create a row before fresh evidence arrives.
        for (activity_id, has_system_error, is_running_hint) in [
            ("activity-113-started", false, true),
            ("activity-113-interrupted", false, false),
            ("activity-113-errored", true, false),
        ] {
            state.record_sub_agent_activity(SubAgentActivityDisplay {
                activity_id: activity_id.to_string(),
                thread_id,
                agent_path: "/root/previous-child".to_string(),
                model: None,
                reasoning_effort: None,
                has_system_error,
                is_running_hint,
            });
            assert!(state.is_empty());
        }

        state.record_sub_agent_activity(SubAgentActivityDisplay {
            activity_id: "activity-113-fresh-start".to_string(),
            thread_id,
            agent_path: "/root/fresh-child".to_string(),
            model: None,
            reasoning_effort: None,
            has_system_error: false,
            is_running_hint: true,
        });

        // The old Interrupted and Errored items can arrive after the new Started item. They must
        // remain no-ops rather than stopping or failing the fresh picker row.
        for (activity_id, has_system_error, is_running_hint) in [
            ("activity-113-started", false, true),
            ("activity-113-interrupted", false, false),
            ("activity-113-errored", true, false),
        ] {
            state.record_sub_agent_activity(SubAgentActivityDisplay {
                activity_id: activity_id.to_string(),
                thread_id,
                agent_path: "/root/previous-child".to_string(),
                model: None,
                reasoning_effort: None,
                has_system_error,
                is_running_hint,
            });
        }

        assert!(state.get(&thread_id).is_some_and(|entry| entry.is_running
            && !entry.is_closed
            && !entry.has_system_error
            && entry.agent_path.as_deref() == Some("/root/fresh-child")));
        assert!(state.terminal_lifecycle_watermarks.contains_key(&thread_id));
    }

    #[test]
    fn terminal_lifecycle_activity_history_is_bounded() {
        let mut state = AgentNavigationState::default();
        let thread_id = ThreadId::new();
        for index in 0..=TERMINAL_LIFECYCLE_ACTIVITY_ID_LIMIT {
            state.record_sub_agent_activity(SubAgentActivityDisplay {
                activity_id: format!("activity-{index}"),
                thread_id,
                agent_path: "/root/history-bound".to_string(),
                model: None,
                reasoning_effort: None,
                has_system_error: false,
                is_running_hint: true,
            });
        }

        state.mark_closed(thread_id);
        let activity_ids = state
            .terminal_lifecycle_watermarks
            .get(&thread_id)
            .expect("closure records a terminal watermark")
            .activity_ids();
        assert_eq!(activity_ids.len(), TERMINAL_LIFECYCLE_ACTIVITY_ID_LIMIT);
        assert!(!activity_ids.contains("activity-0"));
        assert!(activity_ids.contains(&format!(
            "activity-{}",
            TERMINAL_LIFECYCLE_ACTIVITY_ID_LIMIT
        )));
    }

    #[test]
    fn terminal_lifecycle_watermarks_evict_the_oldest_thread() {
        let mut state = AgentNavigationState::default();
        let first_thread_id = ThreadId::new();
        state.record_terminal_lifecycle_closed(first_thread_id, /*status_revision*/ None);
        for _ in 0..TERMINAL_LIFECYCLE_WATERMARK_LIMIT {
            state.record_terminal_lifecycle_closed(ThreadId::new(), /*status_revision*/ None);
        }

        assert_eq!(
            state.terminal_lifecycle_watermarks.len(),
            TERMINAL_LIFECYCLE_WATERMARK_LIMIT
        );
        assert!(
            !state
                .terminal_lifecycle_watermarks
                .contains_key(&first_thread_id)
        );
    }

    #[test]
    fn terminal_unknown_status_provenance_expires_with_its_lifecycle_watermark() {
        let mut state = AgentNavigationState::default();
        let first_thread_id = ThreadId::new();
        assert!(state.accepts_thread_status_change(
            first_thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(8),
            /*is_closed*/ true,
        ));

        for _ in 0..TERMINAL_LIFECYCLE_WATERMARK_LIMIT {
            assert!(state.accepts_thread_status_change(
                ThreadId::new(),
                /*has_system_error*/ false,
                /*status_revision*/ Some(1),
                /*is_closed*/ true,
            ));
        }

        assert!(
            !state
                .terminal_lifecycle_watermarks
                .contains_key(&first_thread_id),
            "the terminal watermark owns and bounds unknown-close provenance"
        );
        assert!(
            !state.last_status_revisions.contains_key(&first_thread_id)
                && !state.last_accepted_statuses.contains_key(&first_thread_id),
            "the evicted lifecycle must not leave status provenance behind"
        );

        // Once the bounded terminal evidence is gone, an older status is a new unknown stream,
        // not a stale revision that the evicted record can falsely reject. It still cannot create
        // a picker row without independent activity or metadata evidence.
        assert!(state.accepts_thread_status_change(
            first_thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(7),
            /*is_closed*/ false,
        ));
        assert!(state.is_empty());
        assert!(
            !state.accepts_thread_status_change(
                first_thread_id,
                /*has_system_error*/ false,
                /*status_revision*/ Some(6),
                /*is_closed*/ false,
            ),
            "the newly accepted stream must again reject its own stale revision"
        );
    }

    #[test]
    fn unknown_nonterminal_status_provenance_is_fifo_bounded() {
        let mut state = AgentNavigationState::default();
        let first_thread_id = ThreadId::new();
        assert!(state.accepts_thread_status_change(
            first_thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(8),
            /*is_closed*/ false,
        ));

        for _ in 0..UNKNOWN_THREAD_STATUS_PROVENANCE_LIMIT {
            assert!(state.accepts_thread_status_change(
                ThreadId::new(),
                /*has_system_error*/ false,
                /*status_revision*/ Some(1),
                /*is_closed*/ false,
            ));
        }

        assert_eq!(
            state.unknown_thread_status_provenance_order.len(),
            UNKNOWN_THREAD_STATUS_PROVENANCE_LIMIT
        );
        assert!(
            !state.last_status_revisions.contains_key(&first_thread_id)
                && !state.last_accepted_statuses.contains_key(&first_thread_id),
            "evicting an unknown status-only thread must clear both revision and kind evidence"
        );

        // The evicted revision must not be retained as a false causal guard. Like the terminal
        // case above, accepting a new unknown status cannot by itself resurrect a picker row.
        assert!(state.accepts_thread_status_change(
            first_thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(7),
            /*is_closed*/ false,
        ));
        assert!(state.is_empty());
        assert!(
            !state.accepts_thread_status_change(
                first_thread_id,
                /*has_system_error*/ false,
                /*status_revision*/ Some(6),
                /*is_closed*/ false,
            ),
            "the fresh bounded record must still fail closed for its own stale revision"
        );
    }

    #[test]
    fn known_closures_do_not_evict_unmatched_close_tombstones() {
        let mut state = AgentNavigationState::default();
        let unmatched_thread_id = ThreadId::new();
        assert!(state.accepts_thread_status_change(
            unmatched_thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(1),
            /*is_closed*/ true,
        ));

        for _ in 0..(CLOSED_THREAD_TOMBSTONE_LIMIT + 1) {
            let known_thread_id = ThreadId::new();
            state.upsert(
                known_thread_id,
                /*agent_nickname*/ None,
                /*agent_role*/ None,
                /*is_closed*/ false,
                /*created_at*/ None,
                /*updated_at*/ None,
            );
            assert!(state.accepts_thread_status_change(
                known_thread_id,
                /*has_system_error*/ false,
                /*status_revision*/ Some(1),
                /*is_closed*/ true,
            ));
        }

        assert!(
            state
                .closed_thread_tombstones
                .contains_key(&unmatched_thread_id)
        );
        assert_eq!(state.closed_thread_tombstones.len(), 1);
    }

    #[test]
    fn refreshed_unmatched_close_tombstone_moves_to_fifo_back() {
        let mut state = AgentNavigationState::default();
        let refreshed_thread_id = ThreadId::new();
        assert!(state.accepts_thread_status_change(
            refreshed_thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(1),
            /*is_closed*/ true,
        ));
        let first_other_thread_id = ThreadId::new();
        assert!(state.accepts_thread_status_change(
            first_other_thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(1),
            /*is_closed*/ true,
        ));
        for _ in 0..(CLOSED_THREAD_TOMBSTONE_LIMIT - 2) {
            assert!(state.accepts_thread_status_change(
                ThreadId::new(),
                /*has_system_error*/ false,
                /*status_revision*/ Some(1),
                /*is_closed*/ true,
            ));
        }
        assert_eq!(
            state.closed_thread_tombstones.len(),
            CLOSED_THREAD_TOMBSTONE_LIMIT
        );

        // A duplicate close notification is recent again even when it carries no newer revision.
        // The next unrelated close must evict the oldest other tombstone instead.
        assert!(!state.accepts_thread_status_change(
            refreshed_thread_id,
            /*has_system_error*/ false,
            /*status_revision*/ Some(1),
            /*is_closed*/ true,
        ));
        assert!(state.accepts_thread_status_change(
            ThreadId::new(),
            /*has_system_error*/ false,
            /*status_revision*/ Some(1),
            /*is_closed*/ true,
        ));

        assert!(
            state
                .closed_thread_tombstones
                .contains_key(&refreshed_thread_id)
        );
        assert!(
            !state
                .closed_thread_tombstones
                .contains_key(&first_other_thread_id)
        );
    }

    #[test]
    fn parent_owned_state_is_removed_with_thread_metadata() {
        let (mut state, _main_thread_id, first_agent_id, second_agent_id) = populated_state();

        state.mark_parent_owned(first_agent_id);
        assert!(state.is_parent_owned(first_agent_id));
        state.remove(first_agent_id);
        assert!(!state.is_parent_owned(first_agent_id));

        state.mark_parent_owned(second_agent_id);
        state.clear();
        assert!(!state.is_parent_owned(second_agent_id));
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
    fn clear_drops_picker_page_cursor() {
        let mut state = AgentNavigationState::default();
        state.set_next_picker_page_cursor(Some("opaque-cursor".to_string()));
        assert!(state.needs_legacy_relation_fallback_check());
        state.mark_legacy_relation_fallback_checked();
        assert!(!state.needs_legacy_relation_fallback_check());

        state.clear();

        assert_eq!(state.next_picker_page_cursor(), None);
        assert!(state.needs_legacy_relation_fallback_check());
    }

    #[test]
    fn picker_refresh_coalesces_reopens_while_one_request_is_in_flight() {
        let mut state = AgentNavigationState::default();
        let root_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000111").expect("valid root");

        let request_generation = state
            .begin_picker_refresh(root_thread_id, /*lifecycle_generation*/ 7)
            .expect("first refresh starts");
        assert_eq!(
            state.begin_picker_refresh(root_thread_id, /*lifecycle_generation*/ 7),
            None,
            "a second picker open must reuse the in-flight refresh"
        );
        assert!(state.finish_picker_refresh(
            root_thread_id,
            /*lifecycle_generation*/ 7,
            request_generation,
        ));
    }

    #[test]
    fn picker_refresh_rejects_stale_reply_after_session_clear() {
        let mut state = AgentNavigationState::default();
        let root_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000112").expect("valid root");
        let stale_request_generation = state
            .begin_picker_refresh(root_thread_id, /*lifecycle_generation*/ 1)
            .expect("first refresh starts");

        state.clear();
        let current_request_generation = state
            .begin_picker_refresh(root_thread_id, /*lifecycle_generation*/ 2)
            .expect("refresh after reset starts");

        assert!(!state.finish_picker_refresh(
            root_thread_id,
            /*lifecycle_generation*/ 1,
            stale_request_generation,
        ));
        assert!(state.finish_picker_refresh(
            root_thread_id,
            /*lifecycle_generation*/ 2,
            current_request_generation,
        ));
    }

    #[test]
    fn active_agent_label_tracks_current_thread() {
        let (mut state, main_thread_id, first_agent_id, _) = populated_state();
        state.set_agent_path(first_agent_id, Some("/root/explorer".to_string()));

        assert_eq!(
            state.active_agent_label(Some(first_agent_id), Some(main_thread_id)),
            Some("Subagent: Robie [explorer] · /root/explorer".to_string())
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
            AgentPickerThreadEntry {
                agent_path: Some("/root".to_string()),
                ..AgentPickerThreadEntry::default()
            },
        );
        state.upsert_with_path(
            researcher_thread_id,
            AgentPickerThreadEntry {
                agent_nickname: Some("Scout".to_string()),
                agent_role: Some("researcher".to_string()),
                agent_path: Some("/root/researcher".to_string()),
                ..AgentPickerThreadEntry::default()
            },
        );
        state.upsert_with_path(
            worker_thread_id,
            AgentPickerThreadEntry {
                agent_nickname: Some("Builder".to_string()),
                agent_role: Some("worker".to_string()),
                agent_path: Some("/root/researcher/worker".to_string()),
                ..AgentPickerThreadEntry::default()
            },
        );
        state.upsert_with_path(
            reviewer_thread_id,
            AgentPickerThreadEntry {
                agent_nickname: Some("Critic".to_string()),
                agent_role: Some("reviewer".to_string()),
                agent_path: Some("/root/reviewer".to_string()),
                ..AgentPickerThreadEntry::default()
            },
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
            AgentPickerThreadEntry {
                agent_nickname: Some("Worker".to_string()),
                agent_role: Some("worker".to_string()),
                agent_path: Some("/root/primary/worker".to_string()),
                ..AgentPickerThreadEntry::default()
            },
        );
        state.upsert_with_path(
            main_thread_id,
            AgentPickerThreadEntry {
                agent_path: Some("/root/primary".to_string()),
                ..AgentPickerThreadEntry::default()
            },
        );
        state.upsert_with_path(
            critic_thread_id,
            AgentPickerThreadEntry {
                agent_nickname: Some("Critic".to_string()),
                agent_role: Some("reviewer".to_string()),
                agent_path: Some("/root/primary/reviewer".to_string()),
                ..AgentPickerThreadEntry::default()
            },
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
            AgentPickerThreadEntry {
                agent_path: Some("/root/main".to_string()),
                ..AgentPickerThreadEntry::default()
            },
        );
        state.upsert_with_path(
            child_thread_id,
            AgentPickerThreadEntry {
                agent_nickname: Some("Child".to_string()),
                agent_role: Some("child".to_string()),
                agent_path: Some("/root/main/child".to_string()),
                ..AgentPickerThreadEntry::default()
            },
        );

        let prefixes = state.picker_tree_prefixes(Some(main_thread_id));
        assert_eq!(prefixes.get(&main_thread_id), Some(&String::new()));
        assert_eq!(prefixes.get(&child_thread_id), Some(&"└─ ".to_string()));
    }

    #[test]
    fn parent_agent_path_honors_root_and_invalid_inputs() {
        assert_eq!(parent_agent_path("/"), None);
        assert_eq!(parent_agent_path("/root"), Some("/"));
        assert_eq!(parent_agent_path("/root/"), Some("/"));
        assert_eq!(parent_agent_path("/root/researcher/"), Some("/root"));
        assert_eq!(parent_agent_path("root/child"), None);
        assert_eq!(parent_agent_path(""), None);
    }
}
