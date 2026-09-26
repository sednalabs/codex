mod streamable_http_test_support;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::http::header::HeaderName;
use axum::http::header::HeaderValue;
use axum::response::Response;
use axum::routing::post;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_exec_server::Environment;
use codex_rmcp_client::ElicitationAction;
use codex_rmcp_client::ElicitationResponse;
use codex_rmcp_client::McpProtocolMode;
use futures::FutureExt;
use rmcp::model::ClientCapabilities;
use rmcp::model::ElicitationCapability;
use rmcp::model::FormElicitationCapability;
use rmcp::model::Implementation;
use rmcp::model::InitializeRequestParams;
use rmcp::model::ProtocolVersion;
use serde_json::Value;
use serde_json::json;
use streamable_http_test_support::create_client;
use tokio::sync::Mutex;
use tokio::sync::Notify;

const SESSION_ID: &str = "cancellation-test-session";
const SESSION_HEADER: HeaderName = HeaderName::from_static("mcp-session-id");

#[derive(Clone, Default)]
struct ServerState {
    requests: Arc<Mutex<Vec<Value>>>,
    cancellations: Arc<Mutex<Vec<Value>>>,
    cancellation_notify: Arc<Notify>,
    error_after_send: Arc<Mutex<bool>>,
    blocked: Arc<Notify>,
    blocked_started: Arc<Notify>,
}

async fn spawn_server() -> (ServerState, String, tokio::task::JoinHandle<()>) {
    let state = ServerState::default();
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind cancellation test server");
    let address = listener
        .local_addr()
        .expect("read cancellation test address");
    let router = Router::new()
        .route("/mcp", post(handle_mcp))
        .with_state(state.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("serve cancellation test server");
    });
    (state, format!("http://{address}"), task)
}

async fn handle_mcp(State(state): State<ServerState>, Json(request): Json<Value>) -> Response {
    state.requests.lock().await.push(request.clone());
    match request.get("method").and_then(Value::as_str) {
        Some("initialize") => {
            let protocol = request
                .pointer("/params/protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-06-18");
            json_response(
                request.get("id").cloned(),
                json!({
                    "protocolVersion": protocol,
                    "capabilities": {},
                    "serverInfo": { "name": "cancellation-test", "version": "0.0.0" }
                }),
                true,
            )
        }
        Some("notifications/initialized") => accepted_response(),
        Some("notifications/cancelled") => {
            state.cancellations.lock().await.push(request);
            state.cancellation_notify.notify_waiters();
            accepted_response()
        }
        Some("tools/call") => {
            let name = request.pointer("/params/name").and_then(Value::as_str);
            if name == Some("error-after-send") && *state.error_after_send.lock().await {
                return Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::empty())
                    .expect("valid expired-session response");
            }
            if matches!(name, Some("blocked" | "mutate")) {
                state.blocked_started.notify_waiters();
                state.blocked.notified().await;
            }
            json_response(
                request.get("id").cloned(),
                json!({ "content": [], "isError": false }),
                false,
            )
        }
        Some("resources/read") => json_response(
            request.get("id").cloned(),
            json!({ "contents": [{
                "uri": request.pointer("/params/uri").cloned().unwrap_or(Value::Null),
                "mimeType": "text/plain",
                "text": "follow-on read"
            }]}),
            false,
        ),
        _ => json_response(request.get("id").cloned(), json!({}), false),
    }
}

fn accepted_response() -> Response {
    Response::builder()
        .status(StatusCode::ACCEPTED)
        .body(Body::empty())
        .expect("valid notification response")
}

fn json_response(id: Option<Value>, result: Value, include_session: bool) -> Response {
    let body = serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        .expect("serialize cancellation response");
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("valid JSON response");
    if include_session {
        response
            .headers_mut()
            .insert(SESSION_HEADER, HeaderValue::from_static(SESSION_ID));
    }
    response
}

async fn wait_for_cancellation(state: &ServerState) {
    if !state.cancellations.lock().await.is_empty() {
        return;
    }
    tokio::time::timeout(Duration::from_secs(5), state.cancellation_notify.notified())
        .await
        .expect("cancellation notification");
}

async fn create_modern_client(base_url: &str) -> anyhow::Result<codex_rmcp_client::RmcpClient> {
    let client = codex_rmcp_client::RmcpClient::new_streamable_http_client_with_protocol_mode(
        "modern-cancellation-test",
        &format!("{base_url}/mcp"),
        Some("test-bearer".to_string()),
        None,
        None,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
        Environment::default_for_tests().get_http_client(),
        None,
        McpProtocolMode::V20260728,
    )
    .await?;
    let mut capabilities = ClientCapabilities::default();
    capabilities.elicitation =
        Some(ElicitationCapability::new().with_form(FormElicitationCapability::new()));
    client
        .initialize(
            InitializeRequestParams::new(
                capabilities,
                Implementation::new("codex-cancellation-test", "0.0.0"),
            )
            .with_protocol_version(ProtocolVersion::V_2026_07_28),
            Some(Duration::from_secs(5)),
            Box::new(|_, _| {
                async {
                    Ok(ElicitationResponse {
                        action: ElicitationAction::Accept,
                        content: Some(json!({})),
                        meta: None,
                    })
                }
                .boxed()
            }),
        )
        .await?;
    Ok(client)
}

#[tokio::test]
async fn timed_out_call_sends_matching_cancellation_and_allows_follow_on_read() {
    let (state, base_url, server) = spawn_server().await;
    let client = Arc::new(
        create_client(&format!("{base_url}/mcp"))
            .await
            .expect("client"),
    );
    let task_client = client.clone();
    let timed_out = tokio::spawn(async move {
        task_client
            .call_tool(
                "blocked".to_string(),
                Some(json!({})),
                None,
                Some(Duration::from_millis(100)),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), state.blocked_started.notified())
        .await
        .expect("blocked request");
    assert!(timed_out.await.expect("join timed-out call").is_err());
    wait_for_cancellation(&state).await;

    let requests = state.requests.lock().await;
    let blocked_id = requests
        .iter()
        .find(|request| request.pointer("/params/name").and_then(Value::as_str) == Some("blocked"))
        .and_then(|request| request.get("id"))
        .cloned()
        .expect("blocked id");
    let cancelled_id = state.cancellations.lock().await[0]
        .pointer("/params/requestId")
        .cloned()
        .expect("cancelled request id");
    assert_eq!(cancelled_id, blocked_id);
    drop(requests);

    state.blocked.notify_waiters();
    let read = client
        .read_resource(
            rmcp::model::ReadResourceRequestParams::new("memo://follow-on".to_string()),
            Some(Duration::from_secs(2)),
        )
        .await
        .expect("follow-on read");
    assert_eq!(
        serde_json::to_value(read).expect("serialize read"),
        json!({
            "contents": [{ "uri": "memo://follow-on", "mimeType": "text/plain", "text": "follow-on read" }]
        })
    );
    client.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn dropped_call_is_cancelled_without_replaying_mutation() {
    let (state, base_url, server) = spawn_server().await;
    let client = Arc::new(
        create_client(&format!("{base_url}/mcp"))
            .await
            .expect("client"),
    );
    let task_client = client.clone();
    let request = tokio::spawn(async move {
        task_client
            .call_tool(
                "mutate".to_string(),
                Some(json!({ "value": "once" })),
                None,
                None,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), state.blocked_started.notified())
        .await
        .expect("mutating request");
    request.abort();
    assert!(request.await.expect_err("aborted request").is_cancelled());
    wait_for_cancellation(&state).await;
    state.blocked.notify_waiters();
    let mutation_count = state
        .requests
        .lock()
        .await
        .iter()
        .filter(|request| request.pointer("/params/name").and_then(Value::as_str) == Some("mutate"))
        .count();
    assert_eq!(mutation_count, 1);
    client.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn modern_timed_out_call_sends_matching_cancellation() {
    let (state, base_url, server) = spawn_server().await;
    let client = Arc::new(
        create_modern_client(&base_url)
            .await
            .expect("modern client"),
    );
    let task_client = client.clone();
    let timed_out = tokio::spawn(async move {
        task_client
            .call_tool(
                "blocked".to_string(),
                Some(json!({})),
                None,
                Some(Duration::from_millis(100)),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), state.blocked_started.notified())
        .await
        .expect("modern blocked request");
    assert!(timed_out.await.expect("join modern timeout").is_err());
    wait_for_cancellation(&state).await;
    let requests = state.requests.lock().await;
    let blocked_id = requests
        .iter()
        .find(|request| request.pointer("/params/name").and_then(Value::as_str) == Some("blocked"))
        .and_then(|request| request.get("id"))
        .cloned()
        .expect("modern blocked id");
    let cancelled_id = state.cancellations.lock().await[0]
        .pointer("/params/requestId")
        .cloned()
        .expect("modern cancelled request id");
    assert_eq!(cancelled_id, blocked_id);
    drop(requests);
    state.blocked.notify_waiters();
    client.shutdown().await;
    server.abort();
}

#[tokio::test]
async fn session_expiry_after_send_does_not_replay_a_mutation() {
    let (state, base_url, server) = spawn_server().await;
    *state.error_after_send.lock().await = true;
    let client = Arc::new(
        create_client(&format!("{base_url}/mcp"))
            .await
            .expect("client"),
    );
    let result = client
        .call_tool(
            "error-after-send".to_string(),
            Some(json!({ "value": "once" })),
            None,
            Some(Duration::from_secs(2)),
        )
        .await;
    assert!(result.is_err(), "uncertain mutation unexpectedly succeeded");
    wait_for_cancellation(&state).await;
    let mutation_count = state
        .requests
        .lock()
        .await
        .iter()
        .filter(|request| {
            request.pointer("/params/name").and_then(Value::as_str) == Some("error-after-send")
        })
        .count();
    assert_eq!(mutation_count, 1, "uncertain mutation must not be replayed");
    client.shutdown().await;
    server.abort();
}
