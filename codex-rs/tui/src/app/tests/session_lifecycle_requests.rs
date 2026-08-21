use super::*;
use app_test_support::create_fake_parented_rollout_with_source;
use app_test_support::create_fake_rollout;
use app_test_support::rollout_path;
use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::CommandExecutionRequestApprovalParams;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId as AppServerRequestId;
use codex_app_server_protocol::ServerRequest;
use codex_protocol::AgentPath;
use codex_state::SqliteConfig;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

/// Returns and resets `(thread/loaded/list, thread/read)` request counts.
fn take_backfill_counts(requests: &Arc<Mutex<Vec<String>>>) -> (usize, usize) {
    let requests = std::mem::take(&mut *requests.lock().expect("request recorder lock"));
    (
        requests
            .iter()
            .filter(|method| *method == "thread/loaded/list")
            .count(),
        requests
            .iter()
            .filter(|method| *method == "thread/read")
            .count(),
    )
}

/// Starts an embedded app server behind a loopback WebSocket proxy that records JSON-RPC methods.
async fn start_recording_app_server(
    config: &Config,
) -> Result<(
    AppServerSession,
    Arc<Mutex<Vec<String>>>,
    JoinHandle<Result<()>>,
)> {
    start_recording_app_server_with_scripts(
        config,
        None,
        VecDeque::new(),
        /*force_resume_failure*/ false,
        /*resume_signal*/ None,
        /*event_recorded_signal*/ None,
    )
    .await
}

/// Starts the recording proxy with an immediate closed transition after unsubscribe. The real
/// server unloads idle threads asynchronously, so this keeps selection tests deterministic while
/// preserving the loaded status after a later resume.
async fn start_recording_app_server_with_closed_transition(
    config: &Config,
    thread_id: ThreadId,
) -> Result<(
    AppServerSession,
    Arc<Mutex<Vec<String>>>,
    JoinHandle<Result<()>>,
)> {
    start_recording_app_server_with_scripts(
        config,
        Some(thread_id.to_string()),
        VecDeque::from([ScriptedThreadRead {
            include_turns: false,
            status: ScriptedThreadStatus::NotLoaded,
        }]),
        /*force_resume_failure*/ false,
        /*resume_signal*/ None,
        /*event_recorded_signal*/ None,
    )
    .await
}

/// Starts the recording proxy with deterministic resume failure injection for fallback tests.
async fn start_recording_app_server_with_resume_failure(
    config: &Config,
    resume_signal: Arc<tokio::sync::Notify>,
    event_recorded_signal: Arc<tokio::sync::Notify>,
) -> Result<(AppServerSession, JoinHandle<Result<()>>)> {
    let (app_server, _requests, proxy) = start_recording_app_server_with_scripts(
        config,
        None,
        VecDeque::new(),
        /*force_resume_failure*/ true,
        Some(resume_signal),
        Some(event_recorded_signal),
    )
    .await?;
    Ok((app_server, proxy))
}

#[derive(Clone, Copy)]
enum ScriptedThreadStatus {
    NotLoaded,
    Active,
}

#[derive(Clone, Copy)]
struct ScriptedThreadRead {
    include_turns: bool,
    status: ScriptedThreadStatus,
}

async fn start_recording_app_server_with_scripts(
    config: &Config,
    scripted_thread_id: Option<String>,
    post_unsubscribe_thread_reads: VecDeque<ScriptedThreadRead>,
    force_resume_failure: bool,
    resume_signal: Option<Arc<tokio::sync::Notify>>,
    event_recorded_signal: Option<Arc<tokio::sync::Notify>>,
) -> Result<(
    AppServerSession,
    Arc<Mutex<Vec<String>>>,
    JoinHandle<Result<()>>,
)> {
    let state_db =
        crate::init_state_db_for_app_server_target(config, &crate::AppServerTarget::Embedded)
            .await?;
    let embedded = crate::start_embedded_app_server(
        codex_arg0::Arg0DispatchPaths::default(),
        config.clone(),
        Vec::new(),
        codex_config::LoaderOverrides::default(),
        /*strict_config*/ false,
        codex_config::CloudConfigBundleLoader::default(),
        codex_feedback::CodexFeedback::new(),
        /*log_db*/ None,
        state_db,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    )
    .await?;
    let codex_home = config.codex_home.display().to_string();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let request_sink = Arc::clone(&requests);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let websocket_url = format!("ws://{}", listener.local_addr()?);
    let proxy = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut websocket = accept_async(stream).await?;
        let mut post_unsubscribe_thread_reads = post_unsubscribe_thread_reads;
        let mut after_unsubscribe = false;
        while let Some(frame) = websocket.next().await {
            let Message::Text(text) = frame? else {
                continue;
            };
            let message = serde_json::from_str::<JSONRPCMessage>(&text)?;
            match message {
                JSONRPCMessage::Request(request) if request.method == "initialize" => {
                    websocket
                        .send(Message::Text(
                            serde_json::to_string(&JSONRPCMessage::Response(JSONRPCResponse {
                                id: request.id,
                                result: serde_json::json!({
                                    "userAgent": "codex-tui-test",
                                    "codexHome": codex_home,
                                }),
                            }))?
                            .into(),
                        ))
                        .await?;
                }
                JSONRPCMessage::Request(request) => {
                    request_sink
                        .lock()
                        .expect("request recorder lock")
                        .push(request.method.clone());
                    let request_id = request.id.clone();
                    let request_thread_id = request
                        .params
                        .as_ref()
                        .and_then(|params| params.get("threadId"))
                        .and_then(serde_json::Value::as_str);
                    if request.method == "thread/unsubscribe"
                        && scripted_thread_id.as_deref() == request_thread_id
                    {
                        after_unsubscribe = true;
                    }
                    if force_resume_failure && request.method == "thread/resume" {
                        if let Some(signal) = &resume_signal {
                            signal.notify_one();
                        }
                        if let Some(signal) = &event_recorded_signal {
                            signal.notified().await;
                        }
                        websocket
                            .send(Message::Text(
                                serde_json::to_string(&JSONRPCMessage::Error(JSONRPCError {
                                    id: request_id,
                                    error: codex_app_server_protocol::JSONRPCErrorError {
                                        code: -32000,
                                        data: None,
                                        message: "forced resume failure".to_string(),
                                    },
                                }))?
                                .into(),
                            ))
                            .await?;
                        continue;
                    }
                    let scripted_status = if after_unsubscribe
                        && request.method == "thread/read"
                        && scripted_thread_id.as_deref() == request_thread_id
                    {
                        let include_turns = request.params.as_ref().is_some_and(|params| {
                            params
                                .get("includeTurns")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(false)
                        });
                        post_unsubscribe_thread_reads
                            .front()
                            .filter(|step| step.include_turns == include_turns)
                            .copied()
                    } else {
                        None
                    };
                    let request =
                        serde_json::from_value::<ClientRequest>(serde_json::to_value(request)?)?;
                    let response = match embedded.request(request).await? {
                        Ok(mut result) => {
                            if let Some(scripted_status) = scripted_status {
                                post_unsubscribe_thread_reads.pop_front();
                                if let Some(thread) = result.get_mut("thread") {
                                    thread["status"] = match scripted_status.status {
                                        ScriptedThreadStatus::NotLoaded => serde_json::json!({
                                            "type": "notLoaded",
                                        }),
                                        ScriptedThreadStatus::Active => serde_json::json!({
                                            "type": "active",
                                            "activeFlags": [],
                                        }),
                                    };
                                }
                            }
                            JSONRPCMessage::Response(JSONRPCResponse {
                                id: request_id,
                                result,
                            })
                        }
                        Err(error) => JSONRPCMessage::Error(JSONRPCError {
                            id: request_id,
                            error,
                        }),
                    };
                    websocket
                        .send(Message::Text(serde_json::to_string(&response)?.into()))
                        .await?;
                }
                JSONRPCMessage::Notification(notification)
                    if notification.method == "initialized" => {}
                JSONRPCMessage::Notification(notification) => {
                    embedded
                        .notify(serde_json::from_value::<ClientNotification>(
                            serde_json::to_value(notification)?,
                        )?)
                        .await?;
                }
                JSONRPCMessage::Response(_) | JSONRPCMessage::Error(_) => {}
            }
        }
        embedded.shutdown().await?;
        Ok(())
    });
    let app_server = crate::connect_remote_app_server(crate::RemoteAppServerEndpoint::WebSocket {
        websocket_url,
        auth_token: None,
    })
    .await?;

    Ok((
        AppServerSession::new(
            app_server,
            crate::app_server_session::ThreadParamsMode::Embedded,
        ),
        requests,
        proxy,
    ))
}

#[tokio::test]
async fn failed_resume_fallback_replaces_events_arriving_after_fence() -> Result<()> {
    let mut app = make_test_app().await;
    let thread_id = ThreadId::from_string(
        &create_fake_rollout(
            app.config.codex_home.as_path(),
            "2026-08-21T02-00-00",
            "2026-08-21T02:00:00Z",
            "authoritative fallback output",
            Some(app.config.model_provider_id.as_str()),
            /*git_info*/ None,
        )
        .expect("fallback rollout should be created"),
    )?;
    let resume_signal = Arc::new(tokio::sync::Notify::new());
    let event_recorded_signal = Arc::new(tokio::sync::Notify::new());
    let (mut app_server, proxy) = start_recording_app_server_with_resume_failure(
        &app.config,
        Arc::clone(&resume_signal),
        Arc::clone(&event_recorded_signal),
    )
    .await?;

    let mut channel = ThreadEventChannel::new(/*capacity*/ 4);
    channel.mark_replay_only();
    let sender = channel.sender.clone();
    let store = Arc::clone(&channel.store);
    app.thread_event_channels.insert(thread_id, channel);
    let stale_request = ServerRequest::CommandExecutionRequestApproval {
        request_id: AppServerRequestId::Integer(901),
        params: CommandExecutionRequestApprovalParams {
            thread_id: thread_id.to_string(),
            turn_id: "stale-turn".to_string(),
            item_id: "stale-approval".to_string(),
            started_at_ms: 0,
            approval_id: None,
            environment_id: None,
            reason: None,
            network_approval_context: None,
            command: Some("echo stale".to_string()),
            cwd: None,
            command_actions: None,
            additional_permissions: None,
            proposed_execpolicy_amendment: None,
            proposed_network_policy_amendments: None,
            available_decisions: None,
        },
    };
    let event_task = tokio::spawn(async move {
        resume_signal.notified().await;
        {
            let mut store = store.lock().await;
            store.push_request(stale_request.clone());
        }
        event_recorded_signal.notify_one();
        let _ = sender
            .send(ThreadBufferedEvent::Request(stale_request))
            .await;
    });

    assert!(
        !app.attach_live_thread_for_selection(&mut app_server, thread_id)
            .await?
    );
    event_task.await?;

    {
        let channel = app
            .thread_event_channels
            .get(&thread_id)
            .expect("fallback channel");
        let store = channel.store.lock().await;
        assert_eq!(store.turns.len(), 1);
        assert!(!store.has_pending_thread_approvals());
        assert!(
            store
                .buffer
                .iter()
                .all(|event| !matches!(event, ThreadBufferedEvent::Request(_)))
        );
        assert!(
            channel
                .receiver
                .as_ref()
                .expect("inactive fallback receiver")
                .is_empty()
        );
    }

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn shutdown_skips_unsubscribe_for_replay_only_widget_thread() -> Result<()> {
    let (mut app_server, requests, proxy) = {
        let app = make_test_app().await;
        start_recording_app_server(app.chat_widget.config_ref()).await?
    };

    let replay_thread_id = ThreadId::new();
    let mut replay_app = make_test_app().await;
    let mut replay_channel = ThreadEventChannel::new(/*capacity*/ 4);
    replay_channel.mark_replay_only();
    replay_app
        .thread_event_channels
        .insert(replay_thread_id, replay_channel);
    replay_app
        .chat_widget
        .handle_thread_session(test_thread_session(
            replay_thread_id,
            test_path_buf("/tmp/replay-shutdown"),
        ));
    replay_app.shutdown_current_thread(&mut app_server).await;
    assert!(
        !requests
            .lock()
            .expect("request recorder lock")
            .iter()
            .any(|method| method == "thread/unsubscribe")
    );

    let live_thread_id = ThreadId::new();
    let mut live_app = make_test_app().await;
    live_app
        .thread_event_channels
        .insert(live_thread_id, ThreadEventChannel::new(/*capacity*/ 4));
    live_app
        .chat_widget
        .handle_thread_session(test_thread_session(
            live_thread_id,
            test_path_buf("/tmp/live-shutdown"),
        ));
    live_app.shutdown_current_thread(&mut app_server).await;
    assert!(
        requests
            .lock()
            .expect("request recorder lock")
            .iter()
            .any(|method| method == "thread/unsubscribe")
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[test]
fn fresh_session_applies_requested_name() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-named-fresh-session".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let mut app = make_test_app().await;
                let codex_home = tempdir()?;
                app.config.codex_home = codex_home.path().to_path_buf().abs();
                app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
                let (mut app_server, requests, proxy) =
                    start_recording_app_server(&app.config).await?;
                let mut tui = crate::tui::test_support::make_test_tui()?;

                app.start_fresh_session_with_summary_hint(
                    &mut tui,
                    &mut app_server,
                    /*session_start_source*/ None,
                    /*initial_user_message*/ None,
                    /*new_thread_name*/ Some("Add User".to_string()),
                )
                .await;

                let thread_id = app
                    .chat_widget
                    .thread_id()
                    .expect("fresh session should have a thread id");
                assert_eq!(app.chat_widget.thread_name(), Some("Add User".to_string()));
                assert!(
                    requests
                        .lock()
                        .expect("request recorder lock")
                        .iter()
                        .any(|method| method == "thread/name/set"),
                    "fresh session should be named through the app server"
                );
                let thread = app_server
                    .thread_read(thread_id, /*include_turns*/ false)
                    .await?;
                assert_eq!(thread.name.as_deref(), Some("Add User"));

                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("named fresh session test thread")
}

#[test]
fn session_lifecycle_avoids_redundant_subagent_metadata_reads() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-session-lifecycle-requests".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let mut app = make_test_app().await;
                let codex_home = tempdir()?;
                app.config.codex_home = codex_home.path().to_path_buf().abs();
                app.config.sqlite =
                    codex_state::SqliteConfig::new_for_testing(codex_home.path().abs());
                let root_timestamp = "2026-01-01T00-00-00";
                let root_thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home.path(),
                        root_timestamp,
                        "2026-01-01T00:00:00Z",
                        "Saved user message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .expect("create root rollout"),
                )?;
                let child_thread_id = ThreadId::from_string(
                    &create_fake_parented_rollout_with_source(
                        codex_home.path(),
                        "2026-01-01T00-00-01",
                        "2026-01-01T00:00:01Z",
                        "Saved child message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                        RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id: root_thread_id,
                            depth: 1,
                            agent_path: Some(
                                AgentPath::try_from("/root/worker").expect("valid agent path"),
                            ),
                            agent_nickname: Some("worker".to_string()),
                            agent_role: Some("worker".to_string()),
                        }),
                        root_thread_id.into(),
                        root_thread_id,
                    )
                    .expect("create child rollout"),
                )?;
                let root_rollout_path = rollout_path(
                    codex_home.path(),
                    root_timestamp,
                    &root_thread_id.to_string(),
                );
                let (mut app_server, requests, proxy) =
                    start_recording_app_server_with_closed_transition(&app.config, child_thread_id)
                        .await?;
                let root = app_server
                    .resume_thread(
                        app.config.clone(),
                        root_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                app.enqueue_primary_thread_session(root.session, root.turns)
                    .await?;
                app_server
                    .resume_thread(
                        app.config.clone(),
                        child_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                let mut tui = crate::tui::test_support::make_test_tui()?;
                take_backfill_counts(&requests);

                let control = Box::pin(app.handle_event(
                    &mut tui,
                    &mut app_server,
                    AppEvent::ForkCurrentSession,
                ))
                .await?;

                assert!(matches!(control, AppRunControl::Continue));
                assert_ne!(app.chat_widget.thread_id(), Some(root_thread_id));
                // Forking may read the source metadata once when the response includes its parent
                // id. It must not scan or backfill loaded threads for the newly created fork.
                assert!(matches!(take_backfill_counts(&requests), (0, 0) | (0, 1)));

                app.start_fresh_session_with_summary_hint(
                    &mut tui,
                    &mut app_server,
                    /*session_start_source*/ None,
                    /*initial_user_message*/ None,
                    /*new_thread_name*/ None,
                )
                .await;

                assert_ne!(app.chat_widget.thread_id(), Some(root_thread_id));
                assert_eq!(take_backfill_counts(&requests), (0, 0));

                let loaded_threads = app_server
                    .thread_loaded_list(ThreadLoadedListParams {
                        cursor: None,
                        limit: None,
                    })
                    .await?
                    .data;
                let expected_reads = loaded_threads
                    .iter()
                    .filter(|thread_id| *thread_id != &root_thread_id.to_string())
                    .count();
                assert!(loaded_threads.contains(&child_thread_id.to_string()));
                take_backfill_counts(&requests);
                app.harness_overrides.cwd = Some(app.config.cwd.to_path_buf());

                let control = app
                    .resume_target_session(
                        &mut tui,
                        &mut app_server,
                        crate::resume_picker::SessionTarget {
                            path: Some(root_rollout_path),
                            thread_id: root_thread_id,
                        },
                    )
                    .await?;

                assert!(matches!(control, AppRunControl::Continue));
                assert_eq!(app.chat_widget.thread_id(), Some(root_thread_id));
                assert_eq!(take_backfill_counts(&requests), (1, expected_reads));
                assert_eq!(
                    app.agent_navigation.get(&child_thread_id),
                    Some(&AgentPickerThreadEntry {
                        agent_nickname: Some("worker".to_string()),
                        agent_role: Some("worker".to_string()),
                        agent_path: Some("/root/worker".to_string()),
                        is_running: false,
                        is_closed: false,
                        created_at: None,
                        updated_at: None,
                        ..AgentPickerThreadEntry::default()
                    })
                );

                // The local channel still has a completed snapshot, but the server can unload
                // it independently. Opening the picker must refresh authoritative liveness rather
                // than trusting that cached terminal state, and must close the channel for
                // replay-only mutation guards.
                app_server.thread_unsubscribe(child_thread_id).await?;
                take_backfill_counts(&requests);

                Box::pin(app.open_agent_picker(&mut app_server)).await;

                let (_, reads) = take_backfill_counts(&requests);
                assert!(reads >= 1, "picker must refresh the cached child liveness");
                assert!(
                    app.agent_navigation
                        .get(&child_thread_id)
                        .is_some_and(|entry| entry.is_closed)
                );
                assert_eq!(
                    app.thread_event_channels
                        .get(&child_thread_id)
                        .expect("child channel")
                        .attachment(),
                    ThreadEventAttachment::ReplayOnly
                );
                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("session lifecycle request test thread")
}

#[test]
fn closed_existing_stale_channel_refreshes_persisted_transcript() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-closed-channel-refresh".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
                let codex_home = app.config.codex_home.as_path();
                let root_thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home,
                        "2026-01-01T00-00-00",
                        "2026-01-01T00:00:00Z",
                        "Saved root message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .expect("create root rollout"),
                )?;
                let child_thread_id = ThreadId::from_string(
                    &create_fake_parented_rollout_with_source(
                        codex_home,
                        "2026-01-01T00-00-01",
                        "2026-01-01T00:00:01Z",
                        "Saved child message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                        RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id: root_thread_id,
                            depth: 1,
                            agent_path: None,
                            agent_nickname: Some("worker".to_string()),
                            agent_role: Some("worker".to_string()),
                        }),
                        root_thread_id.into(),
                        root_thread_id,
                    )
                    .expect("create child rollout"),
                )?;
                let child_rollout_path = rollout_path(
                    codex_home,
                    "2026-01-01T00-00-01",
                    &child_thread_id.to_string(),
                );

                let (mut app_server, requests, proxy) =
                    start_recording_app_server_with_closed_transition(&app.config, child_thread_id)
                        .await?;
                let started = app_server
                    .resume_thread(
                        app.config.clone(),
                        child_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                assert!(!started.turns.is_empty());
                let old_turns = started.turns.clone();
                for item in [
                    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                        turn_id: "later-turn".to_string(),
                        trace_id: None,
                        started_at: None,
                        model_context_window: None,
                        collaboration_mode_kind: ModeKind::default(),
                    })),
                    RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                        message: "Saved later child message".to_string(),
                        ..Default::default()
                    })),
                    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
                        turn_id: "later-turn".to_string(),
                        last_agent_message: None,
                        error: None,
                        started_at: None,
                        compaction_events_in_turn: 0,
                        final_model: None,
                        model_snapshot: None,
                        provider_usage: None,
                        completed_at: None,
                        duration_ms: None,
                        time_to_first_token_ms: None,
                    })),
                ] {
                    codex_rollout::append_rollout_item_to_path(&child_rollout_path, &item).await?;
                }

                // A spawn can leave an existing live channel with an older local snapshot. The
                // server can unload that thread independently, so a closed-thread selection must
                // refresh it before rendering later persisted turns.
                app.thread_event_channels.insert(
                    child_thread_id,
                    ThreadEventChannel::new_with_session(
                        /*capacity*/ 4,
                        started.session.clone(),
                        old_turns.clone(),
                    ),
                );
                app.agent_navigation.upsert(
                    child_thread_id,
                    Some("worker".to_string()),
                    Some("worker".to_string()),
                    /*is_closed*/ false,
                    /*created_at*/ None,
                    /*updated_at*/ None,
                );
                assert_eq!(
                    app.thread_event_channels
                        .get(&child_thread_id)
                        .expect("existing channel")
                        .attachment(),
                    ThreadEventAttachment::Live
                );
                {
                    let store = app
                        .thread_event_channels
                        .get(&child_thread_id)
                        .expect("existing channel")
                        .store
                        .lock()
                        .await;
                    assert!(store.session.is_some());
                    assert_eq!(store.turns, old_turns);
                }

                app_server.thread_unsubscribe(child_thread_id).await?;
                // Preserve the picker metadata observed by a prior refresh while retaining the
                // live channel. Selection must fence that channel before its next liveness read.
                app.mark_agent_picker_thread_closed(child_thread_id);
                assert!(
                    app.agent_navigation
                        .get(&child_thread_id)
                        .is_some_and(|entry| entry.is_closed)
                );
                assert_eq!(
                    app.thread_event_channels
                        .get(&child_thread_id)
                        .expect("retained channel")
                        .attachment(),
                    ThreadEventAttachment::Live
                );
                app.thread_event_channels
                    .get(&child_thread_id)
                    .expect("retained channel")
                    .sender
                    .try_send(ThreadBufferedEvent::Notification(
                        thread_closed_notification(child_thread_id),
                    ))
                    .expect("stale retained-channel event should be queued");

                let mut tui = crate::tui::test_support::make_test_tui()?;
                app.select_agent_thread(&mut tui, &mut app_server, child_thread_id)
                    .await?;
                assert!(
                    app.active_thread_rx
                        .as_mut()
                        .expect("hydrated thread receiver")
                        .try_recv()
                        .is_err(),
                    "closed hydration must fence events queued by the prior attachment"
                );

                let hydrated_turns = {
                    let store = app
                        .thread_event_channels
                        .get(&child_thread_id)
                        .expect("hydrated channel")
                        .store
                        .lock()
                        .await;
                    assert!(store.session.is_some());
                    assert!(store.turns.len() > old_turns.len());
                    assert!(store.turns.iter().any(|turn| {
                        turn.items.iter().any(|item| {
                            matches!(
                                item,
                                ThreadItem::UserMessage { content, .. }
                                    if content.iter().any(|input| matches!(
                                        input,
                                        AppServerUserInput::Text { text, .. }
                                            if text == "Saved later child message"
                                    ))
                            )
                        })
                    }));
                    assert_eq!(
                        app.thread_event_channels
                            .get(&child_thread_id)
                            .expect("hydrated channel")
                            .attachment(),
                        ThreadEventAttachment::ReplayOnly
                    );
                    store.turns.clone()
                };
                let persisted = app_server
                    .thread_read(child_thread_id, /*include_turns*/ true)
                    .await?;
                assert_eq!(hydrated_turns, persisted.turns);

                let rendered = std::iter::from_fn(|| app_event_rx.try_recv().ok())
                    .filter_map(|event| match event {
                        AppEvent::InsertHistoryCell(cell) => {
                            Some(lines_to_single_string(&cell.display_lines(/*width*/ 120)))
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert!(
                    rendered
                        .iter()
                        .any(|cell| cell.contains("Saved later child message"))
                );

                assert!(
                    take_backfill_counts(&requests).1 >= 2,
                    "closure refresh and transcript hydration must both read"
                );
                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("closed channel refresh test thread")
}

#[test]
fn closed_thread_status_race_reconciles_live_attachment() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-closed-thread-status-race".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
                let codex_home = app.config.codex_home.as_path();
                let thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home,
                        "2026-01-01T00-00-00",
                        "2026-01-01T00:00:00Z",
                        "Saved status-race message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .expect("create status-race rollout"),
                )?;

                // The proxy turns the authoritative include-turns read into an active response,
                // while the preceding liveness reads still observe the embedded server's
                // NotLoaded state after unsubscribe.
                let (mut app_server, _requests, proxy) = start_recording_app_server_with_scripts(
                    &app.config,
                    Some(thread_id.to_string()),
                    VecDeque::from([
                        ScriptedThreadRead {
                            include_turns: false,
                            status: ScriptedThreadStatus::NotLoaded,
                        },
                        ScriptedThreadRead {
                            include_turns: false,
                            status: ScriptedThreadStatus::NotLoaded,
                        },
                        ScriptedThreadRead {
                            include_turns: true,
                            status: ScriptedThreadStatus::Active,
                        },
                    ]),
                    /*force_resume_failure*/ false,
                    /*resume_signal*/ None,
                    /*event_recorded_signal*/ None,
                )
                .await?;
                let started = app_server
                    .resume_thread(app.config.clone(), thread_id, app.resume_model_settings())
                    .await?;
                let turns = started.turns.clone();
                app.thread_event_channels.insert(
                    thread_id,
                    ThreadEventChannel::new_with_session(
                        /*capacity*/ 4,
                        started.session,
                        turns,
                    ),
                );
                app.agent_navigation.upsert(
                    thread_id, /*agent_nickname*/ None, /*agent_role*/ None,
                    /*is_closed*/ false, /*created_at*/ None, /*updated_at*/ None,
                );

                app_server.thread_unsubscribe(thread_id).await?;
                assert!(
                    app.refresh_agent_picker_thread_liveness(&mut app_server, thread_id)
                        .await
                );
                assert!(
                    app.agent_navigation
                        .get(&thread_id)
                        .is_some_and(|entry| entry.is_closed)
                );
                assert!(app.is_replay_only_thread(thread_id));

                let mut tui = crate::tui::test_support::make_test_tui()?;
                app.select_agent_thread(&mut tui, &mut app_server, thread_id)
                    .await?;

                // The include-turns read reported Active, so selection must reconcile through
                // resume and reopen the channel before exposing the composer to mutation.
                assert_eq!(
                    app.thread_event_channels
                        .get(&thread_id)
                        .expect("status-race channel")
                        .attachment(),
                    ThreadEventAttachment::Live
                );
                assert!(
                    app.agent_navigation
                        .get(&thread_id)
                        .is_some_and(|entry| !entry.is_closed)
                );
                assert!(!app.is_replay_only_thread(thread_id));

                while app_event_rx.try_recv().is_ok() {}
                app.chat_widget
                    .restore_user_message_to_composer("live status-race op".to_string().into());
                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                assert!(
                    std::iter::from_fn(|| app_event_rx.try_recv().ok())
                        .any(|event| matches!(event, AppEvent::CodexOp(Op::UserTurn { .. })))
                );

                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("closed thread status race test thread")
}

#[test]
fn active_thread_picker_refresh_blocks_replay_input_without_optimistic_prompt() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-active-thread-replay-gate".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
                let codex_home = app.config.codex_home.as_path();
                let thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home,
                        "2026-01-01T00-00-00",
                        "2026-01-01T00:00:00Z",
                        "Saved active-thread message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .expect("create active-thread rollout"),
                )?;
                let (mut app_server, _requests, proxy) =
                    start_recording_app_server_with_closed_transition(&app.config, thread_id)
                        .await?;

                // Load the persisted thread so the first selection creates a real live channel
                // and configures the widget before the later unload/liveness transition.
                app_server
                    .resume_thread(app.config.clone(), thread_id, app.resume_model_settings())
                    .await?;
                app.agent_navigation.upsert(
                    thread_id, /*agent_nickname*/ None, /*agent_role*/ None,
                    /*is_closed*/ false, /*created_at*/ None, /*updated_at*/ None,
                );
                let mut tui = crate::tui::test_support::make_test_tui()?;
                app.select_agent_thread(&mut tui, &mut app_server, thread_id)
                    .await?;
                assert_eq!(app.active_thread_id, Some(thread_id));
                assert_eq!(
                    app.thread_event_channels
                        .get(&thread_id)
                        .expect("live channel")
                        .attachment(),
                    ThreadEventAttachment::Live
                );
                while app_event_rx.try_recv().is_ok() {}

                // The selected thread is unloaded after selection. The picker refresh must
                // synchronize the widget gate before the composer can accept another draft.
                app_server.thread_unsubscribe(thread_id).await?;
                assert!(
                    app.refresh_agent_picker_thread_liveness(&mut app_server, thread_id)
                        .await
                );
                assert!(app.is_replay_only_thread(thread_id));

                // The active-thread branch performs the same refresh when the already-selected
                // row is revisited; keep this check to cover that path as well.
                app.select_agent_thread(&mut tui, &mut app_server, thread_id)
                    .await?;
                assert!(app.is_replay_only_thread(thread_id));

                let prompt = "draft stays while thread is detached".to_string();
                app.chat_widget
                    .restore_user_message_to_composer(prompt.clone().into());
                let draft = app.chat_widget.composer_text_with_pending();
                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                let events =
                    std::iter::from_fn(|| app_event_rx.try_recv().ok()).collect::<Vec<_>>();

                assert_eq!(app.chat_widget.composer_text_with_pending(), draft);
                assert!(
                    !events
                        .iter()
                        .any(|event| matches!(event, AppEvent::CodexOp(Op::UserTurn { .. })))
                );
                assert!(!events.iter().any(|event| match event {
                    AppEvent::InsertHistoryCell(cell) => {
                        lines_to_single_string(&cell.display_lines(/*width*/ 120)).contains(&prompt)
                    }
                    _ => false,
                }));

                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("active thread replay gate test thread")
}

#[test]
fn active_selected_thread_recovers_live_after_closed_refresh() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-active-thread-live-recovery".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
                let codex_home = app.config.codex_home.as_path();
                let thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home,
                        "2026-01-01T00-00-00",
                        "2026-01-01T00:00:00Z",
                        "Saved active recovery message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .expect("create active recovery rollout"),
                )?;
                let (mut app_server, _requests, proxy) =
                    start_recording_app_server_with_closed_transition(&app.config, thread_id)
                        .await?;

                // Select the loaded thread once so the TUI owns a live channel and an active
                // selection before the server-side unload/reload transition.
                app_server
                    .resume_thread(app.config.clone(), thread_id, app.resume_model_settings())
                    .await?;
                app.agent_navigation.upsert(
                    thread_id, /*agent_nickname*/ None, /*agent_role*/ None,
                    /*is_closed*/ false, /*created_at*/ None, /*updated_at*/ None,
                );
                let mut tui = crate::tui::test_support::make_test_tui()?;
                app.select_agent_thread(&mut tui, &mut app_server, thread_id)
                    .await?;
                assert_eq!(app.active_thread_id, Some(thread_id));
                assert_eq!(
                    app.thread_event_channels
                        .get(&thread_id)
                        .expect("initial live channel")
                        .attachment(),
                    ThreadEventAttachment::Live
                );
                while app_event_rx.try_recv().is_ok() {}

                // The selected thread is unloaded. The explicit liveness refresh must fence its
                // existing channel before the active selection can be revisited; the active branch
                // must keep the composer closed until the next liveness read proves that the
                // server has loaded it again.
                app_server.thread_unsubscribe(thread_id).await?;
                assert!(
                    app.refresh_agent_picker_thread_liveness(&mut app_server, thread_id)
                        .await
                );
                assert!(app.is_replay_only_thread(thread_id));
                assert!(
                    app.agent_navigation
                        .get(&thread_id)
                        .is_some_and(|entry| entry.is_closed)
                );

                // Reload the server-side thread without replacing the TUI's replay channel. The
                // next active selection must explicitly resume/listen on that existing channel.
                app_server
                    .resume_thread(app.config.clone(), thread_id, app.resume_model_settings())
                    .await?;
                app.select_agent_thread(&mut tui, &mut app_server, thread_id)
                    .await?;
                assert_eq!(
                    app.thread_event_channels
                        .get(&thread_id)
                        .expect("recovered live channel")
                        .attachment(),
                    ThreadEventAttachment::Live
                );
                assert!(
                    app.agent_navigation
                        .get(&thread_id)
                        .is_some_and(|entry| !entry.is_closed)
                );
                assert!(!app.is_replay_only_thread(thread_id));

                // A recovered active selection must be writable again, with no optimistic replay
                // prompt left behind by the detached interval.
                while app_event_rx.try_recv().is_ok() {}
                app.chat_widget
                    .restore_user_message_to_composer("recovered active op".to_string().into());
                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                assert!(
                    std::iter::from_fn(|| app_event_rx.try_recv().ok())
                        .any(|event| matches!(event, AppEvent::CodexOp(Op::UserTurn { .. })))
                );

                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("active selected thread live recovery test thread")
}

#[tokio::test]
async fn replay_only_model_persistence_does_not_write_config() -> Result<()> {
    let (mut app, _app_event_rx, _op_rx) = make_test_app_with_channels().await;
    let thread_id = ThreadId::new();
    let mut channel = ThreadEventChannel::new(/*capacity*/ 1);
    channel.mark_replay_only();
    app.thread_event_channels.insert(thread_id, channel);
    app.active_thread_id = Some(thread_id);
    app.chat_widget.handle_thread_session(test_thread_session(
        thread_id,
        test_path_buf("/tmp/project"),
    ));
    app.chat_widget.set_replay_only_thread(/*replay_only*/ true);

    let (mut app_server, requests, proxy) = start_recording_app_server(&app.config).await?;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.handle_event(
        &mut tui,
        &mut app_server,
        AppEvent::PersistModelSelection {
            model: "gpt-5.4".to_string(),
            effort: None,
        },
    )
    .await?;

    assert!(
        !requests
            .lock()
            .expect("request recorder lock")
            .iter()
            .any(|method| method == "config/batchWrite")
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}
