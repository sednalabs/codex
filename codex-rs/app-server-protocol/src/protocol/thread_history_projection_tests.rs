use codex_protocol::ThreadId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::CollabAgentTool as CoreCollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem as CoreCollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus as CoreCollabAgentToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;

use super::*;
use crate::protocol::v2::ThreadItem;
use crate::protocol::v2::TurnError;

#[test]
fn projects_turn_lifecycle_without_prior_builder_state() {
    let started = project(RolloutItem::EventMsg(EventMsg::TurnStarted(
        TurnStartedEvent {
            turn_id: "turn-1".to_string(),
            trace_id: None,
            started_at: Some(10),
            model_context_window: None,
            collaboration_mode_kind: Default::default(),
        },
    )));
    let completed = project(RolloutItem::EventMsg(EventMsg::TurnComplete(
        TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: None,
            error: None,
            compaction_events_in_turn: 0,
            final_model: None,
            model_snapshot: None,
            provider_usage: None,
            started_at: Some(10),
            completed_at: Some(20),
            duration_ms: Some(10_000),
            time_to_first_token_ms: None,
        },
    )));

    assert_eq!(started.changed_turns.len(), 1);
    assert_eq!(started.changed_turns[0].turn_id, "turn-1");
    assert_eq!(started.changed_turns[0].status, TurnStatus::InProgress);
    assert_eq!(started.changed_turns[0].started_at, Some(10));
    assert_eq!(
        completed,
        ThreadHistoryChangeSet {
            changed_turns: vec![ThreadHistoryTurnChange {
                turn_id: "turn-1".to_string(),
                status: TurnStatus::Completed,
                error: None,
                started_at: Some(10),
                completed_at: Some(20),
                duration_ms: Some(10_000),
            }],
            ..Default::default()
        }
    );
}

#[test]
fn projects_failed_turn_completion_as_snapshot() {
    let error = ErrorEvent {
        message: "request failed".to_string(),
        codex_error_info: None,
    };

    let changes = project(RolloutItem::EventMsg(EventMsg::TurnComplete(
        TurnCompleteEvent {
            turn_id: "turn-1".to_string(),
            last_agent_message: None,
            error: Some(error),
            compaction_events_in_turn: 0,
            final_model: None,
            model_snapshot: None,
            provider_usage: None,
            started_at: Some(10),
            completed_at: Some(20),
            duration_ms: Some(10_000),
            time_to_first_token_ms: None,
        },
    )));

    assert_eq!(
        changes,
        ThreadHistoryChangeSet {
            changed_turns: vec![ThreadHistoryTurnChange {
                turn_id: "turn-1".to_string(),
                status: TurnStatus::Failed,
                error: Some(TurnError {
                    message: "request failed".to_string(),
                    codex_error_info: None,
                    additional_details: None,
                }),
                started_at: Some(10),
                completed_at: Some(20),
                duration_ms: Some(10_000),
            }],
            ..Default::default()
        }
    );
}

#[test]
fn projects_completed_canonical_turn_items() {
    let thread_id = ThreadId::default();
    let user_item = TurnItem::UserMessage(UserMessageItem {
        id: "user-1".to_string(),
        client_id: None,
        content: vec![UserInput::Text {
            text: "hello".to_string(),
            text_elements: Vec::new(),
        }],
    });
    let agent_item = TurnItem::AgentMessage(AgentMessageItem {
        id: "agent-1".to_string(),
        content: vec![AgentMessageContent::Text {
            text: "done".to_string(),
        }],
        phase: None,
        memory_citation: None,
    });

    let user_changes = project(item_completed(thread_id, "turn-1", user_item.clone()));
    let agent_changes = project(item_completed(thread_id, "turn-1", agent_item.clone()));

    assert_eq!(
        user_changes.changed_items,
        vec![ThreadHistoryItemChange {
            turn_id: "turn-1".to_string(),
            item: ThreadItem::from(user_item),
        }]
    );
    assert_eq!(
        agent_changes.changed_items,
        vec![ThreadHistoryItemChange {
            turn_id: "turn-1".to_string(),
            item: ThreadItem::from(agent_item),
        }]
    );
}

#[test]
fn projects_completed_canonical_spawns_with_requested_provenance() {
    let thread_id = ThreadId::default();
    let turn_id = "turn-1";
    let cases = [
        (
            "spawn-model-only",
            Some("gpt-requested-model"),
            None,
            "gpt-effective-model",
            Some(codex_protocol::openai_models::ReasoningEffort::Medium),
        ),
        (
            "spawn-effort-only",
            None,
            Some(codex_protocol::openai_models::ReasoningEffort::High),
            "gpt-effective-effort",
            Some(codex_protocol::openai_models::ReasoningEffort::Medium),
        ),
        (
            "spawn-explicit-mismatch",
            Some("gpt-requested-pair"),
            Some(codex_protocol::openai_models::ReasoningEffort::Ultra),
            "gpt-effective-pair",
            Some(codex_protocol::openai_models::ReasoningEffort::Low),
        ),
    ];
    let mut projector = ThreadHistoryProjector::default();
    for (id, requested_model, requested_effort, _, _) in &cases {
        let changes = projector.project_rollout_line(&rollout_line(RolloutItem::EventMsg(
            EventMsg::ItemStarted(ItemStartedEvent {
                thread_id,
                turn_id: turn_id.to_string(),
                item: canonical_spawn_item(
                    id,
                    CoreCollabAgentToolCallStatus::InProgress,
                    *requested_model,
                    requested_effort.clone(),
                ),
                started_at_ms: 1,
            }),
        )));
        assert_eq!(changes.changed_items.len(), 1);
    }

    // Complete in a different order to prove pairing is by the exact call/item ID.
    for index in [2, 0, 1] {
        let (id, requested_model, requested_effort, effective_model, effective_effort) =
            &cases[index];
        let changes = projector.project_rollout_line(&rollout_line(RolloutItem::EventMsg(
            EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id,
                turn_id: turn_id.to_string(),
                item: canonical_spawn_item(
                    id,
                    CoreCollabAgentToolCallStatus::Completed,
                    Some(*effective_model),
                    effective_effort.clone(),
                ),
                completed_at_ms: 2,
            }),
        )));
        let ThreadItem::CollabAgentToolCall {
            id: completed_id,
            model,
            reasoning_effort,
            requested_model: completed_requested_model,
            requested_reasoning_effort: completed_requested_effort,
            ..
        } = &changes.changed_items[0].item
        else {
            panic!("expected completed spawn projection");
        };
        assert_eq!(completed_id, id);
        assert_eq!(model.as_deref(), Some(*effective_model));
        assert_eq!(reasoning_effort, effective_effort);
        assert_eq!(completed_requested_model.as_deref(), *requested_model);
        assert_eq!(completed_requested_effort, requested_effort);
    }
}

#[test]
fn ignores_legacy_abort_without_turn_id_and_context_only_records() {
    let aborted = project(RolloutItem::EventMsg(EventMsg::TurnAborted(
        TurnAbortedEvent {
            turn_id: None,
            reason: TurnAbortReason::Interrupted,
            provider_usage: None,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        },
    )));
    let compacted = project(RolloutItem::Compacted(CompactedItem {
        message: String::new(),
        replacement_history: None,
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
    }));

    assert!(aborted.is_empty());
    assert!(compacted.is_empty());
}

#[test]
fn projects_identified_turn_aborts() {
    let changes = project(RolloutItem::EventMsg(EventMsg::TurnAborted(
        TurnAbortedEvent {
            turn_id: Some("turn-1".to_string()),
            reason: TurnAbortReason::Interrupted,
            provider_usage: None,
            started_at: Some(10),
            completed_at: Some(20),
            duration_ms: Some(10_000),
        },
    )));

    assert_eq!(
        changes,
        ThreadHistoryChangeSet {
            changed_turns: vec![ThreadHistoryTurnChange {
                turn_id: "turn-1".to_string(),
                status: TurnStatus::Interrupted,
                error: None,
                started_at: Some(10),
                completed_at: Some(20),
                duration_ms: Some(10_000),
            }],
            ..Default::default()
        }
    );
}

fn project(item: RolloutItem) -> ThreadHistoryChangeSet {
    project_rollout_line(&rollout_line(item))
}

fn rollout_line(item: RolloutItem) -> RolloutLine {
    RolloutLine {
        timestamp: "2026-07-09T00:00:00.000Z".to_string(),
        ordinal: Some(7),
        item,
    }
}

fn item_completed(thread_id: ThreadId, turn_id: &str, item: TurnItem) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id,
        turn_id: turn_id.to_string(),
        item,
        completed_at_ms: 123,
    }))
}

fn canonical_spawn_item(
    id: &str,
    status: CoreCollabAgentToolCallStatus,
    model: Option<&str>,
    reasoning_effort: Option<codex_protocol::openai_models::ReasoningEffort>,
) -> TurnItem {
    let is_start = status == CoreCollabAgentToolCallStatus::InProgress;
    TurnItem::CollabAgentToolCall(CoreCollabAgentToolCallItem {
        id: id.to_string(),
        tool: CoreCollabAgentTool::SpawnAgent,
        status,
        sender_thread_id: ThreadId::default(),
        receiver_thread_ids: Vec::new(),
        receiver_agents: Vec::new(),
        prompt: Some("inspect the repository".to_string()),
        model: model.map(str::to_string),
        reasoning_effort: reasoning_effort.clone(),
        requested_model: is_start.then(|| model.map(str::to_string)).flatten(),
        requested_reasoning_effort: is_start.then_some(reasoning_effort).flatten(),
        agents_states: Default::default(),
    })
}
