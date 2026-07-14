use std::num::NonZeroU64;
use std::time::Duration;

use codex_core::CodexThread;
use codex_protocol::config_types::ModelProviderAuthInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InferenceCallEvent;
use codex_protocol::protocol::InferenceCallStatus;
use codex_protocol::protocol::InferenceCallTransport;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::WebSocketConnectionConfig;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::start_websocket_server_with_rejections;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header_regex;
use wiremock::matchers::method;
use wiremock::matchers::path;

struct ProviderAuthCommandFixture {
    tempdir: TempDir,
    command: String,
    args: Vec<String>,
}

impl ProviderAuthCommandFixture {
    fn new(tokens: &[&str]) -> std::io::Result<Self> {
        let tempdir = tempfile::tempdir()?;
        let tokens_file = tempdir.path().join("tokens.txt");
        let token_file_contents = tokens.join("\n") + "\n";
        std::fs::write(&tokens_file, token_file_contents)?;

        #[cfg(unix)]
        let (command, args) = {
            let script_path = tempdir.path().join("print-token.sh");
            std::fs::write(
                &script_path,
                r#"#!/bin/sh
first_line=$(sed -n '1p' tokens.txt)
printf '%s\n' "$first_line"
tail -n +2 tokens.txt > tokens.next
mv tokens.next tokens.txt
"#,
            )?;
            let mut permissions = std::fs::metadata(&script_path)?.permissions();
            {
                use std::os::unix::fs::PermissionsExt;
                permissions.set_mode(0o755);
            }
            std::fs::set_permissions(&script_path, permissions)?;
            ("./print-token.sh".to_string(), Vec::new())
        };

        #[cfg(windows)]
        let (command, args) = {
            let script_path = tempdir.path().join("print-token.cmd");
            std::fs::write(
                &script_path,
                r#"@echo off
setlocal EnableExtensions DisableDelayedExpansion

set "first_line="
<tokens.txt set /p first_line=
if not defined first_line exit /b 1

echo(%first_line%
more +1 tokens.txt > tokens.next
move /y tokens.next tokens.txt >nul
"#,
            )?;
            (
                "cmd.exe".to_string(),
                vec![
                    "/D".to_string(),
                    "/Q".to_string(),
                    "/C".to_string(),
                    ".\\print-token.cmd".to_string(),
                ],
            )
        };

        Ok(Self {
            tempdir,
            command,
            args,
        })
    }

    fn auth(&self) -> ModelProviderAuthInfo {
        ModelProviderAuthInfo {
            command: self.command.clone(),
            args: self.args.clone(),
            timeout_ms: NonZeroU64::new(5_000).expect("non-zero timeout"),
            refresh_interval_ms: 60_000,
            cwd: codex_utils_absolute_path::AbsolutePathBuf::try_from(self.tempdir.path())
                .expect("tempdir should be absolute"),
        }
    }
}

async fn submit_and_collect_auth_recovery_lifecycle(
    codex: &CodexThread,
) -> Vec<InferenceCallEvent> {
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
    while !turn_completed || inference_calls.len() < 4 {
        let event = tokio::time::timeout(Duration::from_secs(10), codex.next_event())
            .await
            .expect("auth-recovery turn timed out")
            .expect("event stream ended")
            .msg;
        match event {
            EventMsg::InferenceCall(event) => inference_calls.push(event),
            EventMsg::TurnComplete(_) => turn_completed = true,
            EventMsg::Error(error) => panic!("auth-recovery turn failed: {}", error.message),
            _ => {}
        }
    }
    inference_calls
}

fn assert_auth_recovery_lifecycle(
    events: &[InferenceCallEvent],
    transport: InferenceCallTransport,
) {
    assert_eq!(events.len(), 4);
    assert_eq!(
        events.iter().map(|event| event.status).collect::<Vec<_>>(),
        vec![
            InferenceCallStatus::Started,
            InferenceCallStatus::Failed,
            InferenceCallStatus::Started,
            InferenceCallStatus::Completed,
        ]
    );
    assert!(events.iter().all(|event| event.transport == transport));
    assert_eq!(events[0].inference_call_id, events[1].inference_call_id);
    assert_eq!(events[2].inference_call_id, events[3].inference_call_id);
    assert_ne!(events[0].inference_call_id, events[2].inference_call_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_401_auth_recovery_records_distinct_attempts() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = MockServer::start().await;
    let auth_fixture = ProviderAuthCommandFixture::new(&["first-token", "second-token"])?;

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header_regex("Authorization", "Bearer first-token"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header_regex("Authorization", "Bearer second-token"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    sse(vec![
                        ev_response_created("response-complete"),
                        ev_completed("response-complete"),
                    ]),
                    "text/event-stream",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider_auth = auth_fixture.auth();
    let test = test_codex()
        .with_config(move |config| {
            config.model_provider_id = "provider-id".to_string();
            config.model_provider.name = "provider-display-name".to_string();
            config.model_provider.auth = Some(provider_auth);
            config.model_provider.supports_websockets = false;
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        })
        .build(&server)
        .await?;

    let events = submit_and_collect_auth_recovery_lifecycle(&test.codex).await;
    assert_auth_recovery_lifecycle(&events, InferenceCallTransport::ResponsesHttp);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_401_auth_recovery_records_distinct_attempts() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_websocket_server_with_rejections(
        vec![
            WebSocketConnectionConfig {
                requests: vec![vec![
                    ev_response_created("response-prewarm"),
                    ev_completed("response-prewarm"),
                ]],
                response_headers: Vec::new(),
                accept_delay: None,
                close_after_requests: true,
            },
            WebSocketConnectionConfig {
                requests: vec![vec![
                    ev_response_created("response-complete"),
                    ev_completed("response-complete"),
                ]],
                response_headers: Vec::new(),
                accept_delay: None,
                close_after_requests: true,
            },
        ],
        vec![None, Some(401)],
    )
    .await;
    let auth_fixture = ProviderAuthCommandFixture::new(&["first-token", "second-token"])?;
    let provider_auth = auth_fixture.auth();
    let mut builder = test_codex().with_config(move |config| {
        config.model_provider_id = "provider-id".to_string();
        config.model_provider.name = "provider-display-name".to_string();
        config.model_provider.auth = Some(provider_auth);
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(0);
    });
    let test = builder.build_with_websocket_server(&server).await?;

    let events = submit_and_collect_auth_recovery_lifecycle(&test.codex).await;
    assert_auth_recovery_lifecycle(&events, InferenceCallTransport::ResponsesWebsocket);
    let handshakes = server.handshakes();
    assert_eq!(handshakes.len(), 3);
    assert_eq!(
        handshakes[0].header("authorization").as_deref(),
        Some("Bearer first-token")
    );
    assert_eq!(
        handshakes[1].header("authorization").as_deref(),
        Some("Bearer first-token")
    );
    assert_eq!(
        handshakes[2].header("authorization").as_deref(),
        Some("Bearer second-token")
    );
    server.shutdown().await;
    Ok(())
}
