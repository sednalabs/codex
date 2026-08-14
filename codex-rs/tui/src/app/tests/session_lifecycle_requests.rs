use super::super::session_lifecycle::AGENT_PICKER_CURSOR_BUDGET;
use super::super::session_lifecycle::AGENT_PICKER_PAGE_SIZE;
use super::super::session_lifecycle::LEGACY_AGENT_PICKER_MAX_PAGES;
use super::super::session_lifecycle::LEGACY_AGENT_PICKER_MAX_THREADS;
use super::*;
use app_test_support::create_fake_parented_rollout_with_source;
use app_test_support::create_fake_rollout;
use app_test_support::rollout_path;
use codex_app_server_protocol::ClientNotification;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCResponse;
use codex_protocol::AgentPath;
use codex_state::SqliteConfig;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use std::sync::Mutex;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Debug)]
struct RecordedRequest {
    method: String,
    params: serde_json::Value,
}

/// Returns and resets `(thread/loaded/list, thread/read)` request counts.
fn take_backfill_counts(requests: &Arc<Mutex<Vec<RecordedRequest>>>) -> (usize, usize) {
    let requests = std::mem::take(&mut *requests.lock().expect("request recorder lock"));
    (
        requests
            .iter()
            .filter(|request| request.method == "thread/loaded/list")
            .count(),
        requests
            .iter()
            .filter(|request| request.method == "thread/read")
            .count(),
    )
}

/// Starts an embedded app server behind a loopback WebSocket proxy that records JSON-RPC methods.
async fn start_recording_app_server(
    config: &Config,
) -> Result<(
    AppServerSession,
    Arc<Mutex<Vec<RecordedRequest>>>,
    JoinHandle<Result<()>>,
)> {
    start_recording_app_server_with_options(config, RecordingAppServerOptions::default()).await
}

#[derive(Clone, Copy, Default)]
struct RecordingAppServerOptions {
    ignore_ancestor_filter: bool,
    broaden_source_filter: bool,
    fail_thread_list_request: Option<usize>,
    empty_loaded_threads: bool,
    picker_cursor_fault: Option<PickerCursorFault>,
    legacy_unique_cursors: bool,
    legacy_oversized_page: bool,
    empty_picker_pages: bool,
}

#[derive(Clone, Copy)]
enum PickerCursorFault {
    Repeat,
    ShortCycle,
    Unique,
}

impl PickerCursorFault {
    fn cursor_for_page(self, page: usize) -> &'static str {
        match self {
            Self::Repeat => "repeat-cursor",
            Self::ShortCycle if page == 2 => "cycle-b",
            Self::ShortCycle => "cycle-a",
            Self::Unique => match page {
                1 => "unique-a",
                _ => "unique-b",
            },
        }
    }
}

fn test_support_error(error: impl std::fmt::Display) -> color_eyre::eyre::Report {
    color_eyre::eyre::eyre!(error.to_string())
}

#[allow(clippy::too_many_arguments)]
fn create_spawn_rollout(
    codex_home: &std::path::Path,
    model_provider: &str,
    timestamp: &str,
    created_at: &str,
    message: &str,
    parent_thread_id: ThreadId,
    root_thread_id: ThreadId,
    agent_path: &str,
) -> Result<ThreadId> {
    let thread_id = create_fake_parented_rollout_with_source(
        codex_home,
        timestamp,
        created_at,
        message,
        Some(model_provider),
        /*git_info*/ None,
        RolloutSessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: Some(AgentPath::try_from(agent_path).expect("valid agent path")),
            agent_nickname: Some(
                agent_path
                    .rsplit('/')
                    .next()
                    .unwrap_or(agent_path)
                    .to_string(),
            ),
            agent_role: Some("worker".to_string()),
        }),
        root_thread_id.into(),
        root_thread_id,
    )
    .map_err(test_support_error)?;
    Ok(ThreadId::from_string(&thread_id)?)
}

#[allow(clippy::too_many_arguments)]
fn create_non_spawn_subagent_rollout(
    codex_home: &std::path::Path,
    model_provider: &str,
    timestamp: &str,
    created_at: &str,
    message: &str,
    source: SubAgentSource,
    parent_thread_id: ThreadId,
    root_thread_id: ThreadId,
) -> Result<ThreadId> {
    let thread_id = create_fake_parented_rollout_with_source(
        codex_home,
        timestamp,
        created_at,
        message,
        Some(model_provider),
        /*git_info*/ None,
        RolloutSessionSource::SubAgent(source),
        parent_thread_id.into(),
        root_thread_id,
    )
    .map_err(test_support_error)?;
    Ok(ThreadId::from_string(&thread_id)?)
}

async fn start_recording_app_server_with_options(
    config: &Config,
    options: RecordingAppServerOptions,
) -> Result<(
    AppServerSession,
    Arc<Mutex<Vec<RecordedRequest>>>,
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
        let mut thread_list_request_count = 0;
        let mut picker_page_request_count = 0;
        let mut legacy_page_request_count = 0;
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
                    let method = request.method.clone();
                    let request_id = request.id.clone();
                    let mut request = serde_json::to_value(request)?;
                    let is_picker_page_request = method == "thread/list"
                        && request["params"]["limit"] == AGENT_PICKER_PAGE_SIZE
                        && request["params"]["ancestorThreadId"].is_string();
                    let is_legacy_page_request = method == "thread/list"
                        && request["params"]["limit"] == AGENT_PICKER_PAGE_SIZE
                        && request["params"]["sourceKinds"]
                            == serde_json::json!(["subAgentThreadSpawn"])
                        && !request["params"]["ancestorThreadId"].is_string();
                    request_sink
                        .lock()
                        .expect("request recorder lock")
                        .push(RecordedRequest {
                            method: method.clone(),
                            params: request
                                .get("params")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                        });
                    if method == "thread/list" {
                        thread_list_request_count += 1;
                    }
                    if options.empty_loaded_threads && method == "thread/loaded/list" {
                        websocket
                            .send(Message::Text(
                                serde_json::to_string(&JSONRPCMessage::Response(
                                    JSONRPCResponse {
                                        id: request_id,
                                        result: serde_json::json!({
                                            "data": [],
                                            "nextCursor": null,
                                        }),
                                    },
                                ))?
                                .into(),
                            ))
                            .await?;
                        continue;
                    }
                    if options.empty_picker_pages && is_picker_page_request {
                        websocket
                            .send(Message::Text(
                                serde_json::to_string(&JSONRPCMessage::Response(
                                    JSONRPCResponse {
                                        id: request_id,
                                        result: serde_json::json!({
                                            "data": [],
                                            "nextCursor": null,
                                        }),
                                    },
                                ))?
                                .into(),
                            ))
                            .await?;
                        continue;
                    }
                    if options.fail_thread_list_request == Some(thread_list_request_count)
                        && method == "thread/list"
                    {
                        websocket
                            .send(Message::Text(
                                serde_json::to_string(&JSONRPCMessage::Error(JSONRPCError {
                                    id: request_id,
                                    error: JSONRPCErrorError {
                                        code: -32603,
                                        message: "injected thread/list failure".to_string(),
                                        data: None,
                                    },
                                }))?
                                .into(),
                            ))
                            .await?;
                        continue;
                    }
                    if options.ignore_ancestor_filter && method == "thread/list" {
                        request
                            .get_mut("params")
                            .and_then(serde_json::Value::as_object_mut)
                            .map(|params| params.remove("ancestorThreadId"));
                    }
                    if options.broaden_source_filter && method == "thread/list" {
                        request
                            .get_mut("params")
                            .and_then(serde_json::Value::as_object_mut)
                            .map(|params| {
                                params.insert(
                                    "sourceKinds".to_string(),
                                    serde_json::json!(["subAgent"]),
                                )
                            });
                    }
                    if options.picker_cursor_fault.is_some() && is_picker_page_request {
                        picker_page_request_count += 1;
                        request
                            .get_mut("params")
                            .and_then(serde_json::Value::as_object_mut)
                            .map(|params| params.remove("cursor"));
                    }
                    if options.legacy_unique_cursors && is_legacy_page_request {
                        legacy_page_request_count += 1;
                        request
                            .get_mut("params")
                            .and_then(serde_json::Value::as_object_mut)
                            .map(|params| params.remove("cursor"));
                    }
                    let request = serde_json::from_value::<ClientRequest>(request)?;
                    let mut response = match embedded.request(request).await? {
                        Ok(result) => JSONRPCMessage::Response(JSONRPCResponse {
                            id: request_id,
                            result,
                        }),
                        Err(error) => JSONRPCMessage::Error(JSONRPCError {
                            id: request_id,
                            error,
                        }),
                    };
                    if let (
                        Some(cursor_fault),
                        true,
                        JSONRPCMessage::Response(JSONRPCResponse { result, .. }),
                    ) = (
                        options.picker_cursor_fault,
                        is_picker_page_request,
                        &mut response,
                    ) {
                        result["nextCursor"] = serde_json::json!(
                            cursor_fault.cursor_for_page(picker_page_request_count)
                        );
                    }
                    if is_legacy_page_request
                        && let JSONRPCMessage::Response(JSONRPCResponse { result, .. }) =
                            &mut response
                    {
                        if options.legacy_unique_cursors {
                            result["nextCursor"] =
                                serde_json::json!(format!("legacy-{legacy_page_request_count}"));
                        }
                        let oversized_page_thread = options
                            .legacy_oversized_page
                            .then(|| {
                                result["data"]
                                    .as_array()
                                    .and_then(|data| data.first())
                                    .cloned()
                            })
                            .flatten();
                        if let Some(thread) = oversized_page_thread {
                            result["data"] = serde_json::Value::Array(vec![
                                thread.clone();
                                LEGACY_AGENT_PICKER_MAX_THREADS
                                    + 1
                            ]);
                        }
                    }
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
                        .any(|request| request.method == "thread/name/set"),
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
                        model_provider: Some(app.config.model_provider_id.clone()),
                        is_running: false,
                        is_closed: false,
                        created_at: Some(1_767_225_601),
                        updated_at: Some(1_767_225_601),
                        ..AgentPickerThreadEntry::default()
                    })
                );

                Box::pin(app.open_agent_picker(&mut app_server)).await;

                // The picker refreshes the primary thread once. Discovered children were already
                // refreshed by the picker's initial backfill and must not be read a second time.
                assert_eq!(take_backfill_counts(&requests), (1, expected_reads + 1));
                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("session lifecycle request test thread")
}

#[test]
fn agent_picker_pages_persisted_subagents_with_explicit_source_filter() -> Result<()> {
    const DESCENDANT_COUNT: usize = 51;
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-agent-picker-persisted-pages".to_string())
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
                let mut child_thread_ids = Vec::with_capacity(DESCENDANT_COUNT);
                for index in 0..DESCENDANT_COUNT {
                    let seconds_from_start = index + 1;
                    let minute = seconds_from_start / 60;
                    let second = seconds_from_start % 60;
                    let timestamp = format!("2026-01-09T00-{minute:02}-{second:02}");
                    let created_at = format!("2026-01-09T00:{minute:02}:{second:02}Z");
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
                                    AgentPath::try_from(format!("/root/worker_{index}"))
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
                let (mut app_server, requests, proxy) = start_recording_app_server_with_options(
                    &app.config,
                    RecordingAppServerOptions {
                        empty_loaded_threads: true,
                        ..RecordingAppServerOptions::default()
                    },
                )
                .await?;
                // Populate the relationship index as modern sessions do.
                let mut repair_cursor = None;
                loop {
                    let repair_page = app_server
                        .thread_list(codex_app_server_protocol::ThreadListParams {
                            cursor: repair_cursor,
                            limit: Some(AGENT_PICKER_PAGE_SIZE),
                            sort_key: Some(codex_app_server_protocol::ThreadSortKey::UpdatedAt),
                            sort_direction: Some(codex_app_server_protocol::SortDirection::Desc),
                            model_providers: None,
                            source_kinds: Some(vec![
                                codex_app_server_protocol::ThreadSourceKind::SubAgent,
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
                requests.lock().expect("request recorder lock").clear();
                // This fixture exercises the modern ancestor-scoped picker pages. The legacy
                // relationship repair deliberately scans every persisted subagent page and is
                // covered separately below, so keep it from preloading the continuation here.
                app.agent_navigation.mark_legacy_relation_fallback_checked();

                let root = app_server
                    .resume_thread(
                        app.config.clone(),
                        root_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                app.enqueue_primary_thread_session(root.session, root.turns)
                    .await?;

                let first_page = app.backfill_loaded_subagent_threads(&mut app_server).await;
                assert!(first_page.completed);
                let thread_list_params = requests
                    .lock()
                    .expect("request recorder lock")
                    .iter()
                    .filter(|request| request.method == "thread/list")
                    .map(|request| request.params.clone())
                    .collect::<Vec<_>>();
                assert_eq!(
                    thread_list_params[0]["sourceKinds"],
                    serde_json::json!(["subAgentThreadSpawn"]),
                    "the persisted picker page must request only thread-spawn descendants"
                );
                assert_eq!(
                    thread_list_params[0]["ancestorThreadId"],
                    root_thread_id.to_string(),
                    "the persisted picker page must bind the active ancestor"
                );
                assert!(thread_list_params.iter().all(|params| {
                    params["sourceKinds"] == serde_json::json!(["subAgentThreadSpawn"])
                }));
                assert_eq!(
                    app.agent_navigation
                        .ordered_path_backed_subagent_threads(Some(root_thread_id))
                        .len(),
                    AGENT_PICKER_PAGE_SIZE as usize
                );
                assert!(app.agent_navigation.next_picker_page_cursor().is_some());

                Box::pin(app.load_more_agent_picker_page(&mut app_server)).await;

                let picker_page_params = requests
                    .lock()
                    .expect("request recorder lock")
                    .iter()
                    .filter(|request| {
                        request.method == "thread/list"
                            && request.params["limit"] == AGENT_PICKER_PAGE_SIZE
                    })
                    .map(|request| request.params.clone())
                    .collect::<Vec<_>>();
                assert_eq!(picker_page_params.len(), 2);
                assert!(picker_page_params.iter().all(|params| {
                    params["sourceKinds"] == serde_json::json!(["subAgentThreadSpawn"])
                        && params["ancestorThreadId"] == root_thread_id.to_string()
                }));

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
                    "the continuation must expose descendants beyond the first bounded page"
                );
                assert_eq!(app.agent_navigation.next_picker_page_cursor(), None);

                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("persisted agent picker pagination test thread")
}

async fn assert_picker_cursor_fault_fails_closed(
    cursor_fault: PickerCursorFault,
    continuation_attempts: usize,
    preseed_after_first_page: usize,
) -> Result<()> {
    let mut app = make_test_app().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
    let root_thread_id = ThreadId::from_string(
        &create_fake_rollout(
            codex_home.path(),
            "2026-01-09T01-00-00",
            "2026-01-09T01:00:00Z",
            "Cursor-boundary root",
            Some(app.config.model_provider_id.as_str()),
            /*git_info*/ None,
        )
        .map_err(test_support_error)?,
    )?;
    create_spawn_rollout(
        codex_home.path(),
        app.config.model_provider_id.as_str(),
        "2026-01-09T01-00-01",
        "2026-01-09T01:00:01Z",
        "Cursor-boundary child",
        root_thread_id,
        root_thread_id,
        "/root/cursor_child",
    )?;
    let (mut app_server, requests, proxy) = start_recording_app_server_with_options(
        &app.config,
        RecordingAppServerOptions {
            empty_loaded_threads: true,
            picker_cursor_fault: Some(cursor_fault),
            ..RecordingAppServerOptions::default()
        },
    )
    .await?;
    app_server
        .thread_list(codex_app_server_protocol::ThreadListParams {
            cursor: None,
            limit: Some(AGENT_PICKER_PAGE_SIZE),
            sort_key: Some(codex_app_server_protocol::ThreadSortKey::UpdatedAt),
            sort_direction: Some(codex_app_server_protocol::SortDirection::Desc),
            model_providers: None,
            source_kinds: Some(vec![codex_app_server_protocol::ThreadSourceKind::SubAgent]),
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
    requests.lock().expect("request recorder lock").clear();
    app.agent_navigation.mark_legacy_relation_fallback_checked();
    let root = app_server
        .resume_thread(
            app.config.clone(),
            root_thread_id,
            app.resume_model_settings(),
        )
        .await?;
    app.enqueue_primary_thread_session(root.session, root.turns)
        .await?;

    let first_page = app.backfill_loaded_subagent_threads(&mut app_server).await;
    assert!(first_page.completed);
    assert!(app.agent_navigation.next_picker_page_cursor().is_some());
    for index in 0..preseed_after_first_page {
        assert!(
            app.agent_navigation
                .set_next_picker_page_cursor(Some(format!("preseed-{index}")))
        );
    }
    for _ in 0..continuation_attempts {
        Box::pin(app.load_more_agent_picker_page(&mut app_server)).await;
    }
    assert_eq!(
        app.agent_navigation.next_picker_page_cursor(),
        None,
        "a repeated or cycling picker cursor must end pagination"
    );
    let request_count_before_extra_load = requests
        .lock()
        .expect("request recorder lock")
        .iter()
        .filter(|request| {
            request.method == "thread/list" && request.params["limit"] == AGENT_PICKER_PAGE_SIZE
        })
        .count();
    assert_eq!(request_count_before_extra_load, continuation_attempts + 1);
    Box::pin(app.load_more_agent_picker_page(&mut app_server)).await;
    let requests = requests.lock().expect("request recorder lock");
    let picker_page_requests = requests
        .iter()
        .filter(|request| {
            request.method == "thread/list" && request.params["limit"] == AGENT_PICKER_PAGE_SIZE
        })
        .collect::<Vec<_>>();
    assert_eq!(picker_page_requests.len(), request_count_before_extra_load);
    assert!(picker_page_requests.iter().all(|request| {
        request.params["sourceKinds"] == serde_json::json!(["subAgentThreadSpawn"])
            && request.params["ancestorThreadId"] == root_thread_id.to_string()
    }));
    drop(requests);

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[test]
fn agent_picker_stops_repeated_cycling_and_over_budget_continuations() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-agent-picker-cursor-cycle-isolation".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                assert_picker_cursor_fault_fails_closed(PickerCursorFault::Repeat, 1, 0).await?;
                assert_picker_cursor_fault_fails_closed(PickerCursorFault::ShortCycle, 2, 0)
                    .await?;
                assert_picker_cursor_fault_fails_closed(
                    PickerCursorFault::Unique,
                    1,
                    AGENT_PICKER_CURSOR_BUDGET - 1,
                )
                .await
            })
        })?
        .join()
        .expect("agent picker cursor cycle isolation test thread")
}

#[test]
fn agent_picker_rejects_non_spawn_descendants_when_server_broadens_source_filter() -> Result<()> {
    const UNSAFE_DESCENDANT_COUNT: usize = AGENT_PICKER_PAGE_SIZE as usize;
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-agent-picker-source-isolation".to_string())
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
                        "2026-01-09T02-00-00",
                        "2026-01-09T02:00:00Z",
                        "Source-boundary root",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .map_err(test_support_error)?,
                )?;
                let mut unsafe_thread_ids = Vec::with_capacity(UNSAFE_DESCENDANT_COUNT);
                for index in 0..UNSAFE_DESCENDANT_COUNT {
                    let source = match index % 3 {
                        0 => SubAgentSource::Review,
                        1 => SubAgentSource::Compact,
                        _ => SubAgentSource::Other(format!("source-boundary-{index}")),
                    };
                    unsafe_thread_ids.push(create_non_spawn_subagent_rollout(
                        codex_home.path(),
                        app.config.model_provider_id.as_str(),
                        &format!("2026-01-09T02-00-{:02}", index + 1),
                        &format!("2026-01-09T02:00:{:02}Z", index + 1),
                        &format!("Unsafe descendant {index}"),
                        source,
                        root_thread_id,
                        root_thread_id,
                    )?);
                }
                let valid_thread_id = create_spawn_rollout(
                    codex_home.path(),
                    app.config.model_provider_id.as_str(),
                    "2026-01-09T02-01-00",
                    "2026-01-09T02:01:00Z",
                    "Valid spawned descendant",
                    root_thread_id,
                    root_thread_id,
                    "/root/valid_spawn",
                )?;

                let (mut app_server, requests, proxy) = start_recording_app_server_with_options(
                    &app.config,
                    RecordingAppServerOptions {
                        broaden_source_filter: true,
                        empty_loaded_threads: true,
                        ..RecordingAppServerOptions::default()
                    },
                )
                .await?;
                let mut repair_cursor = None;
                loop {
                    let page = app_server
                        .thread_list(codex_app_server_protocol::ThreadListParams {
                            cursor: repair_cursor,
                            limit: Some(AGENT_PICKER_PAGE_SIZE),
                            sort_key: Some(codex_app_server_protocol::ThreadSortKey::UpdatedAt),
                            sort_direction: Some(codex_app_server_protocol::SortDirection::Desc),
                            model_providers: None,
                            source_kinds: Some(vec![
                                codex_app_server_protocol::ThreadSourceKind::SubAgent,
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
                    repair_cursor = page.next_cursor;
                    if repair_cursor.is_none() {
                        break;
                    }
                }
                requests.lock().expect("request recorder lock").clear();
                app.agent_navigation.mark_legacy_relation_fallback_checked();

                let root = app_server
                    .resume_thread(
                        app.config.clone(),
                        root_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                app.enqueue_primary_thread_session(root.session, root.turns)
                    .await?;
                let completed = app.backfill_loaded_subagent_threads(&mut app_server).await;
                assert!(completed.completed);
                assert!(app.agent_navigation.get(&valid_thread_id).is_some());
                for unsafe_thread_id in unsafe_thread_ids {
                    assert!(
                        app.agent_navigation.get(&unsafe_thread_id).is_none(),
                        "an ancestry-valid non-spawn row must not enter picker state"
                    );
                }
                assert_eq!(
                    app.agent_navigation.next_picker_page_cursor(),
                    None,
                    "a cursor from a source-corrupted page must be discarded"
                );
                let thread_list_requests = requests
                    .lock()
                    .expect("request recorder lock")
                    .iter()
                    .filter(|request| request.method == "thread/list")
                    .cloned()
                    .collect::<Vec<_>>();
                assert_eq!(thread_list_requests.len(), 1);
                assert_eq!(
                    thread_list_requests[0].params["sourceKinds"],
                    serde_json::json!(["subAgentThreadSpawn"])
                );
                assert_eq!(
                    thread_list_requests[0].params["ancestorThreadId"],
                    root_thread_id.to_string()
                );

                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("agent picker source isolation test thread")
}

#[test]
fn legacy_agent_picker_relation_repair_retries_until_cursor_exhaustion() -> Result<()> {
    const UNRELATED_COUNT: usize = 51;
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-agent-picker-legacy-repair-pages".to_string())
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
                        "Primary root",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .map_err(test_support_error)?,
                )?;
                let target_thread_id = create_spawn_rollout(
                    codex_home.path(),
                    app.config.model_provider_id.as_str(),
                    "2026-01-09T00-00-01",
                    "2026-01-09T00:00:01Z",
                    "Legacy target beyond the first page",
                    root_thread_id,
                    root_thread_id,
                    "/root/legacy_target",
                )?;
                let unrelated_root_thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home.path(),
                        "2026-01-09T00-00-02",
                        "2026-01-09T00:00:02Z",
                        "Unrelated root",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .map_err(test_support_error)?,
                )?;
                for index in 0..UNRELATED_COUNT {
                    let seconds_from_start = index + 3;
                    let minute = seconds_from_start / 60;
                    let second = seconds_from_start % 60;
                    create_spawn_rollout(
                        codex_home.path(),
                        app.config.model_provider_id.as_str(),
                        &format!("2026-01-09T00-{minute:02}-{second:02}"),
                        &format!("2026-01-09T00:{minute:02}:{second:02}Z"),
                        &format!("Unrelated child {index}"),
                        unrelated_root_thread_id,
                        unrelated_root_thread_id,
                        &format!("/root/unrelated_{index}"),
                    )?;
                }

                let (mut app_server, requests, proxy) = start_recording_app_server_with_options(
                    &app.config,
                    RecordingAppServerOptions {
                        fail_thread_list_request: Some(3),
                        ..RecordingAppServerOptions::default()
                    },
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

                let incomplete = app.backfill_loaded_subagent_threads(&mut app_server).await;
                assert!(!incomplete.completed);
                assert!(
                    app.agent_navigation.needs_legacy_relation_fallback_check(),
                    "a partial legacy scan must remain retryable"
                );
                assert!(app.agent_navigation.get(&target_thread_id).is_none());
                assert_eq!(
                    requests
                        .lock()
                        .expect("request recorder lock")
                        .iter()
                        .filter(|request| request.method == "thread/list")
                        .count(),
                    3,
                    "modern lookup plus two legacy pages must be attempted"
                );
                app_server.shutdown().await?;
                proxy.await??;

                let (mut app_server, requests, proxy) =
                    start_recording_app_server(&app.config).await?;
                let completed = app.backfill_loaded_subagent_threads(&mut app_server).await;
                assert!(completed.completed);
                assert!(
                    !app.agent_navigation.needs_legacy_relation_fallback_check(),
                    "the legacy fallback may be marked complete only after cursor exhaustion"
                );
                assert!(app.agent_navigation.get(&target_thread_id).is_some());
                assert_eq!(
                    requests
                        .lock()
                        .expect("request recorder lock")
                        .iter()
                        .filter(|request| request.method == "thread/list")
                        .count(),
                    3,
                    "the successful retry must consume both legacy pages"
                );

                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("legacy relation repair pagination test thread")
}

async fn assert_legacy_relation_budget_fails_closed(
    legacy_unique_cursors: bool,
    legacy_oversized_page: bool,
    expected_legacy_requests: usize,
) -> Result<()> {
    let mut app = make_test_app().await;
    let codex_home = tempdir()?;
    app.config.codex_home = codex_home.path().to_path_buf().abs();
    app.config.sqlite = SqliteConfig::new_for_testing(codex_home.path().abs());
    let root_thread_id = ThreadId::from_string(
        &create_fake_rollout(
            codex_home.path(),
            "2026-01-09T03-00-00",
            "2026-01-09T03:00:00Z",
            "Legacy budget root",
            Some(app.config.model_provider_id.as_str()),
            /*git_info*/ None,
        )
        .map_err(test_support_error)?,
    )?;
    let target_thread_id = create_spawn_rollout(
        codex_home.path(),
        app.config.model_provider_id.as_str(),
        "2026-01-09T03-00-01",
        "2026-01-09T03:00:01Z",
        "Legacy budget child",
        root_thread_id,
        root_thread_id,
        "/root/legacy_budget_child",
    )?;
    let (mut app_server, requests, proxy) = start_recording_app_server_with_options(
        &app.config,
        RecordingAppServerOptions {
            empty_loaded_threads: true,
            empty_picker_pages: true,
            legacy_unique_cursors,
            legacy_oversized_page,
            ..RecordingAppServerOptions::default()
        },
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

    let incomplete = app.backfill_loaded_subagent_threads(&mut app_server).await;
    assert!(!incomplete.completed);
    assert!(app.agent_navigation.needs_legacy_relation_fallback_check());
    assert!(
        app.agent_navigation.get(&target_thread_id).is_none(),
        "an over-budget legacy scan must not partially admit descendants"
    );
    let legacy_request_count = requests
        .lock()
        .expect("request recorder lock")
        .iter()
        .filter(|request| {
            request.method == "thread/list"
                && request.params["sourceKinds"] == serde_json::json!(["subAgentThreadSpawn"])
                && !request.params["ancestorThreadId"].is_string()
        })
        .count();
    assert_eq!(legacy_request_count, expected_legacy_requests);

    app_server.shutdown().await?;
    proxy.await??;
    Ok(())
}

#[test]
fn legacy_agent_picker_scan_enforces_page_and_thread_budgets() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-agent-picker-legacy-budget-isolation".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                assert_legacy_relation_budget_fails_closed(
                    /*legacy_unique_cursors*/ true,
                    /*legacy_oversized_page*/ false,
                    LEGACY_AGENT_PICKER_MAX_PAGES,
                )
                .await?;
                assert_legacy_relation_budget_fails_closed(
                    /*legacy_unique_cursors*/ false, /*legacy_oversized_page*/ true,
                    /*expected_legacy_requests*/ 1,
                )
                .await
            })
        })?
        .join()
        .expect("legacy relation repair budget test thread")
}

#[test]
fn agent_picker_rejects_mixed_roots_when_server_ignores_ancestor_filter() -> Result<()> {
    const OWN_COUNT: usize = AGENT_PICKER_PAGE_SIZE as usize;
    const FOREIGN_COUNT: usize = 2;
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    std::thread::Builder::new()
        .name("tui-agent-picker-mixed-root-isolation".to_string())
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
                        "Primary root",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .map_err(test_support_error)?,
                )?;
                let foreign_root_thread_id = ThreadId::from_string(
                    &create_fake_rollout(
                        codex_home.path(),
                        "2026-01-10T00-00-01",
                        "2026-01-10T00:00:01Z",
                        "Foreign root",
                        Some(app.config.model_provider_id.as_str()),
                        /*git_info*/ None,
                    )
                    .map_err(test_support_error)?,
                )?;
                let mut own_child_thread_ids = Vec::with_capacity(OWN_COUNT);
                for index in 0..OWN_COUNT {
                    let seconds_from_start = index + 60;
                    let minute = seconds_from_start / 60;
                    let second = seconds_from_start % 60;
                    own_child_thread_ids.push(create_spawn_rollout(
                        codex_home.path(),
                        app.config.model_provider_id.as_str(),
                        &format!("2026-01-10T00-{minute:02}-{second:02}"),
                        &format!("2026-01-10T00:{minute:02}:{second:02}Z"),
                        &format!("Own child {index}"),
                        root_thread_id,
                        root_thread_id,
                        &format!("/root/own_child_{index}"),
                    )?);
                }
                let mut foreign_thread_ids = Vec::with_capacity(FOREIGN_COUNT);
                for index in 0..FOREIGN_COUNT {
                    let seconds_from_start = index + 2;
                    let minute = seconds_from_start / 60;
                    let second = seconds_from_start % 60;
                    foreign_thread_ids.push(create_spawn_rollout(
                        codex_home.path(),
                        app.config.model_provider_id.as_str(),
                        &format!("2026-01-10T00-{minute:02}-{second:02}"),
                        &format!("2026-01-10T00:{minute:02}:{second:02}Z"),
                        &format!("Foreign child {index}"),
                        foreign_root_thread_id,
                        foreign_root_thread_id,
                        &format!("/root/foreign_{index}"),
                    )?);
                }

                let (mut app_server, requests, proxy) = start_recording_app_server_with_options(
                    &app.config,
                    RecordingAppServerOptions {
                        ignore_ancestor_filter: true,
                        ..RecordingAppServerOptions::default()
                    },
                )
                .await?;
                let mut repair_cursor = None;
                loop {
                    let page = app_server
                        .thread_list(codex_app_server_protocol::ThreadListParams {
                            cursor: repair_cursor,
                            limit: Some(AGENT_PICKER_PAGE_SIZE),
                            sort_key: Some(codex_app_server_protocol::ThreadSortKey::UpdatedAt),
                            sort_direction: Some(codex_app_server_protocol::SortDirection::Desc),
                            model_providers: None,
                            source_kinds: Some(vec![
                                codex_app_server_protocol::ThreadSourceKind::SubAgent,
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
                    repair_cursor = page.next_cursor;
                    if repair_cursor.is_none() {
                        break;
                    }
                }
                requests.lock().expect("request recorder lock").clear();

                let root = app_server
                    .resume_thread(
                        app.config.clone(),
                        root_thread_id,
                        app.resume_model_settings(),
                    )
                    .await?;
                app.enqueue_primary_thread_session(root.session, root.turns)
                    .await?;
                let completed = app.backfill_loaded_subagent_threads(&mut app_server).await;
                assert!(completed.completed);
                for own_child_thread_id in own_child_thread_ids {
                    assert!(app.agent_navigation.get(&own_child_thread_id).is_some());
                }
                for foreign_thread_id in foreign_thread_ids {
                    assert!(
                        app.agent_navigation.get(&foreign_thread_id).is_none(),
                        "a foreign-root row must never enter picker selection state"
                    );
                }
                assert_eq!(
                    app.agent_navigation.next_picker_page_cursor(),
                    None,
                    "a cursor from an unscoped mixed-root page must not be reused"
                );
                assert_eq!(
                    app.agent_navigation
                        .ordered_path_backed_subagent_threads(Some(root_thread_id))
                        .len(),
                    OWN_COUNT,
                    "only locally verified descendants may be exposed or resumed"
                );

                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("mixed-root isolation test thread")
}
