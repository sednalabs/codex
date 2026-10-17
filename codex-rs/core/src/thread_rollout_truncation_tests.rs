use super::*;
use crate::session::step_context::StepContext;
use crate::session::tests::build_world_state_from_turn_context;
use crate::session::tests::make_session_and_context;
use codex_history::CodexHarnessMetadata;
use codex_history::CompactedItem;
use codex_history::InterAgentCommunicationAdmission;
use codex_history::InterAgentCommunicationIntent;
use codex_history::ResponseItemEnvelope;
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

fn explicit_inter_agent_admission(update_id: &str) -> RolloutItem {
    let sender_path = AgentPath::root().join("worker").unwrap();
    let recipient_path = AgentPath::root();
    let mut original_communication = InterAgentCommunication::new(
        sender_path.clone(),
        recipient_path.clone(),
        Vec::new(),
        "A bounded progress update.".to_string(),
        /*trigger_turn*/ false,
    );
    let update_id = ResponseItemId::from_server(update_id.to_string());
    original_communication.id = Some(update_id.clone());
    RolloutItem::InterAgentCommunicationMetadata {
        trigger_turn: false,
        admission: Some(InterAgentCommunicationAdmission {
            update_id,
            idempotency_key: "progress-update-1".to_string(),
            intent: InterAgentCommunicationIntent::Progress,
            original_communication,
            root_thread_id: codex_protocol::ThreadId::from_u128(1),
            sender_thread_id: codex_protocol::ThreadId::from_u128(2),
            recipient_thread_id: codex_protocol::ThreadId::from_u128(1),
            sender_path,
            recipient_path,
            supersedes: Vec::new(),
            represented_response_id: None,
        }),
    }
}

fn explicit_inter_agent_response(update_id: &str, response_id: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope {
        item: ResponseItem::AgentMessage {
            id: Some(ResponseItemId::from_server(response_id.to_string())),
            author: "/root/worker".to_string(),
            recipient: "/root".to_string(),
            content: Vec::new(),
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(CodexHarnessMetadata {
            inter_agent_update_id: Some(ResponseItemId::from_server(update_id.to_string())),
            ..Default::default()
        }),
    }
}

fn compacted_with_envelopes(history: Vec<ResponseItemEnvelope>) -> RolloutItem {
    RolloutItem::Compacted(CompactedItem {
        message: "checkpoint".to_string(),
        replacement_history: Some(history),
        guardian_history: None,
        retained_context: None,
        mcp_resource_origins: None,
        window_number: Some(1),
        first_window_id: None,
        previous_window_id: None,
        window_id: None,
        compaction_response_id: None,
        latest_token_usage_record: None,
        resume_metadata: None,
    })
}

#[test]
fn quiet_explicit_update_remains_in_the_prior_fork_turn_suffix() {
    let rollout = vec![
        response_item(user_msg("earlier user turn")),
        explicit_inter_agent_admission("update_1"),
        response_item(assistant_msg("interleaved non-boundary record")),
        RolloutItem::ResponseItem(explicit_inter_agent_response("update_1", "response_1")),
    ];

    let retained_two_turns = truncate_rollout_to_last_n_fork_turns(&rollout, /*n_from_end*/ 2);
    assert_eq!(
        serde_json::to_value(&retained_two_turns).unwrap(),
        serde_json::to_value(&rollout).unwrap()
    );

    let retained_last_turn = truncate_rollout_to_last_n_fork_turns(&rollout, /*n_from_end*/ 1);
    assert_eq!(
        serde_json::to_value(&retained_last_turn).unwrap(),
        serde_json::to_value(&rollout).unwrap(),
        "quiet progress stays in the suffix of the previous real user turn"
    );
    assert_eq!(
        retained_last_turn
            .iter()
            .filter(|item| matches!(
                item,
                RolloutItem::InterAgentCommunicationMetadata {
                    admission: Some(admission),
                    ..
                } if admission.update_id.as_str() == "update_1"
            ))
            .count(),
        1,
        "the explicit admission must be retained exactly once"
    );
    assert_eq!(
        retained_last_turn
            .iter()
            .filter(|item| matches!(
                item,
                RolloutItem::ResponseItem(response)
                    if matches!(&response.item, ResponseItem::AgentMessage {
                        id: Some(id),
                        ..
                    } if id.as_str() == "response_1")
            ))
            .count(),
        1,
        "the quiet AgentMessage must be retained exactly once"
    );
}

#[test]
fn rollback_uses_compacted_response_boundary_not_following_admission() {
    let rollout = vec![
        compacted_with_envelopes(vec![
            user_msg("earlier compacted turn").into(),
            explicit_inter_agent_response("update_1", "response_1"),
        ]),
        explicit_inter_agent_admission("update_1"),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
    ];

    let truncated = truncate_rollout_to_last_n_fork_turns(&rollout, /*n_from_end*/ 1);
    let [RolloutItem::Compacted(compacted)] = truncated.as_slice() else {
        panic!("the retained compacted history should own the fork suffix");
    };
    let history = compacted.replacement_history.as_ref().unwrap();
    assert_eq!(history.len(), 1);
    assert!(matches!(&history[0].item, ResponseItem::Message { role, .. } if role == "user"));
}

#[test]
fn last_n_fork_turns_counts_nested_checkpoint_history_and_drops_superseded_prefix() {
    let rollout = vec![compacted_with_history(vec![
        user_msg("old turn"),
        assistant_msg("old answer"),
        user_msg("kept turn"),
        assistant_msg("kept answer"),
    ])];

    let truncated = truncate_rollout_to_last_n_fork_turns(&rollout, /*n_from_end*/ 1);
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

    let truncated = truncate_rollout_to_last_n_fork_turns(&rollout, /*n_from_end*/ 2);
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

    let truncated = truncate_rollout_to_last_n_fork_turns(&rollout, /*n_from_end*/ 2);
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

    let truncated = truncate_rollout_to_last_n_fork_turns(&rollout, /*n_from_end*/ 1);
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
