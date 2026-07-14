use std::fs;
use std::path::Path;
use std::time::Duration;

use codex_core::CodexThread;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InferenceCallEvent;
use codex_protocol::protocol::InferenceCallStatus;
use codex_protocol::protocol::InferenceCallTransport;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

async fn submit_and_collect_completed_attempt(
    codex: &CodexThread,
) -> anyhow::Result<Vec<InferenceCallEvent>> {
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
        .await?;

    let mut inference_calls = Vec::new();
    let mut turn_completed = false;
    while !turn_completed || inference_calls.len() < 2 {
        let event = tokio::time::timeout(Duration::from_secs(10), codex.next_event())
            .await
            .expect("inference turn timed out")?
            .msg;
        match event {
            EventMsg::InferenceCall(event) => inference_calls.push(event),
            EventMsg::TurnComplete(_) => turn_completed = true,
            EventMsg::Error(error) => anyhow::bail!("inference turn failed: {}", error.message),
            _ => {}
        }
    }
    Ok(inference_calls)
}

fn persisted_inference_calls(
    rollout_path: &Path,
) -> anyhow::Result<(ThreadHistoryMode, Vec<InferenceCallEvent>)> {
    let mut history_mode = None;
    let mut inference_calls = Vec::new();
    for line in fs::read_to_string(rollout_path)?.lines() {
        let line: RolloutLine = serde_json::from_str(line)?;
        match line.item {
            RolloutItem::SessionMeta(meta) => history_mode = Some(meta.meta.history_mode),
            RolloutItem::EventMsg(EventMsg::InferenceCall(event)) => {
                inference_calls.push(event);
            }
            _ => {}
        }
    }
    Ok((
        history_mode.expect("rollout session metadata has a history mode"),
        inference_calls,
    ))
}

async fn completed_attempt_harness(
    history_mode: ThreadHistoryMode,
) -> anyhow::Result<(MockServer, TestCodex)> {
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("response-complete"),
            ev_completed("response-complete"),
        ]),
    )
    .await;
    let mut builder = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config.model_provider_id = "provider-id".to_string();
            config.model_provider.name = "provider-display-name".to_string();
            config.model_provider.supports_websockets = false;
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        });
    let test = builder.build(&server).await?;
    Ok((server, test))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_delivery_persists_whole_event_pairs_in_both_history_modes() -> anyhow::Result<()>
{
    skip_if_no_network!(Ok(()));
    for history_mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        let (_server, test) = completed_attempt_harness(history_mode).await?;
        let live = submit_and_collect_completed_attempt(&test.codex).await?;
        assert_eq!(
            live.iter().map(|event| event.status).collect::<Vec<_>>(),
            vec![InferenceCallStatus::Started, InferenceCallStatus::Completed,]
        );
        assert_eq!(live[0].inference_call_id, live[1].inference_call_id);

        let rollout_path = test.codex.rollout_path().expect("rollout path");
        let (persisted_mode, persisted) = persisted_inference_calls(&rollout_path)?;
        assert_eq!(persisted_mode, history_mode);
        assert_eq!(persisted, live);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_real_http_setup_persists_started_then_cancelled() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(30))
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    sse(vec![
                        ev_response_created("response-too-late"),
                        ev_completed("response-too-late"),
                    ]),
                    "text/event-stream",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;
    let mut builder = test_codex().with_config(|config| {
        config.model_provider_id = "provider-id".to_string();
        config.model_provider.name = "provider-display-name".to_string();
        config.model_provider.supports_websockets = false;
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(0);
    });
    let test = builder.build(&server).await?;
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
        let event = test.codex.next_event().await?.msg;
        if let EventMsg::InferenceCall(event) = event
            && event.status == InferenceCallStatus::Started
        {
            break event;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !server
                .received_requests()
                .await
                .unwrap_or_default()
                .is_empty()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("real HTTP request should reach transport before interruption");

    test.codex.submit(Op::Interrupt).await?;
    let mut cancelled = None;
    let mut turn_aborted = false;
    while cancelled.is_none() || !turn_aborted {
        let event = tokio::time::timeout(Duration::from_secs(10), test.codex.next_event())
            .await
            .expect("interrupted setup timed out")?
            .msg;
        match event {
            EventMsg::InferenceCall(event) if event.status == InferenceCallStatus::Cancelled => {
                cancelled = Some(event);
            }
            EventMsg::TurnAborted(_) => turn_aborted = true,
            _ => {}
        }
    }
    let cancelled = cancelled.expect("cancelled setup observation");
    assert_eq!(cancelled.inference_call_id, started.inference_call_id);
    assert_eq!(cancelled.transport, InferenceCallTransport::ResponsesHttp);
    assert!(cancelled.response_id.is_none());
    assert!(cancelled.observed_model.is_none());
    assert!(cancelled.observed_model_snapshot.is_none());
    assert!(cancelled.observed_service_tier.is_none());
    assert!(cancelled.token_usage.is_none());

    let rollout_path = test.codex.rollout_path().expect("rollout path");
    let (_, persisted) = persisted_inference_calls(&rollout_path)?;
    let persisted_attempt = persisted
        .into_iter()
        .filter(|event| event.inference_call_id == started.inference_call_id)
        .collect::<Vec<_>>();
    assert_eq!(persisted_attempt, vec![started, cancelled]);
    Ok(())
}
