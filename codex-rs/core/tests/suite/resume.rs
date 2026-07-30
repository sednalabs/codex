use anyhow::Result;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::user_input::ByteRange;
use codex_protocol::user_input::TextElement;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_completed_without_usage;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use wiremock::MockServer;

async fn resume_until_initial_messages(
    builder: &mut TestCodexBuilder,
    server: &MockServer,
    home: Arc<TempDir>,
    rollout_path: PathBuf,
    predicate: impl Fn(&[EventMsg]) -> bool,
) -> Result<TestCodex> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let poll_interval = Duration::from_millis(10);
    let mut last_initial_messages = "<missing initial messages>".to_string();

    loop {
        let resumed = builder
            .resume(server, Arc::clone(&home), rollout_path.clone())
            .await?;
        if let Some(initial_messages) = resumed.session_configured.initial_messages.as_ref() {
            if predicate(initial_messages) {
                return Ok(resumed);
            }
            last_initial_messages = format!("{initial_messages:#?}");
        }

        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for rollout resume messages to stabilize: {last_initial_messages}"
            );
        }

        drop(resumed);
        tokio::time::sleep(poll_interval).await;
    }
}

fn remove_provider_usage_from_persisted_turn_abort(path: &PathBuf) -> Result<()> {
    let contents = fs::read_to_string(path)?;
    let mut found = false;
    let lines = contents
        .lines()
        .map(|line| -> Result<String> {
            let mut line: RolloutLine = serde_json::from_str(line)?;
            if let RolloutItem::EventMsg(EventMsg::TurnAborted(aborted)) = &mut line.item {
                aborted.provider_usage = None;
                found = true;
            }
            Ok(serde_json::to_string(&line)?)
        })
        .collect::<Result<Vec<_>>>()?;
    assert!(found, "expected a persisted turn abort event");
    fs::write(path, format!("{}\n", lines.join("\n")))?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_includes_initial_messages_from_rollout_events() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex();
    let initial = builder.build(&server).await?;
    let codex = Arc::clone(&initial.codex);
    let home = initial.home.clone();
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");

    let initial_sse = sse(vec![
        ev_response_created("resp-initial"),
        ev_assistant_message("msg-1", "Completed first turn"),
        ev_completed_with_tokens("resp-initial", /*total_tokens*/ 17),
    ]);
    mount_sse_once(&server, initial_sse).await;

    let text_elements = vec![TextElement::new(
        ByteRange { start: 0, end: 6 },
        Some("<note>".into()),
    )];

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Record some messages".into(),
                text_elements: text_elements.clone(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let resumed = resume_until_initial_messages(
        &mut builder,
        &server,
        home,
        rollout_path,
        |initial_messages| {
            matches!(
                initial_messages,
                [
                    EventMsg::TurnStarted(_),
                    EventMsg::UserMessage(_),
                    EventMsg::AgentMessage(_),
                    EventMsg::TokenCount(_),
                    EventMsg::TurnComplete(_),
                ]
            )
        },
    )
    .await?;
    let initial_messages = resumed
        .session_configured
        .initial_messages
        .as_ref()
        .expect("expected initial messages to be present for resumed session");
    match initial_messages.as_slice() {
        [
            EventMsg::TurnStarted(started),
            EventMsg::UserMessage(first_user),
            EventMsg::AgentMessage(assistant_message),
            EventMsg::TokenCount(_),
            EventMsg::TurnComplete(completed),
        ] => {
            assert_eq!(first_user.message, "Record some messages");
            assert_eq!(first_user.text_elements, text_elements);
            assert_eq!(assistant_message.message, "Completed first turn");
            assert_eq!(completed.turn_id, started.turn_id);
            assert_eq!(
                completed.last_agent_message.as_deref(),
                Some("Completed first turn")
            );
            assert_eq!(
                completed.provider_usage,
                Some(TokenUsage {
                    input_tokens: 17,
                    cached_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 0,
                    reasoning_output_tokens: 0,
                    total_tokens: 17,
                })
            );
        }
        other => panic!("unexpected initial messages after resume: {other:#?}"),
    }

    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-resumed"),
            ev_assistant_message("msg-2", "Completed resumed turn"),
            ev_completed_without_usage("resp-resumed"),
        ]),
    )
    .await;
    resumed
        .codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Start a fresh resumed turn".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    let resumed_complete = wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(resumed_complete) = resumed_complete else {
        unreachable!("wait predicate only accepts turn completion events");
    };
    assert_eq!(resumed_complete.provider_usage, None);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborted_provider_usage_is_durable_isolated_and_legacy_compatible() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex();
    let initial = builder.build(&server).await?;
    let home = Arc::clone(&initial.home);
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");
    let args = json!({"command": "sleep 60", "timeout_ms": 60_000}).to_string();
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-aborted"),
                ev_function_call("call-sleep", "shell_command", &args),
                ev_completed_with_tokens("resp-aborted", /*total_tokens*/ 19),
            ]),
            sse(vec![
                ev_response_created("resp-after-resume"),
                ev_assistant_message("msg-after-resume", "Completed after resume"),
                ev_completed_without_usage("resp-after-resume"),
            ]),
        ],
    )
    .await;
    initial
        .codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Start a tool that will be interrupted".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&initial.codex, |event| {
        matches!(event, EventMsg::ExecCommandBegin(_))
    })
    .await;
    initial.codex.submit(Op::Interrupt).await?;
    let aborted = wait_for_event(&initial.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    let EventMsg::TurnAborted(aborted) = aborted else {
        unreachable!("wait predicate only accepts aborted turns");
    };
    assert_eq!(
        aborted.provider_usage,
        Some(TokenUsage {
            input_tokens: 19,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: 19,
        })
    );
    initial.codex.flush_rollout().await?;

    let persisted_aborted = fs::read_to_string(&rollout_path)?
        .lines()
        .map(serde_json::from_str::<RolloutLine>)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .find_map(|line| match line.item {
            RolloutItem::EventMsg(EventMsg::TurnAborted(event)) => Some(event),
            _ => None,
        })
        .expect("persisted TurnAborted receipt");
    assert_eq!(persisted_aborted, aborted);
    drop(initial);

    let resumed = resume_until_initial_messages(
        &mut builder,
        &server,
        Arc::clone(&home),
        rollout_path.clone(),
        |messages| {
            messages
                .iter()
                .any(|event| matches!(event, EventMsg::TurnAborted(_)))
        },
    )
    .await?;
    let resumed_aborted = resumed
        .session_configured
        .initial_messages
        .as_ref()
        .and_then(|messages| {
            messages.iter().find_map(|event| match event {
                EventMsg::TurnAborted(aborted) => Some(aborted),
                _ => None,
            })
        })
        .expect("resumed turn abort");
    assert_eq!(resumed_aborted, &aborted);

    resumed
        .codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Continue after resume".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    let completed = wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(completed) = completed else {
        unreachable!("wait predicate only accepts completed turns");
    };
    assert_eq!(completed.provider_usage, None);

    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    let resumed_request = requests[1].body_json().to_string();
    assert!(resumed_request.contains("<turn_aborted>"));
    assert!(!resumed_request.contains("\"type\":\"turn_aborted\""));
    assert!(!resumed_request.contains("\"provider_usage\""));

    resumed.codex.flush_rollout().await?;
    drop(resumed);
    remove_provider_usage_from_persisted_turn_abort(&rollout_path)?;
    let legacy_rollout = fs::read_to_string(&rollout_path)?;
    assert!(
        legacy_rollout
            .lines()
            .filter(|line| line.contains("\"turn_aborted\""))
            .all(|line| !line.contains("\"provider_usage\""))
    );
    let legacy =
        resume_until_initial_messages(&mut builder, &server, home, rollout_path, |messages| {
            messages.iter().any(|event| {
                matches!(event, EventMsg::TurnAborted(aborted) if aborted.provider_usage.is_none())
            })
        })
        .await?;
    let legacy_aborted = legacy
        .session_configured
        .initial_messages
        .as_ref()
        .and_then(|messages| {
            messages.iter().find_map(|event| match event {
                EventMsg::TurnAborted(aborted) => Some(aborted),
                _ => None,
            })
        })
        .expect("legacy resumed turn abort");
    assert_eq!(legacy_aborted.provider_usage, None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_includes_initial_messages_from_reasoning_events() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config.show_raw_agent_reasoning = true;
    });
    let initial = builder.build(&server).await?;
    let codex = Arc::clone(&initial.codex);
    let home = initial.home.clone();
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");

    let initial_sse = sse(vec![
        ev_response_created("resp-initial"),
        ev_reasoning_item("reason-1", &["Summarized step"], &["raw detail"]),
        ev_assistant_message("msg-1", "Completed reasoning turn"),
        ev_completed("resp-initial"),
    ]);
    mount_sse_once(&server, initial_sse).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Record reasoning messages".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let resumed = resume_until_initial_messages(
        &mut builder,
        &server,
        home,
        rollout_path,
        |initial_messages| {
            matches!(
                initial_messages,
                [
                    EventMsg::TurnStarted(_),
                    EventMsg::UserMessage(_),
                    EventMsg::AgentReasoning(_),
                    EventMsg::AgentReasoningRawContent(_),
                    EventMsg::AgentMessage(_),
                    EventMsg::TokenCount(_),
                    EventMsg::TurnComplete(_),
                ]
            )
        },
    )
    .await?;
    let initial_messages = resumed
        .session_configured
        .initial_messages
        .expect("expected initial messages to be present for resumed session");
    match initial_messages.as_slice() {
        [
            EventMsg::TurnStarted(started),
            EventMsg::UserMessage(first_user),
            EventMsg::AgentReasoning(reasoning),
            EventMsg::AgentReasoningRawContent(raw),
            EventMsg::AgentMessage(assistant_message),
            EventMsg::TokenCount(_),
            EventMsg::TurnComplete(completed),
        ] => {
            assert_eq!(first_user.message, "Record reasoning messages");
            assert_eq!(reasoning.text, "Summarized step");
            assert_eq!(raw.text, "raw detail");
            assert_eq!(assistant_message.message, "Completed reasoning turn");
            assert_eq!(completed.turn_id, started.turn_id);
            assert_eq!(
                completed.last_agent_message.as_deref(),
                Some("Completed reasoning turn")
            );
        }
        other => panic!("unexpected initial messages after resume: {other:#?}"),
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_switches_models_preserves_base_instructions() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config.model = Some("gpt-5.2".to_string());
    });
    let initial = builder.build(&server).await?;
    let codex = Arc::clone(&initial.codex);
    let home = initial.home.clone();
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");

    let initial_sse = sse(vec![
        ev_response_created("resp-initial"),
        ev_assistant_message("msg-1", "Completed first turn"),
        ev_completed("resp-initial"),
    ]);
    let initial_mock = mount_sse_once(&server, initial_sse).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Record initial instructions".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let initial_body = initial_mock.single_request().body_json();
    let initial_instructions = initial_body
        .get("instructions")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let resumed_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-resume-1"),
                ev_assistant_message("msg-2", "Resumed turn"),
                ev_completed("resp-resume-1"),
            ]),
            sse(vec![
                ev_response_created("resp-resume-2"),
                ev_assistant_message("msg-3", "Second resumed turn"),
                ev_completed("resp-resume-2"),
            ]),
        ],
    )
    .await;

    let mut resume_builder = test_codex().with_config(|config| {
        config.model = Some("gpt-5.4".to_string());
    });
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;
    resumed
        .codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Resume with different model".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    resumed
        .codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Second turn after resume".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = resumed_mock.requests();
    assert_eq!(requests.len(), 2, "expected two resumed requests");

    let first_resumed = &requests[0];
    assert_eq!(first_resumed.instructions_text(), initial_instructions);
    let first_developer_texts = first_resumed.message_input_texts("developer");
    let first_model_switch_count = first_developer_texts
        .iter()
        .filter(|text| text.contains("<model_switch>"))
        .count();
    assert!(
        first_model_switch_count >= 1,
        "expected model switch message on first post-resume turn"
    );

    let second_resumed = &requests[1];
    assert_eq!(second_resumed.instructions_text(), initial_instructions);
    let second_developer_texts = second_resumed.message_input_texts("developer");
    let second_model_switch_count = second_developer_texts
        .iter()
        .filter(|text| text.contains("<model_switch>"))
        .count();
    assert_eq!(
        second_model_switch_count, 1,
        "did not expect duplicate model switch message after first post-resume turn"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_model_switch_is_not_duplicated_after_pre_turn_override() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut builder = test_codex().with_config(|config| {
        config.model = Some("gpt-5.2".to_string());
    });
    let initial = builder.build(&server).await?;
    let codex = Arc::clone(&initial.codex);
    let home = initial.home.clone();
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");

    let initial_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-initial"),
            ev_assistant_message("msg-1", "Completed first turn"),
            ev_completed("resp-initial"),
        ]),
    )
    .await;
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Record initial instructions".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let _ = initial_mock.single_request();

    let resumed_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-resume"),
            ev_assistant_message("msg-2", "Resumed turn"),
            ev_completed("resp-resume"),
        ]),
    )
    .await;

    let mut resume_builder = test_codex().with_config(|config| {
        config.model = Some("gpt-5.5".to_string());
    });
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;
    core_test_support::submit_thread_settings(
        &resumed.codex,
        codex_protocol::protocol::ThreadSettingsOverrides {
            model: Some("gpt-5.4".to_string()),
            ..Default::default()
        },
    )
    .await?;
    resumed
        .codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "first turn after override".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let request = resumed_mock.single_request();
    let developer_texts = request.message_input_texts("developer");
    let model_switch_count = developer_texts
        .iter()
        .filter(|text| text.contains("<model_switch>"))
        .count();
    assert_eq!(model_switch_count, 1);

    Ok(())
}
