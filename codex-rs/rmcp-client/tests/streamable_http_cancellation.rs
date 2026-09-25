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
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use streamable_http_test_support::create_client;
use tokio::sync::Mutex;
use tokio::sync::Notify;

const SESSION_ID: &str = "cancellation-test-session";
const MCP_SESSION_ID: HeaderName = HeaderName::from_static("mcp-session-id");
const BLOCKED_REQUESTS: usize = 16;

#[derive(Clone, Default)]
struct ServerState {
    blocked_started: Arc<Mutex<Vec<Value>>>,
    observed_requests: Arc<Mutex<Vec<Value>>>,
    cancelled: Arc<Mutex<Vec<Value>>>,
    mutating_calls: Arc<Mutex<Vec<Value>>>,
    blocked_started_notify: Arc<Notify>,
    mutating_started_notify: Arc<Notify>,
    cancelled_notify: Arc<Notify>,
    release_blocked: Arc<Notify>,
}

impl ServerState {
    async fn wait_for_blocked(&self, count: usize) -> anyhow::Result<()> {
        loop {
            if self.blocked_started.lock().await.len() >= count {
                return Ok(());
            }
            tokio::time::timeout(Duration::from_secs(10), async {
                let notified = self.blocked_started_notify.notified();
                if self.blocked_started.lock().await.len() < count {
                    notified.await;
                }
            })
            .await?;
        }
    }

    async fn wait_for_cancellation(&self) -> anyhow::Result<()> {
        loop {
            if !self.cancelled.lock().await.is_empty() {
                return Ok(());
            }
            tokio::time::timeout(Duration::from_secs(10), async {
                let notified = self.cancelled_notify.notified();
                if self.cancelled.lock().await.is_empty() {
                    notified.await;
                }
            })
            .await?;
        }
    }

    async fn wait_for_mutating_call(&self) -> anyhow::Result<()> {
        loop {
            if !self.mutating_calls.lock().await.is_empty() {
                return Ok(());
            }
            tokio::time::timeout(Duration::from_secs(10), async {
                let notified = self.mutating_started_notify.notified();
                if self.mutating_calls.lock().await.is_empty() {
                    notified.await;
                }
            })
            .await?;
        }
    }
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
    let method = request.get("method").and_then(Value::as_str);
    match method {
        Some("initialize") => json_response(
            request.get("id").cloned(),
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "serverInfo": { "name": "cancellation-test", "version": "0.0.0" }
            }),
            true,
        ),
        Some("notifications/initialized") | Some("notifications/cancelled") => {
            if method == Some("notifications/cancelled") {
                state.cancelled.lock().await.push(request.clone());
                state.cancelled_notify.notify_one();
            }
            Response::builder()
                .status(StatusCode::ACCEPTED)
                .body(Body::empty())
                .expect("valid notification response")
        }
        Some("tools/call") => {
            state.observed_requests.lock().await.push(request.clone());
            let name = request
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if name == "blocked" {
                state.blocked_started.lock().await.push(request.clone());
                state.blocked_started_notify.notify_one();
                state.release_blocked.notified().await;
            } else if name == "queued" {
                state.release_blocked.notified().await;
            } else if name == "mutate" {
                state.mutating_calls.lock().await.push(request.clone());
                state.mutating_started_notify.notify_one();
                state.release_blocked.notified().await;
            }
            json_response(
                request.get("id").cloned(),
                json!({ "content": [], "isError": false }),
                false,
            )
        }
        Some("resources/read") => json_response(
            request.get("id").cloned(),
            json!({
                "contents": [{
                    "uri": request.pointer("/params/uri").cloned().unwrap_or(Value::Null),
                    "mimeType": "text/plain",
                    "text": "follow-on read"
                }]
            }),
            false,
        ),
        _ => json_response(
            request.get("id").cloned(),
            json!({ "content": [], "isError": false }),
            false,
        ),
    }
}

fn json_response(id: Option<Value>, result: Value, include_session: bool) -> Response {
    let body = serde_json::to_vec(&json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        .expect("serialize cancellation test response");
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("valid JSON response");
    if include_session {
        response
            .headers_mut()
            .insert(MCP_SESSION_ID, HeaderValue::from_static(SESSION_ID));
    }
    response
}

async fn initialized_client(base_url: &str) -> anyhow::Result<Arc<codex_rmcp_client::RmcpClient>> {
    Ok(Arc::new(create_client(base_url).await?))
}

#[tokio::test]
async fn blocked_posts_do_not_starve_an_independent_read() -> anyhow::Result<()> {
    let (state, base_url, server) = spawn_server().await;
    let client = initialized_client(&base_url).await?;

    let mut blocked = Vec::new();
    let task_client = client.clone();
    blocked.push(tokio::spawn(async move {
        task_client
            .call_tool("blocked".to_string(), Some(json!({})), None, None)
            .await
    }));
    state.wait_for_blocked(1).await?;

    let read = tokio::time::timeout(
        Duration::from_secs(2),
        client.read_resource(
            rmcp::model::ReadResourceRequestParams::new("memo://follow-on".to_string()),
            Some(Duration::from_secs(2)),
        ),
    )
    .await??;
    assert_eq!(
        serde_json::to_value(read)?,
        json!({"contents": [{
            "uri": "memo://follow-on", "mimeType": "text/plain", "text": "follow-on read"
        }]})
    );

    state.release_blocked.notify_waiters();
    for request in blocked {
        request.await??;
    }
    client.shutdown().await;
    server.abort();
    Ok(())
}

#[tokio::test]
async fn timed_out_post_sends_matching_cancellation_and_reclaims_capacity() -> anyhow::Result<()> {
    let (state, base_url, server) = spawn_server().await;
    let client = initialized_client(&base_url).await?;
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
    state.wait_for_blocked(1).await?;
    let result = timed_out.await?;
    assert!(result.is_err(), "timed-out call unexpectedly succeeded");
    state.wait_for_cancellation().await?;

    let blocked_id = state.blocked_started.lock().await[0]
        .get("id")
        .cloned()
        .expect("blocked request id");
    let cancelled_id = state.cancelled.lock().await[0]
        .pointer("/params/requestId")
        .cloned()
        .expect("cancelled request id");
    assert_eq!(cancelled_id, blocked_id);

    let read = tokio::time::timeout(
        Duration::from_secs(2),
        client.read_resource(
            rmcp::model::ReadResourceRequestParams::new("memo://follow-on".to_string()),
            Some(Duration::from_secs(2)),
        ),
    )
    .await??;
    assert_eq!(
        serde_json::to_value(read)?,
        json!({"contents": [{
            "uri": "memo://follow-on", "mimeType": "text/plain", "text": "follow-on read"
        }]})
    );

    state.release_blocked.notify_waiters();
    client.shutdown().await;
    server.abort();
    Ok(())
}

#[tokio::test]
async fn cancelled_queued_post_is_never_sent_or_executed() -> anyhow::Result<()> {
    let (state, base_url, server) = spawn_server().await;
    let client = initialized_client(&base_url).await?;
    let mut blocked = Vec::new();
    for _ in 0..BLOCKED_REQUESTS {
        let client = client.clone();
        blocked.push(tokio::spawn(async move {
            client
                .call_tool("blocked".to_string(), Some(json!({})), None, None)
                .await
        }));
    }
    state.wait_for_blocked(BLOCKED_REQUESTS).await?;

    // Poll the actual call future, not a JoinHandle: it reaches rmcp's response
    // wait while every ordinary HTTP slot is held by an observed request.
    let mut queued = Box::pin(client.call_tool("queued".to_string(), Some(json!({})), None, None));
    assert!(futures::poll!(queued.as_mut()).is_pending());
    drop(queued);
    state.wait_for_cancellation().await?;
    let cancelled_id = state.cancelled.lock().await[0]
        .pointer("/params/requestId")
        .cloned()
        .expect("queued cancellation id");
    assert!(
        state
            .blocked_started
            .lock()
            .await
            .iter()
            .all(|request| { request.get("id") != Some(&cancelled_id) }),
        "queued cancellation must not target any active request"
    );
    assert!(!state.observed_requests.lock().await.iter().any(|request| {
        request.pointer("/params/name").and_then(Value::as_str) == Some("queued")
    }));

    state.release_blocked.notify_waiters();
    for request in blocked {
        tokio::time::timeout(Duration::from_secs(10), request).await???;
    }
    assert_eq!(state.blocked_started.lock().await.len(), BLOCKED_REQUESTS);

    let read = client
        .read_resource(
            rmcp::model::ReadResourceRequestParams::new("memo://follow-on".to_string()),
            Some(Duration::from_secs(2)),
        )
        .await?;
    assert_eq!(
        serde_json::to_value(read)?,
        json!({"contents": [{
            "uri": "memo://follow-on", "mimeType": "text/plain", "text": "follow-on read"
        }]})
    );
    assert!(
        !state.observed_requests.lock().await.iter().any(|request| {
            request.pointer("/params/name").and_then(Value::as_str) == Some("queued")
        }),
        "cancelled queued call must stay absent after capacity is released"
    );
    client.shutdown().await;
    server.abort();
    Ok(())
}

#[tokio::test]
async fn timed_out_mutating_call_is_not_replayed() -> anyhow::Result<()> {
    let (state, base_url, server) = spawn_server().await;
    let client = initialized_client(&base_url).await?;
    let task_client = client.clone();
    let timed_out = tokio::spawn(async move {
        task_client
            .call_tool(
                "mutate".to_string(),
                Some(json!({ "value": "once" })),
                None,
                Some(Duration::from_millis(100)),
            )
            .await
    });

    state.wait_for_mutating_call().await?;
    let result = timed_out.await?;
    assert!(
        result.is_err(),
        "timed-out mutating call unexpectedly succeeded"
    );
    state.wait_for_cancellation().await?;
    state.release_blocked.notify_waiters();
    assert_eq!(state.mutating_calls.lock().await.len(), 1);
    client.shutdown().await;
    server.abort();
    Ok(())
}

#[tokio::test]
async fn externally_aborted_post_sends_matching_cancellation() -> anyhow::Result<()> {
    let (state, base_url, server) = spawn_server().await;
    let client = initialized_client(&base_url).await?;
    let task_client = client.clone();
    let request = tokio::spawn(async move {
        task_client
            .call_tool("blocked".to_string(), Some(json!({})), None, None)
            .await
    });
    state.wait_for_blocked(1).await?;
    let blocked_id = state.blocked_started.lock().await[0]
        .get("id")
        .cloned()
        .expect("blocked request id");
    let mut remaining = Vec::new();
    for _ in 1..BLOCKED_REQUESTS {
        let task_client = client.clone();
        remaining.push(tokio::spawn(async move {
            task_client
                .call_tool("blocked".to_string(), Some(json!({})), None, None)
                .await
        }));
    }
    state.wait_for_blocked(BLOCKED_REQUESTS).await?;
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    state.wait_for_cancellation().await?;
    let cancelled_id = state.cancelled.lock().await[0]
        .pointer("/params/requestId")
        .cloned()
        .expect("cancelled request id");
    assert_eq!(cancelled_id, blocked_id);

    // All sixteen ordinary slots were occupied. This read can complete only
    // after cancellation reclaims the aborted POST's slot; none is released here.
    let read = client
        .read_resource(
            rmcp::model::ReadResourceRequestParams::new("memo://follow-on".to_string()),
            Some(Duration::from_secs(2)),
        )
        .await?;
    assert_eq!(
        serde_json::to_value(read)?,
        json!({"contents": [{
            "uri": "memo://follow-on", "mimeType": "text/plain", "text": "follow-on read"
        }]})
    );
    state.release_blocked.notify_waiters();
    for request in remaining {
        tokio::time::timeout(Duration::from_secs(10), request).await???;
    }
    client.shutdown().await;
    server.abort();
    Ok(())
}
