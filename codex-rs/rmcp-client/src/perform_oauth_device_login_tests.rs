use super::*;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::response::IntoResponse;
use axum::routing::post;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_exec_server::HttpClient;
use codex_exec_server::RouteAwareHttpClient;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use oauth2::TokenResponse;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::net::TcpListener;

#[derive(Clone)]
struct PollState {
    count: Arc<AtomicUsize>,
    outcome: &'static str,
    authorization_seen: Arc<AtomicBool>,
}

async fn spawn_server(state: PollState) -> String {
    async fn device(State(state): State<PollState>, headers: HeaderMap) -> Json<serde_json::Value> {
        record_authorization_header(&state, &headers);
        Json(json!({
            "device_code": "synthetic-device-code",
            "user_code": "SYNTHETIC-CODE",
            "verification_uri": "https://login.example.test/device",
            "expires_in": 60,
            "interval": 1
        }))
    }

    async fn token(
        State(state): State<PollState>,
        headers: HeaderMap,
        body: axum::body::Bytes,
    ) -> (StatusCode, Json<serde_json::Value>) {
        record_authorization_header(&state, &headers);
        let form = url::form_urlencoded::parse(&body)
            .into_owned()
            .collect::<HashMap<_, _>>();
        assert_eq!(
            form.get("grant_type").map(String::as_str),
            Some(DEVICE_CODE_GRANT_TYPE)
        );
        assert_eq!(
            form.get("device_code").map(String::as_str),
            Some("synthetic-device-code")
        );
        let poll = state.count.fetch_add(1, Ordering::SeqCst);
        if state.outcome == "success" && poll == 0 {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "authorization_pending"})),
            );
        }
        if state.outcome == "success" {
            return (
                StatusCode::OK,
                Json(json!({
                    "access_token": "synthetic-access-token",
                    "refresh_token": "synthetic-refresh-token",
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "scope": "ops:read"
                })),
            );
        }
        let (error, status) = match state.outcome {
            "denied" => ("access_denied", StatusCode::BAD_REQUEST),
            "expired" => ("expired_token", StatusCode::BAD_REQUEST),
            _ => ("server_error", StatusCode::INTERNAL_SERVER_ERROR),
        };
        (
            status,
            Json(json!({"error": error, "error_description": "synthetic provider response"})),
        )
    }

    async fn register(
        State(state): State<PollState>,
        headers: HeaderMap,
        Json(request): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        record_authorization_header(&state, &headers);
        assert_eq!(request["grant_types"], json!([DEVICE_CODE_GRANT_TYPE]));
        Json(json!({"client_id": "synthetic-client"}))
    }

    let app = Router::new()
        .route("/device", post(device))
        .route("/token", post(token))
        .route("/register", post(register))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind synthetic OAuth server");
    let address = listener.local_addr().expect("synthetic server address");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve synthetic OAuth server")
    });
    format!("http://{address}")
}

fn record_authorization_header(state: &PollState, headers: &HeaderMap) {
    if headers.contains_key(AUTHORIZATION) {
        state.authorization_seen.store(true, Ordering::SeqCst);
    }
}

fn oauth_http_client(
    resource_url: &str,
    configured_headers: Option<HashMap<String, String>>,
) -> OAuthHttpClientAdapter {
    let has_configured_headers = configured_headers
        .as_ref()
        .is_some_and(|headers| !headers.is_empty());
    let default_headers = build_default_headers(configured_headers, None).expect("headers");
    let http_client: Arc<dyn HttpClient> = Arc::new(RouteAwareHttpClient::new(
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    ));
    OAuthHttpClientAdapter::new_with_max_timeout_and_redirect_mode(
        http_client,
        default_headers,
        resource_url,
        DEVICE_HTTP_REQUEST_TIMEOUT,
        has_configured_headers,
        StreamableHttpRedirectMode::Legacy,
    )
    .expect("OAuth HTTP client")
}

fn routed_http_client() -> Arc<dyn HttpClient> {
    Arc::new(RouteAwareHttpClient::new(HttpClientFactory::new(
        OutboundProxyPolicy::ReqwestDefault,
    )))
}

fn prompt(_: DeviceAuthorizationPrompt) {}

#[tokio::test(start_paused = true)]
async fn device_login_saves_issuer_bound_tokens_and_reopens_them() -> Result<()> {
    let _home = crate::oauth::test_support::TempCodexHome::new();
    let authorization_seen = Arc::new(AtomicBool::new(false));
    let server = spawn_server(PollState {
        count: Arc::new(AtomicUsize::new(0)),
        outcome: "success",
        authorization_seen: authorization_seen.clone(),
    })
    .await;
    let resource_url = "https://resource.example.test/mcp";
    perform_oauth_device_login(
        "ops",
        resource_url,
        &server,
        routed_http_client(),
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        Some(HashMap::from([(
            "Authorization".to_string(),
            "Bearer synthetic-resource-credential".to_string(),
        )])),
        None,
        &["ops:read".to_string()],
        None,
        None,
        &format!("{server}/device"),
        &format!("{server}/token"),
        Some(&format!("{server}/register")),
        false,
        false,
        prompt,
    )
    .await?;

    assert!(!authorization_seen.load(Ordering::SeqCst));
    let stored = crate::stored_oauth_credentials(
        "ops",
        resource_url,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?
    .expect("device-flow credential reopens from the selected file store");
    assert_eq!(stored.issuer.as_deref(), Some(server.as_str()));
    assert_eq!(stored.client_id, "synthetic-client");
    assert_eq!(
        stored.token_response.0.refresh_token().unwrap().secret(),
        "synthetic-refresh-token"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn device_login_rejects_denial_expiry_and_provider_errors() {
    let details = DeviceAuthorizationResponse {
        device_code: "synthetic-device-code".to_string(),
        user_code: "SYNTHETIC-CODE".to_string(),
        verification_uri: "https://login.example.test/device".to_string(),
        verification_uri_complete: None,
        expires_in: Some(60),
        interval: Some(1),
    };
    for (outcome, expected) in [
        ("denied", "denied"),
        ("expired", "expired"),
        ("error", "server_error"),
    ] {
        let server = spawn_server(PollState {
            count: Arc::new(AtomicUsize::new(0)),
            outcome,
            authorization_seen: Arc::new(AtomicBool::new(false)),
        })
        .await;
        let client = oauth_http_client("https://mcp.example.test/mcp", None);
        let error = poll_device_token(
            &client,
            &format!("{server}/token"),
            "synthetic-client",
            None,
            &details,
            None,
        )
        .await
        .expect_err("provider terminal result must fail closed");
        assert!(
            error.to_string().contains(expected),
            "unexpected error: {error:#}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn device_login_request_timeout_is_bounded_by_code_expiry() {
    async fn hang() -> impl IntoResponse {
        std::future::pending::<axum::response::Response>().await
    }
    let app = Router::new().route("/token", post(hang));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind hanging OAuth server");
    let address = listener.local_addr().expect("hanging server address");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve hanging OAuth server")
    });
    let server = format!("http://{address}");
    let client = oauth_http_client("https://mcp.example.test/mcp", None);
    let details = DeviceAuthorizationResponse {
        device_code: "synthetic-device-code".to_string(),
        user_code: "SYNTHETIC-CODE".to_string(),
        verification_uri: "https://login.example.test/device".to_string(),
        verification_uri_complete: None,
        expires_in: Some(1),
        interval: Some(1),
    };
    let error = poll_device_token(
        &client,
        &format!("{server}/token"),
        "synthetic-client",
        None,
        &details,
        None,
    )
    .await
    .expect_err("stalled OAuth request must not outlive the device code");
    assert!(error.to_string().contains("timed out"));
}

#[test]
fn device_login_requires_a_matching_nonempty_issuer() -> anyhow::Result<()> {
    assert!(
        validate_device_endpoints(
            "",
            "https://issuer.example.test/device",
            "https://issuer.example.test/token",
            false,
        )
        .is_err()
    );
    assert!(
        validate_device_endpoints(
            "https://issuer.example.test",
            "https://other.example.test/device",
            "https://issuer.example.test/token",
            false,
        )
        .is_err()
    );
    validate_device_endpoints(
        "https://issuer.example.test",
        "https://issuer.example.test/device",
        "https://issuer.example.test/token",
        false,
    )?;
    Ok(())
}
