use super::*;
use crate::AuthManager;
use crate::CodexAuth;
use crate::ThreadManager;
use crate::built_in_model_providers;
use crate::codex::make_session_and_context;
use crate::config::DEFAULT_AGENT_MAX_DEPTH;
use crate::config::types::ShellEnvironmentPolicy;
use crate::function_tool::FunctionCallError;
use crate::protocol::AgentStatus;
use crate::protocol::AskForApproval;
use crate::protocol::EventMsg;
use crate::protocol::FileSystemSandboxPolicy;
use crate::protocol::NetworkSandboxPolicy;
use crate::protocol::Op;
use crate::protocol::SandboxPolicy;
use crate::protocol::SessionSource;
use crate::protocol::SubAgentSource;
use crate::protocol::TurnCompleteEvent;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;
use crate::tools::context::ToolOutput;
use crate::tools::handlers::InspectAgentTreeHandler;
use crate::tools::handlers::multi_agents_v2::AssignTaskHandler as AssignTaskHandlerV2;
use crate::tools::handlers::multi_agents_v2::CloseAgentHandler as CloseAgentHandlerV2;
use crate::tools::handlers::multi_agents_v2::ListAgentsHandler as ListAgentsHandlerV2;
use crate::tools::handlers::multi_agents_v2::SendMessageHandler as SendMessageHandlerV2;
use crate::tools::handlers::multi_agents_v2::SpawnAgentHandler as SpawnAgentHandlerV2;
use crate::tools::handlers::multi_agents_v2::WaitAgentHandler as WaitAgentHandlerV2;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_features::Feature;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::user_input::UserInput;
use core_test_support::TempDirExt;
use pretty_assertions::assert_eq;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

fn invocation(
    session: Arc<crate::codex::Session>,
    turn: Arc<TurnContext>,
    tool_name: &str,
    payload: ToolPayload,
) -> ToolInvocation {
    ToolInvocation {
        session,
        turn,
        tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
        call_id: "call-1".to_string(),
        tool_name: tool_name.to_string(),
        tool_namespace: None,
        payload,
    }
}

fn function_payload(args: serde_json::Value) -> ToolPayload {
    ToolPayload::Function {
        arguments: args.to_string(),
    }
}

fn parse_agent_id(id: &str) -> ThreadId {
    ThreadId::from_string(id).expect("agent id should be valid")
}

fn thread_manager() -> ThreadManager {
    ThreadManager::with_models_provider_for_tests(
        CodexAuth::from_api_key("dummy"),
        built_in_model_providers(/* openai_base_url */ /*openai_base_url*/ None)["openai"].clone(),
    )
}

fn history_contains_inter_agent_communication(
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
            ContentItem::InputText { .. } | ContentItem::InputImage { .. } => false,
        })
    })
}

#[derive(Clone, Copy)]
struct NeverEndingTask;

#[async_trait::async_trait]
impl SessionTask for NeverEndingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.multi_agent_never_ending"
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<SessionTaskContext>,
        _ctx: Arc<TurnContext>,
        _input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> Option<String> {
        cancellation_token.cancelled().await;
        None
    }
}

fn expect_text_output<T>(output: T) -> (String, Option<bool>)
where
    T: ToolOutput,
{
    let response = output.to_response_item(
        "call-1",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );
    match response {
        ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } => {
            let content = match output.body {
                FunctionCallOutputBody::Text(text) => text,
                FunctionCallOutputBody::ContentItems(items) => {
                    codex_protocol::models::function_call_output_content_items_to_text(&items)
                        .unwrap_or_default()
                }
            };
            (content, output.success)
        }
        other => panic!("expected function output, got {other:?}"),
    }
}

#[derive(Debug, Deserialize)]
struct ListAgentsResult {
    agents: Vec<ListedAgentResult>,
}

#[derive(Debug, Deserialize)]
struct ListedAgentResult {
    agent_name: String,
    agent_status: serde_json::Value,
    last_task_message: Option<String>,
    has_active_subagents: bool,
    active_subagent_count: usize,
}

#[tokio::test]
async fn handler_rejects_non_function_payloads() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        ToolPayload::Custom {
            input: "hello".to_string(),
        },
    );
    let Err(err) = SpawnAgentHandler.handle(invocation).await else {
        panic!("payload should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "collab handler received unsupported payload".to_string()
        )
    );
}

#[tokio::test]
async fn spawn_agent_rejects_empty_message() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({"message": "   "})),
    );
    let Err(err) = SpawnAgentHandler.handle(invocation).await else {
        panic!("empty message should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("Empty message can't be sent to an agent".to_string())
    );
}

#[tokio::test]
async fn spawn_agent_rejects_when_message_and_items_are_both_set() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "message": "hello",
            "items": [{"type": "mention", "name": "drive", "path": "app://drive"}]
        })),
    );
    let Err(err) = SpawnAgentHandler.handle(invocation).await else {
        panic!("message+items should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "Provide either message or items, but not both".to_string()
        )
    );
}

#[tokio::test]
async fn spawn_agent_uses_explorer_role_and_preserves_approval_policy() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        agent_id: String,
        nickname: Option<String>,
    }

    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let mut config = (*turn.config).clone();
    let provider =
        built_in_model_providers(/* openai_base_url */ /*openai_base_url*/ None)["ollama"].clone();
    config.model_provider_id = "ollama".to_string();
    config.model_provider = provider.clone();
    config
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy should be set");
    turn.approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy should be set");
    turn.provider = provider;
    turn.config = Arc::new(config);

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "message": "inspect this repo",
            "agent_type": "explorer"
        })),
    );
    let output = SpawnAgentHandler
        .handle(invocation)
        .await
        .expect("spawn_agent should succeed");
    let (content, _) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    let agent_id = parse_agent_id(&result.agent_id);
    assert!(
        result
            .nickname
            .as_deref()
            .is_some_and(|nickname| !nickname.is_empty())
    );
    let snapshot = manager
        .get_thread(agent_id)
        .await
        .expect("spawned agent thread should exist")
        .config_snapshot()
        .await;
    assert_eq!(snapshot.approval_policy, AskForApproval::OnRequest);
    assert_eq!(snapshot.model_provider_id, "ollama");
}

#[tokio::test]
async fn spawn_agent_returns_agent_id_without_task_name() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();

    let output = SpawnAgentHandler
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: serde_json::Value =
        serde_json::from_str(&content).expect("spawn_agent result should be json");

    assert!(result["agent_id"].is_string());
    assert!(result.get("task_name").is_none());
    assert!(result.get("nickname").is_some());
    assert_eq!(success, Some(true));
}

#[tokio::test]
async fn multi_agent_v2_spawn_requires_task_name() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    turn.config = Arc::new(config);

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "message": "inspect this repo"
        })),
    );
    let Err(err) = SpawnAgentHandlerV2.handle(invocation).await else {
        panic!("missing task_name should be rejected");
    };
    let FunctionCallError::RespondToModel(message) = err else {
        panic!("missing task_name should surface as a model-facing error");
    };
    assert!(message.contains("missing field `task_name`"));
}

#[tokio::test]
async fn spawn_agent_errors_when_manager_dropped() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({"message": "hello"})),
    );
    let Err(err) = SpawnAgentHandler.handle(invocation).await else {
        panic!("spawn should fail without a manager");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("collab manager unavailable".to_string())
    );
}

#[tokio::test]
async fn multi_agent_v2_spawn_returns_path_and_send_message_accepts_relative_path() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
        nickname: Option<String>,
    }

    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    turn.config = Arc::new(config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let spawn_output = SpawnAgentHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "test_process"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");
    let (content, _) = expect_text_output(spawn_output);
    let spawn_result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn result should parse");
    assert_eq!(spawn_result.task_name, "/root/test_process");
    assert!(spawn_result.nickname.is_some());

    let child_thread_id = session
        .services
        .agent_control
        .resolve_agent_reference(
            session.conversation_id,
            &turn.session_source,
            "test_process",
        )
        .await
        .expect("relative path should resolve");
    let child_snapshot = manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist")
        .config_snapshot()
        .await;
    assert_eq!(
        child_snapshot.session_source.get_agent_path().as_deref(),
        Some("/root/test_process")
    );
    assert!(manager.captured_ops().iter().any(|(id, op)| {
        *id == child_thread_id
            && matches!(
                op,
                Op::InterAgentCommunication { communication }
                    if communication.author == AgentPath::root()
                        && communication.recipient.as_str() == "/root/test_process"
                        && communication.other_recipients.is_empty()
                        && communication.content == "inspect this repo"
                        && communication.trigger_turn
            )
    }));

    SendMessageHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "send_message",
            function_payload(json!({
                "target": "test_process",
                "items": [{"type": "text", "text": "continue"}]
            })),
        ))
        .await
        .expect("send_message should accept v2 path");

    assert!(manager.captured_ops().iter().any(|(id, op)| {
        *id == child_thread_id
            && matches!(
                op,
                Op::InterAgentCommunication { communication }
                    if communication.author == AgentPath::root()
                        && communication.recipient.as_str() == "/root/test_process"
                        && communication.other_recipients.is_empty()
                        && communication.content == "continue"
                        && !communication.trigger_turn
            )
    }));
}

#[tokio::test]
async fn multi_agent_v2_send_message_accepts_root_target_from_child() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    turn.config = Arc::new(config);

    let child_path = AgentPath::try_from("/root/worker").expect("agent path");
    let child_thread_id = session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            (*turn.config).clone(),
            vec![UserInput::Text {
                text: "inspect this repo".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: Some(child_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("worker spawn should succeed")
        .thread_id;
    session.conversation_id = child_thread_id;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(child_path.clone()),
        agent_nickname: None,
        agent_role: None,
    });

    SendMessageHandlerV2
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "send_message",
            function_payload(json!({
                "target": "/root",
                "items": [{"type": "text", "text": "done"}]
            })),
        ))
        .await
        .expect("send_message should accept the root agent path");

    assert!(manager.captured_ops().iter().any(|(id, op)| {
        *id == root.thread_id
            && matches!(
                op,
                Op::InterAgentCommunication { communication }
                    if communication.author == child_path
                        && communication.recipient == AgentPath::root()
                        && communication.other_recipients.is_empty()
                        && communication.content == "done"
                        && !communication.trigger_turn
            )
    }));
}

#[tokio::test]
async fn multi_agent_v2_list_agents_returns_completed_status_and_last_task_message() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;
    let mut config = (*turn.config).clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    turn.config = Arc::new(config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let spawn_output = SpawnAgentHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");
    let _ = expect_text_output(spawn_output);

    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.conversation_id, &turn.session_source, "worker")
        .await
        .expect("worker path should resolve");
    let child_thread = manager
        .get_thread(agent_id)
        .await
        .expect("child thread should exist");
    let child_turn = child_thread.codex.session.new_default_turn().await;
    child_thread
        .codex
        .session
        .send_event(
            child_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: child_turn.sub_id.clone(),
                last_agent_message: Some("done".to_string()),
                compaction_events_in_turn: 0,
            }),
        )
        .await;

    let output = ListAgentsHandlerV2
        .handle(invocation(
            session,
            turn,
            "list_agents",
            function_payload(json!({})),
        ))
        .await
        .expect("list_agents should succeed");
    let (content, success) = expect_text_output(output);
    let result: ListAgentsResult =
        serde_json::from_str(&content).expect("list_agents result should be json");

    let agent_names = result
        .agents
        .iter()
        .map(|agent| agent.agent_name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(agent_names, vec!["/root", "/root/worker"]);
    let root_agent = result
        .agents
        .iter()
        .find(|agent| agent.agent_name == "/root")
        .expect("root agent should be listed");
    assert_eq!(root_agent.last_task_message.as_deref(), Some("Main thread"));
    assert!(!root_agent.has_active_subagents);
    assert_eq!(root_agent.active_subagent_count, 0);
    let worker = result
        .agents
        .iter()
        .find(|agent| agent.agent_name == "/root/worker")
        .expect("worker agent should be listed");
    assert_eq!(worker.agent_status, json!({"completed": "done"}));
    assert_eq!(
        worker.last_task_message.as_deref(),
        Some("inspect this repo")
    );
    assert!(!worker.has_active_subagents);
    assert_eq!(worker.active_subagent_count, 0);
    assert_eq!(success, Some(true));
}

#[tokio::test]
async fn multi_agent_v2_list_agents_filters_by_relative_path_prefix() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;
    let mut config = (*turn.config).clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    turn.config = Arc::new(config.clone());

    let researcher_path = AgentPath::from_string("/root/researcher".to_string()).expect("path");
    let worker_path = AgentPath::from_string("/root/researcher/worker".to_string()).expect("path");
    let _ = session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            config.clone(),
            vec![UserInput::Text {
                text: "research".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: Some(researcher_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("researcher agent should spawn");
    session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            config,
            vec![UserInput::Text {
                text: "build".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 2,
                agent_path: Some(worker_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("worker agent should spawn");

    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(researcher_path),
        agent_nickname: None,
        agent_role: None,
    });

    let output = ListAgentsHandlerV2
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "list_agents",
            function_payload(json!({
                "path_prefix": "worker"
            })),
        ))
        .await
        .expect("list_agents should succeed");
    let (content, _) = expect_text_output(output);
    let result: ListAgentsResult =
        serde_json::from_str(&content).expect("list_agents result should be json");

    assert_eq!(result.agents.len(), 1);
    assert_eq!(result.agents[0].agent_name, worker_path.as_str());
    assert_eq!(result.agents[0].last_task_message.as_deref(), Some("build"));
    assert!(!result.agents[0].has_active_subagents);
    assert_eq!(result.agents[0].active_subagent_count, 0);
}

#[tokio::test]
async fn multi_agent_v2_list_agents_keeps_active_descendant_hint_under_path_filter() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;
    let mut config = (*turn.config).clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    turn.config = Arc::new(config.clone());

    let researcher_path = AgentPath::from_string("/root/researcher".to_string()).expect("path");
    let worker_path = AgentPath::from_string("/root/researcher/worker".to_string()).expect("path");
    let researcher_id = session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            config.clone(),
            vec![UserInput::Text {
                text: "research".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: Some(researcher_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("researcher agent should spawn")
        .thread_id;
    session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            config,
            vec![UserInput::Text {
                text: "build".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: researcher_id,
                depth: 2,
                agent_path: Some(worker_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("worker agent should spawn");

    let output = ListAgentsHandlerV2
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "list_agents",
            function_payload(json!({
                "path_prefix": "researcher"
            })),
        ))
        .await
        .expect("list_agents should succeed");
    let (content, _) = expect_text_output(output);
    let result: ListAgentsResult =
        serde_json::from_str(&content).expect("list_agents result should be json");

    assert_eq!(result.agents.len(), 2);
    assert_eq!(result.agents[0].agent_name, researcher_path.as_str());
    assert!(result.agents[0].has_active_subagents);
    assert_eq!(result.agents[0].active_subagent_count, 1);
    assert_eq!(result.agents[1].agent_name, worker_path.as_str());
    assert!(!result.agents[1].has_active_subagents);
    assert_eq!(result.agents[1].active_subagent_count, 0);
}

#[tokio::test]
async fn multi_agent_v2_list_agents_omits_closed_agents() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;
    let mut config = (*turn.config).clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    turn.config = Arc::new(config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let spawn_output = SpawnAgentHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");
    let _ = expect_text_output(spawn_output);

    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.conversation_id, &turn.session_source, "worker")
        .await
        .expect("worker path should resolve");
    session
        .services
        .agent_control
        .close_agent(agent_id)
        .await
        .expect("close_agent should succeed");

    let output = ListAgentsHandlerV2
        .handle(invocation(
            session,
            turn,
            "list_agents",
            function_payload(json!({})),
        ))
        .await
        .expect("list_agents should succeed");
    let (content, _) = expect_text_output(output);
    let result: ListAgentsResult =
        serde_json::from_str(&content).expect("list_agents result should be json");

    assert_eq!(result.agents.len(), 1);
    assert_eq!(result.agents[0].agent_name, "/root");
    assert_eq!(
        result.agents[0].last_task_message.as_deref(),
        Some("Main thread")
    );
    assert!(!result.agents[0].has_active_subagents);
    assert_eq!(result.agents[0].active_subagent_count, 0);
}

#[tokio::test]
async fn multi_agent_v2_list_agents_flags_active_descendants() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;
    let mut config = (*turn.config).clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    turn.config = Arc::new(config.clone());

    let researcher_path = AgentPath::from_string("/root/researcher".to_string()).expect("path");
    let worker_path = AgentPath::from_string("/root/researcher/worker".to_string()).expect("path");
    let researcher_id = session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            config.clone(),
            vec![UserInput::Text {
                text: "research".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: Some(researcher_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("researcher agent should spawn")
        .thread_id;
    session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            config,
            vec![UserInput::Text {
                text: "build".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: researcher_id,
                depth: 2,
                agent_path: Some(worker_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("worker agent should spawn");

    let researcher_thread = manager
        .get_thread(researcher_id)
        .await
        .expect("researcher thread should exist");
    let researcher_turn = researcher_thread.codex.session.new_default_turn().await;
    researcher_thread
        .codex
        .session
        .send_event(
            researcher_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: researcher_turn.sub_id.clone(),
                last_agent_message: Some("done".to_string()),
                compaction_events_in_turn: 0,
            }),
        )
        .await;

    let output = ListAgentsHandlerV2
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "list_agents",
            function_payload(json!({})),
        ))
        .await
        .expect("list_agents should succeed");
    let (content, _) = expect_text_output(output);
    let result: ListAgentsResult =
        serde_json::from_str(&content).expect("list_agents result should be json");

    let researcher = result
        .agents
        .iter()
        .find(|agent| agent.agent_name == researcher_path.as_str())
        .expect("researcher should be listed");
    assert_eq!(researcher.agent_status, json!({"completed": "done"}));
    assert!(researcher.has_active_subagents);
    assert_eq!(researcher.active_subagent_count, 1);
    let root_agent = result
        .agents
        .iter()
        .find(|agent| agent.agent_name == "/root")
        .expect("root agent should be listed");
    assert!(root_agent.has_active_subagents);
    assert_eq!(root_agent.active_subagent_count, 1);
}

#[tokio::test]
async fn inspect_agent_tree_defaults_to_current_subtree_for_live_nested_agents() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();

    let config = (*turn.config).clone();
    let researcher_path = AgentPath::from_string("/root/researcher".to_string()).expect("path");
    let worker_path = AgentPath::from_string("/root/researcher/worker".to_string()).expect("path");
    let researcher_id = session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            config.clone(),
            vec![UserInput::Text {
                text: "research".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: Some(researcher_path.clone()),
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("researcher agent should spawn")
        .thread_id;
    session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            config,
            vec![UserInput::Text {
                text: "build".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: researcher_id,
                depth: 2,
                agent_path: Some(worker_path.clone()),
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("worker agent should spawn");

    session.conversation_id = researcher_id;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(researcher_path.clone()),
        agent_nickname: None,
        agent_role: Some("explorer".to_string()),
    });

    let output = InspectAgentTreeHandler
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "inspect_agent_tree",
            function_payload(json!({})),
        ))
        .await
        .expect("inspect_agent_tree should succeed");
    let (content, success) = expect_text_output(output);
    let result: serde_json::Value =
        serde_json::from_str(&content).expect("inspect_agent_tree result should be json");

    assert_eq!(result["root_agent_name"], json!(researcher_path.as_str()));
    assert_eq!(result["scope_applied"], json!("live"));
    assert_eq!(result["agent_roots_applied"], json!([]));
    assert_eq!(result["summary"]["total_agents"], json!(2));
    assert_eq!(result["summary"]["live_agents"], json!(2));
    assert_eq!(result["summary"]["stale_agents"], json!(0));
    assert_eq!(
        result["agents"][0]["agent_name"],
        json!(researcher_path.as_str())
    );
    assert_eq!(result["agents"][0]["depth"], json!(0));
    assert_eq!(result["agents"][0]["session_state"], json!("live"));
    assert_eq!(result["agents"][0]["role"], json!("explorer"));
    assert_eq!(result["agents"][0]["direct_child_count"], json!(1));
    assert_eq!(result["agents"][0]["descendant_count"], json!(1));
    assert_eq!(
        result["agents"][0]["last_task_message_preview"],
        json!("research")
    );
    assert_eq!(
        result["agents"][1]["agent_name"],
        json!(worker_path.as_str())
    );
    assert_eq!(result["agents"][1]["depth"], json!(1));
    assert_eq!(result["agents"][1]["session_state"], json!("live"));
    assert_eq!(result["agents"][1]["role"], json!("worker"));
    assert_eq!(success, Some(true));
}

#[tokio::test]
async fn inspect_agent_tree_can_mix_live_and_stale_descendants() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;

    let config = (*turn.config).clone();
    let researcher_path = AgentPath::from_string("/root/researcher".to_string()).expect("path");
    let worker_path = AgentPath::from_string("/root/researcher/worker".to_string()).expect("path");
    let researcher_id = session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            config.clone(),
            vec![UserInput::Text {
                text: "research".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: Some(researcher_path.clone()),
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("researcher agent should spawn")
        .thread_id;
    let worker_id = session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            config,
            vec![UserInput::Text {
                text: "build".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: researcher_id,
                depth: 2,
                agent_path: Some(worker_path.clone()),
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("worker agent should spawn")
        .thread_id;
    session
        .services
        .agent_control
        .close_agent(worker_id)
        .await
        .expect("worker close should succeed");

    let output = InspectAgentTreeHandler
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "inspect_agent_tree",
            function_payload(json!({
                "target": "researcher",
                "scope": "all"
            })),
        ))
        .await
        .expect("inspect_agent_tree should succeed");
    let (content, success) = expect_text_output(output);
    let result: serde_json::Value =
        serde_json::from_str(&content).expect("inspect_agent_tree result should be json");

    assert_eq!(result["root_agent_name"], json!(researcher_path.as_str()));
    assert_eq!(result["scope_applied"], json!("all"));
    assert_eq!(result["agent_roots_applied"], json!([]));
    assert_eq!(result["summary"]["total_agents"], json!(2));
    assert_eq!(result["summary"]["live_agents"], json!(1));
    assert_eq!(result["summary"]["stale_agents"], json!(1));
    assert_eq!(
        result["agents"][0]["agent_name"],
        json!(researcher_path.as_str())
    );
    assert_eq!(result["agents"][0]["session_state"], json!("live"));
    assert_eq!(result["agents"][0]["direct_child_count"], json!(1));
    assert_eq!(
        result["agents"][1]["agent_name"],
        json!(worker_path.as_str())
    );
    assert_eq!(result["agents"][1]["session_state"], json!("stale"));
    assert!(result["agents"][1]["agent_status"].is_null());
    assert_eq!(success, Some(true));
}

#[tokio::test]
async fn inspect_agent_tree_filters_to_requested_agent_branches() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;

    let config = (*turn.config).clone();
    let researcher_path = AgentPath::from_string("/root/researcher".to_string()).expect("path");
    let reviewer_path = AgentPath::from_string("/root/reviewer".to_string()).expect("path");
    let worker_path = AgentPath::from_string("/root/researcher/worker".to_string()).expect("path");
    session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            config.clone(),
            vec![UserInput::Text {
                text: "research".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: Some(researcher_path.clone()),
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("researcher agent should spawn");
    session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            config.clone(),
            vec![UserInput::Text {
                text: "review".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: Some(reviewer_path),
                agent_nickname: None,
                agent_role: Some("reviewer".to_string()),
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("reviewer agent should spawn");
    session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            config,
            vec![UserInput::Text {
                text: "build".to_string(),
                text_elements: Vec::new(),
            }]
            .into(),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 2,
                agent_path: Some(worker_path.clone()),
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("worker agent should spawn");

    let output = InspectAgentTreeHandler
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "inspect_agent_tree",
            function_payload(json!({
                "agent_roots": ["researcher"]
            })),
        ))
        .await
        .expect("inspect_agent_tree should succeed");
    let (content, success) = expect_text_output(output);
    let result: serde_json::Value =
        serde_json::from_str(&content).expect("inspect_agent_tree result should be json");

    assert_eq!(
        result["agent_roots_applied"],
        json!([researcher_path.as_str()])
    );
    assert_eq!(result["summary"]["total_agents"], json!(2));
    assert_eq!(result["truncated"], json!(false));
    let agent_names = result["agents"]
        .as_array()
        .expect("agents should be an array")
        .iter()
        .map(|agent| {
            agent["agent_name"]
                .as_str()
                .expect("agent_name should be a string")
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        agent_names,
        vec![researcher_path.to_string(), worker_path.to_string()]
    );
    assert_eq!(success, Some(true));
}

#[tokio::test]
async fn multi_agent_v2_send_message_rejects_structured_items() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;
    let mut config = turn.config.as_ref().clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    turn.config = Arc::new(config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    SpawnAgentHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "boot worker",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.conversation_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");
    let invocation = invocation(
        session,
        turn,
        "send_message",
        function_payload(json!({
            "target": agent_id.to_string(),
            "items": [
                {"type": "mention", "name": "drive", "path": "app://google_drive"},
                {"type": "text", "text": "read the folder"}
            ]
        })),
    );

    let Err(err) = SendMessageHandlerV2.handle(invocation).await else {
        panic!("structured items should be rejected in v2");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "send_message only supports text content in MultiAgentV2 for now".to_string()
        )
    );
}

#[tokio::test]
async fn multi_agent_v2_send_message_missing_agent_path_emits_matching_interaction_end_event() {
    let manager = thread_manager();
    let (mut session, mut turn, mut rx) = crate::codex::make_session_and_context_with_rx().await;
    let target_thread_id = {
        let session = Arc::get_mut(&mut session).expect("session should be uniquely owned");
        session.services.agent_control = manager.agent_control();
        let turn_ref = Arc::get_mut(&mut turn).expect("turn context should be uniquely owned");
        let root = manager
            .start_thread((*turn_ref.config).clone())
            .await
            .expect("root thread should start");
        session.conversation_id = root.thread_id;
        let mut config = (*turn_ref.config).clone();
        config
            .features
            .enable(Feature::MultiAgentV2)
            .expect("test config should allow feature update");
        turn_ref.config = Arc::new(config);
        session
            .services
            .agent_control
            .spawn_agent_with_metadata(
                (*turn_ref.config).clone(),
                vec![UserInput::Text {
                    text: "bootstrap".to_string(),
                    text_elements: Vec::new(),
                }]
                .into(),
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id: root.thread_id,
                    depth: 1,
                    agent_path: None,
                    agent_nickname: None,
                    agent_role: None,
                })),
                crate::agent::control::SpawnAgentOptions::default(),
            )
            .await
            .expect("missing-path agent should spawn")
            .thread_id
    };

    let invocation = invocation(
        session.clone(),
        turn.clone(),
        "send_message",
        function_payload(json!({
            "target": target_thread_id.to_string(),
            "items": [{"type": "text", "text": "hello"}]
        })),
    );
    let Err(err) = SendMessageHandlerV2.handle(invocation).await else {
        panic!("send to missing path should fail");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("target agent is missing an agent_path".to_string())
    );

    let mut begin = None;
    let mut end = None;
    for _ in 0..4 {
        let event = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("interaction event should arrive")
            .expect("event channel should remain open");
        match event.msg {
            EventMsg::CollabAgentInteractionBegin(event) => {
                if event.call_id == "call-1" {
                    assert!(end.is_none(), "interaction end should come after begin");
                    begin = Some(event);
                }
            }
            EventMsg::CollabAgentInteractionEnd(event) => {
                if event.call_id == "call-1" {
                    end = Some(event);
                }
            }
            _ => {}
        }
        if begin.is_some() && end.is_some() {
            break;
        }
    }

    let begin = begin.expect("begin event should be emitted for send_message");
    let end = end.expect("matching end event should be emitted for send_message");

    assert_eq!(begin.call_id, "call-1");
    assert_eq!(begin.sender_thread_id, end.sender_thread_id);
    assert_eq!(begin.sender_thread_id, session.conversation_id);
    assert_eq!(begin.receiver_thread_id, end.receiver_thread_id);
    assert_eq!(begin.receiver_thread_id, target_thread_id);
    assert_eq!(begin.prompt, "hello");
    assert_eq!(begin.prompt, end.prompt);
}

#[tokio::test]
async fn multi_agent_v2_send_message_interrupts_busy_child_without_triggering_turn() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;
    let mut config = turn.config.as_ref().clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    turn.config = Arc::new(config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    SpawnAgentHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "boot worker",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.conversation_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");
    let thread = manager
        .get_thread(agent_id)
        .await
        .expect("worker thread should exist");

    let active_turn = thread.codex.session.new_default_turn().await;
    thread
        .codex
        .session
        .spawn_task(
            Arc::clone(&active_turn),
            vec![UserInput::Text {
                text: "working".to_string(),
                text_elements: Vec::new(),
            }],
            NeverEndingTask,
        )
        .await;

    SendMessageHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "send_message",
            function_payload(json!({
                "target": agent_id.to_string(),
                "items": [{"type": "text", "text": "continue"}],
                "interrupt": true
            })),
        ))
        .await
        .expect("interrupting v2 send_message should succeed");

    let ops = manager.captured_ops();
    let ops_for_agent: Vec<&Op> = ops
        .iter()
        .filter_map(|(id, op)| (*id == agent_id).then_some(op))
        .collect();
    assert!(ops_for_agent.iter().any(|op| matches!(op, Op::Interrupt)));
    assert!(ops_for_agent.iter().any(|op| {
        matches!(
            op,
            Op::InterAgentCommunication { communication }
                if communication.author == AgentPath::root()
                    && communication.recipient.as_str() == "/root/worker"
                    && communication.other_recipients.is_empty()
                    && communication.content == "continue"
                    && !communication.trigger_turn
        )
    }));

    timeout(Duration::from_secs(5), async {
        loop {
            if !thread
                .codex
                .session
                .has_queued_response_items_for_next_turn()
                .await
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            let history_items = thread
                .codex
                .session
                .clone_history()
                .await
                .raw_items()
                .to_vec();
            let saw_envelope = history_contains_inter_agent_communication(
                &history_items,
                &InterAgentCommunication::new(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    Vec::new(),
                    "continue".to_string(),
                    /*trigger_turn*/ false,
                ),
            );
            let saw_user_message = history_items.iter().any(|item| {
                matches!(
                    item,
                    ResponseItem::Message { role, content, .. }
                        if role == "user"
                            && content.iter().any(|content_item| matches!(
                                content_item,
                                ContentItem::InputText { text } if text == "continue"
                            ))
                )
            });
            if saw_envelope && !saw_user_message {
                panic!("send_message should not materialize the envelope into history");
            }
            if !saw_user_message {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("interrupting v2 send_message should queue the redirected message without a turn");

    let _ = thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");
}

#[tokio::test]
async fn multi_agent_v2_assign_task_interrupts_busy_child_without_losing_message() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;
    let mut config = turn.config.as_ref().clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    turn.config = Arc::new(config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    SpawnAgentHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "boot worker",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.conversation_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");
    let thread = manager
        .get_thread(agent_id)
        .await
        .expect("worker thread should exist");

    let active_turn = thread.codex.session.new_default_turn().await;
    thread
        .codex
        .session
        .spawn_task(
            Arc::clone(&active_turn),
            vec![UserInput::Text {
                text: "working".to_string(),
                text_elements: Vec::new(),
            }],
            NeverEndingTask,
        )
        .await;

    AssignTaskHandlerV2
        .handle(invocation(
            session,
            turn,
            "assign_task",
            function_payload(json!({
                "target": agent_id.to_string(),
                "items": [{"type": "text", "text": "continue"}],
                "interrupt": true
            })),
        ))
        .await
        .expect("interrupting v2 assign_task should succeed");

    let ops = manager.captured_ops();
    let ops_for_agent: Vec<&Op> = ops
        .iter()
        .filter_map(|(id, op)| (*id == agent_id).then_some(op))
        .collect();
    assert!(ops_for_agent.iter().any(|op| matches!(op, Op::Interrupt)));
    assert!(ops_for_agent.iter().any(|op| {
        matches!(
            op,
            Op::InterAgentCommunication { communication }
                if communication.author == AgentPath::root()
                    && communication.recipient.as_str() == "/root/worker"
                    && communication.other_recipients.is_empty()
                    && communication.content == "continue"
        )
    }));

    timeout(Duration::from_secs(5), async {
        loop {
            let history_items = thread
                .codex
                .session
                .clone_history()
                .await
                .raw_items()
                .to_vec();
            let saw_envelope = history_contains_inter_agent_communication(
                &history_items,
                &InterAgentCommunication::new(
                    AgentPath::root(),
                    AgentPath::try_from("/root/worker").expect("agent path"),
                    Vec::new(),
                    "continue".to_string(),
                    /*trigger_turn*/ true,
                ),
            );
            let saw_user_message = history_items.iter().any(|item| {
                matches!(
                    item,
                    ResponseItem::Message { role, content, .. }
                        if role == "user"
                            && content.iter().any(|content_item| matches!(
                                content_item,
                                ContentItem::InputText { text } if text == "continue"
                            ))
                )
            });
            if saw_envelope && !saw_user_message {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("interrupting v2 assign_task should preserve the redirected message");

    let _ = thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");
}

#[tokio::test]
async fn multi_agent_v2_spawn_includes_agent_id_key_when_named() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    turn.config = Arc::new(config);

    let output = SpawnAgentHandlerV2
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "test_process"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: serde_json::Value =
        serde_json::from_str(&content).expect("spawn_agent result should be json");

    assert_eq!(result["agent_id"], serde_json::Value::Null);
    assert_eq!(result["task_name"], "/root/test_process");
    assert!(result.get("nickname").is_some());
    assert_eq!(success, Some(true));
}

#[tokio::test]
async fn multi_agent_v2_spawn_surfaces_task_name_validation_errors() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    turn.config = Arc::new(config);

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "message": "inspect this repo",
            "task_name": "BadName"
        })),
    );
    let Err(err) = SpawnAgentHandlerV2.handle(invocation).await else {
        panic!("invalid agent name should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "agent_name must use only lowercase letters, digits, and underscores".to_string()
        )
    );
}

#[tokio::test]
async fn spawn_agent_reapplies_runtime_sandbox_after_role_config() {
    fn pick_allowed_sandbox_policy(
        constraint: &crate::config::Constrained<SandboxPolicy>,
        base: SandboxPolicy,
    ) -> SandboxPolicy {
        let candidates = [
            SandboxPolicy::DangerFullAccess,
            SandboxPolicy::new_workspace_write_policy(),
            SandboxPolicy::new_read_only_policy(),
        ];
        candidates
            .into_iter()
            .find(|candidate| *candidate != base && constraint.can_set(candidate).is_ok())
            .unwrap_or(base)
    }

    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        agent_id: String,
        nickname: Option<String>,
    }

    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let expected_sandbox = pick_allowed_sandbox_policy(
        &turn.config.permissions.sandbox_policy,
        turn.config.permissions.sandbox_policy.get().clone(),
    );
    let expected_file_system_sandbox_policy =
        FileSystemSandboxPolicy::from_legacy_sandbox_policy(&expected_sandbox, &turn.cwd);
    let expected_network_sandbox_policy = NetworkSandboxPolicy::from(&expected_sandbox);
    turn.approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy should be set");
    turn.sandbox_policy
        .set(expected_sandbox.clone())
        .expect("sandbox policy should be set");
    turn.file_system_sandbox_policy = expected_file_system_sandbox_policy.clone();
    turn.network_sandbox_policy = expected_network_sandbox_policy;
    assert_ne!(
        expected_sandbox,
        turn.config.permissions.sandbox_policy.get().clone(),
        "test requires a runtime sandbox override that differs from base config"
    );

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "message": "await this command",
            "agent_type": "explorer"
        })),
    );
    let output = SpawnAgentHandler
        .handle(invocation)
        .await
        .expect("spawn_agent should succeed");
    let (content, _) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    let agent_id = parse_agent_id(&result.agent_id);
    assert!(
        result
            .nickname
            .as_deref()
            .is_some_and(|nickname| !nickname.is_empty())
    );

    let snapshot = manager
        .get_thread(agent_id)
        .await
        .expect("spawned agent thread should exist")
        .config_snapshot()
        .await;
    assert_eq!(snapshot.sandbox_policy, expected_sandbox);
    assert_eq!(snapshot.approval_policy, AskForApproval::OnRequest);
    let child_thread = manager
        .get_thread(agent_id)
        .await
        .expect("spawned agent thread should exist");
    let child_turn = child_thread.codex.session.new_default_turn().await;
    assert_eq!(
        child_turn.file_system_sandbox_policy,
        expected_file_system_sandbox_policy
    );
    assert_eq!(
        child_turn.network_sandbox_policy,
        expected_network_sandbox_policy
    );
}

#[tokio::test]
async fn spawn_agent_rejects_when_depth_limit_exceeded() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();

    let max_depth = turn.config.agent_max_depth;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: session.conversation_id,
        depth: max_depth,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({"message": "hello"})),
    );
    let Err(err) = SpawnAgentHandler.handle(invocation).await else {
        panic!("spawn should fail when depth limit exceeded");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "Agent depth limit reached. Solve the task yourself.".to_string()
        )
    );
}

#[tokio::test]
async fn spawn_agent_allows_depth_up_to_configured_max_depth() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        agent_id: String,
        nickname: Option<String>,
    }

    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();

    let mut config = (*turn.config).clone();
    config.agent_max_depth = DEFAULT_AGENT_MAX_DEPTH + 1;
    turn.config = Arc::new(config);
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: session.conversation_id,
        depth: DEFAULT_AGENT_MAX_DEPTH,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({"message": "hello"})),
    );
    let output = SpawnAgentHandler
        .handle(invocation)
        .await
        .expect("spawn should succeed within configured depth");
    let (content, success) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    assert!(!result.agent_id.is_empty());
    assert!(
        result
            .nickname
            .as_deref()
            .is_some_and(|nickname| !nickname.is_empty())
    );
    assert_eq!(success, Some(true));
}

#[tokio::test]
async fn send_input_rejects_empty_message() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "send_input",
        function_payload(json!({"target": ThreadId::new().to_string(), "message": ""})),
    );
    let Err(err) = SendInputHandler.handle(invocation).await else {
        panic!("empty message should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("Empty message can't be sent to an agent".to_string())
    );
}

#[tokio::test]
async fn send_input_rejects_when_message_and_items_are_both_set() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "send_input",
        function_payload(json!({
            "target": ThreadId::new().to_string(),
            "message": "hello",
            "items": [{"type": "mention", "name": "drive", "path": "app://drive"}]
        })),
    );
    let Err(err) = SendInputHandler.handle(invocation).await else {
        panic!("message+items should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "Provide either message or items, but not both".to_string()
        )
    );
}

#[tokio::test]
async fn send_input_rejects_invalid_id() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "send_input",
        function_payload(json!({"target": "not-a-uuid", "message": "hi"})),
    );
    let Err(err) = SendInputHandler.handle(invocation).await else {
        panic!("invalid id should be rejected");
    };
    let FunctionCallError::RespondToModel(msg) = err else {
        panic!("expected respond-to-model error");
    };
    assert_eq!(
        msg,
        "agent_name must use only lowercase letters, digits, and underscores"
    );
}

#[tokio::test]
async fn send_input_reports_missing_agent() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let agent_id = ThreadId::new();
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "send_input",
        function_payload(json!({"target": agent_id.to_string(), "message": "hi"})),
    );
    let Err(err) = SendInputHandler.handle(invocation).await else {
        panic!("missing agent should be reported");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(format!("agent with id {agent_id} not found"))
    );
}

#[tokio::test]
async fn send_input_accepts_task_name_target() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    turn.config = Arc::new(config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let _spawn_output = SpawnAgentHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");

    let output = SendInputHandler
        .handle(invocation(
            session,
            turn,
            "send_input",
            function_payload(json!({
                "target": "worker",
                "message": "hi"
            })),
        ))
        .await
        .expect("send_input should accept task-name targets");

    let (_content, success) = expect_text_output(output);
    assert_eq!(success, Some(true));
}

#[tokio::test]
async fn send_input_interrupts_before_prompt() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let config = turn.config.as_ref().clone();
    let thread = manager.start_thread(config).await.expect("start thread");
    let agent_id = thread.thread_id;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "send_input",
        function_payload(json!({
            "target": agent_id.to_string(),
            "message": "hi",
            "interrupt": true
        })),
    );
    SendInputHandler
        .handle(invocation)
        .await
        .expect("send_input should succeed");

    let ops = manager.captured_ops();
    let ops_for_agent: Vec<&Op> = ops
        .iter()
        .filter_map(|(id, op)| (*id == agent_id).then_some(op))
        .collect();
    assert_eq!(ops_for_agent.len(), 2);
    assert!(matches!(ops_for_agent[0], Op::Interrupt));
    assert!(matches!(ops_for_agent[1], Op::UserInput { .. }));

    let _ = thread
        .thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");
}

#[tokio::test]
async fn send_input_accepts_structured_items() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let config = turn.config.as_ref().clone();
    let thread = manager.start_thread(config).await.expect("start thread");
    let agent_id = thread.thread_id;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "send_input",
        function_payload(json!({
            "target": agent_id.to_string(),
            "items": [
                {"type": "mention", "name": "drive", "path": "app://google_drive"},
                {"type": "text", "text": "read the folder"}
            ]
        })),
    );
    SendInputHandler
        .handle(invocation)
        .await
        .expect("send_input should succeed");

    let expected = Op::UserInput {
        items: vec![
            UserInput::Mention {
                name: "drive".to_string(),
                path: "app://google_drive".to_string(),
            },
            UserInput::Text {
                text: "read the folder".to_string(),
                text_elements: Vec::new(),
            },
        ],
        final_output_json_schema: None,
    };
    let captured = manager
        .captured_ops()
        .into_iter()
        .find(|(id, op)| *id == agent_id && *op == expected);
    assert_eq!(captured, Some((agent_id, expected)));

    let _ = thread
        .thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");
}

#[tokio::test]
async fn resume_agent_rejects_invalid_id() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "resume_agent",
        function_payload(json!({"id": "not-a-uuid"})),
    );
    let Err(err) = ResumeAgentHandler.handle(invocation).await else {
        panic!("invalid id should be rejected");
    };
    let FunctionCallError::RespondToModel(msg) = err else {
        panic!("expected respond-to-model error");
    };
    assert!(msg.starts_with("invalid agent id not-a-uuid:"));
}

#[tokio::test]
async fn resume_agent_reports_missing_agent() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let agent_id = ThreadId::new();
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "resume_agent",
        function_payload(json!({"id": agent_id.to_string()})),
    );
    let Err(err) = ResumeAgentHandler.handle(invocation).await else {
        panic!("missing agent should be reported");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(format!("agent with id {agent_id} not found"))
    );
}

#[tokio::test]
async fn resume_agent_noops_for_active_agent() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let config = turn.config.as_ref().clone();
    let thread = manager.start_thread(config).await.expect("start thread");
    let agent_id = thread.thread_id;
    let status_before = manager.agent_control().get_status(agent_id).await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "resume_agent",
        function_payload(json!({"id": agent_id.to_string()})),
    );

    let output = ResumeAgentHandler
        .handle(invocation)
        .await
        .expect("resume_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: resume_agent::ResumeAgentResult =
        serde_json::from_str(&content).expect("resume_agent result should be json");
    assert_eq!(result.status, status_before);
    assert_eq!(success, Some(true));

    let thread_ids = manager.list_thread_ids().await;
    assert_eq!(thread_ids, vec![agent_id]);

    let _ = thread
        .thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");
}

#[tokio::test]
async fn resume_agent_restores_closed_agent_and_accepts_send_input() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let config = turn.config.as_ref().clone();
    let thread = manager
        .resume_thread_with_history(
            config,
            InitialHistory::Forked(vec![RolloutItem::ResponseItem(ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "materialized".to_string(),
                }],
                end_turn: None,
                phase: None,
            })]),
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy")),
            /*persist_extended_history*/ false,
            /*parent_trace*/ None,
        )
        .await
        .expect("start thread");
    let agent_id = thread.thread_id;
    let _ = manager
        .agent_control()
        .shutdown_live_agent(agent_id)
        .await
        .expect("shutdown agent");
    assert_eq!(
        manager.agent_control().get_status(agent_id).await,
        AgentStatus::NotFound
    );
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let resume_invocation = invocation(
        session.clone(),
        turn.clone(),
        "resume_agent",
        function_payload(json!({"id": agent_id.to_string()})),
    );
    let output = ResumeAgentHandler
        .handle(resume_invocation)
        .await
        .expect("resume_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: resume_agent::ResumeAgentResult =
        serde_json::from_str(&content).expect("resume_agent result should be json");
    assert_ne!(result.status, AgentStatus::NotFound);
    assert_eq!(success, Some(true));

    let send_invocation = invocation(
        session,
        turn,
        "send_input",
        function_payload(json!({"target": agent_id.to_string(), "message": "hello"})),
    );
    let output = SendInputHandler
        .handle(send_invocation)
        .await
        .expect("send_input should succeed after resume");
    let (content, success) = expect_text_output(output);
    let result: serde_json::Value =
        serde_json::from_str(&content).expect("send_input result should be json");
    let submission_id = result
        .get("submission_id")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    assert!(!submission_id.is_empty());
    assert_eq!(success, Some(true));

    let _ = manager
        .agent_control()
        .shutdown_live_agent(agent_id)
        .await
        .expect("shutdown resumed agent");
}

#[tokio::test]
async fn resume_agent_rejects_when_depth_limit_exceeded() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();

    let max_depth = turn.config.agent_max_depth;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: session.conversation_id,
        depth: max_depth,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "resume_agent",
        function_payload(json!({"id": ThreadId::new().to_string()})),
    );
    let Err(err) = ResumeAgentHandler.handle(invocation).await else {
        panic!("resume should fail when depth limit exceeded");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "Agent depth limit reached. Solve the task yourself.".to_string()
        )
    );
}

#[tokio::test]
async fn wait_agent_rejects_non_positive_timeout() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({
            "targets": [ThreadId::new().to_string()],
            "timeout_ms": 0
        })),
    );
    let Err(err) = WaitAgentHandler.handle(invocation).await else {
        panic!("non-positive timeout should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("timeout_ms must be greater than zero".to_string())
    );
}

#[tokio::test]
async fn wait_agent_reports_missing_named_target() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({"targets": ["invalid"]})),
    );
    let Err(err) = WaitAgentHandler.handle(invocation).await else {
        panic!("missing named target should be reported");
    };
    let FunctionCallError::RespondToModel(msg) = err else {
        panic!("expected respond-to-model error");
    };
    assert_eq!(msg, "live agent path `/root/invalid` not found");
}

#[tokio::test]
async fn wait_agent_rejects_empty_targets() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({"targets": []})),
    );
    let Err(err) = WaitAgentHandler.handle(invocation).await else {
        panic!("empty ids should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("agent targets must be non-empty".to_string())
    );
}

#[tokio::test]
async fn wait_agent_rejects_duplicate_targets() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let agent_id = ThreadId::new().to_string();
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({"targets": [agent_id.clone(), agent_id]})),
    );
    let Err(err) = WaitAgentHandler.handle(invocation).await else {
        panic!("duplicate targets should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("targets must resolve to unique agents".to_string())
    );
}

#[tokio::test]
async fn wait_agent_accepts_legacy_ids_argument() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let id_a = ThreadId::new();
    let id_b = ThreadId::new();
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({
            "ids": [id_a.to_string(), id_b.to_string()],
            "timeout_ms": 1000
        })),
    );
    let output = WaitAgentHandler
        .handle(invocation)
        .await
        .expect("legacy ids alias should be accepted");
    let (content, success) = expect_text_output(output);
    let result: wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        wait::WaitAgentResult {
            message: "Wait completed.".to_string(),
            requested_ids: vec![id_a, id_b],
            pending_ids: Vec::new(),
            completion_reason: codex_protocol::protocol::CollabWaitingCompletionReason::Terminal,
            timed_out: false,
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_accepts_targets_argument() {
    let (mut session, mut turn) = make_session_and_context().await;
    let target = ThreadId::new().to_string();
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    turn.config = Arc::new(config);
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({"targets": [target.clone()]})),
    );
    let output = WaitAgentHandlerV2
        .handle(invocation)
        .await
        .expect("targets should be accepted in v2 mode");
    let (content, success) = expect_text_output(output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult {
            message: "Wait completed.".to_string(),
            timed_out: false,
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn wait_agent_returns_not_found_for_missing_agents() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let id_a = ThreadId::new();
    let id_b = ThreadId::new();
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({
            "targets": [id_a.to_string(), id_b.to_string()],
            "timeout_ms": 1000
        })),
    );
    let output = WaitAgentHandler
        .handle(invocation)
        .await
        .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        wait::WaitAgentResult {
            message: "Wait completed.".to_string(),
            requested_ids: vec![id_a, id_b],
            pending_ids: Vec::new(),
            completion_reason: crate::protocol::CollabWaitingCompletionReason::Terminal,
            timed_out: false
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn wait_agent_times_out_when_status_is_not_final() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let config = turn.config.as_ref().clone();
    let thread = manager.start_thread(config).await.expect("start thread");
    let agent_id = thread.thread_id;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({
            "targets": [agent_id.to_string()],
            "timeout_ms": MIN_WAIT_TIMEOUT_MS
        })),
    );
    let output = WaitAgentHandler
        .handle(invocation)
        .await
        .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        wait::WaitAgentResult {
            message: "Wait timed out.".to_string(),
            requested_ids: vec![agent_id],
            pending_ids: vec![agent_id],
            completion_reason: crate::protocol::CollabWaitingCompletionReason::Timeout,
            timed_out: true
        }
    );
    assert_eq!(success, None);

    let _ = thread
        .thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");
}

#[tokio::test]
async fn wait_agent_clamps_short_timeouts_to_minimum() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let config = turn.config.as_ref().clone();
    let thread = manager.start_thread(config).await.expect("start thread");
    let agent_id = thread.thread_id;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({
            "targets": [agent_id.to_string()],
            "timeout_ms": 10
        })),
    );

    let early = timeout(
        Duration::from_millis(50),
        WaitAgentHandler.handle(invocation),
    )
    .await;
    assert!(
        early.is_err(),
        "wait_agent should not return before the minimum timeout clamp"
    );

    let _ = thread
        .thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");
}

#[tokio::test]
async fn wait_agent_returns_final_status_without_timeout() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let config = turn.config.as_ref().clone();
    let thread = manager.start_thread(config).await.expect("start thread");
    let agent_id = thread.thread_id;
    let mut status_rx = manager
        .agent_control()
        .subscribe_status(agent_id)
        .await
        .expect("subscribe should succeed");

    let _ = thread
        .thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");
    let _ = timeout(Duration::from_secs(1), status_rx.changed())
        .await
        .expect("shutdown status should arrive");

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({
            "targets": [agent_id.to_string()],
            "timeout_ms": 1000
        })),
    );
    let output = WaitAgentHandler
        .handle(invocation)
        .await
        .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        wait::WaitAgentResult {
            message: "Wait completed.".to_string(),
            requested_ids: vec![agent_id],
            pending_ids: Vec::new(),
            completion_reason: crate::protocol::CollabWaitingCompletionReason::Terminal,
            timed_out: false
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_returns_summary_for_named_targets() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    turn.config = Arc::new(config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let spawn_output = SpawnAgentHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "test_process"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");
    let _ = expect_text_output(spawn_output);

    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(
            session.conversation_id,
            &turn.session_source,
            "test_process",
        )
        .await
        .expect("relative path should resolve");
    let mut status_rx = manager
        .agent_control()
        .subscribe_status(agent_id)
        .await
        .expect("subscribe should succeed");

    let child_thread = manager
        .get_thread(agent_id)
        .await
        .expect("child should exist");
    let _ = child_thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");
    let _ = timeout(Duration::from_secs(1), status_rx.changed())
        .await
        .expect("shutdown status should arrive");

    let wait_output = WaitAgentHandlerV2
        .handle(invocation(
            session,
            turn,
            "wait_agent",
            function_payload(json!({
                "targets": ["test_process"],
                "timeout_ms": 1000
            })),
        ))
        .await
        .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(wait_output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult {
            message: "Wait completed.".to_string(),
            timed_out: false,
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_honors_return_when_all() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    turn.config = Arc::new(config.clone());

    let first_thread = manager
        .start_thread(config.clone())
        .await
        .expect("first thread should start");
    let second_thread = manager
        .start_thread(config)
        .await
        .expect("second thread should start");
    let first_id = first_thread.thread_id;
    let second_id = second_thread.thread_id;

    let mut first_status_rx = manager
        .agent_control()
        .subscribe_status(first_id)
        .await
        .expect("subscribe should succeed");
    let _ = first_thread
        .thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");
    let _ = timeout(Duration::from_secs(1), first_status_rx.changed())
        .await
        .expect("shutdown status should arrive");

    let wait_future = WaitAgentHandlerV2.handle(invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({
            "targets": [first_id.to_string(), second_id.to_string()],
            "timeout_ms": 1000,
            "return_when": "all"
        })),
    ));
    tokio::pin!(wait_future);

    let early = timeout(Duration::from_millis(50), &mut wait_future).await;
    assert!(
        early.is_err(),
        "v2 wait_agent should not return after the first terminal status when return_when=all"
    );

    let _ = second_thread
        .thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");

    let output = wait_future.await.expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult {
            message: "Wait completed.".to_string(),
            timed_out: false,
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_does_not_return_completed_content() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    turn.config = Arc::new(config.clone());

    let thread = manager.start_thread(config).await.expect("start thread");
    let agent_id = thread.thread_id;
    let child_turn = thread.thread.codex.session.new_default_turn().await;
    thread
        .thread
        .codex
        .session
        .send_event(
            child_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: child_turn.sub_id.clone(),
                last_agent_message: Some("sensitive child output".to_string()),
                compaction_events_in_turn: 0,
            }),
        )
        .await;

    let output = WaitAgentHandlerV2
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "wait_agent",
            function_payload(json!({
                "targets": [agent_id.to_string()],
                "timeout_ms": 1000
            })),
        ))
        .await
        .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult {
            message: "Wait completed.".to_string(),
            timed_out: false,
        }
    );
    assert!(!content.contains("sensitive child output"));
    assert_eq!(success, None);
}

#[tokio::test]
async fn multi_agent_v2_close_agent_accepts_task_name_target() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    turn.config = Arc::new(config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    SpawnAgentHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");

    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.conversation_id, &turn.session_source, "worker")
        .await
        .expect("worker path should resolve");

    let output = CloseAgentHandlerV2
        .handle(invocation(
            session,
            turn,
            "close_agent",
            function_payload(json!({"target": "worker"})),
        ))
        .await
        .expect("close_agent should succeed for v2 task names");
    let (content, success) = expect_text_output(output);
    let result: close_agent::CloseAgentResult =
        serde_json::from_str(&content).expect("close_agent result should be json");
    assert_ne!(result.previous_status, AgentStatus::NotFound);
    assert_eq!(success, Some(true));
    assert_eq!(
        manager.agent_control().get_status(agent_id).await,
        AgentStatus::NotFound
    );
}

#[tokio::test]
async fn multi_agent_v2_close_agent_rejects_root_target_and_id() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.conversation_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    turn.config = Arc::new(config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let root_path_error = CloseAgentHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "close_agent",
            function_payload(json!({"target": "/root"})),
        ))
        .await
        .expect_err("close_agent should reject the root path");
    assert_eq!(
        root_path_error,
        FunctionCallError::RespondToModel("root is not a spawned agent".to_string())
    );

    let root_id_error = CloseAgentHandlerV2
        .handle(invocation(
            session,
            turn,
            "close_agent",
            function_payload(json!({"target": root.thread_id.to_string()})),
        ))
        .await
        .expect_err("close_agent should reject the root thread id");
    assert_eq!(
        root_id_error,
        FunctionCallError::RespondToModel("root is not a spawned agent".to_string())
    );
}

#[tokio::test]
async fn close_agent_submits_shutdown_and_returns_previous_status() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();
    let config = turn.config.as_ref().clone();
    let thread = manager.start_thread(config).await.expect("start thread");
    let agent_id = thread.thread_id;
    let status_before = manager.agent_control().get_status(agent_id).await;

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "close_agent",
        function_payload(json!({"target": agent_id.to_string()})),
    );
    let output = CloseAgentHandler
        .handle(invocation)
        .await
        .expect("close_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: close_agent::CloseAgentResult =
        serde_json::from_str(&content).expect("close_agent result should be json");
    assert_eq!(result.previous_status, status_before);
    assert_eq!(success, Some(true));

    let ops = manager.captured_ops();
    let submitted_shutdown = ops
        .iter()
        .any(|(id, op)| *id == agent_id && matches!(op, Op::Shutdown));
    assert_eq!(submitted_shutdown, true);

    let status_after = manager.agent_control().get_status(agent_id).await;
    assert_eq!(status_after, AgentStatus::NotFound);
}

#[tokio::test]
async fn tool_handlers_cascade_close_and_resume_and_keep_explicitly_closed_subtrees_closed() {
    let (_session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    let mut config = turn.config.as_ref().clone();
    config.agent_max_depth = 3;
    config
        .features
        .enable(Feature::Sqlite)
        .expect("test config should allow sqlite");

    let parent = manager
        .start_thread(config.clone())
        .await
        .expect("parent thread should start");
    let parent_thread_id = parent.thread_id;
    let parent_session = parent.thread.codex.session.clone();

    let child_spawn_output = SpawnAgentHandler
        .handle(invocation(
            parent_session.clone(),
            parent_session.new_default_turn().await,
            "spawn_agent",
            function_payload(json!({"message": "hello child"})),
        ))
        .await
        .expect("child spawn should succeed");
    let (child_content, child_success) = expect_text_output(child_spawn_output);
    let child_result: serde_json::Value =
        serde_json::from_str(&child_content).expect("child spawn result should be json");
    let child_thread_id = parse_agent_id(
        child_result
            .get("agent_id")
            .and_then(serde_json::Value::as_str)
            .expect("child spawn result should include agent_id"),
    );
    assert_eq!(child_success, Some(true));

    let child_thread = manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let child_session = child_thread.codex.session.clone();
    let grandchild_spawn_output = SpawnAgentHandler
        .handle(invocation(
            child_session.clone(),
            child_session.new_default_turn().await,
            "spawn_agent",
            function_payload(json!({"message": "hello grandchild"})),
        ))
        .await
        .expect("grandchild spawn should succeed");
    let (grandchild_content, grandchild_success) = expect_text_output(grandchild_spawn_output);
    let grandchild_result: serde_json::Value =
        serde_json::from_str(&grandchild_content).expect("grandchild spawn result should be json");
    let grandchild_thread_id = parse_agent_id(
        grandchild_result
            .get("agent_id")
            .and_then(serde_json::Value::as_str)
            .expect("grandchild spawn result should include agent_id"),
    );
    assert_eq!(grandchild_success, Some(true));

    let close_output = CloseAgentHandler
        .handle(invocation(
            parent_session.clone(),
            parent_session.new_default_turn().await,
            "close_agent",
            function_payload(json!({"target": child_thread_id.to_string()})),
        ))
        .await
        .expect("close_agent should close the child subtree");
    let (close_content, close_success) = expect_text_output(close_output);
    let close_result: close_agent::CloseAgentResult =
        serde_json::from_str(&close_content).expect("close_agent result should be json");
    assert_ne!(close_result.previous_status, AgentStatus::NotFound);
    assert_eq!(close_success, Some(true));
    assert_eq!(
        manager.agent_control().get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        manager
            .agent_control()
            .get_status(grandchild_thread_id)
            .await,
        AgentStatus::NotFound
    );

    let child_resume_output = ResumeAgentHandler
        .handle(invocation(
            parent_session.clone(),
            parent_session.new_default_turn().await,
            "resume_agent",
            function_payload(json!({"id": child_thread_id.to_string()})),
        ))
        .await
        .expect("resume_agent should reopen the child subtree");
    let (child_resume_content, child_resume_success) = expect_text_output(child_resume_output);
    let child_resume_result: resume_agent::ResumeAgentResult =
        serde_json::from_str(&child_resume_content).expect("resume result should be json");
    assert_ne!(child_resume_result.status, AgentStatus::NotFound);
    assert_eq!(child_resume_success, Some(true));
    assert_ne!(
        manager.agent_control().get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        manager
            .agent_control()
            .get_status(grandchild_thread_id)
            .await,
        AgentStatus::NotFound
    );

    let close_again_output = CloseAgentHandler
        .handle(invocation(
            parent_session.clone(),
            parent_session.new_default_turn().await,
            "close_agent",
            function_payload(json!({"target": child_thread_id.to_string()})),
        ))
        .await
        .expect("close_agent should be repeatable for the child subtree");
    let (close_again_content, close_again_success) = expect_text_output(close_again_output);
    let close_again_result: close_agent::CloseAgentResult =
        serde_json::from_str(&close_again_content)
            .expect("second close_agent result should be json");
    assert_ne!(close_again_result.previous_status, AgentStatus::NotFound);
    assert_eq!(close_again_success, Some(true));
    assert_eq!(
        manager.agent_control().get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        manager
            .agent_control()
            .get_status(grandchild_thread_id)
            .await,
        AgentStatus::NotFound
    );

    let operator = manager
        .start_thread(config)
        .await
        .expect("operator thread should start");
    let operator_session = operator.thread.codex.session.clone();
    let _ = manager
        .agent_control()
        .shutdown_live_agent(parent_thread_id)
        .await
        .expect("parent shutdown should succeed");
    assert_eq!(
        manager.agent_control().get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );

    let parent_resume_output = ResumeAgentHandler
        .handle(invocation(
            operator_session,
            operator.thread.codex.session.new_default_turn().await,
            "resume_agent",
            function_payload(json!({"id": parent_thread_id.to_string()})),
        ))
        .await
        .expect("resume_agent should reopen the parent thread");
    let (parent_resume_content, parent_resume_success) = expect_text_output(parent_resume_output);
    let parent_resume_result: resume_agent::ResumeAgentResult =
        serde_json::from_str(&parent_resume_content).expect("parent resume result should be json");
    assert_ne!(parent_resume_result.status, AgentStatus::NotFound);
    assert_eq!(parent_resume_success, Some(true));
    assert_ne!(
        manager.agent_control().get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        manager.agent_control().get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        manager
            .agent_control()
            .get_status(grandchild_thread_id)
            .await,
        AgentStatus::NotFound
    );

    let shutdown_report = manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert_eq!(shutdown_report.submit_failed, Vec::<ThreadId>::new());
    assert_eq!(shutdown_report.timed_out, Vec::<ThreadId>::new());
}

#[tokio::test]
async fn build_agent_spawn_config_uses_turn_context_values() {
    fn pick_allowed_sandbox_policy(
        constraint: &crate::config::Constrained<SandboxPolicy>,
        base: SandboxPolicy,
    ) -> SandboxPolicy {
        let candidates = [
            SandboxPolicy::new_read_only_policy(),
            SandboxPolicy::new_workspace_write_policy(),
            SandboxPolicy::DangerFullAccess,
        ];
        candidates
            .into_iter()
            .find(|candidate| *candidate != base && constraint.can_set(candidate).is_ok())
            .unwrap_or(base)
    }

    let (_session, mut turn) = make_session_and_context().await;
    let base_instructions = BaseInstructions {
        text: "base".to_string(),
    };
    turn.developer_instructions = Some("dev".to_string());
    turn.compact_prompt = Some("compact".to_string());
    turn.shell_environment_policy = ShellEnvironmentPolicy {
        use_profile: true,
        ..ShellEnvironmentPolicy::default()
    };
    let temp_dir = tempfile::tempdir().expect("temp dir");
    turn.cwd = temp_dir.abs();
    turn.codex_linux_sandbox_exe = Some(PathBuf::from("/bin/echo"));
    let sandbox_policy = pick_allowed_sandbox_policy(
        &turn.config.permissions.sandbox_policy,
        turn.config.permissions.sandbox_policy.get().clone(),
    );
    let file_system_sandbox_policy =
        FileSystemSandboxPolicy::from_legacy_sandbox_policy(&sandbox_policy, &turn.cwd);
    let network_sandbox_policy = NetworkSandboxPolicy::from(&sandbox_policy);
    turn.sandbox_policy
        .set(sandbox_policy)
        .expect("sandbox policy set");
    turn.file_system_sandbox_policy = file_system_sandbox_policy.clone();
    turn.network_sandbox_policy = network_sandbox_policy;
    turn.approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy set");

    let config = build_agent_spawn_config(&base_instructions, &turn).expect("spawn config");
    let mut expected = (*turn.config).clone();
    expected.base_instructions = Some(base_instructions.text);
    expected.model = Some(turn.model_info.slug.clone());
    expected.model_provider = turn.provider.clone();
    expected.model_reasoning_effort = turn.reasoning_effort;
    expected.model_reasoning_summary = Some(turn.reasoning_summary);
    expected.developer_instructions = turn.developer_instructions.clone();
    expected.compact_prompt = turn.compact_prompt.clone();
    expected.permissions.shell_environment_policy = turn.shell_environment_policy.clone();
    expected.codex_linux_sandbox_exe = turn.codex_linux_sandbox_exe.clone();
    expected.cwd = turn.cwd.clone();
    expected
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy set");
    expected
        .permissions
        .sandbox_policy
        .set(turn.sandbox_policy.get().clone())
        .expect("sandbox policy set");
    expected.permissions.file_system_sandbox_policy = file_system_sandbox_policy;
    expected.permissions.network_sandbox_policy = network_sandbox_policy;
    assert_eq!(config, expected);
}

#[tokio::test]
async fn build_agent_spawn_config_preserves_base_user_instructions() {
    let (_session, mut turn) = make_session_and_context().await;
    let mut base_config = (*turn.config).clone();
    base_config.user_instructions = Some("base-user".to_string());
    turn.user_instructions = Some("resolved-user".to_string());
    turn.config = Arc::new(base_config.clone());
    let base_instructions = BaseInstructions {
        text: "base".to_string(),
    };

    let config = build_agent_spawn_config(&base_instructions, &turn).expect("spawn config");

    assert_eq!(config.user_instructions, base_config.user_instructions);
}

#[tokio::test]
async fn build_agent_resume_config_clears_base_instructions() {
    let (_session, mut turn) = make_session_and_context().await;
    let mut base_config = (*turn.config).clone();
    base_config.base_instructions = Some("caller-base".to_string());
    turn.config = Arc::new(base_config);
    turn.approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy set");

    let config = build_agent_resume_config(&turn, /*child_depth*/ 0).expect("resume config");

    let mut expected = (*turn.config).clone();
    expected.base_instructions = None;
    expected.model = Some(turn.model_info.slug.clone());
    expected.model_provider = turn.provider.clone();
    expected.model_reasoning_effort = turn.reasoning_effort;
    expected.model_reasoning_summary = Some(turn.reasoning_summary);
    expected.developer_instructions = turn.developer_instructions.clone();
    expected.compact_prompt = turn.compact_prompt.clone();
    expected.permissions.shell_environment_policy = turn.shell_environment_policy.clone();
    expected.codex_linux_sandbox_exe = turn.codex_linux_sandbox_exe.clone();
    expected.cwd = turn.cwd.clone();
    expected
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy set");
    expected
        .permissions
        .sandbox_policy
        .set(turn.sandbox_policy.get().clone())
        .expect("sandbox policy set");
    assert_eq!(config, expected);
}
