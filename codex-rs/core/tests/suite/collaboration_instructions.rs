use anyhow::Result;
use codex_config::InstructionOverlayComponentToml;
use codex_config::InstructionOverlayEdit;
use codex_config::InstructionOverlayOnAbsent;
use codex_config::InstructionOverlaySelection;
use codex_config::loader::project_trust_key;
use codex_core::ForkSnapshot;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_core::config::ConfigBuilder;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_login::CodexAuth;
use codex_models_manager::model_info::model_info_from_slug;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::openai_models::CollaborationModeMessages;
use codex_protocol::openai_models::ModelMessages;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::protocol::COLLABORATION_MODE_CLOSE_TAG;
use codex_protocol::protocol::COLLABORATION_MODE_OPEN_TAG;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::ReviewTarget;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::user_input::UserInput;
use codex_thread_store::LoadThreadHistoryParams;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_models_once_with_etag;
use core_test_support::responses::mount_response_once;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_requests;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;
use wiremock::MockServer;

fn collab_mode_with_mode_and_instructions(
    mode: ModeKind,
    instructions: Option<&str>,
) -> CollaborationMode {
    CollaborationMode {
        mode,
        settings: Settings {
            model: "gpt-5.4".to_string(),
            reasoning_effort: None,
            developer_instructions: instructions.map(str::to_string),
        },
    }
}

fn collab_mode_with_instructions(instructions: Option<&str>) -> CollaborationMode {
    collab_mode_with_mode_and_instructions(ModeKind::Default, instructions)
}

fn collab_mode_for_model(
    mode: ModeKind,
    model: &str,
    instructions: Option<&str>,
) -> CollaborationMode {
    CollaborationMode {
        mode,
        settings: Settings {
            model: model.to_string(),
            reasoning_effort: None,
            developer_instructions: instructions.map(str::to_string),
        },
    }
}

fn model_with_collaboration_messages(
    slug: &str,
    default: Option<&str>,
    plan: Option<&str>,
) -> codex_protocol::openai_models::ModelInfo {
    let mut model = model_info_from_slug(slug);
    let model_messages = model.model_messages.get_or_insert(ModelMessages {
        content_filter_guidance: None,
        persistent_instructions: None,
        tools: None,
        instructions_template: None,
        approvals: None,
        collaboration_modes: None,
        auto_review: None,
        permissions: None,
        multi_agent: None,
        token_budget: None,
        confirmation_policies: None,
        guardian_v2: None,
    });
    model_messages.collaboration_modes = Some(CollaborationModeMessages {
        default: default.map(str::to_string),
        plan: plan.map(str::to_string),
    });
    model
}

fn developer_texts(input: &[Value]) -> Vec<String> {
    input
        .iter()
        .filter(|item| item.get("role").and_then(Value::as_str) == Some("developer"))
        .filter_map(|item| item.get("content")?.as_array().cloned())
        .flatten()
        .filter_map(|content| {
            let text = content.get("text")?.as_str()?;
            Some(text.to_string())
        })
        .collect()
}

fn collab_xml(text: &str) -> String {
    format!("{COLLABORATION_MODE_OPEN_TAG}{text}{COLLABORATION_MODE_CLOSE_TAG}")
}

fn count_messages_containing(texts: &[String], target: &str) -> usize {
    texts.iter().filter(|text| text.contains(target)).count()
}

fn append_overlay(
    text: &str,
    selection: InstructionOverlaySelection,
) -> InstructionOverlayComponentToml {
    InstructionOverlayComponentToml {
        on_absent: InstructionOverlayOnAbsent::Error,
        selection,
        edits: vec![InstructionOverlayEdit::Append {
            text: text.to_string(),
        }],
    }
}

async fn persisted_model_context_json(test: &TestCodex) -> Result<String> {
    test.codex.ensure_rollout_materialized().await;
    test.codex.flush_rollout().await?;
    let history = test
        .thread_store
        .load_latest_model_context(LoadThreadHistoryParams {
            thread_id: test.session_configured.thread_id,
            include_archived: true,
        })
        .await?;
    Ok(serde_json::to_string(&history.items)?)
}

async fn submit_thread_text_turn(thread: &codex_core::CodexThread, prompt: &str) -> Result<()> {
    let submission = thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: prompt.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let turn_id = match submission {
        TurnInputSubmission::Started { turn_id, .. }
        | TurnInputSubmission::Steered { turn_id, .. } => turn_id,
        TurnInputSubmission::NotSubmitted { .. } => {
            anyhow::bail!("child turn input was not submitted")
        }
    };
    wait_for_event(
        thread,
        |ev| matches!(ev, EventMsg::TurnComplete(event) if event.turn_id == turn_id),
    )
    .await;
    Ok(())
}

async fn submit_text_turn_with_model(test: &TestCodex, prompt: &str, model: &str) -> Result<()> {
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                model: Some(model.to_string()),
                ..Default::default()
            }),
        )
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;
    Ok(())
}

fn decoded_request_body(request: &wiremock::Request) -> Option<Vec<u8>> {
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

fn request_body_contains(request: &wiremock::Request, needle: &str) -> bool {
    decoded_request_body(request)
        .and_then(|body| String::from_utf8(body).ok())
        .is_some_and(|body| body.contains(needle))
}

fn request_has_input_type(request: &wiremock::Request, expected_type: &str) -> bool {
    decoded_request_body(request)
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .and_then(|body| body.get("input")?.as_array().cloned())
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some(expected_type))
        })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_collaboration_instructions_by_default() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let req = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let test = test_codex().build(&server).await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let input = req.single_request().input();
    let dev_texts = developer_texts(&input);
    assert!(
        dev_texts
            .iter()
            .any(|text| text.contains("<permissions instructions>")),
        "expected permissions instructions in developer messages, got {dev_texts:?}"
    );
    assert_eq!(
        count_messages_containing(&dev_texts, COLLABORATION_MODE_OPEN_TAG),
        0
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_developer_overlay_changes_only_its_tagged_request_fragment() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let req = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let test = test_codex()
        .with_config(|config| {
            config.developer_instructions = Some("synthetic developer guidance".to_string());
            config.client_owned_developer_instructions = true;
            config.instruction_overlays.developer_guidance = Some(append_overlay(
                "\nsynthetic caller note",
                InstructionOverlaySelection::Unique,
            ));
        })
        .build(&server)
        .await?;
    test.submit_text_turn("hello").await?;

    let texts = developer_texts(&req.single_request().input());
    let permissions = texts
        .iter()
        .find(|text| text.contains("<permissions instructions>"))
        .expect("permission instructions remain in their separate developer fragment");
    assert!(!permissions.contains("synthetic caller note"));
    assert_eq!(
        count_messages_containing(&texts, "synthetic caller note"),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_model_switch_overlay_is_request_only_and_does_not_accumulate() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
            sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
            sse(vec![ev_response_created("resp-3"), ev_completed("resp-3")]),
        ],
    )
    .await;
    let test = test_codex()
        .with_model("gpt-5.4")
        .with_config(|config| {
            config.instruction_overlays.model_switch_guidance =
                Some(InstructionOverlayComponentToml {
                    on_absent: InstructionOverlayOnAbsent::Skip,
                    ..append_overlay(
                        " synthetic transition note",
                        InstructionOverlaySelection::AllClassified,
                    )
                });
        })
        .build(&server)
        .await?;

    test.submit_text_turn("initial model").await?;
    submit_text_turn_with_model(&test, "switch model", "gpt-5.5").await?;
    submit_text_turn_with_model(&test, "same selected model", "gpt-5.5").await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].body_json()["model"], "gpt-5.4");
    assert_eq!(requests[1].body_json()["model"], "gpt-5.5");
    assert_eq!(requests[2].body_json()["model"], "gpt-5.5");
    let initial_texts = developer_texts(&requests[0].input());
    assert_eq!(
        count_messages_containing(&initial_texts, "<model_switch>"),
        0
    );
    assert_eq!(
        count_messages_containing(&initial_texts, "synthetic transition note"),
        0
    );
    let switched = developer_texts(
        requests[1].body_json()["input"]
            .as_array()
            .expect("request input is an array"),
    );
    let transitions: Vec<&String> = switched
        .iter()
        .filter(|text| text.contains("<model_switch>"))
        .collect();
    assert_eq!(transitions.len(), 1);
    assert!(transitions[0].contains("synthetic transition note"));
    assert_eq!(
        transitions[0].matches("synthetic transition note").count(),
        1
    );
    let repeated = developer_texts(
        requests[2].body_json()["input"]
            .as_array()
            .expect("request input is an array"),
    );
    let repeated_transitions: Vec<&String> = repeated
        .iter()
        .filter(|text| text.contains("<model_switch>"))
        .collect();
    assert_eq!(repeated_transitions, transitions);

    let stored = persisted_model_context_json(&test).await?;
    assert!(stored.contains("<model_switch>"));
    assert!(!stored.contains("synthetic transition note"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_uses_review_baseline_and_does_not_apply_normal_baseline() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("resp-2", "No findings."),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.instruction_overlays.model_baseline = Some(append_overlay(
                " NORMAL_BASE_MARKER",
                InstructionOverlaySelection::Unique,
            ));
            config.instruction_overlays.review_baseline = Some(append_overlay(
                " REVIEW_BASE_MARKER",
                InstructionOverlaySelection::Unique,
            ));
        })
        .build(&server)
        .await?;
    test.submit_text_turn("normal request").await?;
    test.codex
        .submit(Op::Review {
            review_request: ReviewRequest {
                target: ReviewTarget::Custom {
                    instructions: "synthetic review request".to_string(),
                },
                user_facing_hint: None,
            },
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let normal_texts = developer_texts(
        requests[0].body_json()["input"]
            .as_array()
            .expect("request input is an array"),
    );
    assert!(
        normal_texts
            .iter()
            .any(|text| text.contains("NORMAL_BASE_MARKER"))
    );
    assert!(
        !normal_texts
            .iter()
            .any(|text| text.contains("REVIEW_BASE_MARKER"))
    );
    let review_texts = developer_texts(
        requests[1].body_json()["input"]
            .as_array()
            .expect("request input is an array"),
    );
    assert!(
        !review_texts
            .iter()
            .any(|text| text.contains("NORMAL_BASE_MARKER"))
    );
    assert!(
        review_texts
            .iter()
            .any(|text| text.contains("REVIEW_BASE_MARKER"))
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_reconstructs_the_configured_overlay_on_its_child_client() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
            sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.developer_instructions = Some("synthetic fork guidance".to_string());
            config.client_owned_developer_instructions = true;
            config.instruction_overlays.developer_guidance = Some(append_overlay(
                " fork child marker",
                InstructionOverlaySelection::Unique,
            ));
        })
        .build(&server)
        .await?;
    test.submit_text_turn("parent turn").await?;
    test.codex.ensure_rollout_materialized().await;
    test.codex.flush_rollout().await?;
    let path = test
        .codex
        .rollout_path()
        .expect("parent rollout has been persisted");
    let fork = test
        .thread_manager
        .fork_legacy_thread(
            ForkSnapshot::Interrupted,
            StartThreadOptions::new(test.config.clone()),
            path,
        )
        .await?;
    submit_thread_text_turn(&fork.thread, "fork child turn").await?;

    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let child_texts = developer_texts(
        requests[1].body_json()["input"]
            .as_array()
            .expect("child request input is an array"),
    );
    assert_eq!(
        count_messages_containing(&child_texts, "fork child marker"),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_and_config_rollback_change_only_new_outgoing_requests() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
            sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
            sse(vec![ev_response_created("resp-3"), ev_completed("resp-3")]),
        ],
    )
    .await;
    let mut configured_builder = test_codex().with_config(|config| {
        config.developer_instructions = Some("synthetic resume guidance".to_string());
        config.client_owned_developer_instructions = true;
        config.instruction_overlays.developer_guidance = Some(append_overlay(
            " reconstructed config marker",
            InstructionOverlaySelection::Unique,
        ));
    });
    let initial = configured_builder.build(&server).await?;
    initial.submit_text_turn("before restart").await?;
    let raw_before_restart = persisted_model_context_json(&initial).await?;
    assert!(!raw_before_restart.contains("reconstructed config marker"));
    let home = initial.home.clone();
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .expect("test session has a rollout path");
    initial.codex.shutdown_and_wait().await?;

    let mut resume_builder = test_codex().with_config(|config| {
        config.developer_instructions = Some("synthetic resume guidance".to_string());
        config.client_owned_developer_instructions = true;
        config.instruction_overlays.developer_guidance = Some(append_overlay(
            " reconstructed config marker",
            InstructionOverlaySelection::Unique,
        ));
    });
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;
    resumed.submit_text_turn("after resume").await?;
    let raw_after_resume = persisted_model_context_json(&resumed).await?;
    assert!(!raw_after_resume.contains("reconstructed config marker"));

    let mut rollback_builder = test_codex().with_config(|config| {
        config.developer_instructions = Some("synthetic resume guidance".to_string());
        config.client_owned_developer_instructions = true;
    });
    let rolled_back = rollback_builder.restart(&server, &resumed).await?;
    rolled_back.submit_text_turn("after rollback").await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let resumed_texts = developer_texts(
        requests[1].body_json()["input"]
            .as_array()
            .expect("request input is an array"),
    );
    assert_eq!(
        count_messages_containing(&resumed_texts, "reconstructed config marker"),
        1
    );
    let rollback_texts = developer_texts(
        requests[2].body_json()["input"]
            .as_array()
            .expect("request input is an array"),
    );
    assert_eq!(
        count_messages_containing(&rollback_texts, "reconstructed config marker"),
        0
    );
    let raw_after_rollback = persisted_model_context_json(&rolled_back).await?;
    assert!(!raw_after_rollback.contains("reconstructed config marker"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn thread_spawn_child_inherits_the_resolved_overlay_plan() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let request = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.developer_instructions = Some("synthetic child guidance".to_string());
            config.client_owned_developer_instructions = true;
            config.instruction_overlays.developer_guidance = Some(append_overlay(
                " inherited child marker",
                InstructionOverlaySelection::Unique,
            ));
        })
        .build(&server)
        .await?;
    let child = test
        .thread_manager
        .start_thread(StartThreadOptions {
            environments: Some(vec![test.executor_environment().request()]),
            session_source: Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: test.session_configured.thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;
    submit_thread_text_turn(&child.thread, "spawned child request").await?;

    let texts = developer_texts(&request.single_request().input());
    assert_eq!(
        count_messages_containing(&texts, "inherited child marker"),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trusted_project_role_overlay_reaches_the_spawned_child_request() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex().build(&server).await?;
    let project = tempfile::TempDir::new()?;
    let home = tempfile::TempDir::new()?;
    std::fs::write(project.path().join(".project-root"), "")?;
    let project_key = toml::Value::String(project_trust_key(project.path())).to_string();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "project_root_markers = [\".project-root\"]\n[projects.{project_key}]\ntrust_level = \"trusted\"\n"
        ),
    )?;
    let agents = project.path().join(".codex/agents");
    std::fs::create_dir_all(&agents)?;
    std::fs::write(project.path().join(".codex/config.toml"), "")?;
    std::fs::write(
        agents.join("project-role.toml"),
        "name = \"project-role\"\ndescription = \"synthetic project role\"\ndeveloper_instructions = \"PROJECT_ROLE_SOURCE\"\n[instruction_overlays.developer_guidance]\nedits = [{ op = \"append\", text = \" PROJECT_ROLE_OVERLAY\" }]\n",
    )?;
    let discovered = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .fallback_cwd(/*fallback_cwd*/ Some(project.path().to_path_buf()))
        .build()
        .await?;
    assert!(discovered.agent_roles.contains_key("project-role"));

    let mut root_config = test.config.clone();
    root_config.agent_roles = discovered.agent_roles;
    root_config.cwd = discovered.cwd.clone();
    root_config.codex_home = discovered.codex_home.clone();
    root_config.features.enable(Feature::Collab)?;
    root_config.features.enable(Feature::MultiAgentV2)?;
    let root = test
        .thread_manager
        .start_thread(StartThreadOptions {
            environments: Some(vec![test.executor_environment().request()]),
            ..StartThreadOptions::new(root_config)
        })
        .await?;
    let spawn_args = serde_json::to_string(&json!({
        "message": "project role child task",
        "task_name": "project_worker",
        "agent_type": "project-role",
        "fork_turns": "none",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| request_body_contains(request, "spawn this synthetic role"),
        sse(vec![
            ev_response_created("resp-root-spawn"),
            ev_function_call_with_namespace(
                "spawn-project-role",
                "collaboration",
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-root-spawn"),
        ]),
    )
    .await;
    let wait_target_path_seen =
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(/*value*/ false));
    struct WaitForSpawnedRoleResponder {
        wait_target_path_seen: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl wiremock::Respond for WaitForSpawnedRoleResponder {
        fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
            let task_path = decoded_request_body(request)
                .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
                .and_then(|body| body.get("input")?.as_array().cloned())
                .and_then(|input| {
                    input.iter().find_map(|item| {
                        (item.get("type")?.as_str()? == "function_call_output"
                            && item.get("call_id")?.as_str()? == "spawn-project-role")
                            .then(|| item.get("output")?.as_str())
                            .flatten()
                            .and_then(|output| serde_json::from_str::<Value>(output).ok())
                            .and_then(|output| output.get("task_name")?.as_str().map(str::to_owned))
                    })
                });
            let targets = match task_path {
                Some(task_path) => {
                    self.wait_target_path_seen
                        .store(/*value*/ true, std::sync::atomic::Ordering::SeqCst);
                    vec![task_path]
                }
                None => vec!["missing_spawn_path".to_string()],
            };
            let wait_args = json!({
                "targets": targets,
                "return_when": "all",
                "timeout_ms": 10_000,
            });
            wiremock::ResponseTemplate::new(/*status_code*/ 200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse(vec![
                    ev_response_created("resp-root-wait"),
                    ev_function_call_with_namespace(
                        "wait-project-worker",
                        "collaboration",
                        "wait_agent",
                        &wait_args.to_string(),
                    ),
                    ev_completed("resp-root-wait"),
                ]))
        }
    }
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path_regex(".*/responses$"))
        .and(|request: &wiremock::Request| request_body_contains(request, "spawn-project-role"))
        .respond_with(WaitForSpawnedRoleResponder {
            wait_target_path_seen: std::sync::Arc::clone(&wait_target_path_seen),
        })
        .up_to_n_times(/*n_times*/ 1)
        .mount(&server)
        .await;
    let child_request = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_input_type(request, "agent_message")
                && request_body_contains(request, "project role child task")
        },
        sse(vec![
            ev_response_created("resp-project-child"),
            ev_assistant_message("msg-project-child", "project role complete"),
            ev_completed("resp-project-child"),
        ]),
    )
    .await;
    let root_completion = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| request_body_contains(request, "wait-project-worker"),
        sse(vec![ev_completed("resp-root-complete")]),
    )
    .await;

    submit_thread_text_turn(&root.thread, "spawn this synthetic role").await?;
    let wait_output = root_completion
        .single_request()
        .function_call_output_text("wait-project-worker")
        .expect("root continuation contains the targeted wait result");
    assert!(
        wait_target_path_seen.load(std::sync::atomic::Ordering::SeqCst),
        "spawn response did not provide a canonical task path for wait_agent"
    );
    let wait_result: Value = serde_json::from_str(&wait_output)?;
    assert_eq!(
        wait_result["message"],
        "Target agent completion is actionable."
    );
    assert!(
        !wait_result["timed_out"]
            .as_bool()
            .expect("wait timeout flag")
    );
    assert_eq!(wait_result["outcome"], "target_terminal_all");
    let child_texts = developer_texts(&child_request.single_request().input());
    assert!(
        child_texts
            .iter()
            .any(|text| text.contains("PROJECT_ROLE_SOURCE"))
    );
    assert_eq!(
        count_messages_containing(&child_texts, "PROJECT_ROLE_OVERLAY"),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_collaboration_messages_track_mode_changes() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let req1 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let req2 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;

    let model_slug = "catalog-collaboration-model";
    let default_text = "## Plan tool\nPreserve the custom default policy.\n";
    let plan_text = "## `update_plan`\nPreserve the custom Plan Mode policy.\n";
    let model = model_with_collaboration_messages(model_slug, Some(default_text), Some(plan_text));
    let mut builder = test_codex()
        .with_model(model_slug)
        .with_config(move |config| {
            config.model_catalog = Some(ModelsResponse {
                models: vec![model],
            });
        });
    let test = builder.build(&server).await?;

    core_test_support::submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            collaboration_mode: Some(collab_mode_for_model(
                ModeKind::Default,
                model_slug,
                Some("legacy default instructions"),
            )),
            ..Default::default()
        },
    )
    .await?;
    test.submit_text_turn("default turn").await?;

    let first_dev_texts = developer_texts(&req1.single_request().input());
    assert_eq!(
        count_messages_containing(&first_dev_texts, &collab_xml(default_text)),
        1
    );
    assert_eq!(
        count_messages_containing(&first_dev_texts, "legacy default instructions"),
        0
    );

    core_test_support::submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            collaboration_mode: Some(collab_mode_for_model(
                ModeKind::Plan,
                model_slug,
                Some("legacy plan instructions"),
            )),
            ..Default::default()
        },
    )
    .await?;
    test.submit_text_turn("plan turn").await?;

    let second_dev_texts = developer_texts(&req2.single_request().input());
    assert_eq!(
        count_messages_containing(&second_dev_texts, &collab_xml(default_text)),
        1
    );
    assert_eq!(
        count_messages_containing(&second_dev_texts, &collab_xml(plan_text)),
        1
    );
    assert_eq!(
        count_messages_containing(&second_dev_texts, "legacy plan instructions"),
        0
    );

    Ok(())
}

#[test_case(ModeKind::Default; "default")]
#[test_case(ModeKind::Plan; "plan")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_collaboration_messages_refresh_without_mode_or_model_change(
    mode: ModeKind,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    const ETAG_1: &str = "\"collaboration-models-1\"";
    const ETAG_2: &str = "\"collaboration-models-2\"";
    const ETAG_3: &str = "\"collaboration-models-3\"";
    const MODEL: &str = "catalog-collaboration-refresh-model";
    const ORIGINAL: &str = "original catalog collaboration instructions";
    const UPDATED: &str = "updated catalog collaboration instructions";
    const INACTIVE: &str = "inactive mode instructions";

    let catalog = |instructions: &str| ModelsResponse {
        models: vec![match mode {
            ModeKind::Default => {
                model_with_collaboration_messages(MODEL, Some(instructions), Some(INACTIVE))
            }
            ModeKind::Plan => {
                model_with_collaboration_messages(MODEL, Some(INACTIVE), Some(instructions))
            }
        }],
    };
    let server = MockServer::start().await;
    let mut models_mocks =
        vec![mount_models_once_with_etag(&server, catalog(ORIGINAL), ETAG_1).await];
    let test = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_model(MODEL)
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
            config
                .features
                .disable(Feature::Apps)
                .expect("test config should allow feature update");
        })
        .build_with_auto_env(&server)
        .await?;

    core_test_support::submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            collaboration_mode: Some(collab_mode_for_model(
                mode,
                MODEL,
                Some("legacy fallback instructions"),
            )),
            ..Default::default()
        },
    )
    .await?;

    let history = [ORIGINAL, UPDATED, ""];
    let mut requests = Vec::new();
    for (turn, etag, refreshed_instructions, expected) in [
        ("original", ETAG_2, Some(UPDATED), &history[..1]),
        ("updated", ETAG_2, None, &history[..2]),
        ("unchanged", ETAG_3, Some(""), &history[..2]),
        ("cleared", ETAG_3, None, &history[..]),
        ("still-cleared", ETAG_3, None, &history[..]),
    ] {
        if let Some(instructions) = refreshed_instructions {
            models_mocks
                .push(mount_models_once_with_etag(&server, catalog(instructions), etag).await);
        }
        let response = mount_response_once(
            &server,
            sse_response(sse(vec![ev_response_created(turn), ev_completed(turn)]))
                .insert_header("X-Models-Etag", etag),
        )
        .await;

        test.submit_text_turn(turn).await?;
        let request = response.single_request();
        let dev_texts = request.message_input_texts("developer");
        let collaboration_instructions = dev_texts
            .iter()
            .flat_map(|text| text.split(COLLABORATION_MODE_OPEN_TAG).skip(1))
            .map(|text| {
                text.split_once(COLLABORATION_MODE_CLOSE_TAG)
                    .expect("collaboration fragment should have a closing tag")
                    .0
            })
            .collect::<Vec<_>>();
        assert_eq!(collaboration_instructions.as_slice(), expected, "{turn}");
        requests.push(request);
    }

    assert_eq!(
        models_mocks
            .iter()
            .map(|mock| mock.requests().len())
            .collect::<Vec<_>>(),
        [1, 1, 1]
    );
    for pair in requests.windows(2) {
        let previous_input = pair[0].input();
        assert_eq!(
            pair[1].input().get(..previous_input.len()),
            Some(previous_input.as_slice())
        );
        assert_eq!(pair[0].instructions_text(), pair[1].instructions_text());
    }

    Ok(())
}

#[test_case(None; "missing instructions")]
#[test_case(Some(""); "explicit empty suppresses fallback")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_collaboration_guidance_clears_prior_instructions_once(
    empty_instructions: Option<&str>,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let model_slug = "catalog-collaboration-clear-model";
    let empty_model_slug = "catalog-collaboration-empty-model";
    let default_text = "catalog default instructions";
    let fallback_text = "legacy fallback instructions";
    let fallback = empty_instructions.map(|_| fallback_text);
    let model =
        model_with_collaboration_messages(model_slug, Some(default_text), empty_instructions);
    let empty_model =
        model_with_collaboration_messages(empty_model_slug, empty_instructions, empty_instructions);
    let mut builder = test_codex()
        .with_model(model_slug)
        .with_config(move |config| {
            config.model_catalog = Some(ModelsResponse {
                models: vec![model, empty_model],
            });
        });
    let test = builder.build_with_auto_env(&server).await?;

    for (turn, mode, model, clear_count) in [
        ("default", ModeKind::Default, model_slug, 0),
        ("clear", ModeKind::Plan, model_slug, 1),
        ("still-clear", ModeKind::Plan, model_slug, 1),
        ("change-model", ModeKind::Plan, empty_model_slug, 1),
        ("change-mode", ModeKind::Default, empty_model_slug, 1),
    ] {
        let response = mount_sse_once(
            &server,
            sse(vec![ev_response_created(turn), ev_completed(turn)]),
        )
        .await;
        core_test_support::submit_thread_settings(
            &test.codex,
            ThreadSettingsOverrides {
                collaboration_mode: Some(collab_mode_for_model(mode, model, fallback)),
                ..Default::default()
            },
        )
        .await?;
        test.submit_text_turn(turn).await?;

        let dev_texts = response.single_request().message_input_texts("developer");
        assert_eq!(
            (
                count_messages_containing(&dev_texts, &collab_xml(default_text)),
                count_messages_containing(&dev_texts, &collab_xml("")),
                count_messages_containing(&dev_texts, fallback_text),
            ),
            (1, clear_count, 0),
            "{turn}"
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_change_appends_new_catalog_collaboration_message() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let _req1 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let req2 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;

    let first_slug = "catalog-collaboration-model-a";
    let second_slug = "catalog-collaboration-model-b";
    let first_text = "model A collaboration instructions";
    let second_text = "model B collaboration instructions";
    let first = model_with_collaboration_messages(first_slug, Some(first_text), /*plan*/ None);
    let second =
        model_with_collaboration_messages(second_slug, Some(second_text), /*plan*/ None);
    let mut builder = test_codex()
        .with_model(first_slug)
        .with_config(move |config| {
            config.model_catalog = Some(ModelsResponse {
                models: vec![first, second],
            });
        });
    let test = builder.build(&server).await?;

    core_test_support::submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            collaboration_mode: Some(collab_mode_for_model(
                ModeKind::Default,
                first_slug,
                Some("legacy instructions"),
            )),
            ..Default::default()
        },
    )
    .await?;
    test.submit_text_turn("first").await?;

    core_test_support::submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            model: Some(second_slug.to_string()),
            ..Default::default()
        },
    )
    .await?;
    test.submit_text_turn("second").await?;

    let dev_texts = developer_texts(&req2.single_request().input());
    assert_eq!(
        count_messages_containing(&dev_texts, &collab_xml(first_text)),
        1
    );
    assert_eq!(
        count_messages_containing(&dev_texts, &collab_xml(second_text)),
        1
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_input_includes_collaboration_instructions_after_override() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let req = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let test = test_codex().build(&server).await?;

    let collab_text = "collab instructions";
    let collaboration_mode = collab_mode_with_instructions(Some(collab_text));
    core_test_support::submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            collaboration_mode: Some(collaboration_mode),
            ..Default::default()
        },
    )
    .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let input = req.single_request().input();
    let dev_texts = developer_texts(&input);
    let collab_text = collab_xml(collab_text);
    assert_eq!(count_messages_containing(&dev_texts, &collab_text), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collaboration_instructions_added_on_user_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let req = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let test = test_codex().build(&server).await?;
    let collab_text = "turn instructions";
    let collaboration_mode = collab_mode_with_instructions(Some(collab_text));

    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(local_requests(test.config.cwd.clone())),
                approval_policy: Some(test.config.permissions.approval_policy.value()),
                sandbox_policy: Some(test.config.legacy_sandbox_policy()),
                summary: Some(
                    test.config
                        .model_reasoning_summary
                        .unwrap_or(codex_protocol::config_types::ReasoningSummary::Auto),
                ),
                collaboration_mode: Some(collaboration_mode),
                ..Default::default()
            }),
        )
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let input = req.single_request().input();
    let dev_texts = developer_texts(&input);
    let collab_text = collab_xml(collab_text);
    assert_eq!(count_messages_containing(&dev_texts, &collab_text), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collaboration_instructions_omitted_when_disabled() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let req = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config.include_collaboration_mode_instructions = false;
    });
    let test = builder.build(&server).await?;
    let collaboration_mode = collab_mode_with_instructions(Some("turn instructions"));

    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(local_requests(test.config.cwd.clone())),
                approval_policy: Some(test.config.permissions.approval_policy.value()),
                sandbox_policy: Some(test.config.legacy_sandbox_policy()),
                summary: Some(
                    test.config
                        .model_reasoning_summary
                        .unwrap_or(codex_protocol::config_types::ReasoningSummary::Auto),
                ),
                collaboration_mode: Some(collaboration_mode),
                ..Default::default()
            }),
        )
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let input = req.single_request().input();
    let dev_texts = developer_texts(&input);
    assert_eq!(
        count_messages_containing(&dev_texts, COLLABORATION_MODE_OPEN_TAG),
        0
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_turn_overrides_collaboration_instructions_after_override() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let req = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let test = test_codex().build(&server).await?;
    let base_text = "base instructions";
    let base_mode = collab_mode_with_instructions(Some(base_text));
    let turn_text = "turn override";
    let turn_mode = collab_mode_with_instructions(Some(turn_text));

    core_test_support::submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            collaboration_mode: Some(base_mode),
            ..Default::default()
        },
    )
    .await?;

    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                environments: Some(local_requests(test.config.cwd.clone())),
                approval_policy: Some(test.config.permissions.approval_policy.value()),
                sandbox_policy: Some(test.config.legacy_sandbox_policy()),
                summary: Some(
                    test.config
                        .model_reasoning_summary
                        .unwrap_or(codex_protocol::config_types::ReasoningSummary::Auto),
                ),
                collaboration_mode: Some(turn_mode),
                ..Default::default()
            }),
        )
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let input = req.single_request().input();
    let dev_texts = developer_texts(&input);
    let base_text = collab_xml(base_text);
    let turn_text = collab_xml(turn_text);
    assert_eq!(count_messages_containing(&dev_texts, &base_text), 0);
    assert_eq!(count_messages_containing(&dev_texts, &turn_text), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collaboration_mode_update_appends_instruction_changes_within_same_mode() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let _req1 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let req2 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;

    let test = test_codex().build(&server).await?;
    let first_text = "first instructions";
    let second_text = "second instructions";

    core_test_support::submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            collaboration_mode: Some(collab_mode_with_instructions(Some(first_text))),
            ..Default::default()
        },
    )
    .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello 1".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    core_test_support::submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            collaboration_mode: Some(collab_mode_with_instructions(Some(second_text))),
            ..Default::default()
        },
    )
    .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello 2".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let input = req2.single_request().input();
    let dev_texts = developer_texts(&input);
    let first_text = collab_xml(first_text);
    let second_text = collab_xml(second_text);
    assert_eq!(count_messages_containing(&dev_texts, &first_text), 1);
    assert_eq!(count_messages_containing(&dev_texts, &second_text), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collaboration_mode_update_noop_does_not_append() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let _req1 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let req2 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;

    let test = test_codex().build(&server).await?;
    let collab_text = "same instructions";

    core_test_support::submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            collaboration_mode: Some(collab_mode_with_instructions(Some(collab_text))),
            ..Default::default()
        },
    )
    .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello 1".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    core_test_support::submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            collaboration_mode: Some(collab_mode_with_instructions(Some(collab_text))),
            ..Default::default()
        },
    )
    .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello 2".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let input = req2.single_request().input();
    let dev_texts = developer_texts(&input);
    let collab_text = collab_xml(collab_text);
    assert_eq!(count_messages_containing(&dev_texts, &collab_text), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collaboration_mode_update_emits_new_instruction_message_when_mode_changes() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let _req1 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let req2 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;

    let test = test_codex().build(&server).await?;
    let default_text = "default mode instructions";
    let plan_text = "plan mode instructions";

    core_test_support::submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            collaboration_mode: Some(collab_mode_with_mode_and_instructions(
                ModeKind::Default,
                Some(default_text),
            )),
            ..Default::default()
        },
    )
    .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello 1".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    core_test_support::submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            collaboration_mode: Some(collab_mode_with_mode_and_instructions(
                ModeKind::Plan,
                Some(plan_text),
            )),
            ..Default::default()
        },
    )
    .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello 2".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let input = req2.single_request().input();
    let dev_texts = developer_texts(&input);
    let default_text = collab_xml(default_text);
    let plan_text = collab_xml(plan_text);
    assert_eq!(count_messages_containing(&dev_texts, &default_text), 1);
    assert_eq!(count_messages_containing(&dev_texts, &plan_text), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collaboration_mode_update_noop_does_not_append_when_mode_is_unchanged() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let _req1 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let req2 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;

    let test = test_codex().build(&server).await?;
    let collab_text = "mode-stable instructions";

    core_test_support::submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            collaboration_mode: Some(collab_mode_with_mode_and_instructions(
                ModeKind::Default,
                Some(collab_text),
            )),
            ..Default::default()
        },
    )
    .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello 1".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    core_test_support::submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            collaboration_mode: Some(collab_mode_with_mode_and_instructions(
                ModeKind::Default,
                Some(collab_text),
            )),
            ..Default::default()
        },
    )
    .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello 2".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let input = req2.single_request().input();
    let dev_texts = developer_texts(&input);
    let collab_text = collab_xml(collab_text);
    assert_eq!(count_messages_containing(&dev_texts, &collab_text), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_replays_collaboration_instructions() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let _req1 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let req2 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;

    let mut builder = test_codex();
    let initial = builder.build_with_auto_env(&server).await?;

    let collab_text = "resume instructions";
    let mode = collab_mode_for_model(ModeKind::Plan, "gpt-5.5", Some(collab_text));
    core_test_support::submit_thread_settings(
        &initial.codex,
        ThreadSettingsOverrides {
            collaboration_mode: Some(mode),
            ..Default::default()
        },
    )
    .await?;

    initial
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&initial.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let resumed = builder.restart(&server, &initial).await?;
    assert_eq!(
        resumed
            .codex
            .config_snapshot()
            .await
            .collaboration_mode
            .mode,
        ModeKind::Plan
    );
    resumed
        .codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "after resume".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&resumed.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let input = req2.single_request().input();
    let dev_texts = developer_texts(&input);
    let collab_text = collab_xml(collab_text);
    assert_eq!(count_messages_containing(&dev_texts, &collab_text), 1);

    Ok(())
}

#[test_case(json!("default"); "mode only")]
#[test_case(json!({"mode": "default", "model": "catalog-collaboration-resume-model"}); "mode and model")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_resume_refreshes_legacy_collaboration_snapshot_once(
    legacy_snapshot: Value,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    const MODEL: &str = "catalog-collaboration-resume-model";
    const ORIGINAL: &str = "original collaboration instructions";
    const CURRENT: &str = "current collaboration instructions";
    let builder_with_instructions = |instructions: &str| {
        let model =
            model_with_collaboration_messages(MODEL, Some(instructions), /*plan*/ None);
        test_codex().with_model(MODEL).with_config(move |config| {
            config.model_catalog = Some(ModelsResponse {
                models: vec![model],
            });
        })
    };
    let server = start_mock_server().await;
    let initial_response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("initial"),
            ev_assistant_message("initial-message", "original response"),
            ev_completed("initial"),
        ]),
    )
    .await;
    let initial = builder_with_instructions(ORIGINAL)
        .build_with_auto_env(&server)
        .await?;
    initial.submit_text_turn("before upgrading").await?;

    let home = initial.home.clone();
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .expect("initial session should have a rollout path");
    initial.codex.shutdown_and_wait().await?;
    let mut rewrote_snapshot = false;
    let legacy_rollout = std::fs::read_to_string(&rollout_path)?
        .lines()
        .map(|original_line| {
            let mut line = codex_rollout::parse_rollout_line(original_line)?;
            if let RolloutItem::WorldState(world_state) = &mut line.item
                && let Some(snapshot) = world_state.state.get_mut("collaboration_mode")
            {
                *snapshot = legacy_snapshot.clone();
                rewrote_snapshot = true;
                serde_json::to_string(&line)
            } else {
                Ok(original_line.to_string())
            }
        })
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join("\n");
    assert!(
        rewrote_snapshot,
        "initial rollout should record collaboration mode"
    );
    std::fs::write(&rollout_path, format!("{legacy_rollout}\n"))?;

    let resumed = builder_with_instructions(CURRENT)
        .resume(&server, home, rollout_path)
        .await?;
    let mut previous_input = initial_response.single_request().input();
    for turn in ["first-resumed", "second-resumed"] {
        let response = mount_sse_once(
            &server,
            sse(vec![ev_response_created(turn), ev_completed(turn)]),
        )
        .await;
        resumed.submit_text_turn(turn).await?;
        let request = response.single_request();
        let input = request.input();
        assert_eq!(
            input.get(..previous_input.len()),
            Some(previous_input.as_slice())
        );
        let dev_texts = request.message_input_texts("developer");
        assert_eq!(
            (
                count_messages_containing(&dev_texts, &collab_xml(ORIGINAL)),
                count_messages_containing(&dev_texts, &collab_xml(CURRENT)),
                count_messages_containing(&dev_texts, COLLABORATION_MODE_OPEN_TAG),
            ),
            (1, 1, 2),
            "{turn}"
        );
        previous_input = input;
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_collaboration_instructions_are_ignored() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let req = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let test = test_codex().build(&server).await?;
    let current_model = test.session_configured.model.clone();

    core_test_support::submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            collaboration_mode: Some(CollaborationMode {
                mode: ModeKind::Default,
                settings: Settings {
                    model: current_model,
                    reasoning_effort: None,
                    developer_instructions: Some("".to_string()),
                },
            }),
            ..Default::default()
        },
    )
    .await?;

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "hello".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let input = req.single_request().input();
    let dev_texts = developer_texts(&input);
    let collab_text = collab_xml("");
    assert_eq!(count_messages_containing(&dev_texts, &collab_text), 0);

    Ok(())
}
