use super::*;
use axum::Form;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use oauth2::TokenResponse;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::net::TcpListener;

#[derive(Clone)]
struct PollState {
    count: Arc<AtomicUsize>,
    outcome: &'static str,
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
        Form(_form): Form<HashMap<String, String>>,
    ) -> (StatusCode, Json<serde_json::Value>) {
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

    let app = Router::new()
        .route("/device", post(device))
        .route("/token", post(token))
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

fn prompt(_: DeviceAuthorizationPrompt) {}

#[tokio::test(start_paused = true)]
async fn device_login_saves_issuer_bound_tokens_and_reopens_them() -> Result<()> {
    let _home = crate::oauth::test_support::TempCodexHome::new();
    let server = spawn_server(PollState {
        count: Arc::new(AtomicUsize::new(0)),
        outcome: "success",
    })
    .await;
    perform_oauth_device_login(
        "ops",
        &format!("{server}/mcp"),
        &server,
        OAuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
        None,
        None,
        &["ops:read".to_string()],
        Some("synthetic-client"),
        None,
        &format!("{server}/device"),
        &format!("{server}/token"),
        None,
        false,
        false,
        prompt,
    )
    .await?;

    let stored = crate::stored_oauth_credentials(
        "ops",
        &format!("{server}/mcp"),
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
        })
        .await;
        let client = Client::builder()
            .no_proxy()
            .build()
            .expect("synthetic HTTP client");
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
