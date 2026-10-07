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
use oauth2::TokenResponse;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::net::TcpListener;

#[derive(Clone)]
struct PollState {
    polls: Arc<AtomicUsize>,
    poll_errors: Option<Vec<&'static str>>,
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
        let error = state
            .poll_errors
            .as_ref()
            .and_then(|errors| errors.get(poll).copied())
            .or_else(|| {
                (state.poll_errors.is_none() && poll == 0).then_some("authorization_pending")
            });
        if let Some(error) = error {
            return (AxumStatusCode::BAD_REQUEST, Json(json!({"error": error})));
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
fn device_login_requires_a_nonempty_issuer_and_secure_endpoints() {
    assert!(
        validate_device_endpoints(
            "",
            "https://issuer.example/device",
            "https://issuer.example/token"
        )
        .is_err()
    );
    assert!(
        validate_device_endpoints(
            "https://issuer.example",
            "http://issuer.example/device",
            "https://issuer.example/token"
        )
        .is_err()
    );
    assert!(
        validate_device_endpoints(
            "ftp://localhost/issuer",
            "http://localhost/device",
            "http://localhost/token"
        )
        .is_err()
    );
    assert!(
        validate_device_endpoints(
            "https://user:secret@issuer.example",
            "https://issuer.example/device",
            "https://issuer.example/token"
        )
        .is_err()
    );
    validate_device_endpoints(
        "https://issuer.example",
        "https://device.example/device",
        "https://tokens.example/token",
    )
    .expect("issuer metadata may declare secure cross-origin endpoints");
}

#[test]
fn extreme_provider_interval_and_slow_down_are_bounded_by_expiry() {
    let remaining = Duration::from_secs(10);
    let interval = bounded_poll_interval(u64::MAX, remaining);
    assert_eq!(interval, remaining);
    assert_eq!(interval_after_slow_down(interval, remaining), remaining);
    assert_eq!(
        interval_after_slow_down(Duration::from_secs(u64::MAX), Duration::from_secs(1)),
        Duration::from_secs(1)
    );
}

#[tokio::test]
async fn device_login_polls_pending_then_saves_issuer_bound_tokens() -> Result<()> {
    let _home = crate::oauth::test_support::TempCodexHome::new();
    let server = spawn_server(PollState {
        polls: Arc::new(AtomicUsize::new(0)),
        poll_errors: None,
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
async fn device_login_increases_poll_interval_after_slow_down() -> Result<()> {
    let polls = Arc::new(AtomicUsize::new(0));
    let server = spawn_server(PollState {
        polls: polls.clone(),
        poll_errors: Some(vec!["authorization_pending", "slow_down"]),
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
        expires_in: Some(60),
        interval: Some(1),
    };
    let mut requested_sleeps = Vec::new();
    let token = poll_device_token_with_sleep(
        &adapter,
        &format!("{server}/token"),
        "synthetic-client",
        None,
        &details,
        |duration| {
            requested_sleeps.push(duration);
            std::future::ready(())
        },
    )
    .await?;

    assert_eq!(polls.load(Ordering::SeqCst), 3);
    assert_eq!(
        requested_sleeps,
        [
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(6),
        ]
    );
    assert_eq!(token.access_token().secret(), "synthetic-access-token");
    Ok(())
}

#[tokio::test]
async fn device_login_fails_closed_on_denial_and_expiry() {
    for (provider_error, expected) in [("access_denied", "denied"), ("expired_token", "expired")] {
        let server = spawn_server(PollState {
            polls: Arc::new(AtomicUsize::new(0)),
            poll_errors: Some(vec![provider_error]),
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
async fn device_login_does_not_poll_before_an_extreme_provider_interval() {
    let polls = Arc::new(AtomicUsize::new(0));
    let server = spawn_server(PollState {
        polls: polls.clone(),
        poll_errors: None,
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
        interval: Some(u64::MAX),
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
    assert_eq!(polls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unknown_provider_error_is_not_echoed_to_terminal() {
    let provider_error = "\u{1b}[2JPWNED";
    let server = spawn_server(PollState {
        polls: Arc::new(AtomicUsize::new(0)),
        poll_errors: Some(vec![provider_error]),
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
    .expect_err("unknown synthetic provider error must fail");
    let rendered = error.to_string();
    assert!(rendered.contains("unrecognized provider error"));
    assert!(!rendered.contains("PWNED"));
    assert!(!rendered.contains('\u{1b}'));
    let debug = format!("{error:?}");
    assert!(!debug.contains("PWNED"));
    assert!(!debug.contains('\u{1b}'));
}

#[test]
fn malformed_device_response_is_rejected_without_echoing_payload() {
    let error = parse_device_authorization_response(
        br#"{"device_code":"synthetic-only","user_code":7,"verification_uri":"not a URL"}"#,
    )
    .expect_err("malformed response is rejected");
    assert_eq!(
        error.to_string(),
        "failed to parse OAuth device authorization response"
    );
    assert!(!format!("{error:?}").contains("synthetic-only"));
}

#[test]
fn provider_control_characters_in_user_code_are_rejected() {
    let error = validate_device_user_code("ABCD\n\u{1b}[2J")
        .expect_err("provider-supplied terminal controls must not be displayed");
    assert_eq!(
        error.to_string(),
        "OAuth device authorization response included an unsafe user code"
    );
    assert!(!format!("{error:?}").contains("\u{1b}"));
}
