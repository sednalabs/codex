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


#[test]
fn synthetic_replay_image_retention_scaling() {
    use codex_app_server_protocol::DynamicToolCallOutputContentItem;
    use codex_app_server_protocol::ItemCompletedNotification;
    use codex_app_server_protocol::ItemStartedNotification;
    use codex_app_server_protocol::ThreadItem;
    use std::io::Write as _;

    // Counts are image strings, their len/capacity, then placeholder len/capacity.
    // They bound owned string storage, not allocator overhead or resident pages.
    fn note_item(item: &ThreadItem, totals: &mut [usize; 5]) {
        if let ThreadItem::DynamicToolCall {
            content_items: Some(content_items),
            ..
        } = item
        {
            for content in content_items {
                match content {
                    DynamicToolCallOutputContentItem::InputImage { image_url } => {
                        totals[0] += 1;
                        totals[1] += image_url.len();
                        totals[2] += image_url.capacity();
                    }
                    DynamicToolCallOutputContentItem::InputText { text }
                        if text == "<image content>" =>
                    {
                        totals[3] += text.len();
                        totals[4] += text.capacity();
                    }
                    _ => {}
                }
            }
        }
    }
    fn note_event(event: &ThreadBufferedEvent, totals: &mut [usize; 5]) {
        if let ThreadBufferedEvent::Notification(notification) = event {
            match notification.as_ref() {
                ServerNotification::ItemStarted(item) => note_item(&item.item, totals),
                ServerNotification::ItemCompleted(item) => note_item(&item.item, totals),
                _ => {}
            }
        }
    }

    for (threads, images_per_thread, payload_bytes, notifications_per_image) in [
        (1, 1, 64 * 1024, 1),
        (1, 4, 64 * 1024, 1),
        (22, 4, 64 * 1024, 1),
        (22, 4, 64 * 1024, 2),
        (22, 4, 256 * 1024, 2),
    ] {
        // Synthetic encoded text only: no production image or image decoder.
        let image_url = format!("data:image/png;base64,{}", "a".repeat(payload_bytes));
        let mut route_results = Vec::new();
        for active in [false, true] {
            let mut stores = Vec::new();
            for _ in 0..threads {
                let thread_id = ThreadId::new();
                let mut store = ThreadEventStore::new(THREAD_EVENT_CHANNEL_CAPACITY);
                store.active = active;
                let items = (0..images_per_thread)
                    .map(|index| ThreadItem::DynamicToolCall {
                        id: format!("synthetic-image-{index}"),
                        namespace: Some("synthetic".to_owned()),
                        tool: "observe".to_owned(),
                        arguments: serde_json::json!({}),
                        status: codex_app_server_protocol::DynamicToolCallStatus::Completed,
                        content_items: Some(vec![DynamicToolCallOutputContentItem::InputImage {
                            image_url: image_url.clone(),
                        }]),
                        success: Some(true),
                        duration_ms: Some(1),
                    })
                    .collect::<Vec<_>>();
                let ServerNotification::TurnStarted(turn) =
                    turn_started_notification(thread_id, "synthetic-images")
                else {
                    unreachable!();
                };
                let mut canonical_turn = turn.turn;
                canonical_turn.items = items.clone();
                store.set_turns(vec![canonical_turn]);
                for item in items {
                    let mut notifications = Vec::new();
                    if notifications_per_image == 2 {
                        notifications.push(ServerNotification::ItemStarted(
                            ItemStartedNotification {
                                item: item.clone(),
                                thread_id: thread_id.to_string(),
                                turn_id: "synthetic-images".to_owned(),
                                started_at_ms: 1,
                            },
                        ));
                    }
                    notifications.push(ServerNotification::ItemCompleted(
                        ItemCompletedNotification {
                            item,
                            thread_id: thread_id.to_string(),
                            turn_id: "synthetic-images".to_owned(),
                            completed_at_ms: 2,
                        },
                    ));
                    for notification in notifications {
                        if active {
                            store.push_notification_ref(&notification);
                        } else {
                            store.push_notification(notification.clone());
                        }
                        let mut original = [0; 5];
                        match &notification {
                            ServerNotification::ItemStarted(item) => {
                                note_item(&item.item, &mut original);
                            }
                            ServerNotification::ItemCompleted(item) => {
                                note_item(&item.item, &mut original);
                            }
                            _ => unreachable!(),
                        }
                        assert_eq!(original[0], 1);
                        assert_eq!(original[1], image_url.len());
                    }
                }
                assert_eq!(
                    store.buffer.len(),
                    images_per_thread * notifications_per_image,
                );
                stores.push(store);
            }
            let mut cache = [0; 5];
            let mut canonical = [0; 5];
            for store in &stores {
                for event in &store.buffer {
                    note_event(event, &mut cache);
                }
                for turn in &store.turns {
                    for item in &turn.items {
                        note_item(item, &mut canonical);
                    }
                }
            }
            assert_eq!(canonical[0], threads * images_per_thread);
            assert_eq!(canonical[1], threads * images_per_thread * image_url.len());
            assert_eq!(
                cache[0] + cache[3] / "<image content>".len(),
                threads * images_per_thread * notifications_per_image,
            );

            // Snapshot clones are measured separately, never added to cache ownership.
            let snapshots = stores.iter().map(ThreadEventStore::snapshot).collect::<Vec<_>>();
            let mut snapshot = [0; 5];
            for saved in &snapshots {
                for event in &saved.events {
                    note_event(event, &mut snapshot);
                }
                for turn in &saved.turns {
                    for item in &turn.items {
                        note_item(item, &mut snapshot);
                    }
                }
            }
            assert_eq!(snapshot[1], cache[1] + canonical[1]);
            route_results.push((cache, canonical, snapshot));
        }
        assert_eq!(route_results[0], route_results[1]);
        let (cache, canonical, snapshot) = route_results[0];
        writeln!(
            std::io::stderr(),
            "SYNTHETIC_REPLAY_IMAGE_MEASUREMENT {}",
            serde_json::json!({
                "threads": threads,
                "images_per_thread": images_per_thread,
                "image_url_bytes": image_url.len(),
                "notifications_per_image": notifications_per_image,
                "routes_checked": 2,
                "cache": cache,
                "canonical": canonical,
                "snapshot": snapshot,
            }),
        )
        .expect("write synthetic replay measurement");
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
