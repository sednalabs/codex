use crate::RolloutItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;

/// Whether a reverse model-context scan needs more rollout items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelContextScanProgress {
    /// The reader should provide the next older rollout item.
    Continue,
    /// The scan has collected a safe bounded suffix.
    Complete,
}

/// Accumulates newest-to-oldest rollout items until they are sufficient to reconstruct the latest
/// model context.
///
/// Storage implementations own how they fetch older items. Local JSONL readers and future
/// reverse-paged cloud readers can both feed their items through this scan to share the cutoff
/// rules and chronological replay assembly.
///
/// The scan stops once it has both:
///
/// - `saw_compaction`: a `CompactedItem` with `replacement_history` and `window_number`;
/// - `saw_completed_turn_context`: a completed user turn with a compatible `TurnContextItem`.
///
/// If the scan reaches the beginning before finding a bounded cutoff, it has already collected
/// the complete replay and so we can return that directly.
///
/// `TurnContextItem` does not identify whether it came from a user turn, so one only counts after
/// the same turn also proves a user-turn boundary: a paginated
/// `ItemCompleted(UserMessage)` marker, agent message, or inter-agent message. Paginated writers
/// persist that marker for real user turns; older rollouts without it conservatively scan to the
/// beginning. A raw `role=user` response item is not sufficient because contextual user fragments
/// use that role but do not count as turn boundaries during reconstruction. The compaction restores
/// model-visible items; the turn context restores previous settings (`model`, `comp_hash`, and
/// `realtime_active`) and the reference baseline.
///
/// These paginated shapes disable the bounded cutoff:
///
/// - compaction without `replacement_history` or `window_number`;
/// - rollback markers;
///
/// When one appears, the scanner continues to the beginning and returns the complete replay.
#[derive(Debug, Default)]
pub struct ModelContextScan {
    items_newest_first: Vec<RolloutItem>,
    saw_compaction: bool,
    saw_completed_turn_context: bool,
    must_scan_to_start: bool,
    active_segment: ActiveTurnSegment,
}

impl ModelContextScan {
    /// Adds the next newest-to-oldest rollout item and reports whether the reader can stop.
    pub fn push(&mut self, item: RolloutItem) -> ModelContextScanProgress {
        let progress = self.observe(&item);
        self.items_newest_first.push(item);
        progress
    }

    /// Returns the collected items in chronological order.
    ///
    /// Call this after the reader reaches the beginning of its source or after [`Self::push`]
    /// returns [`ModelContextScanProgress::Complete`].
    pub fn finish(mut self) -> Vec<RolloutItem> {
        self.items_newest_first.reverse();
        self.items_newest_first
    }

    fn observe(&mut self, item: &RolloutItem) -> ModelContextScanProgress {
        if self.must_scan_to_start {
            return ModelContextScanProgress::Continue;
        }

        match item {
            RolloutItem::Compacted(compacted)
                if compacted.replacement_history.is_none() || compacted.window_number.is_none() =>
            {
                self.must_scan_to_start = true;
            }
            RolloutItem::Compacted(_) => {
                self.saw_compaction = true;
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_)) => {
                // Paginated threads reject rollback. Keep old rollouts correct rather than
                // duplicating rollback survival semantics in this bounded selector.
                self.must_scan_to_start = true;
            }
            RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) => {
                if self.active_segment.turn_id.is_none() {
                    self.active_segment.turn_id = Some(event.turn_id.clone());
                }
                if turn_ids_are_compatible(
                    self.active_segment.turn_id.as_deref(),
                    Some(event.turn_id.as_str()),
                ) {
                    self.active_segment.has_user_turn |=
                        matches!(&event.item, TurnItem::UserMessage(_));
                }
            }
            RolloutItem::EventMsg(EventMsg::TurnComplete(event)) => {
                self.active_segment
                    .turn_id
                    .get_or_insert_with(|| event.turn_id.clone());
            }
            RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => {
                if let Some(turn_id) = &event.turn_id {
                    self.active_segment
                        .turn_id
                        .get_or_insert_with(|| turn_id.clone());
                }
            }
            RolloutItem::EventMsg(EventMsg::TurnStarted(event)) => {
                if turn_ids_are_compatible(
                    self.active_segment.turn_id.as_deref(),
                    Some(event.turn_id.as_str()),
                ) {
                    self.finalize_active_segment();
                }
            }
            RolloutItem::TurnContext(context) => {
                if self.active_segment.turn_id.is_none() {
                    self.active_segment.turn_id = context.turn_id.clone();
                }
                if turn_ids_are_compatible(
                    self.active_segment.turn_id.as_deref(),
                    context.turn_id.as_deref(),
                ) {
                    self.active_segment.has_turn_context = true;
                }
            }
            RolloutItem::ResponseItem(response_item) => {
                self.active_segment.has_user_turn |=
                    response_item_counts_as_user_turn(&response_item.item);
            }
            RolloutItem::InterAgentCommunication(_) => {
                self.active_segment.has_user_turn = true;
            }
            RolloutItem::EventMsg(EventMsg::UserMessage(_)) => {
                self.active_segment.has_user_turn = true;
            }
            _ => {}
        }

        if self.has_bounded_cutoff() {
            ModelContextScanProgress::Complete
        } else {
            ModelContextScanProgress::Continue
        }
    }

    fn finalize_active_segment(&mut self) {
        if self.active_segment.has_user_turn && self.active_segment.has_turn_context {
            self.saw_completed_turn_context = true;
        }
        self.active_segment = ActiveTurnSegment::default();
    }

    fn has_bounded_cutoff(&self) -> bool {
        !self.must_scan_to_start && self.saw_compaction && self.saw_completed_turn_context
    }
}

#[derive(Debug, Default)]
struct ActiveTurnSegment {
    turn_id: Option<String>,
    has_user_turn: bool,
    has_turn_context: bool,
}

fn turn_ids_are_compatible(active_turn_id: Option<&str>, item_turn_id: Option<&str>) -> bool {
    active_turn_id
        .is_none_or(|turn_id| item_turn_id.is_none_or(|item_turn_id| item_turn_id == turn_id))
}

fn response_item_counts_as_user_turn(response_item: &ResponseItem) -> bool {
    match response_item {
        ResponseItem::AgentMessage { .. } => true,
        ResponseItem::Message { role, content, .. } => {
            role == "assistant" && InterAgentCommunication::is_message_content(content)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(value: serde_json::Value) -> RolloutItem {
        serde_json::from_value(value).expect("valid rollout fixture")
    }

    fn event(payload: serde_json::Value) -> RolloutItem {
        item(json!({"type": "event_msg", "payload": payload}))
    }

    fn checkpoint(window: Option<u64>) -> RolloutItem {
        item(json!({"type": "compacted", "payload": {
            "message": "summary", "replacement_history": [], "window_number": window
        }}))
    }

    fn context() -> RolloutItem {
        item(json!({"type": "turn_context", "payload": {
            "turn_id": "turn", "cwd": "/tmp", "approval_policy": "never",
            "sandbox_policy": {"type": "read-only"}, "model": "test", "summary": "auto"
        }}))
    }

    fn started() -> RolloutItem {
        event(json!({"type": "turn_started", "turn_id": "turn"}))
    }

    fn user() -> RolloutItem {
        event(json!({"type": "user_message", "message": "question", "images": null}))
    }

    #[test]
    fn bounded_replay_preserves_action_and_completion_suffix() {
        let mut scan = ModelContextScan::default();
        let suffix = vec![
            checkpoint(Some(1)),
            started(),
            user(),
            context(),
            item(json!({"type": "response_item", "payload": {
                "type": "function_call_output", "call_id": "call-1", "output": "done"
            }})),
            event(json!({"type": "turn_complete", "turn_id": "turn", "last_agent_message": null})),
        ];
        for (index, value) in suffix.into_iter().rev().enumerate() {
            let progress = scan.push(value);
            assert_eq!(
                progress,
                if index == 5 {
                    ModelContextScanProgress::Complete
                } else {
                    ModelContextScanProgress::Continue
                }
            );
        }
        let replay = scan.finish();
        assert_eq!(replay.len(), 6);
        assert!(matches!(&replay[4], RolloutItem::ResponseItem(_)));
        assert!(matches!(
            &replay[5],
            RolloutItem::EventMsg(EventMsg::TurnComplete(_))
        ));
    }

    #[test]
    fn missing_context_rollback_and_incomplete_checkpoint_prevent_cutoff() {
        let mut missing = ModelContextScan::default();
        for value in [user(), started(), checkpoint(Some(1))] {
            assert_eq!(missing.push(value), ModelContextScanProgress::Continue);
        }
        for bad in [
            event(json!({"type": "thread_rolled_back", "num_turns": 1})),
            checkpoint(None),
        ] {
            let mut scan = ModelContextScan::default();
            for value in [bad, context(), user(), started(), checkpoint(Some(1))] {
                assert_eq!(scan.push(value), ModelContextScanProgress::Continue);
            }
        }
    }
}
