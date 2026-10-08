use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::build_world_state_from_turn_context;
use crate::session::tests::make_session_and_context;
use codex_history::CompactedItem;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use pretty_assertions::assert_eq;
use std::sync::Arc;

fn response_item(item: ResponseItem) -> RolloutItem {
    RolloutItem::ResponseItem(item.into())
}

fn user_msg(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn assistant_msg(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn synthetic_summary_msg() -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: format!(
                "{}\nolder conversation summary",
                codex_prompts::SUMMARY_PREFIX
            ),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn compacted_with_history(history: Vec<ResponseItem>) -> RolloutItem {
    RolloutItem::Compacted(CompactedItem {
        message: "checkpoint".to_string(),
        replacement_history: Some(history.into_iter().map(Into::into).collect()),
        guardian_history: None,
        retained_context: None,
        mcp_resource_origins: None,
        window_number: None,
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
        compaction_response_id: None,
        latest_token_usage_record: None,
        resume_metadata: None,
    })
}

#[test]
fn last_n_fork_turns_counts_nested_checkpoint_history_and_drops_superseded_prefix() {
    let rollout = vec![compacted_with_history(vec![
        user_msg("old turn"),
        assistant_msg("old answer"),
        user_msg("kept turn"),
        assistant_msg("kept answer"),
    ])];

    let truncated = truncate_rollout_to_last_n_fork_turns(&rollout, 1);
    let RolloutItem::Compacted(checkpoint) = &truncated[0] else {
        panic!("checkpoint should be retained as the active history container");
    };
    assert!(checkpoint.message.is_empty());
    let history = checkpoint.replacement_history.as_ref().unwrap();
    assert_eq!(history.len(), 2);
    assert!(matches!(&history[0].item, ResponseItem::Message { role, .. } if role == "user"));
}

#[test]
fn last_n_fork_turns_does_not_count_or_retain_synthetic_checkpoint_summary() {
    let rollout = vec![compacted_with_history(vec![
        synthetic_summary_msg(),
        user_msg("retained turn"),
        assistant_msg("retained answer"),
    ])];

    let truncated = truncate_rollout_to_last_n_fork_turns(&rollout, 2);
    let RolloutItem::Compacted(checkpoint) = &truncated[0] else {
        panic!("checkpoint should be retained as the active history container");
    };
    let history = checkpoint.replacement_history.as_ref().unwrap();
    assert_eq!(history.len(), 2);
    assert!(matches!(
        &history[0].item,
        ResponseItem::Message { role, content, .. }
            if role == "user"
                && matches!(content.first(), Some(ContentItem::OutputText { text }) if text == "retained turn")
    ));
    assert!(!history.iter().any(|item| matches!(
        &item.item,
        ResponseItem::Message { content, .. }
            if content.iter().any(|content| matches!(
                content,
                ContentItem::InputText { text }
                    if text.starts_with(&format!("{}\n", codex_prompts::SUMMARY_PREFIX))
            ))
    )));
}

#[test]
fn last_n_fork_turns_applies_rollback_to_nested_checkpoint_boundaries() {
    let rollout = vec![
        compacted_with_history(vec![
            user_msg("kept turn"),
            assistant_msg("kept answer"),
            user_msg("rolled back turn"),
        ]),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
    ];

    let truncated = truncate_rollout_to_last_n_fork_turns(&rollout, 2);
    let RolloutItem::Compacted(checkpoint) = &truncated[0] else {
        panic!("checkpoint should be retained");
    };
    assert_eq!(checkpoint.replacement_history.as_ref().unwrap().len(), 2);
    assert!(
        truncated
            .iter()
            .all(|item| !matches!(item, RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_))))
    );
}

#[test]
fn last_n_fork_turns_treats_triggering_agent_communication_as_a_turn_boundary() {
    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").unwrap(),
        Vec::new(),
        "trigger task".to_string(),
        /*trigger_turn*/ true,
    );
    let trigger = RolloutItem::InterAgentCommunication(communication.clone());
    let rollout = vec![
        response_item(user_msg("earlier")),
        response_item(assistant_msg("answer")),
        trigger,
    ];

    let truncated = truncate_rollout_to_last_n_fork_turns(&rollout, 1);
    assert!(matches!(
        truncated.as_slice(),
        [RolloutItem::InterAgentCommunication(item)] if item == &communication
    ));
}

fn turn_started(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_attribution: None,
        turn_id: turn_id.to_string(),
        root_turn_id: None,
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: Default::default(),
    }))
}

fn turn_completed(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        root_turn_id: None,
        turn_id: turn_id.to_string(),
        started_at: None,
        last_agent_message: None,
        error: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
}

#[test]
fn truncates_rollout_after_terminal_canonical_turn_id() {
    let rollout = vec![
        turn_started("turn-1"),
        turn_completed("turn-1"),
        turn_started("turn-2"),
        turn_completed("turn-2"),
        turn_started("turn-3"),
        turn_completed("turn-3"),
    ];

    let truncated =
        truncate_rollout_after_turn_id(rollout.clone(), "turn-2").expect("truncate through turn-2");

    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&rollout[..4]).unwrap()
    );
}

#[test]
fn truncates_rollout_before_terminal_canonical_turn_id() {
    let rollout = vec![
        turn_started("turn-1"),
        turn_completed("turn-1"),
        turn_started("turn-2"),
        turn_completed("turn-2"),
    ];

    let truncated =
        truncate_rollout_before_turn_id(rollout.clone(), "turn-2").expect("truncate before turn-2");
    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&rollout[..2]).unwrap()
    );
    assert!(
        truncate_rollout_before_turn_id(rollout, "turn-1")
            .expect("truncate before turn-1")
            .is_empty()
    );
}

#[test]
fn truncates_rollout_before_in_progress_canonical_turn_id() {
    let rollout = vec![
        turn_started("turn-1"),
        turn_completed("turn-1"),
        turn_started("turn-2"),
    ];

    let truncated = truncate_rollout_before_turn_id(rollout.clone(), "turn-2")
        .expect("truncate before in-progress turn-2");

    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&rollout[..2]).unwrap()
    );
}

#[test]
fn truncate_rollout_before_turn_id_rejects_rolled_back_turn() {
    let rollout = vec![
        turn_started("turn-1"),
        turn_completed("turn-1"),
        turn_started("turn-2"),
        turn_completed("turn-2"),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
        turn_started("turn-3"),
        turn_completed("turn-3"),
    ];

    let err = truncate_rollout_before_turn_id(rollout, "turn-2")
        .expect_err("rolled-back turn should not be a fork anchor");

    assert!(matches!(
        err.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message == "beforeTurnId 'turn-2' was not found in the source thread"
    ));
}

#[test]
fn truncate_rollout_before_turn_id_rejects_synthetic_legacy_turn_id() {
    let rollout = vec![RolloutItem::EventMsg(EventMsg::UserMessage(
        UserMessageEvent {
            message: "legacy".to_string(),
            ..Default::default()
        },
    ))];

    let err = truncate_rollout_before_turn_id(rollout, "rollout-0")
        .expect_err("synthetic turn should not be a fork anchor");

    assert!(matches!(
        err.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message
                == "beforeTurnId 'rollout-0' is not a persisted canonical turn in the source thread"
    ));
}

#[test]
fn truncate_rollout_after_turn_id_rejects_rolled_back_turn() {
    let rollout = vec![
        turn_started("turn-1"),
        turn_completed("turn-1"),
        turn_started("turn-2"),
        turn_completed("turn-2"),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
        turn_started("turn-3"),
        turn_completed("turn-3"),
    ];

    let err = truncate_rollout_after_turn_id(rollout, "turn-2")
        .expect_err("rolled-back turn should not be a fork anchor");

    assert!(matches!(
        err.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message == "lastTurnId 'turn-2' was not found in the source thread"
    ));
}

#[test]
fn truncate_rollout_after_turn_id_rejects_synthetic_legacy_turn_id() {
    let rollout = vec![RolloutItem::EventMsg(EventMsg::UserMessage(
        UserMessageEvent {
            message: "legacy".to_string(),
            ..Default::default()
        },
    ))];

    let err = truncate_rollout_after_turn_id(rollout, "rollout-0")
        .expect_err("synthetic turn should not be a fork anchor");

    assert!(matches!(
        err.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message
                == "lastTurnId 'rollout-0' is not a persisted canonical turn in the source thread"
    ));
}

#[test]
fn truncate_rollout_after_turn_id_rejects_in_progress_turn() {
    let rollout = vec![turn_started("turn-1")];

    let err = truncate_rollout_after_turn_id(rollout, "turn-1")
        .expect_err("in-progress turn should not be a fork anchor");

    assert!(matches!(
        err.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message == "lastTurnId 'turn-1' identifies an in-progress turn"
    ));
}

#[test]
fn truncates_rollout_from_start_before_nth_user_only() {
    let items = [
        user_msg("u1"),
        assistant_msg("a1"),
        assistant_msg("a2"),
        user_msg("u2"),
        assistant_msg("a3"),
        ResponseItem::Reasoning {
            id: Some(ResponseItemId::with_suffix("rs", "1")),
            summary: vec![ReasoningItemReasoningSummary::SummaryText {
                text: "s".to_string(),
            }],
            content: None,
            encrypted_content: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCall {
            id: None,
            call_id: "c1".to_string(),
            name: "tool".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            encrypted_function_args: None,
            internal_chat_message_metadata_passthrough: None,
        },
        assistant_msg("a4"),
    ];

    let rollout: Vec<RolloutItem> = items.iter().cloned().map(response_item).collect();

    let truncated = truncate_rollout_before_nth_user_message_from_start(
        rollout.clone(),
        /*n_from_start*/ 1,
    );
    let expected = vec![
        response_item(items[0].clone()),
        response_item(items[1].clone()),
        response_item(items[2].clone()),
    ];
    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );

    let truncated2 = truncate_rollout_before_nth_user_message_from_start(
        rollout.clone(),
        /*n_from_start*/ 2,
    );
    assert_eq!(
        serde_json::to_value(&truncated2).unwrap(),
        serde_json::to_value(&rollout).unwrap()
    );
}

#[test]
fn truncation_max_keeps_full_rollout() {
    let rollout = vec![
        response_item(user_msg("u1")),
        response_item(assistant_msg("a1")),
        response_item(user_msg("u2")),
    ];

    let truncated =
        truncate_rollout_before_nth_user_message_from_start(rollout.clone(), usize::MAX);

    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&rollout).unwrap()
    );
}

#[test]
fn truncates_rollout_from_start_applies_thread_rollback_markers() {
    let rollout_items = vec![
        response_item(user_msg("u1")),
        response_item(assistant_msg("a1")),
        response_item(user_msg("u2")),
        response_item(assistant_msg("a2")),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
        response_item(user_msg("u3")),
        response_item(assistant_msg("a3")),
        response_item(user_msg("u4")),
        response_item(assistant_msg("a4")),
    ];

    // Effective user history after applying rollback(1) is: u1, u3, u4.
    // So n_from_start=2 should cut before u4 (not u3).
    let truncated = truncate_rollout_before_nth_user_message_from_start(
        rollout_items.clone(),
        /*n_from_start*/ 2,
    );
    let expected = rollout_items[..7].to_vec();
    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
}

#[tokio::test]
async fn ignores_session_prefix_messages_when_truncating_rollout_from_start() {
    let (session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let world_state = build_world_state_from_turn_context(&session, &turn_context).await;
    let step_context = StepContext::for_test(turn_context);
    let updates = session
        .build_initial_context_with_world_state(&step_context, &world_state)
        .await
        .0;
    let mut items = crate::context_manager::updates::merge_world_state_updates(updates);
    items.push(user_msg("feature request"));
    items.push(assistant_msg("ack"));
    items.push(user_msg("second question"));
    items.push(assistant_msg("answer"));

    let rollout_items: Vec<RolloutItem> = items.iter().cloned().map(response_item).collect();

    let truncated =
        truncate_rollout_before_nth_user_message_from_start(rollout_items, /*n_from_start*/ 1);
    let expected: Vec<RolloutItem> = vec![
        response_item(items[0].clone()),
        response_item(items[1].clone()),
        response_item(items[2].clone()),
        response_item(items[3].clone()),
    ];

    assert_eq!(
        serde_json::to_value(&truncated).unwrap(),
        serde_json::to_value(&expected).unwrap()
    );
}
