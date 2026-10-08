mod streamable_http_test_support;

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
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

const SESSION_HEADER: HeaderName = HeaderName::from_static("mcp-session-id");
const SESSION_ID: &str = "cancellation-test-session";

#[derive(Clone, Default)]
struct ServerState {
    requests: Arc<Mutex<Vec<Value>>>,
    cancellations: Arc<Mutex<Vec<Value>>>,
    cancellation_notify: Arc<Notify>,
    blocked: Arc<Notify>,
    blocked_started: Arc<Notify>,
    modern_post_dropped: Arc<Notify>,
    modern_dropped_ids: Arc<std::sync::Mutex<Vec<Value>>>,
    read_404_remaining: Arc<AtomicUsize>,
}

struct NotifyOnDrop {
    notify: Arc<Notify>,
    dropped_ids: Arc<std::sync::Mutex<Vec<Value>>>,
    request_id: Value,
}
impl Drop for NotifyOnDrop {
    fn drop(&mut self) {
        if let Ok(mut dropped_ids) = self.dropped_ids.lock() {
            dropped_ids.push(self.request_id.clone());
        }
        self.notify.notify_one();
    }
}

async fn spawn_server() -> anyhow::Result<(ServerState, String, tokio::task::JoinHandle<()>)> {
    let state = ServerState::default();
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    let router = Router::new()
        .route("/mcp", post(handle_mcp))
        .with_state(state.clone());
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok((state, format!("http://{address}"), task))
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
                json!({"protocolVersion":protocol,"capabilities":{},"serverInfo":{"name":"cancel-test","version":"0"}}),
                /*session*/ true,
            )
        }
        Some("notifications/initialized") => accepted_response(),
        Some("notifications/cancelled") => {
            state.cancellations.lock().await.push(request);
            state.cancellation_notify.notify_one();
            accepted_response()
        }
        Some("tools/call") => {
            let name = request.pointer("/params/name").and_then(Value::as_str);
            if name == Some("uncertain-404") {
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::NOT_FOUND;
                return response;
            }
            if name == Some("terminal-error") {
                let body = match serde_json::to_vec(
                    &json!({"jsonrpc":"2.0","id":request.get("id"),"error":{"code":-32603,"message":"terminal test error"}}),
                ) {
                    Ok(body) => body,
                    Err(error) => {
                        panic!("failed to encode terminal JSON-RPC test response: {error}")
                    }
                };
                let mut response = Response::new(Body::from(body));
                response
                    .headers_mut()
                    .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                return response;
            }
            if matches!(name, Some("blocked" | "mutate" | "modern-blocked")) {
                state.blocked_started.notify_one();
                let _dropped = (name == Some("modern-blocked")).then(|| NotifyOnDrop {
                    notify: state.modern_post_dropped.clone(),
                    dropped_ids: state.modern_dropped_ids.clone(),
                    request_id: request.get("id").cloned().unwrap_or(Value::Null),
                });
                state.blocked.notified().await;
            }
            json_response(
                request.get("id").cloned(),
                json!({"content":[],"isError":false}),
                /*session*/ false,
            )
        }
        Some("resources/read") => {
            if state
                .read_404_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    (remaining > 0).then(|| remaining - 1)
                })
                .is_ok()
            {
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::NOT_FOUND;
                return response;
            }
            json_response(
                request.get("id").cloned(),
                json!({"contents":[{"uri":"memo://after-cancel","mimeType":"text/plain","text":"ok"}]}),
                /*session*/ false,
            )
        }
        _ => json_response(request.get("id").cloned(), json!({}), /*session*/ false),
    }
}

fn accepted_response() -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::ACCEPTED;
    response
}

fn json_response(id: Option<Value>, result: Value, session: bool) -> Response {
    let body = match serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"result":result})) {
        Ok(body) => body,
        Err(error) => panic!("failed to encode JSON-RPC test response: {error}"),
    };
    let mut response = Response::new(Body::from(body));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if session {
        response
            .headers_mut()
            .insert(SESSION_HEADER, HeaderValue::from_static(SESSION_ID));
    }
    response
}

async fn create_modern_client(base_url: &str) -> anyhow::Result<codex_rmcp_client::RmcpClient> {
    let client = codex_rmcp_client::RmcpClient::new_streamable_http_client_with_protocol_mode(
        "modern-cancellation-test",
        &format!("{base_url}/mcp"),
        Some("test-bearer".to_string()),
        /*http_headers*/ None,
        /*env_http_headers*/ None,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
        Environment::default_for_tests().get_http_client(),
        /*auth_provider*/ None,
        McpProtocolMode::V20260728,
    )
    .await?;
    let mut capabilities = ClientCapabilities::default();
    capabilities.elicitation =
        Some(ElicitationCapability::new().with_form(FormElicitationCapability::new()));
    client
        .initialize(
            InitializeRequestParams::new(capabilities, Implementation::new("cancel-test", "0"))
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

async fn wait_cancel(state: &ServerState) -> anyhow::Result<()> {
    if !state.cancellations.lock().await.is_empty() {
        return Ok(());
    }
    tokio::time::timeout(Duration::from_secs(5), state.cancellation_notify.notified()).await?;
    Ok(())
}

#[tokio::test]
async fn terminal_json_rpc_error_does_not_send_stale_cancellation() -> anyhow::Result<()> {
    let (state, url, server) = spawn_server().await?;
    let client = create_client(&format!("{url}/mcp")).await?;
    assert!(
        client
            .call_tool(
                "terminal-error".into(),
                Some(json!({})),
                /*meta*/ None,
                Some(Duration::from_secs(2))
            )
            .await
            .is_err()
    );
    let late_cancellation = tokio::time::timeout(
        Duration::from_millis(250),
        state.cancellation_notify.notified(),
    )
    .await;
    assert!(
        late_cancellation.is_err(),
        "terminal JSON-RPC error must not send a stale cancellation"
    );
    assert!(state.cancellations.lock().await.is_empty());
    client.shutdown().await;
    server.abort();
    Ok(())
}

#[tokio::test]
async fn resources_read_session_expiry_recovers_once() -> anyhow::Result<()> {
    let (state, url, server) = spawn_server().await?;
    let client = create_client(&format!("{url}/mcp")).await?;
    state.read_404_remaining.store(1, Ordering::SeqCst);
    let read = client
        .read_resource(
            rmcp::model::ReadResourceRequestParams::new("memo://after-cancel"),
            Some(Duration::from_secs(5)),
        )
        .await?;
    assert_eq!(read.contents.len(), 1);
    let reads = state
        .requests
        .lock()
        .await
        .iter()
        .filter(|request| request.get("method").and_then(Value::as_str) == Some("resources/read"))
        .count();
    assert_eq!(
        reads, 2,
        "one expired read dispatch must have exactly one recovery attempt"
    );
    client.shutdown().await;
    server.abort();
    Ok(())
}

#[tokio::test]
async fn timed_out_call_sends_matching_cancellation_and_is_not_replayed() -> anyhow::Result<()> {
    let (state, url, server) = spawn_server().await?;
    let client = Arc::new(create_client(&format!("{url}/mcp")).await?);
    let request_client = client.clone();
    let request = tokio::spawn(async move {
        request_client
            .call_tool(
                "mutate".into(),
                Some(json!({})),
                /*meta*/ None,
                Some(Duration::from_millis(100)),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), state.blocked_started.notified()).await?;
    assert!(request.await?.is_err());
    wait_cancel(&state).await?;
    let requests = state.requests.lock().await.clone();
    let call = requests
        .iter()
        .find(|r| r.pointer("/params/name").and_then(Value::as_str) == Some("mutate"))
        .unwrap();
    assert_eq!(
        state.cancellations.lock().await[0].pointer("/params/requestId"),
        call.get("id")
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.pointer("/params/name").and_then(Value::as_str) == Some("mutate"))
            .count(),
        1
    );
    state.blocked.notify_waiters();
    let read = client
        .read_resource(
            rmcp::model::ReadResourceRequestParams::new("memo://after-cancel"),
            Some(Duration::from_secs(2)),
        )
        .await?;
    assert_eq!(read.contents.len(), 1);
    client.shutdown().await;
    server.abort();
    Ok(())
}

#[tokio::test]
async fn uncertain_session_expiry_does_not_replay_tools_call() -> anyhow::Result<()> {
    let (state, url, server) = spawn_server().await?;
    let client = create_client(&format!("{url}/mcp")).await?;
    assert!(
        client
            .call_tool(
                "uncertain-404".into(),
                Some(json!({})),
                /*meta*/ None,
                Some(Duration::from_secs(2))
            )
            .await
            .is_err()
    );
    let calls = state
        .requests
        .lock()
        .await
        .iter()
        .filter(|r| r.pointer("/params/name").and_then(Value::as_str) == Some("uncertain-404"))
        .count();
    assert_eq!(
        calls, 1,
        "uncertain tools/call must not be replayed after session expiry"
    );
    client.shutdown().await;
    server.abort();
    Ok(())
}

#[tokio::test]
async fn dropped_call_is_cancelled_without_replaying_mutation() -> anyhow::Result<()> {
    let (state, url, server) = spawn_server().await?;
    let client = Arc::new(create_client(&format!("{url}/mcp")).await?);
    let request_client = client.clone();
    let request = tokio::spawn(async move {
        request_client
            .call_tool(
                "mutate".into(),
                Some(json!({})),
                /*meta*/ None,
                /*timeout*/ None,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), state.blocked_started.notified()).await?;
    request.abort();
    assert!(request.await.is_err());
    wait_cancel(&state).await?;
    let requests = state.requests.lock().await.clone();
    let calls = requests
        .iter()
        .filter(|r| r.pointer("/params/name").and_then(Value::as_str) == Some("mutate"))
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        state.cancellations.lock().await[0].pointer("/params/requestId"),
        calls[0].get("id")
    );
    state.blocked.notify_waiters();
    client.shutdown().await;
    server.abort();
    Ok(())
}

#[tokio::test]
async fn modern_timeout_cancels_matching_inflight_post_without_legacy_control_post()
-> anyhow::Result<()> {
    let (state, url, server) = spawn_server().await?;
    let client = Arc::new(create_modern_client(&url).await?);
    let request_client = client.clone();
    let request = tokio::spawn(async move {
        request_client
            .call_tool(
                "modern-blocked".into(),
                Some(json!({})),
                /*meta*/ None,
                Some(Duration::from_millis(100)),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), state.blocked_started.notified()).await?;
    assert!(request.await?.is_err());
    tokio::time::timeout(Duration::from_secs(5), state.modern_post_dropped.notified()).await?;
    let request_id = state
        .requests
        .lock()
        .await
        .iter()
        .find(|r| r.pointer("/params/name").and_then(Value::as_str) == Some("modern-blocked"))
        .and_then(|r| r.get("id"))
        .cloned();
    assert_eq!(
        state
            .modern_dropped_ids
            .lock()
            .expect("drop ID lock")
            .as_slice(),
        &[request_id.unwrap_or(Value::Null)]
    );
    assert!(
        state.cancellations.lock().await.is_empty(),
        "modern HTTP cancellation must not require a legacy control POST"
    );
    client.shutdown().await;
    server.abort();
    Ok(())
}

#[tokio::test]
async fn modern_drop_cancels_matching_inflight_post_without_legacy_control_post()
-> anyhow::Result<()> {
    let (state, url, server) = spawn_server().await?;
    let client = Arc::new(create_modern_client(&url).await?);
    let request_client = client.clone();
    let request = tokio::spawn(async move {
        request_client
            .call_tool(
                "modern-blocked".into(),
                Some(json!({})),
                /*meta*/ None,
                /*timeout*/ None,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), state.blocked_started.notified()).await?;
    request.abort();
    assert!(request.await.is_err());
    tokio::time::timeout(Duration::from_secs(5), state.modern_post_dropped.notified()).await?;
    let request_id = state
        .requests
        .lock()
        .await
        .iter()
        .find(|r| r.pointer("/params/name").and_then(Value::as_str) == Some("modern-blocked"))
        .and_then(|r| r.get("id"))
        .cloned();
    assert_eq!(
        state
            .modern_dropped_ids
            .lock()
            .expect("drop ID lock")
            .as_slice(),
        &[request_id.unwrap_or(Value::Null)]
    );
    assert!(
        state.cancellations.lock().await.is_empty(),
        "modern HTTP cancellation must not require a legacy control POST"
    );
    client.shutdown().await;
    server.abort();
    Ok(())
}
