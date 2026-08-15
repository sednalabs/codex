use crate::StartThreadOptions;
use crate::ThreadManager;
use crate::agent::AgentControl;
use crate::agent::AgentStatus;
use crate::agent::registry::AgentMetadata;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use crate::config::test_config;
use crate::init_state_db;
use crate::thread_manager::ThreadManagerState;
use crate::thread_manager::V2ThreadUnloadResult;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::yield_now;
use tokio::time::advance;

#[tokio::test]
async fn terminal_idle_unload_preserves_fifo_mail_and_reloads_cold_agent() {
    let (_home, config, manager, control, first, metadata) = terminal_idle_test_agent(
        /*timeout_ms*/ 25, /*ephemeral*/ false, /*sqlite*/ true,
    )
    .await;
    let first_message = test_communication("first queued message", /*trigger_turn*/ false);
    let second_message = test_communication("second queued message", /*trigger_turn*/ false);
    first
        .thread
        .session
        .input_queue
        .enqueue_mailbox_communications(vec![first_message.clone(), second_message.clone()])
        .await;

    mark_thread_status(
        first.thread.as_ref(),
        AgentStatus::Completed(Some("done".to_string())),
    )
    .await;
    wait_for_thread_unloaded(&manager, first.thread_id).await;
    assert_eq!(
        control.get_status(first.thread_id).await,
        AgentStatus::Completed(Some("done".to_string()))
    );
    assert_eq!(metadata.lifecycle.lock().await.cold_mail_len(), 2);

    let stale_injection = first
        .thread
        .inject_response_items(vec![ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "stale injection".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }])
        .await
        .expect_err("an unloaded thread Arc must reject history injection");
    assert_matches::assert_matches!(
        stale_injection.details(),
        CodexErrorDetails::InternalAgentDied
    );

    let injected_item = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "injected after idle unload".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    manager
        .inject_response_items(config, first.thread_id, vec![injected_item.clone()])
        .await
        .expect("manager injection should reload the cold agent");
    let reloaded = manager
        .get_thread(first.thread_id)
        .await
        .expect("reloaded thread should be resident");
    let reloaded_history = reloaded.session.clone_history().await;
    assert!(reloaded_history.raw_items().iter().any(|item| {
        matches!(
            item,
            ResponseItem::Message {
                role,
                content,
                ..
            } if role == "assistant"
                && matches!(
                    content.as_slice(),
                    [ContentItem::OutputText { text }]
                        if text == "injected after idle unload"
                )
        )
    }));
    assert_eq!(
        reloaded
            .session
            .input_queue
            .drain_mailbox_communications()
            .await,
        vec![first_message, second_message]
    );
    assert_eq!(
        control.state.cold_status(first.thread_id, Some(&reloaded)),
        None
    );

    mark_thread_status(
        reloaded.as_ref(),
        AgentStatus::Completed(Some("reloaded turn complete".to_string())),
    )
    .await;
    wait_for_thread_unloaded(&manager, first.thread_id).await;
    assert_eq!(
        control.get_status(first.thread_id).await,
        AgentStatus::Completed(Some("reloaded turn complete".to_string()))
    );
}

#[tokio::test]
async fn manager_injection_into_loaded_v2_root_does_not_require_agent_metadata() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
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
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start V2 root thread");
    let control = root.thread.session.services.agent_control.clone();
    control.state.release_spawned_thread(root.thread_id);
    assert!(
        control
            .state
            .agent_metadata_for_thread(root.thread_id)
            .is_none()
    );
    let injected_item = assistant_output("root injection");

    manager
        .inject_response_items(config, root.thread_id, vec![injected_item.clone()])
        .await
        .expect("loaded V2 root injection should not require subagent metadata");

    wait_for_history_item(root.thread.as_ref(), &injected_item).await;
    assert_eq!(
        root.thread
            .session
            .clone_history()
            .await
            .raw_items()
            .iter()
            .filter(|item| *item == &injected_item)
            .count(),
        1,
        "taskless root injection should be recorded exactly once"
    );
}

#[tokio::test]
async fn external_v2_unload_preserves_cold_delivery_and_releases_capacity() {
    let (_home, mut config, manager, control, first, metadata) = terminal_idle_test_agent(
        /*timeout_ms*/ 0, /*ephemeral*/ false, /*sqlite*/ true,
    )
    .await;
    config.multi_agent_v2.max_concurrent_threads_per_session = 1;
    let queued_message = test_communication(
        "queued while externally unloaded",
        /*trigger_turn*/ false,
    );
    first
        .thread
        .session
        .input_queue
        .enqueue_mailbox_communications(vec![queued_message.clone()])
        .await;
    mark_thread_status(
        first.thread.as_ref(),
        AgentStatus::Completed(Some("done".to_string())),
    )
    .await;

    assert_eq!(
        manager
            .unload_v2_thread_for_external_teardown(&first.thread)
            .await,
        V2ThreadUnloadResult::Unloaded
    );
    assert!(manager.get_thread(first.thread_id).await.is_err());
    assert_eq!(
        control.get_status(first.thread_id).await,
        AgentStatus::Completed(Some("done".to_string()))
    );
    assert_eq!(metadata.lifecycle.lock().await.cold_mail_len(), 1);
    assert_eq!(control.v2_residency.resident_count(), 0);

    control
        .send_inter_agent_communication(
            first.thread_id,
            test_communication("queue-only cold delivery", /*trigger_turn*/ false),
            AgentCommunicationContext::new(AgentCommunicationKind::Message, ThreadId::new()),
        )
        .await
        .expect("queue-only delivery should not reload a cold V2 agent");
    assert!(manager.get_thread(first.thread_id).await.is_err());
    assert_eq!(metadata.lifecycle.lock().await.cold_mail_len(), 2);

    manager
        .inject_response_items(
            config,
            first.thread_id,
            vec![assistant_output("reload after external unload")],
        )
        .await
        .expect("injection should reload the cold V2 agent through one capacity slot");
    let reloaded = manager
        .get_thread(first.thread_id)
        .await
        .expect("injection should reload the externally unloaded agent");
    assert_eq!(control.v2_residency.resident_count(), 1);
    assert!(Arc::ptr_eq(
        &reloaded,
        &manager
            .get_thread(first.thread_id)
            .await
            .expect("reload should retain one resident thread")
    ));
    assert_eq!(metadata.lifecycle.lock().await.cold_mail_len(), 0);
}

#[tokio::test]
async fn external_v2_unload_defers_for_pending_finalizers_and_submissions() {
    let (_home, _config, manager, control, first, _metadata) = terminal_idle_test_agent(
        /*timeout_ms*/ 0, /*ephemeral*/ false, /*sqlite*/ false,
    )
    .await;
    mark_thread_status(
        first.thread.as_ref(),
        AgentStatus::Completed(Some("done".to_string())),
    )
    .await;
    first
        .thread
        .session
        .input_queue
        .register_terminal_finalizer();
    first
        .thread
        .session
        .input_queue
        .register_residency_submission("external-unload".to_string());

    assert_eq!(
        manager
            .unload_v2_thread_for_external_teardown(&first.thread)
            .await,
        V2ThreadUnloadResult::Deferred
    );
    assert!(manager.get_thread(first.thread_id).await.is_ok());
    assert_eq!(control.v2_residency.resident_count(), 1);

    first.thread.session.input_queue.finish_terminal_finalizer();
    assert_eq!(
        manager
            .unload_v2_thread_for_external_teardown(&first.thread)
            .await,
        V2ThreadUnloadResult::Deferred
    );
    first
        .thread
        .session
        .input_queue
        .finish_residency_submission("external-unload");
    assert_eq!(
        manager
            .unload_v2_thread_for_external_teardown(&first.thread)
            .await,
        V2ThreadUnloadResult::Unloaded
    );
    assert!(manager.get_thread(first.thread_id).await.is_err());
    assert_eq!(control.v2_residency.resident_count(), 0);
}

#[tokio::test]
async fn terminal_idle_unload_drops_stale_residency_when_manager_entry_is_missing() {
    let (_home, _config, manager, control, first, _metadata) = terminal_idle_test_agent(
        /*timeout_ms*/ 25, /*ephemeral*/ false, /*sqlite*/ false,
    )
    .await;
    mark_thread_status(
        first.thread.as_ref(),
        AgentStatus::Completed(Some("done".to_string())),
    )
    .await;
    manager
        .remove_thread(&first.thread_id)
        .await
        .expect("manager should still own the resident before the simulated external removal");

    for _ in 0..200 {
        if control.v2_residency.resident_count() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("terminal idle watcher should release a resident missing from the manager");
}

#[tokio::test]
async fn external_v2_unload_leaves_root_teardown_to_the_app_server() {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
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
        .start_thread(StartThreadOptions::new(config))
        .await
        .expect("start V2 root thread");

    assert_eq!(
        manager
            .unload_v2_thread_for_external_teardown(&root.thread)
            .await,
        V2ThreadUnloadResult::NotApplicable
    );
    assert!(manager.get_thread(root.thread_id).await.is_ok());
}

#[tokio::test]
async fn loaded_v1_agent_metadata_does_not_use_v2_lifecycle() {
    let mut config = test_config().await;
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
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start V1 root thread");
    let control = root.thread.session.services.agent_control.clone();
    let state = control.upgrade().expect("thread manager should be live");
    let child = spawn_v2_subagent(&control, &state, config, root.thread_id, "v1-worker").await;
    control
        .state
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve registry slot")
        .commit(AgentMetadata {
            agent_id: Some(child.thread_id),
            ..Default::default()
        });

    assert!(!control.uses_v2_lifecycle(&state, child.thread_id).await);
}

#[tokio::test]
async fn oversized_injection_is_rejected_without_reloading_cold_agent() {
    let (_home, config, manager, control, first, _metadata) = terminal_idle_test_agent(
        /*timeout_ms*/ 25, /*ephemeral*/ false, /*sqlite*/ true,
    )
    .await;
    mark_thread_status(
        first.thread.as_ref(),
        AgentStatus::Completed(Some("done".to_string())),
    )
    .await;
    wait_for_thread_unloaded(&manager, first.thread_id).await;
    let cold_status = control
        .state
        .cold_status(first.thread_id, /*live_thread*/ None);

    let error = manager
        .inject_response_items(
            config,
            first.thread_id,
            vec![assistant_output(&"x".repeat(40_001))],
        )
        .await
        .expect_err("oversized cold injection should be rejected");

    assert_matches::assert_matches!(
        error.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message == "items[0] must not exceed 10000 estimated model-visible tokens"
    );
    assert!(manager.get_thread(first.thread_id).await.is_err());
    assert_eq!(
        control
            .state
            .cold_status(first.thread_id, /*live_thread*/ None),
        cold_status
    );
}

#[tokio::test]
async fn prepared_v2_delivery_rejects_current_but_shutdown_runtime() {
    let (_home, config, manager, control, first, _metadata) = terminal_idle_test_agent(
        /*timeout_ms*/ 0, /*ephemeral*/ false, /*sqlite*/ false,
    )
    .await;
    first
        .thread
        .shutdown_and_wait()
        .await
        .expect("shut down runtime while manager still owns its Arc");
    let context = AgentCommunicationContext::new(AgentCommunicationKind::Message, ThreadId::new());

    let submission_error = control
        .prepare_v2_agent_delivery(first.thread_id)
        .await
        .expect("registered shutdown runtime remains addressable")
        .send(
            test_communication(
                "must not reach shutdown runtime",
                /*trigger_turn*/ false,
            ),
            context,
            /*interrupt*/ false,
        )
        .await
        .expect_err("submission to shutdown runtime should fail");
    assert_matches::assert_matches!(
        submission_error.details(),
        CodexErrorDetails::InternalAgentDied
    );

    let injection_error = manager
        .inject_response_items(
            config,
            first.thread_id,
            vec![assistant_output("must not enter shutdown runtime history")],
        )
        .await
        .expect_err("app-server manager injection into shutdown runtime should fail");
    assert_matches::assert_matches!(
        injection_error.details(),
        CodexErrorDetails::InternalAgentDied
    );
}

#[tokio::test(start_paused = true)]
async fn terminal_idle_unload_timeout_zero_disables_unload() {
    let (_home, _config, manager, _control, first, _metadata) = terminal_idle_test_agent(
        /*timeout_ms*/ 0, /*ephemeral*/ false, /*sqlite*/ false,
    )
    .await;
    mark_thread_status(first.thread.as_ref(), AgentStatus::Interrupted).await;

    advance(Duration::from_secs(3_600)).await;
    yield_now().await;
    let resident = manager
        .get_thread(first.thread_id)
        .await
        .expect("zero timeout should keep the runtime resident");
    assert!(Arc::ptr_eq(&resident, &first.thread));
}

#[tokio::test(start_paused = true)]
async fn terminal_idle_unload_is_invalidated_by_new_user_work() {
    let (_home, _config, manager, control, first, _metadata) = terminal_idle_test_agent(
        /*timeout_ms*/ 100, /*ephemeral*/ false, /*sqlite*/ false,
    )
    .await;
    mark_thread_status(
        first.thread.as_ref(),
        AgentStatus::Completed(Some("first turn complete".to_string())),
    )
    .await;
    yield_now().await;
    advance(Duration::from_millis(50)).await;

    control
        .send_input(
            first.thread_id,
            vec![UserInput::Text {
                text: "new user work".to_string(),
                text_elements: Vec::new(),
            }],
        )
        .await
        .expect("new user work should be accepted");
    advance(Duration::from_millis(50)).await;
    yield_now().await;

    let resident = manager
        .get_thread(first.thread_id)
        .await
        .expect("new user work should invalidate idle unload");
    assert!(Arc::ptr_eq(&resident, &first.thread));
}

#[tokio::test(start_paused = true)]
async fn terminal_idle_unload_failure_preserves_trigger_mail_and_residency() {
    let (_home, _config, manager, _control, first, _metadata) = terminal_idle_test_agent(
        /*timeout_ms*/ 100, /*ephemeral*/ false, /*sqlite*/ false,
    )
    .await;
    let queued = vec![
        test_communication("queue-only", /*trigger_turn*/ false),
        test_communication("trigger work", /*trigger_turn*/ true),
    ];
    first
        .thread
        .session
        .input_queue
        .enqueue_mailbox_communications(queued.clone())
        .await;
    mark_thread_status(
        first.thread.as_ref(),
        AgentStatus::Errored("terminal failure".to_string()),
    )
    .await;
    yield_now().await;

    advance(Duration::from_millis(100)).await;
    yield_now().await;
    let resident = manager
        .get_thread(first.thread_id)
        .await
        .expect("pending trigger work should keep the runtime resident");
    assert!(Arc::ptr_eq(&resident, &first.thread));
    assert_eq!(
        resident
            .session
            .input_queue
            .drain_mailbox_communications()
            .await,
        queued
    );
}

async fn terminal_idle_test_agent(
    timeout_ms: u64,
    ephemeral: bool,
    sqlite: bool,
) -> (
    tempfile::TempDir,
    Config,
    ThreadManager,
    AgentControl,
    crate::thread_manager::NewThread,
    AgentMetadata,
) {
    let mut config = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    config.multi_agent_v2.terminal_idle_unload_timeout_ms = timeout_ms;
    config.ephemeral = ephemeral;
    let temp_home = tempfile::tempdir().expect("create temp home");
    config.codex_home = temp_home.path().to_path_buf().try_into().unwrap();
    config.cwd = temp_home.path().to_path_buf().try_into().unwrap();
    let state_db = if sqlite {
        let _ = config.features.enable(Feature::Sqlite);
        Some(
            init_state_db(&config)
                .await
                .expect("sqlite state db should initialize"),
        )
    } else {
        None
    };
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        state_db,
    );
    let root = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start root thread");
    let control = root.thread.session.services.agent_control.clone();
    let state = control.upgrade().expect("thread manager should be live");
    let residency_slot = control
        .reserve_v2_residency_slot(&state, &config, /*protected_thread_id*/ None)
        .await
        .expect("reserve resident slot");
    let first =
        spawn_v2_subagent(&control, &state, config.clone(), root.thread_id, "worker-1").await;
    residency_slot.commit(first.thread_id);
    control
        .state
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve registry slot")
        .commit(AgentMetadata {
            agent_id: Some(first.thread_id),
            ..Default::default()
        });
    let metadata = control
        .state
        .agent_metadata_for_thread(first.thread_id)
        .expect("registered metadata");
    control.start_terminal_idle_unload_watcher(
        Arc::clone(&first.thread),
        metadata.clone(),
        timeout_ms,
    );
    (temp_home, config, manager, control, first, metadata)
}

fn test_communication(text: &str, trigger_turn: bool) -> InterAgentCommunication {
    InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root(),
        Vec::new(),
        text.to_string(),
        trigger_turn,
    )
}

fn assistant_output(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

async fn wait_for_thread_unloaded(manager: &ThreadManager, thread_id: ThreadId) {
    for _ in 0..200 {
        if manager.get_thread(thread_id).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("thread {thread_id} should be unloaded");
}

async fn wait_for_history_item(thread: &CodexThread, expected_item: &ResponseItem) {
    for _ in 0..200 {
        if thread
            .session
            .clone_history()
            .await
            .raw_items()
            .contains(expected_item)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("injected response item should be recorded in thread history");
}

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
        .start_thread(StartThreadOptions::new(config.clone()))
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
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(thread_id) => assert_eq!(*thread_id, first.thread_id),
            _ => panic!("expected evicted thread to be missing, got {err:?}"),
        },
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
        .start_thread(StartThreadOptions::new(config.clone()))
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
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(thread_id) => assert_eq!(*thread_id, first.thread_id),
            _ => panic!("expected evicted thread to be missing, got {err:?}"),
        },
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
        .start_thread(StartThreadOptions::new(config.clone()))
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
    let CodexErrorDetails::AgentLimitReached { max_threads } = err.details() else {
        panic!("expected AgentLimitReached, got {err:?}");
    };
    assert_eq!(*max_threads, 1);
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
            provider_usage: None,
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
            provider_usage: None,
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
