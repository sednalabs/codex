//! Helpers for truncating rollouts based on "user turn" boundaries.
//!
//! In core, "user turns" are detected by scanning `ResponseItem::Message` items and
//! interpreting them via `event_mapping::parse_turn_item(...)`.

use crate::context_manager::is_user_turn_boundary;
use crate::event_mapping;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::build_turns_from_rollout_items;
use codex_history::InitialHistory;
use codex_history::RolloutItem;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;

pub(crate) fn initial_history_has_prior_user_turns(conversation_history: &InitialHistory) -> bool {
    conversation_history.scan_rollout_items(rollout_item_is_user_turn_boundary)
}

fn rollout_item_is_user_turn_boundary(item: &RolloutItem) -> bool {
    match item {
        RolloutItem::ResponseItem(item) => is_user_turn_boundary(item),
        RolloutItem::Compacted(checkpoint) => checkpoint
            .replacement_history
            .iter()
            .flatten()
            .any(|item| is_user_turn_boundary(item)),
        RolloutItem::InterAgentCommunication(_) => true,
        _ => false,
    }
}

/// Return the indices of user message boundaries in a rollout.
///
/// A user message boundary is a `RolloutItem::ResponseItem(ResponseItem::Message { .. })`
/// whose parsed turn item is `TurnItem::UserMessage`.
///
/// Rollouts can contain `ThreadRolledBack` markers. Those markers indicate that the
/// last N user turns were removed from the effective thread history; we apply them here so
/// indexing uses the post-rollback history rather than the raw stream.
pub(crate) fn user_message_positions_in_rollout(items: &[RolloutItem]) -> Vec<usize> {
    let mut user_positions = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        match item {
            RolloutItem::ResponseItem(item)
                if matches!(&item.item, ResponseItem::Message { .. })
                    && matches!(
                        event_mapping::parse_turn_item(&item.item),
                        Some(TurnItem::UserMessage(_))
                    ) =>
            {
                user_positions.push(idx);
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                let num_turns = usize::try_from(rollback.num_turns).unwrap_or(usize::MAX);
                let new_len = user_positions.len().saturating_sub(num_turns);
                user_positions.truncate(new_len);
            }
            _ => {}
        }
    }
    user_positions
}

#[derive(Clone, Copy)]
enum ForkTurnPosition {
    TopLevel(usize),
    ReplacementHistory {
        checkpoint_index: usize,
        response_index: usize,
    },
}

fn fork_turn_positions_in_rollout(items: &[RolloutItem]) -> Vec<ForkTurnPosition> {
    let mut rollback_turn_positions = Vec::new();
    let mut fork_turn_positions = Vec::new();

    for (index, item) in items.iter().enumerate() {
        match item {
            RolloutItem::Compacted(compacted) => {
                // A replacement checkpoint supersedes all earlier conversation boundaries.
                rollback_turn_positions.clear();
                fork_turn_positions.clear();
                let Some(replacement_history) = compacted.replacement_history.as_ref() else {
                    continue;
                };
                for (response_index, response_item) in replacement_history.iter().enumerate() {
                    let position = ForkTurnPosition::ReplacementHistory {
                        checkpoint_index: index,
                        response_index,
                    };
                    if is_user_turn_boundary(response_item)
                        || is_trigger_turn_boundary(&response_item.item)
                    {
                        rollback_turn_positions.push(position);
                    }
                    if is_real_user_message_boundary(&response_item.item)
                        || is_trigger_turn_boundary(&response_item.item)
                    {
                        fork_turn_positions.push(position);
                    }
                }
            }
            RolloutItem::ResponseItem(response_item) => {
                let position = ForkTurnPosition::TopLevel(index);
                let has_delivery_metadata = matches!(&response_item.item, ResponseItem::AgentMessage { .. })
                    && index.checked_sub(1).is_some_and(|previous_index| {
                        matches!(
                            items.get(previous_index),
                            Some(RolloutItem::InterAgentCommunicationMetadata { .. })
                        )
                    });
                if (is_user_turn_boundary(response_item)
                    || is_trigger_turn_boundary(&response_item.item))
                    && !has_delivery_metadata
                {
                    rollback_turn_positions.push(position);
                }
                if is_real_user_message_boundary(&response_item.item)
                    || is_trigger_turn_boundary(&response_item.item)
                {
                    fork_turn_positions.push(position);
                }
            }
            RolloutItem::InterAgentCommunication(_) => {
                let position = ForkTurnPosition::TopLevel(index);
                rollback_turn_positions.push(position);
                if matches!(item, RolloutItem::InterAgentCommunication(value) if value.trigger_turn) {
                    fork_turn_positions.push(position);
                }
            }
            RolloutItem::InterAgentCommunicationMetadata { trigger_turn } => {
                let position = ForkTurnPosition::TopLevel(index);
                rollback_turn_positions.push(position);
                if *trigger_turn {
                    fork_turn_positions.push(position);
                }
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                let num_turns = usize::try_from(rollback.num_turns).unwrap_or(usize::MAX);
                if num_turns == 0 {
                    continue;
                }
                let rollback_start = rollback_turn_positions
                    .len()
                    .checked_sub(num_turns)
                    .map(|start| rollback_turn_positions[start])
                    .or_else(|| rollback_turn_positions.first().copied());
                if let Some(rollback_start) = rollback_start {
                    rollback_turn_positions.truncate(
                        rollback_turn_positions.len().saturating_sub(num_turns),
                    );
                    fork_turn_positions.retain(|position| position_precedes(*position, rollback_start));
                }
            }
            _ => {}
        }
    }
    fork_turn_positions
}

fn position_precedes(left: ForkTurnPosition, right: ForkTurnPosition) -> bool {
    match (left, right) {
        (ForkTurnPosition::TopLevel(left), ForkTurnPosition::TopLevel(right)) => left < right,
        (
            ForkTurnPosition::ReplacementHistory {
                checkpoint_index: left_checkpoint,
                response_index: left_response,
            },
            ForkTurnPosition::ReplacementHistory {
                checkpoint_index: right_checkpoint,
                response_index: right_response,
            },
        ) => {
            left_checkpoint < right_checkpoint
                || (left_checkpoint == right_checkpoint && left_response < right_response)
        }
        (ForkTurnPosition::ReplacementHistory { checkpoint_index, .. }, ForkTurnPosition::TopLevel(right)) => {
            checkpoint_index < right
        }
        (ForkTurnPosition::TopLevel(left), ForkTurnPosition::ReplacementHistory { checkpoint_index, .. }) => {
            left < checkpoint_index
        }
    }
}

fn normalize_rollbacks_in_fork_suffix(items: &mut Vec<RolloutItem>) {
    loop {
        let Some(rollback_index) = items.iter().position(|item| {
            matches!(item, RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_)))
        }) else {
            return;
        };
        let RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) = &items[rollback_index]
        else {
            unreachable!("rollback index points at rollback event");
        };
        let turns = usize::try_from(rollback.num_turns).unwrap_or(usize::MAX);
        let mut boundaries = Vec::new();
        for (index, item) in items[..rollback_index].iter().enumerate() {
            match item {
                RolloutItem::Compacted(checkpoint) => {
                    boundaries.clear();
                    if let Some(history) = checkpoint.replacement_history.as_ref() {
                        for (response_index, response_item) in history.iter().enumerate() {
                            if is_user_turn_boundary(response_item)
                                || is_trigger_turn_boundary(&response_item.item)
                            {
                                boundaries.push(ForkTurnPosition::ReplacementHistory {
                                    checkpoint_index: index,
                                    response_index,
                                });
                            }
                        }
                    }
                }
                RolloutItem::ResponseItem(response_item) => {
                    let has_delivery_metadata = matches!(
                        &response_item.item,
                        ResponseItem::AgentMessage { .. }
                    ) && index.checked_sub(1).is_some_and(|previous_index| {
                        matches!(
                            items.get(previous_index),
                            Some(RolloutItem::InterAgentCommunicationMetadata { .. })
                        )
                    });
                    if (is_user_turn_boundary(response_item)
                        || is_trigger_turn_boundary(&response_item.item))
                        && !has_delivery_metadata
                    {
                        boundaries.push(ForkTurnPosition::TopLevel(index));
                    }
                }
                RolloutItem::InterAgentCommunication(_)
                | RolloutItem::InterAgentCommunicationMetadata { .. } => {
                    boundaries.push(ForkTurnPosition::TopLevel(index));
                }
                _ => {}
            }
        }
        let first_rolled_back = if turns == 0 || boundaries.is_empty() {
            None
        } else if turns >= boundaries.len() {
            boundaries.first().copied()
        } else {
            Some(boundaries[boundaries.len() - turns])
        };
        match first_rolled_back {
            Some(ForkTurnPosition::TopLevel(start_index)) => {
                items.drain(start_index..=rollback_index);
            }
            Some(ForkTurnPosition::ReplacementHistory {
                checkpoint_index,
                response_index,
            }) => {
                if let Some(RolloutItem::Compacted(checkpoint)) = items.get_mut(checkpoint_index)
                    && let Some(history) = checkpoint.replacement_history.as_mut()
                {
                    history.truncate(response_index);
                }
                items.drain(checkpoint_index + 1..=rollback_index);
            }
            None => {
                items.remove(rollback_index);
            }
        }
    }
}

fn is_real_user_message_boundary(item: &ResponseItem) -> bool {
    matches!(
        event_mapping::parse_turn_item(item),
        Some(TurnItem::UserMessage(_))
    )
}

fn is_trigger_turn_boundary(item: &ResponseItem) -> bool {
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };
    role == "assistant"
        && InterAgentCommunication::from_message_content(content)
            .is_some_and(|communication| communication.trigger_turn)
}

pub(crate) fn truncate_rollout_to_last_n_fork_turns(
    items: &[RolloutItem],
    n_from_end: usize,
) -> Vec<RolloutItem> {
    if n_from_end == 0 {
        return Vec::new();
    }
    let boundaries = fork_turn_positions_in_rollout(items);
    let Some(keep_position) = boundaries
        .len()
        .checked_sub(n_from_end)
        .map(|position| boundaries[position])
        .or_else(|| boundaries.first().copied())
    else {
        return Vec::new();
    };

    let mut suffix = match keep_position {
        ForkTurnPosition::TopLevel(index) => items[index..].to_vec(),
        ForkTurnPosition::ReplacementHistory {
            checkpoint_index,
            response_index,
        } => {
            let mut suffix = items[checkpoint_index..].to_vec();
            if let Some(RolloutItem::Compacted(checkpoint)) = suffix.first_mut()
                && let Some(replacement_history) = checkpoint.replacement_history.as_mut()
            {
                // The summary describes the pre-cut history; the retained replacement history
                // is now the only permitted conversation context for this fork.
                checkpoint.message.clear();
                replacement_history.drain(..response_index);
            }
            suffix
        }
    };
    normalize_rollbacks_in_fork_suffix(&mut suffix);
    suffix
}

/// Return a prefix of `items` obtained by cutting strictly before the nth user message.
///
/// The boundary index is 0-based from the start of `items` (so `n_from_start = 0` returns
/// a prefix that excludes the first user message and everything after it).
///
/// If `n_from_start` is `usize::MAX`, this returns the full rollout (no truncation).
/// If fewer than or equal to `n_from_start` user messages exist, this returns the full
/// rollout unchanged.
pub(crate) fn truncate_rollout_before_nth_user_message_from_start(
    mut items: Vec<RolloutItem>,
    n_from_start: usize,
) -> Vec<RolloutItem> {
    if n_from_start == usize::MAX {
        return items;
    }

    let user_positions = user_message_positions_in_rollout(&items);

    // If fewer than or equal to n user messages exist, keep the full rollout.
    if user_positions.len() <= n_from_start {
        return items;
    }

    // Cut strictly before the nth user message (do not keep the nth itself).
    let cut_idx = user_positions[n_from_start];
    items.truncate(cut_idx);
    items
}

/// Return a rollout prefix ending after the requested persisted terminal turn.
///
/// The turn must still be present in the effective post-rollback history and
/// must have an explicit persisted TurnStarted boundary. Synthetic IDs
/// generated while projecting legacy rollouts are intentionally unsupported
/// because they do not provide a stable raw rollout boundary for a fork.
pub fn truncate_rollout_after_turn_id(
    mut items: Vec<RolloutItem>,
    last_turn_id: &str,
) -> CodexResult<Vec<RolloutItem>> {
    let turns = build_turns_from_rollout_items(&items);
    let turn = turns
        .iter()
        .find(|turn| turn.id == last_turn_id)
        .ok_or_else(|| {
            CodexErr::InvalidRequest(format!(
                "lastTurnId '{last_turn_id}' was not found in the source thread"
            ))
        })?;

    let target_start_index = items
        .iter()
        .position(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(EventMsg::TurnStarted(event))
                    if event.turn_id == last_turn_id
            )
        })
        .ok_or_else(|| {
            CodexErr::InvalidRequest(format!(
                "lastTurnId '{last_turn_id}' is not a persisted canonical turn in the source thread"
            ))
        })?;

    if matches!(turn.status, TurnStatus::InProgress) {
        return Err(CodexErr::InvalidRequest(format!(
            "lastTurnId '{last_turn_id}' identifies an in-progress turn"
        )));
    }

    let cut_index = items
        .iter()
        .enumerate()
        .skip(target_start_index.saturating_add(1))
        .find_map(|(index, item)| {
            matches!(item, RolloutItem::EventMsg(EventMsg::TurnStarted(_))).then_some(index)
        })
        .unwrap_or(items.len());
    items.truncate(cut_index);
    Ok(items)
}

/// Return a rollout prefix ending immediately before the requested persisted turn.
pub fn truncate_rollout_before_turn_id(
    mut items: Vec<RolloutItem>,
    before_turn_id: &str,
) -> CodexResult<Vec<RolloutItem>> {
    let cut_index = items.iter().position(|item| {
        matches!(
            item,
            RolloutItem::EventMsg(EventMsg::TurnStarted(event))
                if event.turn_id == before_turn_id
        )
    });

    let Some(cut_index) = cut_index else {
        // Older rollouts can expose generated turn IDs without a TurnStarted item to fork at.
        if build_turns_from_rollout_items(&items)
            .iter()
            .any(|turn| turn.id == before_turn_id)
        {
            return Err(CodexErr::InvalidRequest(format!(
                "beforeTurnId '{before_turn_id}' is not a persisted canonical turn in the source thread"
            )));
        }

        return Err(CodexErr::InvalidRequest(format!(
            "beforeTurnId '{before_turn_id}' was not found in the source thread"
        )));
    };

    // A persisted turn boundary proves the turn exists unless a later rollback removes it.
    if items[cut_index + 1..]
        .iter()
        .any(|item| matches!(item, RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_))))
        && !build_turns_from_rollout_items(&items)
            .iter()
            .any(|turn| turn.id == before_turn_id)
    {
        return Err(CodexErr::InvalidRequest(format!(
            "beforeTurnId '{before_turn_id}' was not found in the source thread"
        )));
    }

    items.truncate(cut_index);
    Ok(items)
}

#[cfg(test)]
#[path = "thread_rollout_truncation_tests.rs"]
mod tests;
