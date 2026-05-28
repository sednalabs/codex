use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use oauth2::PkceCodeChallenge;
use reqwest::Client;
use reqwest::StatusCode;
use rmcp::transport::auth::OAuthTokenResponse;
use serde::Deserialize;
use tokio::time::sleep;

use crate::StoredOAuthTokens;
use crate::WrappedOAuthTokenResponse;
use crate::oauth::compute_expires_at_millis;
use crate::perform_oauth_login::OAuthProviderError;
use crate::save_oauth_tokens;
use crate::utils::apply_default_headers;
use crate::utils::build_default_headers;
use codex_config::types::OAuthCredentialsStoreMode;

const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEFAULT_DEVICE_EXPIRES_IN_SECS: u64 = 900;
const DEFAULT_DEVICE_POLL_INTERVAL_SECS: u64 = 5;

#[allow(clippy::too_many_arguments)]
pub async fn perform_oauth_device_login(
    server_name: &str,
    server_url: &str,
    store_mode: OAuthCredentialsStoreMode,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    scopes: &[String],
    oauth_client_id: &str,
    oauth_resource: Option<&str>,
    device_authorization_endpoint: &str,
    token_endpoint: &str,
) -> Result<()> {
    let default_headers = build_default_headers(http_headers, env_http_headers)?;
    let http_client = apply_default_headers(Client::builder(), &default_headers).build()?;
    let pkce = DevicePkce::new_random();
    let details = request_device_authorization(
        &http_client,
        device_authorization_endpoint,
        oauth_client_id,
        scopes,
        oauth_resource,
        Some(&pkce),
    )
    .await?;

    print_device_authorization_prompt(server_name, &details);

    let token_response = poll_device_token(
        &http_client,
        token_endpoint,
        oauth_client_id,
        oauth_resource,
        &details,
        Some(&pkce),
    )
    .await?;
    let expires_at = compute_expires_at_millis(&token_response);
    let stored = StoredOAuthTokens {
        server_name: server_name.to_string(),
        url: server_url.to_string(),
        client_id: oauth_client_id.to_string(),
        token_response: WrappedOAuthTokenResponse(token_response),
        expires_at,
    };
    save_oauth_tokens(server_name, &stored, store_mode)
}

async fn request_device_authorization(
    http_client: &Client,
    endpoint: &str,
    client_id: &str,
    scopes: &[String],
    oauth_resource: Option<&str>,
    pkce: Option<&DevicePkce>,
) -> Result<DeviceAuthorizationResponse> {
    let mut form = vec![("client_id", client_id.to_string())];
    if !scopes.is_empty() {
        form.push(("scope", scopes.join(" ")));
    }
    if let Some(resource) = oauth_resource.filter(|value| !value.trim().is_empty()) {
        form.push(("resource", resource.to_string()));
    }
    if let Some(pkce) = pkce {
        form.push(("code_challenge", pkce.code_challenge.clone()));
        form.push(("code_challenge_method", "S256".to_string()));
    }

    let response = http_client
        .post(endpoint)
        .form(&form)
        .send()
        .await
        .context("failed to request OAuth device authorization")?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .context("failed to read OAuth device authorization response")?;
    if !status.is_success() {
        return Err(provider_error_from_body(
            status,
            &body,
            "device authorization",
        ));
    }

    serde_json::from_slice::<DeviceAuthorizationResponse>(&body)
        .context("failed to parse OAuth device authorization response")
}

async fn poll_device_token(
    http_client: &Client,
    token_endpoint: &str,
    client_id: &str,
    oauth_resource: Option<&str>,
    details: &DeviceAuthorizationResponse,
    pkce: Option<&DevicePkce>,
) -> Result<OAuthTokenResponse> {
    let expires_in = details
        .expires_in
        .unwrap_or(DEFAULT_DEVICE_EXPIRES_IN_SECS)
        .max(1);
    let deadline = Instant::now() + Duration::from_secs(expires_in);
    let mut interval = Duration::from_secs(
        details
            .interval
            .unwrap_or(DEFAULT_DEVICE_POLL_INTERVAL_SECS),
    );

    loop {
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "OAuth device code expired before authorization completed"
            ));
        }

        match poll_device_token_once(
            http_client,
            token_endpoint,
            client_id,
            oauth_resource,
            &details.device_code,
            pkce,
        )
        .await?
        {
            DevicePollOutcome::Authorized(token_response) => return Ok(token_response),
            DevicePollOutcome::Pending => sleep_until_next_poll(interval, deadline).await?,
            DevicePollOutcome::SlowDown => {
                interval += Duration::from_secs(5);
                sleep_until_next_poll(interval, deadline).await?;
            }
        }
    }
}

async fn poll_device_token_once(
    http_client: &Client,
    token_endpoint: &str,
    client_id: &str,
    oauth_resource: Option<&str>,
    device_code: &str,
    pkce: Option<&DevicePkce>,
) -> Result<DevicePollOutcome> {
    let mut form = vec![
        ("grant_type", DEVICE_CODE_GRANT_TYPE.to_string()),
        ("device_code", device_code.to_string()),
        ("client_id", client_id.to_string()),
    ];
    if let Some(resource) = oauth_resource.filter(|value| !value.trim().is_empty()) {
        form.push(("resource", resource.to_string()));
    }
    if let Some(pkce) = pkce {
        form.push(("code_verifier", pkce.code_verifier.clone()));
    }

    let response = http_client
        .post(token_endpoint)
        .form(&form)
        .send()
        .await
        .context("failed to poll OAuth device token")?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .context("failed to read OAuth device token response")?;

    if status.is_success() {
        let token_response = serde_json::from_slice::<OAuthTokenResponse>(&body)
            .context("failed to parse OAuth device token response")?;
        return Ok(DevicePollOutcome::Authorized(token_response));
    }

    let error = parse_provider_error(status, &body, "device token")?;
    match error.error.as_str() {
        "authorization_pending" => Ok(DevicePollOutcome::Pending),
        "slow_down" => Ok(DevicePollOutcome::SlowDown),
        "expired_token" => Err(anyhow!(
            "OAuth device code expired before authorization completed"
        )),
        "access_denied" => Err(anyhow!("OAuth device authorization was denied")),
        _ => Err(anyhow::Error::new(OAuthProviderError::new(
            Some(error.error),
            error.error_description,
        ))),
    }
}

async fn sleep_until_next_poll(interval: Duration, deadline: Instant) -> Result<()> {
    let now = Instant::now();
    if now >= deadline {
        return Err(anyhow!(
            "OAuth device code expired before authorization completed"
        ));
    }
    sleep(interval.min(deadline - now)).await;
    Ok(())
}

fn print_device_authorization_prompt(server_name: &str, details: &DeviceAuthorizationResponse) {
    println!(
        "Authorize `{server_name}` by opening this URL in your browser:\n{}\n\nEnter code: {}\n",
        details
            .verification_uri_complete
            .as_deref()
            .unwrap_or(&details.verification_uri),
        details.user_code
    );
}

fn provider_error_from_body(status: StatusCode, body: &[u8], context: &str) -> anyhow::Error {
    match parse_provider_error(status, body, context) {
        Ok(error) => anyhow::Error::new(OAuthProviderError::new(
            Some(error.error),
            error.error_description,
        )),
        Err(err) => err,
    }
}

fn parse_provider_error(status: StatusCode, body: &[u8], context: &str) -> Result<DeviceError> {
    serde_json::from_slice::<DeviceError>(body)
        .with_context(|| format!("OAuth {context} failed with HTTP {status}"))
}

#[derive(Debug, Deserialize)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DeviceError {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

struct DevicePkce {
    code_challenge: String,
    code_verifier: String,
}

impl DevicePkce {
    fn new_random() -> Self {
        let (code_challenge, code_verifier) = PkceCodeChallenge::new_random_sha256();
        Self {
            code_challenge: code_challenge.as_str().to_string(),
            code_verifier: code_verifier.secret().to_string(),
        }
    }
}

enum DevicePollOutcome {
    Authorized(OAuthTokenResponse),
    Pending,
    SlowDown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Form;
    use axum::Json;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::post;
    use oauth2::TokenResponse;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn device_login_polls_until_authorized() {
        let poll_count = Arc::new(AtomicUsize::new(0));
        let server = spawn_device_server(poll_count.clone()).await;
        let client = Client::builder().no_proxy().build().expect("client");
        let details = request_device_authorization(
            &client,
            &format!("{}/device", server),
            "codex-device",
            &["ops:read".to_string(), "ops:write".to_string()],
            None,
            Some(&DevicePkce {
                code_challenge: "pkce-challenge".to_string(),
                code_verifier: "pkce-verifier".to_string(),
            }),
        )
        .await
        .expect("device authorization");

        let token = poll_device_token(
            &client,
            &format!("{server}/token"),
            "codex-device",
            None,
            &details,
            Some(&DevicePkce {
                code_challenge: "pkce-challenge".to_string(),
                code_verifier: "pkce-verifier".to_string(),
            }),
        )
        .await
        .expect("device token");

        assert_eq!(token.access_token().secret(), "access-token");
        assert_eq!(poll_count.load(Ordering::SeqCst), 2);
    }

    async fn spawn_device_server(poll_count: Arc<AtomicUsize>) -> String {
        async fn device(Form(form): Form<HashMap<String, String>>) -> Json<serde_json::Value> {
            assert_eq!(
                form,
                HashMap::from([
                    ("client_id".to_string(), "codex-device".to_string()),
                    ("scope".to_string(), "ops:read ops:write".to_string()),
                    ("code_challenge".to_string(), "pkce-challenge".to_string()),
                    ("code_challenge_method".to_string(), "S256".to_string())
                ])
            );
            Json(json!({
                "device_code": "device-code",
                "user_code": "USER-CODE",
                "verification_uri": "https://issuer.test/device",
                "expires_in": 60,
                "interval": 0
            }))
        }

        async fn token(
            axum::extract::State(poll_count): axum::extract::State<Arc<AtomicUsize>>,
            Form(form): Form<HashMap<String, String>>,
        ) -> (StatusCode, Json<serde_json::Value>) {
            assert_eq!(
                form,
                HashMap::from([
                    ("grant_type".to_string(), DEVICE_CODE_GRANT_TYPE.to_string()),
                    ("device_code".to_string(), "device-code".to_string()),
                    ("client_id".to_string(), "codex-device".to_string()),
                    ("code_verifier".to_string(), "pkce-verifier".to_string())
                ])
            );
            if poll_count.fetch_add(1, Ordering::SeqCst) == 0 {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "authorization_pending"})),
                );
            }
            (
                StatusCode::OK,
                Json(json!({
                    "access_token": "access-token",
                    "refresh_token": "refresh-token",
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "scope": "ops:read ops:write"
                })),
            )
        }

        let router = Router::new()
            .route("/device", post(device))
            .route("/token", post(token))
            .with_state(poll_count);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            axum::serve(listener, router).await.expect("serve");
        });
        format!("http://{addr}")
    }
}
