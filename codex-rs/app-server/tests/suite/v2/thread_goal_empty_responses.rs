//! Exercises the goal extension's no-activity breaker through the public API.

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_sequence;
use app_test_support::write_models_cache;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::CollabAgentTool;
use codex_app_server_protocol::CollabAgentToolCallStatus;
use codex_app_server_protocol::ConfigBatchWriteParams;
use codex_app_server_protocol::ConfigEdit;
use codex_app_server_protocol::ConfigWriteResponse;
use codex_app_server_protocol::HooksListParams;
use codex_app_server_protocol::HooksListResponse;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::MergeStrategy;
use codex_app_server_protocol::ThreadGoalSetResponse;
use codex_app_server_protocol::ThreadGoalStatus;
use codex_app_server_protocol::ThreadGoalUpdatedNotification;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStartedNotification;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_features::Feature;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::StreamingSseRequestRoute;
use core_test_support::streaming_sse::start_streaming_sse_server_with_routes;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::time::Duration;
use tokio::time::timeout;

#[test_case::test_case(None; "empty")]
#[test_case::test_case(Some("final_answer"); "draft")]
#[test_case::test_case(Some("commentary"); "commentary")]
#[test_case::test_case(Some("tool"); "tool")]
#[test_case::test_case(Some("interrupt"); "interrupt")]
#[tokio::test]
async fn empty_goal_continuations_block_after_three_without_activity(
    recovery: Option<&str>,
) -> Result<()> {
    let turns = match recovery {
        Some("interrupt") => 4,
        Some(_) => 6,
        None => 3,
    };
    let mut scripts = Vec::new();
    for turn in 1..=turns {
        let mut id = format!("response-{turn}");
        let mut events = vec![responses::ev_response_created(&id)];
        if recovery == Some("interrupt") {
            events.push(responses::ev_assistant_message(
                "progress",
                "Ready for the next step",
            ));
            events.push(responses::ev_completed_with_tokens(
                &id, /*total_tokens*/ 330_000,
            ));
            scripts.push(responses::sse(events));
            break;
        }
        if turn == 3
            && let Some(recovery) = recovery
        {
            if recovery == "tool" {
                events.push(responses::ev_function_call("get-goal", "get_goal", "{}"));
                events.push(responses::ev_completed(&id));
                scripts.push(responses::sse(events));
                id.push_str("-after-tool");
                events = vec![responses::ev_response_created(&id)];
            } else {
                let mut added = responses::ev_assistant_message("progress", "");
                added["type"] = json!("response.output_item.added");
                events.push(added);
                events.push(responses::ev_output_text_delta("Useful progress"));
                let mut event = responses::ev_assistant_message("progress", "Useful progress");
                event["item"]["phase"] = json!(recovery);
                events.push(event);
            }
        }
        let mut empty_final = responses::ev_assistant_message(&format!("empty-final-{turn}"), "");
        empty_final["item"]["phase"] = json!("final_answer");
        events.push(empty_final);
        events.push(responses::ev_completed(&id));
        scripts.push(responses::sse(events));
    }
    let server = create_mock_responses_server_sequence(scripts).await;
    let codex_home = TempDir::new()?;
    let mut config = MockResponsesConfig::new(&server.uri())
        .with_model("gpt-5.4")
        .enable_feature(Feature::Goals);
    if recovery == Some("interrupt") {
        config = config
            .enable_feature(Feature::CodexHooks)
            .with_root_config(
                "model_auto_compact_token_limit = 200000\nmodel_post_turn_compact_threshold_percent = 0",
            )
            .with_extra_config(
                r#"
[[hooks.PreCompact]]
matcher = "auto"
[[hooks.PreCompact.hooks]]
type = "command"
command = """printf '%s' '{"continue":false}'"""
command_windows = """Write-Output '{"continue":false}'"""
"#,
            );
    }
    config.write(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    if recovery == Some("interrupt") {
        let list = mcp
            .send_hooks_list_request(HooksListParams {
                cwds: vec![codex_home.path().to_path_buf()],
            })
            .await?;
        let HooksListResponse { data } = mcp.read_response(list).await?;
        let hook = &data[0].hooks[0];
        let trust = mcp
            .send_config_batch_write_request(ConfigBatchWriteParams {
                edits: vec![ConfigEdit {
                    key_path: "hooks.state".to_string(),
                    value: json!({hook.key.clone(): {"trusted_hash": hook.current_hash}}),
                    merge_strategy: MergeStrategy::Upsert,
                }],
                file_path: None,
                expected_version: None,
                reload_user_config: true,
            })
            .await?;
        let _: ConfigWriteResponse = mcp.read_response(trust).await?;
    }
    let request = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let ThreadStartResponse { thread, .. } = mcp.read_response(request).await?;
    let request = mcp
        .send_raw_request(
            "thread/goal/set",
            Some(json!({"threadId": thread.id, "objective": "Finish the work"})),
        )
        .await?;
    let _: ThreadGoalSetResponse = mcp.read_response(request).await?;

    for turn in 1..=turns {
        if turn == 3 && matches!(recovery, Some("final_answer" | "commentary")) {
            let delta: serde_json::Value = timeout(
                Duration::from_secs(30),
                mcp.read_notification("item/agentMessage/delta"),
            )
            .await??;
            assert_eq!(json!("Useful progress"), delta["delta"]);
        }
        let completed: TurnCompletedNotification = timeout(
            Duration::from_secs(30),
            mcp.read_notification("turn/completed"),
        )
        .await??;
        let expected = if recovery == Some("interrupt") && turn > 1 {
            TurnStatus::Interrupted
        } else {
            TurnStatus::Completed
        };
        assert_eq!(expected, completed.turn.status);
        assert_eq!(None, completed.turn.error);
    }
    timeout(Duration::from_secs(30), async {
        loop {
            let notification: ThreadGoalUpdatedNotification =
                mcp.read_notification("thread/goal/updated").await?;
            if notification.goal.status == ThreadGoalStatus::Blocked {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    server.verify().await;
    Ok(())
}

/// A real ThreadSpawn child must finish its native goal continuation before the parent's wait
/// observes terminal completion. A held provider response keeps the continuation turn in flight
/// while the parent notification stream is observed for premature handback.
#[tokio::test]
async fn thread_spawn_goal_continuation_precedes_parent_handback() -> Result<()> {
    const PARENT_PROMPT: &str = "spawn a worker whose goal needs one continuation";
    const CHILD_PROMPT: &str = "create a goal that the native continuation can complete";
    const CONTINUATION_PROMPT: &str = "Continue working toward the active thread goal.";
    const SPAWN_CALL_ID: &str = "spawn-goal-worker";
    const WAIT_CALL_ID: &str = "wait-for-goal-worker";
    const CREATE_GOAL_CALL_ID: &str = "create-child-goal";
    const UPDATE_GOAL_CALL_ID: &str = "complete-child-goal";
    const READ_TIMEOUT: Duration = Duration::from_secs(30);

    fn request_contains(request: &serde_json::Value, text: &str) -> bool {
        request.to_string().contains(text)
    }

    fn route(
        matcher: impl Fn(&serde_json::Value) -> bool + Send + Sync + 'static,
        events: Vec<serde_json::Value>,
    ) -> StreamingSseRequestRoute {
        StreamingSseRequestRoute::new(
            matcher,
            vec![StreamingSseChunk {
                gate: None,
                body: responses::sse(events),
            }],
        )
    }

    let (continuation_release_tx, continuation_release_rx) = oneshot::channel();
    let mut continuation_release_tx = Some(continuation_release_tx);
    let (parent_followup_matched_tx, mut parent_followup_matched_rx) = oneshot::channel();
    let (continuation_matched_tx, continuation_matched_rx) = oneshot::channel();
    let continuation_route = StreamingSseRequestRoute::new(
        |request| {
            request_contains(request, CONTINUATION_PROMPT)
                && !request_contains(request, UPDATE_GOAL_CALL_ID)
                && !request_contains(request, SPAWN_CALL_ID)
                && !request_contains(request, WAIT_CALL_ID)
                && !request_contains(request, PARENT_PROMPT)
        },
        vec![
            StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![responses::ev_response_created(
                    "child-goal-continuation",
                )]),
            },
            StreamingSseChunk {
                gate: Some(continuation_release_rx),
                body: responses::sse(vec![
                    responses::ev_function_call(
                        UPDATE_GOAL_CALL_ID,
                        "update_goal",
                        r#"{"status":"complete"}"#,
                    ),
                    responses::ev_completed("child-goal-continuation"),
                ]),
            },
        ],
    )
    .with_request_matched(continuation_matched_tx);
    let (provider, _) = start_streaming_sse_server_with_routes(vec![
        route(
            |request| {
                request_contains(request, PARENT_PROMPT)
                    && !request_contains(request, SPAWN_CALL_ID)
                    && !request_contains(request, WAIT_CALL_ID)
                    && !request_contains(request, CHILD_PROMPT)
                    && !request_contains(request, CREATE_GOAL_CALL_ID)
                    && !request_contains(request, CONTINUATION_PROMPT)
                    && !request_contains(request, UPDATE_GOAL_CALL_ID)
            },
            vec![
                responses::ev_response_created("parent-spawn"),
                responses::ev_function_call_with_namespace(
                    SPAWN_CALL_ID,
                    "collaboration",
                    "spawn_agent",
                    &json!({
                        "message": CHILD_PROMPT,
                        "task_name": "worker",
                        "fork_turns": "none"
                    })
                    .to_string(),
                ),
                responses::ev_completed("parent-spawn"),
            ],
        ),
        StreamingSseRequestRoute::new(
            |request| {
                request_contains(request, SPAWN_CALL_ID) && !request_contains(request, WAIT_CALL_ID)
            },
            vec![StreamingSseChunk {
                gate: None,
                body: responses::sse(vec![
                    responses::ev_response_created("parent-wait"),
                    responses::ev_function_call_with_namespace(
                        WAIT_CALL_ID,
                        "collaboration",
                        "wait_agent",
                        r#"{"timeout_ms":60000}"#,
                    ),
                    responses::ev_completed("parent-wait"),
                ]),
            }],
        )
        .with_request_matched(parent_followup_matched_tx),
        route(
            |request| request_contains(request, WAIT_CALL_ID),
            vec![
                responses::ev_response_created("parent-final"),
                responses::ev_assistant_message("parent-final-message", "Worker goal completed."),
                responses::ev_completed("parent-final"),
            ],
        ),
        route(
            |request| {
                request_contains(request, CHILD_PROMPT)
                    && !request_contains(request, SPAWN_CALL_ID)
                    && !request_contains(request, WAIT_CALL_ID)
                    && !request_contains(request, PARENT_PROMPT)
                    && !request_contains(request, CREATE_GOAL_CALL_ID)
                    && !request_contains(request, CONTINUATION_PROMPT)
                    && !request_contains(request, UPDATE_GOAL_CALL_ID)
            },
            vec![
                responses::ev_response_created("child-create-goal"),
                responses::ev_function_call(
                    CREATE_GOAL_CALL_ID,
                    "create_goal",
                    r#"{"objective":"Finish the child task"}"#,
                ),
                responses::ev_completed("child-create-goal"),
            ],
        ),
        route(
            |request| {
                request_contains(request, CREATE_GOAL_CALL_ID)
                    && !request_contains(request, UPDATE_GOAL_CALL_ID)
                    && !request_contains(request, CONTINUATION_PROMPT)
                    && !request_contains(request, SPAWN_CALL_ID)
                    && !request_contains(request, WAIT_CALL_ID)
                    && !request_contains(request, PARENT_PROMPT)
            },
            vec![
                responses::ev_response_created("child-goal-active"),
                responses::ev_assistant_message("child-goal-active-message", "I will continue."),
                responses::ev_completed("child-goal-active"),
            ],
        ),
        continuation_route,
        route(
            |request| {
                request_contains(request, UPDATE_GOAL_CALL_ID)
                    && !request_contains(request, SPAWN_CALL_ID)
                    && !request_contains(request, WAIT_CALL_ID)
                    && !request_contains(request, PARENT_PROMPT)
            },
            vec![
                responses::ev_response_created("child-goal-finished"),
                responses::ev_assistant_message("child-goal-finished-message", "Goal complete."),
                responses::ev_completed("child-goal-finished"),
            ],
        ),
    ])
    .await;
    let analytics_server = responses::start_mock_server().await;

    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&analytics_server.uri())
        .with_provider_base_url(&format!("{}/v1", provider.uri()))
        .with_model("gpt-5.4")
        .enable_feature(Feature::MultiAgentV2)
        .enable_feature(Feature::Goals)
        .with_root_config(&format!(
            "chatgpt_base_url = \"{}\"",
            analytics_server.uri()
        ))
        .write(codex_home.path())?;
    write_models_cache(codex_home.path()).await?;
    super::analytics::mount_analytics_capture(&analytics_server, codex_home.path()).await?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = mcp
        .start_thread(ThreadStartParams {
            model: Some("gpt-5.4".to_string()),
            ..Default::default()
        })
        .await?
        .thread;
    let parent_turn: TurnStartResponse = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                input: vec![V2UserInput::Text {
                    text: PARENT_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await?;

    let mut notifications = Vec::new();
    let mut spawn_child_thread_id = None;
    let mut parent_followup_matched = false;
    let mut parent_wait_started = false;
    let mut child_continuation_started = false;
    let mut started_turns = Vec::<TurnStartedNotification>::new();
    let pre_observation = timeout(READ_TIMEOUT, async {
        loop {
            let message = tokio::select! {
                message = mcp.read_next_message() => message?,
                matched = &mut parent_followup_matched_rx, if !parent_followup_matched => {
                    let request = matched
                        .map_err(|_| anyhow::anyhow!("parent follow-up route match sender closed"))?;
                    let request: serde_json::Value = serde_json::from_slice(&request)
                        .map_err(|_| anyhow::anyhow!("parent follow-up request was not valid JSON"))?;
                    assert!(
                        request["client_metadata"]["thread_id"] == json!(thread.id)
                            && request["client_metadata"]["turn_id"] == json!(parent_turn.turn.id),
                        "parent follow-up request metadata did not match the parent turn"
                    );
                    parent_followup_matched = true;
                    continue;
                }
            };
            let JSONRPCMessage::Notification(notification) = message else {
                continue;
            };
            match notification.method.as_str() {
                "item/completed" => {
                    let completed: ItemCompletedNotification = serde_json::from_value(
                        notification.params.clone().expect("notification params"),
                    )?;
                    if let ThreadItem::CollabAgentToolCall {
                        id,
                        tool,
                        status,
                        receiver_thread_ids,
                        ..
                    } = completed.item
                        && id == SPAWN_CALL_ID
                    {
                        assert_eq!(tool, CollabAgentTool::SpawnAgent);
                        assert_eq!(status, CollabAgentToolCallStatus::Completed);
                        spawn_child_thread_id = Some(
                            receiver_thread_ids
                                .first()
                                .expect("spawn result has a child thread")
                                .clone(),
                        );
                    }
                }
                "item/started" => {
                    let started: ItemStartedNotification = serde_json::from_value(
                        notification.params.clone().expect("notification params"),
                    )?;
                    if started.thread_id == thread.id
                        && matches!(
                            &started.item,
                            ThreadItem::CollabAgentToolCall {
                                id,
                                tool: CollabAgentTool::Wait,
                                status: CollabAgentToolCallStatus::InProgress,
                                ..
                            } if id == WAIT_CALL_ID
                        )
                    {
                        assert_eq!(started.turn_id, parent_turn.turn.id);
                        parent_wait_started = true;
                    }
                }
                "turn/started" => {
                    started_turns.push(serde_json::from_value(
                        notification.params.clone().expect("notification params"),
                    )?);
                }
                _ => {}
            }
            notifications.push(notification);

            if let Some(child_thread_id) = spawn_child_thread_id.as_deref() {
                child_continuation_started = started_turns
                    .iter()
                    .filter(|started| started.thread_id == child_thread_id)
                    .count()
                    >= 2;
                if parent_followup_matched
                    && parent_wait_started
                    && child_continuation_started
                {
                    return Ok::<(), anyhow::Error>(());
                }
            }
        }
    })
    .await;
    if pre_observation.is_err() {
        return Err(anyhow::anyhow!(
            "pre-observation timed out: parent_followup_matched={parent_followup_matched}, parent_wait_started={parent_wait_started}, child_continuation_started={child_continuation_started}"
        ));
    }
    pre_observation.expect("pre-observation timeout was checked")?;

    let child_thread_id = spawn_child_thread_id.expect("spawned child thread id");
    let child_turns = started_turns
        .iter()
        .filter(|started| started.thread_id == child_thread_id)
        .collect::<Vec<_>>();
    assert_eq!(child_turns.len(), 2);
    let initial_child_turn_id = child_turns[0].turn.id.clone();
    let continuation_turn_id = child_turns[1].turn.id.clone();
    assert_ne!(initial_child_turn_id, continuation_turn_id);

    let continuation_request = timeout(READ_TIMEOUT, continuation_matched_rx).await??;
    let continuation_request_json: serde_json::Value =
        serde_json::from_slice(&continuation_request)?;
    assert!(request_contains(
        &continuation_request_json,
        CONTINUATION_PROMPT
    ));
    assert_eq!(
        continuation_request_json["client_metadata"]["thread_id"],
        json!(child_thread_id)
    );
    assert_eq!(
        continuation_request_json["client_metadata"]["turn_id"],
        json!(continuation_turn_id)
    );

    // Observe the entire bounded interval while the matched response's sender is
    // still withheld. Keep the in-flight app-server read alive across the
    // deadline: its current read_line-backed implementation is not cancel-safe.
    let quiet_deadline = tokio::time::Instant::now() + Duration::from_millis(750);
    let held_observer: Result<()> = async {
        loop {
            let mut read = Box::pin(mcp.read_next_message());
            let (message, quiet_elapsed) = tokio::select! {
                result = read.as_mut() => (result?, false),
                _ = tokio::time::sleep_until(quiet_deadline) => {
                    let requests_while_held = provider.requests().await;
                    assert!(
                        !requests_while_held.iter().any(|request| {
                            String::from_utf8_lossy(request).contains(WAIT_CALL_ID)
                        }),
                        "parent issued a follow-up while the child continuation response was held"
                    );
                    assert!(
                        !requests_while_held.iter().any(|request| {
                            String::from_utf8_lossy(request).contains(UPDATE_GOAL_CALL_ID)
                        }),
                        "child continued past update_goal before the held response was released"
                    );
                    continuation_release_tx
                        .take()
                        .expect("continuation response gate is released once")
                        .send(())
                        .map_err(|_| anyhow::anyhow!("continuation response receiver was dropped"))?;
                    (timeout(READ_TIMEOUT, read.as_mut()).await??, true)
                }
            };
            if let JSONRPCMessage::Notification(notification) = message {
                let parent_wait_completed = if notification.method == "item/completed" {
                    let completed: ItemCompletedNotification = serde_json::from_value(
                        notification.params.clone().expect("notification params"),
                    )?;
                    completed.thread_id == thread.id
                        && matches!(
                            completed.item,
                            ThreadItem::CollabAgentToolCall {
                                id,
                                tool: CollabAgentTool::Wait,
                                status: CollabAgentToolCallStatus::Completed,
                                ..
                            } if id == WAIT_CALL_ID
                        )
                } else {
                    false
                };
                let parent_turn_completed = if notification.method == "turn/completed" {
                    let completed: TurnCompletedNotification = serde_json::from_value(
                        notification.params.clone().expect("notification params"),
                    )?;
                    completed.thread_id == thread.id && completed.turn.id == parent_turn.turn.id
                } else {
                    false
                };
                notifications.push(notification);
                if !quiet_elapsed && (parent_wait_completed || parent_turn_completed) {
                    anyhow::bail!("parent handed back while child continuation response was held");
                }
            }
            if quiet_elapsed {
                return Ok(());
            }
        }
    }
    .await;
    held_observer?;

    timeout(READ_TIMEOUT, async {
        loop {
            let child_turn_completed = notifications.iter().any(|notification| {
                notification.method == "turn/completed"
                    && notification.params.as_ref().is_some_and(|params| {
                        params["threadId"] == child_thread_id
                            && params["turn"]["id"] == continuation_turn_id
                            && params["turn"]["status"] == "completed"
                    })
            });
            let parent_wait_completed = notifications.iter().any(|notification| {
                if notification.method != "item/completed" {
                    return false;
                }
                let Ok(completed) = serde_json::from_value::<ItemCompletedNotification>(
                    notification.params.clone().expect("notification params"),
                ) else {
                    return false;
                };
                completed.thread_id == thread.id
                    && matches!(
                        completed.item,
                        ThreadItem::CollabAgentToolCall {
                            id,
                            tool: CollabAgentTool::Wait,
                            status: CollabAgentToolCallStatus::Completed,
                            ..
                        } if id == WAIT_CALL_ID
                    )
            });
            let parent_turn_completed = notifications.iter().any(|notification| {
                notification.method == "turn/completed"
                    && notification.params.as_ref().is_some_and(|params| {
                        params["threadId"] == thread.id
                            && params["turn"]["id"] == parent_turn.turn.id
                    })
            });
            if child_turn_completed && parent_wait_completed && parent_turn_completed {
                return Ok::<(), anyhow::Error>(());
            }
            let message = mcp.read_next_message().await?;
            if let JSONRPCMessage::Notification(notification) = message {
                notifications.push(notification);
            }
        }
    })
    .await??;

    let continuation_started_index = notifications
        .iter()
        .position(|notification| {
            notification.method == "turn/started"
                && notification.params.as_ref().is_some_and(|params| {
                    params["threadId"] == child_thread_id
                        && params["turn"]["id"] == continuation_turn_id
                })
        })
        .expect("native child continuation Started notification");
    let completed_goal_indices = notifications
        .iter()
        .enumerate()
        .filter_map(|(index, notification)| {
            if notification.method != "thread/goal/updated" {
                return None;
            }
            let update: ThreadGoalUpdatedNotification = serde_json::from_value(
                notification.params.clone().expect("notification params"),
            )
            .expect("valid goal update notification");
            (update.thread_id == child_thread_id
                && update.turn_id.as_deref() == Some(continuation_turn_id.as_str())
                && update.goal.status == ThreadGoalStatus::Complete)
                .then_some(index)
        })
        .collect::<Vec<_>>();
    assert_eq!(completed_goal_indices.len(), 1);
    let goal_completed_index = completed_goal_indices[0];
    let continuation_completed_index = notifications
        .iter()
        .position(|notification| {
            notification.method == "turn/completed"
                && notification.params.as_ref().is_some_and(|params| {
                    params["threadId"] == child_thread_id
                        && params["turn"]["id"] == continuation_turn_id
                        && params["turn"]["status"] == "completed"
                })
        })
        .expect("child continuation completed");
    let parent_wait_started_indices = notifications
        .iter()
        .enumerate()
        .filter_map(|(index, notification)| {
            if notification.method != "item/started" {
                return None;
            }
            let started: ItemStartedNotification = serde_json::from_value(
                notification.params.clone().expect("notification params"),
            )
            .expect("valid item start notification");
            (started.thread_id == thread.id
                && matches!(
                    started.item,
                    ThreadItem::CollabAgentToolCall {
                        id,
                        tool: CollabAgentTool::Wait,
                        status: CollabAgentToolCallStatus::InProgress,
                        ..
                    } if id == WAIT_CALL_ID
                ))
            .then_some(index)
        })
        .collect::<Vec<_>>();
    assert_eq!(parent_wait_started_indices.len(), 1);
    let parent_wait_completed_indices = notifications
        .iter()
        .enumerate()
        .filter_map(|(index, notification)| {
            if notification.method != "item/completed" {
                return None;
            }
            let completed: ItemCompletedNotification = serde_json::from_value(
                notification.params.clone().expect("notification params"),
            )
            .expect("valid item completion notification");
            (completed.thread_id == thread.id
                && matches!(
                    completed.item,
                    ThreadItem::CollabAgentToolCall {
                        id,
                        tool: CollabAgentTool::Wait,
                        status: CollabAgentToolCallStatus::Completed,
                        ..
                    }
                        if id == WAIT_CALL_ID
                ))
            .then_some(index)
        })
        .collect::<Vec<_>>();
    assert_eq!(parent_wait_completed_indices.len(), 1);
    let parent_turn_completed_indices = notifications
        .iter()
        .enumerate()
        .filter_map(|(index, notification)| {
            if notification.method != "turn/completed" {
                return None;
            }
            let completed: TurnCompletedNotification = serde_json::from_value(
                notification.params.clone().expect("notification params"),
            )
            .expect("valid turn completion notification");
            (completed.thread_id == thread.id
                && completed.turn.id == parent_turn.turn.id
                && completed.turn.status == TurnStatus::Completed
                && completed.turn.error.is_none())
                .then_some(index)
        })
        .collect::<Vec<_>>();
    assert_eq!(parent_turn_completed_indices.len(), 1);
    assert!(
        continuation_started_index < goal_completed_index
            && goal_completed_index < continuation_completed_index,
        "native child goal completion must follow Started and precede its turn completion"
    );
    assert!(
        parent_wait_started_indices[0] < parent_wait_completed_indices[0]
            && parent_wait_completed_indices[0] < parent_turn_completed_indices[0],
        "the parent's wait item must complete before its turn"
    );

    let request_order = provider.requests().await;
    assert_eq!(request_order.len(), 7);
    let request_json = |request: &[u8]| {
        serde_json::from_slice::<serde_json::Value>(request).expect("valid provider request JSON")
    };
    let request_contains_bytes = |request: &[u8], text: &str| {
        String::from_utf8_lossy(request).contains(text)
    };
    let initial_child_request = request_order
        .iter()
        .find(|request| {
            request_contains_bytes(request, CHILD_PROMPT)
                && !request_contains_bytes(request, CREATE_GOAL_CALL_ID)
                && request_json(request)["client_metadata"]["thread_id"]
                    == json!(child_thread_id)
        })
        .expect("initial child provider request");
    assert_eq!(
        request_json(initial_child_request)["client_metadata"]["turn_id"],
        json!(initial_child_turn_id)
    );
    let child_final_request = request_order
        .iter()
        .find(|request| {
            request_contains_bytes(request, UPDATE_GOAL_CALL_ID)
                && request_json(request)["client_metadata"]["thread_id"]
                    == json!(child_thread_id)
                && request_json(request)["client_metadata"]["turn_id"]
                    == json!(continuation_turn_id)
        })
        .expect("provider follow-up after completing the goal");
    assert_eq!(
        request_json(child_final_request)["client_metadata"]["thread_id"],
        json!(child_thread_id)
    );
    assert_eq!(
        request_json(child_final_request)["client_metadata"]["turn_id"],
        json!(continuation_turn_id)
    );
    let final_parent_request = request_order
        .iter()
        .find(|request| {
            request_contains_bytes(request, WAIT_CALL_ID)
                && request_json(request)["client_metadata"]["thread_id"] == json!(thread.id)
                && request_json(request)["client_metadata"]["turn_id"]
                    == json!(parent_turn.turn.id)
        })
        .expect("parent follow-up after native child handback");
    assert_eq!(
        request_json(final_parent_request)["client_metadata"]["thread_id"],
        json!(thread.id)
    );
    assert_eq!(
        request_json(final_parent_request)["client_metadata"]["turn_id"],
        json!(parent_turn.turn.id)
    );
    let final_parent_request_json = request_json(final_parent_request);
    let wait_output_text = final_parent_request_json["input"]
        .as_array()
        .and_then(|input| {
            input.iter().find(|item| {
                item["type"] == "function_call_output" && item["call_id"] == WAIT_CALL_ID
            })
        })
        .and_then(|item| item["output"].as_str())
        .expect("parent follow-up includes the wait result");
    let wait_output: serde_json::Value = serde_json::from_str(wait_output_text)?;
    assert_eq!(wait_output["timed_out"], false);
    assert_eq!(wait_output["message"], "Wait completed.");

    let child_continuation_request_index = request_order
        .iter()
        .position(|request| {
            request_contains_bytes(request, CONTINUATION_PROMPT)
                && request_json(request)["client_metadata"]["thread_id"]
                    == json!(child_thread_id)
                && request_json(request)["client_metadata"]["turn_id"]
                    == json!(continuation_turn_id)
        })
        .expect("native continuation provider request");
    let child_final_request_index = request_order
        .iter()
        .position(|request| request_contains_bytes(request, UPDATE_GOAL_CALL_ID))
        .expect("provider follow-up after completing the goal");
    let parent_final_request_index = request_order
        .iter()
        .position(|request| request_contains_bytes(request, WAIT_CALL_ID))
        .expect("parent follow-up after native child handback");
    assert!(
        child_continuation_request_index < child_final_request_index
            && child_final_request_index < parent_final_request_index,
        "child continuation request and completion must precede the parent's wait follow-up"
    );

    provider.shutdown().await;
    analytics_server.verify().await;
    Ok(())
}
