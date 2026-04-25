use anyhow::Context;
use anyhow::Result;
use app_test_support::McpProcess;
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
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::MockServer;

// macOS and Windows Bazel CI can spend tens of seconds starting app-server
// subprocesses or processing test RPCs under load.
#[cfg(any(target_os = "macos", windows))]
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(not(any(target_os = "macos", windows)))]
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn computer_use_call_round_trip_sends_client_response_to_model() -> Result<()> {
    let call_id = "computer-use-call-1";
    let tool_name = "android_observe";
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

    let mut mcp = McpProcess::new(codex_home.path()).await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let android_tool = DynamicToolSpec {
        namespace: None,
        name: tool_name.to_string(),
        description: "Observe Android".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        }),
        defer_loading: false,
        persist_on_resume: true,
        capability: None,
    };

    let thread_req = mcp
        .send_thread_start_request(ThreadStartParams {
            dynamic_tools: Some(vec![android_tool]),
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
    assert_eq!(adapter, "android");
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
        adapter: "android".to_string(),
        tool: tool_name.to_string(),
        arguments: tool_args.clone(),
    };
    assert_eq!(params, expected);

    let response_items = vec![
        ComputerUseCallOutputContentItem::InputText {
            text: "Settings screen visible".to_string(),
        },
        ComputerUseCallOutputContentItem::InputImage {
            image_url: "data:image/png;base64,AAAA".to_string(),
            detail: Some(default_image_detail()),
        },
    ];
    let response = ComputerUseCallResponse {
        content_items: response_items.clone(),
        success: true,
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
    assert_eq!(adapter, "android");
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
            text: "Settings screen visible".to_string(),
        },
        FunctionCallOutputContentItem::InputImage {
            image_url: "data:image/png;base64,AAAA".to_string(),
            detail: Some(default_image_detail()),
        },
    ]);
    assert_eq!(payload, expected_payload);

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
