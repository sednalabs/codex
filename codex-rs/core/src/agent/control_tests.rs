use super::*;
use crate::CodexThread;
use crate::StateDbHandle;
use crate::ThreadManager;
use crate::agent::agent_status_from_event;
use crate::agent::lifecycle::ColdMailboxItem;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::config::AgentRoleConfig;
use crate::config::Config;
use crate::config::ConfigBuilder;
use crate::context::ContextualUserFragment;
use crate::context::SubagentNotification;
use crate::init_state_db;
use crate::thread_manager::RemoveThreadIfSameResult;
use crate::thread_manager::StartThreadOptions;
use assert_matches::assert_matches;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::empty_extension_registry;
use codex_features::Feature;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::capabilities::CapabilityRootLocation;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_thread_store::ArchiveThreadParams;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::LocalThreadStoreConfig;
use codex_thread_store::ThreadStore;
use codex_utils_path_uri::PathUri;
use core_test_support::responses::strip_response_item_ids;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::sleep;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use toml::Value as TomlValue;

async fn test_config_with_cli_overrides(
    mut cli_overrides: Vec<(String, TomlValue)>,
) -> (TempDir, Config) {
    let home = TempDir::new().expect("create temp dir");
    cli_overrides.push((
        "model".to_string(),
        TomlValue::String("gpt-5.5".to_string()),
    ));
    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .cli_overrides(cli_overrides)
        .build()
        .await
        .expect("load default test config");
    (home, config)
}

async fn test_config() -> (TempDir, Config) {
    test_config_with_cli_overrides(Vec::new()).await
}

fn text_input(text: &str) -> Vec<UserInput> {
    vec![UserInput::Text {
        text: text.to_string(),
        text_elements: Vec::new(),
    }]
}

fn assistant_message(text: &str, phase: Option<MessagePhase>) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn register_session_root_skips_threads_with_explicit_parent() {
    let control = AgentControl::default();

    control.register_session_root(ThreadId::new(), Some(ThreadId::new()));

    assert_eq!(control.state.agent_id_for_path(&AgentPath::root()), None);
}

fn spawn_agent_call(call_id: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: "spawn_agent".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

struct AgentControlHarness {
    _home: TempDir,
    config: Config,
    state_db: Option<StateDbHandle>,
    manager: ThreadManager,
    control: AgentControl,
}

impl AgentControlHarness {
    async fn new() -> Self {
        let (home, config) = test_config().await;
        Self::new_with_config(home, config).await
    }

    async fn new_with_config(home: TempDir, config: Config) -> Self {
        let state_db = init_state_db(&config).await;
        Self::new_with_config_and_state_db(home, config, state_db)
    }

    fn new_with_config_and_state_db(
        home: TempDir,
        config: Config,
        state_db: Option<StateDbHandle>,
    ) -> Self {
        let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
            CodexAuth::from_api_key("dummy"),
            config.model_provider.clone(),
            config.codex_home.to_path_buf(),
            std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
            state_db.clone(),
        );
        let control = manager.agent_control();
        Self {
            _home: home,
            config,
            state_db,
            manager,
            control,
        }
    }

    async fn start_thread(&self) -> (ThreadId, Arc<CodexThread>) {
        let new_thread = self
            .manager
            .start_thread(StartThreadOptions::new(self.config.clone()))
            .await
            .expect("start thread");
        (new_thread.thread_id, new_thread.thread)
    }

    async fn start_paginated_thread(&self) -> (ThreadId, Arc<CodexThread>) {
        let new_thread = self
            .manager
            .start_thread(StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Paginated),
                environments: Some(Vec::new()),
                ..StartThreadOptions::new(self.config.clone())
            })
            .await
            .expect("start paginated thread");
        (new_thread.thread_id, new_thread.thread)
    }

    async fn spawn_anonymous_child(
        &self,
        parent_thread_id: ThreadId,
        options: SpawnAgentOptions,
    ) -> ThreadId {
        self.control
            .spawn_agent_with_metadata(
                self.config.clone(),
                text_input("child task"),
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    depth: 1,
                    agent_path: None,
                    agent_nickname: None,
                    agent_role: None,
                })),
                options,
            )
            .await
            .expect("child spawn should succeed")
            .thread_id
    }
}

#[tokio::test]
async fn spawn_agent_outcome_preserves_committed_child_when_initial_input_fails() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _) = harness.start_thread().await;
    harness
        .control
        .fail_next_spawn_initial_input(CodexErr::UnsupportedOperation(
            "injected initial input failure".to_string(),
        ))
        .await;

    let outcome = harness
        .control
        .spawn_agent_with_metadata_outcome(
            harness.config.clone(),
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                parent_thread_id: Some(parent_thread_id),
                ..Default::default()
            },
        )
        .await
        .expect("a committed child should be returned with its delivery failure");

    let SpawnAgentOutcome::InitialInputDeliveryFailed { agent, error } = outcome else {
        panic!("initial input failure should preserve the created child");
    };
    assert_matches!(
        error.details(),
        CodexErrorDetails::UnsupportedOperation(message) if message == "injected initial input failure"
    );
    assert_matches!(
        &agent.status,
        AgentStatus::Errored(message) if message.contains("initial input delivery failed")
    );
    assert_eq!(agent.metadata.agent_id, Some(agent.thread_id));
    assert_eq!(
        harness.control.get_status(agent.thread_id).await,
        agent.status
    );

    let child_thread = harness
        .manager
        .get_thread(agent.thread_id)
        .await
        .expect("child should remain registered after delivery fails");
    assert_eq!(child_thread.agent_status().await, agent.status);
    let inventory = harness
        .control
        .get_live_agent_inventory_info(agent.thread_id)
        .await
        .expect("committed child should remain present in the live inventory");
    assert_eq!(inventory.status, agent.status);
    let listed_agents = harness
        .control
        .list_agents(&SessionSource::Cli, /*path_prefix*/ None)
        .await
        .expect("committed child should be listed with its terminal status");
    assert_eq!(
        listed_agents
            .iter()
            .find(|listed| listed.agent_name == agent.thread_id.to_string())
            .map(|listed| listed.agent_status.clone()),
        Some(agent.status.clone())
    );
    let child_config = child_thread.config_snapshot().await;
    assert_eq!(agent.effective_model, child_config.model);
    assert_eq!(
        agent.effective_reasoning_effort,
        child_config.reasoning_effort
    );
    assert!(
        harness
            .manager
            .list_live_thread_spawn_edges()
            .await
            .contains(&(parent_thread_id, agent.thread_id)),
        "committed child should retain its parent edge"
    );
}

#[tokio::test]
async fn spawn_agent_outcome_reconciles_a_child_that_dies_during_initial_input() {
    let (home, mut config) = test_config().await;
    config
        .features
        .enable(Feature::Sqlite)
        .expect("test config should allow sqlite");
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_thread_id, _) = harness.start_thread().await;
    harness
        .control
        .remove_next_spawn_initial_input_child()
        .await;

    let outcome = harness
        .control
        .spawn_agent_with_metadata_outcome(
            harness.config.clone(),
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                parent_thread_id: Some(parent_thread_id),
                ..Default::default()
            },
        )
        .await
        .expect("the child death should be represented as a spawn outcome");

    let SpawnAgentOutcome::ChildDiedDuringSpawn { error } = outcome else {
        panic!("a removed child must not be represented as a live delivery failure");
    };
    assert_matches!(error.details(), CodexErrorDetails::ThreadNotFound(_));
    assert_eq!(harness.manager.list_thread_ids().await, vec![parent_thread_id]);
    assert!(
        harness
            .control
            .list_agents(&SessionSource::Cli, /*path_prefix*/ None)
            .await
            .expect("live agent inventory should load")
            .is_empty(),
        "the dead child must be absent from the registry-backed inventory"
    );

    let open_children = harness
        .state_db
        .as_ref()
        .expect("sqlite state db should be configured")
        .list_thread_spawn_children_with_status(
            parent_thread_id,
            codex_state::DirectionalThreadSpawnEdgeStatus::Open,
        )
        .await
        .expect("open child edges should load");
    assert!(open_children.is_empty(), "dead child edge must not remain open");
    let closed_children = harness
        .state_db
        .as_ref()
        .expect("sqlite state db should be configured")
        .list_thread_spawn_children_with_status(
            parent_thread_id,
            codex_state::DirectionalThreadSpawnEdgeStatus::Closed,
        )
        .await
        .expect("closed child edges should load");
    assert_eq!(closed_children.len(), 1);
    let child_thread_id = closed_children[0];
    assert!(harness.control.get_agent_metadata(child_thread_id).is_none());
    assert!(
        harness
            .control
            .get_live_agent_inventory_info(child_thread_id)
            .await
            .is_none(),
        "the removed child must not retain live inventory or residency identity"
    );
    assert_eq!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
}

#[tokio::test]
async fn spawn_cancellation_reconciles_a_child_that_dies_before_interrupt() {
    let (home, mut config) = test_config().await;
    config
        .features
        .enable(Feature::Sqlite)
        .expect("test config should allow sqlite");
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_thread_id, _) = harness.start_thread().await;
    let (initial_input_started, allow_initial_input) =
        harness.control.pause_next_spawn_initial_input().await;
    harness
        .control
        .kill_next_spawn_cancellation_interrupt_child()
        .await;
    let cancellation_token = CancellationToken::new();
    let control = harness.control.clone();
    let child_config = harness.config.clone();
    let child_cancellation_token = cancellation_token.clone();
    let spawn_task = tokio::spawn(async move {
        control
            .spawn_agent_with_metadata_outcome(
                child_config,
                text_input("child task"),
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    depth: 1,
                    agent_path: None,
                    agent_nickname: None,
                    agent_role: None,
                })),
                SpawnAgentOptions {
                    parent_thread_id: Some(parent_thread_id),
                    cancellation_token: Some(child_cancellation_token),
                    ..Default::default()
                },
            )
            .await
    });

    initial_input_started
        .await
        .expect("initial delivery should pause after the child is committed");
    cancellation_token.cancel();
    allow_initial_input
        .send(())
        .expect("initial delivery pause should still be waiting");

    let outcome = spawn_task
        .await
        .expect("spawn task should join")
        .expect("the child death should be represented as a spawn outcome");
    let SpawnAgentOutcome::ChildDiedDuringSpawn { error } = outcome else {
        panic!("a dead child during cancellation must not be reported as cancelled and live");
    };
    assert_matches!(error.details(), CodexErrorDetails::InternalAgentDied);
    assert_eq!(harness.manager.list_thread_ids().await, vec![parent_thread_id]);

    let open_children = harness
        .state_db
        .as_ref()
        .expect("sqlite state db should be configured")
        .list_thread_spawn_children_with_status(
            parent_thread_id,
            codex_state::DirectionalThreadSpawnEdgeStatus::Open,
        )
        .await
        .expect("open child edges should load");
    assert!(open_children.is_empty());
    let closed_children = harness
        .state_db
        .as_ref()
        .expect("sqlite state db should be configured")
        .list_thread_spawn_children_with_status(
            parent_thread_id,
            codex_state::DirectionalThreadSpawnEdgeStatus::Closed,
        )
        .await
        .expect("closed child edges should load");
    assert_eq!(closed_children.len(), 1);
    let child_thread_id = closed_children[0];
    assert!(harness.control.get_agent_metadata(child_thread_id).is_none());
    assert!(
        harness
            .control
            .get_live_agent_inventory_info(child_thread_id)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn spawn_cancellation_cleanup_owns_already_removed_precommit_and_postcommit_children() {
    for phase in [
        SpawnCancellationCleanupPhase::BeforeRegistryCommit,
        SpawnCancellationCleanupPhase::AfterRegistryCommit,
    ] {
        let (home, mut config) = test_config().await;
        config
            .features
            .enable(Feature::Sqlite)
            .expect("test config should allow sqlite");
        let harness = AgentControlHarness::new_with_config(home, config).await;
        let (parent_thread_id, _) = harness.start_thread().await;
        let (cleanup_started, allow_cleanup) = harness
            .control
            .pause_next_spawn_cancellation_cleanup(phase)
            .await;
        let cancellation_token = CancellationToken::new();
        let control = harness.control.clone();
        let child_config = harness.config.clone();
        let child_cancellation_token = cancellation_token.clone();
        let spawn_task = tokio::spawn(async move {
            control
                .spawn_agent_with_metadata_outcome(
                    child_config,
                    text_input("child task"),
                    Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                        parent_thread_id,
                        depth: 1,
                        agent_path: None,
                        agent_nickname: None,
                        agent_role: None,
                    })),
                    SpawnAgentOptions {
                        parent_thread_id: Some(parent_thread_id),
                        cancellation_token: Some(child_cancellation_token),
                        ..Default::default()
                    },
                )
                .await
        });

        cleanup_started
            .await
            .expect("spawn should pause before its selected cancellation cleanup phase");
        let child_thread_id = harness
            .manager
            .list_thread_ids()
            .await
            .into_iter()
            .find(|thread_id| *thread_id != parent_thread_id)
            .expect("created child should remain visible until the concurrent teardown");
        assert_eq!(
            harness.control.get_agent_metadata(child_thread_id).is_some(),
            phase == SpawnCancellationCleanupPhase::AfterRegistryCommit
        );
        cancellation_token.cancel();
        match phase {
            SpawnCancellationCleanupPhase::BeforeRegistryCommit => {
                let child = harness
                    .manager
                    .get_thread(child_thread_id)
                    .await
                    .expect("precommit child should still be managed");
                child
                    .shutdown_and_wait()
                    .await
                    .expect("concurrent precommit teardown should complete");
                let error = child
                    .submit(Op::Shutdown {})
                    .await
                    .expect_err("the closed child submission channel should report child death");
                assert_matches!(error.details(), CodexErrorDetails::InternalAgentDied);
            }
            SpawnCancellationCleanupPhase::AfterRegistryCommit => {
                harness
                    .control
                    .shutdown_live_agent(child_thread_id)
                    .await
                    .expect("concurrent postcommit teardown should complete");
                assert!(harness.control.get_agent_metadata(child_thread_id).is_none());
                assert!(harness.manager.get_thread(child_thread_id).await.is_err());
                let error = harness
                    .control
                    .shutdown_live_agent(child_thread_id)
                    .await
                    .expect_err("a postcommit cleanup retry should observe the removed child");
                assert_matches!(error.details(), CodexErrorDetails::ThreadNotFound(_));
            }
        }
        allow_cleanup
            .send(())
            .expect("cancellation cleanup pause should still be waiting");

        let error = spawn_task
            .await
            .expect("spawn task should join")
            .expect_err("the caller cancellation should own benign cleanup races");
        assert_matches!(error.details(), CodexErrorDetails::TurnAborted);
        assert_eq!(harness.manager.list_thread_ids().await, vec![parent_thread_id]);
        assert!(harness.control.get_agent_metadata(child_thread_id).is_none());
        assert!(
            harness
                .control
                .list_agents(&SessionSource::Cli, /*path_prefix*/ None)
                .await
                .expect("live agent inventory should load")
                .is_empty(),
            "cancellation cleanup must not leave a live child identity"
        );

        let state_db = harness
            .state_db
            .as_ref()
            .expect("sqlite state db should be configured");
        for status in [
            codex_state::DirectionalThreadSpawnEdgeStatus::Open,
            codex_state::DirectionalThreadSpawnEdgeStatus::Closed,
        ] {
            assert!(
                state_db
                    .list_thread_spawn_children_with_status(parent_thread_id, status)
                    .await
                    .expect("child edge state should load")
                    .is_empty(),
                "cleanup before lifecycle publication must not leave a persisted child edge"
            );
        }
    }
}

#[tokio::test]
async fn spawn_cancellation_preserves_naturally_completed_and_errored_child_statuses() {
    for expected_status in [
        AgentStatus::Completed(Some("child finished first".to_string())),
        AgentStatus::Errored("child failed first".to_string()),
    ] {
        let (home, config) = test_config().await;
        let harness = AgentControlHarness::new_with_config(home, config).await;
        let (parent_thread_id, _) = harness.start_thread().await;
        let (initial_input_started, allow_initial_input) =
            harness.control.pause_next_spawn_initial_input().await;
        harness
            .control
            .finish_next_spawn_cancellation_before_interrupt(expected_status.clone())
            .await;
        let cancellation_token = CancellationToken::new();
        let control = harness.control.clone();
        let child_config = harness.config.clone();
        let child_cancellation_token = cancellation_token.clone();
        let spawn_task = tokio::spawn(async move {
            control
                .spawn_agent_with_metadata_outcome(
                    child_config,
                    text_input("child task"),
                    Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                        parent_thread_id,
                        depth: 1,
                        agent_path: None,
                        agent_nickname: None,
                        agent_role: None,
                    })),
                    SpawnAgentOptions {
                        parent_thread_id: Some(parent_thread_id),
                        cancellation_token: Some(child_cancellation_token),
                        ..Default::default()
                    },
                )
                .await
        });

        initial_input_started
            .await
            .expect("initial delivery should pause after the child is committed");
        cancellation_token.cancel();
        allow_initial_input
            .send(())
            .expect("initial delivery pause should still be waiting");

        let outcome = spawn_task
            .await
            .expect("spawn task should join")
            .expect("a naturally terminal child should have a successful spawn outcome");
        let SpawnAgentOutcome::TerminalBeforeCancellation { agent } = outcome else {
            panic!("a child that naturally finished before the interrupt must not be cancelled");
        };
        assert_eq!(agent.status, expected_status);
        assert_eq!(harness.control.get_status(agent.thread_id).await, expected_status);
        assert!(
            harness
                .manager
                .list_live_thread_spawn_edges()
                .await
                .contains(&(parent_thread_id, agent.thread_id)),
            "a naturally terminal child remains a durable, live spawn target"
        );
        let child_ops = harness
            .manager
            .captured_ops()
            .into_iter()
            .filter_map(|(thread_id, op)| (thread_id == agent.thread_id).then_some(op))
            .collect::<Vec<_>>();
        assert!(
            child_ops.iter().any(|op| matches!(op, Op::UserInput { .. })),
            "the regression requires successful initial input delivery"
        );
        assert!(
            !child_ops.iter().any(|op| matches!(op, Op::Interrupt)),
            "a naturally terminal child must not receive a synthetic cancellation interrupt"
        );
    }
}

async fn spawn_with_cancellation_in_final_status_window(
    harness: &AgentControlHarness,
    parent_thread_id: ThreadId,
) -> SpawnAgentOutcome {
    let (final_status_started, allow_final_status) =
        harness.control.pause_next_spawn_final_status().await;
    let cancellation_token = CancellationToken::new();
    let control = harness.control.clone();
    let child_config = harness.config.clone();
    let child_cancellation_token = cancellation_token.clone();
    let spawn_task = tokio::spawn(async move {
        control
            .spawn_agent_with_metadata_outcome(
                child_config,
                text_input("child task"),
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    depth: 1,
                    agent_path: None,
                    agent_nickname: None,
                    agent_role: None,
                })),
                SpawnAgentOptions {
                    parent_thread_id: Some(parent_thread_id),
                    cancellation_token: Some(child_cancellation_token),
                    ..Default::default()
                },
            )
            .await
    });

    final_status_started
        .await
        .expect("spawn should pause after its final status and liveness checks");
    cancellation_token.cancel();
    allow_final_status
        .send(())
        .expect("final status pause should still be waiting");
    spawn_task
        .await
        .expect("spawn task should join")
        .expect("late cancellation should be represented as a spawn outcome")
}

#[tokio::test]
async fn spawn_cancellation_in_final_status_window_interrupts_active_child() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _) = harness.start_thread().await;
    let outcome = spawn_with_cancellation_in_final_status_window(&harness, parent_thread_id).await;
    let SpawnAgentOutcome::Cancelled { agent } = outcome else {
        panic!("an active child cancelled at the publication boundary must be interrupted");
    };
    assert_eq!(agent.status, AgentStatus::Interrupted);
    assert_eq!(harness.control.get_status(agent.thread_id).await, agent.status);
    assert!(
        harness
            .manager
            .captured_ops()
            .into_iter()
            .any(|(thread_id, op)| thread_id == agent.thread_id && matches!(op, Op::Interrupt)),
        "late cancellation must submit the same interrupt as an earlier cancellation"
    );
}

#[tokio::test]
async fn spawn_cancellation_in_final_status_window_preserves_natural_terminal_child() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _) = harness.start_thread().await;
    let expected_status = AgentStatus::Completed(Some("child finished first".to_string()));
    harness
        .control
        .finish_next_spawn_cancellation_before_interrupt(expected_status.clone())
        .await;
    let outcome = spawn_with_cancellation_in_final_status_window(&harness, parent_thread_id).await;
    let SpawnAgentOutcome::TerminalBeforeCancellation { agent } = outcome else {
        panic!("a naturally terminal child at the publication boundary must not be interrupted");
    };
    assert_eq!(agent.status, expected_status);
    assert_eq!(harness.control.get_status(agent.thread_id).await, expected_status);
    assert!(
        !harness
            .manager
            .captured_ops()
            .into_iter()
            .any(|(thread_id, op)| thread_id == agent.thread_id && matches!(op, Op::Interrupt)),
        "a naturally terminal child must not receive a synthetic cancellation interrupt"
    );
}

#[tokio::test]
async fn spawn_cancellation_in_final_status_window_reconciles_removed_child() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _) = harness.start_thread().await;
    harness
        .control
        .remove_next_spawn_cancellation_interrupt_child()
        .await;
    let outcome = spawn_with_cancellation_in_final_status_window(&harness, parent_thread_id).await;
    let SpawnAgentOutcome::ChildDiedDuringSpawn { error } = outcome else {
        panic!("a removed child at the publication boundary must not be reported as cancelled");
    };
    assert_matches!(error.details(), CodexErrorDetails::ThreadNotFound(_));
    assert_eq!(harness.manager.list_thread_ids().await, vec![parent_thread_id]);
    assert!(
        harness
            .control
            .list_agents(&SessionSource::Cli, /*path_prefix*/ None)
            .await
            .expect("live agent inventory should load")
            .is_empty(),
        "the removed child must not retain a live agent identity"
    );
}

async fn persisted_originator(thread: &CodexThread) -> String {
    thread.ensure_rollout_materialized().await;
    thread
        .flush_rollout()
        .await
        .expect("thread rollout should flush");
    let stored_thread = thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ true,
        )
        .await
        .expect("thread should be readable");
    let history = stored_thread.history.expect("history should be loaded");
    history
        .items
        .iter()
        .find_map(|item| match item {
            RolloutItem::SessionMeta(meta_line) => Some(meta_line.meta.originator.clone()),
            RolloutItem::ResponseItem(_)
            | RolloutItem::InterAgentCommunication(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::EventMsg(_)
            | RolloutItem::Compacted(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::TurnContext(_) => None,
        })
        .expect("session metadata should be persisted")
}

fn has_subagent_notification(history_items: &[ResponseItem]) -> bool {
    history_items.iter().any(|item| {
        let ResponseItem::Message { role, content, .. } = item else {
            return false;
        };
        if role != "user" {
            return false;
        }
        content.iter().any(|content_item| match content_item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                SubagentNotification::matches_text(text)
            }
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => false,
        })
    })
}

/// Returns true when any message item contains `needle` in a text span.
fn history_contains_text(history_items: &[ResponseItem], needle: &str) -> bool {
    history_items.iter().any(|item| {
        let ResponseItem::Message { content, .. } = item else {
            return false;
        };
        content.iter().any(|content_item| match content_item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                text.contains(needle)
            }
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => false,
        })
    })
}

fn history_contains_assistant_inter_agent_communication(
    history_items: &[ResponseItem],
    expected: &InterAgentCommunication,
) -> bool {
    history_items.iter().any(|item| {
        let ResponseItem::Message { role, content, .. } = item else {
            return false;
        };
        if role != "assistant" {
            return false;
        }
        content.iter().any(|content_item| match content_item {
            ContentItem::OutputText { text } => {
                serde_json::from_str::<InterAgentCommunication>(text)
                    .ok()
                    .as_ref()
                    == Some(expected)
            }
            ContentItem::InputText { .. }
            | ContentItem::InputImage { .. }
            | ContentItem::InputAudio { .. } => false,
        })
    })
}

async fn wait_for_subagent_notification(parent_thread: &Arc<CodexThread>) -> bool {
    let wait = async {
        loop {
            let history_items = parent_thread
                .session
                .clone_history()
                .await
                .raw_items()
                .to_vec();
            if has_subagent_notification(&history_items) {
                return true;
            }
            sleep(Duration::from_millis(25)).await;
        }
    };
    // CI can take several seconds to schedule the detached completion watcher,
    // especially on slower Windows runners.
    timeout(Duration::from_secs(10), wait).await.is_ok()
}

async fn persist_thread_for_tree_resume(thread: &Arc<CodexThread>, message: &str) {
    thread
        .inject_user_message_without_turn(message.to_string())
        .await;
    thread.session.ensure_rollout_materialized().await;
    thread
        .session
        .flush_rollout()
        .await
        .expect("test thread rollout should flush");
}

async fn wait_for_live_thread_spawn_children(
    control: &AgentControl,
    parent_thread_id: ThreadId,
    expected_children: &[ThreadId],
) {
    let mut expected_children = expected_children.to_vec();
    expected_children.sort_by_key(std::string::ToString::to_string);

    timeout(Duration::from_secs(5), async {
        loop {
            let mut child_ids = control
                .open_thread_spawn_children(parent_thread_id)
                .await
                .expect("live child list should load")
                .into_iter()
                .map(|(thread_id, _)| thread_id)
                .collect::<Vec<_>>();
            child_ids.sort_by_key(std::string::ToString::to_string);
            if child_ids == expected_children {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("expected persisted child tree");
}

async fn spawn_named_agent_for_tree_inspection(
    harness: &AgentControlHarness,
    parent_thread_id: ThreadId,
    depth: usize,
    agent_path: AgentPath,
) -> ThreadId {
    harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: depth.try_into().expect("test depth should fit in i32"),
                agent_path: Some(agent_path),
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("named child spawn should succeed")
}

#[tokio::test]
async fn inspect_agent_tree_without_state_db_points_to_subagent_tail() {
    let (home, config) = test_config().await;
    let harness =
        AgentControlHarness::new_with_config_and_state_db(home, config, /*state_db*/ None);
    assert!(harness.state_db.is_none());
    let (root_thread_id, _root_thread) = harness.start_thread().await;
    harness
        .control
        .register_session_root(root_thread_id, /*current_parent_thread_id*/ None);

    let err = harness
        .control
        .inspect_agent_tree(
            root_thread_id,
            &SessionSource::Exec,
            /*target*/ None,
            /*agent_roots*/ None,
            AgentTreeScope::All,
            /*max_depth*/ 2,
            /*max_agents*/ 10,
        )
        .await
        .expect_err("stale inspection should require the state db");
    assert_matches!(
        err.details(),
        CodexErrorDetails::UnsupportedOperation(message)
            if message == INSPECT_AGENT_TREE_STATE_DB_UNAVAILABLE_MESSAGE
    );
}

#[tokio::test]
async fn inspect_agent_tree_stops_at_depth_and_agent_bound_before_materializing_fanout() {
    const HUGE_CHILD_COUNT: usize = 128;

    let harness = AgentControlHarness::new().await;
    let (root_thread_id, _root_thread) = harness.start_thread().await;
    harness
        .control
        .register_session_root(root_thread_id, /*current_parent_thread_id*/ None);
    for _ in 0..HUGE_CHILD_COUNT {
        harness
            .manager
            .start_thread(StartThreadOptions {
                session_source: Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: root_thread_id,
                    depth: 1,
                    agent_path: None,
                    agent_nickname: None,
                    agent_role: None,
                })),
                environments: Some(Vec::new()),
                ..StartThreadOptions::new(harness.config.clone())
            })
            .await
            .expect("start fanout child");
    }

    let inspection = harness
        .control
        .inspect_agent_tree(
            root_thread_id,
            &SessionSource::Exec,
            /*target*/ None,
            /*agent_roots*/ None,
            AgentTreeScope::Live,
            /*max_depth*/ 1,
            /*max_agents*/ 1,
        )
        .await
        .expect("bounded inspection should succeed");

    assert_eq!(inspection.agents.len(), 1);
    assert_eq!(inspection.agents[0].depth, 0);
    assert!(inspection.truncated);
    assert_eq!(inspection.summary.total_agents, 1);
}

#[tokio::test]
async fn inspect_agent_tree_applies_agent_roots_before_the_output_bound() {
    let harness = AgentControlHarness::new().await;
    let (root_thread_id, _root_thread) = harness.start_thread().await;
    harness
        .control
        .register_session_root(root_thread_id, /*current_parent_thread_id*/ None);

    let mut b_thread_id = None;
    for agent_name in ["a", "b"] {
        let agent_path = AgentPath::root().join(agent_name).expect("agent path");
        let thread_id = harness
            .control
            .spawn_agent(
                harness.config.clone(),
                text_input("child task"),
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: root_thread_id,
                    depth: 1,
                    agent_path: Some(agent_path),
                    agent_nickname: None,
                    agent_role: Some("worker".to_string()),
                })),
            )
            .await
            .expect("named child spawn should succeed");
        if agent_name == "b" {
            b_thread_id = Some(thread_id);
        }
    }
    let b_thread_id = b_thread_id.expect("b should be spawned");
    harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("grandchild task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: b_thread_id,
                depth: 2,
                agent_path: Some(
                    AgentPath::root()
                        .join("b")
                        .expect("b agent path")
                        .join("child")
                        .expect("child agent path"),
                ),
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("b child spawn should succeed");

    let agent_roots = vec!["/root/b".to_string()];
    let inspection = harness
        .control
        .inspect_agent_tree(
            root_thread_id,
            &SessionSource::Exec,
            /*target*/ None,
            Some(&agent_roots),
            AgentTreeScope::Live,
            /*max_depth*/ 2,
            /*max_agents*/ 2,
        )
        .await
        .expect("filtered inspection should succeed");

    assert_eq!(
        inspection
            .agents
            .iter()
            .map(|agent| agent.agent_name.as_str())
            .collect::<Vec<_>>(),
        vec!["/root/b", "/root/b/child"]
    );
    assert_eq!(inspection.summary.total_agents, 2);
    assert!(!inspection.truncated);
}

#[tokio::test]
async fn inspect_agent_tree_explicit_target_respects_scope_and_serializes_stale_status() {
    let harness = AgentControlHarness::new().await;
    let (root_thread_id, root_thread) = harness.start_thread().await;
    harness
        .control
        .register_session_root(root_thread_id, /*current_parent_thread_id*/ None);
    let child_path = AgentPath::root().join("closed").expect("agent path");
    let child_thread_id = spawn_named_agent_for_tree_inspection(
        &harness,
        root_thread_id,
        /*depth*/ 1,
        child_path.clone(),
    )
    .await;
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    persist_thread_for_tree_resume(&root_thread, "root persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, root_thread_id, &[child_thread_id]).await;
    harness
        .control
        .close_agent(child_thread_id)
        .await
        .expect("child close should succeed");

    let live_err = harness
        .control
        .inspect_agent_tree(
            root_thread_id,
            &SessionSource::Exec,
            Some(child_path.as_str()),
            /*agent_roots*/ None,
            AgentTreeScope::Live,
            /*max_depth*/ 2,
            /*max_agents*/ 10,
        )
        .await
        .expect_err("closed target must not be returned by live scope");
    assert_matches!(
        live_err.details(),
        CodexErrorDetails::UnsupportedOperation(message)
            if message == "agent path `/root/closed` not found in the live tree"
    );

    let stale = harness
        .control
        .inspect_agent_tree(
            root_thread_id,
            &SessionSource::Exec,
            Some(child_path.as_str()),
            /*agent_roots*/ None,
            AgentTreeScope::Stale,
            /*max_depth*/ 2,
            /*max_agents*/ 10,
        )
        .await
        .expect("closed target should be returned by stale scope");
    assert_eq!(stale.summary.stale_agents, 1);
    assert_eq!(stale.summary.live_agents, 0);
    assert_eq!(stale.agents.len(), 1);
    assert_eq!(stale.agents[0].agent_name, "/root/closed");
    assert_eq!(stale.agents[0].session_state, AgentSessionState::Stale);
    assert_eq!(stale.agents[0].agent_status, None);
    assert_eq!(
        serde_json::to_value(&stale).expect("stale receipt should serialize")["agents"][0]["agent_status"],
        serde_json::Value::Null
    );

    let stale_from_live_root = harness
        .control
        .inspect_agent_tree(
            root_thread_id,
            &SessionSource::Exec,
            /*target*/ None,
            /*agent_roots*/ None,
            AgentTreeScope::Stale,
            /*max_depth*/ 2,
            /*max_agents*/ 10,
        )
        .await
        .expect("stale scope should include persisted children without reporting the live root");
    assert_eq!(stale_from_live_root.root_agent_name, "/root");
    assert_eq!(stale_from_live_root.summary.live_agents, 0);
    assert_eq!(stale_from_live_root.summary.stale_agents, 1);
    assert_eq!(stale_from_live_root.agents.len(), 1);
    assert_eq!(stale_from_live_root.agents[0].agent_name, "/root/closed");
    assert_eq!(stale_from_live_root.agents[0].depth, 1);
    assert_eq!(
        stale_from_live_root.agents[0].session_state,
        AgentSessionState::Stale
    );

    let agent_roots = vec![child_path.to_string()];
    let stale_filtered = harness
        .control
        .inspect_agent_tree(
            root_thread_id,
            &SessionSource::Exec,
            /*target*/ None,
            Some(&agent_roots),
            AgentTreeScope::Stale,
            /*max_depth*/ 2,
            /*max_agents*/ 10,
        )
        .await
        .expect("a closed explicit branch root should be returned by stale scope");
    assert_eq!(stale_filtered.agents.len(), 1);
    assert_eq!(stale_filtered.agents[0].agent_name, "/root/closed");
    assert_eq!(
        stale_filtered.agents[0].session_state,
        AgentSessionState::Stale
    );

    let live_filtered_err = harness
        .control
        .inspect_agent_tree(
            root_thread_id,
            &SessionSource::Exec,
            /*target*/ None,
            Some(&agent_roots),
            AgentTreeScope::Live,
            /*max_depth*/ 2,
            /*max_agents*/ 10,
        )
        .await
        .expect_err("a closed explicit branch root must not be returned by live scope");
    assert_matches!(
        live_filtered_err.details(),
        CodexErrorDetails::UnsupportedOperation(message)
            if message == "agent path `/root/closed` not found in the live tree"
    );

    let all = harness
        .control
        .inspect_agent_tree(
            root_thread_id,
            &SessionSource::Exec,
            Some(child_path.as_str()),
            /*agent_roots*/ None,
            AgentTreeScope::All,
            /*max_depth*/ 2,
            /*max_agents*/ 10,
        )
        .await
        .expect("all scope should include the closed target as stale");
    assert_eq!(all.agents[0].session_state, AgentSessionState::Stale);

    let stale_root_err = harness
        .control
        .inspect_agent_tree(
            root_thread_id,
            &SessionSource::Exec,
            Some("/root"),
            /*agent_roots*/ None,
            AgentTreeScope::Stale,
            /*max_depth*/ 2,
            /*max_agents*/ 10,
        )
        .await
        .expect_err("live root must not be returned by explicit stale scope");
    assert_matches!(
        stale_root_err.details(),
        CodexErrorDetails::UnsupportedOperation(message)
            if message == "agent path `/root` not found in the stale tree"
    );
}

#[tokio::test]
async fn inspect_agent_tree_never_reclassifies_live_or_open_paths_as_stale() {
    let harness = AgentControlHarness::new().await;
    let (root_thread_id, root_thread) = harness.start_thread().await;
    harness
        .control
        .register_session_root(root_thread_id, /*current_parent_thread_id*/ None);
    let child_path = AgentPath::root().join("open").expect("agent path");
    let child_thread_id = spawn_named_agent_for_tree_inspection(
        &harness,
        root_thread_id,
        /*depth*/ 1,
        child_path.clone(),
    )
    .await;
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    persist_thread_for_tree_resume(&root_thread, "root persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, root_thread_id, &[child_thread_id]).await;

    let stale_live_err = harness
        .control
        .inspect_agent_tree(
            root_thread_id,
            &SessionSource::Exec,
            Some(child_path.as_str()),
            /*agent_roots*/ None,
            AgentTreeScope::Stale,
            /*max_depth*/ 2,
            /*max_agents*/ 10,
        )
        .await
        .expect_err("a live target must not be returned by stale scope");
    assert_matches!(
        stale_live_err.details(),
        CodexErrorDetails::UnsupportedOperation(message)
            if message == "agent path `/root/open` not found in the stale tree"
    );

    let live = harness
        .control
        .inspect_agent_tree(
            root_thread_id,
            &SessionSource::Exec,
            Some(child_path.as_str()),
            /*agent_roots*/ None,
            AgentTreeScope::All,
            /*max_depth*/ 2,
            /*max_agents*/ 10,
        )
        .await
        .expect("all scope should prefer the loaded target");
    assert_eq!(live.agents[0].session_state, AgentSessionState::Live);

    harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("open child shutdown should succeed");
    let persisted_open_children = harness
        .state_db
        .as_ref()
        .expect("state db should be configured")
        .list_thread_spawn_children_with_status(
            root_thread_id,
            codex_state::DirectionalThreadSpawnEdgeStatus::Open,
        )
        .await
        .expect("open edge should remain persisted after ordinary shutdown");
    assert_eq!(persisted_open_children, vec![child_thread_id]);
    let open_err = harness
        .control
        .inspect_agent_tree(
            root_thread_id,
            &SessionSource::Exec,
            Some(child_path.as_str()),
            /*agent_roots*/ None,
            AgentTreeScope::All,
            /*max_depth*/ 2,
            /*max_agents*/ 10,
        )
        .await
        .expect_err("an evicted Open edge must not be fabricated as a stale target");
    assert_matches!(
        open_err.details(),
        CodexErrorDetails::UnsupportedOperation(message)
            if message == "agent path `/root/open` not found in the inspected tree"
    );
}

#[tokio::test]
async fn inspect_agent_tree_rejects_unresolved_explicit_agent_roots() {
    let harness = AgentControlHarness::new().await;
    let (root_thread_id, _root_thread) = harness.start_thread().await;
    harness
        .control
        .register_session_root(root_thread_id, /*current_parent_thread_id*/ None);
    let agent_roots = vec!["/root/missing".to_string()];

    let err = harness
        .control
        .inspect_agent_tree(
            root_thread_id,
            &SessionSource::Exec,
            /*target*/ None,
            Some(&agent_roots),
            AgentTreeScope::Live,
            /*max_depth*/ 2,
            /*max_agents*/ 10,
        )
        .await
        .expect_err("an unresolved explicit branch root must not look like an empty tree");
    assert_matches!(
        err.details(),
        CodexErrorDetails::UnsupportedOperation(message)
            if message == "agent path `/root/missing` not found in the live tree"
    );
}

#[tokio::test]
async fn inspect_agent_tree_discards_persisted_cycle_edges_before_counting() {
    let harness = AgentControlHarness::new().await;
    let (root_thread_id, root_thread) = harness.start_thread().await;
    harness
        .control
        .register_session_root(root_thread_id, /*current_parent_thread_id*/ None);
    let child_path = AgentPath::root().join("cycle").expect("agent path");
    let child_thread_id = spawn_named_agent_for_tree_inspection(
        &harness,
        root_thread_id,
        /*depth*/ 1,
        child_path,
    )
    .await;
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    persist_thread_for_tree_resume(&root_thread, "root persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, root_thread_id, &[child_thread_id]).await;
    harness
        .control
        .close_agent(child_thread_id)
        .await
        .expect("child close should succeed");
    let state_db = harness
        .state_db
        .as_ref()
        .expect("state db should be configured");
    state_db
        .upsert_thread_spawn_edge(
            child_thread_id,
            root_thread_id,
            codex_state::DirectionalThreadSpawnEdgeStatus::Closed,
        )
        .await
        .expect("malformed persisted cycle should be inserted for regression coverage");

    let inspection = harness
        .control
        .inspect_agent_tree(
            root_thread_id,
            &SessionSource::Exec,
            /*target*/ None,
            /*agent_roots*/ None,
            AgentTreeScope::All,
            /*max_depth*/ 3,
            /*max_agents*/ 10,
        )
        .await
        .expect("cycle-safe inspection should succeed");
    assert_eq!(
        inspection
            .agents
            .iter()
            .map(|agent| agent.agent_name.as_str())
            .collect::<Vec<_>>(),
        vec!["/root", "/root/cycle"]
    );
    let root = inspection
        .agents
        .iter()
        .find(|agent| agent.agent_name == "/root")
        .expect("root row");
    assert_eq!(root.direct_child_count, 1);
    assert_eq!(root.descendant_count, 1);
    let child = inspection
        .agents
        .iter()
        .find(|agent| agent.agent_name == "/root/cycle")
        .expect("child row");
    assert_eq!(child.direct_child_count, 0);
    assert_eq!(child.descendant_count, 0);

    let stale = harness
        .control
        .inspect_agent_tree(
            root_thread_id,
            &SessionSource::Exec,
            /*target*/ None,
            /*agent_roots*/ None,
            AgentTreeScope::Stale,
            /*max_depth*/ 3,
            /*max_agents*/ 10,
        )
        .await
        .expect("stale inspection should exclude its live context root");
    assert_eq!(stale.root_agent_name, "/root");
    assert_eq!(stale.summary.live_agents, 0);
    assert_eq!(stale.summary.stale_agents, 1);
    assert_eq!(stale.agents.len(), 1);
    assert_eq!(stale.agents[0].agent_name, "/root/cycle");
    assert_eq!(stale.agents[0].session_state, AgentSessionState::Stale);
    assert_eq!(stale.agents[0].direct_child_count, 0);
    assert_eq!(stale.agents[0].descendant_count, 0);

    let stale_from_explicit_child = harness
        .control
        .inspect_agent_tree(
            root_thread_id,
            &SessionSource::Exec,
            /*target*/ Some("/root/cycle"),
            /*agent_roots*/ None,
            AgentTreeScope::Stale,
            /*max_depth*/ 3,
            /*max_agents*/ 10,
        )
        .await
        .expect("an explicit stale child must not reintroduce the live canonical root");
    assert_eq!(stale_from_explicit_child.root_agent_name, "/root/cycle");
    assert_eq!(stale_from_explicit_child.summary.live_agents, 0);
    assert_eq!(stale_from_explicit_child.summary.stale_agents, 1);
    assert_eq!(stale_from_explicit_child.agents.len(), 1);
    assert_eq!(
        stale_from_explicit_child.agents[0].agent_name,
        "/root/cycle"
    );
    assert_eq!(
        stale_from_explicit_child.agents[0].session_state,
        AgentSessionState::Stale
    );
    assert_eq!(stale_from_explicit_child.agents[0].depth, 0);
    assert_eq!(stale_from_explicit_child.agents[0].direct_child_count, 0);
    assert_eq!(stale_from_explicit_child.agents[0].descendant_count, 0);
}

async fn assert_thread_not_loaded(manager: &ThreadManager, thread_id: ThreadId) {
    match manager.get_thread(thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(id) => assert_eq!(*id, thread_id),
            _ => panic!("expected ThreadNotFound, got {err:?}"),
        },
        Ok(_) => panic!("expected thread not to be loaded"),
    }
}

#[tokio::test]
async fn send_input_errors_when_manager_dropped() {
    let control = AgentControl::default();
    let err = control
        .send_input(
            ThreadId::new(),
            vec![UserInput::Text {
                text: "hello".to_string(),
                text_elements: Vec::new(),
            }],
        )
        .await
        .expect_err("send_input should fail without a manager");
    assert_eq!(
        err.to_string(),
        "unsupported operation: thread manager dropped"
    );
}

#[tokio::test]
async fn get_status_returns_not_found_without_manager() {
    let control = AgentControl::default();
    let got = control.get_status(ThreadId::new()).await;
    assert_eq!(got, AgentStatus::NotFound);
}

#[tokio::test]
async fn on_event_updates_status_from_task_started() {
    let status = agent_status_from_event(&EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: "turn-1".to_string(),
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: ModeKind::Default,
    }));
    assert_eq!(status, Some(AgentStatus::Running));
}

#[tokio::test]
async fn on_event_updates_status_from_task_complete() {
    let status = agent_status_from_event(&EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: "turn-1".to_string(),
        started_at: None,
        last_agent_message: Some("done".to_string()),
        compaction_events_in_turn: 0,
        final_model: None,
        model_snapshot: None,
        provider_usage: None,
        error: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }));
    let expected = AgentStatus::Completed(Some("done".to_string()));
    assert_eq!(status, Some(expected));
}

#[tokio::test]
async fn on_event_updates_status_from_error() {
    let status = agent_status_from_event(&EventMsg::Error(ErrorEvent {
        message: "boom".to_string(),
        codex_error_info: None,
    }));

    let expected = AgentStatus::Errored("boom".to_string());
    assert_eq!(status, Some(expected));
}

#[tokio::test]
async fn on_event_updates_status_from_turn_aborted() {
    let status = agent_status_from_event(&EventMsg::TurnAborted(TurnAbortedEvent {
        turn_id: Some("turn-1".to_string()),
        started_at: None,
        reason: TurnAbortReason::Interrupted,
        provider_usage: None,
        completed_at: None,
        duration_ms: None,
    }));

    let expected = AgentStatus::Interrupted;
    assert_eq!(status, Some(expected));
}

#[tokio::test]
async fn on_event_updates_status_from_shutdown_complete() {
    let status = agent_status_from_event(&EventMsg::ShutdownComplete);
    assert_eq!(status, Some(AgentStatus::Shutdown));
}

#[tokio::test]
async fn spawn_agent_errors_when_manager_dropped() {
    let control = AgentControl::default();
    let (_home, config) = test_config().await;
    let err = control
        .spawn_agent(config, text_input("hello"), /*session_source*/ None)
        .await
        .expect_err("spawn_agent should fail without a manager");
    assert_eq!(
        err.to_string(),
        "unsupported operation: thread manager dropped"
    );
}

#[tokio::test]
async fn resume_agent_errors_when_manager_dropped() {
    let control = AgentControl::default();
    let (_home, config) = test_config().await;
    let err = control
        .resume_agent_from_rollout(config, ThreadId::new(), SessionSource::Exec)
        .await
        .expect_err("resume_agent should fail without a manager");
    assert_eq!(
        err.to_string(),
        "unsupported operation: thread manager dropped"
    );
}

#[tokio::test]
async fn send_input_errors_when_thread_missing() {
    let harness = AgentControlHarness::new().await;
    let thread_id = ThreadId::new();
    let err = harness
        .control
        .send_input(
            thread_id,
            vec![UserInput::Text {
                text: "hello".to_string(),
                text_elements: Vec::new(),
            }],
        )
        .await
        .expect_err("send_input should fail for missing thread");
    assert_matches!(
        err.details(),
        CodexErrorDetails::ThreadNotFound(id) if *id == thread_id
    );
}

#[tokio::test]
async fn get_status_returns_not_found_for_missing_thread() {
    let harness = AgentControlHarness::new().await;
    let status = harness.control.get_status(ThreadId::new()).await;
    assert_eq!(status, AgentStatus::NotFound);
}

#[tokio::test]
async fn get_status_returns_pending_init_for_new_thread() {
    let harness = AgentControlHarness::new().await;
    let (thread_id, _) = harness.start_thread().await;
    let status = harness.control.get_status(thread_id).await;
    assert_eq!(status, AgentStatus::PendingInit);
}

#[tokio::test]
async fn subscribe_status_errors_for_missing_thread() {
    let harness = AgentControlHarness::new().await;
    let thread_id = ThreadId::new();
    let err = harness
        .control
        .subscribe_status(thread_id)
        .await
        .expect_err("subscribe_status should fail for missing thread");
    assert_matches!(
        err.details(),
        CodexErrorDetails::ThreadNotFound(id) if *id == thread_id
    );
}

#[tokio::test]
async fn subscribe_status_updates_on_shutdown() {
    let harness = AgentControlHarness::new().await;
    let (thread_id, thread) = harness.start_thread().await;
    let mut status_rx = harness
        .control
        .subscribe_status(thread_id)
        .await
        .expect("subscribe_status should succeed");
    assert_eq!(status_rx.borrow().clone(), AgentStatus::PendingInit);

    let _ = thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");

    let _ = status_rx.changed().await;
    assert_eq!(status_rx.borrow().clone(), AgentStatus::Shutdown);
}

#[tokio::test]
async fn send_input_submits_user_message() {
    let harness = AgentControlHarness::new().await;
    let (thread_id, _thread) = harness.start_thread().await;

    let submission_id = harness
        .control
        .send_input(
            thread_id,
            vec![UserInput::Text {
                text: "hello from tests".to_string(),
                text_elements: Vec::new(),
            }],
        )
        .await
        .expect("send_input should succeed");
    assert!(!submission_id.is_empty());
    let expected = (
        thread_id,
        Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello from tests".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        },
    );
    let captured = harness
        .manager
        .captured_ops()
        .into_iter()
        .find(|entry| *entry == expected);
    assert_eq!(captured, Some(expected));
}

#[tokio::test]
async fn send_inter_agent_communication_without_turn_queues_message_without_triggering_turn() {
    let harness = AgentControlHarness::new().await;
    let (thread_id, thread) = harness.start_thread().await;
    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "hello from tests".to_string(),
        /*trigger_turn*/ false,
    );

    let submission_id = harness
        .control
        .send_inter_agent_communication(
            thread_id,
            communication.clone(),
            AgentCommunicationContext::new(AgentCommunicationKind::Message, ThreadId::new()),
        )
        .await
        .expect("send_inter_agent_communication should succeed");
    assert!(!submission_id.is_empty());

    let expected = (
        thread_id,
        Op::InterAgentCommunication {
            communication: communication.clone(),
        },
    );
    let captured = harness
        .manager
        .captured_ops()
        .into_iter()
        .find(|entry| *entry == expected);
    assert_eq!(captured, Some(expected));

    timeout(Duration::from_secs(5), async {
        loop {
            if thread
                .session
                .input_queue
                .has_pending_input(&thread.session.active_turn)
                .await
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("inter-agent communication should stay pending");

    let history_items = thread.session.clone_history().await.raw_items().to_vec();
    assert!(!history_contains_assistant_inter_agent_communication(
        &history_items,
        &communication
    ));
}

#[tokio::test]
async fn ensure_v2_agent_loaded_reloads_registered_unloaded_agent() {
    let (home, mut config) = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    let _ = config.features.enable(Feature::Sqlite);
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_thread_id, _parent_thread) = harness.start_paginated_thread().await;
    let agent_path = AgentPath::try_from("/root/worker").expect("agent path");
    let spawned_agent = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(agent_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                parent_thread_id: Some(parent_thread_id),
                ..Default::default()
            },
        )
        .await
        .expect("spawn_agent should succeed");
    let child_thread = harness
        .manager
        .get_thread(spawned_agent.thread_id)
        .await
        .expect("child thread should exist");
    let original_config = child_thread.config_snapshot().await;
    child_thread
        .inject_response_items(vec![assistant_message(
            "child persisted",
            Some(MessagePhase::FinalAnswer),
        )])
        .await
        .expect("child rollout should persist with v2 metadata");
    child_thread
        .shutdown_and_wait()
        .await
        .expect("child thread should shut down");
    let stored_child = child_thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ false,
        )
        .await
        .expect("child metadata should be readable");
    assert_eq!(stored_child.history_mode, ThreadHistoryMode::Paginated);

    let registered_metadata = harness
        .control
        .get_agent_metadata(spawned_agent.thread_id)
        .expect("registered child metadata");
    let cold_status = AgentStatus::Completed(Some("child persisted".to_string()));
    let manager_state = harness.control.upgrade().expect("thread manager state");
    let removal = manager_state
        .remove_thread_if_same(&spawned_agent.thread_id, &child_thread, || {
            harness.control.state.publish_cold_status_if_current(
                spawned_agent.thread_id,
                &registered_metadata,
                &child_thread,
                cold_status.clone(),
            );
        })
        .await;
    assert_eq!(removal, RemoveThreadIfSameResult::Removed);
    assert_eq!(
        harness.control.get_status(spawned_agent.thread_id).await,
        cold_status
    );
    match harness.manager.get_thread(spawned_agent.thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(id) => assert_eq!(*id, spawned_agent.thread_id),
            _ => panic!("expected ThreadNotFound, got {err:?}"),
        },
        Ok(_) => panic!("expected thread to be removed"),
    }

    let cold_communication = InterAgentCommunication::new(
        AgentPath::root(),
        agent_path.clone(),
        Vec::new(),
        "queued while cold".to_string(),
        /*trigger_turn*/ false,
    );
    harness
        .control
        .prepare_v2_agent_delivery(spawned_agent.thread_id)
        .await
        .expect("cold queue-only delivery should prepare without reload")
        .send(
            cold_communication.clone(),
            AgentCommunicationContext::new(AgentCommunicationKind::Message, ThreadId::new()),
            /*interrupt*/ false,
        )
        .await
        .expect("cold queue-only delivery should succeed");
    assert!(
        harness
            .manager
            .get_thread(spawned_agent.thread_id)
            .await
            .is_err()
    );
    assert_eq!(
        registered_metadata.lifecycle.lock().await.cold_mail_len(),
        1
    );

    let mut conflicting_resume_config = harness.config.clone();
    conflicting_resume_config.model = Some("different-caller-model".to_string());
    conflicting_resume_config.model_reasoning_effort = Some(ReasoningEffort::High);
    conflicting_resume_config.service_tier = Some("priority".to_string());
    let mut missing_provider_config = conflicting_resume_config.clone();
    missing_provider_config.model_provider_id = "different-caller-provider".to_string();
    missing_provider_config
        .model_providers
        .remove(&original_config.model_provider_id);
    let err = harness
        .control
        .ensure_v2_agent_loaded(missing_provider_config, spawned_agent.thread_id)
        .await
        .expect_err("reload should fail closed when the persisted provider is unavailable");
    assert!(err.to_string().contains(&format!(
        "persisted model provider `{}` is not configured",
        original_config.model_provider_id
    )));
    assert_eq!(
        harness.control.get_status(spawned_agent.thread_id).await,
        cold_status
    );
    assert_eq!(
        registered_metadata.lifecycle.lock().await.cold_mail_len(),
        1
    );
    conflicting_resume_config.model_provider.base_url =
        Some("http://runtime-provider.invalid/v1".to_string());
    conflicting_resume_config.model_provider.supports_websockets = false;
    let expected_runtime_provider = conflicting_resume_config.model_provider.clone();
    harness
        .control
        .ensure_v2_agent_loaded(conflicting_resume_config, spawned_agent.thread_id)
        .await
        .expect("known v2 agent should reload");
    let reloaded_thread = harness
        .manager
        .get_thread(spawned_agent.thread_id)
        .await
        .expect("reloaded child thread should exist");
    assert_eq!(
        reloaded_thread
            .session
            .input_queue
            .drain_mailbox_communications()
            .await,
        vec![cold_communication]
    );
    assert_eq!(
        harness
            .control
            .state
            .cold_status(spawned_agent.thread_id, Some(&reloaded_thread)),
        None
    );
    let stale_error = harness
        .control
        .handle_thread_request_result(
            spawned_agent.thread_id,
            &manager_state,
            Some(&child_thread),
            Err(CodexErr::InternalAgentDied),
        )
        .await
        .expect_err("stale request cleanup should retain the replacement");
    assert_matches!(stale_error.details(), CodexErrorDetails::InternalAgentDied);
    let current_thread = harness
        .manager
        .get_thread(spawned_agent.thread_id)
        .await
        .expect("replacement should remain registered");
    assert!(Arc::ptr_eq(&current_thread, &reloaded_thread));
    let reloaded_config = reloaded_thread.config_snapshot().await;
    assert_eq!(
        (
            reloaded_config.model,
            reloaded_config.model_provider_id,
            reloaded_config.reasoning_effort,
        ),
        (
            original_config.model,
            original_config.model_provider_id,
            original_config.reasoning_effort,
        )
    );
    let reloaded_provider = reloaded_thread.session.provider().await;
    assert_eq!(
        reloaded_provider.base_url,
        expected_runtime_provider.base_url
    );
    assert_eq!(
        reloaded_provider.supports_websockets,
        expected_runtime_provider.supports_websockets
    );

    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        agent_path,
        Vec::new(),
        "hello after reload".to_string(),
        /*trigger_turn*/ false,
    );
    harness
        .control
        .send_inter_agent_communication(
            spawned_agent.thread_id,
            communication.clone(),
            AgentCommunicationContext::new(AgentCommunicationKind::Message, ThreadId::new()),
        )
        .await
        .expect("send_inter_agent_communication should succeed after reload");
    let expected = (
        spawned_agent.thread_id,
        Op::InterAgentCommunication { communication },
    );
    let captured = harness
        .manager
        .captured_ops()
        .into_iter()
        .find(|entry| *entry == expected);
    assert_eq!(captured, Some(expected));
}

#[tokio::test]
async fn close_agent_discards_registry_owned_cold_mail() {
    let harness = AgentControlHarness::new().await;
    let agent_id = ThreadId::new();
    harness
        .control
        .state
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve registry slot")
        .commit(AgentMetadata {
            agent_id: Some(agent_id),
            ..Default::default()
        });
    let metadata = harness
        .control
        .state
        .agent_metadata_for_thread(agent_id)
        .expect("registered metadata");
    metadata
        .lifecycle
        .lock()
        .await
        .push_cold_mail(ColdMailboxItem {
            receive_id: Some("cold-mail".to_string()),
            communication: InterAgentCommunication::new(
                AgentPath::root(),
                AgentPath::try_from("/root/worker").expect("agent path"),
                Vec::new(),
                "discard me".to_string(),
                /*trigger_turn*/ false,
            ),
        });

    harness
        .control
        .close_agent(agent_id)
        .await
        .expect("closing a known cold agent should succeed");

    assert_eq!(metadata.lifecycle.lock().await.cold_mail_len(), 0);
    assert!(harness.control.get_agent_metadata(agent_id).is_none());
}

#[tokio::test]
async fn resume_agent_from_rollout_does_not_reopen_v2_descendants() {
    let (home, mut config) = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    let _ = config.features.enable(Feature::Sqlite);
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let worker_path = AgentPath::root().join("worker").expect("worker path");
    let worker_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello worker"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(worker_path.clone()),
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("worker spawn should succeed");
    let reviewer_path = worker_path.join("reviewer").expect("reviewer path");
    let reviewer_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello reviewer"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: worker_thread_id,
                depth: 2,
                agent_path: Some(reviewer_path.clone()),
                agent_nickname: None,
                agent_role: Some("reviewer".to_string()),
            })),
        )
        .await
        .expect("reviewer spawn should succeed");

    let worker_thread = harness
        .manager
        .get_thread(worker_thread_id)
        .await
        .expect("worker thread should exist");
    let reviewer_thread = harness
        .manager
        .get_thread(reviewer_thread_id)
        .await
        .expect("reviewer thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&worker_thread, "worker persisted").await;
    persist_thread_for_tree_resume(&reviewer_thread, "reviewer persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[worker_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, worker_thread_id, &[reviewer_thread_id])
        .await;

    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert_eq!(report.submit_failed, Vec::<ThreadId>::new());
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());

    let resumed_manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        harness.config.model_provider.clone(),
        harness.config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        harness.state_db.clone(),
    );
    let resumed_control = resumed_manager.agent_control();
    let resumed_parent_thread_id = resumed_control
        .resume_agent_from_rollout(
            harness.config.clone(),
            parent_thread_id,
            SessionSource::Exec,
        )
        .await
        .expect("v2 root resume should succeed");
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_ne!(
        resumed_control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_thread_not_loaded(&resumed_manager, worker_thread_id).await;
    assert_thread_not_loaded(&resumed_manager, reviewer_thread_id).await;
}

#[tokio::test]
async fn spawn_agent_creates_thread_and_sends_prompt() {
    let harness = AgentControlHarness::new().await;
    let thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("spawned"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed");
    let _thread = harness
        .manager
        .get_thread(thread_id)
        .await
        .expect("thread should be registered");
    let expected = (
        thread_id,
        Op::UserInput {
            items: vec![UserInput::Text {
                text: "spawned".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        },
    );
    let captured = harness
        .manager
        .captured_ops()
        .into_iter()
        .find(|entry| *entry == expected);
    assert_eq!(captured, Some(expected));
}

#[tokio::test]
async fn ephemeral_spawn_does_not_persist_agent_graph_edge() {
    let (home, mut config) = test_config().await;
    config.ephemeral = true;
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;
    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("spawned"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
        )
        .await
        .expect("ephemeral agent spawn should succeed");

    let persisted_children = harness
        .state_db
        .as_ref()
        .expect("manager should retain state db")
        .list_thread_spawn_children(parent_thread_id)
        .await
        .expect("persisted child list should load");
    assert_eq!(persisted_children, Vec::<ThreadId>::new());
    assert!(
        harness.manager.get_thread(child_thread_id).await.is_ok(),
        "ephemeral child should remain live"
    );
}

#[tokio::test]
async fn paginated_subagent_fork_cold_resume_preserves_child_settings() {
    let (home, mut config) = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    let _ = config.features.enable(Feature::Sqlite);
    let child_provider_id = "child-provider";
    let mut child_provider = config.model_provider.clone();
    child_provider.name = "Child provider".to_string();
    config
        .model_providers
        .insert(child_provider_id.to_string(), child_provider.clone());
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_thread_id, parent_thread) = harness.start_paginated_thread().await;
    parent_thread
        .inject_user_message_without_turn("paginated parent context".to_string())
        .await;
    let turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-paginated".to_string();
    parent_thread
        .session
        .record_conversation_items(
            turn_context.as_ref(),
            &[spawn_agent_call(&parent_spawn_call_id)],
        )
        .await;
    parent_thread
        .session
        .persist_rollout_items(&[
            RolloutItem::ResponseItem(ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: "id-less inherited context".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }),
            RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: parent_thread_id,
                turn_id: "parent-turn".to_string(),
                item: TurnItem::UserMessage(UserMessageItem {
                    id: "parent-user".to_string(),
                    client_id: None,
                    content: Vec::new(),
                }),
                completed_at_ms: 1,
            })),
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
                ThreadSettingsAppliedEvent {
                    thread_settings: ThreadSettingsSnapshot {
                        model: "parent-only-model".to_string(),
                        model_provider_id: "parent-only-provider".to_string(),
                        service_tier: None,
                        approval_policy: AskForApproval::Never,
                        approvals_reviewer: ApprovalsReviewer::User,
                        permission_profile: PermissionProfile::workspace_write(),
                        active_permission_profile: None,
                        cwd: harness.config.cwd.clone(),
                        reasoning_effort: None,
                        reasoning_summary: None,
                        personality: None,
                        collaboration_mode: CollaborationMode {
                            mode: ModeKind::Default,
                            settings: Settings {
                                model: "parent-only-model".to_string(),
                                reasoning_effort: None,
                                developer_instructions: None,
                            },
                        },
                    },
                },
            )),
        ])
        .await;

    let mut child_config = harness.config.clone();
    child_config.model = Some("gpt-5.4".to_string());
    child_config.model_provider_id = child_provider_id.to_string();
    child_config.model_provider = child_provider;
    child_config.model_reasoning_effort = Some(ReasoningEffort::High);
    child_config.approvals_reviewer = ApprovalsReviewer::AutoReview;
    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            child_config,
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                parent_thread_id: Some(parent_thread_id),
                ..Default::default()
            },
        )
        .await
        .expect("paginated child fork should succeed")
        .thread_id;
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let expected_child_settings = child_thread
        .config_snapshot()
        .await
        .into_thread_settings_snapshot();
    assert_eq!(
        (
            expected_child_settings.model.clone(),
            expected_child_settings.model_provider_id.clone(),
            expected_child_settings.reasoning_effort.clone(),
            expected_child_settings.approvals_reviewer,
        ),
        (
            "gpt-5.4".to_string(),
            child_provider_id.to_string(),
            Some(ReasoningEffort::High),
            ApprovalsReviewer::AutoReview,
        )
    );
    assert!(
        history_contains_text(
            child_thread.session.clone_history().await.raw_items(),
            "paginated parent context",
        ),
        "bounded parent context should remain model-visible to the child"
    );
    child_thread.ensure_rollout_materialized().await;
    child_thread
        .flush_rollout()
        .await
        .expect("child rollout should flush");
    let rollout_path = child_thread
        .rollout_path()
        .expect("child rollout should exist");
    let lines = std::fs::read_to_string(&rollout_path)
        .expect("read child rollout")
        .lines()
        .map(|line| serde_json::from_str::<RolloutLine>(line).expect("parse rollout line"))
        .collect::<Vec<_>>();
    let RolloutItem::SessionMeta(meta_line) = &lines[0].item else {
        panic!("child rollout should start with session metadata");
    };
    assert_eq!(
        (
            meta_line.meta.history_mode,
            meta_line.meta.parent_thread_id,
            meta_line.meta.forked_from_id,
            meta_line.meta.thread_source.clone(),
        ),
        (
            ThreadHistoryMode::Paginated,
            Some(parent_thread_id),
            Some(parent_thread_id),
            Some(ThreadSource::Subagent),
        )
    );
    assert_eq!(
        lines.iter().map(|line| line.ordinal).collect::<Vec<_>>(),
        (0..u64::try_from(lines.len()).expect("rollout length should fit in u64"))
            .map(Some)
            .collect::<Vec<_>>(),
        "paginated child records should have contiguous ordinals"
    );
    let child_history_start_ordinal = meta_line
        .meta
        .subagent_history_start_ordinal
        .expect("paginated child should mark its local history boundary");
    let prefix_end =
        usize::try_from(child_history_start_ordinal).expect("history boundary should fit in usize");
    let copied_prefix = &lines[1..prefix_end];
    let copied_idless_context = copied_prefix
        .iter()
        .find_map(|line| match &line.item {
            RolloutItem::ResponseItem(response_item)
                if serde_json::to_string(response_item)
                    .expect("serialize response item")
                    .contains("id-less inherited context") =>
            {
                Some(response_item)
            }
            _ => None,
        })
        .expect("copied prefix should contain inherited response item");
    assert!(
        copied_idless_context.id().is_some_and(|id| !id.is_empty()),
        "copied model context should receive response item ids before persistence"
    );
    let copied_idless_context_ordinal = copied_prefix
        .iter()
        .find(|line| {
            matches!(
                &line.item,
                RolloutItem::ResponseItem(response_item)
                    if response_item.id() == copied_idless_context.id()
            )
        })
        .and_then(|line| line.ordinal)
        .expect("copied response item should have an ordinal");
    assert!(
        copied_idless_context_ordinal < child_history_start_ordinal,
        "copied context should remain below the child-owned history boundary"
    );
    let copied_parent_context_count = lines
        .iter()
        .filter(|line| {
            serde_json::to_string(&line.item)
                .expect("serialize rollout item")
                .contains("paginated parent context")
        })
        .count();
    assert_eq!(
        copied_parent_context_count, 1,
        "copied model context should be persisted once"
    );
    assert!(
        !copied_prefix.iter().any(|line| {
            matches!(
                &line.item,
                RolloutItem::EventMsg(
                    EventMsg::ItemCompleted(_) | EventMsg::ThreadSettingsApplied(_)
                )
            )
        }),
        "copied non-structural presentation and metadata records should not enter the child rollout"
    );

    let child_owned_settings = lines[prefix_end..]
        .iter()
        .filter_map(|line| match &line.item {
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(event)) => {
                Some((line.ordinal, event.thread_settings.clone()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        child_owned_settings,
        vec![(
            Some(child_history_start_ordinal),
            expected_child_settings.clone(),
        )],
        "the first child-owned record should be its sole effective settings snapshot"
    );

    let stored_child = child_thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ false,
        )
        .await
        .expect("child metadata should be readable");
    assert_eq!(
        (
            stored_child.history_mode,
            stored_child.thread_source,
            stored_child.parent_thread_id,
            stored_child.forked_from_id,
            stored_child.model,
            stored_child.model_provider,
            stored_child.reasoning_effort,
        ),
        (
            ThreadHistoryMode::Paginated,
            Some(ThreadSource::Subagent),
            Some(parent_thread_id),
            Some(parent_thread_id),
            Some(expected_child_settings.model.clone()),
            expected_child_settings.model_provider_id.clone(),
            expected_child_settings.reasoning_effort.clone(),
        )
    );

    child_thread
        .shutdown_and_wait()
        .await
        .expect("child thread should shut down before eviction");
    let registered_metadata = harness
        .control
        .get_agent_metadata(child_thread_id)
        .expect("registered child metadata");
    let cold_status = AgentStatus::Completed(Some("child persisted".to_string()));
    let manager_state = harness.control.upgrade().expect("thread manager state");
    let removal = manager_state
        .remove_thread_if_same(&child_thread_id, &child_thread, || {
            harness.control.state.publish_cold_status_if_current(
                child_thread_id,
                &registered_metadata,
                &child_thread,
                cold_status.clone(),
            );
        })
        .await;
    assert_eq!(removal, RemoveThreadIfSameResult::Removed);
    match harness.manager.get_thread(child_thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(id) => assert_eq!(*id, child_thread_id),
            _ => panic!("expected ThreadNotFound, got {err:?}"),
        },
        Ok(_) => panic!("expected child thread to be evicted"),
    }

    assert_ne!(
        harness.config.model.as_deref(),
        Some(expected_child_settings.model.as_str()),
        "the reload caller must carry a conflicting model default"
    );
    assert_ne!(
        harness.config.model_provider_id, expected_child_settings.model_provider_id,
        "the reload caller must carry a conflicting provider default"
    );
    assert_ne!(
        harness.config.model_reasoning_effort, expected_child_settings.reasoning_effort,
        "the reload caller must carry a conflicting reasoning default"
    );
    assert_ne!(
        harness.config.approvals_reviewer, expected_child_settings.approvals_reviewer,
        "the reload caller must carry a conflicting reviewer default"
    );
    harness
        .control
        .ensure_v2_agent_loaded(harness.config.clone(), child_thread_id)
        .await
        .expect("evicted paginated child should cold reload");
    let reloaded_child = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("reloaded child should be registered");
    assert_eq!(
        reloaded_child
            .config_snapshot()
            .await
            .into_thread_settings_snapshot(),
        expected_child_settings,
        "cold reload should restore the child's complete effective settings"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_without_fork_from_paginated_parent_stays_fresh_and_paginated() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_paginated_thread().await;
    parent_thread
        .inject_user_message_without_turn("parent-only context".to_string())
        .await;

    let child_thread_id = harness
        .spawn_anonymous_child(
            parent_thread_id,
            SpawnAgentOptions {
                parent_thread_id: Some(parent_thread_id),
                ..Default::default()
            },
        )
        .await;
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    assert!(
        !history_contains_text(
            child_thread.session.clone_history().await.raw_items(),
            "parent-only context",
        ),
        "fork_turns=none should not copy parent context"
    );
    child_thread.ensure_rollout_materialized().await;
    child_thread
        .flush_rollout()
        .await
        .expect("child rollout should flush");
    let meta = codex_rollout::read_session_meta_line(
        &child_thread
            .rollout_path()
            .expect("child rollout should exist"),
    )
    .await
    .expect("read child session metadata");
    assert_eq!(meta.meta.history_mode, ThreadHistoryMode::Paginated);
    assert_eq!(meta.meta.subagent_history_start_ordinal, None);

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_numeric_fork_from_compacted_paginated_parent_clamps_to_provable_turns() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_paginated_thread().await;
    let parent_spawn_call_id = "spawn-call-paginated-numeric".to_string();
    parent_thread
        .session
        .persist_rollout_items(&[
            RolloutItem::Compacted(CompactedItem {
                message: String::new(),
                replacement_history: Some(vec![ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "compacted summary".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }]),
                window_number: None,
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
            }),
            RolloutItem::ResponseItem(ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "recent parent turn".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }),
            RolloutItem::ResponseItem(spawn_agent_call(&parent_spawn_call_id)),
        ])
        .await;

    let clamped_child_thread_id = harness
        .spawn_anonymous_child(
            parent_thread_id,
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id),
                fork_mode: Some(SpawnAgentForkMode::LastNTurns(2)),
                ..Default::default()
            },
        )
        .await;
    let clamped_child_thread = harness
        .manager
        .get_thread(clamped_child_thread_id)
        .await
        .expect("clamped child thread should be registered");
    let clamped_history = clamped_child_thread.session.clone_history().await;
    assert!(
        history_contains_text(clamped_history.raw_items(), "recent parent turn"),
        "clamped numeric fork should keep the provable recent turn"
    );
    assert!(
        !history_contains_text(clamped_history.raw_items(), "compacted summary"),
        "clamped numeric fork should not expand into compacted parent context"
    );

    let _ = harness
        .control
        .shutdown_live_agent(clamped_child_thread_id)
        .await
        .expect("clamped child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_can_fork_parent_thread_history_with_sanitized_items() {
    let harness = AgentControlHarness::new().await;
    let mut parent_config = harness.config.clone();
    let _ = parent_config.features.enable(Feature::MultiAgentV2);
    parent_config.multi_agent_v2.root_agent_usage_hint_text =
        Some("Parent root guidance.".to_string());
    parent_config.multi_agent_v2.subagent_usage_hint_text =
        Some("Parent subagent guidance.".to_string());
    let mut child_config = harness.config.clone();
    let _ = child_config.features.enable(Feature::MultiAgentV2);
    child_config.multi_agent_v2.root_agent_usage_hint_text =
        Some("Child root guidance.".to_string());
    child_config.multi_agent_v2.subagent_usage_hint_text =
        Some("Child subagent guidance.".to_string());
    let new_thread = harness
        .manager
        .start_thread(StartThreadOptions::new(parent_config.clone()))
        .await
        .expect("start parent thread");
    let parent_thread_id = new_thread.thread_id;
    let parent_thread = new_thread.thread;
    parent_thread
        .inject_user_message_without_turn("parent seed context".to_string())
        .await;
    let expected_parent_seed = parent_thread
        .session
        .clone_history()
        .await
        .raw_items()
        .first()
        .cloned()
        .expect("parent seed should be recorded");
    let turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-history".to_string();
    let trigger_message = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "parent trigger message".to_string(),
        /*trigger_turn*/ true,
    );
    parent_thread
        .session
        .record_conversation_items(
            turn_context.as_ref(),
            &[
                ResponseItem::Message {
                    id: None,
                    role: "developer".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "Parent root guidance.".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::Message {
                    id: None,
                    role: "developer".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "Parent subagent guidance.".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                assistant_message("parent commentary", Some(MessagePhase::Commentary)),
                assistant_message("parent final answer", Some(MessagePhase::FinalAnswer)),
                assistant_message("parent unknown phase", /*phase*/ None),
                ResponseItem::Reasoning {
                    id: Some(ResponseItemId::with_suffix("rs", "parent-reasoning")),
                    summary: Vec::new(),
                    content: None,
                    encrypted_content: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                trigger_message.to_response_input_item().into(),
                spawn_agent_call(&parent_spawn_call_id),
            ],
        )
        .await;
    let parent_reference_context_item = turn_context.to_turn_context_item();
    parent_thread
        .session
        .persist_rollout_items(&[RolloutItem::TurnContext(
            parent_reference_context_item.clone(),
        )])
        .await;
    parent_thread.session.ensure_rollout_materialized().await;
    parent_thread
        .session
        .flush_rollout()
        .await
        .expect("parent rollout should flush");
    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            child_config,
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id.clone()),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                ..Default::default()
            },
        )
        .await
        .expect("forked spawn should succeed")
        .thread_id;

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    assert_ne!(child_thread_id, parent_thread_id);
    assert_eq!(
        child_thread.config_snapshot().await.history_mode,
        ThreadHistoryMode::Legacy
    );
    let history = child_thread.session.clone_history().await;
    let mut expected_final_answer =
        assistant_message("parent final answer", Some(MessagePhase::FinalAnswer));
    expected_final_answer.set_turn_id_if_missing(&turn_context.sub_id);
    let expected_history = [
        expected_parent_seed,
        expected_final_answer,
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "Child subagent guidance.".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    assert_eq!(
        strip_response_item_ids(history.raw_items()),
        strip_response_item_ids(&expected_history),
        "full-history forked child history should replace parent usage hints with the child subagent hint while filtering non-final assistant/tool chatter"
    );
    assert_eq!(
        serde_json::to_value(child_thread.session.reference_context_item().await)
            .expect("serialize child reference context item"),
        serde_json::to_value(Some(parent_reference_context_item))
            .expect("serialize expected reference context item"),
        "full-history forked child should preserve the parent diff baseline"
    );

    let mut no_hint_child_config = harness.config.clone();
    let _ = no_hint_child_config.features.enable(Feature::MultiAgentV2);
    no_hint_child_config.multi_agent_v2.subagent_usage_hint_text = None;
    let no_hint_child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            no_hint_child_config,
            text_input("child task without hints"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id.clone()),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                ..Default::default()
            },
        )
        .await
        .expect("forked spawn should honor an empty subagent usage hint")
        .thread_id;
    let no_hint_child_thread = harness
        .manager
        .get_thread(no_hint_child_thread_id)
        .await
        .expect("no-hint child thread should be registered");
    let no_hint_history = no_hint_child_thread.session.clone_history().await;
    assert!(
        !history_contains_text(no_hint_history.raw_items(), "Child subagent guidance."),
        "full-history forked child should not add empty subagent guidance"
    );

    let expected = (
        child_thread_id,
        Op::UserInput {
            items: vec![UserInput::Text {
                text: "child task".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        },
    );
    let captured = harness
        .manager
        .captured_ops()
        .into_iter()
        .find(|entry| *entry == expected);
    assert_eq!(captured, Some(expected));

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = harness
        .control
        .shutdown_live_agent(no_hint_child_thread_id)
        .await
        .expect("no-hint child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_fork_strips_parent_usage_hints_from_compacted_history() {
    let harness = AgentControlHarness::new().await;
    let mut parent_config = harness.config.clone();
    let _ = parent_config.features.enable(Feature::MultiAgentV2);
    parent_config.multi_agent_v2.root_agent_usage_hint_text =
        Some("Parent root guidance.".to_string());
    parent_config.multi_agent_v2.subagent_usage_hint_text =
        Some("Parent subagent guidance.".to_string());
    let mut child_config = harness.config.clone();
    let _ = child_config.features.enable(Feature::MultiAgentV2);
    child_config.multi_agent_v2.root_agent_usage_hint_text =
        Some("Child root guidance.".to_string());
    child_config.multi_agent_v2.subagent_usage_hint_text =
        Some("Child subagent guidance.".to_string());
    let new_thread = harness
        .manager
        .start_thread(StartThreadOptions::new(parent_config))
        .await
        .expect("start parent thread");
    let parent_thread_id = new_thread.thread_id;
    let parent_thread = new_thread.thread;
    let turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-compacted-usage-hints".to_string();
    let replacement_history = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "compacted parent summary".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "Parent root guidance.".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    parent_thread
        .session
        .persist_rollout_items(&[
            RolloutItem::Compacted(CompactedItem {
                message: String::new(),
                replacement_history: Some(replacement_history),
                window_number: None,
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
            }),
            RolloutItem::TurnContext(turn_context.to_turn_context_item()),
            RolloutItem::ResponseItem(spawn_agent_call(&parent_spawn_call_id)),
        ])
        .await;
    parent_thread.session.ensure_rollout_materialized().await;
    parent_thread
        .session
        .flush_rollout()
        .await
        .expect("parent rollout should flush");

    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            child_config,
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                ..Default::default()
            },
        )
        .await
        .expect("forked spawn should sanitize compacted usage hints")
        .thread_id;

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let history = child_thread.session.clone_history().await;
    assert!(
        history_contains_text(history.raw_items(), "compacted parent summary"),
        "forked child history should retain compacted non-hint content"
    );
    assert!(
        !history_contains_text(history.raw_items(), "Parent root guidance."),
        "forked child history should strip stale parent hints from compacted replacement history"
    );
    assert!(
        history_contains_text(history.raw_items(), "Child subagent guidance."),
        "full-history forked child should add the child subagent hint after compacted-history sanitization"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_fork_flushes_parent_rollout_before_loading_history() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-unflushed".to_string();
    parent_thread
        .session
        .record_conversation_items(
            turn_context.as_ref(),
            &[
                assistant_message("unflushed final answer", Some(MessagePhase::FinalAnswer)),
                spawn_agent_call(&parent_spawn_call_id),
            ],
        )
        .await;

    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id.clone()),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                ..Default::default()
            },
        )
        .await
        .expect("forked spawn should flush parent rollout before loading history")
        .thread_id;

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let history = child_thread.session.clone_history().await;
    assert!(
        history_contains_text(history.raw_items(), "unflushed final answer"),
        "forked child history should include unflushed assistant final answers after flushing the parent rollout"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_fork_last_n_turns_keeps_only_recent_turns() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    parent_thread
        .inject_user_message_without_turn("old parent context".to_string())
        .await;
    let queued_communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "queued message".to_string(),
        /*trigger_turn*/ false,
    );
    let queued_turn_context = parent_thread.session.new_default_turn().await;
    parent_thread
        .session
        .record_conversation_items(
            queued_turn_context.as_ref(),
            &[queued_communication.to_response_input_item().into()],
        )
        .await;

    let triggered_communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "triggered context".to_string(),
        /*trigger_turn*/ true,
    );
    let triggered_turn_context = parent_thread.session.new_default_turn().await;
    parent_thread
        .session
        .record_conversation_items(
            triggered_turn_context.as_ref(),
            &[triggered_communication.to_response_input_item().into()],
        )
        .await;
    parent_thread
        .inject_user_message_without_turn("current parent task".to_string())
        .await;
    let spawn_turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-last-n".to_string();
    parent_thread
        .session
        .record_conversation_items(
            spawn_turn_context.as_ref(),
            &[spawn_agent_call(&parent_spawn_call_id)],
        )
        .await;
    parent_thread
        .session
        .persist_rollout_items(&[RolloutItem::TurnContext(
            spawn_turn_context.to_turn_context_item(),
        )])
        .await;
    parent_thread.session.ensure_rollout_materialized().await;
    parent_thread
        .session
        .flush_rollout()
        .await
        .expect("parent rollout should flush");

    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id.clone()),
                fork_mode: Some(SpawnAgentForkMode::LastNTurns(2)),
                ..Default::default()
            },
        )
        .await
        .expect("forked spawn should keep only the last two turns")
        .thread_id;

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let history = child_thread.session.clone_history().await;

    assert!(
        !history_contains_text(history.raw_items(), "old parent context"),
        "forked child history should drop parent context outside the requested last-N turn window"
    );
    assert!(
        !history_contains_text(history.raw_items(), "queued message"),
        "forked child history should drop queued inter-agent messages outside the requested last-N turn window"
    );
    assert!(
        !history_contains_text(history.raw_items(), "triggered context"),
        "forked child history should filter assistant inter-agent messages even when they fall inside the requested last-N turn window"
    );
    assert!(
        history_contains_text(history.raw_items(), "current parent task"),
        "forked child history should keep the parent user message from the requested last-N turn window"
    );
    assert!(
        child_thread
            .session
            .reference_context_item()
            .await
            .is_none(),
        "last-N forked child should rebuild context after truncating the cached prefix"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_fork_last_n_turns_drops_parent_startup_prefix_when_under_limit() {
    let harness = AgentControlHarness::new().await;
    let selected_capability_roots = vec![SelectedCapabilityRoot {
        id: "demo@1".to_string(),
        location: CapabilityRootLocation::Environment {
            environment_id: "build".to_string(),
            path: PathUri::parse("file:///plugins/demo").expect("plugin root URI"),
        },
    }];
    let mut thread_extension_init = ExtensionDataInit::new();
    thread_extension_init.insert(selected_capability_roots.clone());
    let parent = harness
        .manager
        .start_thread(StartThreadOptions {
            environments: Some(Vec::new()),
            thread_extension_init,
            ..StartThreadOptions::new(harness.config.clone())
        })
        .await
        .expect("start parent thread");
    let parent_thread_id = parent.thread_id;
    let parent_thread = parent.thread;
    let startup_turn_context = parent_thread.session.new_default_turn().await;
    parent_thread
        .session
        .record_conversation_items(
            startup_turn_context.as_ref(),
            &[ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: "parent startup developer context".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
        )
        .await;
    parent_thread
        .inject_user_message_without_turn("current parent task".to_string())
        .await;
    let spawn_turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-last-n-under-limit".to_string();
    parent_thread
        .session
        .record_conversation_items(
            spawn_turn_context.as_ref(),
            &[spawn_agent_call(&parent_spawn_call_id)],
        )
        .await;
    parent_thread.session.ensure_rollout_materialized().await;
    parent_thread
        .session
        .flush_rollout()
        .await
        .expect("parent rollout should flush");

    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id),
                fork_mode: Some(SpawnAgentForkMode::LastNTurns(2)),
                ..Default::default()
            },
        )
        .await
        .expect("bounded forked spawn should drop startup prefix")
        .thread_id;

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let history = child_thread.session.clone_history().await;
    assert!(
        history_contains_text(history.raw_items(), "current parent task"),
        "bounded fork should retain the requested recent parent turn"
    );
    assert!(
        !history_contains_text(history.raw_items(), "parent startup developer context"),
        "bounded fork should drop parent startup context even when fewer turns exist than requested"
    );
    assert_eq!(
        &child_thread.session.services.selected_capability_roots,
        &selected_capability_roots
    );
    assert!(
        child_thread
            .session
            .reference_context_item()
            .await
            .is_none(),
        "bounded forked child should still rebuild context after truncating the cached prefix"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_fork_last_n_turns_strips_parent_usage_hints() {
    let harness = AgentControlHarness::new().await;
    let mut parent_config = harness.config.clone();
    let _ = parent_config.features.enable(Feature::MultiAgentV2);
    parent_config.multi_agent_v2.root_agent_usage_hint_text =
        Some("Parent root guidance.".to_string());
    let mut child_config = harness.config.clone();
    let _ = child_config.features.enable(Feature::MultiAgentV2);
    child_config.multi_agent_v2.subagent_usage_hint_text =
        Some("Child subagent guidance.".to_string());
    let new_thread = harness
        .manager
        .start_thread(StartThreadOptions::new(parent_config))
        .await
        .expect("start parent thread");
    let parent_thread_id = new_thread.thread_id;
    let parent_thread = new_thread.thread;
    parent_thread
        .inject_user_message_without_turn("parent task".to_string())
        .await;
    let turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-last-n-usage-hints".to_string();
    parent_thread
        .session
        .record_conversation_items(
            turn_context.as_ref(),
            &[
                ResponseItem::Message {
                    id: None,
                    role: "developer".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "Parent root guidance.".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                spawn_agent_call(&parent_spawn_call_id),
            ],
        )
        .await;
    parent_thread.session.ensure_rollout_materialized().await;
    parent_thread
        .session
        .flush_rollout()
        .await
        .expect("parent rollout should flush");

    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            child_config,
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id),
                fork_mode: Some(SpawnAgentForkMode::LastNTurns(2)),
                ..Default::default()
            },
        )
        .await
        .expect("bounded forked spawn should sanitize parent usage hints")
        .thread_id;

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let history = child_thread.session.clone_history().await;
    assert!(
        history_contains_text(history.raw_items(), "parent task"),
        "bounded fork should retain the requested recent parent turn"
    );
    assert!(
        !history_contains_text(history.raw_items(), "Parent root guidance."),
        "bounded fork should strip stale parent root hints before the child rebuilds startup context"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_respects_legacy_max_threads_alias() {
    let max_threads = 1usize;
    let (_home, config) = test_config_with_cli_overrides(vec![(
        "agents.max_threads".to_string(),
        TomlValue::Integer(max_threads as i64),
    )])
    .await;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let control = manager.agent_control();

    let _ = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start thread");

    let first_agent_id = control
        .spawn_agent(
            config.clone(),
            text_input("hello"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed");

    let err = control
        .spawn_agent(
            config,
            text_input("hello again"),
            /*session_source*/ None,
        )
        .await
        .expect_err("spawn_agent should respect max threads");
    let CodexErrorDetails::AgentLimitReached {
        max_threads: seen_max_threads,
    } = err.details()
    else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(*seen_max_threads, max_threads);

    let _ = control
        .shutdown_live_agent(first_agent_id)
        .await
        .expect("shutdown agent");
}

#[tokio::test]
async fn spawn_agent_releases_slot_after_shutdown() {
    let max_threads = 1usize;
    let (_home, config) = test_config_with_cli_overrides(vec![(
        "agents.max_concurrent_threads_per_session".to_string(),
        TomlValue::Integer(max_threads as i64),
    )])
    .await;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let control = manager.agent_control();

    let first_agent_id = control
        .spawn_agent(
            config.clone(),
            text_input("hello"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed");
    let _ = control
        .shutdown_live_agent(first_agent_id)
        .await
        .expect("shutdown agent");

    let second_agent_id = control
        .spawn_agent(
            config.clone(),
            text_input("hello again"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed after shutdown");
    let _ = control
        .shutdown_live_agent(second_agent_id)
        .await
        .expect("shutdown agent");
}

#[tokio::test]
async fn spawn_agent_limit_shared_across_clones() {
    let max_threads = 1usize;
    let (_home, config) = test_config_with_cli_overrides(vec![(
        "agents.max_concurrent_threads_per_session".to_string(),
        TomlValue::Integer(max_threads as i64),
    )])
    .await;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let control = manager.agent_control();
    let cloned = control.clone();

    let first_agent_id = cloned
        .spawn_agent(
            config.clone(),
            text_input("hello"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed");

    let err = control
        .spawn_agent(
            config,
            text_input("hello again"),
            /*session_source*/ None,
        )
        .await
        .expect_err("spawn_agent should respect shared guard");
    let CodexErrorDetails::AgentLimitReached { max_threads } = err.details() else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(*max_threads, 1);

    let _ = control
        .shutdown_live_agent(first_agent_id)
        .await
        .expect("shutdown agent");
}

#[tokio::test]
async fn resume_agent_respects_max_threads_limit() {
    let max_threads = 1usize;
    let (_home, config) = test_config_with_cli_overrides(vec![(
        "agents.max_concurrent_threads_per_session".to_string(),
        TomlValue::Integer(max_threads as i64),
    )])
    .await;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let control = manager.agent_control();

    let resumable_id = control
        .spawn_agent(
            config.clone(),
            text_input("hello"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed");
    let _ = control
        .shutdown_live_agent(resumable_id)
        .await
        .expect("shutdown resumable thread");

    let active_id = control
        .spawn_agent(
            config.clone(),
            text_input("occupy"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed for active slot");

    let err = control
        .resume_agent_from_rollout(config, resumable_id, SessionSource::Exec)
        .await
        .expect_err("resume should respect max threads");
    let CodexErrorDetails::AgentLimitReached {
        max_threads: seen_max_threads,
    } = err.details()
    else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(*seen_max_threads, max_threads);

    let _ = control
        .shutdown_live_agent(active_id)
        .await
        .expect("shutdown active thread");
}

#[tokio::test]
async fn resume_agent_releases_slot_after_resume_failure() {
    let max_threads = 1usize;
    let (_home, config) = test_config_with_cli_overrides(vec![(
        "agents.max_concurrent_threads_per_session".to_string(),
        TomlValue::Integer(max_threads as i64),
    )])
    .await;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let control = manager.agent_control();

    let _ = control
        .resume_agent_from_rollout(config.clone(), ThreadId::new(), SessionSource::Exec)
        .await
        .expect_err("resume should fail for missing rollout path");

    let resumed_id = control
        .spawn_agent(config, text_input("hello"), /*session_source*/ None)
        .await
        .expect("spawn should succeed after failed resume");
    let _ = control
        .shutdown_live_agent(resumed_id)
        .await
        .expect("shutdown resumed thread");
}

#[tokio::test]
async fn spawn_child_completion_notifies_parent_history() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let _ = child_thread
        .submit(Op::Shutdown {})
        .await
        .expect("child shutdown should submit");

    assert_eq!(wait_for_subagent_notification(&parent_thread).await, true);
}

#[tokio::test]
async fn multi_agent_v2_completion_ignores_dead_direct_parent() {
    let harness = AgentControlHarness::new().await;
    let mut config = harness.config.clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    let root = harness
        .manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("root thread should start");
    let root_thread_id = root.thread_id;
    let root_thread = root.thread;
    let worker_path = AgentPath::root().join("worker_a").expect("worker path");
    let worker_thread_id = harness
        .control
        .spawn_agent(
            config.clone(),
            text_input("hello worker"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root_thread_id,
                depth: 1,
                agent_path: Some(worker_path.clone()),
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("worker spawn should succeed");
    let tester_path = worker_path.join("tester").expect("tester path");
    let tester_thread_id = harness
        .control
        .spawn_agent(
            config,
            text_input("hello tester"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: worker_thread_id,
                depth: 2,
                agent_path: Some(tester_path.clone()),
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("tester spawn should succeed");
    harness
        .control
        .shutdown_live_agent(worker_thread_id)
        .await
        .expect("worker shutdown should succeed");

    let tester_thread = harness
        .manager
        .get_thread(tester_thread_id)
        .await
        .expect("tester thread should exist");
    let tester_turn = tester_thread.session.new_default_turn().await;
    tester_thread
        .session
        .send_event(
            tester_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: tester_turn.sub_id.clone(),
                started_at: None,
                last_agent_message: Some("done".to_string()),
                compaction_events_in_turn: 0,
                final_model: None,
                model_snapshot: None,
                provider_usage: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;

    sleep(Duration::from_millis(100)).await;

    assert!(
        !harness
            .manager
            .captured_ops()
            .into_iter()
            .any(|(thread_id, op)| {
                thread_id == worker_thread_id
                    && matches!(
                        op,
                        Op::InterAgentCommunication { communication }
                            if communication.author == tester_path
                                && communication.recipient == worker_path
                                && communication.content == "done"
                    )
            })
    );

    let root_history_items = root_thread
        .session
        .clone_history()
        .await
        .raw_items()
        .to_vec();
    assert!(!history_contains_assistant_inter_agent_communication(
        &root_history_items,
        &InterAgentCommunication::new(
            tester_path,
            AgentPath::root(),
            Vec::new(),
            "done".to_string(),
            /*trigger_turn*/ true,
        )
    ));
    assert!(!has_subagent_notification(&root_history_items));
}

#[tokio::test]
async fn multi_agent_v2_completion_queues_message_for_direct_parent() {
    let harness = AgentControlHarness::new().await;
    let (_root_thread_id, root_thread) = harness.start_thread().await;
    let (worker_thread_id, _worker_thread) = harness.start_thread().await;
    let mut tester_config = harness.config.clone();
    let _ = tester_config.features.enable(Feature::MultiAgentV2);
    let tester_thread_id = harness
        .manager
        .start_thread(StartThreadOptions::new(tester_config.clone()))
        .await
        .expect("tester thread should start")
        .thread_id;
    let tester_thread = harness
        .manager
        .get_thread(tester_thread_id)
        .await
        .expect("tester thread should exist");
    let worker_path = AgentPath::root().join("worker_a").expect("worker path");
    let tester_path = worker_path.join("tester").expect("tester path");
    harness.control.maybe_start_completion_watcher(
        tester_thread_id,
        Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: worker_thread_id,
            depth: 2,
            agent_path: Some(tester_path.clone()),
            agent_nickname: None,
            agent_role: Some("explorer".to_string()),
        })),
        tester_path.to_string(),
        Some(tester_path.clone()),
    );
    let tester_turn = tester_thread.session.new_default_turn().await;
    tester_thread
        .session
        .send_event(
            tester_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: tester_turn.sub_id.clone(),
                started_at: None,
                last_agent_message: Some("done".to_string()),
                compaction_events_in_turn: 0,
                final_model: None,
                model_snapshot: None,
                provider_usage: None,
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;

    let expected_message = crate::session_prefix::format_inter_agent_completion_message(
        worker_path.clone(),
        tester_path.clone(),
        &AgentStatus::Completed(Some("done".to_string())),
    )
    .expect("completed status should render");
    let expected = (
        worker_thread_id,
        Op::InterAgentCommunication {
            communication: InterAgentCommunication::new(
                tester_path.clone(),
                worker_path.clone(),
                Vec::new(),
                expected_message.clone(),
                /*trigger_turn*/ false,
            ),
        },
    );

    timeout(Duration::from_secs(5), async {
        loop {
            let captured = harness
                .manager
                .captured_ops()
                .into_iter()
                .find(|entry| *entry == expected);
            if captured == Some(expected.clone()) {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("completion watcher should queue a direct-parent message");

    let root_history_items = root_thread
        .session
        .clone_history()
        .await
        .raw_items()
        .to_vec();
    assert!(!history_contains_assistant_inter_agent_communication(
        &root_history_items,
        &InterAgentCommunication::new(
            tester_path,
            AgentPath::root(),
            Vec::new(),
            expected_message,
            /*trigger_turn*/ false,
        )
    ));
}

#[tokio::test]
async fn completion_watcher_notifies_parent_when_child_is_missing() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let child_thread_id = ThreadId::new();

    harness.control.maybe_start_completion_watcher(
        child_thread_id,
        Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth: 1,
            agent_path: None,
            agent_nickname: None,
            agent_role: Some("explorer".to_string()),
        })),
        child_thread_id.to_string(),
        /*child_agent_path*/ None,
    );

    assert_eq!(wait_for_subagent_notification(&parent_thread).await, true);

    let history_items = parent_thread
        .session
        .clone_history()
        .await
        .raw_items()
        .to_vec();
    assert_eq!(
        history_contains_text(
            &history_items,
            &format!("\"agent_path\":\"{child_thread_id}\"")
        ),
        true
    );
    assert_eq!(
        history_contains_text(&history_items, "\"status\":\"not_found\""),
        true
    );
}

#[tokio::test]
async fn spawn_thread_subagent_gets_random_nickname_in_session_source() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let snapshot = child_thread.config_snapshot().await;

    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: seen_parent_thread_id,
        depth,
        agent_nickname,
        agent_role,
        ..
    }) = snapshot.session_source
    else {
        panic!("expected thread-spawn sub-agent source");
    };
    assert_eq!(seen_parent_thread_id, parent_thread_id);
    assert_eq!(depth, 1);
    assert!(agent_nickname.is_some());
    assert_eq!(agent_role, Some("explorer".to_string()));
}

#[tokio::test]
async fn spawn_thread_subagents_persist_parent_originator_across_new_and_truncated_fork() {
    let harness = AgentControlHarness::new().await;
    let parent = harness
        .manager
        .start_thread(StartThreadOptions {
            metrics_service_name: Some("codex_work_desktop".to_string()),
            environments: Some(Vec::new()),
            ..StartThreadOptions::new(harness.config.clone())
        })
        .await
        .expect("parent thread should start");
    let parent_originator = persisted_originator(&parent.thread).await;
    assert_eq!(parent_originator, "codex_work_desktop");

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: parent.thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let child_originator = persisted_originator(&child_thread).await;
    assert_eq!(child_originator, parent_originator);

    let child = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("hello forked child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: parent.thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some("spawn-call-last-n".to_string()),
                fork_mode: Some(SpawnAgentForkMode::LastNTurns(1)),
                ..Default::default()
            },
        )
        .await
        .expect("forked child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child.thread_id)
        .await
        .expect("child thread should be registered");
    let child_originator = persisted_originator(&child_thread).await;
    assert_eq!(child_originator, parent_originator);
}

#[tokio::test]
async fn spawn_thread_subagent_uses_role_specific_nickname_candidates() {
    let mut harness = AgentControlHarness::new().await;
    harness.config.agent_roles.insert(
        "researcher".to_string(),
        AgentRoleConfig {
            description: Some("Research role".to_string()),
            config_file: None,
            nickname_candidates: Some(vec!["Atlas".to_string()]),
        },
    );
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("researcher".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let snapshot = child_thread.config_snapshot().await;

    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_nickname, .. }) =
        snapshot.session_source
    else {
        panic!("expected thread-spawn sub-agent source");
    };
    assert_eq!(agent_nickname, Some("Atlas".to_string()));
}

#[tokio::test]
async fn resume_thread_subagent_restores_stored_metadata() {
    let (home, config) = test_config().await;
    let thread_store = Arc::new(InMemoryThreadStore::default());
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = ThreadManager::new(
        &config,
        auth_manager.clone(),
        crate::thread_manager::build_models_manager(&config, auth_manager),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        thread_store.clone(),
        /*agent_graph_store*/ None,
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    let control = manager.agent_control();
    let harness = AgentControlHarness {
        _home: home,
        config,
        state_db: None,
        manager,
        control,
    };
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;
    let agent_path = AgentPath::from_string("/root/explorer".to_string())
        .expect("test agent path should be valid");

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(agent_path.clone()),
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    child_thread.session.ensure_rollout_materialized().await;
    child_thread
        .session
        .flush_rollout()
        .await
        .expect("flush child rollout");
    let mut status_rx = harness
        .control
        .subscribe_status(child_thread_id)
        .await
        .expect("status subscription should succeed");
    if matches!(status_rx.borrow().clone(), AgentStatus::PendingInit) {
        timeout(Duration::from_secs(5), async {
            loop {
                status_rx
                    .changed()
                    .await
                    .expect("child status should advance past pending init");
                if !matches!(status_rx.borrow().clone(), AgentStatus::PendingInit) {
                    break;
                }
            }
        })
        .await
        .expect("child should initialize before shutdown");
    }
    let original_snapshot = child_thread.config_snapshot().await;
    let original_nickname = original_snapshot
        .session_source
        .get_nickname()
        .expect("spawned sub-agent should have a nickname");
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(stored_thread) = thread_store
                .read_thread(ReadThreadParams {
                    thread_id: child_thread_id,
                    include_archived: true,
                    include_history: false,
                })
                .await
                && stored_thread.agent_nickname.is_some()
                && stored_thread.agent_role.as_deref() == Some("explorer")
                && stored_thread.agent_path.as_deref() == Some(agent_path.as_str())
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child thread metadata should be persisted to sqlite before shutdown");

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");

    let resumed_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            child_thread_id,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
        )
        .await
        .expect("resume should succeed");
    assert_eq!(resumed_thread_id, child_thread_id);

    let resumed_snapshot = harness
        .manager
        .get_thread(resumed_thread_id)
        .await
        .expect("resumed child thread should exist")
        .config_snapshot()
        .await;
    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: resumed_parent_thread_id,
        depth: resumed_depth,
        agent_path: resumed_agent_path,
        agent_nickname: resumed_nickname,
        agent_role: resumed_role,
        ..
    }) = resumed_snapshot.session_source
    else {
        panic!("expected thread-spawn sub-agent source");
    };
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_eq!(resumed_depth, 1);
    assert_eq!(resumed_agent_path, Some(agent_path));
    assert_eq!(resumed_nickname, Some(original_nickname));
    assert_eq!(resumed_role, Some("explorer".to_string()));

    let _ = harness
        .control
        .shutdown_live_agent(resumed_thread_id)
        .await
        .expect("resumed child shutdown should submit");
}

#[tokio::test]
async fn resume_agent_from_rollout_reads_archived_rollout_path() {
    let harness = AgentControlHarness::new().await;
    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello"),
            /*session_source*/ None,
        )
        .await
        .expect("child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    persist_thread_for_tree_resume(&child_thread, "persist before archiving").await;
    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should succeed");
    let store = LocalThreadStore::new(
        LocalThreadStoreConfig::from_config(&harness.config),
        harness.state_db.clone(),
    );
    store
        .archive_thread(ArchiveThreadParams {
            thread_id: child_thread_id,
        })
        .await
        .expect("child thread should archive");

    let resumed_thread_id = harness
        .control
        .resume_agent_from_rollout(harness.config.clone(), child_thread_id, SessionSource::Exec)
        .await
        .expect("resume should find archived rollout");
    assert_eq!(resumed_thread_id, child_thread_id);

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("resumed child shutdown should succeed");
}

#[tokio::test]
async fn resume_agent_from_paginated_rollout_loads_model_context() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_paginated_thread().await;
    let child_thread_id = harness
        .spawn_anonymous_child(
            parent_thread_id,
            SpawnAgentOptions {
                parent_thread_id: Some(parent_thread_id),
                ..Default::default()
            },
        )
        .await;
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    assert_eq!(
        child_thread.config_snapshot().await.history_mode,
        ThreadHistoryMode::Paginated
    );
    persist_thread_for_tree_resume(&child_thread, "persist before resume").await;
    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should succeed");

    let resumed_thread_id = harness
        .control
        .resume_agent_from_rollout(harness.config.clone(), child_thread_id, SessionSource::Exec)
        .await
        .expect("resume should load paginated model context");
    assert_eq!(resumed_thread_id, child_thread_id);
    let resumed_thread = harness
        .manager
        .get_thread(resumed_thread_id)
        .await
        .expect("resumed child thread should exist");
    assert!(
        history_contains_text(
            resumed_thread.session.clone_history().await.raw_items(),
            "persist before resume",
        ),
        "resumed child should keep its persisted model context"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("resumed child shutdown should succeed");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn list_agent_subtree_thread_ids_includes_anonymous_and_closed_descendants() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;
    let worker_path = AgentPath::root().join("worker").expect("worker path");
    let reviewer_path = AgentPath::root().join("reviewer").expect("reviewer path");

    let worker_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello worker"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(worker_path.clone()),
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("worker spawn should succeed");
    let worker_child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello worker child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: worker_thread_id,
                depth: 2,
                agent_path: Some(
                    worker_path
                        .join("child")
                        .expect("worker child path should be valid"),
                ),
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("worker child spawn should succeed");
    let no_path_child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello anonymous child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: worker_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("no-path child spawn should succeed");
    let no_path_grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello anonymous grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: no_path_child_thread_id,
                depth: 3,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("no-path grandchild spawn should succeed");
    let _reviewer_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello reviewer"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(reviewer_path),
                agent_nickname: None,
                agent_role: Some("reviewer".to_string()),
            })),
        )
        .await
        .expect("reviewer spawn should succeed");

    let _ = harness
        .control
        .shutdown_live_agent(no_path_grandchild_thread_id)
        .await
        .expect("no-path grandchild shutdown should succeed");

    let mut worker_subtree_thread_ids = harness
        .manager
        .list_agent_subtree_thread_ids(worker_thread_id)
        .await
        .expect("worker subtree thread ids should load");
    worker_subtree_thread_ids.sort_by_key(ToString::to_string);
    let mut expected_worker_subtree_thread_ids = vec![
        worker_thread_id,
        worker_child_thread_id,
        no_path_child_thread_id,
        no_path_grandchild_thread_id,
    ];
    expected_worker_subtree_thread_ids.sort_by_key(ToString::to_string);
    assert_eq!(
        worker_subtree_thread_ids,
        expected_worker_subtree_thread_ids
    );

    let mut no_path_child_subtree_thread_ids = harness
        .manager
        .list_agent_subtree_thread_ids(no_path_child_thread_id)
        .await
        .expect("no-path subtree thread ids should load");
    no_path_child_subtree_thread_ids.sort_by_key(ToString::to_string);
    let mut expected_no_path_child_subtree_thread_ids =
        vec![no_path_child_thread_id, no_path_grandchild_thread_id];
    expected_no_path_child_subtree_thread_ids.sort_by_key(ToString::to_string);
    assert_eq!(
        no_path_child_subtree_thread_ids,
        expected_no_path_child_subtree_thread_ids
    );
}

#[tokio::test]
async fn list_agent_subtree_thread_ids_finds_live_descendants_of_unloaded_root() {
    let (_home, config) = test_config().await;
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        /*state_db*/ None,
    );
    let control = manager.agent_control();
    let parent_thread_id = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("parent should start")
        .thread_id;

    let child_thread_id = control
        .spawn_agent(
            config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = control
        .spawn_agent(
            config,
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    manager.remove_thread(&parent_thread_id).await;

    let mut subtree_thread_ids = manager
        .list_agent_subtree_thread_ids(parent_thread_id)
        .await
        .expect("live subtree should load");
    subtree_thread_ids.sort_by_key(ToString::to_string);
    let mut expected_subtree_thread_ids =
        vec![parent_thread_id, child_thread_id, grandchild_thread_id];
    expected_subtree_thread_ids.sort_by_key(ToString::to_string);

    assert_eq!(subtree_thread_ids, expected_subtree_thread_ids);
}

#[tokio::test]
async fn shutdown_agent_tree_closes_live_descendants() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let _ = harness
        .control
        .shutdown_agent_tree(parent_thread_id)
        .await
        .expect("tree shutdown should succeed");

    assert_eq!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let shutdown_ids = harness
        .manager
        .captured_ops()
        .into_iter()
        .filter_map(|(thread_id, op)| matches!(op, Op::Shutdown).then_some(thread_id))
        .collect::<Vec<_>>();
    let mut expected_shutdown_ids = vec![parent_thread_id, child_thread_id, grandchild_thread_id];
    expected_shutdown_ids.sort_by_key(std::string::ToString::to_string);
    let mut shutdown_ids = shutdown_ids;
    shutdown_ids.sort_by_key(std::string::ToString::to_string);
    assert_eq!(shutdown_ids, expected_shutdown_ids);
}

#[tokio::test]
async fn shutdown_agent_tree_closes_descendants_when_started_at_child() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let _ = harness
        .control
        .close_agent(child_thread_id)
        .await
        .expect("child close should succeed");

    let _ = harness
        .control
        .shutdown_agent_tree(parent_thread_id)
        .await
        .expect("tree shutdown should succeed");

    assert_eq!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );

    let shutdown_ids = harness
        .manager
        .captured_ops()
        .into_iter()
        .filter_map(|(thread_id, op)| matches!(op, Op::Shutdown).then_some(thread_id))
        .collect::<Vec<_>>();
    let mut expected_shutdown_ids = vec![parent_thread_id, child_thread_id, grandchild_thread_id];
    expected_shutdown_ids.sort_by_key(std::string::ToString::to_string);
    let mut shutdown_ids = shutdown_ids;
    shutdown_ids.sort_by_key(std::string::ToString::to_string);
    assert_eq!(shutdown_ids, expected_shutdown_ids);
}

#[tokio::test]
async fn resume_agent_from_rollout_does_not_reopen_closed_descendants() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let _ = harness
        .control
        .close_agent(child_thread_id)
        .await
        .expect("child close should succeed");
    let _ = harness
        .control
        .shutdown_live_agent(parent_thread_id)
        .await
        .expect("parent shutdown should succeed");

    let resumed_parent_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            parent_thread_id,
            SessionSource::Exec,
        )
        .await
        .expect("single-thread resume should succeed");
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_ne!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let _ = harness
        .control
        .shutdown_agent_tree(parent_thread_id)
        .await
        .expect("tree shutdown after resume should succeed");
}

#[tokio::test]
async fn resume_closed_child_reopens_open_descendants() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let _ = harness
        .control
        .close_agent(child_thread_id)
        .await
        .expect("child close should succeed");

    let resumed_child_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            child_thread_id,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
        )
        .await
        .expect("child resume should succeed");
    assert_eq!(resumed_child_thread_id, child_thread_id);
    assert_ne!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let _ = harness
        .control
        .close_agent(child_thread_id)
        .await
        .expect("child close after resume should succeed");
    let _ = harness
        .control
        .shutdown_live_agent(parent_thread_id)
        .await
        .expect("parent shutdown should succeed");
}

#[tokio::test]
async fn resume_agent_from_rollout_reopens_open_descendants_after_manager_shutdown() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert_eq!(report.submit_failed, Vec::<ThreadId>::new());
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());

    let resumed_parent_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            parent_thread_id,
            SessionSource::Exec,
        )
        .await
        .expect("tree resume should succeed");
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_ne!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let _ = harness
        .control
        .shutdown_agent_tree(parent_thread_id)
        .await
        .expect("tree shutdown after subtree resume should succeed");
}

#[tokio::test]
async fn resume_agent_from_rollout_uses_edge_data_when_descendant_metadata_source_is_stale() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let state_db = grandchild_thread
        .state_db()
        .expect("sqlite state db should be available");
    let mut stale_metadata = state_db
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild metadata query should succeed")
        .expect("grandchild metadata should exist");
    stale_metadata.source =
        serde_json::to_string(&SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 99,
            agent_path: None,
            agent_nickname: None,
            agent_role: Some("worker".to_string()),
        }))
        .expect("stale session source should serialize");
    state_db
        .upsert_thread(&stale_metadata)
        .await
        .expect("stale grandchild metadata should persist");

    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert_eq!(report.submit_failed, Vec::<ThreadId>::new());
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());

    let resumed_parent_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            parent_thread_id,
            SessionSource::Exec,
        )
        .await
        .expect("tree resume should succeed");
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_ne!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let resumed_grandchild_snapshot = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("resumed grandchild thread should exist")
        .config_snapshot()
        .await;
    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: resumed_parent_thread_id,
        depth: resumed_depth,
        ..
    }) = resumed_grandchild_snapshot.session_source
    else {
        panic!("expected thread-spawn sub-agent source");
    };
    assert_eq!(resumed_parent_thread_id, child_thread_id);
    assert_eq!(resumed_depth, 2);

    let _ = harness
        .control
        .shutdown_agent_tree(parent_thread_id)
        .await
        .expect("tree shutdown after subtree resume should succeed");
}

#[tokio::test]
async fn resume_agent_from_rollout_skips_descendants_when_parent_resume_fails() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let child_rollout_path = child_thread
        .rollout_path()
        .expect("child thread should have rollout path");
    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert_eq!(report.submit_failed, Vec::<ThreadId>::new());
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());
    tokio::fs::remove_file(&child_rollout_path)
        .await
        .expect("child rollout path should be removable");

    let resumed_parent_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            parent_thread_id,
            SessionSource::Exec,
        )
        .await
        .expect("root resume should succeed");
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_ne!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let _ = harness
        .control
        .shutdown_agent_tree(parent_thread_id)
        .await
        .expect("tree shutdown after partial subtree resume should succeed");
}
