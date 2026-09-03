use super::AuthRequestTelemetryContext;
use super::CompactConversationRequestSettings;
use super::ModelClient;
use super::PendingUnauthorizedRetry;
use super::Prompt;
use super::UnauthorizedRecoveryExecution;
use super::X_CODEX_INSTALLATION_ID_HEADER;
use super::X_CODEX_PARENT_THREAD_ID_HEADER;
use super::X_CODEX_TURN_METADATA_HEADER;
use super::X_CODEX_WINDOW_ID_HEADER;
use super::X_OPENAI_SUBAGENT_HEADER;
use crate::AttestationContext;
use crate::AttestationProvider;
use crate::GenerateAttestationFuture;
use crate::responses_metadata::CodexResponsesMetadata;
use crate::test_support::TestCodexResponsesRequestKind;
use crate::test_support::responses_metadata as test_responses_metadata;
use codex_api::AgentIdentityTelemetry;
use codex_api::ApiError;
use codex_api::RealtimeEventParser;
use codex_api::RealtimeOutputModality;
use codex_api::RealtimeSessionConfig;
use codex_api::RealtimeSessionMode;
use codex_api::ResponseEvent;
use codex_api::TransportError;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::ExternalAuth;
use codex_login::ExternalAuthFuture;
use codex_login::ExternalAuthRefreshContext;
use codex_login::REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR;
use codex_login::auth::AgentIdentityAuthPolicy;
use codex_model_provider::BearerAuthProvider;
use codex_model_provider::SharedModelProvider;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::CHATGPT_CODEX_BASE_URL;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_model_provider_info::create_oss_provider_with_base_url;
use codex_otel::SessionTelemetry;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::auth::AuthMode;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::RealtimeVoice;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_rollout_trace::CompactionTraceContext;
use codex_rollout_trace::ExecutionStatus;
use codex_rollout_trace::InferenceTraceAttempt;
use codex_rollout_trace::InferenceTraceContext;
use codex_rollout_trace::RawTraceEventPayload;
use codex_rollout_trace::RolloutTrace;
use codex_rollout_trace::TraceWriter;
use codex_rollout_trace::replay_bundle;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::start_websocket_server;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::json;
use serial_test::serial;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Notify;
use tracing::Event;
use tracing::Subscriber;
use tracing::field::Visit;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context as LayerContext;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const TEST_CHATGPT_ID_TOKEN: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJlbWFpbCI6InVzZXJAZXhhbXBsZS5jb20iLCJlbWFpbF92ZXJpZmllZCI6dHJ1ZSwiaHR0cHM6Ly9hcGkub3BlbmFpLmNvbS9hdXRoIjp7ImNoYXRncHRfdXNlcl9pZCI6InVzZXItMTIzNDUiLCJ1c2VyX2lkIjoidXNlci0xMjM0NSIsImNoYXRncHRfcGxhbl90eXBlIjoicHJvIiwiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjb3VudC0xMjMifX0.c2ln";
const TEST_INSTALLATION_ID: &str = "11111111-1111-4111-8111-111111111111";

fn test_model_client(session_source: SessionSource) -> ModelClient {
    test_model_client_with_thread_id(ThreadId::new(), session_source)
}

fn test_model_client_with_thread_id(
    thread_id: ThreadId,
    session_source: SessionSource,
) -> ModelClient {
    let provider = create_oss_provider_with_base_url("https://example.com/v1", WireApi::Responses);
    ModelClient::new(
        /*auth_manager*/ None,
        AgentIdentityAuthPolicy::JwtOnly,
        thread_id.into(),
        thread_id,
        TEST_INSTALLATION_ID.to_string(),
        provider,
        session_source,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    )
}

#[tokio::test]
async fn automatic_turn_setup_rejects_stale_auth_revision_before_provider_capture() {
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("test-key"));
    let client = ModelClient::new(
        Some(Arc::clone(&auth_manager)),
        AgentIdentityAuthPolicy::JwtOnly,
        SessionId::new(),
        ThreadId::new(),
        TEST_INSTALLATION_ID.to_string(),
        ModelProviderInfo::create_openai_provider(/*base_url*/ None),
        SessionSource::Exec,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );

    let admitted_authority = client
        .current_provider_authority()
        .await
        .expect("initial provider authority");
    auth_manager
        .set_external_auth(Arc::new(StaticExternalAuth(CodexAuth::from_api_key(
            "replacement-key",
        ))))
        .await
        .expect("credential transition");
    let error = match client.current_client_setup(Some(admitted_authority)).await {
        Ok(_) => panic!("stale automatic-turn auth must fail before provider setup"),
        Err(error) => error,
    };
    assert!(matches!(
        error.details(),
        codex_protocol::error::CodexErrorDetails::AutomaticTurnContextChanged
    ));
    assert_eq!(
        error.to_string(),
        "automatic turn context changed before provider request"
    );
}

#[derive(Clone)]
struct StaticExternalAuth(CodexAuth);

impl ExternalAuth for StaticExternalAuth {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Ok(self.0.clone()) })
    }

    fn refresh(&self, _context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async { Ok(self.0.clone()) })
    }
}

struct RetryOnceResponder {
    calls: AtomicUsize,
    first_attempt: Arc<Notify>,
}

struct RealtimeRetryOnceResponder {
    calls: AtomicUsize,
    first_attempt: Arc<Notify>,
}

impl Respond for RealtimeRetryOnceResponder {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first_attempt.notify_one();
            ResponseTemplate::new(/*status*/ 500)
        } else {
            ResponseTemplate::new(/*status*/ 200)
                .insert_header("location", "/v1/realtime/calls/rtc_auth_b")
                .set_body_string("v=answer-b\r\n")
        }
    }
}

impl Respond for RetryOnceResponder {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first_attempt.notify_one();
            ResponseTemplate::new(/*status*/ 500)
        } else {
            ResponseTemplate::new(/*status*/ 200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(
                    sse(vec![
                        ev_response_created("retry-success"),
                        ev_completed("retry-success"),
                    ]),
                    "text/event-stream",
                )
        }
    }
}

fn automatic_retry_prompt() -> Prompt {
    Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "continue".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        base_instructions: BaseInstructions {
            text: "base instructions".to_string(),
        },
        ..Default::default()
    }
}

fn automatic_retry_client(server: &MockServer, auth_manager: Arc<AuthManager>) -> ModelClient {
    let mut provider = ModelProviderInfo::create_openai_provider(/*base_url*/ None);
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    provider.request_max_retries = Some(1);
    ModelClient::new(
        Some(auth_manager),
        AgentIdentityAuthPolicy::JwtOnly,
        SessionId::new(),
        ThreadId::new(),
        TEST_INSTALLATION_ID.to_string(),
        provider,
        SessionSource::Exec,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    )
}

#[tokio::test]
async fn automatic_http_retry_revalidates_same_authority_and_succeeds() {
    let server = MockServer::start().await;
    let first_attempt = Arc::new(Notify::new());
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(RetryOnceResponder {
            calls: AtomicUsize::new(0),
            first_attempt,
        })
        .expect(/*requests*/ 2)
        .mount(&server)
        .await;
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("key-a"));
    let client = automatic_retry_client(&server, auth_manager);
    let admitted_authority = client
        .current_provider_authority()
        .await
        .expect("initial provider authority");
    let mut session = client.new_session_with_authority(Some(admitted_authority));
    let responses_metadata = test_responses_metadata_for_client(
        &client,
        Some("automatic-retry"),
        format!("{}:0", client.state.thread_id),
        /*parent_thread_id*/ None,
        TestCodexResponsesRequestKind::Turn,
    );

    let mut stream = session
        .stream(
            &automatic_retry_prompt(),
            &test_model_info(),
            &test_session_telemetry(),
            /*effort*/ None,
            codex_protocol::config_types::ReasoningSummary::None,
            /*service_tier*/ None,
            &responses_metadata,
            &InferenceTraceContext::disabled(),
        )
        .await
        .expect("same-authority retry should start a response stream");
    while let Some(event) = stream.next().await {
        if matches!(
            event.expect("retry response event"),
            ResponseEvent::Completed { .. }
        ) {
            break;
        }
    }
    assert_eq!(
        server.received_requests().await.expect("request log").len(),
        2
    );
}

#[tokio::test]
async fn realtime_override_retry_keeps_target_and_joins_with_successful_auth() {
    let override_server = MockServer::start().await;
    let ordinary_provider_server = MockServer::start().await;
    let first_attempt = Arc::new(Notify::new());
    Mock::given(method("POST"))
        .and(path("/v1/realtime/calls"))
        .respond_with(RealtimeRetryOnceResponder {
            calls: AtomicUsize::new(0),
            first_attempt: Arc::clone(&first_attempt),
        })
        .expect(/*requests*/ 2)
        .mount(&override_server)
        .await;
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("key-a"));
    let mut provider = ModelProviderInfo::create_openai_provider(Some(format!(
        "{}/v1",
        ordinary_provider_server.uri()
    )));
    provider.request_max_retries = Some(1);
    let client = ModelClient::new(
        Some(Arc::clone(&auth_manager)),
        AgentIdentityAuthPolicy::JwtOnly,
        SessionId::new(),
        ThreadId::new(),
        TEST_INSTALLATION_ID.to_string(),
        provider,
        SessionSource::Exec,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let override_provider =
        ModelProviderInfo::create_openai_provider(Some(format!("{}/v1", override_server.uri())))
            .to_api_provider(Some(AuthMode::ApiKey))
            .expect("override provider should resolve");
    let session_config = RealtimeSessionConfig {
        instructions: "test realtime retry".to_string(),
        initial_items: Vec::new(),
        model: Some("gpt-realtime".to_string()),
        session_id: Some("sess-retry".to_string()),
        event_parser: RealtimeEventParser::V1,
        session_mode: RealtimeSessionMode::Conversational,
        output_modality: RealtimeOutputModality::Audio,
        voice: RealtimeVoice::Cove,
    };
    let mut extra_headers = http::HeaderMap::new();
    extra_headers.insert(
        "x-sideband-shared",
        http::HeaderValue::from_static("preserved"),
    );
    let call = client.create_realtime_call_with_headers(
        "v=offer\r\n".to_string(),
        session_config,
        extra_headers,
        Some(override_provider),
    );
    let transition = async {
        first_attempt.notified().await;
        auth_manager
            .set_external_auth(Arc::new(StaticExternalAuth(CodexAuth::from_api_key(
                "key-b",
            ))))
            .await
    };
    let (call_result, transition_result) = tokio::join!(call, transition);
    transition_result.expect("credential transition should complete during retry backoff");
    let call = call_result.expect("the override retry should succeed");

    assert_eq!(call.call_id, "rtc_auth_b");
    assert_eq!(call.sdp, "v=answer-b\r\n");
    assert_eq!(
        call.sideband_headers
            .get_all(http::header::AUTHORIZATION)
            .iter()
            .collect::<Vec<_>>(),
        vec![&http::HeaderValue::from_static("Bearer key-b")]
    );
    assert_eq!(
        call.sideband_headers.get("x-sideband-shared"),
        Some(&http::HeaderValue::from_static("preserved"))
    );
    let override_requests = override_server
        .received_requests()
        .await
        .expect("override request log");
    assert_eq!(override_requests.len(), 2);
    assert_eq!(override_requests[0].url, override_requests[1].url);
    assert_eq!(override_requests[0].body, override_requests[1].body);
    assert_eq!(
        override_requests
            .iter()
            .map(|request| request.headers.get(http::header::AUTHORIZATION))
            .collect::<Vec<_>>(),
        vec![
            Some(&http::HeaderValue::from_static("Bearer key-a")),
            Some(&http::HeaderValue::from_static("Bearer key-b")),
        ]
    );
    assert!(
        ordinary_provider_server
            .received_requests()
            .await
            .expect("ordinary provider request log")
            .is_empty()
    );
}

#[tokio::test]
async fn automatic_http_retry_rejects_changed_authority_before_second_wire_attempt() {
    let server = MockServer::start().await;
    let first_attempt = Arc::new(Notify::new());
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(RetryOnceResponder {
            calls: AtomicUsize::new(0),
            first_attempt: Arc::clone(&first_attempt),
        })
        .mount(&server)
        .await;
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("key-a"));
    let client = automatic_retry_client(&server, Arc::clone(&auth_manager));
    let admitted_authority = client
        .current_provider_authority()
        .await
        .expect("initial provider authority");
    let mut session = client.new_session_with_authority(Some(admitted_authority));
    let responses_metadata = test_responses_metadata_for_client(
        &client,
        Some("automatic-retry"),
        format!("{}:0", client.state.thread_id),
        /*parent_thread_id*/ None,
        TestCodexResponsesRequestKind::Turn,
    );
    let prompt = automatic_retry_prompt();
    let model_info = test_model_info();
    let session_telemetry = test_session_telemetry();
    let inference_trace = InferenceTraceContext::disabled();
    let retry = session.stream(
        &prompt,
        &model_info,
        &session_telemetry,
        /*effort*/ None,
        codex_protocol::config_types::ReasoningSummary::None,
        /*service_tier*/ None,
        &responses_metadata,
        &inference_trace,
    );
    let transition = async {
        first_attempt.notified().await;
        auth_manager
            .set_external_auth(Arc::new(StaticExternalAuth(CodexAuth::from_api_key(
                "key-b",
            ))))
            .await
    };
    let (retry_result, transition_result) = tokio::join!(retry, transition);
    transition_result.expect("credential transition should complete during retry backoff");

    let error = match retry_result {
        Ok(_) => panic!("changed authority must stop the retry"),
        Err(error) => error,
    };
    assert!(matches!(
        error.details(),
        codex_protocol::error::CodexErrorDetails::AutomaticTurnContextChanged
    ));
    assert_eq!(
        server.received_requests().await.expect("request log").len(),
        1
    );
}

#[tokio::test]
async fn automatic_turn_transition_at_provider_gate_sends_no_request() {
    let server = MockServer::start().await;
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("key-a"));
    let client = ModelClient::new(
        Some(Arc::clone(&auth_manager)),
        AgentIdentityAuthPolicy::JwtOnly,
        SessionId::new(),
        ThreadId::new(),
        TEST_INSTALLATION_ID.to_string(),
        ModelProviderInfo::create_openai_provider(Some(server.uri())),
        SessionSource::Exec,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let admitted_authority = client
        .current_provider_authority()
        .await
        .expect("initial provider authority");
    let admitted_revision = admitted_authority.revision.expect("managed auth revision");
    let boundary_barrier = auth_manager
        .provider_request_guard(admitted_revision)
        .await
        .expect("initial credential revision should be current");

    let transition = auth_manager.set_external_auth(Arc::new(StaticExternalAuth(
        CodexAuth::from_api_key("key-b"),
    )));
    tokio::pin!(transition);
    assert!(matches!(futures::poll!(transition.as_mut()), Poll::Pending));

    let mut session = client.new_session_with_authority(Some(admitted_authority));
    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "continue".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        base_instructions: BaseInstructions {
            text: "base instructions".to_string(),
        },
        ..Default::default()
    };
    let responses_metadata = test_responses_metadata_for_client(
        &client,
        Some("automatic-turn"),
        format!("{}:0", client.state.thread_id),
        /*parent_thread_id*/ None,
        TestCodexResponsesRequestKind::Turn,
    );
    let model_info = test_model_info();
    let session_telemetry = test_session_telemetry();
    let inference_trace = InferenceTraceContext::disabled();
    let stream = session.stream(
        &prompt,
        &model_info,
        &session_telemetry,
        /*effort*/ None,
        codex_protocol::config_types::ReasoningSummary::None,
        /*service_tier*/ None,
        &responses_metadata,
        &inference_trace,
    );
    tokio::pin!(stream);
    assert!(matches!(futures::poll!(stream.as_mut()), Poll::Pending));

    drop(boundary_barrier);
    let (transition_result, stream_result) = tokio::join!(&mut transition, &mut stream);
    transition_result.expect("credential transition should complete");
    let error = match stream_result {
        Ok(_) => panic!("automatic request must be denied after transition"),
        Err(error) => error,
    };
    assert!(matches!(
        error.details(),
        codex_protocol::error::CodexErrorDetails::AutomaticTurnContextChanged
    ));
    assert!(
        server
            .received_requests()
            .await
            .expect("request log")
            .is_empty()
    );
}

#[tokio::test]
async fn external_auth_resolution_succeeds_without_provider_gate_deadlock() {
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("initial-key"));
    auth_manager
        .set_external_auth(Arc::new(StaticExternalAuth(CodexAuth::from_api_key(
            "external-key",
        ))))
        .await
        .expect("install external auth");
    let client = ModelClient::new(
        Some(auth_manager),
        AgentIdentityAuthPolicy::JwtOnly,
        SessionId::new(),
        ThreadId::new(),
        TEST_INSTALLATION_ID.to_string(),
        ModelProviderInfo::create_openai_provider(/*base_url*/ None),
        SessionSource::Exec,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );

    tokio::time::timeout(Duration::from_secs(1), client.current_provider_authority())
        .await
        .expect("external auth resolution must not deadlock")
        .expect("external auth authority should resolve");
}

#[tokio::test]
#[serial(provider_authority_refresh_env)]
async fn proactive_refresh_succeeds_without_provider_gate_deadlock() {
    let refresh_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "refreshed-access-token",
            "refresh_token": "refreshed-refresh-token"
        })))
        .mount(&refresh_server)
        .await;
    let old_refresh_endpoint = std::env::var_os(REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR);
    // SAFETY: this uniquely serialized test restores the process environment before returning.
    unsafe { std::env::set_var(REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR, refresh_server.uri()) };

    let codex_home = TempDir::new().expect("temporary codex home");
    let auth_json = json!({
        "tokens": {
            "id_token": TEST_CHATGPT_ID_TOKEN,
            "access_token": "stale-access-token",
            "refresh_token": "stale-refresh-token",
            "account_id": "account-123"
        },
        "last_refresh": "2000-01-01T00:00:00Z"
    });
    std::fs::write(
        codex_home.path().join("auth.json"),
        serde_json::to_vec(&auth_json).expect("serialize auth"),
    )
    .expect("write auth");
    let auth_manager = AuthManager::shared(
        codex_home.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*forced_chatgpt_workspace_id*/ None,
        /*chatgpt_base_url*/ None,
        AuthKeyringBackendKind::default(),
        codex_login::test_support::transport_default_auth_route_config(),
    )
    .await;
    let client = ModelClient::new(
        Some(Arc::clone(&auth_manager)),
        AgentIdentityAuthPolicy::JwtOnly,
        SessionId::new(),
        ThreadId::new(),
        TEST_INSTALLATION_ID.to_string(),
        ModelProviderInfo::create_openai_provider(/*base_url*/ None),
        SessionSource::Exec,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );

    tokio::time::timeout(Duration::from_secs(1), client.current_provider_authority())
        .await
        .expect("proactive refresh must not deadlock")
        .expect("refreshed provider authority");
    assert_eq!(auth_manager.auth_revision(), 1);

    match old_refresh_endpoint {
        Some(value) => {
            // SAFETY: restoration is covered by the same serialized test boundary.
            unsafe { std::env::set_var(REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR, value) };
        }
        None => {
            // SAFETY: restoration is covered by the same serialized test boundary.
            unsafe { std::env::remove_var(REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR) };
        }
    }
}

#[tokio::test]
async fn legacy_remote_compaction_uses_automatic_turn_authority() {
    let server = MockServer::start().await;
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("key-a"));
    let client = ModelClient::new(
        Some(Arc::clone(&auth_manager)),
        AgentIdentityAuthPolicy::JwtOnly,
        SessionId::new(),
        ThreadId::new(),
        TEST_INSTALLATION_ID.to_string(),
        ModelProviderInfo::create_openai_provider(Some(server.uri())),
        SessionSource::Exec,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let admitted_authority = client
        .current_provider_authority()
        .await
        .expect("initial provider authority");
    auth_manager
        .set_external_auth(Arc::new(StaticExternalAuth(CodexAuth::from_api_key(
            "key-b",
        ))))
        .await
        .expect("credential transition should complete");
    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "compact".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        base_instructions: BaseInstructions {
            text: "base instructions".to_string(),
        },
        ..Default::default()
    };
    let responses_metadata = test_responses_metadata_for_client(
        &client,
        Some("automatic-turn"),
        format!("{}:0", client.state.thread_id),
        /*parent_thread_id*/ None,
        TestCodexResponsesRequestKind::Turn,
    );
    let result = client
        .compact_conversation_history(
            &prompt,
            &test_model_info(),
            /*turn_state*/ None,
            Some(admitted_authority),
            CompactConversationRequestSettings {
                effort: None,
                summary: codex_protocol::config_types::ReasoningSummary::None,
                service_tier: None,
            },
            &test_session_telemetry(),
            &CompactionTraceContext::disabled(),
            &responses_metadata,
        )
        .await;
    let error = match result {
        Ok(_) => panic!("stale automatic compaction must be denied"),
        Err(error) => error,
    };
    assert!(matches!(
        error.details(),
        codex_protocol::error::CodexErrorDetails::AutomaticTurnContextChanged
    ));
    assert!(
        server
            .received_requests()
            .await
            .expect("request log")
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_websocket_reuse_is_bound_to_credential_revision() {
    let server = start_websocket_server(vec![
        vec![
            vec![ev_response_created("resp-a1"), ev_completed("resp-a1")],
            vec![ev_response_created("resp-a2"), ev_completed("resp-a2")],
        ],
        vec![vec![ev_response_created("resp-b"), ev_completed("resp-b")]],
    ])
    .await;
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("key-a"));
    let client = ModelClient::new(
        Some(Arc::clone(&auth_manager)),
        AgentIdentityAuthPolicy::JwtOnly,
        SessionId::new(),
        ThreadId::new(),
        TEST_INSTALLATION_ID.to_string(),
        ModelProviderInfo::create_openai_provider(Some(server.uri().to_string())),
        SessionSource::Exec,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "continue".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        base_instructions: BaseInstructions {
            text: "base instructions".to_string(),
        },
        ..Default::default()
    };
    let responses_metadata = test_responses_metadata_for_client(
        &client,
        Some("automatic-turn"),
        format!("{}:0", client.state.thread_id),
        /*parent_thread_id*/ None,
        TestCodexResponsesRequestKind::Turn,
    );
    let authority_a = client
        .current_provider_authority()
        .await
        .expect("provider authority A");

    for _ in 0..2 {
        let mut session = client.new_session_with_authority(Some(authority_a));
        let mut stream = session
            .stream(
                &prompt,
                &test_model_info(),
                &test_session_telemetry(),
                /*effort*/ None,
                codex_protocol::config_types::ReasoningSummary::None,
                /*service_tier*/ None,
                &responses_metadata,
                &InferenceTraceContext::disabled(),
            )
            .await
            .expect("same-revision websocket request should start");
        while let Some(event) = stream.next().await {
            if matches!(
                event.expect("websocket event"),
                ResponseEvent::Completed { .. }
            ) {
                break;
            }
        }
        drop(stream);
        drop(session);
    }
    assert_eq!(
        server.connections().len(),
        1,
        "revision A should reuse its socket"
    );

    auth_manager
        .set_external_auth(Arc::new(StaticExternalAuth(CodexAuth::from_api_key(
            "key-b",
        ))))
        .await
        .expect("credential transition should complete");
    let authority_b = client
        .current_provider_authority()
        .await
        .expect("provider authority B");
    let mut session = client.new_session_with_authority(Some(authority_b));
    let mut stream = session
        .stream(
            &prompt,
            &test_model_info(),
            &test_session_telemetry(),
            /*effort*/ None,
            codex_protocol::config_types::ReasoningSummary::None,
            /*service_tier*/ None,
            &responses_metadata,
            &InferenceTraceContext::disabled(),
        )
        .await
        .expect("revision B websocket request should start");
    while let Some(event) = stream.next().await {
        if matches!(
            event.expect("websocket event"),
            ResponseEvent::Completed { .. }
        ) {
            break;
        }
    }
    drop(stream);
    assert_eq!(
        server.connections().len(),
        2,
        "revision B must open a new socket"
    );
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn env_authority_change_rejects_automatic_and_never_reuses_stale_socket() {
    const TOKEN_ENV: &str = "CODEX_TEST_PROVIDER_AUTHORITY_TOKEN_691";
    const HEADER_ENV: &str = "CODEX_TEST_PROVIDER_AUTHORITY_HEADER_691";
    // SAFETY: these test-only names are unique to this test and are removed before return.
    unsafe {
        std::env::set_var(TOKEN_ENV, "token-a-secret");
        std::env::set_var(HEADER_ENV, "header-a-secret");
    }
    let server = start_websocket_server(vec![
        vec![
            vec![ev_response_created("resp-a1"), ev_completed("resp-a1")],
            vec![ev_response_created("resp-a2"), ev_completed("resp-a2")],
        ],
        vec![vec![ev_response_created("resp-b"), ev_completed("resp-b")]],
    ])
    .await;
    let mut provider = ModelProviderInfo::create_openai_provider(Some(server.uri().to_string()));
    provider.requires_openai_auth = false;
    provider.env_key = Some(TOKEN_ENV.to_string());
    provider.env_http_headers = Some(HashMap::from([(
        "x-provider-authority".to_string(),
        HEADER_ENV.to_string(),
    )]));
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("unused"));
    let client = ModelClient::new(
        Some(auth_manager),
        AgentIdentityAuthPolicy::JwtOnly,
        SessionId::new(),
        ThreadId::new(),
        TEST_INSTALLATION_ID.to_string(),
        provider,
        SessionSource::Exec,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "continue".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        base_instructions: BaseInstructions {
            text: "base instructions".to_string(),
        },
        ..Default::default()
    };
    let responses_metadata = test_responses_metadata_for_client(
        &client,
        Some("automatic-turn"),
        format!("{}:0", client.state.thread_id),
        /*parent_thread_id*/ None,
        TestCodexResponsesRequestKind::Turn,
    );
    let authority_a = client
        .current_provider_authority()
        .await
        .expect("authority A");
    assert!(!format!("{authority_a:?}").contains("token-a-secret"));
    assert!(!format!("{authority_a:?}").contains("header-a-secret"));

    for _ in 0..2 {
        let mut session = client.new_session_with_authority(Some(authority_a));
        let mut stream = session
            .stream(
                &prompt,
                &test_model_info(),
                &test_session_telemetry(),
                /*effort*/ None,
                codex_protocol::config_types::ReasoningSummary::None,
                /*service_tier*/ None,
                &responses_metadata,
                &InferenceTraceContext::disabled(),
            )
            .await
            .expect("authority A request");
        while let Some(event) = stream.next().await {
            if matches!(
                event.expect("websocket event"),
                ResponseEvent::Completed { .. }
            ) {
                break;
            }
        }
        drop(stream);
        drop(session);
    }
    assert_eq!(
        server.connections().len(),
        1,
        "same complete identity reuses"
    );

    // SAFETY: these test-only names are unique to this test.
    unsafe {
        std::env::set_var(TOKEN_ENV, "token-b-secret");
        std::env::set_var(HEADER_ENV, "header-b-secret");
    }
    let stale_error = match client.current_client_setup(Some(authority_a)).await {
        Ok(_) => panic!("changed env authority must reject stale automatic work"),
        Err(error) => error,
    };
    assert!(matches!(
        stale_error.details(),
        codex_protocol::error::CodexErrorDetails::AutomaticTurnContextChanged
    ));
    assert!(!stale_error.to_string().contains("token-b-secret"));
    assert!(!stale_error.to_string().contains("header-b-secret"));

    let authority_b = client
        .current_provider_authority()
        .await
        .expect("authority B");
    assert_ne!(authority_a, authority_b);
    let mut session = client.new_session_with_authority(Some(authority_b));
    let mut stream = session
        .stream(
            &prompt,
            &test_model_info(),
            &test_session_telemetry(),
            /*effort*/ None,
            codex_protocol::config_types::ReasoningSummary::None,
            /*service_tier*/ None,
            &responses_metadata,
            &InferenceTraceContext::disabled(),
        )
        .await
        .expect("authority B request");
    while let Some(event) = stream.next().await {
        if matches!(
            event.expect("websocket event"),
            ResponseEvent::Completed { .. }
        ) {
            break;
        }
    }
    assert_eq!(server.connections().len(), 2, "socket A must not be reused");
    // SAFETY: restore the unique test-only process environment.
    unsafe {
        std::env::remove_var(TOKEN_ENV);
        std::env::remove_var(HEADER_ENV);
    }
}

#[tokio::test]
async fn compact_uses_bearer_after_agent_identity_session_fallback() -> anyhow::Result<()> {
    let server = MockServer::start().await;
    let registration_count = Arc::new(AtomicUsize::new(0));
    let response_count = Arc::clone(&registration_count);
    Mock::given(method("POST"))
        .and(path("/v1/agent/register"))
        .respond_with(move |_request: &wiremock::Request| {
            response_count.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(/*status*/ 503)
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses/compact"))
        .respond_with(ResponseTemplate::new(/*status*/ 200).set_body_json(json!({
            "output": []
        })))
        .expect(/*requests*/ 1)
        .mount(&server)
        .await;

    let codex_home = TempDir::new()?;
    let auth_manager = chatgpt_auth_manager(&codex_home, server.uri()).await;
    let mut provider = ModelProviderInfo::create_openai_provider(/*base_url*/ None);
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    let thread_id = ThreadId::new();
    let client = ModelClient::new(
        Some(auth_manager),
        AgentIdentityAuthPolicy::ChatGptAuth,
        SessionId::from(thread_id),
        thread_id,
        "test_installation_id".to_string(),
        provider,
        SessionSource::Cli,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        /*attestation_provider*/ None,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "please compact".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }],
        base_instructions: BaseInstructions {
            text: "base instructions".to_string(),
        },
        ..Default::default()
    };
    let responses_metadata = test_responses_metadata_for_client(
        &client,
        /*turn_id*/ None,
        format!("{}:0", client.state.thread_id),
        /*parent_thread_id*/ None,
        TestCodexResponsesRequestKind::Turn,
    );

    let output = client
        .compact_conversation_history(
            &prompt,
            &test_model_info(),
            /*turn_state*/ None,
            /*expected_authority*/ None,
            CompactConversationRequestSettings {
                effort: None,
                summary: codex_protocol::config_types::ReasoningSummary::None,
                service_tier: None,
            },
            &test_session_telemetry(),
            &CompactionTraceContext::disabled(),
            &responses_metadata,
        )
        .await?;

    assert!(output.is_empty());
    assert_eq!(registration_count.load(Ordering::SeqCst), 3);
    let requests = server
        .received_requests()
        .await
        .expect("server should record requests");
    let compact_request = requests
        .iter()
        .find(|request| request.url.path() == "/v1/responses/compact")
        .expect("compact request should be captured");
    assert_eq!(
        compact_request
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
        Some("Bearer test-access-token")
    );
    assert_eq!(
        compact_request
            .headers
            .get("ChatGPT-Account-ID")
            .and_then(|value| value.to_str().ok()),
        Some("account-123")
    );

    Ok(())
}

fn test_model_provider() -> SharedModelProvider {
    test_model_client(SessionSource::Cli).state.provider.clone()
}

fn test_responses_metadata_for_client(
    client: &ModelClient,
    turn_id: Option<&str>,
    window_id: String,
    parent_thread_id: Option<ThreadId>,
    request_kind: TestCodexResponsesRequestKind,
) -> CodexResponsesMetadata {
    let thread_id = client.state.thread_id.to_string();
    test_responses_metadata(
        TEST_INSTALLATION_ID,
        &thread_id,
        &thread_id,
        turn_id,
        window_id,
        &client.state.session_source,
        parent_thread_id,
        request_kind,
    )
}

fn test_model_info() -> ModelInfo {
    serde_json::from_value(json!({
        "slug": "gpt-test",
        "display_name": "gpt-test",
        "description": "desc",
        "default_reasoning_level": "medium",
        "supported_reasoning_levels": [
            {"effort": "medium", "description": "medium"}
        ],
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 1,
        "upgrade": null,
        "base_instructions": "base instructions",
        "model_messages": null,
        "support_verbosity": false,
        "default_verbosity": null,
        "apply_patch_tool_type": null,
        "truncation_policy": {"mode": "bytes", "limit": 10000},
        "supports_parallel_tool_calls": false,
        "supports_image_detail_original": false,
        "context_window": 272000,
        "auto_compact_token_limit": null,
        "experimental_supported_tools": []
    }))
    .expect("deserialize test model info")
}

fn test_session_telemetry() -> SessionTelemetry {
    SessionTelemetry::new(
        ThreadId::new(),
        "gpt-test",
        "gpt-test",
        /*account_id*/ None,
        /*account_email*/ None,
        /*auth_mode*/ None,
        "test-originator".to_string(),
        /*log_user_prompts*/ false,
        "test-terminal".to_string(),
        SessionSource::Cli,
    )
}

#[test]
fn ultra_reasoning_uses_max_for_requests() {
    assert_eq!(
        (
            super::reasoning_effort_for_request(ReasoningEffort::Ultra),
            super::reasoning_effort_for_request(ReasoningEffort::High),
        ),
        (ReasoningEffort::Max, ReasoningEffort::High,)
    );
}

fn write_chatgpt_auth_json(codex_home: &std::path::Path) {
    let auth_json = json!({
        "tokens": {
            "id_token": TEST_CHATGPT_ID_TOKEN,
            "access_token": "test-access-token",
            "refresh_token": "test-refresh-token",
            "account_id": "account-123"
        },
        "last_refresh": "2099-01-01T00:00:00Z"
    });
    std::fs::write(
        codex_home.join("auth.json"),
        serde_json::to_string_pretty(&auth_json).expect("serialize auth.json"),
    )
    .expect("write auth.json");
}

async fn chatgpt_auth_manager(
    codex_home: &TempDir,
    agent_identity_authapi_base_url: String,
) -> Arc<AuthManager> {
    write_chatgpt_auth_json(codex_home.path());
    let auth_manager = AuthManager::shared(
        codex_home.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*forced_chatgpt_workspace_id*/ None,
        /*chatgpt_base_url*/ None,
        AuthKeyringBackendKind::default(),
        codex_login::test_support::transport_default_auth_route_config(),
    )
    .await;
    let auth = auth_manager.auth().await.expect("auth should load");
    AuthManager::from_auth_for_testing_with_agent_identity_authapi_base_url(
        auth,
        agent_identity_authapi_base_url,
    )
}

#[derive(Default)]
struct TagCollectorVisitor {
    tags: BTreeMap<String, String>,
}

impl Visit for TagCollectorVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.tags
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.tags
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

#[derive(Clone)]
struct TagCollectorLayer {
    tags: Arc<Mutex<BTreeMap<String, String>>>,
}

impl<S> Layer<S> for TagCollectorLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: LayerContext<'_, S>) {
        if event.metadata().target() != "feedback_tags" {
            return;
        }
        let mut visitor = TagCollectorVisitor::default();
        event.record(&mut visitor);
        self.tags.lock().unwrap().extend(visitor.tags);
    }
}

fn started_inference_attempt(temp: &TempDir) -> anyhow::Result<InferenceTraceAttempt> {
    let writer = Arc::new(TraceWriter::create(
        temp.path(),
        "trace-1".to_string(),
        "rollout-1".to_string(),
        "thread-root".to_string(),
    )?);
    writer.append(RawTraceEventPayload::ThreadStarted {
        thread_id: "thread-root".to_string(),
        agent_path: "/root".to_string(),
        metadata_payload: None,
    })?;
    writer.append(RawTraceEventPayload::CodexTurnStarted {
        codex_turn_id: "turn-1".to_string(),
        thread_id: "thread-root".to_string(),
    })?;

    let inference_trace = InferenceTraceContext::enabled(
        writer,
        "thread-root".to_string(),
        "turn-1".to_string(),
        "gpt-test".to_string(),
        "test-provider".to_string(),
    );
    let attempt = inference_trace.start_attempt();
    attempt.record_started(&json!({
        "model": "gpt-test",
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "hello"}]
        }],
    }));
    Ok(attempt)
}

fn output_message(id: &str, text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(codex_protocol::ResponseItemId::with_suffix("msg", id)),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

async fn replay_until_cancelled(temp: &TempDir) -> anyhow::Result<RolloutTrace> {
    let mut rollout = replay_bundle(temp.path())?;
    for _ in 0..50 {
        let inference = rollout
            .inference_calls
            .values()
            .next()
            .expect("inference should be reduced");
        if inference.execution.status == ExecutionStatus::Cancelled {
            return Ok(rollout);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        rollout = replay_bundle(temp.path())?;
    }
    Ok(rollout)
}

struct NotifyAfterEventStream {
    events: VecDeque<ResponseEvent>,
    yielded: usize,
    notify_after: usize,
    notify: Arc<Notify>,
}

impl futures::Stream for NotifyAfterEventStream {
    type Item = std::result::Result<ResponseEvent, ApiError>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(event) = self.events.pop_front() else {
            return Poll::Pending;
        };
        self.yielded += 1;
        if self.yielded == self.notify_after {
            self.notify.notify_one();
        }
        Poll::Ready(Some(Ok(event)))
    }
}

#[test]
fn build_subagent_headers_sets_other_subagent_label() {
    let client = test_model_client(SessionSource::SubAgent(SubAgentSource::Other(
        "memory_consolidation".to_string(),
    )));
    let headers = client.build_subagent_headers();
    let value = headers
        .get(X_OPENAI_SUBAGENT_HEADER)
        .and_then(|value| value.to_str().ok());
    assert_eq!(value, Some("memory_consolidation"));
}

#[test]
fn build_subagent_headers_sets_internal_memory_consolidation_label() {
    let client = test_model_client(SessionSource::Internal(
        InternalSessionSource::MemoryConsolidation,
    ));
    let headers = client.build_subagent_headers();
    let value = headers
        .get(X_OPENAI_SUBAGENT_HEADER)
        .and_then(|value| value.to_str().ok());
    assert_eq!(value, Some("memory_consolidation"));
    assert_eq!(
        headers.get("originator"),
        Some(&http::HeaderValue::from_static("test_originator"))
    );
}

#[test]
fn build_ws_client_metadata_includes_window_lineage_and_turn_metadata() {
    let parent_thread_id = ThreadId::new();
    let client = test_model_client(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 2,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    }));

    let thread_id = client.state.thread_id.to_string();
    let expected_window_id = format!("{thread_id}:1");
    let responses_metadata = test_responses_metadata_for_client(
        &client,
        Some("turn-123"),
        expected_window_id.clone(),
        Some(parent_thread_id),
        TestCodexResponsesRequestKind::Turn,
    );
    let client_metadata =
        client.build_ws_client_metadata(&responses_metadata, /*use_responses_lite*/ false);
    let parent_thread_id = parent_thread_id.to_string();
    let turn_metadata: serde_json::Value = serde_json::from_str(
        client_metadata
            .get(X_CODEX_TURN_METADATA_HEADER)
            .expect("turn metadata"),
    )
    .expect("valid turn metadata");
    for (client_key, metadata_key, expected) in [
        (
            X_CODEX_INSTALLATION_ID_HEADER,
            "installation_id",
            "11111111-1111-4111-8111-111111111111",
        ),
        ("session_id", "session_id", thread_id.as_str()),
        ("thread_id", "thread_id", thread_id.as_str()),
        ("turn_id", "turn_id", "turn-123"),
        (
            X_CODEX_WINDOW_ID_HEADER,
            "window_id",
            expected_window_id.as_str(),
        ),
        (
            X_CODEX_PARENT_THREAD_ID_HEADER,
            "parent_thread_id",
            parent_thread_id.as_str(),
        ),
    ] {
        assert_eq!(
            client_metadata.get(client_key).map(String::as_str),
            Some(expected)
        );
        assert_eq!(turn_metadata[metadata_key].as_str(), Some(expected));
    }
    assert_eq!(
        client_metadata
            .get(X_OPENAI_SUBAGENT_HEADER)
            .map(String::as_str),
        Some("collab_spawn")
    );
}

#[tokio::test]
async fn summarize_memories_returns_empty_for_empty_input() {
    let client = test_model_client(SessionSource::Cli);
    let model_info = test_model_info();
    let session_telemetry = test_session_telemetry();

    let output = client
        .summarize_memories(
            Vec::new(),
            &model_info,
            /*effort*/ None,
            &session_telemetry,
        )
        .await
        .expect("empty summarize request should succeed");
    assert_eq!(output.len(), 0);
}

#[tokio::test]
async fn dropped_response_stream_traces_cancelled_partial_output() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let attempt = started_inference_attempt(&temp)?;

    // The provider has produced one complete output item, but no terminal
    // response.completed event. The harness has enough information to keep this
    // item in history, so the trace should preserve it when the stream is
    // abandoned.
    let item = output_message("1", "partial answer");
    let api_stream = futures::stream::iter([Ok(ResponseEvent::OutputItemDone(item))])
        .chain(futures::stream::pending());
    let (mut stream, _) = super::map_response_events(
        /*upstream_request_id*/ None,
        api_stream,
        test_session_telemetry(),
        attempt,
        test_model_provider(),
    );

    let observed = stream
        .next()
        .await
        .expect("mapped stream should yield output item")?;
    assert!(matches!(observed, ResponseEvent::OutputItemDone(_)));

    // Dropping the consumer is how turn interruption/preemption stops polling
    // the provider stream. The mapper task observes that drop asynchronously
    // and records cancellation using the output items it has already seen.
    drop(stream);

    // Cancellation is recorded by the mapper task after Drop wakes it, so the
    // replay may need a short wait before the terminal event appears on disk.
    let rollout = replay_until_cancelled(&temp).await?;
    let inference = rollout
        .inference_calls
        .values()
        .next()
        .expect("inference should be reduced");

    assert_eq!(inference.execution.status, ExecutionStatus::Cancelled);
    assert_eq!(inference.response_item_ids.len(), 1);
    assert_eq!(rollout.raw_payloads.len(), 2);

    Ok(())
}

#[tokio::test]
async fn response_stream_records_last_model_feedback_ids() {
    let tags = Arc::new(Mutex::new(BTreeMap::new()));
    let _guard = tracing_subscriber::registry()
        .with(TagCollectorLayer { tags: tags.clone() })
        .set_default();

    let api_stream = futures::stream::iter([
        Ok(ResponseEvent::Created),
        Ok(ResponseEvent::Completed {
            response_id: "resp-123".to_string(),
            token_usage: None,
            end_turn: Some(true),
        }),
    ]);
    let (mut stream, _) = super::map_response_events(
        Some("req-123".to_string()),
        api_stream,
        test_session_telemetry(),
        InferenceTraceAttempt::disabled(),
        test_model_provider(),
    );

    while stream.next().await.is_some() {}

    let tags = tags.lock().unwrap().clone();
    assert_eq!(
        tags.get("last_model_request_id").map(String::as_str),
        Some("\"req-123\"")
    );
    assert_eq!(
        tags.get("last_model_response_id").map(String::as_str),
        Some("\"resp-123\"")
    );
}

#[tokio::test]
async fn bedrock_unauthorized_error_uses_provider_mapping() {
    let provider = create_model_provider(
        ModelProviderInfo::create_amazon_bedrock_provider(/*aws*/ None),
        /*auth_manager*/ None,
    );
    let mut auth_recovery = None;
    let url = "https://bedrock-mantle.us-east-2.api.aws/openai/v1/responses";
    let error = super::handle_unauthorized(
        TransportError::Http {
            status: http::StatusCode::UNAUTHORIZED,
            url: Some(url.to_string()),
            headers: None,
            body: Some(
                "Signature expired: 20260609T133205Z is now earlier than 20260614T062525Z"
                    .to_string(),
            ),
        },
        &mut auth_recovery,
        &test_session_telemetry(),
        &provider,
    )
    .await
    .expect_err("expired Bedrock signature should fail");

    assert_eq!(
        error.to_string(),
        format!(
            "Amazon Bedrock rejected the request because its AWS signature has expired. Refresh your AWS credentials and retry. If `AWS_BEARER_TOKEN_BEDROCK` is set, update or unset it, then restart Codex, url: {url}"
        )
    );
}

#[tokio::test]
async fn dropped_backpressured_response_stream_traces_cancelled_partial_output()
-> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let attempt = started_inference_attempt(&temp)?;
    let backpressured_item_yielded = Arc::new(Notify::new());
    let mut events = VecDeque::new();
    for _ in 0..super::RESPONSE_STREAM_CHANNEL_CAPACITY {
        events.push_back(ResponseEvent::Created);
    }
    events.push_back(ResponseEvent::OutputItemDone(output_message(
        "1",
        "partial answer",
    )));
    let api_stream = NotifyAfterEventStream {
        events,
        yielded: 0,
        notify_after: super::RESPONSE_STREAM_CHANNEL_CAPACITY + 1,
        notify: Arc::clone(&backpressured_item_yielded),
    };

    let (stream, _) = super::map_response_events(
        /*upstream_request_id*/ None,
        api_stream,
        test_session_telemetry(),
        attempt,
        test_model_provider(),
    );

    // Fill the mapper channel with non-terminal events, then yield one output
    // item. The mapper has observed that item and is blocked trying to send it
    // downstream, so dropping the consumer covers the send-failure path rather
    // than the `consumer_dropped` select branch.
    backpressured_item_yielded.notified().await;
    drop(stream);

    let rollout = replay_until_cancelled(&temp).await?;
    let inference = rollout
        .inference_calls
        .values()
        .next()
        .expect("inference should be reduced");

    assert_eq!(inference.execution.status, ExecutionStatus::Cancelled);
    assert_eq!(inference.response_item_ids.len(), 1);
    assert_eq!(rollout.raw_payloads.len(), 2);

    Ok(())
}

#[test]
fn auth_request_telemetry_context_tracks_attached_auth_and_retry_phase() {
    let auth_context = AuthRequestTelemetryContext::new(
        Some(AuthMode::Chatgpt),
        &BearerAuthProvider::for_test(Some("access-token"), Some("workspace-123")),
        /*agent_identity_telemetry*/ None,
        PendingUnauthorizedRetry::from_recovery(UnauthorizedRecoveryExecution {
            mode: "managed",
            phase: "refresh_token",
        }),
    );

    assert_eq!(auth_context.auth_mode, Some("Chatgpt"));
    assert!(auth_context.auth_header_attached);
    assert_eq!(auth_context.auth_header_name, Some("authorization"));
    assert!(auth_context.retry_after_unauthorized);
    assert_eq!(auth_context.recovery_mode, Some("managed"));
    assert_eq!(auth_context.recovery_phase, Some("refresh_token"));
}

#[test]
fn auth_request_telemetry_context_tracks_agent_identity_ids() {
    let auth_context = AuthRequestTelemetryContext::new(
        Some(AuthMode::Chatgpt),
        &BearerAuthProvider::for_test(/*token*/ None, /*account_id*/ None),
        Some(AgentIdentityTelemetry {
            agent_id: "agent-runtime-context".to_string(),
            task_id: "task-run-context".to_string(),
        }),
        PendingUnauthorizedRetry::default(),
    );

    assert_eq!(
        auth_context.agent_identity_telemetry(),
        Some(&AgentIdentityTelemetry {
            agent_id: "agent-runtime-context".to_string(),
            task_id: "task-run-context".to_string(),
        })
    );
}

fn model_client_with_counting_attestation(
    include_attestation: bool,
) -> (ModelClient, Arc<AtomicUsize>) {
    #[derive(Debug)]
    struct CountingAttestationProvider {
        calls: Arc<AtomicUsize>,
    }

    impl AttestationProvider for CountingAttestationProvider {
        fn header_for_request(
            &self,
            _context: AttestationContext,
        ) -> GenerateAttestationFuture<'_> {
            let calls = self.calls.clone();
            Box::pin(async move {
                let call = calls.fetch_add(1, Ordering::Relaxed) + 1;
                Some(http::HeaderValue::from_bytes(format!("v1.header-{call}").as_bytes()).unwrap())
            })
        }
    }

    let attestation_calls = Arc::new(AtomicUsize::new(0));
    let (auth_manager, provider) = if include_attestation {
        (
            Some(AuthManager::from_auth_for_testing(
                CodexAuth::create_dummy_chatgpt_auth_for_testing(),
            )),
            ModelProviderInfo::create_openai_provider(Some(CHATGPT_CODEX_BASE_URL.to_string())),
        )
    } else {
        (
            None,
            create_oss_provider_with_base_url("https://example.com/v1", WireApi::Responses),
        )
    };
    let thread_id = ThreadId::new();
    let model_client = ModelClient::new(
        auth_manager,
        AgentIdentityAuthPolicy::JwtOnly,
        thread_id.into(),
        thread_id,
        TEST_INSTALLATION_ID.to_string(),
        provider,
        SessionSource::Exec,
        "test_originator".to_string(),
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
        /*concurrent_reasoning_summaries_enabled*/ false,
        Some(Arc::new(CountingAttestationProvider {
            calls: attestation_calls.clone(),
        })),
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    (model_client, attestation_calls)
}

#[tokio::test]
async fn websocket_handshake_includes_attestation_for_chatgpt_codex_responses() {
    let (model_client, attestation_calls) =
        model_client_with_counting_attestation(/*include_attestation*/ true);
    let responses_metadata = test_responses_metadata_for_client(
        &model_client,
        /*turn_id*/ None,
        format!("{}:0", model_client.state.thread_id),
        /*parent_thread_id*/ None,
        TestCodexResponsesRequestKind::WebsocketConnection,
    );

    let headers = model_client
        .build_websocket_headers(&responses_metadata)
        .await;

    assert_eq!(
        headers
            .get(crate::attestation::X_OAI_ATTESTATION_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("v1.header-1"),
    );
    assert_eq!(attestation_calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn non_chatgpt_codex_endpoints_omit_attestation_generation() {
    let (model_client, attestation_calls) =
        model_client_with_counting_attestation(/*include_attestation*/ false);
    let mut response_headers = http::HeaderMap::new();

    if let Some(header_value) = model_client.generate_attestation_header_for().await {
        response_headers.insert(crate::attestation::X_OAI_ATTESTATION_HEADER, header_value);
    }
    let mut compaction_headers = http::HeaderMap::new();
    if let Some(header_value) = model_client.generate_attestation_header_for().await {
        compaction_headers.insert(crate::attestation::X_OAI_ATTESTATION_HEADER, header_value);
    }
    let mut realtime_headers = http::HeaderMap::new();
    if let Some(header_value) = model_client.generate_attestation_header_for().await {
        realtime_headers.insert(crate::attestation::X_OAI_ATTESTATION_HEADER, header_value);
    }

    assert_eq!(
        response_headers.get(crate::attestation::X_OAI_ATTESTATION_HEADER),
        None,
    );
    assert_eq!(
        compaction_headers.get(crate::attestation::X_OAI_ATTESTATION_HEADER),
        None,
    );
    assert_eq!(
        realtime_headers.get(crate::attestation::X_OAI_ATTESTATION_HEADER),
        None,
    );
    assert_eq!(attestation_calls.load(Ordering::Relaxed), 0);
}
