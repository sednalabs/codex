use super::super::agent_navigation::AgentNavigationDirection;
use super::super::session_lifecycle::ThreadAttachPresentation;
use super::super::thread_events::ThreadEventAttachment;
use super::*;
use app_test_support::create_fake_parented_rollout_with_source;
use app_test_support::create_fake_rollout;
use app_test_support::rollout_path;
use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SortDirection;
use codex_app_server_protocol::ThreadClosedNotification;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadSortKey;
use codex_app_server_protocol::ThreadSourceKind;
use codex_app_server_protocol::ThreadStatus;
use codex_protocol::AgentPath;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CollabAgentSpawnEndEvent;
use codex_protocol::protocol::CollabWaitingBeginEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::TurnStartedEvent;
use codex_state::SqliteConfig;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
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
enum PickerBackfillTestBehavior {
    PersistedDescendantList,
    LegacyScan,
    ThreadReadAndHideFromThreadList {
        thread_id: String,
    },
    StaleThreadReadStatus {
        thread_id: String,
        status: ThreadStatus,
    },
    PreAcknowledgementAncestorCompatibility {
        denied_thread_read_id: String,
        denied_thread_reads: Arc<Mutex<Vec<String>>>,
        transient_compatibility_relation_failure: Option<Arc<Mutex<bool>>>,
    },
}

impl PickerBackfillTestBehavior {
    fn matches(&self, request: &ClientRequest) -> bool {
        matches!(
            (self, request),
            (
                Self::PersistedDescendantList,
                ClientRequest::ThreadList { params, .. },
            ) if params.use_state_db_only && params.ancestor_thread_id.is_some()
        ) || matches!(
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

    fn strips_loaded_ancestor_thread_filter(&self) -> bool {
        matches!(self, Self::PreAcknowledgementAncestorCompatibility { .. })
    }

    fn strip_loaded_ancestor_thread_filter(&self, request: &mut ClientRequest) {
        if !self.strips_loaded_ancestor_thread_filter() {
            return;
        }
        match request {
            ClientRequest::ThreadLoadedList { params, .. } => params.ancestor_thread_id = None,
            _ => {}
        }
    }

    fn omits_thread_list_ancestor_filter_ack(&self, request: &ClientRequest) -> bool {
        matches!(
            (self, request),
            (
                Self::PreAcknowledgementAncestorCompatibility { .. },
                ClientRequest::ThreadList { params, .. },
            ) if params.ancestor_thread_id.is_some()
        )
    }

    fn fails_compatibility_relation_page_once(&self, request: &ClientRequest) -> bool {
        match (self, request) {
            (
                Self::PreAcknowledgementAncestorCompatibility {
                    transient_compatibility_relation_failure: Some(pending),
                    ..
                },
                ClientRequest::ThreadList { params, .. },
            ) if params.use_state_db_only
                && params.ancestor_thread_id.is_some()
                && params.limit == Some(100) =>
            {
                std::mem::replace(
                    &mut *pending.lock().expect("compatibility relation failure lock"),
                    false,
                )
            }
            _ => false,
        }
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

    fn rejects_thread_read(&self, request: &ClientRequest) -> bool {
        match (self, request) {
            (
                Self::PreAcknowledgementAncestorCompatibility {
                    denied_thread_read_id,
                    denied_thread_reads,
                    ..
                },
                ClientRequest::ThreadRead { params, .. },
            ) if params.thread_id.as_str() == denied_thread_read_id => {
                denied_thread_reads
                    .lock()
                    .expect("denied thread read recorder lock")
                    .push(params.thread_id.clone());
                true
            }
            _ => false,
        }
    }

    fn stale_thread_read_status(&self, request: &ClientRequest) -> Option<ThreadStatus> {
        match (self, request) {
            (
                Self::StaleThreadReadStatus { thread_id, status },
                ClientRequest::ThreadRead { params, .. },
            ) if params.thread_id.as_str() == thread_id => Some(status.clone()),
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
    start_recording_app_server_with_picker_backfill_test_behavior(
        config, /*test_behavior*/ None,
    )
    .await
}

async fn start_recording_app_server_with_picker_backfill_test_behavior(
    config: &Config,
    test_behavior: Option<PickerBackfillTestBehavior>,
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
                    let mut request =
                        serde_json::from_value::<ClientRequest>(serde_json::to_value(request)?)?;
                    let hidden_thread_id = test_behavior
                        .as_ref()
                        .and_then(|failure| failure.hidden_thread_id_for_thread_list(&request))
                        .map(str::to_owned);
                    let rejected_thread_read = test_behavior
                        .as_ref()
                        .is_some_and(|behavior| behavior.rejects_thread_read(&request));
                    let stale_thread_read_status = test_behavior
                        .as_ref()
                        .and_then(|behavior| behavior.stale_thread_read_status(&request));
                    let transient_compatibility_relation_failure =
                        test_behavior.as_ref().is_some_and(|behavior| {
                            behavior.fails_compatibility_relation_page_once(&request)
                        });
                    let omits_thread_list_ancestor_filter_ack =
                        test_behavior.as_ref().is_some_and(|behavior| {
                            behavior.omits_thread_list_ancestor_filter_ack(&request)
                        });
                    let response = if rejected_thread_read {
                        JSONRPCMessage::Error(JSONRPCError {
                            id: request_id,
                            error: JSONRPCErrorError {
                                code: -32000,
                                message: "injected rejected untrusted metadata read".to_string(),
                                data: None,
                            },
                        })
                    } else if transient_compatibility_relation_failure {
                        JSONRPCMessage::Error(JSONRPCError {
                            id: request_id,
                            error: JSONRPCErrorError {
                                code: -32000,
                                message: "injected transient compatibility relation failure"
                                    .to_string(),
                                data: None,
                            },
                        })
                    } else if !transient_failure_injected
                        && test_behavior
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
                        if let Some(behavior) = test_behavior.as_ref() {
                            // The older app-server boundary supports thread/list's long-standing
                            // ancestor filter but ignores the newer thread/loaded/list field.
                            behavior.strip_loaded_ancestor_thread_filter(&mut request);
                        }
                        match embedded.request(request).await? {
                            Ok(result) => {
                                let result = if let Some(status) = stale_thread_read_status {
                                    let mut response =
                                        serde_json::from_value::<ThreadReadResponse>(result)?;
                                    response.thread.status = status;
                                    serde_json::to_value(response)?
                                } else {
                                    result
                                };
                                let result = if let Some(thread_id) = hidden_thread_id {
                                    let mut response =
                                        serde_json::from_value::<ThreadListResponse>(result)?;
                                    response.data.retain(|thread| thread.id != thread_id);
                                    serde_json::to_value(response)?
                                } else {
                                    result
                                };
                                let result = if omits_thread_list_ancestor_filter_ack {
                                    let mut response =
                                        serde_json::from_value::<ThreadListResponse>(result)?;
                                    response.ancestor_filter_applied = false;
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
                JSONRPCMessage::Response(_) => {}
                JSONRPCMessage::Error(_) => request_sink
                    .lock()
                    .expect("request recorder lock")
                    .push("server-request/error".to_string()),
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
async fn primary_reset_rejects_buffered_request_before_a_different_primary_attaches() -> Result<()>
{
    let mut app = make_test_app().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
    let (mut app_server, requests, proxy) = start_recording_app_server(&app.config).await?;
    let startup_thread_a = ThreadId::new();
    let request = request_user_input_request(startup_thread_a, "turn-a", "input-a");

    assert_eq!(
        app.pending_app_server_requests
            .note_thread_server_request(startup_thread_a, &request),
        None
    );
    app.enqueue_primary_thread_request(startup_thread_a, request)
        .await?;
    assert_eq!(app.pending_primary_events.len(), 1);

    app.reset_thread_event_state(Some(&app_server)).await;
    assert!(app.thread_is_discarded(startup_thread_a));
    assert!(app.pending_primary_events.is_empty());
    assert!(
        app.pending_app_server_requests
            .pending_thread_ids()
            .is_empty()
    );

    let thread_b = app_server.start_thread(&app.config).await?;
    let thread_b_id = thread_b.session.thread_id;
    app.enqueue_primary_thread_session_with_presentation_and_server(
        Some(&app_server),
        thread_b.thread_subscription_id,
        thread_b.session,
        thread_b.turns,
        ThreadAttachPresentation::SessionLineage,
    )
    .await?;
    assert_eq!(app.primary_thread_id, Some(thread_b_id));
    assert!(app.chat_widget.pending_thread_approvals().is_empty());

    let recorded = std::mem::take(&mut *requests.lock().expect("request recorder lock"));
    assert_eq!(
        recorded
            .iter()
            .filter(|event| event.as_str() == "server-request/error")
            .count(),
        1,
        "discarding startup thread A must reject its buffered request exactly once: {recorded:?}"
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[tokio::test]
async fn stale_thread_operation_after_reattach_cannot_issue_an_rpc_but_current_generation_can()
-> Result<()> {
    let mut app = make_test_app().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
    let (mut app_server, requests, proxy) = start_recording_app_server(&app.config).await?;
    let started = app_server.start_thread(&app.config).await?;
    let thread_id = started.session.thread_id;
    app.enqueue_primary_thread_session_with_presentation_and_server(
        Some(&app_server),
        started.thread_subscription_id,
        started.session,
        started.turns,
        ThreadAttachPresentation::SessionLineage,
    )
    .await?;
    let stale_generation = app.thread_lifecycle_generation(thread_id);
    app.discard_thread_local_state(&app_server, thread_id).await;
    app.mark_thread_attached(thread_id);
    app.ensure_thread_channel(thread_id).mark_live();
    app.active_thread_id = Some(thread_id);
    app.primary_thread_id = Some(thread_id);
    let live_generation = app.thread_lifecycle_generation(thread_id);
    app.chat_widget
        .set_thread_lifecycle_generation(live_generation);
    let mut tui = crate::tui::test_support::make_test_tui()?;

    let _ = std::mem::take(&mut *requests.lock().expect("request recorder lock"));
    let control = app
        .handle_event(
            &mut tui,
            &mut app_server,
            AppEvent::SubmitThreadOp {
                thread_id,
                lifecycle_generation: stale_generation,
                op: AppCommand::interrupt(),
            },
        )
        .await?;
    assert!(matches!(control, AppRunControl::Continue));
    tokio::task::yield_now().await;
    assert!(
        !requests
            .lock()
            .expect("request recorder lock")
            .iter()
            .any(|method| method == "turn/interrupt"),
        "a stale UI operation must not reach the reattached lifecycle"
    );

    let control = app
        .handle_event(
            &mut tui,
            &mut app_server,
            AppEvent::SubmitThreadOp {
                thread_id,
                lifecycle_generation: live_generation,
                op: AppCommand::interrupt(),
            },
        )
        .await?;
    assert!(matches!(control, AppRunControl::Continue));
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert!(
        requests
            .lock()
            .expect("request recorder lock")
            .iter()
            .any(|method| method == "turn/interrupt"),
        "the current lifecycle generation must retain its normal RPC path"
    );

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[test]
fn switching_away_from_inactive_closed_side_keeps_lifecycle_generations_isolated() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-inactive-closed-side-discard".to_string())
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
                let root = app_server.start_thread(&app.config).await?;
                let root_thread_id = root.session.thread_id;
                app.enqueue_primary_thread_session(root.session, root.turns)
                    .await?;
                let mut tui = crate::tui::test_support::make_test_tui()?;

                // A current live attachment must still accept a branch result from its async
                // lookup. This anchors the stale-result assertion below to lifecycle validity,
                // rather than disabling metadata synchronization entirely.
                let _ = std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                let control = app
                    .handle_event(
                        &mut tui,
                        &mut app_server,
                        AppEvent::SyncThreadGitBranch {
                            thread_id: root_thread_id,
                            lifecycle_generation: app.thread_lifecycle_generation(root_thread_id),
                            branch: "live-branch".to_string(),
                        },
                    )
                    .await?;
                assert!(matches!(control, AppRunControl::Continue));
                assert!(
                    requests
                        .lock()
                        .expect("request recorder lock")
                        .iter()
                        .any(|method| method == "thread/metadata/update"),
                    "a branch result for a live attached thread must reach the app server"
                );
                let unrelated = app_server.start_thread(&app.config).await?;
                let unrelated_thread_id = unrelated.session.thread_id;
                let closed_side_thread_id = ThreadId::new();
                app.side_threads.insert(
                    closed_side_thread_id,
                    SideThreadState::new(root_thread_id),
                );
                assert_eq!(
                    app.ensure_thread_channel(closed_side_thread_id).attachment(),
                    ThreadEventAttachment::Live,
                    "the regression requires an inactive retained live side channel"
                );
                let stale_side_generation =
                    app.thread_lifecycle_generation(closed_side_thread_id);
                app.agent_navigation.upsert(
                    closed_side_thread_id,
                    Some("Closed side".to_string()),
                    Some("side".to_string()),
                    /*is_closed*/ false,
                    /*created_at*/ None,
                    /*updated_at*/ None,
                );
                app.enqueue_thread_notification(
                    closed_side_thread_id,
                    ServerNotification::ThreadClosed(ThreadClosedNotification {
                        thread_id: closed_side_thread_id.to_string(),
                    }),
                )
                .await?;
                assert!(
                    app.agent_navigation
                        .get(&closed_side_thread_id)
                        .is_some_and(|entry| entry.is_closed),
                    "the inactive ThreadClosed notification must record terminal liveness"
                );

                let _ = std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                app.select_agent_thread_and_discard_side(
                    &mut tui,
                    &mut app_server,
                    unrelated_thread_id,
                )
                .await?;
                let switch_requests =
                    std::mem::take(&mut *requests.lock().expect("request recorder lock"));

                assert_eq!(app.active_thread_id, Some(unrelated_thread_id));
                assert!(
                    !switch_requests
                        .iter()
                        .any(|method| matches!(method.as_str(), "turn/interrupt" | "thread/unsubscribe")),
                    "switching away must not interrupt or unsubscribe an already closed inactive side: {switch_requests:?}"
                );
                assert!(!app.side_threads.contains_key(&closed_side_thread_id));
                assert!(!app.thread_event_channels.contains_key(&closed_side_thread_id));
                assert_eq!(app.agent_navigation.get(&closed_side_thread_id), None);

                // An authoritative recovery can reattach the same server thread id. Delayed work
                // from the discarded presentation must still be rejected by its captured
                // generation rather than being accepted merely because this new channel is live.
                app.mark_thread_attached(closed_side_thread_id);
                app.ensure_thread_channel(closed_side_thread_id).mark_live();
                app.active_thread_id = Some(closed_side_thread_id);
                let reattached_generation =
                    app.thread_lifecycle_generation(closed_side_thread_id);
                assert_ne!(stale_side_generation, reattached_generation);

                let _ = std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                let control = app
                    .handle_event(
                        &mut tui,
                        &mut app_server,
                        AppEvent::SyncThreadGitBranch {
                            thread_id: closed_side_thread_id,
                            lifecycle_generation: stale_side_generation,
                            branch: "discarded-side-branch".to_string(),
                        },
                    )
                    .await?;
                assert!(matches!(control, AppRunControl::Continue));
                let delayed_branch_requests =
                    std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                assert!(
                    !delayed_branch_requests
                        .iter()
                        .any(|method| method == "thread/metadata/update"),
                    "a branch result delivered after side discard must not issue metadata RPC: {delayed_branch_requests:?}"
                );

                let control = app
                    .handle_event(
                        &mut tui,
                        &mut app_server,
                        AppEvent::SetThreadGoalStatus {
                            thread_id: closed_side_thread_id,
                            lifecycle_generation: stale_side_generation,
                            status: codex_app_server_protocol::ThreadGoalStatus::Paused,
                        },
                    )
                    .await?;
                assert!(matches!(control, AppRunControl::Continue));
                app.send_thread_settings_update_for_lifecycle(
                    &mut app_server,
                    codex_app_server_protocol::ThreadSettingsUpdateParams {
                        thread_id: closed_side_thread_id.to_string(),
                        model: Some("stale-model".to_string()),
                        ..Default::default()
                    },
                    stale_side_generation,
                )
                .await;
                let stale_goal_and_settings_requests =
                    std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                assert!(
                    stale_goal_and_settings_requests.iter().all(|method| !matches!(
                        method.as_str(),
                        "thread/goal/set" | "thread/settings/update"
                    )),
                    "stale goal and settings work must not write into a reattached lifecycle: {stale_goal_and_settings_requests:?}"
                );

                let control = app
                    .handle_event(
                        &mut tui,
                        &mut app_server,
                        AppEvent::SyncThreadGitBranch {
                            thread_id: closed_side_thread_id,
                            lifecycle_generation: reattached_generation,
                            branch: "reattached-side-branch".to_string(),
                        },
                    )
                    .await?;
                assert!(matches!(control, AppRunControl::Continue));
                assert!(
                    requests
                        .lock()
                        .expect("request recorder lock")
                        .iter()
                        .any(|method| method == "thread/metadata/update"),
                    "a branch result from the current reattached lifecycle must reach the app server"
                );

                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("inactive closed side discard test thread")
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

                // Discovery/hydration remains separately testable from the nonblocking picker
                // open path, which now schedules this bounded work in the background.
                let _backfill = app.backfill_loaded_subagent_threads(&mut app_server).await;

                // This direct hydration pass refreshes the primary thread once. Discovered
                // children were already refreshed by the initial backfill and must not be read
                // a second time.
                assert_eq!(take_backfill_counts(&requests), (1, 1));
                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("session lifecycle request test thread")
}

#[test]
fn select_agent_thread_replays_a_closed_persisted_sidecar() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-closed-sidecar-replay".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let (mut app, mut app_event_rx, _op_rx) = make_test_app_with_channels().await;
                let codex_home = tempdir()?;
                app.config.codex_home = codex_home.path().to_path_buf().abs();
                app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
                let root_thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home.path(),
                        "2026-01-02T00-00-00",
                        "2026-01-02T00:00:00Z",
                        "Saved user message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .expect("create root rollout"),
                )?;
                let child_thread_id = ThreadId::from_string(
                    &create_fake_parented_rollout_with_source(
                        codex_home.path(),
                        "2026-01-02T00-00-01",
                        "2026-01-02T00:00:01Z",
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
                let (mut app_server, requests, proxy) =
                    start_recording_app_server_with_picker_backfill_test_behavior(
                        &app.config,
                        Some(PickerBackfillTestBehavior::StaleThreadReadStatus {
                            thread_id: child_thread_id.to_string(),
                            status: ThreadStatus::Idle,
                        }),
                    )
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

                let backfill = app.backfill_loaded_subagent_threads(&mut app_server).await;
                assert!(backfill.completed);
                assert!(
                    app.agent_navigation
                        .get(&child_thread_id)
                        .is_some_and(|entry| entry.is_closed),
                    "a persisted but not-loaded descendant should be offered as saved history"
                );
                let child = app_server.thread_read(child_thread_id, /*include_turns*/ true).await?;
                assert_eq!(
                    child.can_accept_direct_input,
                    Some(true),
                    "the closed descendant must model an older direct-input-capable thread"
                );

                let mut tui = crate::tui::test_support::make_test_tui()?;
                while app_event_rx.try_recv().is_ok() {}
                let _ = std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                app.select_agent_thread(&mut tui, &mut app_server, child_thread_id)
                    .await?;

                assert!(
                    app.agent_navigation
                        .get(&child_thread_id)
                        .is_some_and(|entry| entry.is_closed),
                    "a stale idle thread/read must not reopen a terminal picker row before selection"
                );

                let selection_requests =
                    std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                assert!(
                    !selection_requests
                        .iter()
                        .any(|method| method == "thread/resume"),
                    "selecting a closed sidecar must not revive it through thread/resume"
                );
                assert_eq!(app.active_thread_id, Some(child_thread_id));
                assert_eq!(
                    app.thread_event_channels
                        .get(&child_thread_id)
                        .map(|channel| channel.attachment()),
                    Some(ThreadEventAttachment::ReplayOnly),
                    "closed sidecars must be represented by a replay-only channel"
                );
                // Model the saved child as the selected side thread as well. Shutdown must
                // discard its replay-local state, rather than sending live-thread interrupts
                // or subscription teardown requests.
                app.side_threads
                    .insert(child_thread_id, SideThreadState::new(root_thread_id));
                let loaded = app_server
                    .thread_loaded_list(ThreadLoadedListParams {
                        cursor: None,
                        limit: None,
                        ancestor_thread_id: Some(root_thread_id.to_string()),
                    })
                    .await?;
                assert_eq!(loaded.data, Vec::<String>::new());

                let mut replayed_history = String::new();
                let mut dispatched_selection_skills_refresh = false;
                while let Ok(event) = app_event_rx.try_recv() {
                    match event {
                        AppEvent::InsertHistoryCell(cell) => {
                            replayed_history.push_str(&lines_to_single_string(
                                &cell.transcript_lines(/*width*/ 100),
                            ));
                        }
                        AppEvent::CodexOp(op @ AppCommand::ListSkills { .. }) => {
                            let control = Box::pin(app.handle_event(
                                &mut tui,
                                &mut app_server,
                                AppEvent::CodexOp(op),
                            ))
                            .await?;
                            assert!(matches!(control, AppRunControl::Continue));
                            dispatched_selection_skills_refresh = true;
                        }
                        _ => {}
                    }
                }
                assert!(
                    replayed_history.contains("Saved child message"),
                    "the closed sidecar transcript should replay from thread/read(includeTurns: true)"
                );
                assert!(
                    dispatched_selection_skills_refresh,
                    "selecting a replay-only transcript must dispatch its queued global skills refresh"
                );
                let selection_refresh_requests =
                    std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                assert!(
                    selection_refresh_requests
                        .iter()
                        .any(|method| method == "skills/list"),
                    "the replay-only selection should allow its global skills refresh"
                );
                assert!(
                    !replayed_history.contains(crate::chatwidget::REPLAY_ONLY_INPUT_MESSAGE),
                    "a replay-only selection must not show a read-only error before user input"
                );

                let state_db = codex_state::StateRuntime::init(
                    app.config.sqlite.clone(),
                    app.config.model_provider_id.clone(),
                )
                .await
                .expect("state db should initialize");
                let child_memory_mode_before = state_db
                    .get_thread_memory_mode(child_thread_id)
                    .await
                    .expect("child thread memory mode should be readable");

                app.config.memories.use_memories = true;
                app.config.memories.generate_memories = false;
                app.chat_widget
                    .set_feature_enabled(Feature::MemoryTool, /*enabled*/ true);
                app.chat_widget
                    .set_memory_settings(/*use_memories*/ true, /*generate_memories*/ false);
                let _ = std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                app.chat_widget.apply_external_edit("/memories".to_string());
                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

                let memory_settings_event = app_event_rx
                    .try_recv()
                    .expect("/memories confirmation should update the settings");
                assert!(matches!(
                    memory_settings_event,
                    AppEvent::UpdateMemorySettings {
                        use_memories: true,
                        generate_memories: true,
                    }
                ));
                let control = Box::pin(app.handle_event(
                    &mut tui,
                    &mut app_server,
                    memory_settings_event,
                ))
                .await?;
                assert!(matches!(control, AppRunControl::Continue));
                assert!(app.config.memories.generate_memories);
                assert!(app.chat_widget.config_ref().memories.generate_memories);

                let child_memory_mode_after = state_db
                    .get_thread_memory_mode(child_thread_id)
                    .await
                    .expect("child thread memory mode should remain readable");
                assert_eq!(child_memory_mode_after, child_memory_mode_before);
                let memory_settings_requests =
                    std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                assert!(
                    memory_settings_requests
                        .iter()
                        .any(|method| method == "config/batchWrite"),
                    "/memories must still persist the global setting: {memory_settings_requests:?}"
                );
                assert!(
                    !memory_settings_requests
                        .iter()
                        .any(|method| method == "thread/memoryMode/set"),
                    "/memories must not mutate replay-only thread metadata: {memory_settings_requests:?}"
                );

                let _ = std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                for text in ["normal replay input", "!echo replay-only"] {
                    app.chat_widget.apply_external_edit(text.to_string());
                    app.chat_widget
                        .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                    assert_eq!(
                        app.chat_widget.composer_text_with_pending(),
                        text,
                        "replay-only input must remain available for the user to copy or edit"
                    );
                }
                app.submit_active_thread_op(
                    &mut app_server,
                    AppCommand::run_user_shell_command("echo replay-only-dispatch".to_string()),
                )
                .await?;
                app.submit_active_thread_op(
                    &mut app_server,
                    AppCommand::override_turn_context(
                        /*cwd*/ None,
                        /*approval_policy*/ None,
                        /*approvals_reviewer*/ None,
                        /*permission_profile*/ None,
                        /*active_permission_profile*/ None,
                        /*windows_sandbox_level*/ None,
                        Some("replay-only-settings".to_string()),
                        /*effort*/ None,
                        /*summary*/ None,
                        /*service_tier*/ None,
                        /*collaboration_mode*/ None,
                        /*personality*/ None,
                    ),
                )
                .await?;

                let input_events = std::iter::from_fn(|| app_event_rx.try_recv().ok())
                    .collect::<Vec<_>>();
                assert!(
                    input_events.iter().all(|event| !matches!(
                        event,
                        AppEvent::CodexOp(_) | AppEvent::SubmitThreadOp { .. }
                    )),
                    "replay-only input must not emit a Codex operation"
                );
                let input_feedback = input_events
                    .into_iter()
                    .filter_map(|event| match event {
                        AppEvent::InsertHistoryCell(cell) => Some(cell.transcript_lines(/*width*/ 100)),
                        _ => None,
                    })
                    .flatten()
                    .map(|line| line.to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    input_feedback.contains(crate::chatwidget::REPLAY_ONLY_INPUT_MESSAGE),
                    "replay-only input must explain that the saved transcript is read-only"
                );
                assert!(
                    !input_feedback.contains("controlled by its parent"),
                    "replay-only input must not use the parent-owned wording"
                );

                let blocked_operations =
                    std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                assert!(
                    blocked_operations.iter().all(|method| !matches!(
                        method.as_str(),
                        "turn/start" | "turn/steer" | "thread/settings/update" | "thread/shellCommand"
                    )),
                    "replay-only input must not emit turn, settings, or shell-command requests: {blocked_operations:?}"
                );

                // Goal slash actions dispatch straight to their app-server helpers instead of
                // flowing through `submit_thread_op`. Exercise every write-shaped event after
                // the channel becomes replay-only, along with the delayed directive that syncs
                // git metadata, so neither path can write the saved transcript.
                let _ = std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                for event in [
                    AppEvent::SetThreadGoalObjective {
                        thread_id: child_thread_id,
                        lifecycle_generation: app.thread_lifecycle_generation(child_thread_id),
                        objective: "blocked objective".to_string(),
                        mode: crate::app_event::ThreadGoalSetMode::ConfirmIfExists,
                    },
                    AppEvent::SetThreadGoalDraft {
                        thread_id: child_thread_id,
                        lifecycle_generation: app.thread_lifecycle_generation(child_thread_id),
                        draft: crate::goal_files::GoalDraft {
                            objective: "blocked draft".to_string(),
                            ..Default::default()
                        },
                        mode: crate::app_event::ThreadGoalSetMode::ReplaceExisting,
                    },
                    AppEvent::SetThreadGoalStatus {
                        thread_id: child_thread_id,
                        lifecycle_generation: app.thread_lifecycle_generation(child_thread_id),
                        status: codex_app_server_protocol::ThreadGoalStatus::Paused,
                    },
                    AppEvent::ClearThreadGoal {
                        thread_id: child_thread_id,
                        lifecycle_generation: app.thread_lifecycle_generation(child_thread_id),
                    },
                    AppEvent::SyncThreadGitBranch {
                        thread_id: child_thread_id,
                        lifecycle_generation: app.thread_lifecycle_generation(child_thread_id),
                        branch: "late-replay-only-branch".to_string(),
                    },
                ] {
                    let control = Box::pin(app.handle_event(&mut tui, &mut app_server, event))
                        .await?;
                    assert!(matches!(control, AppRunControl::Continue));
                }
                let replay_only_write_requests =
                    std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                assert!(
                    replay_only_write_requests.iter().all(|method| !matches!(
                        method.as_str(),
                        "thread/goal/set" | "thread/goal/clear" | "thread/metadata/update"
                    )),
                    "replay-only goal and delayed metadata writes must be rejected before RPC dispatch: {replay_only_write_requests:?}"
                );

                // These direct event handlers bypass normal operation submission as well. The
                // replay-only boundary must reject destructive archive/delete calls and every
                // settings-picker thread write while leaving their local/global UI changes alone.
                let _ = std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                for event in [
                    AppEvent::ArchiveCurrentThread,
                    AppEvent::DeleteCurrentThread,
                    AppEvent::UpdateModel("gpt-5.4".to_string()),
                    AppEvent::UpdateReasoningEffort(Some(
                        codex_protocol::openai_models::ReasoningEffort::High,
                    )),
                    AppEvent::UpdatePersonality(codex_protocol::config_types::Personality::Pragmatic),
                ] {
                    let control = Box::pin(app.handle_event(&mut tui, &mut app_server, event))
                        .await?;
                    assert!(matches!(control, AppRunControl::Continue));
                }
                let replay_only_direct_write_requests =
                    std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                assert!(
                    replay_only_direct_write_requests.iter().all(|method| !matches!(
                        method.as_str(),
                        "thread/archive" | "thread/delete" | "thread/settings/update"
                    )),
                    "replay-only destructive and settings writes must be rejected before RPC dispatch: {replay_only_direct_write_requests:?}"
                );
                assert_eq!(app.chat_widget.current_model(), "gpt-5.4");
                assert_eq!(
                    app.chat_widget.current_reasoning_effort(),
                    Some(codex_protocol::openai_models::ReasoningEffort::High)
                );
                assert_eq!(
                    app.chat_widget.config_ref().personality,
                    Some(codex_protocol::config_types::Personality::Pragmatic),
                    "rejecting a replay-only thread write must not undo the current global selection"
                );

                // `/fork` dispatches directly to `app_server.fork_thread`, so it needs the same
                // replay-only boundary as other write-shaped events. It must surface the
                // read-only guidance before any fork RPC is sent.
                let _ = std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                while app_event_rx.try_recv().is_ok() {}
                let control = Box::pin(app.handle_event(
                    &mut tui,
                    &mut app_server,
                    AppEvent::ForkCurrentSession,
                ))
                .await?;
                assert!(matches!(control, AppRunControl::Continue));
                let replay_only_fork_requests =
                    std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                assert!(
                    !replay_only_fork_requests
                        .iter()
                        .any(|method| method == "thread/fork"),
                    "replay-only /fork must not reach the app server: {replay_only_fork_requests:?}"
                );
                let replay_only_fork_feedback =
                    std::iter::from_fn(|| app_event_rx.try_recv().ok())
                    .filter_map(|event| match event {
                        AppEvent::InsertHistoryCell(cell) => {
                            Some(cell.transcript_lines(/*width*/ 100))
                        }
                        _ => None,
                    })
                    .flatten()
                    .map(|line| line.to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    replay_only_fork_feedback.contains(crate::chatwidget::REPLAY_ONLY_INPUT_MESSAGE),
                    "replay-only /fork must explain that the saved transcript is read-only"
                );

                // Prompt editing branches by reading the transcript and then either forking it
                // or starting a new thread. `/side` similarly forks directly, including when it
                // carries an inline prompt, and safety-buffered retry interrupts then forks.
                // All three must reject the saved transcript before any app-server request and
                // retain the inline text locally for the user.
                let _ = std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                let prompt = crate::chatwidget::UserMessage::from("replay-only prompt edit");
                let control = Box::pin(app.handle_event(
                    &mut tui,
                    &mut app_server,
                    AppEvent::ForkSessionForPromptEdit {
                        thread_id: child_thread_id,
                        lifecycle_generation: app.thread_lifecycle_generation(child_thread_id),
                        nth_user_message: 0,
                        prompt: prompt.clone(),
                    },
                ))
                .await?;
                assert!(matches!(control, AppRunControl::Continue));
                assert_eq!(app.chat_widget.composer_text_with_pending(), prompt.text);
                let primary_thread_id = app.primary_thread_id;
                app.primary_thread_id = Some(child_thread_id);
                let control = Box::pin(app.handle_event(
                    &mut tui,
                    &mut app_server,
                    AppEvent::RetrySafetyBufferedTurn {
                        thread_id: child_thread_id,
                        lifecycle_generation: app.thread_lifecycle_generation(child_thread_id),
                        turn_id: "replay-only-turn".to_string(),
                        model: "gpt-5.4".to_string(),
                        turn: AppCommand::run_user_shell_command(
                            "echo replay-only retry".to_string(),
                        ),
                        prompt: crate::chatwidget::UserMessage::from("replay-only retry"),
                    },
                ))
                .await?;
                assert!(matches!(control, AppRunControl::Continue));
                app.primary_thread_id = primary_thread_id;
                for user_message in [
                    None,
                    Some(crate::chatwidget::UserMessage::from("replay-only /side prompt")),
                ] {
                    let control = Box::pin(app.handle_event(
                        &mut tui,
                        &mut app_server,
                        AppEvent::StartSide {
                            parent_thread_id: child_thread_id,
                            lifecycle_generation: app.thread_lifecycle_generation(child_thread_id),
                            user_message,
                        },
                    ))
                    .await?;
                    assert!(matches!(control, AppRunControl::Continue));
                }
                let replay_only_branch_requests =
                    std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                assert!(
                    replay_only_branch_requests.is_empty(),
                    "replay-only prompt editing, retry, and bare or inline /side must not reach the app server: {replay_only_branch_requests:?}"
                );

                // Switching away routes through `select_agent_thread_and_discard_side`. A
                // selected saved sidecar has no live operation or app-server subscription to
                // tear down, so this direct cleanup path must remain entirely local too.
                let _ = std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                Box::pin(app.select_agent_thread_and_discard_side(
                    &mut tui,
                    &mut app_server,
                    root_thread_id,
                ))
                .await?;
                let switch_requests =
                    std::mem::take(&mut *requests.lock().expect("request recorder lock"));
                assert!(
                    !switch_requests
                        .iter()
                        .any(|method| matches!(method.as_str(), "thread/unsubscribe" | "turn/interrupt")),
                    "switching away from a replay-only side selection must not interrupt or unsubscribe it: {switch_requests:?}"
                );
                assert!(
                    app.active_thread_id == Some(root_thread_id),
                    "switching away from the saved sidecar should activate its live parent"
                );
                assert!(!app.side_threads.contains_key(&child_thread_id));
                assert!(!app.thread_event_channels.contains_key(&child_thread_id));
                assert_eq!(app.agent_navigation.get(&child_thread_id), None);
                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("closed sidecar replay test thread")
}

#[test]
fn agent_picker_backfill_combines_indexed_and_legacy_descendants() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-mixed-agent-picker-backfill".to_string())
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
                let root_thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home.path(),
                        "2026-01-04T00-00-00",
                        "2026-01-04T00:00:00Z",
                        "Saved root message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .expect("create root rollout"),
                )?;
                let legacy_child_thread_id = ThreadId::from_string(
                    &create_fake_parented_rollout_with_source(
                        codex_home.path(),
                        "2026-01-04T00-00-01",
                        "2026-01-04T00:00:01Z",
                        "Saved legacy child message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                        RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id: root_thread_id,
                            depth: 1,
                            agent_path: Some(
                                AgentPath::try_from("/root/legacy")
                                    .expect("valid legacy agent path"),
                            ),
                            agent_nickname: Some("legacy".to_string()),
                            agent_role: Some("worker".to_string()),
                        }),
                        root_thread_id.into(),
                        root_thread_id,
                    )
                    .expect("create legacy child rollout"),
                )?;
                let other_indexed_child_thread_id = ThreadId::from_string(
                    &create_fake_parented_rollout_with_source(
                        codex_home.path(),
                        "2026-01-04T00-00-02",
                        "2026-01-04T00:00:02Z",
                        "Saved other indexed child message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                        RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id: root_thread_id,
                            depth: 1,
                            agent_path: Some(
                                AgentPath::try_from("/root/indexed-two")
                                    .expect("valid second indexed agent path"),
                            ),
                            agent_nickname: Some("indexed-two".to_string()),
                            agent_role: Some("worker".to_string()),
                        }),
                        root_thread_id.into(),
                        root_thread_id,
                    )
                    .expect("create second indexed child rollout"),
                )?;
                let indexed_child_thread_id = ThreadId::from_string(
                    &create_fake_parented_rollout_with_source(
                        codex_home.path(),
                        "2026-01-04T00-00-03",
                        "2026-01-04T00:00:03Z",
                        "Saved indexed child message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                        RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id: root_thread_id,
                            depth: 1,
                            agent_path: Some(
                                AgentPath::try_from("/root/indexed")
                                    .expect("valid indexed agent path"),
                            ),
                            agent_nickname: Some("indexed".to_string()),
                            agent_role: Some("worker".to_string()),
                        }),
                        root_thread_id.into(),
                        root_thread_id,
                    )
                    .expect("create indexed child rollout"),
                )?;

                let (mut app_server, _requests, proxy) =
                    start_recording_app_server(&app.config).await?;
                // The first one-row page overfetches its bounded scan to repair the two newest
                // children before opening the root. The older child remains unindexed, producing
                // a deliberately mixed state database.
                let repair_page = app_server
                    .thread_list(ThreadListParams {
                        cursor: None,
                        limit: Some(1),
                        sort_key: Some(ThreadSortKey::UpdatedAt),
                        sort_direction: Some(SortDirection::Desc),
                        model_providers: None,
                        source_kinds: Some(vec![ThreadSourceKind::SubAgentThreadSpawn]),
                        thread_sources: None,
                        archived: Some(false),
                        is_pinned: None,
                        cwd: None,
                        use_state_db_only: false,
                        search_term: None,
                        parent_thread_id: None,
                        ancestor_thread_id: None,
                    })
                    .await?;
                assert_eq!(repair_page.data.len(), 1);
                assert_eq!(repair_page.data[0].id, indexed_child_thread_id.to_string());

                let root = app_server
                    .resume_thread(
                        app.config.clone(),
                        root_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                app.enqueue_primary_thread_session(root.session, root.turns)
                    .await?;

                let backfill = app.backfill_loaded_subagent_threads(&mut app_server).await;
                assert!(backfill.completed);
                assert!(
                    app.agent_navigation.get(&indexed_child_thread_id).is_some(),
                    "the relation-indexed descendant should remain visible"
                );
                assert!(
                    app.agent_navigation
                        .get(&other_indexed_child_thread_id)
                        .is_some(),
                    "the other relation-indexed descendant should remain visible"
                );
                assert!(
                    app.agent_navigation.get(&legacy_child_thread_id).is_some(),
                    "the bounded legacy repair must supplement a non-empty indexed page"
                );
                assert_eq!(
                    app.agent_navigation
                        .ordered_path_backed_subagent_threads(Some(root_thread_id))
                        .len(),
                    3,
                    "mixed backfill should deduplicate indexed children while adding the legacy child"
                );
                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("mixed agent picker backfill test thread")
}

#[test]
fn agent_picker_retries_legacy_fallback_after_transient_scan_failure() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-agent-picker-legacy-fallback-retry".to_string())
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
                let root_thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home.path(),
                        "2026-01-05T00-00-00",
                        "2026-01-05T00:00:00Z",
                        "Saved user message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .expect("create root rollout"),
                )?;
                let legacy_child_thread_id = ThreadId::from_string(
                    &create_fake_parented_rollout_with_source(
                        codex_home.path(),
                        "2026-01-05T00-00-01",
                        "2026-01-05T00:00:01Z",
                        "Saved legacy child message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                        RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id: root_thread_id,
                            depth: 1,
                            agent_path: Some(
                                AgentPath::try_from("/root/legacy-retry")
                                    .expect("valid legacy agent path"),
                            ),
                            agent_nickname: Some("legacy-retry".to_string()),
                            agent_role: Some("worker".to_string()),
                        }),
                        root_thread_id.into(),
                        root_thread_id,
                    )
                    .expect("create legacy child rollout"),
                )?;

                let (mut app_server, _requests, proxy) =
                    start_recording_app_server_with_picker_backfill_test_behavior(
                        &app.config,
                        Some(PickerBackfillTestBehavior::LegacyScan),
                    )
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

                let first_backfill = app.backfill_loaded_subagent_threads(&mut app_server).await;
                assert!(
                    !first_backfill.completed,
                    "a failed legacy scan must leave the bounded fallback retryable"
                );
                assert!(app.agent_navigation.needs_legacy_relation_fallback_check());
                assert!(
                    app.agent_navigation.get(&legacy_child_thread_id).is_none(),
                    "the injected failed scan must not pretend the legacy child was discovered"
                );

                let retry_backfill = app.backfill_loaded_subagent_threads(&mut app_server).await;
                assert!(retry_backfill.completed);
                assert!(!app.agent_navigation.needs_legacy_relation_fallback_check());
                assert!(
                    app.agent_navigation.get(&legacy_child_thread_id).is_some(),
                    "a later successful bounded pass must expose the legacy descendant"
                );

                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("legacy fallback retry test thread")
}

#[test]
fn agent_picker_retries_legacy_fallback_after_transient_loaded_metadata_failure() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-agent-picker-legacy-metadata-retry".to_string())
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
                let root_thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home.path(),
                        "2026-01-06T00-00-00",
                        "2026-01-06T00:00:00Z",
                        "Saved user message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .expect("create root rollout"),
                )?;
                let legacy_child_thread_id = ThreadId::from_string(
                    &create_fake_parented_rollout_with_source(
                        codex_home.path(),
                        "2026-01-06T00-00-01",
                        "2026-01-06T00:00:01Z",
                        "Saved legacy child message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                        RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id: root_thread_id,
                            depth: 1,
                            agent_path: Some(
                                AgentPath::try_from("/root/legacy-metadata-retry")
                                    .expect("valid legacy agent path"),
                            ),
                            agent_nickname: Some("legacy-metadata-retry".to_string()),
                            agent_role: Some("worker".to_string()),
                        }),
                        root_thread_id.into(),
                        root_thread_id,
                    )
                    .expect("create legacy child rollout"),
                )?;

                // The proxy models a pre-index legacy child that is visible through the loaded
                // process list but absent from the persisted relation/scan result. The first
                // metadata read fails; the next bounded refresh must retry it and discover child.
                let (mut app_server, _requests, proxy) =
                    start_recording_app_server_with_picker_backfill_test_behavior(
                        &app.config,
                        Some(
                            PickerBackfillTestBehavior::ThreadReadAndHideFromThreadList {
                                thread_id: legacy_child_thread_id.to_string(),
                            },
                        ),
                    )
                    .await?;
                let root = app_server
                    .resume_thread(
                        app.config.clone(),
                        root_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                let _legacy_child = app_server
                    .resume_thread(
                        app.config.clone(),
                        legacy_child_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                app.enqueue_primary_thread_session(root.session, root.turns)
                    .await?;

                let _backfill = app.backfill_loaded_subagent_threads(&mut app_server).await;
                assert!(
                    app.agent_navigation.needs_legacy_relation_fallback_check(),
                    "a failed loaded-thread metadata read must leave the fallback retryable"
                );
                assert!(
                    app.agent_navigation.get(&legacy_child_thread_id).is_none(),
                    "the hidden scan result must not mask the failed metadata lookup"
                );

                let _backfill = app.backfill_loaded_subagent_threads(&mut app_server).await;
                assert!(!app.agent_navigation.needs_legacy_relation_fallback_check());
                assert!(
                    app.agent_navigation.get(&legacy_child_thread_id).is_some(),
                    "the next bounded refresh must retry and discover the loaded legacy child"
                );

                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("legacy metadata fallback retry test thread")
}

#[test]
fn agent_picker_keeps_loaded_descendants_when_persisted_list_fails() -> Result<()> {
    const LOADED_DESCENDANT_COUNT: usize = 2;
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-agent-picker-persisted-list-fallback".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let mut app = make_test_app().await;
                // The root plus two descendants is a supported V2 concurrency configuration.
                app.config.multi_agent_v2.max_concurrent_threads_per_session =
                    LOADED_DESCENDANT_COUNT + 1;
                let codex_home = tempdir()?;
                app.config.codex_home = codex_home.path().to_path_buf().abs();
                app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
                let root_thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home.path(),
                        "2026-01-08T00-00-00",
                        "2026-01-08T00:00:00Z",
                        "Saved user message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .expect("create root rollout"),
                )?;
                let mut loaded_child_thread_ids = Vec::with_capacity(LOADED_DESCENDANT_COUNT);
                for index in 0..LOADED_DESCENDANT_COUNT {
                    let timestamp = format!("2026-01-08T00-00-0{}", index + 1);
                    let created_at = format!("2026-01-08T00:00:0{}Z", index + 1);
                    let child_thread_id = ThreadId::from_string(
                        &create_fake_parented_rollout_with_source(
                            codex_home.path(),
                            &timestamp,
                            &created_at,
                            &format!("Saved loaded child message {index}"),
                            Some(app.config.model_provider_id.as_str()),
                            /*git_info*/ None,
                            RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                                parent_thread_id: root_thread_id,
                                depth: 1,
                                agent_path: Some(
                                    AgentPath::try_from(format!("/root/fallback-worker-{index}"))
                                        .expect("valid loaded agent path"),
                                ),
                                agent_nickname: Some(format!("fallback-worker-{index}")),
                                agent_role: Some("worker".to_string()),
                            }),
                            root_thread_id.into(),
                            root_thread_id,
                        )
                        .expect("create loaded child rollout"),
                    )?;
                    loaded_child_thread_ids.push(child_thread_id);
                }

                let (mut app_server, requests, proxy) =
                    start_recording_app_server_with_picker_backfill_test_behavior(
                        &app.config,
                        Some(PickerBackfillTestBehavior::PersistedDescendantList),
                    )
                    .await?;
                let root = app_server
                    .resume_thread(
                        app.config.clone(),
                        root_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                for child_thread_id in loaded_child_thread_ids.iter().copied() {
                    let _loaded_child = app_server
                        .resume_thread(
                            app.config.clone(),
                            child_thread_id,
                            app.resume_model_settings(),
                        )
                        .await?;
                }
                app.enqueue_primary_thread_session(root.session, root.turns)
                    .await?;

                let backfill = app.backfill_loaded_subagent_threads(&mut app_server).await;
                let expected_loaded_thread_ids = loaded_child_thread_ids
                    .iter()
                    .copied()
                    .collect::<std::collections::HashSet<_>>();
                assert!(
                    !backfill.completed,
                    "the failed persisted relation page must remain retryable"
                );
                assert_eq!(backfill.refreshed_thread_ids, expected_loaded_thread_ids);
                assert!(
                    app.agent_navigation.needs_legacy_relation_fallback_check(),
                    "a persisted-page failure must not mark the legacy fallback complete"
                );

                let visible_loaded_thread_ids = app
                    .agent_navigation
                    .ordered_path_backed_subagent_threads(Some(root_thread_id))
                    .into_iter()
                    .filter_map(|(thread_id, entry)| (!entry.is_closed).then_some(thread_id))
                    .collect::<std::collections::HashSet<_>>();
                assert_eq!(
                    visible_loaded_thread_ids, expected_loaded_thread_ids,
                    "a healthy loaded-descendant lookup must survive a persisted relation failure"
                );
                Box::pin(app.render_agent_picker()).await;
                let rendered = super::render_bottom_popup(&app.chat_widget, /*width*/ 100);
                assert!(rendered.contains("fallback-worker-0"));
                assert!(rendered.contains("fallback-worker-1"));

                let mut current_thread_id = root_thread_id;
                let mut keyboard_reachable_loaded_thread_ids =
                    std::collections::HashSet::with_capacity(LOADED_DESCENDANT_COUNT);
                for _ in 0..LOADED_DESCENDANT_COUNT {
                    app.active_thread_id = Some(current_thread_id);
                    let next_thread_id = app
                        .adjacent_thread_id_with_backfill(
                            &mut app_server,
                            AgentNavigationDirection::Next,
                        )
                        .await
                        .expect("loaded child should be reachable by keyboard navigation");
                    assert!(expected_loaded_thread_ids.contains(&next_thread_id));
                    assert!(keyboard_reachable_loaded_thread_ids.insert(next_thread_id));
                    current_thread_id = next_thread_id;
                }
                assert_eq!(
                    keyboard_reachable_loaded_thread_ids, expected_loaded_thread_ids,
                    "keyboard navigation must cover every loaded descendant after the failure"
                );
                {
                    let requests = requests.lock().expect("request recorder lock");
                    assert!(requests.iter().any(|method| method == "thread/loaded/list"));
                    assert!(requests.iter().any(|method| method == "thread/list"));
                }

                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("persisted list fallback test thread")
}

#[test]
fn agent_picker_keeps_the_forward_page_after_reopen() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;
    const DESCENDANT_COUNT: usize = 2 * 50 + 1;

    std::thread::Builder::new()
        .name("tui-agent-picker-page-continuation".to_string())
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
                let root_thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home.path(),
                        "2026-01-03T00-00-00",
                        "2026-01-03T00:00:00Z",
                        "Saved user message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .expect("create root rollout"),
                )?;
                let mut child_thread_ids = Vec::with_capacity(DESCENDANT_COUNT);
                for index in 0..DESCENDANT_COUNT {
                    let seconds_from_start = index + 1;
                    let minute = seconds_from_start / 60;
                    let second = seconds_from_start % 60;
                    let timestamp = format!("2026-01-03T00-{minute:02}-{second:02}");
                    let created_at = format!("2026-01-03T00:{minute:02}:{second:02}Z");
                    let child_thread_id = ThreadId::from_string(
                        &create_fake_parented_rollout_with_source(
                            codex_home.path(),
                            &timestamp,
                            &created_at,
                            &format!("Saved child message {index}"),
                            Some(app.config.model_provider_id.as_str()),
                            /*git_info*/ None,
                            RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                                parent_thread_id: root_thread_id,
                                depth: 1,
                                agent_path: Some(
                                    AgentPath::try_from(format!("/root/worker-{index}"))
                                        .expect("valid agent path"),
                                ),
                                agent_nickname: Some(format!("worker-{index}")),
                                agent_role: Some("worker".to_string()),
                            }),
                            root_thread_id.into(),
                            root_thread_id,
                        )
                        .expect("create child rollout"),
                    )?;
                    child_thread_ids.push(child_thread_id);
                }

                let (mut app_server, _requests, proxy) =
                    start_recording_app_server(&app.config).await?;
                // Prime the state database as a modern rollout would have done while the agents
                // were spawned. The picker assertions below then exercise relation pagination,
                // rather than the separate one-page legacy repair fallback.
                let mut repair_cursor = None;
                loop {
                    let repair_page = app_server
                        .thread_list(codex_app_server_protocol::ThreadListParams {
                            cursor: repair_cursor,
                            limit: Some(50),
                            sort_key: Some(codex_app_server_protocol::ThreadSortKey::UpdatedAt),
                            sort_direction: Some(codex_app_server_protocol::SortDirection::Desc),
                            model_providers: None,
                            source_kinds: Some(vec![
                                codex_app_server_protocol::ThreadSourceKind::SubAgentThreadSpawn,
                            ]),
                            thread_sources: None,
                            archived: Some(false),
                            is_pinned: None,
                            cwd: None,
                            use_state_db_only: false,
                            search_term: None,
                            parent_thread_id: None,
                            ancestor_thread_id: None,
                        })
                        .await?;
                    repair_cursor = repair_page.next_cursor;
                    if repair_cursor.is_none() {
                        break;
                    }
                }
                let root = app_server
                    .resume_thread(
                        app.config.clone(),
                        root_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                app.enqueue_primary_thread_session(root.session, root.turns)
                    .await?;

                let _backfill = app.backfill_loaded_subagent_threads(&mut app_server).await;
                Box::pin(app.render_agent_picker()).await;
                for character in "closed".chars() {
                    app.chat_widget.handle_key_event(KeyEvent::new(
                        KeyCode::Char(character),
                        KeyModifiers::NONE,
                    ));
                }
                Box::pin(app.load_more_agent_picker_page(&mut app_server)).await;
                assert_eq!(
                    app.agent_navigation
                        .ordered_path_backed_subagent_threads(Some(root_thread_id))
                        .len(),
                    100
                );
                let third_page_cursor = app
                    .agent_navigation
                    .next_picker_page_cursor()
                    .expect("the third page should remain available after page two");
                assert_eq!(
                    app.chat_widget.selection_view_search_query("agent-picker"),
                    Some("closed".to_string())
                );

                app.chat_widget
                    .handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                assert_eq!(
                    app.chat_widget.selection_view_search_query("agent-picker"),
                    None
                );

                let _backfill = app.backfill_loaded_subagent_threads(&mut app_server).await;
                Box::pin(app.render_agent_picker()).await;
                assert_eq!(
                    app.agent_navigation.next_picker_page_cursor(),
                    Some(third_page_cursor),
                    "a first-page refresh must not reset pagination to page two"
                );
                for character in "closed".chars() {
                    app.chat_widget.handle_key_event(KeyEvent::new(
                        KeyCode::Char(character),
                        KeyModifiers::NONE,
                    ));
                }
                Box::pin(app.load_more_agent_picker_page(&mut app_server)).await;

                assert_eq!(
                    app.agent_navigation
                        .ordered_path_backed_subagent_threads(Some(root_thread_id))
                        .len(),
                    DESCENDANT_COUNT
                );
                assert!(
                    app.agent_navigation
                        .get(child_thread_ids.last().expect("last child"))
                        .is_some(),
                    "the continuation after reopen must advance to the previously unseen page"
                );
                assert_eq!(app.agent_navigation.next_picker_page_cursor(), None);
                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("agent picker continuation test thread")
}

#[test]
fn agent_picker_prioritizes_loaded_descendant_over_closed_history() -> Result<()> {
    // This crosses the 100-item loaded-list page boundary: the picker must consume its bounded
    // continuation page before it falls back to persisted closed history.
    const LOADED_DESCENDANT_COUNT: usize = 102;
    const CLOSED_DESCENDANT_COUNT: usize = 50;
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-agent-picker-loaded-priority".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let mut app = make_test_app().await;
                // The root plus 102 descendants is a supported V2 concurrency configuration.
                app.config.multi_agent_v2.max_concurrent_threads_per_session =
                    LOADED_DESCENDANT_COUNT + 1;
                let codex_home = tempdir()?;
                app.config.codex_home = codex_home.path().to_path_buf().abs();
                app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
                let root_thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home.path(),
                        "2026-01-07T00-00-00",
                        "2026-01-07T00:00:00Z",
                        "Saved user message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .expect("create root rollout"),
                )?;
                let mut loaded_child_thread_ids = Vec::with_capacity(LOADED_DESCENDANT_COUNT);
                for index in 0..LOADED_DESCENDANT_COUNT {
                    let seconds_from_start = index + 1;
                    let minute = seconds_from_start / 60;
                    let second = seconds_from_start % 60;
                    let timestamp = format!("2026-01-07T00-{minute:02}-{second:02}");
                    let created_at = format!("2026-01-07T00:{minute:02}:{second:02}Z");
                    let child_thread_id = ThreadId::from_string(
                        &create_fake_parented_rollout_with_source(
                        codex_home.path(),
                        &timestamp,
                        &created_at,
                        &format!("Saved loaded child message {index}"),
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                        RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id: root_thread_id,
                            depth: 1,
                            agent_path: Some(
                                AgentPath::try_from(format!("/root/loaded-worker-{index}"))
                                    .expect("valid loaded agent path"),
                            ),
                            agent_nickname: Some(format!("loaded-worker-{index}")),
                            agent_role: Some("worker".to_string()),
                        }),
                        root_thread_id.into(),
                        root_thread_id,
                    )
                    .expect("create loaded child rollout"),
                    )?;
                    loaded_child_thread_ids.push(child_thread_id);
                }
                for index in 0..CLOSED_DESCENDANT_COUNT {
                    let seconds_from_start = LOADED_DESCENDANT_COUNT + index + 1;
                    let minute = seconds_from_start / 60;
                    let second = seconds_from_start % 60;
                    let timestamp = format!("2026-01-07T00-{minute:02}-{second:02}");
                    let created_at = format!("2026-01-07T00:{minute:02}:{second:02}Z");
                    create_fake_parented_rollout_with_source(
                        codex_home.path(),
                        &timestamp,
                        &created_at,
                        &format!("Saved closed child message {index}"),
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                        RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id: root_thread_id,
                            depth: 1,
                            agent_path: Some(
                                AgentPath::try_from(format!("/root/closed-worker-{index}"))
                                    .expect("valid closed agent path"),
                            ),
                            agent_nickname: Some(format!("closed-worker-{index}")),
                            agent_role: Some("worker".to_string()),
                        }),
                        root_thread_id.into(),
                        root_thread_id,
                    )
                    .expect("create closed child rollout");
                }

                let (mut app_server, _requests, proxy) =
                    start_recording_app_server(&app.config).await?;
                // Populate the persisted relationship index exactly as a modern session would.
                // The first persisted page then contains the 50 newer closed descendants, while
                // the 102 older loaded children must all come from the priority path.
                let mut repair_cursor = None;
                loop {
                    let repair_page = app_server
                        .thread_list(ThreadListParams {
                            cursor: repair_cursor,
                            limit: Some(50),
                            sort_key: Some(ThreadSortKey::UpdatedAt),
                            sort_direction: Some(SortDirection::Desc),
                            model_providers: None,
                            source_kinds: Some(vec![ThreadSourceKind::SubAgentThreadSpawn]),
                            thread_sources: None,
                            archived: Some(false),
                            is_pinned: None,
                            cwd: None,
                            use_state_db_only: false,
                            search_term: None,
                            parent_thread_id: None,
                            ancestor_thread_id: None,
                        })
                        .await?;
                    repair_cursor = repair_page.next_cursor;
                    if repair_cursor.is_none() {
                        break;
                    }
                }
                let root = app_server
                    .resume_thread(
                        app.config.clone(),
                        root_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                for child_thread_id in loaded_child_thread_ids.iter().copied() {
                    let _loaded_child = app_server
                        .resume_thread(
                        app.config.clone(),
                        child_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                }
                app.enqueue_primary_thread_session(root.session, root.turns)
                    .await?;

                let _backfill = app.backfill_loaded_subagent_threads(&mut app_server).await;
                Box::pin(app.render_agent_picker()).await;

                let expected_loaded_thread_ids = loaded_child_thread_ids
                    .iter()
                    .copied()
                    .collect::<std::collections::HashSet<_>>();
                let visible_loaded_thread_ids = app
                    .agent_navigation
                    .ordered_path_backed_subagent_threads(Some(root_thread_id))
                    .into_iter()
                    .filter_map(|(thread_id, entry)| (!entry.is_closed).then_some(thread_id))
                    .collect::<std::collections::HashSet<_>>();
                assert_eq!(
                    visible_loaded_thread_ids, expected_loaded_thread_ids,
                    "every loaded descendant must be visible before historical fallback"
                );

                let rendered = super::render_bottom_popup(&app.chat_widget, /*width*/ 100);
                assert!(
                    !rendered.contains("closed-worker-49"),
                    "closed history must remain hidden from the default picker view"
                );
                let mut current_thread_id = root_thread_id;
                let mut keyboard_reachable_loaded_thread_ids =
                    std::collections::HashSet::with_capacity(LOADED_DESCENDANT_COUNT);
                for _ in 0..LOADED_DESCENDANT_COUNT {
                    app.active_thread_id = Some(current_thread_id);
                    let next_thread_id = app
                        .adjacent_thread_id_with_backfill(
                            &mut app_server,
                            AgentNavigationDirection::Next,
                        )
                        .await
                        .expect("every loaded child should be reachable by keyboard navigation");
                    assert!(
                        expected_loaded_thread_ids.contains(&next_thread_id),
                        "closed history must not appear before every loaded descendant"
                    );
                    assert!(
                        keyboard_reachable_loaded_thread_ids.insert(next_thread_id),
                        "keyboard navigation must not repeat a loaded descendant before covering the set"
                    );
                    current_thread_id = next_thread_id;
                }
                assert_eq!(
                    keyboard_reachable_loaded_thread_ids, expected_loaded_thread_ids,
                    "keyboard navigation must cover every loaded descendant before closed history"
                );
                app.active_thread_id = Some(current_thread_id);
                let first_closed_thread_id = app
                    .adjacent_thread_id_with_backfill(
                        &mut app_server,
                        AgentNavigationDirection::Next,
                    )
                    .await
                    .expect("the first persisted closed descendant should follow loaded children");
                assert!(
                    app.agent_navigation
                        .get(&first_closed_thread_id)
                        .is_some_and(|entry| entry.is_closed),
                    "closed history should follow, not precede, the loaded descendant priority set"
                );

                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("loaded descendant priority test thread")
}

#[test]
fn agent_picker_keeps_errored_activity_failed_across_stale_loaded_descendant_backfill() -> Result<()>
{
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-agent-picker-stale-loaded-status".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                for (index, (status_name, stale_status)) in [
                    ("idle", ThreadStatus::Idle),
                    (
                        "active",
                        ThreadStatus::Active {
                            active_flags: Vec::new(),
                        },
                    ),
                ]
                .into_iter()
                .enumerate()
                {
                    let mut app = make_test_app().await;
                    let codex_home = tempdir()?;
                    app.config.codex_home = codex_home.path().to_path_buf().abs();
                    app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
                    let timestamp = format!("2026-01-10T00-00-{index:02}");
                    let created_at = format!("2026-01-10T00:00:{index:02}Z");
                    let root_thread_id = ThreadId::from_string(
                        &create_fake_rollout(
                            codex_home.path(),
                            &timestamp,
                            &created_at,
                            "Saved root message",
                            Some(app.config.model_provider_id.as_str()),
                            /*git_info*/ None,
                        )
                        .expect("create root rollout"),
                    )?;
                    let agent_path = format!("/root/stale-status-{status_name}");
                    let child_thread_id = ThreadId::from_string(
                        &create_fake_parented_rollout_with_source(
                            codex_home.path(),
                            &format!("2026-01-10T00-00-{:02}", index + 10),
                            &format!("2026-01-10T00:00:{:02}Z", index + 10),
                            "Saved child message",
                            Some(app.config.model_provider_id.as_str()),
                            /*git_info*/ None,
                            RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                                parent_thread_id: root_thread_id,
                                depth: 1,
                                agent_path: Some(
                                    AgentPath::try_from(agent_path.as_str())
                                        .expect("valid child agent path"),
                                ),
                                agent_nickname: Some(format!("stale-status-{status_name}")),
                                agent_role: Some("worker".to_string()),
                            }),
                            root_thread_id.into(),
                            root_thread_id,
                        )
                        .expect("create child rollout"),
                    )?;

                    let (mut app_server, _requests, proxy) =
                        start_recording_app_server_with_picker_backfill_test_behavior(
                            &app.config,
                            Some(PickerBackfillTestBehavior::StaleThreadReadStatus {
                                thread_id: child_thread_id.to_string(),
                                status: stale_status,
                            }),
                        )
                        .await?;
                    // Populate the durable relationship index, then load the child so the
                    // ancestor-filtered priority query must refresh it through `thread/read`.
                    let _ = app_server
                        .thread_list(ThreadListParams {
                            cursor: None,
                            limit: Some(50),
                            sort_key: Some(ThreadSortKey::UpdatedAt),
                            sort_direction: Some(SortDirection::Desc),
                            model_providers: None,
                            source_kinds: Some(vec![ThreadSourceKind::SubAgentThreadSpawn]),
                            thread_sources: None,
                            archived: Some(false),
                            is_pinned: None,
                            cwd: None,
                            use_state_db_only: false,
                            search_term: None,
                            parent_thread_id: None,
                            ancestor_thread_id: None,
                        })
                        .await?;
                    let root = app_server
                        .resume_thread(
                            app.config.clone(),
                            root_thread_id,
                            app.resume_model_settings(),
                        )
                        .await?;
                    let _child = app_server
                        .resume_thread(
                            app.config.clone(),
                            child_thread_id,
                            app.resume_model_settings(),
                        )
                        .await?;
                    app.enqueue_primary_thread_session(root.session, root.turns)
                        .await?;
                    app.agent_navigation
                        .record_sub_agent_activity(SubAgentActivityDisplay {
                            activity_id: "activity-child".to_string(),
                            thread_id: child_thread_id,
                            agent_path: agent_path.clone(),
                            model: None,
                            reasoning_effort: None,
                            has_system_error: true,
                            is_running_hint: false,
                        });

                    let backfill = app.backfill_loaded_subagent_threads(&mut app_server).await;
                    assert!(backfill.completed);
                    assert!(backfill.refreshed_thread_ids.contains(&child_thread_id));
                    assert!(
                        app.agent_navigation
                            .get(&child_thread_id)
                            .is_some_and(|entry| {
                                entry.has_system_error && !entry.is_running && !entry.is_closed
                            }),
                        "a stale {status_name} metadata read must not recover an errored activity"
                    );

                    Box::pin(app.render_agent_picker()).await;
                    let rendered = super::render_bottom_popup(&app.chat_widget, /*width*/ 100);
                    assert!(rendered.contains(&agent_path));
                    assert!(rendered.contains("system error failed inspect saved transcript"));

                    app_server.shutdown().await?;
                    proxy.await??;
                }
                Ok(())
            })
        })?
        .join()
        .expect("stale loaded descendant backfill test thread")
}

#[test]
fn agent_picker_prioritizes_loaded_nested_descendant_through_unloaded_parent() -> Result<()> {
    const CLOSED_DESCENDANT_COUNT: usize = 50;
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-agent-picker-loaded-nested-priority".to_string())
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
                let root_thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home.path(),
                        "2026-01-09T00-00-00",
                        "2026-01-09T00:00:00Z",
                        "Saved user message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .expect("create root rollout"),
                )?;
                let unloaded_child_thread_id = ThreadId::from_string(
                    &create_fake_parented_rollout_with_source(
                        codex_home.path(),
                        "2026-01-09T00-00-01",
                        "2026-01-09T00:00:01Z",
                        "Saved unloaded child message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                        RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id: root_thread_id,
                            depth: 1,
                            agent_path: Some(
                                AgentPath::try_from("/root/unloaded-intermediary")
                                    .expect("valid unloaded child agent path"),
                            ),
                            agent_nickname: Some("unloaded-intermediary".to_string()),
                            agent_role: Some("worker".to_string()),
                        }),
                        root_thread_id.into(),
                        root_thread_id,
                    )
                    .expect("create unloaded child rollout"),
                )?;
                let loaded_grandchild_thread_id = ThreadId::from_string(
                    &create_fake_parented_rollout_with_source(
                        codex_home.path(),
                        "2026-01-09T00-00-02",
                        "2026-01-09T00:00:02Z",
                        "Saved loaded grandchild message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                        RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id: unloaded_child_thread_id,
                            depth: 2,
                            agent_path: Some(
                                AgentPath::try_from("/root/loaded-nested-worker")
                                    .expect("valid loaded grandchild agent path"),
                            ),
                            agent_nickname: Some("loaded-nested-worker".to_string()),
                            agent_role: Some("worker".to_string()),
                        }),
                        root_thread_id.into(),
                        unloaded_child_thread_id,
                    )
                    .expect("create loaded grandchild rollout"),
                )?;
                for index in 0..CLOSED_DESCENDANT_COUNT {
                    let seconds_from_start = index + 3;
                    let timestamp = format!("2026-01-09T00-00-{seconds_from_start:02}");
                    let created_at = format!("2026-01-09T00:00:{seconds_from_start:02}Z");
                    create_fake_parented_rollout_with_source(
                        codex_home.path(),
                        &timestamp,
                        &created_at,
                        &format!("Saved closed child message {index}"),
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                        RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id: root_thread_id,
                            depth: 1,
                            agent_path: Some(
                                AgentPath::try_from(format!("/root/nested-closed-worker-{index}"))
                                    .expect("valid closed agent path"),
                            ),
                            agent_nickname: Some(format!("nested-closed-worker-{index}")),
                            agent_role: Some("worker".to_string()),
                        }),
                        root_thread_id.into(),
                        root_thread_id,
                    )
                    .expect("create closed child rollout");
                }

                let (mut app_server, _requests, proxy) =
                    start_recording_app_server(&app.config).await?;
                // Populate the durable root -> child -> grandchild chain. The first persisted
                // page is instead full of the 50 newer closed children, so the loaded grandchild
                // must arrive through the independent ancestor-filtered loaded-priority query.
                let mut repair_cursor = None;
                loop {
                    let repair_page = app_server
                        .thread_list(ThreadListParams {
                            cursor: repair_cursor,
                            limit: Some(50),
                            sort_key: Some(ThreadSortKey::UpdatedAt),
                            sort_direction: Some(SortDirection::Desc),
                            model_providers: None,
                            source_kinds: Some(vec![ThreadSourceKind::SubAgentThreadSpawn]),
                            thread_sources: None,
                            archived: Some(false),
                            is_pinned: None,
                            cwd: None,
                            use_state_db_only: false,
                            search_term: None,
                            parent_thread_id: None,
                            ancestor_thread_id: None,
                        })
                        .await?;
                    repair_cursor = repair_page.next_cursor;
                    if repair_cursor.is_none() {
                        break;
                    }
                }
                let root = app_server
                    .resume_thread(
                        app.config.clone(),
                        root_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                let _loaded_grandchild = app_server
                    .resume_thread(
                        app.config.clone(),
                        loaded_grandchild_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                app.enqueue_primary_thread_session(root.session, root.turns)
                    .await?;

                let _backfill = app.backfill_loaded_subagent_threads(&mut app_server).await;
                Box::pin(app.render_agent_picker()).await;

                assert!(
                    app.agent_navigation.next_picker_page_cursor().is_some(),
                    "the first persisted page must remain full of newer closed history"
                );
                assert!(
                    app.agent_navigation.get(&unloaded_child_thread_id).is_none(),
                    "the unloaded intermediary must not be registered as a loaded descendant"
                );
                assert!(
                    app.agent_navigation
                        .get(&loaded_grandchild_thread_id)
                        .is_some_and(|entry| !entry.is_closed),
                    "the ancestor-filtered loaded grandchild must not be dropped by local relation reconstruction"
                );
                let visible_loaded_thread_ids = app
                    .agent_navigation
                    .ordered_path_backed_subagent_threads(Some(root_thread_id))
                    .into_iter()
                    .filter_map(|(thread_id, entry)| (!entry.is_closed).then_some(thread_id))
                    .collect::<std::collections::HashSet<_>>();
                assert_eq!(
                    visible_loaded_thread_ids,
                    [loaded_grandchild_thread_id]
                        .into_iter()
                        .collect::<std::collections::HashSet<_>>(),
                    "the loaded nested descendant must be visible before historical pagination"
                );
                Box::pin(app.render_agent_picker()).await;
                let rendered = super::render_bottom_popup(&app.chat_widget, /*width*/ 100);
                assert!(rendered.contains("loaded-nested-worker"));
                assert!(
                    !rendered.contains("nested-closed-worker-49"),
                    "closed history must remain hidden from the default picker view"
                );
                app.active_thread_id = Some(root_thread_id);
                let next_thread_id = app
                    .adjacent_thread_id_with_backfill(
                        &mut app_server,
                        AgentNavigationDirection::Next,
                    )
                    .await
                    .expect("the loaded grandchild should be reachable before closed history");
                assert_eq!(next_thread_id, loaded_grandchild_thread_id);

                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("loaded nested descendant priority test thread")
}

#[test]
fn agent_picker_locally_filters_unacknowledged_ancestor_responses() -> Result<()> {
    const CLOSED_DESCENDANT_COUNT: usize = 50;
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-agent-picker-unacknowledged-ancestor-filter".to_string())
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
                let root_thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home.path(),
                        "2026-01-10T00-00-00",
                        "2026-01-10T00:00:00Z",
                        "Saved root message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .expect("create root rollout"),
                )?;
                let expected_child_thread_id = ThreadId::from_string(
                    &create_fake_parented_rollout_with_source(
                        codex_home.path(),
                        "2026-01-10T00-00-01",
                        "2026-01-10T00:00:01Z",
                        "Saved expected child message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                        RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id: root_thread_id,
                            depth: 1,
                            agent_path: Some(
                                AgentPath::try_from("/root/expected-child")
                                    .expect("valid expected child agent path"),
                            ),
                            agent_nickname: Some("expected-child".to_string()),
                            agent_role: Some("worker".to_string()),
                        }),
                        root_thread_id.into(),
                        root_thread_id,
                    )
                    .expect("create expected child rollout"),
                )?;
                // These newer closed children fill the normal 50-row picker relation page. The
                // still-loaded expected child must arrive from the bounded old-server
                // compatibility window instead of an untrusted global loaded-id sweep.
                for index in 0..CLOSED_DESCENDANT_COUNT {
                    let seconds_from_start = index + 2;
                    let timestamp = format!("2026-01-10T00-00-{seconds_from_start:02}");
                    let created_at = format!("2026-01-10T00:00:{seconds_from_start:02}Z");
                    create_fake_parented_rollout_with_source(
                        codex_home.path(),
                        &timestamp,
                        &created_at,
                        &format!("Saved newer closed child message {index}"),
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                        RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id: root_thread_id,
                            depth: 1,
                            agent_path: Some(
                                AgentPath::try_from(format!("/root/closed-compat-worker-{index}"))
                                    .expect("valid closed compatibility agent path"),
                            ),
                            agent_nickname: Some(format!("closed-compat-worker-{index}")),
                            agent_role: Some("worker".to_string()),
                        }),
                        root_thread_id.into(),
                        root_thread_id,
                    )
                    .expect("create newer closed child rollout");
                }
                let unrelated_root_thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home.path(),
                        "2026-01-10T00-01-00",
                        "2026-01-10T00:01:00Z",
                        "Saved unrelated root message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .expect("create unrelated root rollout"),
                )?;
                let unrelated_child_thread_id = ThreadId::from_string(
                    &create_fake_parented_rollout_with_source(
                        codex_home.path(),
                        "2026-01-10T00-01-01",
                        "2026-01-10T00:01:01Z",
                        "Saved unrelated child message",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                        RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                            parent_thread_id: unrelated_root_thread_id,
                            depth: 1,
                            agent_path: Some(
                                AgentPath::try_from("/root/unrelated-child")
                                    .expect("valid unrelated child agent path"),
                            ),
                            agent_nickname: Some("unrelated-child".to_string()),
                            agent_role: Some("worker".to_string()),
                        }),
                        unrelated_root_thread_id.into(),
                        unrelated_root_thread_id,
                    )
                    .expect("create unrelated child rollout"),
                )?;

                let denied_thread_reads = Arc::new(Mutex::new(Vec::new()));
                let (mut app_server, requests, proxy) =
                    start_recording_app_server_with_picker_backfill_test_behavior(
                        &app.config,
                        Some(PickerBackfillTestBehavior::PreAcknowledgementAncestorCompatibility {
                            denied_thread_read_id: unrelated_child_thread_id.to_string(),
                            denied_thread_reads: Arc::clone(&denied_thread_reads),
                            transient_compatibility_relation_failure: Some(Arc::new(Mutex::new(
                                true,
                            ))),
                        }),
                    )
                    .await?;
                // Populate the state-db relation index before the proxy models the compatibility
                // boundary: thread/loaded/list ignores the newer ancestor field, while the older
                // thread/list ancestor filter still applies but omits its new acknowledgement.
                let mut repair_cursor = None;
                loop {
                    let repair_page = app_server
                        .thread_list(ThreadListParams {
                            cursor: repair_cursor,
                            limit: Some(50),
                            sort_key: Some(ThreadSortKey::UpdatedAt),
                            sort_direction: Some(SortDirection::Desc),
                            model_providers: None,
                            source_kinds: Some(vec![ThreadSourceKind::SubAgentThreadSpawn]),
                            thread_sources: None,
                            archived: Some(false),
                            is_pinned: None,
                            cwd: None,
                            use_state_db_only: false,
                            search_term: None,
                            parent_thread_id: None,
                            ancestor_thread_id: None,
                        })
                        .await?;
                    repair_cursor = repair_page.next_cursor;
                    if repair_cursor.is_none() {
                        break;
                    }
                }
                let root = app_server
                    .resume_thread(
                        app.config.clone(),
                        root_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                let mut _loaded_threads = Vec::new();
                for thread_id in [
                    expected_child_thread_id,
                    unrelated_root_thread_id,
                    unrelated_child_thread_id,
                ] {
                    let loaded_thread = app_server
                        .resume_thread(app.config.clone(), thread_id, app.resume_model_settings())
                        .await?;
                    _loaded_threads.push(loaded_thread);
                }
                app.enqueue_primary_thread_session(root.session, root.turns)
                    .await?;

                app.active_thread_id = Some(root_thread_id);
                let first_navigation_attempt = app
                    .adjacent_thread_id_with_backfill(
                        &mut app_server,
                        AgentNavigationDirection::Next,
                    )
                    .await;
                assert_eq!(
                    first_navigation_attempt, None,
                    "a failed bounded compatibility page must not treat the ordinary 50-row page as complete"
                );
                assert!(
                    app.agent_navigation.get(&expected_child_thread_id).is_none(),
                    "the failed compatibility page must not expose a partial relation result"
                );
                let retried_thread_id = app
                    .adjacent_thread_id_with_backfill(
                        &mut app_server,
                        AgentNavigationDirection::Next,
                    )
                    .await
                    .expect("the next keyboard attempt must retry the bounded compatibility page");
                assert_eq!(retried_thread_id, expected_child_thread_id);
                let _backfill = app.backfill_loaded_subagent_threads(&mut app_server).await;
                Box::pin(app.render_agent_picker()).await;

                assert!(
                    app.agent_navigation
                        .get(&expected_child_thread_id)
                        .is_some_and(|entry| !entry.is_closed),
                    "the direct child must remain available after local relation verification"
                );
                for unrelated_thread_id in [unrelated_root_thread_id, unrelated_child_thread_id] {
                    assert!(
                        app.agent_navigation.get(&unrelated_thread_id).is_none(),
                        "unfiltered older-server data must not leak unrelated threads into /agent"
                    );
                }
                let visible_loaded_thread_ids = app
                    .agent_navigation
                    .ordered_path_backed_subagent_threads(Some(root_thread_id))
                    .into_iter()
                    .filter_map(|(thread_id, entry)| (!entry.is_closed).then_some(thread_id))
                    .collect::<std::collections::HashSet<_>>();
                assert_eq!(
                    visible_loaded_thread_ids,
                    [expected_child_thread_id]
                        .into_iter()
                        .collect::<std::collections::HashSet<_>>(),
                    "only the primary thread's relationship-proven child may be visible"
                );
                Box::pin(app.render_agent_picker()).await;
                let rendered = super::render_bottom_popup(&app.chat_widget, /*width*/ 100);
                assert!(rendered.contains("expected-child"));
                assert!(!rendered.contains("unrelated-child"));
                app.active_thread_id = Some(root_thread_id);
                let next_thread_id = app
                    .adjacent_thread_id_with_backfill(
                        &mut app_server,
                        AgentNavigationDirection::Next,
                    )
                    .await
                    .expect("the relationship-proven child should be keyboard-reachable");
                assert_eq!(next_thread_id, expected_child_thread_id);
                let recorded_requests = requests.lock().expect("request recorder lock").clone();
                assert!(
                    recorded_requests
                        .iter()
                        .filter(|method| method.as_str() == "thread/loaded/list")
                        .count()
                        >= 3,
                    "the repeated compatibility responses must be exercised"
                );
                assert!(
                    !recorded_requests
                        .iter()
                        .any(|method| method == "thread/read"),
                    "an unacknowledged global loaded list must not trigger metadata reads"
                );
                assert!(
                    denied_thread_reads
                        .lock()
                        .expect("denied thread read recorder lock")
                        .is_empty(),
                    "the proxy would reject the unrelated loaded metadata read if it were attempted"
                );

                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("unacknowledged ancestor filter test thread")
}
