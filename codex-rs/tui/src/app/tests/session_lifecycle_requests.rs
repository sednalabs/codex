use super::*;
use super::super::agent_navigation::AgentNavigationDirection;
use app_test_support::create_fake_parented_rollout_with_source;
use app_test_support::create_fake_rollout;
use app_test_support::rollout_path;
use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadSortKey;
use codex_app_server_protocol::ThreadSourceKind;
use codex_protocol::AgentPath;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CollabAgentSpawnEndEvent;
use codex_protocol::protocol::CollabWaitingBeginEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::TurnStartedEvent;
use codex_state::SqliteConfig;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use std::sync::Mutex;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

/// Returns and resets `(thread/list, thread/read)` request counts.
fn take_backfill_counts(requests: &Arc<Mutex<Vec<String>>>) -> (usize, usize) {
    let requests = std::mem::take(&mut *requests.lock().expect("request recorder lock"));
    (
        requests
            .iter()
            .filter(|method| *method == "thread/list")
            .count(),
        requests
            .iter()
            .filter(|method| *method == "thread/read")
            .count(),
    )
}

fn append_rollout_record(
    path: &std::path::Path,
    timestamp: &str,
    item_type: &str,
    payload: serde_json::Value,
) -> Result<()> {
    let record = serde_json::json!({
        "timestamp": timestamp,
        "type": item_type,
        "payload": payload,
    });
    std::fs::write(
        path,
        format!("{}{}\n", std::fs::read_to_string(path)?, record),
    )?;
    Ok(())
}

#[derive(Clone)]
enum TransientPickerBackfillFailure {
    LegacyScan,
    ThreadReadAndHideFromThreadList { thread_id: String },
}

impl TransientPickerBackfillFailure {
    fn matches(&self, request: &ClientRequest) -> bool {
        matches!(
            (self, request),
            (
                Self::LegacyScan,
                ClientRequest::ThreadList { params, .. },
            ) if !params.use_state_db_only
                && params.source_kinds.as_deref()
                    == Some(&[ThreadSourceKind::SubAgentThreadSpawn])
        ) || matches!(
            (self, request),
            (
                Self::ThreadReadAndHideFromThreadList { thread_id },
                ClientRequest::ThreadRead { params, .. },
            ) if params.thread_id.as_str() == thread_id
        )
    }

    fn hidden_thread_id_for_thread_list(&self, request: &ClientRequest) -> Option<&str> {
        match (self, request) {
            (
                Self::ThreadReadAndHideFromThreadList { thread_id },
                ClientRequest::ThreadList { .. },
            ) => Some(thread_id),
            _ => None,
        }
    }
}

/// Starts an embedded app server behind a loopback WebSocket proxy that records JSON-RPC methods.
async fn start_recording_app_server(
    config: &Config,
) -> Result<(
    AppServerSession,
    Arc<Mutex<Vec<String>>>,
    JoinHandle<Result<()>>,
)> {
    start_recording_app_server_with_transient_picker_backfill_failure(
        config,
        /*transient_failure*/ None,
    )
    .await
}

async fn start_recording_app_server_with_transient_picker_backfill_failure(
    config: &Config,
    transient_failure: Option<TransientPickerBackfillFailure>,
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
        let mut transient_failure_injected = false;
        let (stream, _) = listener.accept().await?;
        let mut websocket = accept_async(stream).await?;
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
                    let request =
                        serde_json::from_value::<ClientRequest>(serde_json::to_value(request)?)?;
                    let hidden_thread_id = transient_failure
                        .as_ref()
                        .and_then(|failure| failure.hidden_thread_id_for_thread_list(&request))
                        .map(str::to_owned);
                    let response = if !transient_failure_injected
                        && transient_failure
                            .as_ref()
                            .is_some_and(|failure| failure.matches(&request))
                    {
                        transient_failure_injected = true;
                        JSONRPCMessage::Error(JSONRPCError {
                            id: request_id,
                            error: JSONRPCErrorError {
                                code: -32000,
                                message: "injected transient picker backfill failure".to_string(),
                                data: None,
                            },
                        })
                    } else {
                        match embedded.request(request).await? {
                            Ok(result) => {
                                let result = if let Some(thread_id) = hidden_thread_id {
                                    let mut response =
                                        serde_json::from_value::<ThreadListResponse>(result)?;
                                    response.data.retain(|thread| thread.id != thread_id);
                                    serde_json::to_value(response)?
                                } else {
                                    result
                                };
                                JSONRPCMessage::Response(JSONRPCResponse {
                                    id: request_id,
                                    result,
                                })
                            }
                            Err(error) => JSONRPCMessage::Error(JSONRPCError {
                                id: request_id,
                                error,
                            }),
                        }
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
                let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
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
                let child_rollout_path = rollout_path(
                    codex_home.path(),
                    "2026-01-01T00-00-01",
                    &child_thread_id.to_string(),
                );
                append_rollout_record(
                    &child_rollout_path,
                    "2026-01-01T00:00:01Z",
                    "turn_context",
                    serde_json::to_value(TurnContextItem {
                        turn_id: None,
                        cwd: app.config.cwd.clone(),
                        workspace_roots: None,
                        current_date: None,
                        timezone: None,
                        approval_policy: app.config.permissions.approval_policy.value(),
                        approvals_reviewer: None,
                        sandbox_policy: app.config.legacy_sandbox_policy(),
                        permission_profile: Some(
                            app.config.permissions.permission_profile().clone(),
                        ),
                        network: None,
                        file_system_sandbox_policy: None,
                        model: "gpt-5.4".to_string(),
                        comp_hash: None,
                        personality: None,
                        collaboration_mode: None,
                        multi_agent_version: None,
                        multi_agent_mode: None,
                        realtime_active: None,
                        effort: Some(ReasoningEffortConfig::High),
                        summary: Default::default(),
                    })?,
                )?;
                append_rollout_record(
                    &root_rollout_path,
                    "2026-01-01T00:00:02Z",
                    "event_msg",
                    serde_json::to_value(EventMsg::TurnStarted(TurnStartedEvent {
                        turn_id: "historical-agent-turn".to_string(),
                        trace_id: None,
                        started_at: None,
                        model_context_window: None,
                        collaboration_mode_kind: Default::default(),
                    }))?,
                )?;
                append_rollout_record(
                    &root_rollout_path,
                    "2026-01-01T00:00:03Z",
                    "event_msg",
                    serde_json::to_value(EventMsg::CollabAgentSpawnEnd(
                        CollabAgentSpawnEndEvent {
                            call_id: "spawn-worker".to_string(),
                            completed_at_ms: 3,
                            sender_thread_id: root_thread_id,
                            new_thread_id: Some(child_thread_id),
                            new_agent_nickname: None,
                            new_agent_role: None,
                            prompt: "Explore the historical metadata".to_string(),
                            model: "gpt-5".to_string(),
                            reasoning_effort: ReasoningEffortConfig::Medium,
                            status: AgentStatus::Completed(None),
                        },
                    ))?,
                )?;
                append_rollout_record(
                    &root_rollout_path,
                    "2026-01-01T00:00:04Z",
                    "event_msg",
                    serde_json::to_value(EventMsg::CollabWaitingBegin(
                        CollabWaitingBeginEvent {
                            started_at_ms: 4,
                            sender_thread_id: root_thread_id,
                            receiver_thread_ids: vec![child_thread_id],
                            receiver_agents: Vec::new(),
                            call_id: "wait-worker".to_string(),
                        },
                    ))?,
                )?;
                append_rollout_record(
                    &root_rollout_path,
                    "2026-01-01T00:00:05Z",
                    "event_msg",
                    serde_json::to_value(EventMsg::TurnComplete(TurnCompleteEvent {
                        turn_id: "historical-agent-turn".to_string(),
                        started_at: None,
                        last_agent_message: None,
                        compaction_events_in_turn: 0,
                        final_model: None,
                        model_snapshot: None,
                        provider_usage: None,
                        error: None,
                        completed_at: None,
                        duration_ms: None,
                        time_to_first_token_ms: None,
                    }))?,
                )?;
                let (mut app_server, requests, proxy) =
                    start_recording_app_server(&app.config).await?;
                let root = app_server
                    .resume_thread(
                        app.config.clone(),
                        root_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                app.enqueue_primary_thread_session(root.session, root.turns)
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

                take_backfill_counts(&requests);
                while app_event_rx.try_recv().is_ok() {}
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
                // An indexed relation uses one list request. This fixture deliberately starts
                // from an unindexed historical rollout, so the bounded compatibility fallback
                // adds one more `thread/list` plus one root metadata read. It does not fan out
                // into a read for every persisted sidecar.
                assert_eq!(take_backfill_counts(&requests), (2, 1));
                let child_entry = app
                    .agent_navigation
                    .get(&child_thread_id)
                    .expect("relation-filtered list should discover the child");
                assert_eq!(child_entry.agent_nickname.as_deref(), Some("worker"));
                assert_eq!(child_entry.agent_role.as_deref(), Some("worker"));
                assert_eq!(child_entry.agent_path.as_deref(), Some("/root/worker"));
                assert!(!child_entry.is_running);
                assert!(child_entry.is_closed);
                assert!(child_entry.created_at.is_some());
                assert!(child_entry.updated_at.is_some());

                let mut saw_named_spawn = false;
                let mut saw_named_wait = false;
                let mut saw_effective_identity = false;
                while let Ok(event) = app_event_rx.try_recv() {
                    if let AppEvent::InsertHistoryCell(cell) = event {
                        let transcript =
                            lines_to_single_string(&cell.transcript_lines(/*width*/ 100));
                        saw_named_spawn |= transcript.contains("Spawned")
                            && transcript.contains("worker [worker] · /root/worker");
                        saw_named_wait |= transcript.contains("Waiting")
                            && transcript.contains("worker [worker] · /root/worker");
                        saw_effective_identity |= transcript.contains("effective: gpt-5.4 high");
                    }
                }
                assert!(
                    saw_named_spawn,
                    "resuming from an empty navigation cache must render the historical Spawn with the backfilled friendly identity"
                );
                assert!(
                    saw_named_wait,
                    "resuming from an empty navigation cache must render the historical Wait with the backfilled friendly identity"
                );
                assert!(
                    saw_effective_identity,
                    "resuming from an empty navigation cache must render the backfilled effective identity"
                );

                Box::pin(app.open_agent_picker(&mut app_server)).await;

                // The picker refreshes the primary thread once. Discovered children were already
                // refreshed by the picker's initial backfill and must not be read a second time.
                assert_eq!(take_backfill_counts(&requests), (1, 1));
                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("session lifecycle request test thread")
}
