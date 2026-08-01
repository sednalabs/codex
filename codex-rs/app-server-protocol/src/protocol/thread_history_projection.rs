//! Incremental projection from canonical paginated rollout records to thread-history changes.
//!
//! This module is only for the new paginated rollout format that persists canonical
//! item lifecycle records, not legacy event-only rollouts.

use std::collections::HashMap;

use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;

use crate::protocol::thread_history::ThreadHistoryChangeSet;
use crate::protocol::thread_history::ThreadHistoryItemChange;
use crate::protocol::thread_history::ThreadHistoryTurnChange;
use crate::protocol::v2::ThreadItem;
use crate::protocol::v2::TurnError;
use crate::protocol::v2::TurnStatus;
use crate::protocol::v2::merge_spawn_request_provenance;

/// Stateful projector for a canonical paginated rollout suffix.
///
/// A spawn request is present only on its `ItemStarted` snapshot, while the
/// terminal `ItemCompleted` snapshot records the effective selected values.
/// Keep the optional request provenance by the exact `(turn_id, item_id)` pair
/// until the matching terminal item arrives.
#[derive(Default)]
pub struct ThreadHistoryProjector {
    spawn_starts: HashMap<(String, String), ThreadItem>,
}

impl ThreadHistoryProjector {
    /// Project one durable rollout line in ordinal order.
    pub fn project_rollout_line(&mut self, line: &RolloutLine) -> ThreadHistoryChangeSet {
        match &line.item {
            RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => ThreadHistoryChangeSet {
                changed_turns: vec![ThreadHistoryTurnChange {
                    turn_id: event.turn_id.clone(),
                    status: TurnStatus::InProgress,
                    error: None,
                    started_at: event.started_at,
                    completed_at: None,
                    duration_ms: None,
                }],
                ..Default::default()
            },
            RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => ThreadHistoryChangeSet {
                changed_turns: vec![ThreadHistoryTurnChange {
                    turn_id: event.turn_id.clone(),
                    status: if event.error.is_some() {
                        TurnStatus::Failed
                    } else {
                        TurnStatus::Completed
                    },
                    error: event.error.as_ref().map(|error| TurnError {
                        message: error.message.clone(),
                        codex_error_info: error.codex_error_info.clone().map(Into::into),
                        additional_details: None,
                    }),
                    started_at: event.started_at,
                    completed_at: event.completed_at,
                    duration_ms: event.duration_ms,
                }],
                ..Default::default()
            },
            RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => {
                let Some(turn_id) = event.turn_id.as_ref() else {
                    return ThreadHistoryChangeSet::default();
                };
                ThreadHistoryChangeSet {
                    changed_turns: vec![ThreadHistoryTurnChange {
                        turn_id: turn_id.clone(),
                        status: TurnStatus::Interrupted,
                        error: None,
                        started_at: event.started_at,
                        completed_at: event.completed_at,
                        duration_ms: event.duration_ms,
                    }],
                    ..Default::default()
                }
            }
            RolloutItem::EventMsg(EventMsg::ItemStarted(event)) => {
                let item = ThreadItem::from(event.item.clone());
                if is_spawn_start(&item) {
                    self.spawn_starts
                        .insert((event.turn_id.clone(), item.id().to_string()), item.clone());
                    // Persist the start snapshot as the durable source of request provenance.
                    // The matching completion replaces it with effective values, retaining those
                    // optional request fields below.
                    ThreadHistoryChangeSet {
                        changed_items: vec![ThreadHistoryItemChange {
                            turn_id: event.turn_id.clone(),
                            item,
                        }],
                        ..Default::default()
                    }
                } else {
                    ThreadHistoryChangeSet::default()
                }
            }
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => {
                let mut item = ThreadItem::from(event.item.clone());
                if let Some(started_item) = self
                    .spawn_starts
                    .remove(&(event.turn_id.clone(), item.id().to_string()))
                {
                    merge_spawn_request_provenance(&mut item, &started_item);
                }
                ThreadHistoryChangeSet {
                    changed_items: vec![ThreadHistoryItemChange {
                        turn_id: event.turn_id.clone(),
                        item,
                    }],
                    ..Default::default()
                }
            }
            RolloutItem::SessionMeta(_)
            | RolloutItem::ResponseItem(_)
            | RolloutItem::InterAgentCommunication(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::Compacted(_)
            | RolloutItem::TurnContext(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::EventMsg(_) => ThreadHistoryChangeSet::default(),
        }
    }
}

fn is_spawn_start(item: &ThreadItem) -> bool {
    matches!(
        item,
        ThreadItem::CollabAgentToolCall {
            tool: crate::protocol::v2::CollabAgentTool::SpawnAgent,
            status: crate::protocol::v2::CollabAgentToolCallStatus::InProgress,
            ..
        }
    )
}

/// Project one durable rollout line without retaining earlier lifecycle state.
///
/// Callers that replay a JSONL suffix should prefer [`ThreadHistoryProjector`]
/// and invoke it once per line in ordinal order. This compatibility helper is
/// suitable only for records whose projection does not need an earlier start.
pub fn project_rollout_line(line: &RolloutLine) -> ThreadHistoryChangeSet {
    ThreadHistoryProjector::default().project_rollout_line(line)
}

#[cfg(test)]
#[path = "thread_history_projection_tests.rs"]
mod tests;
