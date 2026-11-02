use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_mock_responses_server_repeating_assistant;
use codex_app_server_protocol::ThreadGoalGetResponse;
use codex_app_server_protocol::ThreadGoalSetResponse;
use codex_app_server_protocol::ThreadGoalStatus;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::TurnExternalNoticeResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use codex_protocol::ThreadId;
use core_test_support::responses;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn independent_notice_starts_with_paused_goal() -> Result<()> {
    let gateway = create_mock_responses_server_repeating_assistant("Done").await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&gateway.uri()).enable_feature(Feature::Goals).write(home.path())?;
    let mut server = TestAppServer::builder().with_codex_home(home.path()).build_initialized().await?;
    let thread = server.start_thread(ThreadStartParams::default()).await?.thread;
    let notice = json!({"threadId": thread.id, "source": "irc", "notice": "a retained batch is ready"});
    let id = server.send_raw_request("thread/goal/set", Some(json!({
        "threadId": thread.id, "objective": "Preserve the paused goal", "status": "paused"
    }))).await?;
    let _: ThreadGoalSetResponse = server.read_response(id).await?;
    let id = server.send_raw_request("turn/externalNotice", Some(notice)).await?;
    let receipt: TurnExternalNoticeResponse = server.read_response(id).await?;
    assert!(receipt.turn_id.is_some());
    let id = server.send_raw_request("thread/goal/get", Some(json!({"threadId": thread.id}))).await?;
    let goal: ThreadGoalGetResponse = server.read_response(id).await?;
    assert_eq!(goal.goal.expect("paused goal").status, ThreadGoalStatus::Paused);
    Ok(())
}

#[tokio::test]
async fn automatic_notice_steers_existing_turn_with_exact_receipt() -> Result<()> {
    let (release, gate) = oneshot::channel();
    let (gateway, _) = start_streaming_sse_server(vec![vec![
        StreamingSseChunk { gate: None, body: responses::sse(vec![responses::ev_response_created("response-1")]) },
        StreamingSseChunk { gate: Some(gate), body: responses::sse(vec![responses::ev_completed("response-1")]) },
    ]]).await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(gateway.uri()).enable_feature(Feature::Goals).write(home.path())?;
    let mut server = TestAppServer::builder().with_codex_home(home.path()).build_initialized().await?;
    let thread = server.start_thread(ThreadStartParams::default()).await?.thread;
    let id = server.send_turn_start_request(TurnStartParams {
        thread_id: thread.id.clone(),
        input: vec![UserInput::Text { text: "Continue".to_string(), text_elements: Vec::new() }],
        ..Default::default()
    }).await?;
    let started: TurnStartResponse = timeout(Duration::from_secs(60), server.read_response(id)).await??;
    timeout(Duration::from_secs(60), gateway.wait_for_request_count(1)).await?;
    let id = server.send_raw_request("turn/externalNotice", Some(json!({
        "threadId": thread.id, "source": "irc", "notice": "a retained batch is ready"
    }))).await?;
    let receipt: TurnExternalNoticeResponse = timeout(Duration::from_secs(60), server.read_response(id)).await??;
    assert_eq!(receipt.turn_id.as_deref(), Some(started.turn.id.as_str()));
    let _ = release.send(());
    Ok(())
}

#[tokio::test]
async fn automatic_notice_starts_idle_thread_with_active_goal() -> Result<()> {
    let gateway = create_mock_responses_server_repeating_assistant("Done").await;
    let home = TempDir::new()?;
    MockResponsesConfig::new(&gateway.uri()).enable_feature(Feature::Goals).write(home.path())?;
    let mut server = TestAppServer::builder().with_codex_home(home.path()).build_initialized().await?;
    let thread = server.start_thread(ThreadStartParams::default()).await?.thread;
    // Seed the goal directly so the normal goal-set operation cannot itself
    // start a turn before the notice under test arrives.
    let sqlite = codex_state::SqliteConfig::new_for_testing(
        codex_utils_absolute_path::AbsolutePathBuf::try_from(home.path())?,
    );
    let state = codex_state::StateRuntime::init(sqlite, "mock_provider".to_string()).await?;
    state.thread_goals().replace_thread_goal(
        ThreadId::from_string(&thread.id)?, "Resume useful work", codex_state::ThreadGoalStatus::Active, None,
    ).await?;
    let id = server.send_raw_request("thread/read", Some(json!({
        "threadId": thread.id, "includeTurns": false
    }))).await?;
    let before: ThreadReadResponse = server.read_response(id).await?;
    assert_eq!(before.thread.status, ThreadStatus::Idle);
    let id = server.send_raw_request("turn/externalNotice", Some(json!({
        "threadId": thread.id, "source": "irc", "notice": "a retained batch is ready"
    }))).await?;
    let receipt: TurnExternalNoticeResponse = timeout(Duration::from_secs(60), server.read_response(id)).await??;
    assert!(receipt.turn_id.is_some());
    Ok(())
}
