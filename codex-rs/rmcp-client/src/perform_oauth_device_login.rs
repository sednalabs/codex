use std::collections::HashMap;
use std::fmt;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use oauth2::PkceCodeChallenge;
use reqwest::Client;
use reqwest::StatusCode;
use rmcp::transport::AuthorizationManager;
use rmcp::transport::auth::OAuthTokenResponse;
use serde::Deserialize;
use serde::Serialize;
use tokio::time::sleep;

use crate::StoredOAuthTokens;
use crate::WrappedOAuthTokenResponse;
use crate::oauth::compute_expires_at_millis;
use crate::perform_oauth_login::OAuthProviderError;
use crate::save_oauth_tokens_locked;
use crate::utils::build_default_headers;
use crate::utils::build_reqwest_client;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;

const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEFAULT_DEVICE_EXPIRES_IN_SECS: u64 = 900;
const MIN_DEVICE_EXPIRES_IN_SECS: u64 = 1;
const MAX_DEVICE_EXPIRES_IN_SECS: u64 = 86_400;
const DEFAULT_DEVICE_POLL_INTERVAL_SECS: u64 = 5;
const MIN_DEVICE_POLL_INTERVAL_SECS: u64 = 1;
const MAX_ERROR_BODY_PREVIEW_CHARS: usize = 500;

#[allow(clippy::too_many_arguments)]
pub async fn perform_oauth_device_login(
    server_name: &str,
    server_url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    scopes: &[String],
    oauth_client_id: Option<&str>,
    oauth_resource: Option<&str>,
    device_authorization_endpoint: &str,
    token_endpoint: &str,
    mut show_authorization_prompt: impl FnMut(DeviceAuthorizationPrompt),
) -> Result<()> {
    let default_headers = build_default_headers(http_headers, env_http_headers)?;
    let http_client = build_reqwest_client(Client::builder(), &default_headers)?;
    let oauth_client_id =
        resolve_device_oauth_client_id(server_url, &http_client, scopes, oauth_client_id).await?;
    let (details, pkce) = request_device_authorization_with_pkce_fallback(
        &http_client,
        device_authorization_endpoint,
        &oauth_client_id,
        scopes,
        oauth_resource,
    )
    .await?;

    show_authorization_prompt(DeviceAuthorizationPrompt::new(server_name, &details));

    let token_response = match poll_device_token(
        &http_client,
        token_endpoint,
        &oauth_client_id,
        oauth_resource,
        &details,
        pkce.as_ref(),
    )
    .await
    {
        Ok(token_response) => token_response,
        Err(err) if pkce.is_some() && is_invalid_request_provider_error(&err) => {
            let details = request_device_authorization(
                &http_client,
                device_authorization_endpoint,
                &oauth_client_id,
                scopes,
                oauth_resource,
                /*pkce*/ None,
            )
            .await
            .context(
                "OAuth device token endpoint rejected PKCE verifier, and retry without PKCE failed during device authorization",
            )?;
            show_authorization_prompt(DeviceAuthorizationPrompt::new(server_name, &details));
            poll_device_token(
                &http_client,
                token_endpoint,
                &oauth_client_id,
                oauth_resource,
                &details,
                /*pkce*/ None,
            )
            .await
            .context(
                "OAuth device token endpoint rejected PKCE verifier, and retry without PKCE failed",
            )?
        }
        Err(err) => return Err(err),
    };
    let expires_at = compute_expires_at_millis(&token_response);
    let stored = StoredOAuthTokens {
        server_name: server_name.to_string(),
        url: server_url.to_string(),
        client_id: oauth_client_id,
        token_response: WrappedOAuthTokenResponse(token_response),
        expires_at,
    };
    save_oauth_tokens_locked(server_name, &stored, store_mode, keyring_backend_kind).await
}

async fn resolve_device_oauth_client_id(
    server_url: &str,
    http_client: &Client,
    scopes: &[String],
    oauth_client_id: Option<&str>,
) -> Result<String> {
    if let Some(client_id) = oauth_client_id.filter(|client_id| !client_id.trim().is_empty()) {
        return Ok(client_id.trim().to_string());
    }

    let mut auth_manager = AuthorizationManager::new(server_url).await?;
    auth_manager.with_client(http_client.clone())?;
    let metadata = auth_manager.discover_metadata().await?;
    let registration_endpoint = metadata
        .registration_endpoint
        .as_deref()
        .filter(|endpoint| !endpoint.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "OAuth device login requires a configured public OAuth client id because the authorization server does not advertise dynamic client registration."
            )
        })?;
    let supports_refresh_token = metadata_supports_refresh_token(&metadata.additional_fields);

    register_device_oauth_client(
        http_client,
        registration_endpoint,
        scopes,
        supports_refresh_token,
    )
    .await
}

async fn register_device_oauth_client(
    http_client: &Client,
    registration_endpoint: &str,
    scopes: &[String],
    supports_refresh_token: bool,
) -> Result<String> {
    let mut grant_types = vec![DEVICE_CODE_GRANT_TYPE];
    if supports_refresh_token {
        grant_types.push("refresh_token");
    }
    let request = DeviceClientRegistrationRequest {
        client_name: "Codex",
        grant_types,
        token_endpoint_auth_method: "none",
        scope: (!scopes.is_empty()).then(|| scopes.join(" ")),
    };
    let response = http_client
        .post(registration_endpoint)
        .json(&request)
        .send()
        .await
        .context("failed to dynamically register OAuth device client")?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .context("failed to read OAuth dynamic client registration response")?;
    if !status.is_success() {
        return Err(anyhow!(
            "OAuth dynamic client registration failed with HTTP {status}. Response: {}",
            body_preview(&body)
        ));
    }

    let registration = serde_json::from_slice::<DeviceClientRegistrationResponse>(&body)
        .context("failed to parse OAuth dynamic client registration response")?;
    let client_id = registration.client_id.trim();
    if client_id.is_empty() {
        return Err(anyhow!(
            "OAuth dynamic client registration response did not include a client_id"
        ));
    }

    Ok(client_id.to_string())
}

fn metadata_supports_refresh_token(additional_fields: &HashMap<String, serde_json::Value>) -> bool {
    additional_fields
        .get("grant_types_supported")
        .and_then(serde_json::Value::as_array)
        .is_none_or(|grant_types| {
            grant_types
                .iter()
                .any(|grant_type| grant_type.as_str() == Some("refresh_token"))
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAuthorizationPrompt {
    server_name: String,
    verification_uri: String,
    user_code: String,
}

impl DeviceAuthorizationPrompt {
    fn new(server_name: &str, details: &DeviceAuthorizationResponse) -> Self {
        Self {
            server_name: server_name.to_string(),
            verification_uri: details
                .verification_uri_complete
                .as_deref()
                .unwrap_or(&details.verification_uri)
                .to_string(),
            user_code: details.user_code.clone(),
        }
    }

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    pub fn verification_uri(&self) -> &str {
        &self.verification_uri
    }

    pub fn user_code(&self) -> &str {
        &self.user_code
    }
}

async fn request_device_authorization_with_pkce_fallback(
    http_client: &Client,
    endpoint: &str,
    client_id: &str,
    scopes: &[String],
    oauth_resource: Option<&str>,
) -> Result<(DeviceAuthorizationResponse, Option<DevicePkce>)> {
    let mut pkce = Some(DevicePkce::new_random());
    match request_device_authorization(
        http_client,
        endpoint,
        client_id,
        scopes,
        oauth_resource,
        pkce.as_ref(),
    )
    .await
    {
        Ok(details) => Ok((details, pkce)),
        Err(err) if is_invalid_request_provider_error(&err) => {
            pkce = None;
            let details = request_device_authorization(
                http_client,
                endpoint,
                client_id,
                scopes,
                oauth_resource,
                /*pkce*/ None,
            )
            .await
            .context(
                "OAuth device authorization rejected PKCE parameters, and retry without PKCE failed",
            )?;
            Ok((details, pkce))
        }
        Err(err) => Err(err),
    }
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
    let expires_in = device_authorization_expires_in(details);
    let deadline = Instant::now() + Duration::from_secs(expires_in);
    let mut interval = Duration::from_secs(
        details
            .interval
            .unwrap_or(DEFAULT_DEVICE_POLL_INTERVAL_SECS),
    )
    .max(Duration::from_secs(MIN_DEVICE_POLL_INTERVAL_SECS));

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

fn device_authorization_expires_in(details: &DeviceAuthorizationResponse) -> u64 {
    details
        .expires_in
        .unwrap_or(DEFAULT_DEVICE_EXPIRES_IN_SECS)
        .clamp(MIN_DEVICE_EXPIRES_IN_SECS, MAX_DEVICE_EXPIRES_IN_SECS)
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
        _ => Err(anyhow::Error::new(DeviceProviderError::new(
            status,
            "device token",
            error,
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

fn provider_error_from_body(status: StatusCode, body: &[u8], context: &str) -> anyhow::Error {
    match parse_provider_error(status, body, context) {
        Ok(error) => anyhow::Error::new(DeviceProviderError::new(status, context, error)),
        Err(err) => err,
    }
}

fn parse_provider_error(status: StatusCode, body: &[u8], context: &str) -> Result<DeviceError> {
    let body_preview = body_preview(body);
    let error_context =
        format!("OAuth {context} failed with HTTP {status}. Response: {body_preview}");
    serde_json::from_slice::<DeviceError>(body).with_context(|| error_context)
}

fn body_preview(body: &[u8]) -> String {
    let body = String::from_utf8_lossy(body);
    let mut chars = body.chars();
    let mut preview: String = chars.by_ref().take(MAX_ERROR_BODY_PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        preview.push_str("...");
    }
    preview
}

fn is_invalid_request_provider_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<DeviceProviderError>()
        .is_some_and(|err| err.error.error == "invalid_request")
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

#[derive(Debug)]
struct DeviceProviderError {
    status: StatusCode,
    context: String,
    error: DeviceError,
}

impl DeviceProviderError {
    fn new(status: StatusCode, context: &str, error: DeviceError) -> Self {
        Self {
            status,
            context: context.to_string(),
            error,
        }
    }
}

impl fmt::Display for DeviceProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OAuth {} failed with HTTP {}: {}",
            self.context,
            self.status,
            OAuthProviderError::new(
                Some(self.error.error.clone()),
                self.error.error_description.clone()
            )
        )
    }
}

impl std::error::Error for DeviceProviderError {}

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

#[derive(Debug, Serialize)]
struct DeviceClientRegistrationRequest<'a> {
    client_name: &'a str,
    grant_types: Vec<&'static str>,
    token_endpoint_auth_method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceClientRegistrationResponse {
    client_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Form;
    use axum::Json;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::http::header::CONTENT_TYPE;
    use axum::response::IntoResponse;
    use axum::response::Response;
    use axum::routing::get;
    use axum::routing::post;
    use oauth2::TokenResponse;
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use tokio::net::TcpListener;

    fn json_response(status: StatusCode, value: serde_json::Value) -> Response {
        (
            status,
            [(CONTENT_TYPE, "application/json")],
            value.to_string(),
        )
            .into_response()
    }

    #[derive(Clone)]
    struct RejectPkceState {
        device_request_count: Arc<AtomicUsize>,
        poll_count: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    struct RegistrationState {
        registration_count: Arc<AtomicUsize>,
        expected_grant_types: Vec<&'static str>,
    }

    #[tokio::test]
    async fn device_login_dynamic_registration_uses_device_grant_shape() {
        let registration_count = Arc::new(AtomicUsize::new(0));
        let server = spawn_device_registration_server(registration_count.clone()).await;
        let client = Client::builder().no_proxy().build().expect("client");

        let client_id = resolve_device_oauth_client_id(
            &format!("{server}/mcp"),
            &client,
            &["profile".to_string(), "ops:read".to_string()],
            /*oauth_client_id*/ None,
        )
        .await
        .expect("dynamic device registration");

        assert_eq!(client_id, "dynamic-device-client");
        assert_eq!(registration_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn device_login_dynamic_registration_omits_refresh_when_not_supported() {
        let registration_count = Arc::new(AtomicUsize::new(0));
        let server = spawn_device_registration_server_with_grants(
            registration_count.clone(),
            vec![DEVICE_CODE_GRANT_TYPE],
            vec![DEVICE_CODE_GRANT_TYPE],
        )
        .await;
        let client = Client::builder().no_proxy().build().expect("client");

        let client_id = resolve_device_oauth_client_id(
            &format!("{server}/mcp"),
            &client,
            &["profile".to_string(), "ops:read".to_string()],
            /*oauth_client_id*/ None,
        )
        .await
        .expect("dynamic device registration");

        assert_eq!(client_id, "dynamic-device-client");
        assert_eq!(registration_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn device_login_configured_client_id_skips_dynamic_registration() {
        let registration_count = Arc::new(AtomicUsize::new(0));
        let server = spawn_device_registration_server(registration_count.clone()).await;
        let client = Client::builder().no_proxy().build().expect("client");

        let client_id = resolve_device_oauth_client_id(
            &format!("{server}/mcp"),
            &client,
            &["profile".to_string(), "ops:read".to_string()],
            Some("configured-device-client"),
        )
        .await
        .expect("configured client id");

        assert_eq!(client_id, "configured-device-client");
        assert_eq!(registration_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn device_login_polls_until_authorized() {
        let poll_count = Arc::new(AtomicUsize::new(0));
        let server = spawn_device_server(poll_count.clone()).await;
        let client = Client::builder().no_proxy().build().expect("client");
        let details = request_device_authorization(
            &client,
            &format!("{}/device", server),
            "codex-device",
            &["ops:read".to_string(), "ops:write".to_string()],
            /*oauth_resource*/ None,
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
            /*oauth_resource*/ None,
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

    #[tokio::test(start_paused = true)]
    async fn device_login_retries_without_pkce_when_rejected() {
        let device_request_count = Arc::new(AtomicUsize::new(0));
        let poll_count = Arc::new(AtomicUsize::new(0));
        let server =
            spawn_device_server_rejecting_pkce(device_request_count.clone(), poll_count.clone())
                .await;
        let client = Client::builder().no_proxy().build().expect("client");

        let (details, pkce) = request_device_authorization_with_pkce_fallback(
            &client,
            &format!("{}/device", server),
            "codex-device",
            &["ops:read".to_string()],
            /*oauth_resource*/ None,
        )
        .await
        .expect("device authorization");
        assert!(pkce.is_none());

        let token = poll_device_token(
            &client,
            &format!("{server}/token"),
            "codex-device",
            /*oauth_resource*/ None,
            &details,
            pkce.as_ref(),
        )
        .await
        .expect("device token");

        assert_eq!(token.access_token().secret(), "access-token");
        assert_eq!(device_request_count.load(Ordering::SeqCst), 2);
        assert_eq!(poll_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn device_authorization_expiry_uses_safe_bounds() {
        let mut details = DeviceAuthorizationResponse {
            device_code: "device-code".to_string(),
            user_code: "USER-CODE".to_string(),
            verification_uri: "https://issuer.test/device".to_string(),
            verification_uri_complete: None,
            expires_in: None,
            interval: None,
        };

        assert_eq!(
            device_authorization_expires_in(&details),
            DEFAULT_DEVICE_EXPIRES_IN_SECS
        );

        let below_min_expires_in = MIN_DEVICE_EXPIRES_IN_SECS - 1;
        details.expires_in = Some(below_min_expires_in);
        assert_eq!(
            device_authorization_expires_in(&details),
            MIN_DEVICE_EXPIRES_IN_SECS
        );

        details.expires_in = Some(u64::MAX);
        assert_eq!(
            device_authorization_expires_in(&details),
            MAX_DEVICE_EXPIRES_IN_SECS
        );
    }

    async fn spawn_device_registration_server(registration_count: Arc<AtomicUsize>) -> String {
        spawn_device_registration_server_with_grants(
            registration_count,
            vec![DEVICE_CODE_GRANT_TYPE, "refresh_token"],
            vec![DEVICE_CODE_GRANT_TYPE, "refresh_token"],
        )
        .await
    }

    async fn spawn_device_registration_server_with_grants(
        registration_count: Arc<AtomicUsize>,
        grant_types_supported: Vec<&'static str>,
        expected_grant_types: Vec<&'static str>,
    ) -> String {
        async fn register(
            axum::extract::State(state): axum::extract::State<RegistrationState>,
            Json(body): Json<Value>,
        ) -> Response {
            state.registration_count.fetch_add(1, Ordering::SeqCst);
            assert_eq!(body["client_name"], "Codex");
            assert_eq!(body["token_endpoint_auth_method"], "none");
            assert_eq!(body["scope"], "profile ops:read");
            assert_eq!(body["grant_types"], json!(state.expected_grant_types));
            assert!(body.get("redirect_uris").is_none());
            assert!(body.get("response_types").is_none());

            json_response(
                StatusCode::CREATED,
                json!({
                    "client_id": "dynamic-device-client"
                }),
            )
        }

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind metadata listener");
        let addr = listener.local_addr().expect("read metadata listener addr");
        let base_url = format!("http://{addr}");
        let metadata = json!({
            "authorization_endpoint": format!("{base_url}/oauth/authorize"),
            "token_endpoint": format!("{base_url}/token"),
            "registration_endpoint": format!("{base_url}/register"),
            "device_authorization_endpoint": format!("{base_url}/device"),
            "grant_types_supported": grant_types_supported,
            "scopes_supported": ["offline_access"],
        });
        let path_scoped_metadata = metadata.clone();
        let router = Router::new()
            .route(
                "/.well-known/oauth-authorization-server/mcp",
                get(move || {
                    let metadata = path_scoped_metadata.clone();
                    async move { json_response(StatusCode::OK, metadata) }
                }),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(move || {
                    let metadata = metadata.clone();
                    async move { json_response(StatusCode::OK, metadata) }
                }),
            )
            .route("/register", post(register))
            .with_state(RegistrationState {
                registration_count,
                expected_grant_types,
            });

        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("serve oauth metadata");
        });
        base_url
    }

    async fn spawn_device_server(poll_count: Arc<AtomicUsize>) -> String {
        async fn device(Form(form): Form<HashMap<String, String>>) -> Response {
            assert_eq!(
                form,
                HashMap::from([
                    ("client_id".to_string(), "codex-device".to_string()),
                    ("scope".to_string(), "ops:read ops:write".to_string()),
                    ("code_challenge".to_string(), "pkce-challenge".to_string()),
                    ("code_challenge_method".to_string(), "S256".to_string())
                ])
            );
            json_response(
                StatusCode::OK,
                json!({
                    "device_code": "device-code",
                    "user_code": "USER-CODE",
                    "verification_uri": "https://issuer.test/device",
                    "expires_in": 60,
                    "interval": 1
                }),
            )
        }

        async fn token(
            axum::extract::State(poll_count): axum::extract::State<Arc<AtomicUsize>>,
            Form(form): Form<HashMap<String, String>>,
        ) -> Response {
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
                return json_response(
                    StatusCode::BAD_REQUEST,
                    json!({"error": "authorization_pending"}),
                );
            }
            json_response(
                StatusCode::OK,
                json!({
                    "access_token": "access-token",
                    "refresh_token": "refresh-token",
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "scope": "ops:read ops:write"
                }),
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

    async fn spawn_device_server_rejecting_pkce(
        device_request_count: Arc<AtomicUsize>,
        poll_count: Arc<AtomicUsize>,
    ) -> String {
        async fn device(
            axum::extract::State(state): axum::extract::State<RejectPkceState>,
            Form(form): Form<HashMap<String, String>>,
        ) -> Response {
            let request_count = state.device_request_count.fetch_add(1, Ordering::SeqCst);
            if request_count == 0 {
                assert!(form.contains_key("code_challenge"));
                return json_response(
                    StatusCode::BAD_REQUEST,
                    json!({
                        "error": "invalid_request",
                        "error_description": "PKCE is not supported for device authorization"
                    }),
                );
            }
            assert_eq!(
                form,
                HashMap::from([
                    ("client_id".to_string(), "codex-device".to_string()),
                    ("scope".to_string(), "ops:read".to_string())
                ])
            );
            json_response(
                StatusCode::OK,
                json!({
                    "device_code": "device-code",
                    "user_code": "USER-CODE",
                    "verification_uri": "https://issuer.test/device",
                    "expires_in": 60,
                    "interval": 1
                }),
            )
        }

        async fn token(
            axum::extract::State(state): axum::extract::State<RejectPkceState>,
            Form(form): Form<HashMap<String, String>>,
        ) -> Response {
            assert_eq!(
                form,
                HashMap::from([
                    ("grant_type".to_string(), DEVICE_CODE_GRANT_TYPE.to_string()),
                    ("device_code".to_string(), "device-code".to_string()),
                    ("client_id".to_string(), "codex-device".to_string())
                ])
            );
            if state.poll_count.fetch_add(1, Ordering::SeqCst) == 0 {
                return json_response(
                    StatusCode::BAD_REQUEST,
                    json!({"error": "authorization_pending"}),
                );
            }
            json_response(
                StatusCode::OK,
                json!({
                    "access_token": "access-token",
                    "refresh_token": "refresh-token",
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "scope": "ops:read"
                }),
            )
        }

        let router = Router::new()
            .route("/device", post(device))
            .route("/token", post(token))
            .with_state(RejectPkceState {
                device_request_count,
                poll_count,
            });
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
