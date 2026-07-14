use anyhow::Result;
use codex_protocol::models::ContentItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ModelRerouteReason;
use codex_protocol::protocol::ModelVerification;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_model_verification_metadata;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_response_once;
use core_test_support::responses::mount_response_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_completed;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use wiremock::ResponseTemplate;

const SERVER_MODEL: &str = "gpt-5.2";
const TERMINAL_SERVER_MODEL: &str = "gpt-5.1-codex";
const REQUESTED_MODEL: &str = "gpt-5.3-codex";
const FIRST_MODEL_SNAPSHOT: &str = "gpt-5.2-2026-05-01";
const TERMINAL_MODEL_SNAPSHOT: &str = "gpt-5.1-codex-2026-06-01";
const TRUSTED_ACCESS_FOR_CYBER_VERIFICATION: &str = "trusted_access_for_cyber";

const CYBER_POLICY_MESSAGE: &str =
    "This request has been flagged for potentially high-risk cyber activity.";

fn ev_completed_with_usage(
    id: &str,
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
    reasoning_output_tokens: i64,
) -> serde_json::Value {
    serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": id,
            "usage": {
                "input_tokens": input_tokens,
                "input_tokens_details": {"cached_tokens": cached_input_tokens},
                "output_tokens": output_tokens,
                "output_tokens_details": {"reasoning_tokens": reasoning_output_tokens},
                "total_tokens": input_tokens + output_tokens
            }
        }
    })
}

fn disabled_text_turn(test: &TestCodex, text: &str) -> Op {
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.cwd_path());
    Op::UserInput {
        items: vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }],
        final_output_json_schema: None,
        responsesapi_client_metadata: None,
        additional_context: Default::default(),
        thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
            environments: Some(local_selections(test.config.cwd.clone())),
            approval_policy: Some(AskForApproval::Never),
            sandbox_policy: Some(sandbox_policy),
            permission_profile,
            collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                mode: codex_protocol::config_types::ModeKind::Default,
                settings: codex_protocol::config_types::Settings {
                    model: test.session_configured.model.clone(),
                    reasoning_effort: test.config.model_reasoning_effort.clone(),
                    developer_instructions: None,
                },
            }),
            ..Default::default()
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_model_header_mismatch_emits_warning_event() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response =
        sse_response(sse_completed("resp-1")).insert_header("OpenAI-Model", SERVER_MODEL);
    let _mock = mount_response_once(&server, response).await;

    let mut builder = test_codex().with_model(REQUESTED_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(&test, "trigger safety check"))
        .await?;

    let reroute = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ModelReroute(_))
    })
    .await;
    let EventMsg::ModelReroute(reroute) = reroute else {
        panic!("expected model reroute event");
    };
    assert_eq!(reroute.from_model, REQUESTED_MODEL);
    assert_eq!(reroute.to_model, SERVER_MODEL);
    assert_eq!(reroute.reason, ModelRerouteReason::HighRiskCyberActivity);

    let warning = wait_for_event(&test.codex, |event| matches!(event, EventMsg::Warning(_))).await;
    let EventMsg::Warning(warning) = warning else {
        panic!("expected warning event");
    };
    assert!(warning.message.contains(REQUESTED_MODEL));
    assert!(warning.message.contains(SERVER_MODEL));

    let _ = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cyber_policy_response_emits_typed_error_without_retry() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = ResponseTemplate::new(400).set_body_json(serde_json::json!({
        "error": {
            "message": CYBER_POLICY_MESSAGE,
            "type": "invalid_request",
            "param": null,
            "code": "cyber_policy"
        }
    }));
    let mock = mount_response_once(&server, response).await;

    let mut builder = test_codex().with_model(REQUESTED_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(&test, "trigger cyber policy error"))
        .await?;

    let error = wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
    let EventMsg::Error(error) = error else {
        panic!("expected error event");
    };
    assert_eq!(error.message, CYBER_POLICY_MESSAGE);
    assert_eq!(error.codex_error_info, Some(CodexErrorInfo::CyberPolicy));

    mock.single_request();

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_model_field_mismatch_emits_warning_when_header_matches_requested() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = sse_response(sse(vec![
        serde_json::json!({
            "type": "response.created",
            "response": {
                "id": "resp-1",
                "headers": {
                    "OpenAI-Model": SERVER_MODEL
                }
            }
        }),
        core_test_support::responses::ev_completed("resp-1"),
    ]))
    .insert_header("OpenAI-Model", REQUESTED_MODEL);
    let _mock = mount_response_once(&server, response).await;

    let mut builder = test_codex().with_model(REQUESTED_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(&test, "trigger response model check"))
        .await?;

    let reroute = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ModelReroute(_))
    })
    .await;
    let EventMsg::ModelReroute(reroute) = reroute else {
        panic!("expected model reroute event");
    };
    assert_eq!(reroute.from_model, REQUESTED_MODEL);
    assert_eq!(reroute.to_model, SERVER_MODEL);
    assert_eq!(reroute.reason, ModelRerouteReason::HighRiskCyberActivity);

    let warning = wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::Warning(warning)
                if warning
                    .message
                    .contains("flagged for potentially high-risk cyber activity")
        )
    })
    .await;
    let EventMsg::Warning(warning) = warning else {
        panic!("expected warning event");
    };
    assert!(warning.message.contains(REQUESTED_MODEL));
    assert!(warning.message.contains(SERVER_MODEL));

    let _ = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_model_header_mismatch_only_emits_one_warning_per_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let tool_args = serde_json::json!({
        "command": "echo hello",
        "timeout_ms": 1_000
    });

    let first_response = sse_response(sse(vec![
        ev_response_created("resp-1"),
        ev_function_call(
            "call-1",
            "shell_command",
            &serde_json::to_string(&tool_args)?,
        ),
        ev_completed_with_usage("resp-1", 10, 4, 5, 1),
    ]))
    .insert_header("OpenAI-Model", SERVER_MODEL)
    .insert_header("OpenAI-Model-Snapshot", FIRST_MODEL_SNAPSHOT);
    let second_response = sse_response(sse(vec![
        ev_response_created("resp-2"),
        ev_assistant_message("msg-1", "done"),
        ev_completed_with_usage("resp-2", 20, 7, 8, 2),
    ]))
    .insert_header("OpenAI-Model", TERMINAL_SERVER_MODEL)
    .insert_header("OpenAI-Model-Snapshot", TERMINAL_MODEL_SNAPSHOT);
    let third_response = sse_response(sse(vec![
        ev_response_created("resp-3"),
        ev_assistant_message("msg-2", "done again"),
        core_test_support::responses::ev_completed("resp-3"),
    ]));
    let _mock = mount_response_sequence(
        &server,
        vec![first_response, second_response, third_response],
    )
    .await;

    let mut builder = test_codex().with_model(REQUESTED_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(&test, "trigger follow-up turn"))
        .await?;

    let mut warning_count = 0;
    let turn_complete = loop {
        let event = wait_for_event(&test.codex, |_| true).await;
        match event {
            EventMsg::Warning(warning)
                if warning
                    .message
                    .contains("flagged for potentially high-risk cyber activity") =>
            {
                warning_count += 1;
            }
            EventMsg::TurnComplete(turn_complete) => break turn_complete,
            _ => {}
        }
    };

    assert_eq!(warning_count, 1);
    assert_eq!(
        turn_complete.final_model.as_deref(),
        Some(TERMINAL_SERVER_MODEL)
    );
    assert_eq!(
        turn_complete.model_snapshot.as_deref(),
        Some(TERMINAL_MODEL_SNAPSHOT)
    );
    assert_eq!(
        turn_complete.provider_usage,
        Some(TokenUsage {
            input_tokens: 30,
            cached_input_tokens: 11,
            output_tokens: 13,
            reasoning_output_tokens: 3,
            total_tokens: 43,
        })
    );

    test.codex
        .submit(disabled_text_turn(&test, "second ordinary turn"))
        .await?;
    let second_turn_complete = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(second_turn_complete) = second_turn_complete else {
        panic!("expected second turn complete event");
    };
    assert_eq!(second_turn_complete.provider_usage, None);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nonterminal_response_identity_is_not_reported_when_follow_up_fails() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let tool_args = serde_json::json!({
        "command": "echo hello",
        "timeout_ms": 1_000
    });
    let first_response = sse_response(sse(vec![
        ev_response_created("resp-1"),
        ev_function_call(
            "call-1",
            "shell_command",
            &serde_json::to_string(&tool_args)?,
        ),
        ev_completed_with_usage("resp-1", 12, 3, 4, 1),
    ]))
    .insert_header("OpenAI-Model", SERVER_MODEL)
    .insert_header("OpenAI-Model-Snapshot", FIRST_MODEL_SNAPSHOT);
    let failed_follow_up = ResponseTemplate::new(400).set_body_json(serde_json::json!({
        "error": {
            "message": "synthetic follow-up failure",
            "type": "invalid_request_error",
            "param": null,
            "code": "invalid_prompt"
        }
    }));
    let _mock = mount_response_sequence(&server, vec![first_response, failed_follow_up]).await;

    let mut builder = test_codex().with_model(REQUESTED_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(&test, "trigger failed follow-up"))
        .await?;

    let error = wait_for_event(&test.codex, |event| matches!(event, EventMsg::Error(_))).await;
    let EventMsg::Error(error) = error else {
        panic!("expected error event");
    };
    let turn_complete = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(turn_complete) = turn_complete else {
        panic!("expected turn complete event");
    };

    let started_at = turn_complete.started_at.expect("turn start timestamp");
    let completed_at = turn_complete
        .completed_at
        .expect("turn completion timestamp");
    let duration_ms = turn_complete.duration_ms.expect("turn duration");
    let time_to_first_token_ms = turn_complete
        .time_to_first_token_ms
        .expect("time to first token");
    let provider_usage = TokenUsage {
        input_tokens: 12,
        cached_input_tokens: 3,
        output_tokens: 4,
        reasoning_output_tokens: 1,
        total_tokens: 16,
    };
    let expected = TurnCompleteEvent {
        turn_id: turn_complete.turn_id.clone(),
        last_agent_message: None,
        error: Some(error.clone()),
        started_at: Some(started_at),
        compaction_events_in_turn: 0,
        final_model: None,
        model_snapshot: None,
        provider_usage: Some(provider_usage.clone()),
        completed_at: Some(completed_at),
        duration_ms: Some(duration_ms),
        time_to_first_token_ms: Some(time_to_first_token_ms),
    };

    assert_eq!(turn_complete, expected);
    assert_eq!(
        serde_json::to_value(turn_complete)?,
        serde_json::json!({
            "turn_id": expected.turn_id,
            "last_agent_message": null,
            "error": error,
            "started_at": started_at,
            "compaction_events_in_turn": 0,
            "provider_usage": provider_usage,
            "completed_at": completed_at,
            "duration_ms": duration_ms,
            "time_to_first_token_ms": time_to_first_token_ms,
        })
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_model_header_casing_only_mismatch_does_not_warn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let requested_header = REQUESTED_MODEL.to_ascii_uppercase();
    let response = sse_response(sse_completed("resp-1"))
        .insert_header("OpenAI-Model", requested_header.as_str());
    let _mock = mount_response_once(&server, response).await;

    let mut builder = test_codex().with_model(REQUESTED_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(&test, "trigger casing check"))
        .await?;

    let mut reroute_count = 0;
    let mut warning_count = 0;
    loop {
        let event = wait_for_event(&test.codex, |_| true).await;
        match event {
            EventMsg::ModelReroute(_) => reroute_count += 1,
            EventMsg::Warning(warning)
                if warning
                    .message
                    .contains("flagged for potentially high-risk cyber activity") =>
            {
                warning_count += 1;
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(reroute_count, 0);
    assert_eq!(warning_count, 0);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_verification_emits_structured_event_without_reroute_or_warning() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = sse_response(sse(vec![
        ev_response_created("resp-1"),
        ev_model_verification_metadata("resp-1", vec![TRUSTED_ACCESS_FOR_CYBER_VERIFICATION]),
        core_test_support::responses::ev_completed("resp-1"),
    ]));
    let _mock = mount_response_once(&server, response).await;

    let mut builder = test_codex().with_model(SERVER_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(&test, "trigger model verification"))
        .await?;

    let mut verification_count = 0;
    let mut reroute_count = 0;
    let mut warning_count = 0;
    let mut warning_item_count = 0;
    loop {
        let event = wait_for_event(&test.codex, |_| true).await;
        match event {
            EventMsg::ModelVerification(event) => {
                assert_eq!(
                    event.verifications,
                    vec![ModelVerification::TrustedAccessForCyber]
                );
                verification_count += 1;
            }
            EventMsg::Warning(_) => warning_count += 1,
            EventMsg::ModelReroute(_) => reroute_count += 1,
            EventMsg::RawResponseItem(raw)
                if matches!(
                    &raw.item,
                    ResponseItem::Message { content, .. }
                        if content.iter().any(|item| matches!(
                            item,
                            ContentItem::InputText { text } if text.starts_with("Warning: ")
                        ))
                ) =>
            {
                warning_item_count += 1;
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(verification_count, 1);
    assert_eq!(reroute_count, 0);
    assert_eq!(warning_count, 0);
    assert_eq!(warning_item_count, 0);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_verification_only_emits_once_per_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let tool_args = serde_json::json!({
        "command": "echo hello",
        "timeout_ms": 1_000
    });

    let first_response = sse_response(sse(vec![
        ev_response_created("resp-1"),
        ev_function_call(
            "call-1",
            "shell_command",
            &serde_json::to_string(&tool_args)?,
        ),
        ev_model_verification_metadata("resp-1", vec![TRUSTED_ACCESS_FOR_CYBER_VERIFICATION]),
        core_test_support::responses::ev_completed("resp-1"),
    ]));
    let second_response = sse_response(sse(vec![
        ev_response_created("resp-2"),
        ev_model_verification_metadata("resp-2", vec![TRUSTED_ACCESS_FOR_CYBER_VERIFICATION]),
        ev_assistant_message("msg-1", "done"),
        core_test_support::responses::ev_completed("resp-2"),
    ]));
    let _mock = mount_response_sequence(&server, vec![first_response, second_response]).await;

    let mut builder = test_codex().with_model(SERVER_MODEL);
    let test = builder.build(&server).await?;

    test.codex
        .submit(disabled_text_turn(
            &test,
            "trigger follow-up model verification",
        ))
        .await?;

    let mut verification_count = 0;
    loop {
        let event = wait_for_event(&test.codex, |_| true).await;
        match event {
            EventMsg::ModelVerification(_) => verification_count += 1,
            EventMsg::Warning(warning) if warning.message.contains("high-risk cyber activity") => {
                panic!("model verification should not emit a warning event");
            }
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }

    assert_eq!(verification_count, 1);

    Ok(())
}
