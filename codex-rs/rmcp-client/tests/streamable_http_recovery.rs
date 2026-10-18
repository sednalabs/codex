mod streamable_http_test_support;

#[path = "streamable_http_recovery/telemetry_tests.rs"]
mod telemetry_tests;

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_exec_server::Environment;
use codex_exec_server::ExecServerError;
use codex_exec_server::HttpClient;
use codex_exec_server::HttpRequestParams;
use codex_exec_server::HttpRequestResponse;
use codex_exec_server::HttpResponseBodyStream;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use pretty_assertions::assert_eq;
use rmcp::model::CallToolResult;
use rmcp::model::ContentBlock;
use rmcp::model::MetaObject;
use serde_json::Value;
use serde_json::json;
use tokio::sync::Notify;

use streamable_http_test_support::arm_initialize_post_failure;
use streamable_http_test_support::arm_initialize_post_json_rpc_failure;
use streamable_http_test_support::arm_initialized_notification_post_json_rpc_failure;
use streamable_http_test_support::arm_session_post_failure;
use streamable_http_test_support::arm_session_post_failure_after_dispatch;
use streamable_http_test_support::arm_session_post_failure_with_retry_after;
use streamable_http_test_support::arm_session_post_json_rpc_failure;
use streamable_http_test_support::call_echo_tool;
use streamable_http_test_support::create_client;
use streamable_http_test_support::create_client_with_http_client;
use streamable_http_test_support::expected_echo_result;
use streamable_http_test_support::read_side_effect_count;
use streamable_http_test_support::spawn_streamable_http_server;

const JSON_RPC_INTERNAL_ERROR_CODE: i64 = -32603;
const SIMULATED_NO_RESPONSE_MESSAGE: &str =
    "http/request failed: error sending request for url (simulated no response)";

#[derive(Clone)]
struct FailFirstInitializeHttpClient {
    inner: Arc<dyn HttpClient>,
    failures_remaining: Arc<AtomicUsize>,
    initialize_attempts: Arc<AtomicUsize>,
    tool_call_attempts: Arc<AtomicUsize>,
}

impl FailFirstInitializeHttpClient {
    fn new(inner: Arc<dyn HttpClient>, failures_remaining: usize) -> Self {
        Self {
            inner,
            failures_remaining: Arc::new(AtomicUsize::new(failures_remaining)),
            initialize_attempts: Arc::new(AtomicUsize::new(0)),
            tool_call_attempts: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn initialize_attempts(&self) -> usize {
        self.initialize_attempts.load(Ordering::SeqCst)
    }

    fn tool_call_attempts(&self) -> usize {
        self.tool_call_attempts.load(Ordering::SeqCst)
    }

    fn fail_next_initialize(&self) {
        self.failures_remaining.store(1, Ordering::SeqCst);
    }
}

impl HttpClient for FailFirstInitializeHttpClient {
    fn http_request(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
        self.inner.http_request(params)
    }

    fn http_request_stream(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>> {
        let inner = Arc::clone(&self.inner);
        let failures_remaining = Arc::clone(&self.failures_remaining);
        let initialize_attempts = Arc::clone(&self.initialize_attempts);
        let tool_call_attempts = Arc::clone(&self.tool_call_attempts);

        async move {
            if is_initialize_post(&params) {
                initialize_attempts.fetch_add(1, Ordering::SeqCst);
                if failures_remaining.swap(0, Ordering::SeqCst) > 0 {
                    return Err(ExecServerError::Server {
                        code: JSON_RPC_INTERNAL_ERROR_CODE,
                        message: SIMULATED_NO_RESPONSE_MESSAGE.to_string(),
                    });
                }
            }
            if is_tools_call_post(&params) {
                tool_call_attempts.fetch_add(1, Ordering::SeqCst);
            }

            inner.http_request_stream(params).await
        }
        .boxed()
    }
}

#[derive(Clone)]
struct DelayedToolCallHttpClient {
    inner: Arc<dyn HttpClient>,
    initialize_attempts: Arc<AtomicUsize>,
    tool_call_attempts: Arc<AtomicUsize>,
    delayed_message: String,
    request_captured: Arc<Notify>,
    release_request: Arc<Notify>,
}

impl DelayedToolCallHttpClient {
    fn new(inner: Arc<dyn HttpClient>, delayed_message: &str) -> Self {
        Self {
            inner,
            initialize_attempts: Arc::new(AtomicUsize::new(0)),
            tool_call_attempts: Arc::new(AtomicUsize::new(0)),
            delayed_message: delayed_message.to_string(),
            request_captured: Arc::new(Notify::new()),
            release_request: Arc::new(Notify::new()),
        }
    }

    fn initialize_attempts(&self) -> usize {
        self.initialize_attempts.load(Ordering::SeqCst)
    }

    fn tool_call_attempts(&self) -> usize {
        self.tool_call_attempts.load(Ordering::SeqCst)
    }
}

impl HttpClient for DelayedToolCallHttpClient {
    fn http_request(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
        self.inner.http_request(params)
    }

    fn http_request_stream(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>> {
        let inner = Arc::clone(&self.inner);
        let initialize_attempts = Arc::clone(&self.initialize_attempts);
        let tool_call_attempts = Arc::clone(&self.tool_call_attempts);
        let delayed_message = self.delayed_message.clone();
        let request_captured = Arc::clone(&self.request_captured);
        let release_request = Arc::clone(&self.release_request);

        async move {
            if is_initialize_post(&params) {
                initialize_attempts.fetch_add(1, Ordering::SeqCst);
            }
            let tool_name = request_tool_name(&params);
            let message = request_tool_message(&params);
            if tool_name.is_some() {
                tool_call_attempts.fetch_add(1, Ordering::SeqCst);
            }

            if tool_name.as_deref() == Some("echo")
                && message.as_deref() == Some(delayed_message.as_str())
            {
                request_captured.notify_one();
                release_request.notified().await;
            }

            inner.http_request_stream(params).await
        }
        .boxed()
    }
}

fn is_initialize_post(params: &HttpRequestParams) -> bool {
    params.method.eq_ignore_ascii_case("POST")
        && params
            .body
            .as_ref()
            .and_then(|body| serde_json::from_slice::<Value>(&body.0).ok())
            .and_then(|body| {
                body.get("method")
                    .and_then(Value::as_str)
                    .map(|method| method == "initialize")
            })
            .unwrap_or(false)
}

fn is_tools_call_post(params: &HttpRequestParams) -> bool {
    request_tool_name(params).is_some()
}

fn request_tool_name(params: &HttpRequestParams) -> Option<String> {
    if !params.method.eq_ignore_ascii_case("POST") {
        return None;
    }
    params
        .body
        .as_ref()
        .and_then(|body| serde_json::from_slice::<Value>(&body.0).ok())
        .and_then(|body| {
            if body.get("method").and_then(Value::as_str) != Some("tools/call") {
                return None;
            }
            body.pointer("/params/name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn request_tool_message(params: &HttpRequestParams) -> Option<String> {
    if !params.method.eq_ignore_ascii_case("POST") {
        return None;
    }
    params
        .body
        .as_ref()
        .and_then(|body| serde_json::from_slice::<Value>(&body.0).ok())
        .and_then(|body| {
            body.pointer("/params/arguments/message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_initialize_retries_remote_no_response_error() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let http_client = FailFirstInitializeHttpClient::new(
        Environment::default_for_tests().get_http_client(),
        /*failures_remaining*/ 1,
    );

    let client = create_client_with_http_client(&base_url, Arc::new(http_client.clone())).await?;
    let result = call_echo_tool(&client, "after-init-retry").await?;

    assert_eq!(http_client.initialize_attempts(), 2);
    assert_eq!(result, expected_echo_result("after-init-retry"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_initialize_retries_transient_http_status() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;

    arm_initialize_post_failure(&base_url, /*status*/ 502, /*remaining*/ 1).await?;

    let client = create_client(&base_url).await?;
    let result = call_echo_tool(&client, "after-status-retry").await?;

    assert_eq!(result, expected_echo_result("after-status-retry"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_initialize_retries_json_rpc_transient_status() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;

    arm_initialize_post_json_rpc_failure(&base_url, /*status*/ 502, /*remaining*/ 1).await?;

    let client = create_client(&base_url).await?;
    let result = call_echo_tool(&client, "after-json-status-retry").await?;

    assert_eq!(result, expected_echo_result("after-json-status-retry"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_retries_initialized_notification_status() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;

    arm_initialized_notification_post_json_rpc_failure(
        &base_url, /*status*/ 502, /*remaining*/ 1,
    )
    .await?;

    let client = create_client(&base_url).await?;
    let result = call_echo_tool(&client, "after-notification-status-retry").await?;

    assert_eq!(
        result,
        expected_echo_result("after-notification-status-retry")
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_tools_list_retries_transient_http_status() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;

    let expected = client
        .list_tools(
            /*params*/ None,
            /*timeout*/ Some(Duration::from_secs(5)),
        )
        .await?;
    arm_session_post_failure(
        &base_url,
        /*status*/ 502,
        /*remaining*/ 1,
        /*www_authenticate_headers*/ &[],
    )
    .await?;

    let result = client
        .list_tools(
            /*params*/ None,
            /*timeout*/ Some(Duration::from_secs(5)),
        )
        .await?;

    assert_eq!(result, expected);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_tools_list_honors_retry_after_and_restores_catalogue() -> anyhow::Result<()>
{
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;

    let expected = client
        .list_tools(
            /*params*/ None,
            /*timeout*/ Some(Duration::from_secs(5)),
        )
        .await?;
    let retry_after =
        httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(2));
    arm_session_post_failure_with_retry_after(
        &base_url,
        /*status*/ 429,
        /*remaining*/ 1,
        /*www_authenticate_headers*/ &[],
        Some(&retry_after),
    )
    .await?;

    let retry_started = std::time::Instant::now();
    let refreshed = client
        .list_tools(
            /*params*/ None,
            /*timeout*/ Some(Duration::from_secs(5)),
        )
        .await?;

    assert!(
        retry_started.elapsed() >= Duration::from_millis(900),
        "Retry-After should delay the idempotent catalogue retry"
    );
    assert_eq!(refreshed, expected);
    assert_eq!(
        call_echo_tool(&client, "after-catalogue-recovery").await?,
        expected_echo_result("after-catalogue-recovery")
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_tool_call_does_not_retry_rate_limit_response() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;

    arm_session_post_failure_with_retry_after(
        &base_url,
        /*status*/ 429,
        /*remaining*/ 2,
        /*www_authenticate_headers*/ &[],
        Some("1"),
    )
    .await?;

    let error = call_echo_tool(&client, "rate-limited").await.unwrap_err();
    assert!(error.to_string().contains("429"));
    let second_error = call_echo_tool(&client, "still-rate-limited")
        .await
        .unwrap_err();
    assert!(second_error.to_string().contains("429"));
    assert_eq!(
        call_echo_tool(&client, "after-rate-limit").await?,
        expected_echo_result("after-rate-limit")
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_tools_list_retries_json_rpc_transient_status() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;

    let expected = client
        .list_tools(
            /*params*/ None,
            /*timeout*/ Some(Duration::from_secs(5)),
        )
        .await?;
    arm_session_post_json_rpc_failure(&base_url, /*status*/ 502, /*remaining*/ 1).await?;

    let result = client
        .list_tools(
            /*params*/ None,
            /*timeout*/ Some(Duration::from_secs(5)),
        )
        .await?;

    assert_eq!(result, expected);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_404_tool_call_is_uncertain_and_next_call_recovers() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let http_client = FailFirstInitializeHttpClient::new(
        Environment::default_for_tests().get_http_client(),
        /*failures_remaining*/ 0,
    );
    let client = create_client_with_http_client(&base_url, Arc::new(http_client.clone())).await?;

    let warmup = call_echo_tool(&client, "warmup").await?;
    assert_eq!(warmup, expected_echo_result("warmup"));
    assert_eq!(http_client.initialize_attempts(), 1);

    arm_session_post_failure_after_dispatch(&base_url, /*status*/ 404, /*remaining*/ 1).await?;

    let uncertain = client
        .call_tool(
            "counter_write".to_string(),
            Some(serde_json::json!({})),
            /*meta*/ None,
            /*timeout*/ Some(Duration::from_secs(5)),
        )
        .await
        .unwrap_err();
    let uncertain_message = uncertain.to_string();
    assert!(uncertain_message.contains("MCP session expired during tools/call"));
    assert!(uncertain_message.contains("this invocation was not retried"));
    assert!(uncertain_message.contains("result is unknown"));
    assert!(uncertain_message.contains("separate independent call"));
    assert!(uncertain_message.contains("read-only"));
    assert_eq!(read_side_effect_count(&base_url).await?, 1);
    assert_eq!(http_client.initialize_attempts(), 1);
    assert_eq!(http_client.tool_call_attempts(), 2);

    let recovered = call_echo_tool(&client, "distinct-follow-up").await?;
    assert_eq!(recovered, expected_echo_result("distinct-follow-up"));
    assert_eq!(http_client.initialize_attempts(), 2);
    assert_eq!(http_client.tool_call_attempts(), 3);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_404_recovers_safe_resource_reads() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;
    let timeout = Some(Duration::from_secs(5));

    let expected_resources = client.list_resources(/*params*/ None, timeout).await?;
    arm_session_post_failure(
        &base_url,
        /*status*/ 404,
        /*remaining*/ 1,
        /*www_authenticate_headers*/ &[],
    )
    .await?;
    let recovered_resources = client.list_resources(/*params*/ None, timeout).await?;
    assert_eq!(recovered_resources, expected_resources);

    let read_params = rmcp::model::ReadResourceRequestParams::new("memo://codex/example-note");
    let expected_resource = client.read_resource(read_params.clone(), timeout).await?;
    arm_session_post_failure(
        &base_url,
        /*status*/ 404,
        /*remaining*/ 1,
        /*www_authenticate_headers*/ &[],
    )
    .await?;
    let recovered_resource = client.read_resource(read_params, timeout).await?;
    assert_eq!(recovered_resource, expected_resource);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn late_expired_session_failure_does_not_retire_replacement() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let http_client =
        DelayedToolCallHttpClient::new(Environment::default_for_tests().get_http_client(), "late");
    let client =
        Arc::new(create_client_with_http_client(&base_url, Arc::new(http_client.clone())).await?);

    assert_eq!(
        call_echo_tool(&client, "warmup").await?,
        expected_echo_result("warmup")
    );
    let initial_initialize_attempts = http_client.initialize_attempts();
    assert!(initial_initialize_attempts >= 1);

    // Pause after the old service generation has produced the request but
    // before transport dispatch. The test controls which generation receives
    // each 404 instead of relying on network timing to order the failures.
    let operation_timeout = Duration::from_secs(30);
    let late_client = Arc::clone(&client);
    let late_call = tokio::spawn(async move {
        late_client
            .call_tool(
                "echo".to_string(),
                Some(serde_json::json!({ "message": "late" })),
                /*meta*/ None,
                Some(operation_timeout),
            )
            .await
    });
    tokio::time::timeout(
        Duration::from_secs(5),
        http_client.request_captured.notified(),
    )
    .await?;
    assert_eq!(http_client.tool_call_attempts(), 2);

    arm_session_post_failure(
        &base_url,
        /*status*/ 404,
        /*remaining*/ 1,
        /*www_authenticate_headers*/ &[],
    )
    .await?;
    let first_expiry = call_echo_tool(&client, "expire-current-session")
        .await
        .unwrap_err();
    assert!(
        first_expiry
            .to_string()
            .contains("MCP session expired during tools/call")
    );
    assert_eq!(
        http_client.initialize_attempts(),
        initial_initialize_attempts
    );

    let replacement_result = call_echo_tool(&client, "replacement-session").await?;
    assert_eq!(
        replacement_result,
        expected_echo_result("replacement-session")
    );
    assert_eq!(
        http_client.initialize_attempts(),
        initial_initialize_attempts + 1
    );

    arm_session_post_failure(
        &base_url,
        /*status*/ 404,
        /*remaining*/ 1,
        /*www_authenticate_headers*/ &[],
    )
    .await?;
    http_client.release_request.notify_one();
    let late_error = late_call.await?.unwrap_err();
    assert!(
        late_error
            .to_string()
            .contains("MCP session expired during tools/call")
    );

    let after_late_failure = call_echo_tool(&client, "replacement-remains-current").await?;
    assert_eq!(
        after_late_failure,
        expected_echo_result("replacement-remains-current")
    );
    assert_eq!(
        http_client.initialize_attempts(),
        initial_initialize_attempts + 1
    );
    assert_eq!(http_client.tool_call_attempts(), 5);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_session_recovery_retries_initialize_failure() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let http_client = FailFirstInitializeHttpClient::new(
        Environment::default_for_tests().get_http_client(),
        /*failures_remaining*/ 0,
    );
    let client = create_client_with_http_client(&base_url, Arc::new(http_client.clone())).await?;

    let warmup = call_echo_tool(&client, "warmup").await?;
    assert_eq!(warmup, expected_echo_result("warmup"));
    assert_eq!(http_client.initialize_attempts(), 1);

    arm_session_post_failure(
        &base_url,
        /*status*/ 404,
        /*remaining*/ 1,
        /*www_authenticate_headers*/ &[],
    )
    .await?;
    let uncertain = call_echo_tool(&client, "uncertain-before-retry")
        .await
        .unwrap_err();
    assert!(
        uncertain
            .to_string()
            .contains("MCP session expired during tools/call")
    );
    assert_eq!(http_client.initialize_attempts(), 1);

    http_client.fail_next_initialize();
    let recovered = call_echo_tool(&client, "distinct-call-after-retry").await?;
    assert_eq!(http_client.initialize_attempts(), 3);
    assert_eq!(recovered, expected_echo_result("distinct-call-after-retry"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_401_does_not_trigger_recovery() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;

    let warmup = call_echo_tool(&client, "warmup").await?;
    assert_eq!(warmup, expected_echo_result("warmup"));

    arm_session_post_failure(
        &base_url,
        /*status*/ 401,
        /*remaining*/ 2,
        /*www_authenticate_headers*/ &[],
    )
    .await?;

    let first_error = call_echo_tool(&client, "unauthorized").await.unwrap_err();
    assert!(first_error.to_string().contains("401"));

    let second_error = call_echo_tool(&client, "still-unauthorized")
        .await
        .unwrap_err();
    assert!(second_error.to_string().contains("401"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_401_challenge_is_a_tool_error_without_replay() -> anyhow::Result<()> {
    let challenge = r#"Bearer error="invalid_token", resource_metadata="https://example.com/.well-known/oauth-protected-resource""#;
    for challenges in [
        vec![challenge],
        vec![r#"Basic realm="proxy, login""#, challenge],
    ] {
        let (_server, base_url) = spawn_streamable_http_server().await?;
        let client = create_client(&base_url).await?;

        arm_session_post_failure(
            &base_url,
            /*status*/ 401,
            /*remaining*/ 1,
            &challenges,
        )
        .await?;

        let result = call_echo_tool(&client, "rejected").await?;
        let mut expected =
            CallToolResult::error(vec![ContentBlock::text("Authentication required")]);
        expected.meta = Some(MetaObject::from(serde_json::Map::from_iter([(
            "mcp/www_authenticate".to_string(),
            json!([challenges.join(", ")]),
        )])));
        assert_eq!(result, expected);

        // A retry inside the failed call would consume the single rejection and return success.
        assert_eq!(
            call_echo_tool(&client, "next-user-call").await?,
            expected_echo_result("next-user-call"),
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_403_scope_challenge_returns_insufficient_scope() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;

    let warmup = call_echo_tool(&client, "warmup").await?;
    assert_eq!(warmup, expected_echo_result("warmup"));

    arm_session_post_failure(
        &base_url,
        /*status*/ 403,
        /*remaining*/ 1,
        /*www_authenticate_headers*/
        &[r#"Bearer error="insufficient_scope", scope="files:read files:write""#],
    )
    .await?;

    let error = call_echo_tool(&client, "forbidden").await.unwrap_err();
    assert!(
        error.to_string().contains("Insufficient scope"),
        "expected insufficient-scope transport error, got: {error:#}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_403_finds_bearer_challenge_in_later_header_value() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;

    let warmup = call_echo_tool(&client, "warmup").await?;
    assert_eq!(warmup, expected_echo_result("warmup"));

    arm_session_post_failure(
        &base_url,
        /*status*/ 403,
        /*remaining*/ 1,
        /*www_authenticate_headers*/
        &[
            r#"Basic realm="example""#,
            r#"Bearer error="insufficient_scope", scope="files:read""#,
        ],
    )
    .await?;

    let error = call_echo_tool(&client, "forbidden").await.unwrap_err();
    assert!(
        error.to_string().contains("Insufficient scope"),
        "expected insufficient-scope transport error, got: {error:#}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_expired_session_reconnects_once_per_operation() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let http_client = FailFirstInitializeHttpClient::new(
        Environment::default_for_tests().get_http_client(),
        /*failures_remaining*/ 0,
    );
    let client = create_client_with_http_client(&base_url, Arc::new(http_client.clone())).await?;

    let warmup = call_echo_tool(&client, "warmup").await?;
    assert_eq!(warmup, expected_echo_result("warmup"));
    assert_eq!(http_client.initialize_attempts(), 1);

    arm_session_post_failure(
        &base_url,
        /*status*/ 404,
        /*remaining*/ 2,
        /*www_authenticate_headers*/ &[],
    )
    .await?;

    let first_error = call_echo_tool(&client, "first-404").await.unwrap_err();
    assert!(
        first_error
            .to_string()
            .contains("MCP session expired during tools/call")
    );
    assert_eq!(http_client.initialize_attempts(), 1);
    assert_eq!(http_client.tool_call_attempts(), 2);

    let second_error = call_echo_tool(&client, "second-404").await.unwrap_err();
    assert!(
        second_error
            .to_string()
            .contains("MCP session expired during tools/call")
    );
    assert_eq!(http_client.initialize_attempts(), 2);
    assert_eq!(http_client.tool_call_attempts(), 3);

    let recovered = call_echo_tool(&client, "after-two-404s").await?;
    assert_eq!(recovered, expected_echo_result("after-two-404s"));
    assert_eq!(http_client.initialize_attempts(), 3);
    assert_eq!(http_client.tool_call_attempts(), 4);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_non_session_failure_does_not_trigger_recovery() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;

    let warmup = call_echo_tool(&client, "warmup").await?;
    assert_eq!(warmup, expected_echo_result("warmup"));

    arm_session_post_failure(
        &base_url,
        /*status*/ 500,
        /*remaining*/ 2,
        /*www_authenticate_headers*/ &[],
    )
    .await?;

    let first_error = call_echo_tool(&client, "server-error").await.unwrap_err();
    assert!(first_error.to_string().contains("500"));

    let second_error = call_echo_tool(&client, "still-server-error")
        .await
        .unwrap_err();
    assert!(second_error.to_string().contains("500"));

    Ok(())
}
