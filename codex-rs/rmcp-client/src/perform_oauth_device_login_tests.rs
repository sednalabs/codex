use super::*;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode as AxumStatusCode;
use axum::routing::post;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_exec_server::RouteAwareHttpClient;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::net::TcpListener;

#[derive(Clone)]
struct PollState {
    polls: Arc<AtomicUsize>,
    terminal_error: Option<&'static str>,
    slow_down_first: bool,
}

async fn spawn_server(state: PollState) -> String {
    async fn device() -> Json<serde_json::Value> {
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
        body: axum::body::Bytes,
    ) -> (AxumStatusCode, Json<serde_json::Value>) {
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
        let poll = state.polls.fetch_add(1, Ordering::SeqCst);
        if state.slow_down_first && poll == 0 {
            return (
                AxumStatusCode::BAD_REQUEST,
                Json(json!({"error": "slow_down"})),
            );
        }
        if let Some(error) = state.terminal_error {
            return (AxumStatusCode::BAD_REQUEST, Json(json!({"error": error})));
        }
        if poll == 0 {
            return (
                AxumStatusCode::BAD_REQUEST,
                Json(json!({"error": "authorization_pending"})),
            );
        }
        (
            AxumStatusCode::OK,
            Json(json!({
                "access_token": "synthetic-access-token",
                "refresh_token": "synthetic-refresh-token",
                "token_type": "Bearer",
                "expires_in": 3600
            })),
        )
    }
    async fn register(Json(request): Json<serde_json::Value>) -> Json<serde_json::Value> {
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
        .expect("bind synthetic server");
    let address = listener.local_addr().expect("synthetic server address");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve synthetic server")
    });
    format!("http://{address}")
}

fn http_client() -> Arc<dyn HttpClient> {
    Arc::new(RouteAwareHttpClient::new(HttpClientFactory::new(
        OutboundProxyPolicy::ReqwestDefault,
    )))
}

fn prompt(_: DeviceAuthorizationPrompt) {}

#[test]
fn device_login_requires_a_nonempty_issuer_and_matching_endpoint_origins() {
    assert!(
        validate_device_endpoints(
            "",
            "https://issuer.example/device",
            "https://issuer.example/token",
            false
        )
        .is_err()
    );
    assert!(
        validate_device_endpoints(
            "https://issuer.example",
            "https://other.example/device",
            "https://issuer.example/token",
            false
        )
        .is_err()
    );
    assert!(
        validate_device_endpoints(
            "https://issuer.example",
            "http://issuer.example/device",
            "https://issuer.example/token",
            false
        )
        .is_err()
    );
    assert!(
        validate_device_endpoints(
            "ftp://localhost/issuer",
            "http://localhost/device",
            "http://localhost/token",
            true,
        )
        .is_err()
    );
    assert!(
        validate_device_endpoints(
            "https://user:secret@issuer.example",
            "https://issuer.example/device",
            "https://issuer.example/token",
            false,
        )
        .is_err()
    );
    validate_device_endpoints(
        "https://issuer.example",
        "https://issuer.example/device",
        "https://issuer.example/token",
        false,
    )
    .expect("matching secure endpoints");
}

#[tokio::test]
async fn device_login_polls_pending_then_saves_issuer_bound_tokens() -> Result<()> {
    let _home = crate::oauth::test_support::TempCodexHome::new();
    let server = spawn_server(PollState {
        polls: Arc::new(AtomicUsize::new(0)),
        terminal_error: None,
        slow_down_first: false,
    })
    .await;
    perform_oauth_device_login(
        "synthetic-server",
        "https://resource.example.test/mcp",
        &server,
        http_client(),
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        None,
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
    let stored = crate::stored_oauth_credentials(
        "synthetic-server",
        "https://resource.example.test/mcp",
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )?
    .expect("synthetic credentials reopen from the file store");
    assert_eq!(stored.issuer.as_deref(), Some(server.as_str()));
    assert_eq!(stored.client_id, "synthetic-client");
    assert_eq!(
        stored.token_response.0.refresh_token().unwrap().secret(),
        "synthetic-refresh-token"
    );
    Ok(())
}

#[tokio::test]
async fn device_login_fails_closed_on_denial_and_expiry() {
    for (provider_error, expected) in [("access_denied", "denied"), ("expired_token", "expired")] {
        let server = spawn_server(PollState {
            polls: Arc::new(AtomicUsize::new(0)),
            terminal_error: Some(provider_error),
            slow_down_first: false,
        })
        .await;
        let adapter = OAuthHttpClientAdapter::new_with_max_timeout_and_redirect_mode(
            http_client(),
            build_default_headers(None, None).expect("empty headers"),
            "https://resource.example.test/mcp",
            DEVICE_HTTP_REQUEST_TIMEOUT,
            false,
            StreamableHttpRedirectMode::Legacy,
        )
        .expect("synthetic OAuth client");
        let details = DeviceAuthorizationResponse {
            device_code: "synthetic-device-code".to_string(),
            user_code: "SYNTHETIC-CODE".to_string(),
            verification_uri: "https://login.example.test/device".to_string(),
            verification_uri_complete: None,
            expires_in: Some(30),
            interval: Some(1),
        };
        let error = poll_device_token(
            &adapter,
            &format!("{server}/token"),
            "synthetic-client",
            None,
            &details,
        )
        .await
        .expect_err("terminal provider error must fail");
        assert!(
            error.to_string().contains(expected),
            "unexpected error: {error:#}"
        );
    }
}

#[tokio::test]
async fn device_login_stops_polling_when_the_local_code_expires() {
    let server = spawn_server(PollState {
        polls: Arc::new(AtomicUsize::new(0)),
        terminal_error: None,
        slow_down_first: true,
    })
    .await;
    let adapter = OAuthHttpClientAdapter::new_with_max_timeout_and_redirect_mode(
        http_client(),
        build_default_headers(None, None).expect("empty headers"),
        "https://resource.example.test/mcp",
        DEVICE_HTTP_REQUEST_TIMEOUT,
        false,
        StreamableHttpRedirectMode::Legacy,
    )
    .expect("synthetic OAuth client");
    let details = DeviceAuthorizationResponse {
        device_code: "synthetic-device-code".to_string(),
        user_code: "SYNTHETIC-CODE".to_string(),
        verification_uri: "https://login.example.test/device".to_string(),
        verification_uri_complete: None,
        expires_in: Some(1),
        interval: Some(5),
    };
    let error = poll_device_token(
        &adapter,
        &format!("{server}/token"),
        "synthetic-client",
        None,
        &details,
    )
    .await
    .expect_err("polling cannot continue past local expiry");
    assert!(error.to_string().contains("expired"));
}

#[test]
fn malformed_device_response_is_rejected_without_echoing_payload() {
    let error = serde_json::from_slice::<DeviceAuthorizationResponse>(
        br#"{"device_code":"synthetic-only","user_code":7,"verification_uri":"not a URL"}"#,
    )
    .expect_err("malformed response is rejected");
    assert!(!error.to_string().contains("synthetic-only"));
}
