//! Bounded replay-buffer policy for per-thread app-server events.

use super::ServerNotification;
use super::ThreadBufferedEvent;
use super::ThreadEventStore;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::ThreadItem;
use std::borrow::Cow;

// Keep merged text finite so continued streaming still reaches bounded replay eviction.
const MAX_COALESCED_AGENT_MESSAGE_DELTA_BYTES: usize = 4 * 1024;
const MAX_BUFFERED_AGENT_MESSAGE_DELTA_BYTES: usize = 256 * 1024;

impl ThreadEventStore {
    pub(super) fn push_replay_notification(&mut self, notification: Cow<'_, ServerNotification>) {
        if let ServerNotification::AgentMessageDelta(delta) = notification.as_ref()
            && delta.delta.len() > MAX_BUFFERED_AGENT_MESSAGE_DELTA_BYTES
        {
            return;
        }

        if let ServerNotification::AgentMessageDelta(delta) = notification.as_ref()
            && let Some(ThreadBufferedEvent::Notification(previous)) = self.buffer.back_mut()
            && let ServerNotification::AgentMessageDelta(previous) = previous.as_mut()
            && previous.thread_id == delta.thread_id
            && previous.turn_id == delta.turn_id
            && previous.item_id == delta.item_id
            && previous.delta.len().saturating_add(delta.delta.len())
                <= MAX_COALESCED_AGENT_MESSAGE_DELTA_BYTES
        {
            previous.delta.push_str(&delta.delta);
            self.buffered_agent_message_delta_bytes = self
                .buffered_agent_message_delta_bytes
                .saturating_add(delta.delta.len());
            self.evict_overflowing_events();
            return;
        }

        self.push_buffered_event(ThreadBufferedEvent::Notification(Box::new(
            replay_notification(notification),
        )));
    }

    pub(super) fn push_buffered_event(&mut self, event: ThreadBufferedEvent) {
        if let ThreadBufferedEvent::Notification(notification) = &event
            && let ServerNotification::AgentMessageDelta(delta) = notification.as_ref()
        {
            self.buffered_agent_message_delta_bytes = self
                .buffered_agent_message_delta_bytes
                .saturating_add(delta.delta.len());
        }
        self.buffer.push_back(event);
        self.evict_overflowing_events();
    }

    fn evict_overflowing_events(&mut self) {
        while self.buffer.len() > self.capacity
            || self.buffered_agent_message_delta_bytes > MAX_BUFFERED_AGENT_MESSAGE_DELTA_BYTES
        {
            let Some(removed) = self.buffer.pop_front() else {
                break;
            };
            match removed {
                ThreadBufferedEvent::Notification(notification) => {
                    if let ServerNotification::AgentMessageDelta(delta) = notification.as_ref() {
                        self.buffered_agent_message_delta_bytes = self
                            .buffered_agent_message_delta_bytes
                            .saturating_sub(delta.delta.len());
                    }
                }
                ThreadBufferedEvent::Request(request) => self
                    .pending_interactive_replay
                    .note_evicted_server_request(request.as_ref()),
                ThreadBufferedEvent::HistoryEntryResponse(_)
                | ThreadBufferedEvent::FeedbackSubmission(_) => {}
            }
        }
    }
}

fn replay_notification(notification: Cow<'_, ServerNotification>) -> ServerNotification {
    // The renderer displays only a placeholder for these images. Project before cloning so
    // borrowed live notifications do not duplicate image bytes in the replay cache. Neither the
    // original notification nor the canonical thread history is changed.
    let projected = match notification.as_ref() {
        ServerNotification::ItemStarted(item) => {
            replay_dynamic_tool_item(&item.item).map(|projected| {
                ServerNotification::ItemStarted(ItemStartedNotification {
                    item: projected,
                    thread_id: item.thread_id.clone(),
                    turn_id: item.turn_id.clone(),
                    started_at_ms: item.started_at_ms,
                })
            })
        }
        ServerNotification::ItemCompleted(item) => {
            replay_dynamic_tool_item(&item.item).map(|projected| {
                ServerNotification::ItemCompleted(ItemCompletedNotification {
                    item: projected,
                    thread_id: item.thread_id.clone(),
                    turn_id: item.turn_id.clone(),
                    completed_at_ms: item.completed_at_ms,
                })
            })
        }
        _ => None,
    };
    projected.unwrap_or_else(|| notification.into_owned())
}

fn replay_dynamic_tool_item(item: &ThreadItem) -> Option<ThreadItem> {
    let ThreadItem::DynamicToolCall {
        id,
        namespace,
        tool,
        arguments,
        status,
        content_items: Some(content_items),
        success,
        duration_ms,
    } = item
    else {
        return None;
    };
    if !content_items
        .iter()
        .any(|item| matches!(item, DynamicToolCallOutputContentItem::InputImage { .. }))
    {
        return None;
    }
    Some(ThreadItem::DynamicToolCall {
        id: id.clone(),
        namespace: namespace.clone(),
        tool: tool.clone(),
        arguments: arguments.clone(),
        status: status.clone(),
        content_items: Some(
            content_items
                .iter()
                .map(|item| match item {
                    DynamicToolCallOutputContentItem::InputImage { .. } => {
                        DynamicToolCallOutputContentItem::InputText {
                            text: "<image content>".to_owned(),
                        }
                    }
                    _ => item.clone(),
                })
                .collect(),
        ),
        success: *success,
        duration_ms: *duration_ms,
    })
}

#[cfg(test)]
#[path = "thread_event_buffer_tests.rs"]
mod tests;
