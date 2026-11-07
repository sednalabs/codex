use anyhow::Context;
use anyhow::Result;
use app_test_support::McpProcess;
use app_test_support::create_fake_paginated_rollout;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence_unchecked;
use app_test_support::to_response;
use codex_app_server_protocol::ComputerUseCallOutputContentItem;
use codex_app_server_protocol::ComputerUseCallParams;
use codex_app_server_protocol::ComputerUseCallResponse;
use codex_app_server_protocol::ComputerUseCallStatus;
use codex_app_server_protocol::DynamicToolSpec;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_protocol::dynamic_tools::DynamicToolCapability;
use codex_protocol::models::ContentItem;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::MockServer;

const TINY_PNG_DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

// macOS and Windows Bazel CI can spend tens of seconds starting app-server
// subprocesses or processing test RPCs under load.
#[cfg(any(target_os = "macos", windows))]
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(not(any(target_os = "macos", windows)))]
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn desktop_computer_use_call_round_trip_preserves_typed_image_output() -> Result<()> {
    let call_id = "computer-use-call-1";
    let tool_name = "desktop_observe";
    let tool_args = json!({ "scope": "screen_and_ui" });
    let tool_call_arguments = serde_json::to_string(&tool_args)?;

    let responses = vec![
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call(call_id, tool_name, &tool_call_arguments),
            responses::ev_completed("resp-1"),
        ]),
        create_final_assistant_message_sse_response("Done")?,
    ];
    let server = create_mock_responses_server_sequence_unchecked(responses).await;

    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = McpProcess::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let desktop_tool = DynamicToolSpec {
        namespace: None,
        name: tool_name.to_string(),
        description: "Capture the current desktop app state as a model-visible screenshot, optionally with a compact accessibility/UI digest.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        }),
        defer_loading: false,
        persist_on_resume: false,
        capability: Some(DynamicToolCapability {
            family: Some("desktop".to_string()),
            capability_scope: Some("session".to_string()),
            mutation_class: Some("non_mutating".to_string()),
            lease_mode: None,
        }),
    };

    let thread_req = mcp
        .send_thread_start_request(ThreadStartParams {
            dynamic_tools: Some(vec![desktop_tool]),
            ..Default::default()
        })
        .await?;
    let thread_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_req)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_resp)?;
    let thread_id = thread.id.clone();

    let turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread_id.clone(),
            input: vec![V2UserInput::Text {
                text: "Observe desktop".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let turn_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_req)),
    )
    .await??;
    let TurnStartResponse { turn } = to_response::<TurnStartResponse>(turn_resp)?;
    let turn_id = turn.id.clone();

    let started = wait_for_computer_use_started(&mut mcp, call_id).await?;
    assert_eq!(started.thread_id, thread_id);
    assert_eq!(started.turn_id, turn_id.clone());
    let ThreadItem::ComputerUseCall {
        id,
        environment_id,
        adapter,
        tool,
        arguments,
        status,
        content_items,
        success,
        error,
        duration_ms,
    } = started.item
    else {
        panic!("expected computer-use call item");
    };
    assert_eq!(id, call_id);
    assert_eq!(
        environment_id.as_deref(),
        Some(codex_exec_server::LOCAL_ENVIRONMENT_ID)
    );
    assert_eq!(adapter, "desktop");
    assert_eq!(tool, tool_name);
    assert_eq!(arguments, tool_args);
    assert_eq!(status, ComputerUseCallStatus::InProgress);
    assert_eq!(content_items, None);
    assert_eq!(success, None);
    assert_eq!(error, None);
    assert_eq!(duration_ms, None);

    let request = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_request_message(),
    )
    .await??;
    let (request_id, params) = match request {
        ServerRequest::ComputerUseCall { request_id, params } => (request_id, params),
        other => panic!("expected ComputerUseCall request, got {other:?}"),
    };

    let expected = ComputerUseCallParams {
        thread_id: thread_id.clone(),
        turn_id: turn_id.clone(),
        call_id: call_id.to_string(),
        environment_id: Some(codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string()),
        adapter: "desktop".to_string(),
        tool: tool_name.to_string(),
        arguments: tool_args.clone(),
    };
    assert_eq!(params, expected);

    let response_items = vec![
        ComputerUseCallOutputContentItem::InputText {
            text: "Desktop screen visible".to_string(),
        },
        ComputerUseCallOutputContentItem::InputImage {
            image_url: TINY_PNG_DATA_URL.to_string(),
            detail: Some(default_image_detail()),
        },
    ];
    let response = ComputerUseCallResponse {
        content_items: response_items.clone(),
        success: true,
        error: None,
    };
    mcp.send_response(request_id, serde_json::to_value(response)?)
        .await?;

    let completed = wait_for_computer_use_completed(&mut mcp, call_id).await?;
    assert_eq!(completed.thread_id, thread_id);
    assert_eq!(completed.turn_id, turn_id);
    let ThreadItem::ComputerUseCall {
        id,
        environment_id,
        adapter,
        tool,
        arguments,
        status,
        content_items,
        success,
        error,
        duration_ms,
    } = completed.item
    else {
        panic!("expected computer-use call item");
    };
    assert_eq!(id, call_id);
    assert_eq!(
        environment_id.as_deref(),
        Some(codex_exec_server::LOCAL_ENVIRONMENT_ID)
    );
    assert_eq!(adapter, "desktop");
    assert_eq!(tool, tool_name);
    assert_eq!(arguments, tool_args);
    assert_eq!(status, ComputerUseCallStatus::Completed);
    assert_eq!(content_items, Some(response_items));
    assert_eq!(success, Some(true));
    assert_eq!(error, None);
    assert!(duration_ms.is_some());

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let bodies = responses_bodies(&server).await?;
    let payload = bodies
        .iter()
        .find_map(|body| function_call_output_payload(body, call_id))
        .context("expected function_call_output in follow-up request")?;
    let expected_payload = FunctionCallOutputPayload::from_content_items(vec![
        FunctionCallOutputContentItem::InputText {
            text: "Desktop screen visible".to_string(),
        },
        FunctionCallOutputContentItem::InputImage {
            image_url: TINY_PNG_DATA_URL.to_string(),
            detail: Some(DEFAULT_IMAGE_DETAIL),
        },
    ]);
    assert_eq!(payload, expected_payload);

    Ok(())
}

#[tokio::test]
async fn thread_resume_preserves_history_and_client_supplied_android_named_tool() -> Result<()> {
    let responses = vec![create_final_assistant_message_sse_response("Done")?];
    let server = create_mock_responses_server_sequence_unchecked(responses).await;

    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = McpProcess::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let resume_history = vec![ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "Resume this thread".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];

    let thread_req = mcp
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: "thread_resume_seed".to_string(),
            history: Some(resume_history),
            dynamic_tools: Some(vec![android_observe_tool()]),
            ..Default::default()
        })
        .await?;
    let thread_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_req)),
    )
    .await??;
    let ThreadResumeResponse { thread, .. } = to_response::<ThreadResumeResponse>(thread_resp)?;

    let turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id,
            input: vec![V2UserInput::Text {
                text: "Observe Android".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let turn_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_req)),
    )
    .await??;
    let _turn: TurnStartResponse = to_response::<TurnStartResponse>(turn_resp)?;

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let bodies = responses_bodies(&server).await?;
    let body = bodies
        .last()
        .context("expected a resumed turn responses request")?;
    let serialized_input = body
        .get("input")
        .context("expected resumed history in the model request")?
        .to_string();
    assert!(
        serialized_input.contains("Resume this thread"),
        "thread resume should preserve the supplied historical message"
    );
    assert_client_supplied_dynamic_tool(
        body,
        "android_observe",
        "client-supplied observe description",
    )?;

    Ok(())
}

#[tokio::test]
async fn thread_resume_preserves_client_tool_after_replacing_loaded_thread() -> Result<()> {
    let responses = vec![
        create_final_assistant_message_sse_response("Seeded")?,
        create_final_assistant_message_sse_response("Done")?,
    ];
    let server = create_mock_responses_server_sequence_unchecked(responses).await;

    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = McpProcess::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_req = mcp
        .send_thread_start_request(ThreadStartParams::default())
        .await?;
    let thread_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_req)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_resp)?;

    let seed_turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![V2UserInput::Text {
                text: "Seed rollout storage".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let seed_turn_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(seed_turn_req)),
    )
    .await??;
    let _seed_turn: TurnStartResponse = to_response::<TurnStartResponse>(seed_turn_resp)?;

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let resume_req = mcp
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread.id.clone(),
            dynamic_tools: Some(vec![android_observe_tool()]),
            ..Default::default()
        })
        .await?;
    let resume_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(resume_req)),
    )
    .await??;
    let ThreadResumeResponse { thread, .. } = to_response::<ThreadResumeResponse>(resume_resp)?;

    let turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id,
            input: vec![V2UserInput::Text {
                text: "Observe Android".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let turn_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_req)),
    )
    .await??;
    let _turn: TurnStartResponse = to_response::<TurnStartResponse>(turn_resp)?;

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let bodies = responses_bodies(&server).await?;
    let body = bodies
        .last()
        .context("expected a resumed turn responses request after replacing the loaded thread")?;
    let serialized_input = body
        .get("input")
        .context("expected loaded history in the resumed model request")?
        .to_string();
    assert!(
        serialized_input.contains("Seed rollout storage"),
        "resuming the loaded thread should preserve its prior user message"
    );
    assert_client_supplied_dynamic_tool(
        body,
        "android_observe",
        "client-supplied observe description",
    )?;

    Ok(())
}

#[tokio::test]
async fn thread_fork_preserves_client_supplied_android_named_tool() -> Result<()> {
    let responses = vec![
        create_final_assistant_message_sse_response("Seeded")?,
        create_final_assistant_message_sse_response("Done")?,
    ];
    let server = create_mock_responses_server_sequence_unchecked(responses).await;

    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;

    let mut mcp = McpProcess::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_req = mcp
        .send_thread_start_request(ThreadStartParams::default())
        .await?;
    let thread_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_req)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_resp)?;

    let seed_turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![V2UserInput::Text {
                text: "Seed rollout storage".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let seed_turn_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(seed_turn_req)),
    )
    .await??;
    let _seed_turn: TurnStartResponse = to_response::<TurnStartResponse>(seed_turn_resp)?;

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let fork_req = mcp
        .send_thread_fork_request(ThreadForkParams {
            thread_id: thread.id,
            dynamic_tools: Some(vec![android_step_tool()]),
            ..Default::default()
        })
        .await?;
    let fork_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(fork_req)),
    )
    .await??;
    let ThreadForkResponse { thread, .. } = to_response::<ThreadForkResponse>(fork_resp)?;

    let turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id,
            input: vec![V2UserInput::Text {
                text: "Take an Android step".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let turn_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_req)),
    )
    .await??;
    let _turn: TurnStartResponse = to_response::<TurnStartResponse>(turn_resp)?;

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let bodies = responses_bodies(&server).await?;
    let body = bodies
        .last()
        .context("expected a forked turn responses request")?;
    assert_client_supplied_dynamic_tool(body, "android_step", "client-supplied step description")?;

    Ok(())
}

#[tokio::test]
async fn paginated_thread_fork_preserves_client_supplied_android_named_tool() -> Result<()> {
    let server = create_mock_responses_server_sequence_unchecked(vec![
        create_final_assistant_message_sse_response("Done")?,
    ])
    .await;
    let codex_home = TempDir::new()?;
    create_config_toml(codex_home.path(), &server.uri())?;
    let source_thread_id = create_fake_paginated_rollout(
        codex_home.path(),
        "2025-01-05T12-00-00",
        "2025-01-05T12:00:00Z",
        "Saved user message",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;

    let mut mcp = McpProcess::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let fork_req = mcp
        .send_thread_fork_request(ThreadForkParams {
            thread_id: source_thread_id,
            dynamic_tools: Some(vec![android_step_tool()]),
            ..Default::default()
        })
        .await?;
    let fork_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(fork_req)),
    )
    .await??;
    let ThreadForkResponse { thread, .. } = to_response::<ThreadForkResponse>(fork_resp)?;

    let turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id,
            input: vec![V2UserInput::Text {
                text: "Take an Android step".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let turn_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_req)),
    )
    .await??;
    let _turn: TurnStartResponse = to_response::<TurnStartResponse>(turn_resp)?;

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let bodies = responses_bodies(&server).await?;
    let body = bodies
        .last()
        .context("expected a paginated forked turn responses request")?;
    assert_client_supplied_dynamic_tool(body, "android_step", "client-supplied step description")?;

    Ok(())
}

async fn responses_bodies(server: &MockServer) -> Result<Vec<Value>> {
    let requests = server
        .received_requests()
        .await
        .context("failed to fetch received requests")?;

    requests
        .into_iter()
        .filter(|req| req.url.path().ends_with("/responses"))
        .map(|req| {
            req.body_json::<Value>()
                .context("request body should be JSON")
        })
        .collect()
}

fn android_observe_tool() -> DynamicToolSpec {
    DynamicToolSpec {
        namespace: None,
        name: "android_observe".to_string(),
        description: "client-supplied observe description".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "clientOnly": { "type": "string" }
            },
            "additionalProperties": false,
        }),
        defer_loading: false,
        persist_on_resume: true,
        capability: Some(android_capability("read_only", "shared_read")),
    }
}

fn android_step_tool() -> DynamicToolSpec {
    DynamicToolSpec {
        namespace: None,
        name: "android_step".to_string(),
        description: "client-supplied step description".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "clientOnly": { "type": "string" }
            },
            "additionalProperties": false,
        }),
        defer_loading: false,
        persist_on_resume: true,
        capability: Some(android_capability("mutating", "exclusive_write")),
    }
}

fn android_capability(mutation_class: &str, lease_mode: &str) -> DynamicToolCapability {
    DynamicToolCapability {
        family: Some("android".to_string()),
        capability_scope: Some("environment".to_string()),
        mutation_class: Some(mutation_class.to_string()),
        lease_mode: Some(lease_mode.to_string()),
    }
}

fn assert_client_supplied_dynamic_tool(
    body: &Value,
    name: &str,
    expected_description: &str,
) -> Result<()> {
    let tool = find_tool(body, name)
        .with_context(|| format!("expected client-supplied dynamic tool `{name}` in request"))?;
    assert_eq!(tool.get("type").and_then(Value::as_str), Some("function"));
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .context("client-supplied dynamic tool description should be present")?;
    assert_eq!(description, expected_description);
    let input_schema = tool
        .get("parameters")
        .context("client-supplied dynamic tool schema should be present")?;
    assert_eq!(
        input_schema.get("type").and_then(Value::as_str),
        Some("object")
    );
    assert_eq!(
        input_schema.get("additionalProperties"),
        Some(&Value::Bool(false))
    );
    let parameters = input_schema
        .get("properties")
        .and_then(Value::as_object)
        .context("client-supplied dynamic tool should include object properties")?;
    assert!(
        parameters.contains_key("clientOnly"),
        "Android-named caller tools should retain the caller's schema, not claim a native provider"
    );
    assert_eq!(parameters.len(), 1, "caller-supplied tool schema changed");
    assert_eq!(
        parameters
            .get("clientOnly")
            .and_then(|property| property.get("type"))
            .and_then(Value::as_str),
        Some("string")
    );

    Ok(())
}

fn find_tool<'a>(body: &'a Value, name: &str) -> Option<&'a Value> {
    body.get("tools")
        .and_then(Value::as_array)
        .and_then(|tools| {
            tools
                .iter()
                .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
        })
}

fn function_call_output_payload(body: &Value, call_id: &str) -> Option<FunctionCallOutputPayload> {
    body.get("input")
        .and_then(Value::as_array)
        .and_then(|items| {
            items.iter().find(|item| {
                item.get("type").and_then(Value::as_str) == Some("function_call_output")
                    && item.get("call_id").and_then(Value::as_str) == Some(call_id)
            })
        })
        .and_then(|item| item.get("output"))
        .cloned()
        .and_then(|output| serde_json::from_value(output).ok())
}

fn default_image_detail() -> String {
    serde_json::to_value(DEFAULT_IMAGE_DETAIL)
        .expect("default image detail should serialize")
        .as_str()
        .expect("default image detail should serialize as a string")
        .to_string()
}

async fn wait_for_computer_use_started(
    mcp: &mut McpProcess,
    call_id: &str,
) -> Result<ItemStartedNotification> {
    loop {
        let notification: JSONRPCNotification = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_notification_message("item/started"),
        )
        .await??;
        let Some(params) = notification.params else {
            continue;
        };
        let started: ItemStartedNotification = serde_json::from_value(params)?;
        if matches!(&started.item, ThreadItem::ComputerUseCall { id, .. } if id == call_id) {
            return Ok(started);
        }
    }
}

async fn wait_for_computer_use_completed(
    mcp: &mut McpProcess,
    call_id: &str,
) -> Result<ItemCompletedNotification> {
    loop {
        let notification: JSONRPCNotification = timeout(
            DEFAULT_READ_TIMEOUT,
            mcp.read_stream_until_notification_message("item/completed"),
        )
        .await??;
        let Some(params) = notification.params else {
            continue;
        };
        let completed: ItemCompletedNotification = serde_json::from_value(params)?;
        if matches!(&completed.item, ThreadItem::ComputerUseCall { id, .. } if id == call_id) {
            return Ok(completed);
        }
    }
}

fn create_config_toml(codex_home: &Path, server_uri: &str) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        config_toml,
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"

model_provider = "mock_provider"

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{server_uri}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
        ),
    )
}
