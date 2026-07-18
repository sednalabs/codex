use anyhow::Result;
use codex_core::config::AgentRoleConfig;
use codex_features::Feature;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use tokio::time::Instant;
use tokio::time::sleep;

const MULTI_AGENT_V2_NAMESPACE: &str = "agents";
const SPAWN_CALL_ID: &str = "spawn-worker";
const FOLLOWUP_CALL_ID: &str = "followup-worker";
const INITIAL_PROMPT: &str = "spawn a durable worker";
const INITIAL_TASK: &str = "inspect the repository";
const WORKER_MODEL: &str = "gpt-5.4";
const WORKER_REASONING_EFFORT: &str = "low";
const FOLLOWUP_PROMPT: &str = "continue the durable worker";
const FOLLOWUP_TASK: &str = "inspect the tests too";
const ROLE_NAME: &str = "durable_worker";
const ROLE_MODEL: &str = "gpt-5.4";
const ROLE_MODEL_PROVIDER_ID: &str = "mock";
const ROLE_DEVELOPER_INSTRUCTIONS: &str = "Keep the durable worker role configuration.";

fn decoded_body(request: &wiremock::Request) -> Option<Vec<u8>> {
    let is_zstd = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|entry| entry.trim().eq_ignore_ascii_case("zstd"))
        });
    if is_zstd {
        zstd::stream::decode_all(std::io::Cursor::new(&request.body)).ok()
    } else {
        Some(request.body.clone())
    }
}

fn body_contains(request: &wiremock::Request, text: &str) -> bool {
    decoded_body(request)
        .and_then(|body| String::from_utf8(body).ok())
        .is_some_and(|body| body.contains(text))
}

fn request_has_input_type(request: &wiremock::Request, input_type: &str) -> bool {
    decoded_body(request)
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .and_then(|body| body.get("input").and_then(Value::as_array).cloned())
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some(input_type))
        })
}

#[derive(Clone, Copy)]
enum ResumeScenario {
    ExplicitSelection,
    Role,
}

impl ResumeScenario {
    fn configure(self, config: &mut codex_core::config::Config, model_provider_base_url: &str) {
        match self {
            Self::ExplicitSelection => configure_multi_agent_v2(config),
            Self::Role => configure_multi_agent_v2_with_role(config, model_provider_base_url),
        }
    }

    fn effective_selection(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::ExplicitSelection => (WORKER_MODEL, "openai", WORKER_REASONING_EFFORT),
            Self::Role => (ROLE_MODEL, ROLE_MODEL_PROVIDER_ID, "high"),
        }
    }
}

fn configure_multi_agent_v2(config: &mut codex_core::config::Config) {
    config
        .features
        .enable(Feature::Collab)
        .expect("test config should allow feature update");
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
}

fn configure_multi_agent_v2_with_role(
    config: &mut codex_core::config::Config,
    model_provider_base_url: &str,
) {
    configure_multi_agent_v2(config);
    let role_path = config.codex_home.join("durable-worker-role.toml");
    std::fs::write(
        &role_path,
        format!(
            "model = \"{ROLE_MODEL}\"\nmodel_reasoning_effort = \"high\"\ndeveloper_instructions = \"{ROLE_DEVELOPER_INSTRUCTIONS}\"\nsandbox_mode = \"read-only\"\nmodel_provider = \"mock\"\n\n[model_providers.mock]\nname = \"mock\"\nbase_url = \"{model_provider_base_url}\"\nenv_key = \"PATH\"\nwire_api = \"responses\"\n"
        ),
    )
    .expect("write durable worker role config");
    config.agent_roles.insert(
        ROLE_NAME.to_string(),
        AgentRoleConfig {
            description: Some("Durable worker role".to_string()),
            config_file: Some(role_path.to_path_buf()),
            nickname_candidates: None,
        },
    );
}

async fn assert_cold_root_resume_restores_agent_identity(scenario: ResumeScenario) -> Result<()> {
    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&match scenario {
        ResumeScenario::ExplicitSelection => json!({
            "message": INITIAL_TASK,
            "task_name": "worker",
            "model": WORKER_MODEL,
            "reasoning_effort": WORKER_REASONING_EFFORT,
            "fork_turns": "none",
        }),
        ResumeScenario::Role => json!({
            "message": INITIAL_TASK,
            "task_name": "worker",
            "agent_type": ROLE_NAME,
            "fork_turns": "none",
        }),
    })?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, INITIAL_PROMPT),
        sse(vec![
            ev_response_created("resp-spawn-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-spawn-1"),
        ]),
    )
    .await;
    let initial_child_request = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_input_type(request, "agent_message") && body_contains(request, INITIAL_TASK)
        },
        sse(vec![
            ev_response_created("resp-worker-1"),
            ev_assistant_message("msg-worker-1", "initial task complete"),
            ev_completed("resp-worker-1"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, SPAWN_CALL_ID)
                && !request_has_input_type(request, "agent_message")
        },
        sse(vec![
            ev_response_created("resp-spawn-2"),
            ev_assistant_message("msg-spawn-2", "worker spawned"),
            ev_completed("resp-spawn-2"),
        ]),
    )
    .await;

    let initial_model_provider_base_url = format!("{}/v1", server.uri());
    let mut initial_builder = test_codex().with_config(move |config| {
        scenario.configure(config, &initial_model_provider_base_url);
    });
    let initial = initial_builder.build_with_auto_env(&server).await?;
    let root_thread_id = initial.session_configured.thread_id;
    let home = initial.home.clone();
    let rollout_path = initial
        .codex
        .rollout_path()
        .expect("root rollout path")
        .to_path_buf();
    initial.submit_turn(INITIAL_PROMPT).await?;

    let deadline = Instant::now() + Duration::from_secs(2);
    let worker_thread_id = loop {
        if let Some(thread_id) = initial
            .thread_manager
            .list_thread_ids()
            .await
            .into_iter()
            .find(|thread_id| *thread_id != root_thread_id)
        {
            break thread_id;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for spawned worker");
        }
        sleep(Duration::from_millis(10)).await;
    };
    let worker_thread = initial.thread_manager.get_thread(worker_thread_id).await?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if matches!(
            worker_thread.agent_status().await,
            AgentStatus::Completed(_)
        ) {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for worker completion");
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert!(
        initial_child_request
            .requests()
            .iter()
            .any(|request| request.body_contains_text(INITIAL_TASK))
    );
    if matches!(scenario, ResumeScenario::Role) {
        assert!(
            initial_child_request
                .requests()
                .iter()
                .any(|request| request.body_contains_text(ROLE_DEVELOPER_INSTRUCTIONS))
        );
    }
    let initial_worker_config = worker_thread.config_snapshot().await;
    let initial_worker_role_config = (
        initial_worker_config.model,
        initial_worker_config.model_provider_id,
        initial_worker_config.reasoning_effort,
        initial_worker_config.permission_profile,
    );
    match scenario {
        ResumeScenario::ExplicitSelection => {
            assert_eq!(initial_worker_role_config.0, WORKER_MODEL);
            assert_eq!(initial_worker_role_config.1, "openai");
            assert_eq!(initial_worker_role_config.2, Some(ReasoningEffort::Low));
        }
        ResumeScenario::Role => assert_eq!(
            initial_worker_role_config,
            (
                ROLE_MODEL.to_string(),
                ROLE_MODEL_PROVIDER_ID.to_string(),
                Some(ReasoningEffort::High),
                PermissionProfile::Disabled,
            )
        ),
    }
    worker_thread.flush_rollout().await?;
    initial.codex.flush_rollout().await?;
    drop(worker_thread);
    drop(initial);

    let followup_args = serde_json::to_string(&json!({
        "target": "worker",
        "message": FOLLOWUP_TASK,
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, FOLLOWUP_PROMPT),
        sse(vec![
            ev_response_created("resp-followup-1"),
            ev_function_call_with_namespace(
                FOLLOWUP_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "followup_task",
                &followup_args,
            ),
            ev_completed("resp-followup-1"),
        ]),
    )
    .await;
    let followup_child_request = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_input_type(request, "agent_message")
                && body_contains(request, FOLLOWUP_TASK)
        },
        sse(vec![
            ev_response_created("resp-worker-2"),
            ev_assistant_message("msg-worker-2", "follow-up complete"),
            ev_completed("resp-worker-2"),
        ]),
    )
    .await;
    let followup_root_request = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, FOLLOWUP_CALL_ID)
                && !request_has_input_type(request, "agent_message")
        },
        sse(vec![
            ev_response_created("resp-followup-2"),
            ev_assistant_message("msg-followup-2", "follow-up sent"),
            ev_completed("resp-followup-2"),
        ]),
    )
    .await;

    let resumed_model_provider_base_url = format!("{}/v1", server.uri());
    let mut resume_builder = test_codex().with_config(move |config| {
        scenario.configure(config, &resumed_model_provider_base_url);
    });
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;
    assert_eq!(
        resumed.thread_manager.list_thread_ids().await,
        vec![root_thread_id]
    );
    assert!(
        resumed
            .thread_manager
            .get_thread(worker_thread_id)
            .await
            .is_err()
    );

    resumed.submit_turn(FOLLOWUP_PROMPT).await?;

    let followup_output = followup_root_request
        .requests()
        .iter()
        .find_map(|request| request.function_call_output_text(FOLLOWUP_CALL_ID))
        .expect("follow-up tool should return a model-visible result");
    let worker_thread_id_string = worker_thread_id.to_string();
    let deadline = Instant::now() + Duration::from_secs(10);
    let followup_request = loop {
        if let Some(request) = followup_child_request
            .requests()
            .into_iter()
            .find(|request| {
                !request.inputs_of_type("agent_message").is_empty()
                    && request.body_contains_text(FOLLOWUP_TASK)
            })
        {
            break request;
        }
        if Instant::now() >= deadline {
            let worker_status = resumed
                .thread_manager
                .get_thread(worker_thread_id)
                .await?
                .agent_status()
                .await;
            let response_requests: Vec<Value> = server
                .received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .filter(|request| request.url.path().ends_with("/responses"))
                .map(|request| {
                    let body = decoded_body(request)
                        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
                        .unwrap_or(Value::Null);
                    let input_types: Vec<&str> = body
                        .get("input")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|item| item.get("type").and_then(Value::as_str))
                        .collect();
                    json!({
                        "path": request.url.path(),
                        "model": body.get("model").and_then(Value::as_str),
                        "thread_id": body.pointer("/client_metadata/thread_id").and_then(Value::as_str),
                        "input_types": input_types,
                        "contains_followup_task": body_contains(request, FOLLOWUP_TASK),
                    })
                })
                .collect();
            anyhow::bail!(
                "timed out waiting for child follow-up; tool result: {followup_output}; worker status: {worker_status:?}; response requests: {}",
                serde_json::to_string(&response_requests)?
            );
        }
        sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(
        followup_request
            .body_json()
            .pointer("/client_metadata/thread_id")
            .and_then(Value::as_str),
        Some(worker_thread_id_string.as_str())
    );
    let followup_result: Value = serde_json::from_str(&followup_output)?;
    assert_eq!(followup_result["task_name"], "/root/worker");
    let (expected_model, expected_provider, expected_reasoning_effort) =
        scenario.effective_selection();
    assert_eq!(followup_result["effective_model"], expected_model);
    assert_eq!(
        followup_result["effective_model_provider_id"],
        expected_provider
    );
    assert_eq!(
        followup_result["effective_reasoning_effort"],
        expected_reasoning_effort
    );
    if matches!(scenario, ResumeScenario::Role) {
        assert!(followup_request.body_contains_text(ROLE_DEVELOPER_INSTRUCTIONS));
    }
    let reloaded_worker = resumed
        .thread_manager
        .get_thread(worker_thread_id)
        .await
        .expect("follow-up should lazily reload the original worker");
    let reloaded_worker_config = reloaded_worker.config_snapshot().await;
    let reloaded_worker_role_config = (
        reloaded_worker_config.model,
        reloaded_worker_config.model_provider_id,
        reloaded_worker_config.reasoning_effort,
        reloaded_worker_config.permission_profile,
    );
    assert_eq!(reloaded_worker_role_config, initial_worker_role_config);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_root_resume_restores_agent_identity_and_reloads_target_on_followup() -> Result<()> {
    assert_cold_root_resume_restores_agent_identity(ResumeScenario::ExplicitSelection).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_root_resume_restores_agent_identity_and_role_on_followup() -> Result<()> {
    assert_cold_root_resume_restores_agent_identity(ResumeScenario::Role).await
}
