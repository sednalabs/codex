use anyhow::Result;
use codex_api::ApiError;
use codex_api::map_api_error;
use codex_client::RetryOn;
use codex_client::RetryPolicy;
use codex_client::run_with_retry;
use codex_http_client::Request;
use codex_http_client::RetryAfter;
use codex_http_client::TransportError;
use codex_login::CodexAuth;
use codex_models_manager::bundled_models_response;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::items::TurnItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::user_input::UserInput;
use core_test_support::PathBufExt;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use http::Method;
use http::StatusCode;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use tokio::net::TcpSocket;
use tokio::sync::mpsc;
use tracing::Event;
use tracing::Subscriber;
use tracing::dispatcher::DefaultGuard;
use tracing::field::Field;
use tracing::field::Visit;
use tracing::span::Attributes;
use tracing::span::Id;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const FIRST_RETRY_MIN_DELAY: Duration = Duration::from_millis(180);
const FIRST_RETRY_MAX_DELAY: Duration = Duration::from_millis(220);
const SECOND_RETRY_MIN_DELAY: Duration = Duration::from_millis(360);
const SECOND_RETRY_MAX_DELAY: Duration = Duration::from_millis(440);

#[derive(Debug, PartialEq, Eq)]
pub(super) struct RetryTelemetryEvent {
    attempt: u64,
    pub(super) delay: Duration,
    pub(super) layer: String,
    pub(super) operation: String,
}

#[derive(Default)]
struct RetryTelemetryVisitor {
    name: Option<String>,
    attempt: Option<u64>,
    delay_ms: Option<u64>,
    layer: Option<String>,
    operation: Option<String>,
    is_responses_request: bool,
}

impl Visit for RetryTelemetryVisitor {
    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "retry.attempt" => self.attempt = Some(value),
            "retry.delay_ms" => self.delay_ms = Some(value),
            _ => {}
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if let Ok(value) = u64::try_from(value) {
            self.record_u64(field, value);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "event.name" => self.name = Some(value.to_string()),
            "retry.layer" => self.layer = Some(value.to_string()),
            "retry.operation" => self.operation = Some(value.to_string()),
            "message" => {
                self.is_responses_request = value
                    .split_once(": ")
                    .and_then(|(request, _)| request.split_whitespace().nth(2))
                    .is_some_and(|url| url.ends_with("/responses"));
            }
            _ => {}
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let value = format!("{value:?}");
        self.record_str(field, value.trim_matches('"'));
    }
}

struct RetryTelemetryLayer {
    events: mpsc::UnboundedSender<RetryTelemetryEvent>,
    resumptions: mpsc::UnboundedSender<Duration>,
    last_request: Mutex<Option<Instant>>,
    pending_retry: Mutex<Option<Instant>>,
}

impl RetryTelemetryLayer {
    fn record_request_after_retry(&self) {
        let now = Instant::now();
        let mut last_request = self
            .last_request
            .lock()
            .expect("last request should not be poisoned");
        let started = self
            .pending_retry
            .lock()
            .expect("pending retry should not be poisoned")
            .take();
        if let Some(started) = started {
            let _ = self.resumptions.send(now.duration_since(started));
        }
        *last_request = Some(now);
    }
}

impl<S> Layer<S> for RetryTelemetryLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        if event.metadata().target() == "codex_http_client::transport" {
            let mut visitor = RetryTelemetryVisitor::default();
            event.record(&mut visitor);
            if visitor.is_responses_request {
                self.record_request_after_retry();
            }
            return;
        }

        if event.metadata().target() != "codex_otel.trace_safe" {
            return;
        }

        let mut visitor = RetryTelemetryVisitor::default();
        event.record(&mut visitor);
        if visitor.name.as_deref() != Some("codex.retry") {
            return;
        }

        let retry = RetryTelemetryEvent {
            attempt: visitor
                .attempt
                .expect("retry event should include an attempt"),
            delay: Duration::from_millis(
                visitor
                    .delay_ms
                    .expect("retry event should include its selected delay"),
            ),
            layer: visitor
                .layer
                .expect("retry event should identify its layer"),
            operation: visitor
                .operation
                .expect("retry event should identify its operation"),
        };
        // The deadline is captured before retry telemetry. Starting at the failed request
        // gives a lower bound that still holds if the test is descheduled before telemetry.
        let started = *self
            .last_request
            .lock()
            .expect("last request should not be poisoned");
        *self
            .pending_retry
            .lock()
            .expect("pending retry should not be poisoned") = started;
        let _ = self.events.send(retry);
    }

    fn on_new_span(&self, attributes: &Attributes<'_>, _id: &Id, _context: Context<'_, S>) {
        if matches!(
            attributes.metadata().name(),
            "responses_websocket.connect" | "responses_websocket.stream_request"
        ) {
            self.record_request_after_retry();
        }
    }
}

pub(super) struct RetryTelemetryCapture {
    events: mpsc::UnboundedReceiver<RetryTelemetryEvent>,
    resumptions: mpsc::UnboundedReceiver<Duration>,
    _subscriber: DefaultGuard,
    _interest_cache_guard: tracing::Dispatch,
}

impl RetryTelemetryCapture {
    pub(super) fn install() -> Self {
        let (sender, events) = mpsc::unbounded_channel();
        let (resumptions_sender, resumptions) = mpsc::unbounded_channel();
        // Avoid caching no interest when another test first reaches a shared callsite.
        let interest_cache_guard =
            tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let subscriber = tracing_subscriber::registry()
            .with(RetryTelemetryLayer {
                events: sender,
                resumptions: resumptions_sender,
                last_request: Mutex::new(None),
                pending_retry: Mutex::new(None),
            })
            .set_default();
        tracing::callsite::rebuild_interest_cache();

        Self {
            events,
            resumptions,
            _subscriber: subscriber,
            _interest_cache_guard: interest_cache_guard,
        }
    }

    pub(super) async fn next_retry(&mut self) -> RetryTelemetryEvent {
        tokio::time::timeout(Duration::from_secs(10), self.events.recv())
            .await
            .expect("timed out waiting for retry telemetry")
            .expect("retry telemetry subscriber should remain installed")
    }
}

async fn wait_for_retry(
    telemetry: &mut RetryTelemetryCapture,
    retry: &RetryTelemetryEvent,
) -> Duration {
    let elapsed = tokio::time::timeout(
        retry.delay + Duration::from_secs(10),
        telemetry.resumptions.recv(),
    )
    .await
    .expect("timed out waiting for the request after a retry")
    .expect("retry should start another request after its sleep");
    assert!(
        elapsed >= retry.delay,
        "{} {} retry waited {elapsed:?}, less than its selected {:?} delay",
        retry.layer,
        retry.operation,
        retry.delay
    );
    elapsed
}

async fn submit_user_input(test: &TestCodex, text: &str) -> Result<()> {
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    Ok(())
}

async fn submit_plan_input(test: &TestCodex, text: &str) -> Result<()> {
    let cwd = std::env::current_dir()?.abs();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd.as_path());
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(
                codex_protocol::protocol::ThreadSettingsOverrides {
                    environments: Some(local_selections(cwd)),
                    approval_policy: Some(AskForApproval::Never),
                    sandbox_policy: Some(sandbox_policy),
                    permission_profile,
                    collaboration_mode: Some(CollaborationMode {
                        mode: ModeKind::Plan,
                        settings: Settings {
                            model: test.session_configured.model.clone(),
                            reasoning_effort: None,
                            developer_instructions: None,
                        },
                    }),
                    ..Default::default()
                },
            ),
        )
        .await?;
    Ok(())
}

async fn wait_for_turn_completion(test: &TestCodex) {
    let EventMsg::TurnComplete(completed) = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await
    else {
        unreachable!("predicate guarantees a turn complete event");
    };
    assert_eq!(completed.error, None, "turn should complete successfully");
}

/// HTTP overloads use server advice within the request and stream retry budgets.
#[tokio::test(flavor = "current_thread")]
async fn responses_http_uses_retry_after() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            ResponseTemplate::new(503)
                .insert_header("Retry-After", "1")
                .set_body_json(json!({ "error": { "code": "server_is_overloaded" } })),
            ResponseTemplate::new(503)
                .insert_header("Retry-After", "1")
                .set_body_json(json!({ "error": { "code": "server_is_overloaded" } })),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("recovered"),
                responses::ev_completed("recovered"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(1);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;

    submit_user_input(&test, "retry the upstream overload").await?;
    let retry = telemetry.next_retry().await;
    assert!(retry.delay <= Duration::from_secs(1));
    assert_eq!(
        retry,
        RetryTelemetryEvent {
            attempt: 1,
            delay: retry.delay,
            layer: "http".into(),
            operation: "request".into(),
        }
    );
    assert!(wait_for_retry(&mut telemetry, &retry).await >= Duration::from_secs(1));
    let retry = telemetry.next_retry().await;
    assert!(retry.delay <= Duration::from_secs(1));
    assert_eq!(
        retry,
        RetryTelemetryEvent {
            attempt: 1,
            delay: retry.delay,
            layer: "stream".into(),
            operation: "sampling".into(),
        }
    );
    assert!(wait_for_retry(&mut telemetry, &retry).await >= Duration::from_secs(1));
    wait_for_turn_completion(&test).await;

    assert_eq!(response_mock.requests().len(), 3);
    assert_eq!(
        telemetry.events.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );
    Ok(())
}

/// A 429 with server advice retries in the sampling loop and completes without a terminal error.
#[tokio::test(flavor = "current_thread")]
async fn responses_http_429_uses_retry_after() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "1")
                .set_body_json(json!({ "error": { "code": "rate_limit_exceeded" } })),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("recovered"),
                responses::ev_completed("recovered"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;

    submit_user_input(&test, "retry the rate-limited request").await?;
    let retry = telemetry.next_retry().await;
    assert!(retry.delay <= Duration::from_secs(1));
    assert_eq!(
        retry,
        RetryTelemetryEvent {
            attempt: 1,
            delay: retry.delay,
            layer: "stream".into(),
            operation: "sampling".into(),
        }
    );
    assert!(wait_for_retry(&mut telemetry, &retry).await >= Duration::from_secs(1));
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Error(error) => panic!("unexpected terminal error: {error:?}"),
            EventMsg::TurnComplete(completed) => {
                assert_eq!(completed.error, None);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(response_mock.requests().len(), 2);
    assert_eq!(
        telemetry.events.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );
    Ok(())
}

/// Check backoff without a live server or tracing events coordinating the retry loop.
#[tokio::test(start_paused = true)]
async fn http_retry_backoff_exhausts_attempts() {
    let attempts = Mutex::new(Vec::new());
    let result = run_with_retry(
        RetryPolicy {
            max_attempts: 2,
            base_delay: Duration::from_millis(200),
            retry_on: RetryOn {
                retry_429: false,
                retry_5xx: true,
                retry_transport: false,
            },
        },
        || Request::new(Method::POST, "http://localhost/v1/responses".into()),
        |_, attempt| {
            attempts
                .lock()
                .expect("retry attempts should not be poisoned")
                .push((attempt, tokio::time::Instant::now()));
            std::future::ready(Err::<(), _>(TransportError::Http {
                retry_after: None,
                status: StatusCode::SERVICE_UNAVAILABLE,
                url: None,
                headers: None,
                body: None,
            }))
        },
    )
    .await;

    assert!(
        matches!(result, Err(TransportError::Http { status, .. }) if status == StatusCode::SERVICE_UNAVAILABLE)
    );
    let attempts = attempts
        .into_inner()
        .expect("retry attempts should not be poisoned");
    assert_eq!(
        attempts
            .iter()
            .map(|(attempt, _)| *attempt)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(
        (FIRST_RETRY_MIN_DELAY..=FIRST_RETRY_MAX_DELAY).contains(&(attempts[1].1 - attempts[0].1))
    );
    assert!(
        (SECOND_RETRY_MIN_DELAY..=SECOND_RETRY_MAX_DELAY)
            .contains(&(attempts[2].1 - attempts[1].1))
    );
}

/// Exhausting HTTP retries and mapping the error preserve the last server deadline.
#[tokio::test(start_paused = true)]
async fn exhausted_http_retries_preserve_deadline_through_error_mapping() {
    use tokio::time::Instant;

    let started = Instant::now();
    let transport_error = run_with_retry(
        RetryPolicy {
            max_attempts: 1,
            base_delay: Duration::from_millis(200),
            retry_on: RetryOn {
                retry_429: false,
                retry_5xx: true,
                retry_transport: false,
            },
        },
        || Request::new(Method::POST, "http://localhost/v1/responses".into()),
        |_, attempt| {
            let elapsed = Instant::now() - started;
            assert_eq!(elapsed.as_secs(), attempt * 3);
            let seconds = if attempt == 0 { 3 } else { 10 };
            std::future::ready(Err::<(), _>(TransportError::Http {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                url: None,
                headers: None,
                body: None,
                retry_after: RetryAfter::from_delay(Duration::from_secs(seconds)),
            }))
        },
    )
    .await
    .unwrap_err();
    assert_eq!(Instant::now() - started, Duration::from_secs(3));
    tokio::time::advance(Duration::from_secs(4)).await;
    let outer_error = map_api_error(ApiError::Transport(transport_error));
    let retry_after = outer_error.retry_after().expect("retry advice");
    assert_eq!(retry_after.deadline(), started + Duration::from_secs(13));
    assert_eq!(
        outer_error.server_retry_delay(),
        Some(Duration::from_secs(6))
    );
    tokio::time::advance(Duration::from_secs(6)).await;
    assert_eq!(outer_error.retry_after(), Some(retry_after));
    assert_eq!(outer_error.server_retry_delay(), Some(Duration::ZERO));
}

/// Three overload failures exceed both configured request and stream budgets,
/// yet the original turn completes without a second user submission.
#[tokio::test(flavor = "current_thread")]
async fn responses_http_overload_recovers_beyond_old_retry_limits() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let overload = ResponseTemplate::new(503)
        .set_body_json(json!({ "error": { "code": "server_is_overloaded" } }));
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            overload.clone(),
            overload.clone(),
            overload,
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("recovered-after-capacity"),
                responses::ev_completed("recovered-after-capacity"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;

    submit_user_input(&test, "recover this same turn after capacity").await?;
    let mut reconnect_messages = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match wait_for_event(&test.codex, |_| true).await {
                EventMsg::Error(error) => panic!("transient capacity became terminal: {error:?}"),
                EventMsg::StreamError(error) => reconnect_messages.push(error.message),
                EventMsg::TurnComplete(event) => {
                    assert_eq!(event.error, None);
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("capacity should recover the same turn within 10 seconds");

    assert_eq!(reconnect_messages.len(), 3);
    assert!(
        reconnect_messages
            .iter()
            .all(|message| message.contains("Model at capacity; retrying"))
    );
    let request_count = server
        .received_requests()
        .await
        .expect("mock server should record requests")
        .into_iter()
        .filter(|request| request.url.path() == "/v1/responses")
        .count();
    assert_eq!(
        request_count, 4,
        "three overloads beyond the old budgets must not terminalize the turn"
    );
    assert_eq!(response_mock.requests().len(), 4);

    Ok(())
}

/// A failed response can already have emitted accepted output and executed a
/// tool. Replaying their exact provider IDs must not accept either effect
/// twice; a new call ID in the recovery response still executes normally.
#[tokio::test(flavor = "current_thread")]
async fn capacity_retry_deduplicates_accepted_output_and_tool_ids() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let first_call_id = "capacity-accepted-call";
    let distinct_call_id = "capacity-distinct-call";
    let partial_id = "capacity-accepted-message";
    let effects_dir = tempfile::tempdir()?;
    let effect_path = effects_dir.path().join("effect.txt");
    let first_args = json!({
        "command": format!("printf 'first\\n' >> {effect_path:?}"),
        "login": false,
        "timeout_ms": 5_000,
    })
    .to_string();
    let distinct_args = json!({
        "command": format!("printf 'distinct\\n' >> {effect_path:?}"),
        "login": false,
        "timeout_ms": 5_000,
    })
    .to_string();
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("capacity-partial-failed"),
                responses::ev_message_item_added(partial_id, ""),
                responses::ev_output_text_delta("accepted before failure"),
                responses::ev_assistant_message(partial_id, "accepted before failure"),
                responses::ev_function_call(first_call_id, "shell_command", &first_args),
                json!({
                    "type": "response.failed",
                    "response": {
                        "id": "capacity-partial-failed",
                        "error": {
                            "code": "server_is_overloaded",
                            "message": "Selected model is at capacity."
                        }
                    }
                }),
            ])),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("capacity-replay-recovery"),
                responses::ev_message_item_added(partial_id, ""),
                responses::ev_output_text_delta("accepted before failure"),
                responses::ev_assistant_message(partial_id, "accepted before failure"),
                responses::ev_function_call(first_call_id, "shell_command", &first_args),
                responses::ev_function_call(distinct_call_id, "shell_command", &distinct_args),
                responses::ev_completed("capacity-replay-recovery"),
            ])),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("capacity-replay-finished"),
                responses::ev_assistant_message("capacity-final-message", "finished"),
                responses::ev_completed("capacity-replay-finished"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;
    submit_user_input(&test, "recover the accepted effects").await?;

    let mut accepted_message_ids = Vec::new();
    let mut streamed_partial_deltas = 0;
    let mut retry_events = 0;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::ItemCompleted(event) => {
                if let TurnItem::AgentMessage(item) = event.item {
                    accepted_message_ids.push(item.id);
                }
            }
            EventMsg::AgentMessageContentDelta(event)
                if event.item_id == partial_id
                    && event.delta.contains("accepted before failure") =>
            {
                streamed_partial_deltas += 1;
            }
            EventMsg::StreamError(_) => retry_events += 1,
            EventMsg::Error(error) => panic!("capacity replay became terminal: {error:?}"),
            EventMsg::TurnComplete(event) => {
                assert_eq!(event.error, None);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(retry_events, 1);
    assert_eq!(streamed_partial_deltas, 1);
    assert_eq!(
        accepted_message_ids
            .iter()
            .filter(|id| id.as_str() == partial_id)
            .count(),
        1,
        "only one completed assistant item with the accepted provider ID",
    );
    assert_eq!(std::fs::read_to_string(&effect_path)?, "first\ndistinct\n");

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    let retry_body = requests[1].body_json();
    let retry_items = retry_body["input"].as_array().expect("retry prompt input");
    assert_eq!(
        retry_items
            .iter()
            .filter(|item| item["type"] == "message" && item["id"] == partial_id)
            .count(),
        1,
        "the completed assistant item, not merely its draft delta, entered history",
    );
    assert_eq!(
        retry_items
            .iter()
            .filter(|item| item["type"] == "function_call" && item["call_id"] == first_call_id)
            .count(),
        1,
    );
    assert_eq!(
        retry_items
            .iter()
            .filter(|item| {
                item["type"] == "function_call_output" && item["call_id"] == first_call_id
            })
            .count(),
        1,
    );
    let follow_up_body = requests[2].body_json();
    let follow_up_items = follow_up_body["input"].as_array().expect("follow-up input");
    assert_eq!(
        follow_up_items
            .iter()
            .filter(|item| {
                item["type"] == "function_call_output" && item["call_id"] == first_call_id
            })
            .count(),
        1,
    );
    assert_eq!(
        follow_up_items
            .iter()
            .filter(|item| {
                item["type"] == "function_call_output" && item["call_id"] == distinct_call_id
            })
            .count(),
        1,
    );
    Ok(())
}

/// Reusing a call ID for a different action is a provider protocol conflict,
/// not an idempotent replay and not a reason to execute the new action.
#[tokio::test(flavor = "current_thread")]
async fn capacity_retry_rejects_conflicting_tool_call_id() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let effects_dir = tempfile::tempdir()?;
    let effect_path = effects_dir.path().join("effect.txt");
    let accepted_args = json!({
        "command": format!("printf 'accepted\\n' >> {effect_path:?}"),
        "login": false,
        "timeout_ms": 5_000,
    })
    .to_string();
    let conflicting_args = json!({
        "command": format!("printf 'conflict\\n' >> {effect_path:?}"),
        "login": false,
        "timeout_ms": 5_000,
    })
    .to_string();
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("capacity-conflict-failed"),
                responses::ev_function_call(
                    "capacity-conflict-call",
                    "shell_command",
                    &accepted_args,
                ),
                json!({
                    "type": "response.failed",
                    "response": {
                        "id": "capacity-conflict-failed",
                        "error": {
                            "code": "server_is_overloaded",
                            "message": "Selected model is at capacity."
                        }
                    }
                }),
            ])),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("capacity-conflict-replayed"),
                responses::ev_function_call(
                    "capacity-conflict-call",
                    "shell_command",
                    &conflicting_args,
                ),
                responses::ev_completed("capacity-conflict-replayed"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;
    submit_user_input(&test, "reject a changed accepted call").await?;
    let mut terminal_errors = 0;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Error(error) => {
                terminal_errors += 1;
                assert!(error.message.contains("different arguments"));
            }
            EventMsg::TurnComplete(event) => {
                assert!(event.error.is_some());
                break;
            }
            _ => {}
        }
    }
    assert_eq!(terminal_errors, 1);
    assert_eq!(std::fs::read_to_string(&effect_path)?, "accepted\n");
    assert_eq!(response_mock.requests().len(), 2);
    Ok(())
}

/// An unfinished streamed item is not an accepted history item. Its stable
/// provider ID still correlates a draft replay, so only the first visible
/// delta is sent; a distinct new item keeps normal streaming behavior.
#[tokio::test(flavor = "current_thread")]
async fn capacity_retry_suppresses_same_id_unfinished_draft_replay() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let draft_id = "capacity-unfinished-draft";
    let distinct_id = "capacity-distinct-message";
    let failed = responses::sse_response(responses::sse(vec![
        responses::ev_response_created("capacity-draft-failed"),
        responses::ev_message_item_added(draft_id, ""),
        responses::ev_output_text_delta("draft line\n"),
        json!({
            "type": "response.failed",
            "response": {
                "id": "capacity-draft-failed",
                "error": {
                    "code": "server_is_overloaded",
                    "message": "Selected model is at capacity."
                }
            }
        }),
    ]));
    let recovered = responses::sse_response(responses::sse(vec![
        responses::ev_response_created("capacity-draft-recovered"),
        responses::ev_message_item_added(draft_id, ""),
        responses::ev_output_text_delta("draft line\n"),
        responses::ev_assistant_message(draft_id, "draft line\n"),
        responses::ev_message_item_added(distinct_id, ""),
        responses::ev_output_text_delta("distinct line\n"),
        responses::ev_assistant_message(distinct_id, "distinct line\n"),
        responses::ev_completed("capacity-draft-recovered"),
    ]));
    let response_mock = responses::mount_response_sequence(&server, vec![failed, recovered]).await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;
    submit_user_input(&test, "recover an unfinished streamed draft").await?;

    let mut draft_deltas = Vec::new();
    let mut distinct_deltas = Vec::new();
    let mut completed_ids = Vec::new();
    let mut retry_events = 0;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::AgentMessageContentDelta(event) if event.item_id == draft_id => {
                draft_deltas.push(event.delta);
            }
            EventMsg::AgentMessageContentDelta(event) if event.item_id == distinct_id => {
                distinct_deltas.push(event.delta);
            }
            EventMsg::ItemCompleted(event) => {
                if let TurnItem::AgentMessage(item) = event.item {
                    completed_ids.push(item.id);
                }
            }
            EventMsg::StreamError(_) => retry_events += 1,
            EventMsg::Error(error) => panic!("draft replay became terminal: {error:?}"),
            EventMsg::TurnComplete(event) => {
                assert_eq!(event.error, None);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(retry_events, 1);
    assert_eq!(draft_deltas, vec!["draft line\n"]);
    assert_eq!(distinct_deltas, vec!["distinct line\n"]);
    assert_eq!(
        completed_ids
            .iter()
            .filter(|id| id.as_str() == draft_id)
            .count(),
        1
    );
    assert_eq!(
        completed_ids
            .iter()
            .filter(|id| id.as_str() == distinct_id)
            .count(),
        1
    );
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].body_json()["input"]
            .as_array()
            .expect("retry prompt input")
            .iter()
            .filter(|item| item["type"] == "message" && item["id"] == draft_id)
            .count(),
        0,
        "an unfinished draft is not accepted history",
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn capacity_retry_interrupted_before_draft_done_keeps_one_delta() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let draft_id = "capacity-interrupted-draft";
    let failed_response = |response_id: &str| {
        responses::sse_response(responses::sse(vec![
            responses::ev_response_created(response_id),
            responses::ev_message_item_added(draft_id, ""),
            responses::ev_output_text_delta("interrupted draft\n"),
            json!({
                "type": "response.failed",
                "response": {
                    "id": response_id,
                    "error": {
                        "code": "server_is_overloaded",
                        "message": "Selected model is at capacity."
                    }
                }
            }),
        ]))
    };
    let response_mock = responses::mount_response_sequence(
        &server,
        (0..5)
            .map(|index| failed_response(&format!("capacity-interrupted-{index}")))
            .collect(),
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;
    submit_user_input(&test, "interrupt the draft replay").await?;
    let mut draft_deltas = Vec::new();
    let mut completed_draft_items = 0;
    let mut retry_events = 0;
    while retry_events < 2 {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::AgentMessageContentDelta(event) if event.item_id == draft_id => {
                draft_deltas.push(event.delta);
            }
            EventMsg::ItemCompleted(event) if matches!(&event.item, TurnItem::AgentMessage(item) if item.id == draft_id) =>
            {
                completed_draft_items += 1;
            }
            EventMsg::StreamError(_) => retry_events += 1,
            EventMsg::Error(error) => panic!("draft retry became terminal: {error:?}"),
            _ => {}
        }
    }
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    assert_eq!(draft_deltas, vec!["interrupted draft\n"]);
    assert_eq!(completed_draft_items, 0);
    assert!(response_mock.requests().len() >= 2);
    Ok(())
}

/// Plan-mode normal text and plan text share the same provider item identity.
/// A failed unfinished item may be replayed, but its visible deltas must not be.
#[tokio::test(flavor = "current_thread")]
async fn capacity_retry_suppresses_same_id_unfinished_plan_draft_replay() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let draft_id = "capacity-plan-draft";
    let distinct_id = "capacity-plan-distinct";
    let plan_text = "Intro\n<proposed_plan>\n- Step 1\n</proposed_plan>";
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("capacity-plan-failed"),
                responses::ev_message_item_added(draft_id, ""),
                responses::ev_output_text_delta(plan_text),
                json!({
                    "type": "response.failed",
                    "response": {
                        "id": "capacity-plan-failed",
                        "error": {
                            "code": "server_is_overloaded",
                            "message": "Selected model is at capacity."
                        }
                    }
                }),
            ])),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("capacity-plan-recovered"),
                responses::ev_message_item_added(draft_id, ""),
                responses::ev_output_text_delta(plan_text),
                responses::ev_assistant_message(draft_id, plan_text),
                responses::ev_message_item_added(distinct_id, ""),
                responses::ev_output_text_delta("Distinct note\n"),
                responses::ev_assistant_message(distinct_id, "Distinct note\n"),
                responses::ev_completed("capacity-plan-recovered"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;
    submit_plan_input(&test, "recover an unfinished plan draft").await?;

    let mut draft_text = String::new();
    let mut distinct_text = String::new();
    let mut plan_deltas = String::new();
    let mut completed_draft = 0;
    let mut completed_plan = 0;
    let mut retries = 0;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::AgentMessageContentDelta(event) if event.item_id == draft_id => {
                draft_text.push_str(&event.delta);
            }
            EventMsg::AgentMessageContentDelta(event) if event.item_id == distinct_id => {
                distinct_text.push_str(&event.delta);
            }
            EventMsg::PlanDelta(event) => plan_deltas.push_str(&event.delta),
            EventMsg::ItemCompleted(event) => match event.item {
                TurnItem::AgentMessage(item) if item.id == draft_id => completed_draft += 1,
                TurnItem::Plan(_) => completed_plan += 1,
                _ => {}
            },
            EventMsg::StreamError(_) => retries += 1,
            EventMsg::Error(error) => panic!("plan replay became terminal: {error:?}"),
            EventMsg::TurnComplete(event) => {
                assert_eq!(event.error, None);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(retries, 1);
    assert_eq!(draft_text, "Intro\n");
    assert_eq!(plan_deltas, "- Step 1\n");
    assert_eq!(distinct_text, "Distinct note\n");
    assert_eq!(completed_draft, 1);
    assert_eq!(completed_plan, 1);
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].body_json()["input"]
            .as_array()
            .expect("retry prompt input")
            .iter()
            .filter(|item| item["type"] == "message" && item["id"] == draft_id)
            .count(),
        0,
        "the failed plan draft is not accepted history",
    );
    Ok(())
}

/// Added-item seed text uses the same visibility signal as direct deltas.
#[tokio::test(flavor = "current_thread")]
async fn capacity_retry_interrupted_before_seeded_plan_done_keeps_one_delta() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let draft_id = "capacity-seeded-plan-draft";
    let seeded = "Intro\n<proposed_plan>\n- Step 1\n";
    let failed_response = |response_id: &str| {
        responses::sse_response(responses::sse(vec![
            responses::ev_response_created(response_id),
            responses::ev_message_item_added(draft_id, seeded),
            json!({
                "type": "response.failed",
                "response": {
                    "id": response_id,
                    "error": {
                        "code": "server_is_overloaded",
                        "message": "Selected model is at capacity."
                    }
                }
            }),
        ]))
    };
    let response_mock = responses::mount_response_sequence(
        &server,
        (0..5)
            .map(|index| failed_response(&format!("capacity-seeded-plan-{index}")))
            .collect(),
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;
    submit_plan_input(&test, "interrupt seeded plan replay").await?;

    let mut draft_text = String::new();
    let mut plan_deltas = String::new();
    let mut completed_plan = 0;
    let mut retries = 0;
    while retries < 2 {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::AgentMessageContentDelta(event) if event.item_id == draft_id => {
                draft_text.push_str(&event.delta);
            }
            EventMsg::PlanDelta(event) => plan_deltas.push_str(&event.delta),
            EventMsg::ItemCompleted(event) if matches!(event.item, TurnItem::Plan(_)) => {
                completed_plan += 1;
            }
            EventMsg::StreamError(_) => retries += 1,
            EventMsg::Error(error) => panic!("seeded plan retry became terminal: {error:?}"),
            _ => {}
        }
    }
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    assert_eq!(draft_text, "Intro\n");
    assert_eq!(plan_deltas, "- Step 1\n");
    assert_eq!(completed_plan, 0);
    assert!(response_mock.requests().len() >= 2);
    Ok(())
}

/// Parsing a start marker is not itself visible plan text and must not cause
/// the retry to discard the first actual plan delta.
#[tokio::test(flavor = "current_thread")]
async fn capacity_retry_plan_marker_only_does_not_suppress_replay() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let draft_id = "capacity-marker-only-plan";
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("capacity-marker-failed"),
                responses::ev_message_item_added(draft_id, ""),
                responses::ev_output_text_delta("<proposed_plan>\n"),
                json!({
                    "type": "response.failed",
                    "response": {
                        "id": "capacity-marker-failed",
                        "error": {
                            "code": "server_is_overloaded",
                            "message": "Selected model is at capacity."
                        }
                    }
                }),
            ])),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("capacity-marker-recovered"),
                responses::ev_message_item_added(draft_id, ""),
                responses::ev_output_text_delta("<proposed_plan>\n- Step 1\n</proposed_plan>"),
                responses::ev_assistant_message(
                    draft_id,
                    "<proposed_plan>\n- Step 1\n</proposed_plan>",
                ),
                responses::ev_completed("capacity-marker-recovered"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;
    submit_plan_input(&test, "recover after a marker-only attempt").await?;
    let mut plan_deltas = String::new();
    let mut completed_plan = 0;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::PlanDelta(event) => plan_deltas.push_str(&event.delta),
            EventMsg::ItemCompleted(event) if matches!(event.item, TurnItem::Plan(_)) => {
                completed_plan += 1;
            }
            EventMsg::Error(error) => panic!("marker-only replay became terminal: {error:?}"),
            EventMsg::TurnComplete(event) => {
                assert_eq!(event.error, None);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(plan_deltas, "- Step 1\n");
    assert_eq!(completed_plan, 1);
    assert_eq!(response_mock.requests().len(), 2);
    Ok(())
}

/// The error-path parser flush can reveal a partial tag as ordinary text.
/// That text is visible before retry and must enter the same draft-ID guard.
#[tokio::test(flavor = "current_thread")]
async fn capacity_retry_plan_failed_parser_flush_suppresses_draft_replay() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let draft_id = "capacity-plan-flushed-draft";
    let draft_text = "<propo";
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("capacity-plan-flush-failed"),
                responses::ev_message_item_added(draft_id, ""),
                responses::ev_output_text_delta(draft_text),
                json!({
                    "type": "response.failed",
                    "response": {
                        "id": "capacity-plan-flush-failed",
                        "error": {
                            "code": "server_is_overloaded",
                            "message": "Selected model is at capacity."
                        }
                    }
                }),
            ])),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("capacity-plan-flush-recovered"),
                responses::ev_message_item_added(draft_id, ""),
                responses::ev_output_text_delta(draft_text),
                responses::ev_assistant_message(draft_id, draft_text),
                responses::ev_completed("capacity-plan-flush-recovered"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;
    submit_plan_input(&test, "recover parser-flushed plan text").await?;
    let mut visible_text = String::new();
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::AgentMessageContentDelta(event) if event.item_id == draft_id => {
                visible_text.push_str(&event.delta);
            }
            EventMsg::Error(error) => panic!("parser-flush replay became terminal: {error:?}"),
            EventMsg::TurnComplete(event) => {
                assert_eq!(event.error, None);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(visible_text, draft_text);
    assert_eq!(response_mock.requests().len(), 2);
    Ok(())
}

/// A plan-mode Done is accepted output, not a second draft. Replaying its
/// exact provider ID after a failed response must not complete the plan twice.
#[tokio::test(flavor = "current_thread")]
async fn capacity_retry_deduplicates_accepted_plan_item_done() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let item_id = "capacity-accepted-plan";
    let plan_text = "Intro\n<proposed_plan>\n- Step 1\n</proposed_plan>";
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("capacity-accepted-plan-failed"),
                responses::ev_message_item_added(item_id, ""),
                responses::ev_output_text_delta(plan_text),
                responses::ev_assistant_message(item_id, plan_text),
                json!({
                    "type": "response.failed",
                    "response": {
                        "id": "capacity-accepted-plan-failed",
                        "error": {
                            "code": "server_is_overloaded",
                            "message": "Selected model is at capacity."
                        }
                    }
                }),
            ])),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("capacity-accepted-plan-recovered"),
                responses::ev_message_item_added(item_id, ""),
                responses::ev_output_text_delta(plan_text),
                responses::ev_assistant_message(item_id, plan_text),
                responses::ev_completed("capacity-accepted-plan-recovered"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&server)
        .await?;
    submit_plan_input(&test, "accept the completed plan only once").await?;
    let mut plan_deltas = String::new();
    let mut completed_plan = 0;
    let mut completed_message = 0;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::PlanDelta(event) => plan_deltas.push_str(&event.delta),
            EventMsg::ItemCompleted(event) => match event.item {
                TurnItem::Plan(_) => completed_plan += 1,
                TurnItem::AgentMessage(message) if message.id == item_id => {
                    completed_message += 1;
                }
                _ => {}
            },
            EventMsg::Error(error) => panic!("accepted plan replay became terminal: {error:?}"),
            EventMsg::TurnComplete(event) => {
                assert_eq!(event.error, None);
                break;
            }
            _ => {}
        }
    }
    assert_eq!(plan_deltas, "- Step 1\n");
    assert_eq!(completed_plan, 1);
    assert_eq!(completed_message, 1);
    assert_eq!(response_mock.requests().len(), 2);
    Ok(())
}

/// A server deadline may exceed the local backoff cap, but remains interruptible.
#[tokio::test(flavor = "current_thread")]
async fn sustained_http_503_honors_retry_after_and_interrupts() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_mock_server().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "120"))
        .mount(&server)
        .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        })
        .build_with_auto_env(&server)
        .await?;

    submit_user_input(&test, "wait for this temporary outage").await?;
    let retry = telemetry.next_retry().await;
    assert_eq!(retry.layer, "stream");
    assert_eq!(retry.operation, "sampling");
    assert!(retry.delay >= Duration::from_secs(119));
    let event = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::StreamError(_))
    })
    .await;
    let EventMsg::StreamError(progress) = event else {
        unreachable!("predicate requires recovery progress");
    };
    assert!(
        progress
            .message
            .contains("Service temporarily unavailable; retrying")
    );
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    let requests = server.received_requests().await.expect("mock requests");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/v1/responses")
            .count(),
        1,
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn responses_http_504_preserves_status_after_stream_retry() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock =
        responses::mount_response_sequence(&server, vec![ResponseTemplate::new(504); 2]).await;
    let test = test_codex()
        .with_config(|config| {
            config.service_tier = Some("flex".to_string());
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;

    submit_user_input(&test, "surface the timeout after one retry").await?;
    assert_terminal_failure(
        &test,
        CodexErrorInfo::HttpConnectionFailed {
            http_status_code: Some(504),
        },
        /*expected_retries*/ 1,
    )
    .await?;
    assert_eq!(response_mock.requests().len(), 2);
    Ok(())
}

/// Remote compaction v2 uses the upstream header before another HTTP request.
#[tokio::test(flavor = "current_thread")]
async fn compact_v2_uses_retry_after() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("seed"),
                responses::ev_completed("seed"),
            ])),
            ResponseTemplate::new(503)
                .insert_header("Retry-After", "1")
                .set_body_json(json!({ "error": { "code": "server_is_overloaded" } })),
            responses::sse_response(responses::sse(vec![
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "RETRIED_COMPACTION_SUMMARY",
                    }
                }),
                responses::ev_completed("compacted"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(1);
            config.model_provider.stream_max_retries = Some(0);
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("seed history for compaction").await?;

    test.codex.submit(Op::Compact).await?;
    let retry = telemetry.next_retry().await;
    assert!(retry.delay <= Duration::from_secs(1));
    assert_eq!(
        retry,
        RetryTelemetryEvent {
            attempt: 1,
            delay: retry.delay,
            layer: "http".into(),
            operation: "request".into(),
        }
    );
    assert!(wait_for_retry(&mut telemetry, &retry).await >= Duration::from_secs(1));
    wait_for_turn_completion(&test).await;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert_eq!(request.path(), "/v1/responses");
        assert!(
            !request.inputs_of_type("compaction_trigger").is_empty(),
            "expected a remote compaction v2 request"
        );
    }
    assert_eq!(
        telemetry.events.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );
    Ok(())
}

// TODO(anp) respect Retry-After
/// Remote compaction v2 stream failures retry without using the enclosing response header.
#[tokio::test(flavor = "current_thread")]
async fn compact_v2_stream_failure_uses_local_backoff_despite_retry_after() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("seed"),
                responses::ev_completed("seed"),
            ])),
            responses::sse_response(responses::sse_failed(
                "rate-limited",
                "rate_limit_exceeded",
                "Rate limit exceeded.",
            ))
            .insert_header("Retry-After", "1"),
            responses::sse_response(responses::sse(vec![
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "RETRIED_COMPACTION_SUMMARY",
                    }
                }),
                responses::ev_completed("compacted"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("seed history for compaction").await?;

    test.codex.submit(Op::Compact).await?;
    let retry = telemetry.next_retry().await;
    assert!((FIRST_RETRY_MIN_DELAY..FIRST_RETRY_MAX_DELAY).contains(&retry.delay));
    assert_eq!(
        retry,
        RetryTelemetryEvent {
            attempt: 1,
            delay: retry.delay,
            layer: "stream".into(),
            operation: "remote_compaction_v2".into(),
        }
    );
    wait_for_retry(&mut telemetry, &retry).await;
    wait_for_turn_completion(&test).await;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert_eq!(request.path(), "/v1/responses");
        assert!(
            !request.inputs_of_type("compaction_trigger").is_empty(),
            "expected a remote compaction v2 request"
        );
    }
    assert_eq!(
        telemetry.events.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );

    Ok(())
}

/// Headerless remote compaction stream rate limits exhaust retries before one terminal error.
#[tokio::test(flavor = "current_thread")]
async fn compact_v2_stream_failure_without_retry_after_exhausts_stream_retries() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("seed"),
                responses::ev_completed("seed"),
            ])),
            responses::sse_response(responses::sse_failed(
                "rate-limited",
                "rate_limit_exceeded",
                "Rate limit exceeded.",
            )),
            responses::sse_response(responses::sse_failed(
                "still-rate-limited",
                "rate_limit_exceeded",
                "Rate limit exceeded.",
            )),
        ],
    )
    .await;
    let test = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("seed history for compaction").await?;

    test.codex.submit(Op::Compact).await?;
    let retry = telemetry.next_retry().await;
    assert!((FIRST_RETRY_MIN_DELAY..FIRST_RETRY_MAX_DELAY).contains(&retry.delay));
    assert_eq!(
        retry,
        RetryTelemetryEvent {
            attempt: 1,
            delay: retry.delay,
            layer: "stream".into(),
            operation: "remote_compaction_v2".into(),
        }
    );
    wait_for_retry(&mut telemetry, &retry).await;

    let mut error_events = 0;
    let mut stream_error_events = 0;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Error(error) => {
                error_events += 1;
                assert_eq!(
                    error.codex_error_info,
                    Some(CodexErrorInfo::RateLimitExceeded)
                );
                assert!(error.message.contains("Rate limit exceeded."));
            }
            EventMsg::StreamError(_) => stream_error_events += 1,
            EventMsg::TurnComplete(event) => {
                assert_eq!(
                    event.error.and_then(|error| error.codex_error_info),
                    Some(CodexErrorInfo::RateLimitExceeded)
                );
                break;
            }
            _ => {}
        }
    }

    assert_eq!(error_events, 1);
    assert_eq!(stream_error_events, 1);
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert_eq!(request.path(), "/v1/responses");
        assert!(
            !request.inputs_of_type("compaction_trigger").is_empty(),
            "expected a remote compaction v2 request"
        );
    }
    assert_eq!(
        telemetry.events.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );

    Ok(())
}

// TODO(anp) respect Retry-After
/// Remote compaction v2 already honors exact retry advice embedded in rate-limit messages.
#[tokio::test(flavor = "current_thread")]
async fn compact_v2_rate_limit_message_uses_server_advised_retry_delay() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("seed"),
                responses::ev_completed("seed"),
            ])),
            responses::sse_response(responses::sse_failed(
                "rate-limited",
                "rate_limit_exceeded",
                "Rate limit exceeded. Please try again in 1s.",
            ))
            .insert_header("Retry-After", "2"),
            responses::sse_response(responses::sse(vec![
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "RETRIED_COMPACTION_SUMMARY",
                    }
                }),
                responses::ev_completed("compacted"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("seed history for compaction").await?;

    test.codex.submit(Op::Compact).await?;
    let retry = telemetry.next_retry().await;
    assert!(retry.delay <= Duration::from_secs(1));
    assert_eq!(
        retry,
        RetryTelemetryEvent {
            attempt: 1,
            delay: retry.delay,
            layer: "stream".into(),
            operation: "remote_compaction_v2".into(),
        }
    );
    assert!(wait_for_retry(&mut telemetry, &retry).await >= Duration::from_secs(1));
    wait_for_turn_completion(&test).await;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert_eq!(request.path(), "/v1/responses");
        assert!(
            !request.inputs_of_type("compaction_trigger").is_empty(),
            "expected a remote compaction v2 request"
        );
    }
    assert_eq!(
        telemetry.events.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );

    Ok(())
}

/// Remote compaction rate-limit messages provide exact retry advice without an HTTP header.
#[tokio::test(flavor = "current_thread")]
async fn compact_v2_rate_limit_message_without_retry_after_uses_server_advised_delay() -> Result<()>
{
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("seed"),
                responses::ev_completed("seed"),
            ])),
            responses::sse_response(responses::sse_failed(
                "rate-limited",
                "rate_limit_exceeded",
                "Rate limit exceeded. Please try again in 1s.",
            )),
            responses::sse_response(responses::sse(vec![
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "RETRIED_COMPACTION_SUMMARY",
                    }
                }),
                responses::ev_completed("compacted"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("seed history for compaction").await?;

    test.codex.submit(Op::Compact).await?;
    let retry = telemetry.next_retry().await;
    assert!(retry.delay <= Duration::from_secs(1));
    assert_eq!(
        retry,
        RetryTelemetryEvent {
            attempt: 1,
            delay: retry.delay,
            layer: "stream".into(),
            operation: "remote_compaction_v2".into(),
        }
    );
    assert!(wait_for_retry(&mut telemetry, &retry).await >= Duration::from_secs(1));
    wait_for_turn_completion(&test).await;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert_eq!(request.path(), "/v1/responses");
        assert!(
            !request.inputs_of_type("compaction_trigger").is_empty(),
            "expected a remote compaction v2 request"
        );
    }
    assert_eq!(
        telemetry.events.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );

    Ok(())
}

/// Remote compaction V2 retries beyond its old request and stream budgets.
#[tokio::test(flavor = "current_thread")]
async fn compact_v2_overload_recovers_beyond_old_retry_limits() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("seed"),
                responses::ev_completed("seed"),
            ])),
            ResponseTemplate::new(503)
                .set_body_json(json!({ "error": { "code": "server_is_overloaded" } })),
            ResponseTemplate::new(503)
                .set_body_json(json!({ "error": { "code": "server_is_overloaded" } })),
            ResponseTemplate::new(503)
                .set_body_json(json!({ "error": { "code": "server_is_overloaded" } })),
            responses::sse_response(responses::sse(vec![
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "capacity-recovered-summary",
                    }
                }),
                responses::ev_completed("capacity-recovered-compaction"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("seed history for compaction").await?;

    test.codex.submit(Op::Compact).await?;
    let mut stream_error_events = 0;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Error(error) => panic!("remote capacity became terminal: {error:?}"),
            EventMsg::StreamError(_) => stream_error_events += 1,
            EventMsg::TurnComplete(event) => {
                assert_eq!(event.error, None);
                break;
            }
            _ => {}
        }
    }

    assert_eq!(stream_error_events, 3);
    let requests = response_mock.requests();
    assert_eq!(
        requests.len(),
        5,
        "expected a seed request and four remote compaction v2 attempts"
    );
    for request in &requests[1..] {
        assert_eq!(request.path(), "/v1/responses");
        assert!(
            !request.inputs_of_type("compaction_trigger").is_empty(),
            "expected a remote compaction v2 request"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn manual_compact_capacity_wait_is_interruptible() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("manual-cancel-seed"),
                responses::ev_completed("manual-cancel-seed"),
            ])),
            ResponseTemplate::new(503)
                .insert_header("Retry-After", "120")
                .set_body_json(json!({ "error": { "code": "server_is_overloaded" } })),
        ],
    )
    .await;
    let test = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("seed manual compaction").await?;
    test.codex.submit(Op::Compact).await?;
    let progress = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::StreamError(_))
    })
    .await;
    let EventMsg::StreamError(progress) = progress else {
        unreachable!("predicate selected retry progress")
    };
    assert!(progress.message.contains("Model at capacity; retrying"));
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    assert_eq!(response_mock.requests().len(), 2);
    Ok(())
}

// TODO(anp) respect Retry-After
/// SSE failures currently retry with local backoff instead of the enclosing response header.
#[tokio::test(flavor = "current_thread")]
async fn sse_failure_uses_local_backoff_despite_retry_after() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![
                json!({
                    "type": "error",
                    "error": {
                        "type": "tokens",
                        "code": "rate_limit_exceeded",
                        "message": "Rate limit exceeded."
                    }
                }),
                json!({
                    "type": "response.failed",
                    "response": {
                        "id": "rate-limited",
                        "status": "failed",
                        "error": {
                            "code": "rate_limit_exceeded",
                            "message": "Rate limit exceeded."
                        }
                    }
                }),
            ]))
            .insert_header("Retry-After", "1"),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("recovered"),
                responses::ev_completed("recovered"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;

    submit_user_input(&test, "retry the rate-limited stream").await?;
    let retry = telemetry.next_retry().await;
    assert!((FIRST_RETRY_MIN_DELAY..FIRST_RETRY_MAX_DELAY).contains(&retry.delay));
    assert_eq!(
        retry,
        RetryTelemetryEvent {
            attempt: 1,
            delay: retry.delay,
            layer: "stream".into(),
            operation: "sampling".into(),
        }
    );
    wait_for_retry(&mut telemetry, &retry).await;
    wait_for_turn_completion(&test).await;

    assert_eq!(response_mock.requests().len(), 2);
    assert_eq!(
        telemetry.events.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );
    Ok(())
}

/// Headerless sampled stream rate limits exhaust retries before one terminal error.
#[test_case::test_case("rate_limit_exceeded"; "rate_limit")]
#[test_case::test_case("slow_down"; "slow_down")]
#[tokio::test(flavor = "current_thread")]
async fn sse_failure_without_retry_after_exhausts_stream_retries(code: &str) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse_failed(
                "rate-limited",
                code,
                "Rate limit exceeded.",
            )),
            responses::sse_response(responses::sse_failed(
                "still-rate-limited",
                code,
                "Rate limit exceeded.",
            )),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;

    submit_user_input(&test, "exhaust the headerless rate-limited stream").await?;
    let retry = telemetry.next_retry().await;
    assert!(
        (FIRST_RETRY_MIN_DELAY..FIRST_RETRY_MAX_DELAY).contains(&retry.delay),
        "{retry:?}",
    );
    assert_eq!(
        retry,
        RetryTelemetryEvent {
            attempt: 1,
            delay: retry.delay,
            layer: "stream".into(),
            operation: "sampling".into(),
        }
    );
    wait_for_retry(&mut telemetry, &retry).await;

    let mut error_events = 0;
    let mut stream_error_events = 0;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Error(error) => {
                error_events += 1;
                assert_eq!(
                    error.codex_error_info,
                    Some(CodexErrorInfo::RateLimitExceeded)
                );
                assert!(error.message.contains("Rate limit exceeded."));
            }
            EventMsg::StreamError(_) => stream_error_events += 1,
            EventMsg::TurnComplete(event) => {
                assert_eq!(
                    event.error.and_then(|error| error.codex_error_info),
                    Some(CodexErrorInfo::RateLimitExceeded)
                );
                break;
            }
            _ => {}
        }
    }

    assert_eq!(error_events, 1);
    assert_eq!(stream_error_events, 1);
    assert_eq!(response_mock.requests().len(), 2);
    assert_eq!(
        telemetry.events.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );

    Ok(())
}

/// Rate-limit messages already provide an exact retry delay without an HTTP header.
#[test_case::test_case("rate_limit_exceeded"; "rate_limit")]
#[test_case::test_case("slow_down"; "slow_down")]
#[tokio::test(flavor = "current_thread")]
async fn sse_rate_limit_message_uses_server_advised_retry_delay(code: &str) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse_failed(
                "rate-limited",
                code,
                "Rate limit exceeded. Please try again in 1s.",
            )),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("recovered"),
                responses::ev_completed("recovered"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;

    submit_user_input(&test, "retry after the rate-limit message delay").await?;
    let retry = telemetry.next_retry().await;
    assert!(retry.delay <= Duration::from_secs(1));
    assert_eq!(
        retry,
        RetryTelemetryEvent {
            attempt: 1,
            delay: retry.delay,
            layer: "stream".into(),
            operation: "sampling".into(),
        }
    );
    assert!(wait_for_retry(&mut telemetry, &retry).await >= Duration::from_secs(1));
    wait_for_turn_completion(&test).await;

    assert_eq!(response_mock.requests().len(), 2);
    assert_eq!(
        telemetry.events.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );

    Ok(())
}

// TODO(anp) respect Retry-After
/// Rate-limit messages currently override an enclosing response's different retry delay.
#[tokio::test(flavor = "current_thread")]
async fn sse_rate_limit_message_with_retry_after_uses_server_advised_retry_delay() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse_failed(
                "rate-limited",
                "rate_limit_exceeded",
                "Rate limit exceeded. Please try again in 1s.",
            ))
            .insert_header("Retry-After", "2"),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("recovered"),
                responses::ev_completed("recovered"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;

    submit_user_input(&test, "retry after both rate-limit delay signals").await?;
    let retry = telemetry.next_retry().await;
    assert!(retry.delay <= Duration::from_secs(1));
    assert_eq!(
        retry,
        RetryTelemetryEvent {
            attempt: 1,
            delay: retry.delay,
            layer: "stream".into(),
            operation: "sampling".into(),
        }
    );
    assert!(wait_for_retry(&mut telemetry, &retry).await >= Duration::from_secs(1));
    wait_for_turn_completion(&test).await;

    assert_eq!(response_mock.requests().len(), 2);
    assert_eq!(
        telemetry.events.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );

    Ok(())
}

/// A typed Flex denial stays terminal even if an enclosing response suggests retry.
#[tokio::test(flavor = "current_thread")]
async fn sse_flex_denial_with_retry_after_is_terminal() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_once(
        &server,
        responses::sse_response(responses::sse_failed(
            "flex-denial",
            "flex_unavailable",
            "Flex capacity unavailable.",
        ))
        .insert_header("Retry-After", "1"),
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(2);
            config.model_provider.stream_max_retries = Some(2);
        })
        .build_with_auto_env(&server)
        .await?;

    submit_user_input(&test, "reject the Flex denial despite retry advice").await?;

    let mut error_events = 0;
    let mut stream_error_events = 0;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Error(error) => {
                error_events += 1;
                assert_eq!(
                    error.codex_error_info,
                    Some(CodexErrorInfo::FlexUnavailable)
                );
                assert_eq!(error.message, "Flex capacity unavailable.");
            }
            EventMsg::StreamError(_) => stream_error_events += 1,
            EventMsg::TurnComplete(event) => {
                assert_eq!(
                    event.error.and_then(|error| error.codex_error_info),
                    Some(CodexErrorInfo::FlexUnavailable)
                );
                break;
            }
            _ => {}
        }
    }

    assert_eq!(error_events, 1);
    assert_eq!(stream_error_events, 0);
    assert_eq!(response_mock.requests().len(), 1);
    let request_count = server
        .received_requests()
        .await
        .expect("mock server should record requests")
        .into_iter()
        .filter(|request| request.url.path() == "/v1/responses")
        .count();
    assert_eq!(request_count, 1, "typed Flex denial must not retry");
    assert_eq!(
        telemetry.events.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );

    Ok(())
}

/// An unadvised streamed overload also remains inside the original turn.
#[tokio::test(flavor = "current_thread")]
async fn sse_overload_without_retry_after_recovers() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse_failed(
                "capacity-one",
                "server_is_overloaded",
                "Selected model is at capacity.",
            )),
            responses::sse_response(responses::sse_failed(
                "capacity-two",
                "server_is_overloaded",
                "Selected model is at capacity.",
            )),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("streamed-capacity-recovered"),
                responses::ev_completed("streamed-capacity-recovered"),
            ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;

    submit_user_input(&test, "recover the streamed overload").await?;

    let mut stream_error_events = 0;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Error(error) => panic!("streamed capacity became terminal: {error:?}"),
            EventMsg::StreamError(_) => stream_error_events += 1,
            EventMsg::TurnComplete(event) => {
                assert_eq!(event.error, None);
                break;
            }
            _ => {}
        }
    }

    assert_eq!(stream_error_events, 2);
    assert_eq!(response_mock.requests().len(), 3);
    let request_count = server
        .received_requests()
        .await
        .expect("mock server should record requests")
        .into_iter()
        .filter(|request| request.url.path() == "/v1/responses")
        .count();
    assert_eq!(request_count, 3, "unadvised SSE overload must recover");

    Ok(())
}

async fn assert_terminal_failure(
    test: &TestCodex,
    expected: CodexErrorInfo,
    expected_retries: usize,
) -> Result<()> {
    let mut errors = Vec::new();
    let mut completions = Vec::new();
    let mut retries = 0;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Error(error) => errors.push(error.codex_error_info),
            EventMsg::StreamError(_) => retries += 1,
            EventMsg::TurnComplete(event) => {
                completions.push(event.error.and_then(|error| error.codex_error_info));
                test.codex.submit(Op::Shutdown).await?;
            }
            EventMsg::ShutdownComplete => break,
            _ => {}
        }
    }
    assert_eq!(errors, vec![Some(expected.clone())]);
    assert_eq!(completions, vec![Some(expected)]);
    assert_eq!(retries, expected_retries);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn http_and_sse_flex_unavailable_are_terminal() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let error = json!({
        "type": "resource_unavailable", "code": "flex_unavailable",
        "message": "Flex capacity unavailable."
    });
    for response in [
        ResponseTemplate::new(429)
            .insert_header("Retry-After", "300")
            .set_body_json(json!({"error": error})),
        responses::sse_response(responses::sse(vec![
            json!({"type": "error", "error": error}),
        ])),
        responses::sse_response(responses::sse_failed(
            "failed",
            "flex_unavailable",
            "No capacity",
        )),
    ] {
        let server = responses::start_mock_server().await;
        let response_mock = responses::mount_response_once(&server, response).await;
        let test = test_codex()
            .with_config(|config| {
                config.service_tier = Some("flex".to_string());
                config.model_provider.request_max_retries = Some(2);
                config.model_provider.stream_max_retries = Some(2);
            })
            .build_with_auto_env(&server)
            .await?;

        submit_user_input(&test, "surface the Flex capacity failure").await?;
        assert_terminal_failure(
            &test,
            CodexErrorInfo::FlexUnavailable,
            /*expected_retries*/ 0,
        )
        .await?;
        assert_eq!(
            response_mock.single_request().body_json()["service_tier"],
            json!("flex")
        );
        assert_eq!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| request.url.path() == "/v1/responses")
                .count(),
            1
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn websocket_streamed_flex_unavailable_is_terminal() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let error = json!({
        "type": "resource_unavailable", "code": "flex_unavailable",
        "message": "Flex capacity unavailable."
    });
    for event in [
        json!({"type": "error", "status": 429, "headers": {"retry-after": "300"}, "error": error}),
        json!({"type": "response.failed", "response": {"error": error}}),
    ] {
        let server = responses::start_websocket_server(vec![vec![
            vec![
                responses::ev_response_created("prewarm"),
                responses::ev_completed("prewarm"),
            ],
            vec![event],
        ]])
        .await;
        let test = test_codex()
            .with_config(|config| {
                config.service_tier = Some("flex".to_string());
                config.model_catalog =
                    Some(bundled_models_response().expect("bundled models.json should parse"));
                config.model_provider.request_max_retries = Some(2);
                config.model_provider.stream_max_retries = Some(2);
            })
            .build_with_websocket_server(&server)
            .await?;

        submit_user_input(&test, "surface the Flex capacity failure").await?;
        assert_terminal_failure(
            &test,
            CodexErrorInfo::FlexUnavailable,
            /*expected_retries*/ 0,
        )
        .await?;
        let requests = server.single_connection();
        assert_eq!(requests.len(), 2, "prewarm and failed turn");
        assert_eq!(requests[1].body_json()["service_tier"], json!("flex"));
        server.shutdown().await;
    }
    Ok(())
}

/// Network reconnects keep their own attempt count without consuming stream retry budget.
#[tokio::test(flavor = "current_thread")]
async fn connection_failures_increment_retry_telemetry_without_consuming_retry_budget() -> Result<()>
{
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let bootstrap_server = responses::start_mock_server().await;
    let unavailable_socket = TcpSocket::new_v4()?;
    unavailable_socket.bind("127.0.0.1:0".parse()?)?;
    let unavailable_address = unavailable_socket.local_addr()?;

    let test = test_codex()
        .with_config(move |config| {
            config.model_provider.base_url = Some(format!("http://{unavailable_address}/v1"));
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build_with_auto_env(&bootstrap_server)
        .await?;

    submit_user_input(&test, "recover after repeated network failures").await?;

    let first_retry = telemetry.next_retry().await;
    assert_eq!(
        first_retry,
        RetryTelemetryEvent {
            attempt: 1,
            delay: Duration::from_secs(5),
            layer: "stream".into(),
            operation: "sampling".into(),
        }
    );
    wait_for_retry(&mut telemetry, &first_retry).await;

    let second_retry = telemetry.next_retry().await;
    assert_eq!(
        second_retry,
        RetryTelemetryEvent {
            attempt: 2,
            delay: Duration::from_secs(10),
            layer: "stream".into(),
            operation: "sampling".into(),
        }
    );

    let recovered_server = MockServer::builder()
        .listener(unavailable_socket.listen(/*backlog*/ 128)?.into_std()?)
        .start()
        .await;
    let response_mock = responses::mount_sse_once(
        &recovered_server,
        responses::sse(vec![
            responses::ev_response_created("recovered"),
            responses::ev_completed("recovered"),
        ]),
    )
    .await;

    wait_for_retry(&mut telemetry, &second_retry).await;
    wait_for_turn_completion(&test).await;

    assert_eq!(response_mock.requests().len(), 1);
    assert_eq!(
        telemetry.events.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );

    Ok(())
}

/// Retryable websocket errors reconnect after the delay reported by retry telemetry.
#[tokio::test(flavor = "current_thread")]
async fn websocket_connection_limit_retries_with_local_backoff() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_websocket_server(vec![
        vec![
            vec![
                responses::ev_response_created("prewarm"),
                responses::ev_completed("prewarm"),
            ],
            vec![json!({
                "type": "error",
                "status": 400,
                "error": {
                    "type": "invalid_request_error",
                    "code": "websocket_connection_limit_reached",
                    "message": "Responses websocket connection limit reached (60 minutes). Create a new websocket connection to continue."
                }
            })],
        ],
        vec![vec![
            responses::ev_response_created("recovered"),
            responses::ev_completed("recovered"),
        ]],
    ])
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_websocket_server(&server)
        .await?;

    let warmup = server
        .wait_for_request(/*connection_index*/ 0, /*request_index*/ 0)
        .await;
    assert_eq!(warmup.body_json()["generate"].as_bool(), Some(false));
    submit_user_input(&test, "retry after reaching the websocket connection limit").await?;
    let retry = telemetry.next_retry().await;
    assert!((FIRST_RETRY_MIN_DELAY..FIRST_RETRY_MAX_DELAY).contains(&retry.delay));
    assert_eq!(
        retry,
        RetryTelemetryEvent {
            attempt: 1,
            delay: retry.delay,
            layer: "stream".into(),
            operation: "sampling".into(),
        }
    );
    wait_for_retry(&mut telemetry, &retry).await;
    wait_for_turn_completion(&test).await;

    let connections = server.connections();
    assert_eq!(connections.len(), 2);
    let request_count: usize = connections.iter().map(Vec::len).sum();
    assert_eq!(request_count, 3);
    assert_eq!(
        telemetry.events.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );
    server.shutdown().await;

    Ok(())
}

/// Advised WebSocket failures reach HTTP after the existing retry budget. HTTP
/// recovery uses its own retry budget and honors the preceding advice deadlines.
#[tokio::test(flavor = "current_thread")]
async fn websocket_upgrade_rejection_uses_retry_after() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let prewarm_rejection = Mock::given(method("GET"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(/*s*/ 503))
        .up_to_n_times(/*n*/ 1)
        .expect(/*r*/ 1)
        .mount_as_scoped(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(/*s*/ 504)
                .insert_header("Retry-After", "1")
                .set_body_json(json!({ "error": { "code": "server_error" } })),
        )
        .mount(&server)
        .await;
    let http_overload = ResponseTemplate::new(/*s*/ 503)
        .insert_header("Retry-After", "0")
        .set_body_json(json!({ "error": { "code": "server_is_overloaded" } }));
    let response_mock = responses::mount_response_sequence(
        &server,
        vec![
            http_overload,
            ResponseTemplate::new(/*s*/ 200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(responses::sse(vec![
                    responses::ev_response_created("recovered"),
                    responses::ev_completed("recovered"),
                ])),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_catalog = Some(bundled_models_response().expect("bundled models"));
            config.model_provider.supports_websockets = true;
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_auto_env(&server)
        .await?;

    // Prewarm is best effort; start the advised failures in the sampling retry loop.
    tokio::time::timeout(
        Duration::from_secs(10),
        prewarm_rejection.wait_until_satisfied(),
    )
    .await?;
    drop(prewarm_rejection);
    let start = Instant::now();
    submit_user_input(&test, "retry the rejected websocket upgrade").await?;
    wait_for_turn_completion(&test).await;

    // Both WebSocket advice deadlines apply, including the one before transport fallback.
    assert!(start.elapsed() >= Duration::from_secs(2));
    let requests = server.received_requests().await.unwrap_or_default();
    let methods = requests
        .iter()
        .filter(|request| request.url.path() == "/v1/responses")
        .map(|request| request.method.as_str())
        .collect::<Vec<_>>();
    assert_eq!(methods, vec!["GET", "GET", "GET", "POST", "POST"]);
    assert_eq!(response_mock.requests().len(), 2);
    Ok(())
}

// TODO(anp) respect Retry-After
/// Nested websocket retry headers are currently ignored, leaving rate-limit errors terminal.
#[tokio::test(flavor = "current_thread")]
async fn websocket_rate_limit_with_nested_retry_after_is_terminal() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_websocket_server(vec![vec![
        vec![
            responses::ev_response_created("prewarm"),
            responses::ev_completed("prewarm"),
        ],
        vec![json!({
            "type": "error",
            "status": 429,
            "error": {
                "type": "rate_limit_error",
                "code": "rate_limit_exceeded",
                "message": "Rate limit exceeded.",
                "headers": { "Retry-After": "1" }
            }
        })],
    ]])
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_websocket_server(&server)
        .await?;

    let warmup = server
        .wait_for_request(/*connection_index*/ 0, /*request_index*/ 0)
        .await;
    assert_eq!(warmup.body_json()["generate"].as_bool(), Some(false));
    submit_user_input(&test, "surface the websocket rate limit").await?;

    let mut error_events = 0;
    let mut stream_error_events = 0;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Error(error) => {
                error_events += 1;
                assert_eq!(
                    error.codex_error_info,
                    Some(CodexErrorInfo::ResponseTooManyFailedAttempts {
                        http_status_code: Some(429),
                    })
                );
            }
            EventMsg::StreamError(_) => stream_error_events += 1,
            EventMsg::TurnComplete(event) => {
                assert_eq!(
                    event.error.and_then(|error| error.codex_error_info),
                    Some(CodexErrorInfo::ResponseTooManyFailedAttempts {
                        http_status_code: Some(429),
                    })
                );
                break;
            }
            _ => {}
        }
    }

    assert_eq!(error_events, 1);
    assert_eq!(stream_error_events, 0);
    let request_count: usize = server.connections().iter().map(Vec::len).sum();
    assert_eq!(request_count, 2, "expected only prewarm and terminal error");
    assert_eq!(
        telemetry.events.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );
    server.shutdown().await;

    Ok(())
}

/// Headerless websocket rate limits complete with the same terminal error as nested headers.
#[tokio::test(flavor = "current_thread")]
async fn websocket_rate_limit_without_retry_after_is_terminal() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_websocket_server(vec![vec![
        vec![
            responses::ev_response_created("prewarm"),
            responses::ev_completed("prewarm"),
        ],
        vec![json!({
            "type": "error",
            "status": 429,
            "error": {
                "type": "rate_limit_error",
                "code": "rate_limit_exceeded",
                "message": "Rate limit exceeded."
            }
        })],
    ]])
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build_with_websocket_server(&server)
        .await?;

    let warmup = server
        .wait_for_request(/*connection_index*/ 0, /*request_index*/ 0)
        .await;
    assert_eq!(warmup.body_json()["generate"].as_bool(), Some(false));
    submit_user_input(&test, "surface the headerless websocket rate limit").await?;

    let mut error_events = 0;
    let mut stream_error_events = 0;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Error(error) => {
                error_events += 1;
                assert_eq!(
                    error.codex_error_info,
                    Some(CodexErrorInfo::ResponseTooManyFailedAttempts {
                        http_status_code: Some(429),
                    })
                );
            }
            EventMsg::StreamError(_) => stream_error_events += 1,
            EventMsg::TurnComplete(event) => {
                assert_eq!(
                    event.error.and_then(|error| error.codex_error_info),
                    Some(CodexErrorInfo::ResponseTooManyFailedAttempts {
                        http_status_code: Some(429),
                    })
                );
                break;
            }
            _ => {}
        }
    }

    assert_eq!(error_events, 1);
    assert_eq!(stream_error_events, 0);
    let request_count: usize = server.connections().iter().map(Vec::len).sum();
    assert_eq!(request_count, 2, "expected only prewarm and terminal error");
    assert_eq!(
        telemetry.events.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );
    server.shutdown().await;

    Ok(())
}

// TODO(anp) respect Retry-After
/// Typed WebSocket Flex denials remain terminal despite a nested retry header.
#[tokio::test(flavor = "current_thread")]
async fn websocket_flex_denial_with_nested_retry_after_is_terminal() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut telemetry = RetryTelemetryCapture::install();
    let server = responses::start_websocket_server(vec![vec![
        vec![
            responses::ev_response_created("prewarm"),
            responses::ev_completed("prewarm"),
        ],
        vec![json!({
            "type": "error",
            "status": 503,
            "error": {
                "code": "flex_unavailable",
                "message": "Flex capacity unavailable.",
                "headers": { "Retry-After": "1" }
            }
        })],
    ]])
    .await;
    let test = test_codex()
        .with_config(|config| {
            // Capture inference retries without unrelated startup model-discovery retries.
            config.model_catalog =
                Some(bundled_models_response().expect("bundled models.json should parse"));
            config.model_provider.request_max_retries = Some(2);
            config.model_provider.stream_max_retries = Some(2);
        })
        .build_with_websocket_server(&server)
        .await?;

    let warmup = server
        .wait_for_request(/*connection_index*/ 0, /*request_index*/ 0)
        .await;
    assert_eq!(warmup.body_json()["generate"].as_bool(), Some(false));
    submit_user_input(
        &test,
        "reject the websocket Flex denial despite retry advice",
    )
    .await?;

    let mut error_events = 0;
    let mut stream_error_events = 0;
    let mut fallback_warning_events = 0;
    loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Error(error) => {
                error_events += 1;
                assert_eq!(
                    error.codex_error_info,
                    Some(CodexErrorInfo::FlexUnavailable)
                );
                assert_eq!(error.message, "Flex capacity unavailable.");
            }
            EventMsg::StreamError(_) => stream_error_events += 1,
            EventMsg::Warning(warning)
                if warning.message.contains("Falling back from WebSockets") =>
            {
                fallback_warning_events += 1;
            }
            EventMsg::TurnComplete(event) => {
                assert_eq!(
                    event.error.and_then(|error| error.codex_error_info),
                    Some(CodexErrorInfo::FlexUnavailable)
                );
                break;
            }
            _ => {}
        }
    }

    assert_eq!(error_events, 1);
    assert_eq!(stream_error_events, 0);
    assert_eq!(
        fallback_warning_events, 0,
        "websocket must not fall back to HTTP"
    );
    let request_count: usize = server.connections().iter().map(Vec::len).sum();
    assert_eq!(request_count, 2, "expected only prewarm and terminal error");
    assert_eq!(
        telemetry.events.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    );
    server.shutdown().await;

    Ok(())
}

/// A capacity wait remains interruptible without switching transport.
#[tokio::test(flavor = "current_thread")]
async fn websocket_capacity_wait_interrupts_without_fallback() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_websocket_server(vec![vec![
        vec![
            responses::ev_response_created("prewarm"),
            responses::ev_completed("prewarm"),
        ],
        vec![json!({
            "type": "error",
            "status": 503,
            "error": {
                "code": "server_is_overloaded",
                "message": "Selected model is at capacity.",
                "headers": { "Retry-After": "120" }
            }
        })],
    ]])
    .await;
    let test = test_codex()
        .with_config(|config| {
            // Capture inference retries without unrelated startup model-discovery retries.
            config.model_catalog =
                Some(bundled_models_response().expect("bundled models.json should parse"));
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        })
        .build_with_websocket_server(&server)
        .await?;

    let warmup = server
        .wait_for_request(/*connection_index*/ 0, /*request_index*/ 0)
        .await;
    assert_eq!(warmup.body_json()["generate"].as_bool(), Some(false));
    submit_user_input(&test, "wait for the websocket capacity to recover").await?;
    let event = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::StreamError(_))
    })
    .await;
    let EventMsg::StreamError(progress) = event else {
        unreachable!("predicate requires recovery progress");
    };
    assert!(progress.message.contains("Model at capacity; retrying"));
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    let request_count: usize = server.connections().iter().map(Vec::len).sum();
    assert_eq!(
        request_count, 2,
        "capacity wait must not reconnect or fall back"
    );
    server.shutdown().await;

    Ok(())
}
