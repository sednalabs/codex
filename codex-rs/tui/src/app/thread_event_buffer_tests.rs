use super::*;
use crate::app::THREAD_EVENT_CHANNEL_CAPACITY;
use crate::app::Turn;
use crate::app::TurnStatus;
use crate::test_support::PathBufExt;
use crate::test_support::test_path_buf;
use codex_app_server_protocol::AgentMessageDeltaNotification;
use codex_app_server_protocol::CommandExecutionRequestApprovalParams;
use codex_app_server_protocol::RequestId as AppServerRequestId;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::TurnStartedNotification;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use serde_json::json;

fn dynamic_tool_item(content_items: Vec<DynamicToolCallOutputContentItem>) -> ThreadItem {
    ThreadItem::DynamicToolCall {
        id: "dynamic-image".to_owned(),
        namespace: Some("example".to_owned()),
        tool: "observe".to_owned(),
        arguments: json!({"target": "page"}),
        status: codex_app_server_protocol::DynamicToolCallStatus::Completed,
        content_items: Some(content_items),
        success: Some(true),
        duration_ms: Some(42),
    }
}

fn dynamic_tool_presentations(item: ThreadItem) -> Vec<Vec<ratatui::text::Line<'static>>> {
    use crate::history_cell::DynamicToolCallCell;
    use crate::history_cell::HistoryCell;
    use crate::terminal_hyperlinks::visible_lines;

    let cell = DynamicToolCallCell::from_item(item).expect("dynamic tool item should render");
    vec![
        cell.display_lines(/*width*/ 80),
        visible_lines(cell.transcript_hyperlink_lines(/*width*/ 80)),
        cell.raw_lines(),
    ]
}

#[test]
fn thread_event_store_projects_replay_images_without_changing_live_or_canonical_items() {
    let thread_id = ThreadId::new();
    let original_item = dynamic_tool_item(vec![
        DynamicToolCallOutputContentItem::InputText {
            text: "before image".to_owned(),
        },
        DynamicToolCallOutputContentItem::InputImage {
            image_url: format!("data:image/png;base64,{}", "a".repeat(64 * 1024)),
        },
        DynamicToolCallOutputContentItem::InputAudio {
            audio_url: "data:audio/wav;base64,unchanged".to_owned(),
        },
        DynamicToolCallOutputContentItem::InputImage {
            image_url: "https://example.test/second.png".to_owned(),
        },
        DynamicToolCallOutputContentItem::InputText {
            text: "after image".to_owned(),
        },
    ]);
    let mut projected_item = original_item.clone();
    if let ThreadItem::DynamicToolCall {
        content_items: Some(content_items),
        ..
    } = &mut projected_item
    {
        for item in content_items {
            if matches!(item, DynamicToolCallOutputContentItem::InputImage { .. }) {
                *item = DynamicToolCallOutputContentItem::InputText {
                    text: "<image content>".to_owned(),
                };
            }
        }
    }
    let notifications = [
        ServerNotification::ItemStarted(ItemStartedNotification {
            item: original_item.clone(),
            thread_id: thread_id.to_string(),
            turn_id: "turn-images".to_owned(),
            started_at_ms: 10,
        }),
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            item: original_item.clone(),
            thread_id: thread_id.to_string(),
            turn_id: "turn-images".to_owned(),
            completed_at_ms: 52,
        }),
    ];
    for notification in notifications {
        let original = serde_json::to_value(&notification).expect("notification should serialize");
        for active in [false, true] {
            let mut store = ThreadEventStore::new(/*capacity*/ 8);
            store.active = active;
            let ServerNotification::TurnStarted(turn) =
                turn_started_notification(thread_id, "turn-images")
            else {
                unreachable!();
            };
            let mut canonical_turn = turn.turn;
            canonical_turn.items = vec![original_item.clone()];
            store.set_turns(vec![canonical_turn.clone()]);
            let approval = exec_approval_request(
                thread_id,
                "turn-images",
                "approval-images",
                /*approval_id*/ None,
            );
            store.push_request(approval.clone());
            if active {
                // Active routing retains a projected copy, then delivers this original.
                store.push_notification_ref(&notification);
            } else {
                store.push_notification(notification.clone());
            }
            assert_eq!(
                serde_json::to_value(&notification).expect("live notification should serialize"),
                original,
            );
            let snapshot = store.snapshot();
            assert_eq!(snapshot.turns, vec![canonical_turn]);
            assert!(store.has_pending_thread_approvals());
            assert_eq!(
                serde_json::to_value(store.pending_replay_requests())
                    .expect("pending requests should serialize"),
                serde_json::to_value(vec![approval]).expect("approval should serialize"),
            );
            let ThreadBufferedEvent::Notification(cached) = &snapshot.events[1] else {
                panic!("expected cached tool notification");
            };
            let cached_item = match cached.as_ref() {
                ServerNotification::ItemStarted(item) => {
                    assert_eq!(item.started_at_ms, 10);
                    &item.item
                }
                ServerNotification::ItemCompleted(item) => {
                    assert_eq!(item.completed_at_ms, 52);
                    &item.item
                }
                _ => panic!("expected cached item lifecycle notification"),
            };
            assert_eq!(cached_item, &projected_item);
            assert_eq!(
                dynamic_tool_presentations(cached_item.clone()),
                dynamic_tool_presentations(original_item.clone()),
            );
            assert!(
                dynamic_tool_presentations(cached_item.clone())[2]
                    .iter()
                    .any(|line| line.to_string() == "<image content>")
            );
        }
    }
}

#[test]
fn thread_event_store_preserves_dynamic_replay_without_images() {
    let notification = ServerNotification::ItemCompleted(ItemCompletedNotification {
        item: dynamic_tool_item(vec![
            DynamicToolCallOutputContentItem::InputText {
                text: "unchanged result".to_owned(),
            },
            DynamicToolCallOutputContentItem::InputAudio {
                audio_url: "data:audio/wav;base64,unchanged".to_owned(),
            },
        ]),
        thread_id: ThreadId::new().to_string(),
        turn_id: "turn-text".to_owned(),
        completed_at_ms: 52,
    });
    let mut store = ThreadEventStore::new(/*capacity*/ 2);
    store.push_notification_ref(&notification);
    store.push_notification(notification.clone());
    for event in store.snapshot().events {
        let ThreadBufferedEvent::Notification(cached) = event else {
            panic!("expected cached notification");
        };
        assert_eq!(
            serde_json::to_value(cached).expect("cached notification should serialize"),
            serde_json::to_value(&notification).expect("original notification should serialize"),
        );
    }
}

fn turn_started_notification(thread_id: ThreadId, turn_id: &str) -> ServerNotification {
    ServerNotification::TurnStarted(TurnStartedNotification {
        thread_id: thread_id.to_string(),
        turn: Turn {
            id: turn_id.to_string(),
            root_turn_id: None,
            items_view: TurnItemsView::Full,
            items: Vec::new(),
            status: TurnStatus::InProgress,
            error: None,
            started_at: Some(0),
            completed_at: None,
            duration_ms: None,
        },
    })
}

fn exec_approval_request(
    thread_id: ThreadId,
    turn_id: &str,
    item_id: &str,
    approval_id: Option<&str>,
) -> ServerRequest {
    ServerRequest::CommandExecutionRequestApproval {
        request_id: AppServerRequestId::Integer(1),
        params: CommandExecutionRequestApprovalParams {
            kind: Default::default(),
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            item_id: item_id.to_string(),
            started_at_ms: 0,
            approval_id: approval_id.map(str::to_string),
            environment_id: None,
            reason: Some("needs approval".to_string()),
            network_approval_context: None,
            command: Some("echo hello".to_string()),
            cwd: Some(test_path_buf("/tmp/project").abs().into()),
            command_actions: None,
            additional_permissions: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            available_decisions: None,
        },
    }
}

#[test]
fn thread_event_store_coalesces_only_adjacent_matching_agent_message_deltas() {
    let thread_id = ThreadId::new();
    let mut store = ThreadEventStore::new(/*capacity*/ 6);
    let delta = |turn_id: &str, item_id: &str, text: &str| {
        ServerNotification::AgentMessageDelta(AgentMessageDeltaNotification {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            item_id: item_id.to_string(),
            delta: text.to_string(),
        })
    };
    let approval =
        exec_approval_request(thread_id, "turn-2", "approval-1", /*approval_id*/ None);
    let other_thread_delta = ServerNotification::AgentMessageDelta(AgentMessageDeltaNotification {
        thread_id: ThreadId::new().to_string(),
        turn_id: "turn-2".to_string(),
        item_id: "item-2".to_string(),
        delta: "other thread".to_string(),
    });
    let snapshot_events = |store: &ThreadEventStore| {
        store
            .snapshot()
            .events
            .into_iter()
            .map(|event| match event {
                ThreadBufferedEvent::Notification(notification) => {
                    serde_json::to_value(notification).expect("notification should serialize")
                }
                ThreadBufferedEvent::Request(request) => {
                    serde_json::to_value(request).expect("request should serialize")
                }
                other => panic!("unexpected buffered event: {other:?}"),
            })
            .collect::<Vec<_>>()
    };

    store.push_notification(delta("turn-1", "item-1", "hello"));
    store.push_notification_ref(&delta("turn-1", "item-1", " world"));
    store.push_notification(delta("turn-1", "item-2", "another"));
    store.push_notification(delta("turn-2", "item-2", "next"));
    store.push_notification(other_thread_delta.clone());
    store.push_request(approval.clone());
    store.push_notification(delta("turn-2", "item-2", "after approval"));

    assert_eq!(
        snapshot_events(&store),
        vec![
            serde_json::to_value(delta("turn-1", "item-1", "hello world"))
                .expect("delta should serialize"),
            serde_json::to_value(delta("turn-1", "item-2", "another"))
                .expect("delta should serialize"),
            serde_json::to_value(delta("turn-2", "item-2", "next"))
                .expect("delta should serialize"),
            serde_json::to_value(other_thread_delta).expect("delta should serialize"),
            serde_json::to_value(approval).expect("request should serialize"),
            serde_json::to_value(delta("turn-2", "item-2", "after approval"))
                .expect("delta should serialize"),
        ]
    );
    assert!(store.has_pending_thread_approvals());

    let remaining = MAX_COALESCED_AGENT_MESSAGE_DELTA_BYTES - "after approval".len();
    let exact_limit = "é".repeat(remaining / "é".len());
    let full_chunk = "é".repeat(MAX_COALESCED_AGENT_MESSAGE_DELTA_BYTES / "é".len());
    store.push_notification(delta("turn-2", "item-2", &exact_limit));
    store.push_notification(delta("turn-2", "item-2", "🙂"));
    store.push_notification(delta("turn-2", "item-2", &full_chunk));
    store.push_notification(delta("turn-2", "item-2", "🙂"));
    store.push_notification(delta("turn-2", "item-2", &full_chunk));
    store.push_notification(delta("turn-2", "item-2", "🙂"));

    assert_eq!(
        snapshot_events(&store),
        vec![
            serde_json::to_value(delta(
                "turn-2",
                "item-2",
                &format!("after approval{exact_limit}"),
            ))
            .expect("delta should serialize"),
            serde_json::to_value(delta("turn-2", "item-2", "🙂")).expect("delta should serialize"),
            serde_json::to_value(delta("turn-2", "item-2", &full_chunk))
                .expect("delta should serialize"),
            serde_json::to_value(delta("turn-2", "item-2", "🙂")).expect("delta should serialize"),
            serde_json::to_value(delta("turn-2", "item-2", &full_chunk))
                .expect("delta should serialize"),
            serde_json::to_value(delta("turn-2", "item-2", "🙂")).expect("delta should serialize"),
        ]
    );
    assert!(!store.has_pending_thread_approvals());

    let mut store = ThreadEventStore::new(/*capacity*/ THREAD_EVENT_CHANNEL_CAPACITY);
    store.push_request(exec_approval_request(
        thread_id,
        "turn-budget",
        "approval-budget",
        /*approval_id*/ None,
    ));
    for chunk in 0..65 {
        let fragment = format!("{chunk:08}");
        let notification = delta("turn-budget", "item-budget", &fragment);
        for _ in 0..MAX_COALESCED_AGENT_MESSAGE_DELTA_BYTES / fragment.len() {
            store.push_notification_ref(&notification);
        }
    }

    assert_eq!(
        snapshot_events(&store),
        (1..65)
            .map(|chunk| {
                serde_json::to_value(delta(
                    "turn-budget",
                    "item-budget",
                    &format!("{chunk:08}").repeat(MAX_COALESCED_AGENT_MESSAGE_DELTA_BYTES / 8),
                ))
                .expect("delta should serialize")
            })
            .collect::<Vec<_>>()
    );
    assert!(!store.has_pending_thread_approvals());

    let mut store = ThreadEventStore::new(/*capacity*/ 4);
    store.push_notification(turn_started_notification(thread_id, "turn-oversized"));
    store.push_request(exec_approval_request(
        thread_id,
        "turn-oversized",
        "approval-oversized",
        /*approval_id*/ None,
    ));
    store.push_notification(delta("turn-oversized", "item-oversized", "retained"));
    let expected_events = snapshot_events(&store);
    let oversized = "🙂".repeat(MAX_BUFFERED_AGENT_MESSAGE_DELTA_BYTES / "🙂".len() + 1);

    store.push_notification(delta("turn-oversized", "item-oversized", &oversized));
    store.push_notification_ref(&delta("turn-oversized", "item-oversized", &oversized));

    assert_eq!(snapshot_events(&store), expected_events);
    assert!(store.has_pending_thread_approvals());
    assert_eq!(store.buffered_agent_message_delta_bytes, "retained".len());

    store.push_notification(delta("turn-oversized", "item-oversized", " text"));
    assert_eq!(
        snapshot_events(&store),
        vec![
            serde_json::to_value(turn_started_notification(thread_id, "turn-oversized"))
                .expect("turn notification should serialize"),
            serde_json::to_value(exec_approval_request(
                thread_id,
                "turn-oversized",
                "approval-oversized",
                /*approval_id*/ None,
            ))
            .expect("approval should serialize"),
            serde_json::to_value(delta("turn-oversized", "item-oversized", "retained text"))
                .expect("delta should serialize"),
        ]
    );
    assert!(store.has_pending_thread_approvals());
}
