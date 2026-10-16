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
use streamable_http_test_support::arm_session_post_json_rpc_failure;
use streamable_http_test_support::call_echo_tool;
use streamable_http_test_support::create_client;
use streamable_http_test_support::create_client_with_http_client;
use streamable_http_test_support::expected_echo_result;
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
    request_captured: Arc<Notify>,
    release_request: Arc<Notify>,
}

impl DelayedToolCallHttpClient {
    fn new(inner: Arc<dyn HttpClient>) -> Self {
        Self {
            inner,
            initialize_attempts: Arc::new(AtomicUsize::new(0)),
            tool_call_attempts: Arc::new(AtomicUsize::new(0)),
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
        let request_captured = Arc::clone(&self.request_captured);
        let release_request = Arc::clone(&self.release_request);

        async move {
            if is_initialize_post(&params) {
                initialize_attempts.fetch_add(1, Ordering::SeqCst);
            }
            if is_tools_call_post(&params) {
                tool_call_attempts.fetch_add(1, Ordering::SeqCst);
            }
            if is_late_echo_post(&params) {
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
    params.method.eq_ignore_ascii_case("POST")
        && params
            .body
            .as_ref()
            .and_then(|body| serde_json::from_slice::<Value>(&body.0).ok())
            .and_then(|body| {
                body.get("method")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .is_some_and(|method| method == "tools/call")
}

fn is_late_echo_post(params: &HttpRequestParams) -> bool {
    if !params.method.eq_ignore_ascii_case("POST") {
        return false;
    }
    params
        .body
        .as_ref()
        .and_then(|body| serde_json::from_slice::<Value>(&body.0).ok())
        .is_some_and(|body| {
            body.get("method").and_then(Value::as_str) == Some("tools/call")
                && body.pointer("/params/name").and_then(Value::as_str) == Some("echo")
                && body
                    .pointer("/params/arguments/message")
                    .and_then(Value::as_str)
                    == Some("late")
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
async fn streamable_http_404_tools_call_is_uncertain_and_next_call_recovers() -> anyhow::Result<()>
{
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

    let uncertain = call_echo_tool(&client, "uncertain").await.unwrap_err();
    assert!(
        uncertain
            .to_string()
            .contains("MCP request outcome is uncertain")
    );
    assert_eq!(http_client.initialize_attempts(), 1);
    assert_eq!(http_client.tool_call_attempts(), 2);

    let recovered = call_echo_tool(&client, "distinct-follow-up").await?;
    assert_eq!(recovered, expected_echo_result("distinct-follow-up"));
    assert_eq!(http_client.initialize_attempts(), 2);
    assert_eq!(http_client.tool_call_attempts(), 3);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn late_expired_session_failure_does_not_retire_replacement() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let http_client =
        DelayedToolCallHttpClient::new(Environment::default_for_tests().get_http_client());
    let client =
        Arc::new(create_client_with_http_client(&base_url, Arc::new(http_client.clone())).await?);

    assert_eq!(
        call_echo_tool(&client, "warmup").await?,
        expected_echo_result("warmup")
    );
    let initial_initialize_attempts = http_client.initialize_attempts();
    assert!(initial_initialize_attempts >= 1);

    // Hold the request after this service generation produced it but before
    // transport dispatch. The test orders the old and replacement failures.
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
            .contains("MCP request outcome is uncertain")
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
            .contains("MCP request outcome is uncertain")
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

    let expected = client
        .list_tools(/*params*/ None, Some(Duration::from_secs(5)))
        .await?;

    arm_session_post_failure(
        &base_url,
        /*status*/ 404,
        /*remaining*/ 1,
        /*www_authenticate_headers*/ &[],
    )
    .await?;
    http_client.fail_next_initialize();

    let recovered = client
        .list_tools(/*params*/ None, Some(Duration::from_secs(5)))
        .await?;
    assert_eq!(http_client.initialize_attempts(), 3);
    assert_eq!(recovered, expected);

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
async fn streamable_http_404_recovery_only_retries_once() -> anyhow::Result<()> {
    let (_server, base_url) = spawn_streamable_http_server().await?;
    let client = create_client(&base_url).await?;

    let expected = client
        .list_tools(/*params*/ None, Some(Duration::from_secs(5)))
        .await?;

    arm_session_post_failure(
        &base_url,
        /*status*/ 404,
        /*remaining*/ 2,
        /*www_authenticate_headers*/ &[],
    )
    .await?;

    let error = client
        .list_tools(/*params*/ None, Some(Duration::from_secs(5)))
        .await
        .unwrap_err();
    let error_message = error.to_string();
    assert!(
        error_message.contains("404") || error_message.contains("session expired"),
        "expected session-expiry error, got: {error:#}"
    );

    let recovered = client
        .list_tools(/*params*/ None, Some(Duration::from_secs(5)))
        .await?;
    assert_eq!(recovered, expected);

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
