use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use http::Method;
use http::StatusCode;
use http::header::CONTENT_TYPE;
use rmcp::transport::auth::OAuthHttpRedirectPolicy;
use rmcp::transport::auth::OAuthTokenResponse;
use serde::Deserialize;
use serde::Serialize;
use tokio::time::sleep;
use url::Url;

use crate::StoredOAuthTokens;
use crate::WrappedOAuthTokenResponse;
use crate::http_client_adapter::StreamableHttpRedirectMode;
use crate::oauth::compute_expires_at_millis;
use crate::oauth::save_oauth_tokens;
use crate::oauth_http_client::OAuthHttpClientAdapter;
use crate::utils::build_default_headers;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_exec_server::HttpClient;

const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEFAULT_DEVICE_EXPIRES_IN_SECS: u64 = 900;
const DEFAULT_DEVICE_POLL_INTERVAL_SECS: u64 = 5;
const DEVICE_HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Performs OAuth device authorization using endpoints from one accepted
/// authorization-server metadata discovery result. The caller must pass the
/// issuer and endpoints from the same discovery result. Device and token
/// endpoints may be cross-origin when declared by that metadata; URL safety
/// checks and the no-redirect policy still apply.
#[allow(clippy::too_many_arguments)]
pub async fn perform_oauth_device_login(
    server_name: &str,
    server_url: &str,
    issuer: &str,
    http_client: Arc<dyn HttpClient>,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    scopes: &[String],
    oauth_client_id: Option<&str>,
    oauth_resource: Option<&str>,
    device_authorization_endpoint: &str,
    token_endpoint: &str,
    registration_endpoint: Option<&str>,
    supports_refresh_token: bool,
    prompt: impl FnOnce(DeviceAuthorizationPrompt),
) -> Result<()> {
    validate_device_endpoints(issuer, device_authorization_endpoint, token_endpoint)?;
    let has_configured_headers = http_headers
        .as_ref()
        .is_some_and(|headers| !headers.is_empty())
        || env_http_headers
            .as_ref()
            .is_some_and(|headers| !headers.is_empty());
    let default_headers = build_default_headers(http_headers, env_http_headers)?;
    let http_client = OAuthHttpClientAdapter::new_with_max_timeout_and_redirect_mode(
        http_client,
        default_headers,
        server_url,
        DEVICE_HTTP_REQUEST_TIMEOUT,
        has_configured_headers,
        StreamableHttpRedirectMode::Legacy,
    )
    .context("failed to build OAuth device HTTP client")?;
    let client_id = match oauth_client_id.filter(|client_id| !client_id.trim().is_empty()) {
        Some(client_id) => client_id.trim().to_string(),
        None => {
            let registration_endpoint = registration_endpoint
                .filter(|endpoint| !endpoint.trim().is_empty())
                .ok_or_else(|| anyhow!("OAuth device login requires a configured public OAuth client id because dynamic client registration is unavailable"))?;
            validate_registration_endpoint(issuer, registration_endpoint)?;
            register_device_client(
                &http_client,
                registration_endpoint,
                scopes,
                supports_refresh_token,
            )
            .await?
        }
    };

    let details = request_device_authorization(
        &http_client,
        device_authorization_endpoint,
        &client_id,
        scopes,
        oauth_resource,
    )
    .await?;
    prompt(DeviceAuthorizationPrompt::new(server_name, &details));
    let token_response = poll_device_token(
        &http_client,
        token_endpoint,
        &client_id,
        oauth_resource,
        &details,
    )
    .await?;
    let expires_at = compute_expires_at_millis(&token_response);
    let stored = StoredOAuthTokens {
        server_name: server_name.to_string(),
        url: server_url.to_string(),
        issuer: Some(issuer.to_string()),
        client_id,
        token_response: WrappedOAuthTokenResponse(token_response),
        expires_at,
    };
    save_oauth_tokens(server_name, &stored, store_mode, keyring_backend_kind).await
}

fn validate_device_endpoints(
    issuer: &str,
    device_endpoint: &str,
    token_endpoint: &str,
) -> Result<()> {
    let issuer = issuer.trim();
    if issuer.is_empty() {
        bail!("OAuth device login requires an authorization server issuer");
    }
    let issuer_url =
        Url::parse(issuer).context("OAuth authorization server issuer must be a valid URL")?;
    validate_secure_url(&issuer_url, "authorization server issuer")?;
    for (name, endpoint) in [
        ("device authorization", device_endpoint),
        ("token", token_endpoint),
    ] {
        let endpoint_url = Url::parse(endpoint)
            .with_context(|| format!("OAuth {name} endpoint must be a valid URL"))?;
        validate_secure_url(&endpoint_url, &format!("{name} endpoint"))?;
    }
    Ok(())
}

fn validate_registration_endpoint(issuer: &str, endpoint: &str) -> Result<()> {
    let issuer_url = Url::parse(issuer.trim())
        .context("OAuth authorization server issuer must be a valid URL")?;
    let endpoint_url = Url::parse(endpoint)
        .context("OAuth dynamic client registration endpoint must be a valid URL")?;
    validate_secure_url(&endpoint_url, "dynamic client registration endpoint")?;
    if endpoint_url.origin() != issuer_url.origin() {
        bail!(
            "OAuth dynamic client registration endpoint origin does not match the advertised authorization server issuer"
        );
    }
    Ok(())
}

fn validate_secure_url(url: &Url, name: &str) -> Result<()> {
    let secure = url.scheme() == "https";
    let loopback_http =
        url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "localhost"));
    if !secure && !loopback_http {
        bail!("OAuth {name} must use HTTPS");
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        bail!("OAuth {name} URL must not contain credentials or a fragment");
    }
    Ok(())
}

async fn register_device_client(
    http_client: &OAuthHttpClientAdapter,
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
    let body = serde_json::to_vec(&request)
        .context("failed to encode OAuth device client registration request")?;
    let response = execute_oauth_request(
        http_client,
        registration_endpoint,
        body,
        "application/json",
        DEVICE_HTTP_REQUEST_TIMEOUT,
    )
    .await
    .context("failed to dynamically register OAuth device client")?;
    if !response.status().is_success() {
        bail!(
            "OAuth dynamic client registration failed with HTTP {}",
            response.status()
        );
    }
    let response = serde_json::from_slice::<DeviceClientRegistrationResponse>(response.body())
        .map_err(|_| anyhow!("failed to parse OAuth dynamic client registration response"))?;
    if response.client_id.trim().is_empty() {
        bail!("OAuth dynamic client registration response did not include a client_id");
    }
    Ok(response.client_id)
}

async fn request_device_authorization(
    http_client: &OAuthHttpClientAdapter,
    endpoint: &str,
    client_id: &str,
    scopes: &[String],
    resource: Option<&str>,
) -> Result<DeviceAuthorizationResponse> {
    let mut form = vec![("client_id", client_id.to_string())];
    if !scopes.is_empty() {
        form.push(("scope", scopes.join(" ")));
    }
    if let Some(resource) = resource.filter(|resource| !resource.trim().is_empty()) {
        form.push(("resource", resource.to_string()));
    }
    let response = execute_oauth_request(
        http_client,
        endpoint,
        encode_form(&form),
        "application/x-www-form-urlencoded",
        DEVICE_HTTP_REQUEST_TIMEOUT,
    )
    .await
    .context("failed to request OAuth device authorization")?;
    if !response.status().is_success() {
        return Err(provider_error_from_body(
            response.status(),
            response.body(),
            "device authorization",
        ));
    }
    let details = parse_device_authorization_response(response.body())?;
    if details.device_code.trim().is_empty()
        || details.user_code.trim().is_empty()
        || details.verification_uri.trim().is_empty()
    {
        bail!("OAuth device authorization response omitted a required code or verification URI");
    }
    validate_device_user_code(&details.user_code)?;
    let uri = Url::parse(
        details
            .verification_uri_complete
            .as_deref()
            .unwrap_or(&details.verification_uri),
    )
    .context("OAuth device verification URI must be a valid URL")?;
    validate_secure_url(&uri, "device verification URI")?;
    Ok(details)
}

fn validate_device_user_code(user_code: &str) -> Result<()> {
    if user_code.chars().any(char::is_control) {
        bail!("OAuth device authorization response included an unsafe user code");
    }
    Ok(())
}

fn parse_device_authorization_response(body: &[u8]) -> Result<DeviceAuthorizationResponse> {
    serde_json::from_slice::<DeviceAuthorizationResponse>(body)
        .map_err(|_| anyhow!("failed to parse OAuth device authorization response"))
}

async fn poll_device_token(
    http_client: &OAuthHttpClientAdapter,
    endpoint: &str,
    client_id: &str,
    resource: Option<&str>,
    details: &DeviceAuthorizationResponse,
) -> Result<OAuthTokenResponse> {
    poll_device_token_with_sleep(http_client, endpoint, client_id, resource, details, sleep).await
}

async fn poll_device_token_with_sleep<F, Fut>(
    http_client: &OAuthHttpClientAdapter,
    endpoint: &str,
    client_id: &str,
    resource: Option<&str>,
    details: &DeviceAuthorizationResponse,
    mut sleep: F,
) -> Result<OAuthTokenResponse>
where
    F: FnMut(Duration) -> Fut,
    Fut: Future<Output = ()>,
{
    let expires_in = details.expires_in.unwrap_or(DEFAULT_DEVICE_EXPIRES_IN_SECS);
    if expires_in == 0 {
        bail!("OAuth device authorization response expired immediately");
    }
    let expires_in = expires_in.min(86_400);
    let deadline = Instant::now() + Duration::from_secs(expires_in);
    let grant_lifetime = Duration::from_secs(expires_in);
    let mut interval = bounded_poll_interval(
        details
            .interval
            .unwrap_or(DEFAULT_DEVICE_POLL_INTERVAL_SECS),
        grant_lifetime,
    );
    sleep_until_next_poll(interval, deadline, &mut sleep).await?;
    loop {
        if Instant::now() >= deadline {
            bail!("OAuth device code expired before authorization completed");
        }
        let mut form = vec![
            ("grant_type", DEVICE_CODE_GRANT_TYPE.to_string()),
            ("device_code", details.device_code.clone()),
            ("client_id", client_id.to_string()),
        ];
        if let Some(resource) = resource.filter(|resource| !resource.trim().is_empty()) {
            form.push(("resource", resource.to_string()));
        }
        let timeout =
            DEVICE_HTTP_REQUEST_TIMEOUT.min(deadline.saturating_duration_since(Instant::now()));
        let response = execute_oauth_request(
            http_client,
            endpoint,
            encode_form(&form),
            "application/x-www-form-urlencoded",
            timeout,
        )
        .await
        .context("failed to poll OAuth device token")?;
        if response.status().is_success() {
            return serde_json::from_slice::<OAuthTokenResponse>(response.body())
                .map_err(|_| anyhow!("failed to parse OAuth device token response"));
        }
        let error = parse_provider_error(response.status(), response.body(), "device token")?;
        match error.error.as_str() {
            "authorization_pending" => {
                sleep_until_next_poll(interval, deadline, &mut sleep).await?
            }
            "slow_down" => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                interval = interval_after_slow_down(interval, remaining);
                sleep_until_next_poll(interval, deadline, &mut sleep).await?;
            }
            "expired_token" => bail!("OAuth device code expired before authorization completed"),
            "access_denied" => bail!("OAuth device authorization was denied"),
            _ => {
                return Err(anyhow::Error::new(DeviceProviderError::new(
                    response.status(),
                    "device token",
                    error,
                )));
            }
        }
    }
}

fn bounded_poll_interval(provider_seconds: u64, remaining: Duration) -> Duration {
    Duration::from_secs(provider_seconds.max(1)).min(remaining)
}

fn interval_after_slow_down(interval: Duration, remaining: Duration) -> Duration {
    interval
        .saturating_add(Duration::from_secs(5))
        .min(remaining)
}

async fn execute_oauth_request(
    http_client: &OAuthHttpClientAdapter,
    endpoint: &str,
    body: Vec<u8>,
    content_type: &str,
    timeout: Duration,
) -> Result<oauth2::HttpResponse> {
    let request = http::Request::builder()
        .method(Method::POST)
        .uri(endpoint)
        .header(CONTENT_TYPE, content_type)
        .body(body)
        .context("failed to build OAuth device request")?;
    http_client
        .execute_request(request, OAuthHttpRedirectPolicy::Stop, Some(timeout))
        .await
        .map_err(|error| anyhow!(error.to_string()))
}

fn encode_form(fields: &[(impl AsRef<str>, String)]) -> Vec<u8> {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in fields {
        serializer.append_pair(name.as_ref(), value);
    }
    serializer.finish().into_bytes()
}

async fn sleep_until_next_poll<F, Fut>(
    interval: Duration,
    deadline: Instant,
    sleep: &mut F,
) -> Result<()>
where
    F: FnMut(Duration) -> Fut,
    Fut: Future<Output = ()>,
{
    let now = Instant::now();
    if now >= deadline {
        bail!("OAuth device code expired before authorization completed");
    }
    sleep(interval.min(deadline - now)).await;
    Ok(())
}

fn provider_error_from_body(status: StatusCode, body: &[u8], context: &str) -> anyhow::Error {
    match parse_provider_error(status, body, context) {
        Ok(error) => anyhow::Error::new(DeviceProviderError::new(status, context, error)),
        Err(error) => error,
    }
}

fn parse_provider_error(status: StatusCode, body: &[u8], context: &str) -> Result<DeviceError> {
    serde_json::from_slice::<DeviceError>(body).map_err(|_| {
        anyhow!("OAuth {context} failed with HTTP {status} and an invalid error response")
    })
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

#[derive(Deserialize)]
struct DeviceError {
    error: String,
}

#[derive(Debug)]
struct DeviceProviderError {
    status: StatusCode,
    context: String,
    error_code: &'static str,
}

impl DeviceProviderError {
    fn new(status: StatusCode, context: &str, error: DeviceError) -> Self {
        Self {
            status,
            context: context.to_string(),
            error_code: safe_provider_error_code(&error.error),
        }
    }
}

fn safe_provider_error_code(error: &str) -> &'static str {
    match error {
        "invalid_request" => "invalid_request",
        "invalid_client" => "invalid_client",
        "invalid_grant" => "invalid_grant",
        "unauthorized_client" => "unauthorized_client",
        "unsupported_grant_type" => "unsupported_grant_type",
        "invalid_scope" => "invalid_scope",
        "server_error" => "server_error",
        "temporarily_unavailable" => "temporarily_unavailable",
        _ => "unrecognized provider error",
    }
}

impl fmt::Display for DeviceProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "OAuth {} failed with HTTP {}: {}",
            self.context, self.status, self.error_code
        )
    }
}

impl std::error::Error for DeviceProviderError {}

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
#[path = "perform_oauth_device_login_tests.rs"]
mod tests;
