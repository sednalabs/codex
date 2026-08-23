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
use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadStatus;
use codex_protocol::AgentPath;
use codex_state::SqliteConfig;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Mutex;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Debug)]
struct RecordedRequest {
    method: String,
    params: Option<serde_json::Value>,
}

type RecordedRequests = Arc<Mutex<Vec<RecordedRequest>>>;

#[derive(Clone)]
enum ScriptedLineageResponse {
    Page(serde_json::Value),
    Error(String),
}

#[derive(Clone)]
enum ScriptedThreadReadResponse {
    Thread(serde_json::Value),
    Error(String),
}

fn take_recorded_requests(requests: &RecordedRequests) -> Vec<RecordedRequest> {
    std::mem::take(&mut *requests.lock().expect("request recorder lock"))
}

/// Returns and resets `(thread/list, thread/loaded/list, thread/read)` request counts.
fn take_backfill_counts(requests: &RecordedRequests) -> (usize, usize, usize) {
    let requests = take_recorded_requests(requests);
    (
        requests
            .iter()
            .filter(|request| request.method == "thread/list")
            .count(),
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

fn take_backfill_counts_and_lineage_cursors(
    requests: &RecordedRequests,
) -> ((usize, usize, usize), Vec<Option<String>>) {
    let requests = take_recorded_requests(requests);
    let counts = (
        requests
            .iter()
            .filter(|request| request.method == "thread/list")
            .count(),
        requests
            .iter()
            .filter(|request| request.method == "thread/loaded/list")
            .count(),
        requests
            .iter()
            .filter(|request| request.method == "thread/read")
            .count(),
    );
    let cursors = requests
        .iter()
        .filter(|request| request.method == "thread/list")
        .map(|request| {
            request
                .params
                .as_ref()
                .and_then(|params| params.get("cursor"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect();
    (counts, cursors)
}

/// Starts an embedded app server behind a loopback WebSocket proxy that records JSON-RPC methods.
async fn start_recording_app_server(
    config: &Config,
    blocked_thread_read_id: Option<ThreadId>,
) -> Result<(AppServerSession, RecordedRequests, JoinHandle<Result<()>>)> {
    start_recording_app_server_with_lineage(
        config,
        blocked_thread_read_id,
        /*lineage_responses*/ None,
        /*thread_read_responses*/ None,
    )
    .await
}

async fn start_recording_app_server_with_lineage(
    config: &Config,
    blocked_thread_read_id: Option<ThreadId>,
    lineage_responses: Option<Arc<Mutex<VecDeque<ScriptedLineageResponse>>>>,
    thread_read_responses: Option<Arc<Mutex<VecDeque<ScriptedThreadReadResponse>>>>,
) -> Result<(AppServerSession, RecordedRequests, JoinHandle<Result<()>>)> {
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
    let blocked_thread_read_id = blocked_thread_read_id.map(|thread_id| thread_id.to_string());
    let proxy = tokio::spawn(async move {
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
                        .push(RecordedRequest {
                            method: request.method.clone(),
                            params: request.params.clone(),
                        });
                    if request.method == "thread/list"
                        && let Some(lineage_responses) = &lineage_responses
                    {
                        let response = lineage_responses
                            .lock()
                            .expect("lineage response lock")
                            .pop_front()
                            .expect("scripted thread/list response");
                        let message = match response {
                            ScriptedLineageResponse::Page(result) => {
                                JSONRPCMessage::Response(JSONRPCResponse {
                                    id: request.id.clone(),
                                    result,
                                })
                            }
                            ScriptedLineageResponse::Error(message) => {
                                JSONRPCMessage::Error(JSONRPCError {
                                    id: request.id.clone(),
                                    error: JSONRPCErrorError {
                                        code: -32603,
                                        message,
                                        data: None,
                                    },
                                })
                            }
                        };
                        websocket
                            .send(Message::Text(serde_json::to_string(&message)?.into()))
                            .await?;
                        continue;
                    }
                    if request.method == "thread/read"
                        && let Some(thread_read_responses) = &thread_read_responses
                    {
                        let response = thread_read_responses
                            .lock()
                            .expect("thread/read response lock")
                            .pop_front()
                            .expect("scripted thread/read response");
                        let message = match response {
                            ScriptedThreadReadResponse::Thread(thread) => {
                                JSONRPCMessage::Response(JSONRPCResponse {
                                    id: request.id.clone(),
                                    result: serde_json::json!({ "thread": thread }),
                                })
                            }
                            ScriptedThreadReadResponse::Error(message) => {
                                JSONRPCMessage::Error(JSONRPCError {
                                    id: request.id.clone(),
                                    error: JSONRPCErrorError {
                                        code: -32603,
                                        message,
                                        data: None,
                                    },
                                })
                            }
                        };
                        websocket
                            .send(Message::Text(serde_json::to_string(&message)?.into()))
                            .await?;
                        continue;
                    }
                    if request.method == "thread/read"
                        && blocked_thread_read_id
                            .as_deref()
                            .is_some_and(|blocked_thread_read_id| {
                                request
                                    .params
                                    .as_ref()
                                    .and_then(|params| params.get("threadId"))
                                    .and_then(serde_json::Value::as_str)
                                    == Some(blocked_thread_read_id)
                            })
                    {
                        std::future::pending::<()>().await;
                    }
                    let request_id = request.id.clone();
                    let request =
                        serde_json::from_value::<ClientRequest>(serde_json::to_value(request)?)?;
                    let response = match embedded.request(request).await? {
                        Ok(result) => JSONRPCMessage::Response(JSONRPCResponse {
                            id: request_id,
                            result,
                        }),
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

fn scripted_lineage_thread(
    config: &Config,
    thread_id: ThreadId,
    parent_thread_id: ThreadId,
    depth: i32,
) -> Thread {
    Thread {
        id: thread_id.to_string(),
        extra: None,
        session_id: thread_id.to_string(),
        forked_from_id: None,
        parent_thread_id: Some(parent_thread_id.to_string()),
        preview: String::new(),
        ephemeral: false,
        is_pinned: false,
        history_mode: Default::default(),
        model_provider: config.model_provider_id.clone(),
        model: config.model.clone(),
        reasoning_effort: None,
        created_at: i64::from(depth),
        updated_at: i64::from(depth),
        recency_at: Some(i64::from(depth)),
        status: ThreadStatus::Idle,
        path: None,
        cwd: config.cwd.clone(),
        cli_version: "0.0.0".to_string(),
        source: SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth,
            agent_path: None,
            agent_nickname: Some(format!("child-{depth}")),
            agent_role: Some("worker".to_string()),
        }),
        can_accept_direct_input: Some(true),
        thread_source: None,
        agent_nickname: Some(format!("child-{depth}")),
        agent_role: Some("worker".to_string()),
        git_info: None,
        name: None,
        turns: Vec::new(),
    }
}

fn scripted_lineage_page(threads: Vec<Thread>, next_cursor: Option<String>) -> serde_json::Value {
    serde_json::json!({
        "data": threads,
        "nextCursor": next_cursor,
    })
}

fn configure_backfill_primary(app: &mut App, primary_thread_id: ThreadId) {
    app.primary_thread_id = Some(primary_thread_id);
    app.agent_navigation.upsert(
        primary_thread_id,
        None,
        None,
        /*is_closed*/ false,
        None,
        None,
    );
}

#[test]
fn lineage_backfill_resumes_failed_cursor_without_refetching_prefix() -> Result<()> {
    run_large_stack_app_test(|| async {
        let mut app = make_test_app().await;
        let primary_thread_id = ThreadId::new();
        let child_thread_id = ThreadId::new();
        let grandchild_thread_id = ThreadId::new();
        configure_backfill_primary(&mut app, primary_thread_id);
        let responses = Arc::new(Mutex::new(VecDeque::from([
            ScriptedLineageResponse::Page(scripted_lineage_page(
                vec![scripted_lineage_thread(
                    &app.config,
                    child_thread_id,
                    primary_thread_id,
                    1,
                )],
                Some("page-2".to_string()),
            )),
            ScriptedLineageResponse::Error("transient list failure".to_string()),
            ScriptedLineageResponse::Page(scripted_lineage_page(
                vec![scripted_lineage_thread(
                    &app.config,
                    grandchild_thread_id,
                    child_thread_id,
                    2,
                )],
                None,
            )),
        ])));
        let (mut app_server, requests, proxy) = start_recording_app_server_with_lineage(
            &app.config,
            None,
            Some(Arc::clone(&responses)),
            None,
        )
        .await?;

        let first = app.backfill_loaded_subagent_threads(&mut app_server).await;
        assert_eq!(
            first.status,
            crate::app::session_lifecycle::LoadedSubagentBackfillStatus::RetryableError
        );
        assert!(app.agent_navigation.get(&child_thread_id).is_some());
        let second = app.backfill_loaded_subagent_threads(&mut app_server).await;
        assert!(second.completed);
        assert_eq!(
            second.status,
            crate::app::session_lifecycle::LoadedSubagentBackfillStatus::Complete
        );
        assert!(app.agent_navigation.get(&grandchild_thread_id).is_some());

        let cursors = take_recorded_requests(&requests)
            .into_iter()
            .filter(|request| request.method == "thread/list")
            .map(|request| {
                request
                    .params
                    .and_then(|params| params.get("cursor").cloned())
                    .and_then(|cursor| cursor.as_str().map(str::to_string))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            cursors,
            vec![None, Some("page-2".to_string()), Some("page-2".to_string())]
        );
        app_server.shutdown().await?;
        proxy.await??;
        Ok(())
    })
}

#[test]
fn lineage_backfill_retries_authoritative_thread_read_without_relisting() -> Result<()> {
    run_large_stack_app_test(|| async {
        let mut app = make_test_app().await;
        let primary_thread_id = ThreadId::new();
        let child_thread_id = ThreadId::new();
        configure_backfill_primary(&mut app, primary_thread_id);
        let mut listed_child =
            scripted_lineage_thread(&app.config, child_thread_id, primary_thread_id, 1);
        listed_child.can_accept_direct_input = None;
        let mut authoritative_child = listed_child.clone();
        authoritative_child.can_accept_direct_input = Some(false);
        let lineage_responses =
            Arc::new(Mutex::new(VecDeque::from([ScriptedLineageResponse::Page(
                scripted_lineage_page(vec![listed_child], None),
            )])));
        let thread_read_responses = Arc::new(Mutex::new(VecDeque::from([
            ScriptedThreadReadResponse::Error("transient read failure".to_string()),
            ScriptedThreadReadResponse::Thread(serde_json::to_value(authoritative_child)?),
        ])));
        let (mut app_server, requests, proxy) = start_recording_app_server_with_lineage(
            &app.config,
            None,
            Some(Arc::clone(&lineage_responses)),
            Some(Arc::clone(&thread_read_responses)),
        )
        .await?;

        let first = app.backfill_loaded_subagent_threads(&mut app_server).await;
        assert!(!first.completed);
        assert_eq!(
            first.status,
            crate::app::session_lifecycle::LoadedSubagentBackfillStatus::RetryableError
        );
        assert_eq!(take_backfill_counts(&requests), (1, 0, 1));
        assert!(app.agent_navigation.get(&child_thread_id).is_none());
        let second = app.backfill_loaded_subagent_threads(&mut app_server).await;
        assert!(second.completed);
        assert_eq!(take_backfill_counts(&requests), (0, 0, 1));
        assert_eq!(
            second.refreshed_thread_ids,
            HashSet::from([child_thread_id])
        );
        assert!(app.agent_navigation.is_parent_owned(child_thread_id));

        app_server.shutdown().await?;
        proxy.await??;
        Ok(())
    })
}

#[test]
fn lineage_backfill_cursor_cycle_is_bounded_per_open() -> Result<()> {
    run_large_stack_app_test(|| async {
        let mut app = make_test_app().await;
        let primary_thread_id = ThreadId::new();
        configure_backfill_primary(&mut app, primary_thread_id);
        let responses = Arc::new(Mutex::new(VecDeque::from([
            ScriptedLineageResponse::Page(scripted_lineage_page(
                Vec::new(),
                Some("cycle".to_string()),
            )),
            ScriptedLineageResponse::Page(scripted_lineage_page(
                Vec::new(),
                Some("cycle".to_string()),
            )),
            ScriptedLineageResponse::Page(scripted_lineage_page(
                Vec::new(),
                Some("cycle".to_string()),
            )),
            ScriptedLineageResponse::Page(scripted_lineage_page(
                Vec::new(),
                Some("cycle".to_string()),
            )),
        ])));
        let (mut app_server, requests, proxy) = start_recording_app_server_with_lineage(
            &app.config,
            None,
            Some(Arc::clone(&responses)),
            None,
        )
        .await?;

        let first = app.backfill_loaded_subagent_threads(&mut app_server).await;
        let second = app.backfill_loaded_subagent_threads(&mut app_server).await;
        assert_eq!(
            first.status,
            crate::app::session_lifecycle::LoadedSubagentBackfillStatus::CursorCycle
        );
        assert_eq!(second.status, first.status);
        assert_eq!(take_backfill_counts(&requests).0, 4);
        assert!(app.subagent_backfill_progress.is_none());

        app_server.shutdown().await?;
        proxy.await??;
        Ok(())
    })
}

#[test]
fn lineage_backfill_persistent_cycle_refreshes_pathless_v2_child_per_open() -> Result<()> {
    run_large_stack_app_test(|| async {
        let mut app = make_test_app().await;
        let primary_thread_id = ThreadId::new();
        let child_thread_id = ThreadId::new();
        configure_backfill_primary(&mut app, primary_thread_id);
        let mut listed_child =
            scripted_lineage_thread(&app.config, child_thread_id, primary_thread_id, 1);
        listed_child.can_accept_direct_input = None;
        let mut authoritative_child = listed_child.clone();
        authoritative_child.can_accept_direct_input = Some(false);
        let responses = Arc::new(Mutex::new(VecDeque::from([
            ScriptedLineageResponse::Page(scripted_lineage_page(
                vec![listed_child.clone()],
                Some("cycle".to_string()),
            )),
            ScriptedLineageResponse::Page(scripted_lineage_page(
                Vec::new(),
                Some("cycle".to_string()),
            )),
            ScriptedLineageResponse::Page(scripted_lineage_page(
                vec![listed_child],
                Some("cycle".to_string()),
            )),
            ScriptedLineageResponse::Page(scripted_lineage_page(
                Vec::new(),
                Some("cycle".to_string()),
            )),
        ])));
        let thread_read_responses = Arc::new(Mutex::new(VecDeque::from([
            ScriptedThreadReadResponse::Thread(serde_json::to_value(authoritative_child.clone())?),
            ScriptedThreadReadResponse::Thread(serde_json::to_value(authoritative_child)?),
        ])));
        let (mut app_server, requests, proxy) = start_recording_app_server_with_lineage(
            &app.config,
            None,
            Some(Arc::clone(&responses)),
            Some(Arc::clone(&thread_read_responses)),
        )
        .await?;

        let first = app.backfill_loaded_subagent_threads(&mut app_server).await;
        assert_eq!(
            first.status,
            crate::app::session_lifecycle::LoadedSubagentBackfillStatus::CursorCycle
        );
        assert_eq!(
            take_backfill_counts_and_lineage_cursors(&requests),
            ((2, 0, 1), vec![None, Some("cycle".to_string())])
        );
        assert!(first.refreshed_thread_ids.contains(&child_thread_id));
        assert!(app.agent_navigation.get(&child_thread_id).is_some());
        assert!(app.agent_navigation.is_parent_owned(child_thread_id));
        assert!(app.subagent_backfill_progress.is_none());

        let second = app.backfill_loaded_subagent_threads(&mut app_server).await;
        assert_eq!(second.status, first.status);
        assert_eq!(
            take_backfill_counts_and_lineage_cursors(&requests),
            ((2, 0, 1), vec![None, Some("cycle".to_string())])
        );
        assert!(second.refreshed_thread_ids.contains(&child_thread_id));
        assert!(app.agent_navigation.is_parent_owned(child_thread_id));
        assert!(app.subagent_backfill_progress.is_none());

        app_server.shutdown().await?;
        proxy.await??;
        Ok(())
    })
}

#[test]
fn lineage_backfill_recovers_after_cursor_cycle_and_finds_new_child() -> Result<()> {
    run_large_stack_app_test(|| async {
        let mut app = make_test_app().await;
        let primary_thread_id = ThreadId::new();
        let child_thread_id = ThreadId::new();
        configure_backfill_primary(&mut app, primary_thread_id);
        let responses = Arc::new(Mutex::new(VecDeque::from([
            ScriptedLineageResponse::Page(scripted_lineage_page(
                Vec::new(),
                Some("cycle".to_string()),
            )),
            ScriptedLineageResponse::Page(scripted_lineage_page(
                Vec::new(),
                Some("cycle".to_string()),
            )),
            ScriptedLineageResponse::Page(scripted_lineage_page(
                vec![scripted_lineage_thread(
                    &app.config,
                    child_thread_id,
                    primary_thread_id,
                    1,
                )],
                None,
            )),
        ])));
        let (mut app_server, requests, proxy) = start_recording_app_server_with_lineage(
            &app.config,
            None,
            Some(Arc::clone(&responses)),
            None,
        )
        .await?;

        let first = app.backfill_loaded_subagent_threads(&mut app_server).await;
        assert_eq!(
            first.status,
            crate::app::session_lifecycle::LoadedSubagentBackfillStatus::CursorCycle
        );
        let second = app.backfill_loaded_subagent_threads(&mut app_server).await;
        assert!(second.completed);
        assert!(app.agent_navigation.get(&child_thread_id).is_some());
        assert_eq!(take_backfill_counts(&requests).0, 3);

        app_server.shutdown().await?;
        proxy.await??;
        Ok(())
    })
}

#[test]
fn lineage_backfill_advances_beyond_page_budget_across_opens() -> Result<()> {
    run_large_stack_app_test(|| async {
        const PAGE_COUNT: usize =
            crate::app::loaded_threads::SUBAGENT_BACKFILL_PAGES_PER_ATTEMPT + 2;
        let mut app = make_test_app().await;
        let primary_thread_id = ThreadId::new();
        configure_backfill_primary(&mut app, primary_thread_id);
        let mut pages = VecDeque::new();
        let descendant_thread_ids = (0..PAGE_COUNT).map(|_| ThreadId::new()).collect::<Vec<_>>();
        for index in 0..PAGE_COUNT {
            pages.push_back(ScriptedLineageResponse::Page(scripted_lineage_page(
                vec![scripted_lineage_thread(
                    &app.config,
                    descendant_thread_ids[index],
                    primary_thread_id,
                    1,
                )],
                (index + 1 < PAGE_COUNT).then(|| format!("page-{}", index + 1)),
            )));
        }
        let responses = Arc::new(Mutex::new(pages));
        let (mut app_server, requests, proxy) = start_recording_app_server_with_lineage(
            &app.config,
            None,
            Some(Arc::clone(&responses)),
            None,
        )
        .await?;

        let first = app.backfill_loaded_subagent_threads(&mut app_server).await;
        assert_eq!(
            first.status,
            crate::app::session_lifecycle::LoadedSubagentBackfillStatus::Paused
        );
        assert_eq!(take_backfill_counts(&requests).0, PAGE_COUNT - 2);
        assert_eq!(
            descendant_thread_ids
                .iter()
                .filter(|thread_id| app.agent_navigation.get(thread_id).is_some())
                .count(),
            PAGE_COUNT - 2
        );
        let second = app.backfill_loaded_subagent_threads(&mut app_server).await;
        assert!(second.completed);
        assert_eq!(take_backfill_counts(&requests).0, 2);
        assert!(
            descendant_thread_ids
                .iter()
                .all(|thread_id| app.agent_navigation.get(thread_id).is_some())
        );
        assert!(responses.lock().expect("lineage response lock").is_empty());

        app_server.shutdown().await?;
        proxy.await??;
        Ok(())
    })
}

#[test]
fn reset_clears_paused_lineage_continuation() -> Result<()> {
    run_large_stack_app_test(|| async {
        const PAGE_COUNT: usize = crate::app::loaded_threads::SUBAGENT_BACKFILL_PAGES_PER_ATTEMPT;
        let mut app = make_test_app().await;
        let primary_thread_id = ThreadId::new();
        configure_backfill_primary(&mut app, primary_thread_id);
        let responses = Arc::new(Mutex::new(VecDeque::from_iter((0..PAGE_COUNT).map(
            |index| {
                ScriptedLineageResponse::Page(scripted_lineage_page(
                    Vec::new(),
                    Some(format!("page-{}", index + 1)),
                ))
            },
        ))));
        let (mut app_server, _requests, proxy) = start_recording_app_server_with_lineage(
            &app.config,
            None,
            Some(Arc::clone(&responses)),
            None,
        )
        .await?;

        let result = app.backfill_loaded_subagent_threads(&mut app_server).await;
        assert_eq!(
            result.status,
            crate::app::session_lifecycle::LoadedSubagentBackfillStatus::Paused
        );
        assert!(app.subagent_backfill_progress.is_some());
        app.reset_thread_event_state();
        assert!(app.subagent_backfill_progress.is_none());

        app_server.shutdown().await?;
        proxy.await??;
        Ok(())
    })
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
                    start_recording_app_server(&app.config, /*blocked_thread_read_id*/ None)
                        .await?;
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
                    start_recording_app_server(&app.config, /*blocked_thread_read_id*/ None)
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
                assert!(matches!(
                    take_backfill_counts(&requests),
                    (0, 0, 0) | (0, 0, 1)
                ));

                app.start_fresh_session_with_summary_hint(
                    &mut tui,
                    &mut app_server,
                    /*session_start_source*/ None,
                    /*initial_user_message*/ None,
                    /*new_thread_name*/ None,
                )
                .await;

                assert_ne!(app.chat_widget.thread_id(), Some(root_thread_id));
                assert_eq!(take_backfill_counts(&requests), (0, 0, 0));
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
                assert_eq!(take_backfill_counts(&requests), (1, 0, 0));
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

                Box::pin(app.open_agent_picker(&mut app_server)).await;

                // The picker refreshes the primary thread once. Discovered children were already
                // refreshed by the picker's initial backfill and must not be read a second time.
                assert_eq!(take_backfill_counts(&requests), (1, 0, 1));
                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("session lifecycle request test thread")
}

#[test]
fn open_agent_picker_bounds_metadata_to_primary_lineage() -> Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;
    const UNRELATED_THREAD_COUNT: usize = 128;

    std::thread::Builder::new()
        .name("tui-agent-picker-lineage-bound".to_string())
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
                        "2026-01-01T00-00-00",
                        "2026-01-01T00:00:00Z",
                        "Primary thread",
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
                        "Descendant thread",
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
                let mut unrelated_thread_ids = Vec::with_capacity(UNRELATED_THREAD_COUNT);
                for index in 0..UNRELATED_THREAD_COUNT {
                    let minute = index / 60;
                    let second = index % 60;
                    let rollout_timestamp = format!("2026-01-02T00-{minute:02}-{second:02}");
                    let created_at = format!("2026-01-02T00:{minute:02}:{second:02}Z");
                    unrelated_thread_ids.push(ThreadId::from_string(
                        &create_fake_rollout(
                            codex_home.path(),
                            &rollout_timestamp,
                            &created_at,
                            "Unrelated thread",
                            Some(app.config.model_provider_id.as_str()),
                            /*git_info*/ None,
                        )
                        .expect("create unrelated rollout"),
                    )?);
                }

                let blocked_thread_read_id = unrelated_thread_ids[0];
                let (mut app_server, requests, proxy) =
                    start_recording_app_server(&app.config, Some(blocked_thread_read_id)).await?;
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
                for thread_id in unrelated_thread_ids {
                    app_server
                        .resume_thread(app.config.clone(), thread_id, app.resume_model_settings())
                        .await?;
                }
                let mut tui = crate::tui::test_support::make_test_tui()?;
                take_recorded_requests(&requests);

                let picker_result = tokio::time::timeout(
                    Duration::from_secs(10),
                    Box::pin(app.handle_event(
                        &mut tui,
                        &mut app_server,
                        AppEvent::OpenAgentPicker,
                    )),
                )
                .await;
                let control = match picker_result {
                    Ok(result) => result?,
                    Err(_) => {
                        proxy.abort();
                        return Err(color_eyre::eyre::eyre!(
                            "agent picker waited for an unrelated thread/read"
                        ));
                    }
                };

                assert!(matches!(control, AppRunControl::Continue));
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

                let recorded = take_recorded_requests(&requests);
                let lineage_requests: Vec<_> = recorded
                    .iter()
                    .filter(|request| request.method == "thread/list")
                    .collect();
                assert_eq!(lineage_requests.len(), 1);
                assert_eq!(
                    lineage_requests[0]
                        .params
                        .as_ref()
                        .and_then(|params| params.get("ancestorThreadId"))
                        .and_then(serde_json::Value::as_str),
                    Some(root_thread_id.to_string().as_str())
                );
                assert_eq!(
                    lineage_requests[0]
                        .params
                        .as_ref()
                        .and_then(|params| params.get("limit"))
                        .and_then(serde_json::Value::as_u64),
                    Some(u64::from(
                        crate::app::session_lifecycle::SUBAGENT_BACKFILL_PAGE_SIZE,
                    ))
                );
                assert_eq!(
                    lineage_requests[0]
                        .params
                        .as_ref()
                        .and_then(|params| params.get("useStateDbOnly"))
                        .and_then(serde_json::Value::as_bool),
                    Some(true)
                );
                let read_thread_ids: Vec<_> = recorded
                    .iter()
                    .filter(|request| request.method == "thread/read")
                    .filter_map(|request| request.params.as_ref())
                    .filter_map(|params| params.get("threadId"))
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect();
                assert_eq!(read_thread_ids, vec![root_thread_id.to_string()]);
                assert!(!read_thread_ids.contains(&blocked_thread_read_id.to_string()));

                app_server.shutdown().await?;
                proxy.await??;
                Ok(())
            })
        })?
        .join()
        .expect("agent picker lineage bound test thread")
}
