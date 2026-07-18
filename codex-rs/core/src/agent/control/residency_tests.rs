use crate::ThreadManager;
use crate::agent::AgentControl;
use crate::agent::AgentStatus;
use crate::agent::registry::AgentMetadata;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use crate::config::test_config;
use crate::init_state_db;
use crate::thread_manager::ThreadManagerState;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use pretty_assertions::assert_eq;
use std::sync::Arc;

#[tokio::test]
async fn residency_slot_reservation_unloads_oldest_idle_v2_agent() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control.upgrade().expect("thread manager should be live");

    let first_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("first resident slot");
    let first =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-1").await;
    first_slot.commit(first.thread_id);
    let registry = Arc::clone(&control.state);
    registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve first registry slot")
        .commit(AgentMetadata {
            agent_id: Some(first.thread_id),
            ..Default::default()
        });
    let stale_metadata = registry
        .agent_metadata_for_thread(first.thread_id)
        .expect("first metadata");
    registry.release_spawned_thread(first.thread_id);
    registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve replacement registry slot")
        .commit(AgentMetadata {
            agent_id: Some(first.thread_id),
            ..Default::default()
        });
    registry.publish_cold_status_if_current(
        first.thread_id,
        &stale_metadata,
        &first.thread,
        AgentStatus::Completed(Some("stale generation".to_string())),
    );
    assert_eq!(
        registry.cold_status(first.thread_id, /*live_thread*/ None),
        None
    );

    let error_message = "\u{00e9}".repeat(3000);
    mark_thread_status(first.thread.as_ref(), AgentStatus::Errored(error_message)).await;

    let second_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("second resident slot should evict the first idle agent");
    match manager.get_thread(first.thread_id).await {
        Err(CodexErr::ThreadNotFound(thread_id)) => assert_eq!(thread_id, first.thread_id),
        Err(err) => panic!("expected evicted thread to be missing, got {err:?}"),
        Ok(_) => panic!("expected evicted thread to be missing"),
    }
    let expected_status = AgentStatus::Errored(format!("{}...[truncated]", "\u{00e9}".repeat(57)));
    assert_eq!(control.get_status(first.thread_id).await, expected_status);
    let status_rx = control
        .subscribe_status(first.thread_id)
        .await
        .expect("subscribe to evicted status");
    assert_eq!(status_rx.borrow().clone(), expected_status);
    let listed_agents = control
        .list_agents(&SessionSource::Cli, /*path_prefix*/ None)
        .await
        .expect("list agents with cold identity");
    assert_eq!(
        listed_agents
            .iter()
            .find(|agent| agent.agent_name == first.thread_id.to_string())
            .map(|agent| agent.agent_status.clone()),
        Some(expected_status)
    );
    let second = spawn_v2_subagent(&control, &state, config, root.thread_id, "worker-2").await;
    second_slot.commit(second.thread_id);

    assert!(manager.get_thread(root.thread_id).await.is_ok());
    assert!(manager.get_thread(second.thread_id).await.is_ok());
}

#[tokio::test]
async fn interrupted_v2_agent_remains_known_and_reloads_after_residency_eviction() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let _ = config.features.enable(Feature::Sqlite);
    let state_db = init_state_db(&config)
        .await
        .expect("sqlite state db should initialize");
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Some(state_db.clone()),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("start root thread");
    let control = manager.agent_control();
    let state = control.upgrade().expect("thread manager should be live");

    let first_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("first resident slot");
    let first =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-1").await;
    first_slot.commit(first.thread_id);
    control
        .state
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve first registry slot")
        .commit(AgentMetadata {
            agent_id: Some(first.thread_id),
            ..Default::default()
        });
    mark_thread_status(first.thread.as_ref(), AgentStatus::Interrupted).await;
    let stored_metadata = state_db
        .get_thread(first.thread_id)
        .await
        .expect("read indexed first-agent metadata")
        .expect("first-agent metadata should be indexed");
    assert_eq!(stored_metadata.model.as_deref(), Some("gpt-5.5"));

    let second_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("second resident slot should evict the first interrupted idle agent");
    match manager.get_thread(first.thread_id).await {
        Err(CodexErr::ThreadNotFound(thread_id)) => assert_eq!(thread_id, first.thread_id),
        Err(err) => panic!("expected evicted thread to be missing, got {err:?}"),
        Ok(_) => panic!("expected evicted thread to be missing"),
    }
    assert_eq!(
        control.get_status(first.thread_id).await,
        AgentStatus::Interrupted
    );
    let status_rx = control
        .subscribe_status(first.thread_id)
        .await
        .expect("subscribe to evicted interrupted status");
    assert_eq!(status_rx.borrow().clone(), AgentStatus::Interrupted);
    let listed_agents = control
        .list_agents(&SessionSource::Cli, /*path_prefix*/ None)
        .await
        .expect("list agents with cold interrupted identity");
    assert_eq!(
        listed_agents
            .iter()
            .find(|agent| agent.agent_name == first.thread_id.to_string())
            .map(|agent| agent.agent_status.clone()),
        Some(AgentStatus::Interrupted)
    );
    let second =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-2").await;
    second_slot.commit(second.thread_id);
    mark_thread_status(
        second.thread.as_ref(),
        AgentStatus::Completed(Some("done".to_string())),
    )
    .await;

    control
        .ensure_v2_agent_loaded(config, first.thread_id)
        .await
        .expect("evicted interrupted agent should reload");
    let reloaded = manager
        .get_thread(first.thread_id)
        .await
        .expect("reloaded interrupted agent");
    assert_eq!(
        control.state.cold_status(first.thread_id, Some(&reloaded)),
        None
    );

    assert!(manager.get_thread(root.thread_id).await.is_ok());
    assert!(manager.get_thread(first.thread_id).await.is_ok());
}

#[tokio::test]
async fn ephemeral_v2_agent_is_not_evicted_without_reloadable_history() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    config.ephemeral = true;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("start ephemeral root thread");
    let control = manager.agent_control();
    let state = control.upgrade().expect("thread manager should be live");

    let first_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("first resident slot");
    let first = spawn_v2_subagent(
        &control,
        &state,
        config.clone(),
        root.thread_id,
        "ephemeral-worker",
    )
    .await;
    first_slot.commit(first.thread_id);
    mark_thread_status(first.thread.as_ref(), AgentStatus::Interrupted).await;

    let err = match control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
    {
        Ok(_) => {
            panic!("ephemeral resident must not be evicted into an unreloadable cold state")
        }
        Err(err) => err,
    };
    let CodexErr::AgentLimitReached { max_threads } = err else {
        panic!("expected AgentLimitReached, got {err:?}");
    };
    assert_eq!(max_threads, 1);
    let still_loaded = manager
        .get_thread(first.thread_id)
        .await
        .expect("ephemeral resident should remain loaded");
    assert!(Arc::ptr_eq(&still_loaded, &first.thread));
    assert_eq!(still_loaded.agent_status().await, AgentStatus::Interrupted);
    assert_eq!(
        control
            .state
            .cold_status(first.thread_id, Some(&still_loaded)),
        None
    );
}

async fn spawn_v2_subagent(
    control: &AgentControl,
    state: &Arc<ThreadManagerState>,
    config: Config,
    parent_thread_id: ThreadId,
    label: &str,
) -> crate::thread_manager::NewThread {
    state
        .spawn_new_thread_with_source(
            config,
            control.clone(),
            SessionSource::SubAgent(SubAgentSource::Other(label.to_string())),
            /*history_mode*/ None,
            Some(parent_thread_id),
            /*forked_from_thread_id*/ None,
            Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
        )
        .await
        .expect("spawn v2 subagent")
}

async fn mark_thread_status(thread: &CodexThread, status: AgentStatus) {
    let turn = thread.session.new_default_turn().await;
    thread
        .session
        .persist_rollout_items(&[RolloutItem::TurnContext(turn.to_turn_context_item())])
        .await;
    let event = match status {
        AgentStatus::Completed(last_agent_message) => EventMsg::TurnComplete(TurnCompleteEvent {
            turn_id: turn.sub_id.clone(),
            started_at: None,
            last_agent_message,
            final_model: None,
            model_snapshot: None,
            error: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
            compaction_events_in_turn: 0,
        }),
        AgentStatus::Errored(message) => EventMsg::Error(ErrorEvent {
            message,
            codex_error_info: None,
        }),
        AgentStatus::Interrupted => EventMsg::TurnAborted(TurnAbortedEvent {
            turn_id: Some(turn.sub_id.clone()),
            started_at: None,
            reason: TurnAbortReason::Interrupted,
            completed_at: None,
            duration_ms: None,
        }),
        status => panic!("unsupported fixture status: {status:?}"),
    };
    thread.session.send_event(turn.as_ref(), event).await;
    clear_active_turn(thread).await;
}

async fn clear_active_turn(thread: &CodexThread) {
    // The fixture has no task runner to clear the turn after the terminal event.
    *thread.session.active_turn.lock().await = None;
}
