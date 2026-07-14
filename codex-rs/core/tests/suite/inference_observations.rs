use std::time::Duration;

use codex_core::CodexThread;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InferenceCallEvent;
use codex_protocol::protocol::InferenceCallStatus;
use codex_protocol::protocol::InferenceCallTransport;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_failed;
use core_test_support::responses::start_websocket_server;
use core_test_support::skip_if_no_network;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::sync::oneshot;
use wiremock::MockServer;

async fn submit_and_collect(codex: &CodexThread) -> Vec<InferenceCallEvent> {
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await
        .expect("submit inference turn");

    let mut inference_calls = Vec::new();
    let mut turn_completed = false;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), codex.next_event())
            .await
            .expect("inference turn timed out")
            .expect("event stream ended")
            .msg;
        match event {
            EventMsg::InferenceCall(event) => inference_calls.push(event),
            EventMsg::TurnComplete(_) => turn_completed = true,
            EventMsg::Error(error) => panic!("inference turn failed: {}", error.message),
            _ => {}
        }
        if turn_completed && inference_calls.len() == 4 {
            return inference_calls;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_retry_emits_distinct_failed_and_completed_attempts() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = MockServer::start().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse_failed("response-failed", "server_error", "retry this request"),
            sse(vec![
                ev_response_created("response-complete"),
                ev_completed("response-complete"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider_id = "provider-id".to_string();
            config.model_provider.name = "provider-display-name".to_string();
            config.model_provider.stream_max_retries = Some(1);
            config.model_provider.supports_websockets = false;
        })
        .build(&server)
        .await?;

    let events = submit_and_collect(&test.codex).await;

    assert_eq!(responses.requests().len(), 2);
    assert_eq!(
        events.iter().map(|event| event.status).collect::<Vec<_>>(),
        vec![
            InferenceCallStatus::Started,
            InferenceCallStatus::Failed,
            InferenceCallStatus::Started,
            InferenceCallStatus::Completed,
        ]
    );
    assert!(
        events
            .iter()
            .all(|event| event.transport == InferenceCallTransport::ResponsesHttp)
    );
    assert_eq!(events[0].inference_call_id, events[1].inference_call_id);
    assert_eq!(events[2].inference_call_id, events[3].inference_call_id);
    assert_ne!(events[0].inference_call_id, events[2].inference_call_id);
    assert!(
        events
            .iter()
            .all(|event| event.configured_provider == "provider-id")
    );
    assert!(events[1].response_id.is_none());
    assert!(events[1].observed_model.is_none());
    assert!(events[1].observed_model_snapshot.is_none());
    assert!(events[1].observed_service_tier.is_none());
    assert!(events[1].token_usage.is_none());
    assert_eq!(events[3].response_id.as_deref(), Some("response-complete"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_retry_emits_distinct_failed_and_completed_attempts() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let connection_limit_error = json!({
        "type": "error",
        "status": 400,
        "error": {
            "type": "invalid_request_error",
            "code": "websocket_connection_limit_reached",
            "message": "Create a new websocket connection to continue."
        }
    });
    let server = start_websocket_server(vec![
        vec![
            vec![
                ev_response_created("response-prewarm"),
                ev_completed("response-prewarm"),
            ],
            vec![connection_limit_error],
        ],
        vec![vec![
            ev_response_created("response-complete"),
            ev_completed("response-complete"),
        ]],
    ])
    .await;
    let mut builder = test_codex().with_config(|config| {
        config.model_provider_id = "provider-id".to_string();
        config.model_provider.name = "provider-display-name".to_string();
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(1);
    });
    let test = builder.build_with_websocket_server(&server).await?;

    let events = submit_and_collect(&test.codex).await;

    assert_eq!(
        events.iter().map(|event| event.status).collect::<Vec<_>>(),
        vec![
            InferenceCallStatus::Started,
            InferenceCallStatus::Failed,
            InferenceCallStatus::Started,
            InferenceCallStatus::Completed,
        ]
    );
    assert!(
        events
            .iter()
            .all(|event| event.transport == InferenceCallTransport::ResponsesWebsocket)
    );
    assert_eq!(events[0].inference_call_id, events[1].inference_call_id);
    assert_eq!(events[2].inference_call_id, events[3].inference_call_id);
    assert_ne!(events[0].inference_call_id, events[2].inference_call_id);
    assert!(events[1].response_id.is_none());
    assert!(events[1].observed_model.is_none());
    assert!(events[1].observed_model_snapshot.is_none());
    assert!(events[1].observed_service_tier.is_none());
    assert!(events[1].token_usage.is_none());
    assert_eq!(events[3].response_id.as_deref(), Some("response-complete"));
    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupting_pending_http_response_emits_cancelled_without_completion_evidence()
-> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let (release_tx, release_rx) = oneshot::channel();
    let completed = format!(
        "data: {}\n\n",
        json!({
            "type": "response.completed",
            "response": {"id": "response-too-late"}
        })
    );
    let (server, _) = start_streaming_sse_server(vec![vec![StreamingSseChunk {
        gate: Some(release_rx),
        body: completed,
    }]])
    .await;
    let mut builder = test_codex().with_config(|config| {
        config.model_provider_id = "provider-id".to_string();
        config.model_provider.name = "provider-display-name".to_string();
        config.model_provider.supports_websockets = false;
        config.model_provider.stream_max_retries = Some(0);
    });
    let test = builder.build_with_streaming_server(&server).await?;
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let started = loop {
        let event = test
            .codex
            .next_event()
            .await
            .expect("event stream ended")
            .msg;
        if let EventMsg::InferenceCall(event) = event
            && event.status == InferenceCallStatus::Started
        {
            break event;
        }
    };
    test.codex.submit(Op::Interrupt).await?;
    let mut cancelled = None;
    let mut turn_aborted = false;
    while cancelled.is_none() || !turn_aborted {
        let event = tokio::time::timeout(Duration::from_secs(10), test.codex.next_event())
            .await
            .expect("interrupted turn timed out")
            .expect("event stream ended")
            .msg;
        match event {
            EventMsg::InferenceCall(event) if event.status == InferenceCallStatus::Cancelled => {
                cancelled = Some(event);
            }
            EventMsg::TurnAborted(_) => turn_aborted = true,
            _ => {}
        }
    }
    let cancelled = cancelled.expect("cancelled inference observation");

    assert_eq!(cancelled.inference_call_id, started.inference_call_id);
    assert_eq!(cancelled.transport, InferenceCallTransport::ResponsesHttp);
    assert!(cancelled.response_id.is_none());
    assert!(cancelled.observed_model.is_none());
    assert!(cancelled.observed_model_snapshot.is_none());
    assert!(cancelled.observed_service_tier.is_none());
    assert!(cancelled.token_usage.is_none());
    drop(release_tx);
    server.shutdown().await;
    Ok(())
}
