use std::collections::HashMap;
use std::fmt;
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
use oauth2::HttpRequest;
use oauth2::PkceCodeChallenge;
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
const MAX_ERROR_BODY_PREVIEW_CHARS: usize = 500;
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
    supports_issuer_parameter: bool,
    prompt: impl FnOnce(DeviceAuthorizationPrompt),
) -> Result<()> {
    validate_device_endpoints(
        issuer,
        device_authorization_endpoint,
        token_endpoint,
        supports_issuer_parameter,
    )?;
    let has_configured_headers = http_headers
        .as_ref()
        .is_some_and(|headers| !headers.is_empty())
        || env_http_headers
            .as_ref()
            .is_some_and(|headers| !headers.is_empty());
    let default_headers = build_default_headers(http_headers, env_http_headers)?;
    // OAuth endpoints may be on a different origin from the MCP resource. Keep
    // resource credentials origin-scoped and bound every outbound operation.
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
                .ok_or_else(|| anyhow!("OAuth device login requires a configured public OAuth client id because the authorization server does not advertise dynamic client registration."))?;
            register_device_client(
                &http_client,
                registration_endpoint,
                scopes,
                supports_refresh_token,
            )
            .await?
        }
    };

    let (details, pkce) = request_device_authorization_with_pkce_fallback(
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
        pkce.as_ref(),
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
    supports_issuer_parameter: bool,
) -> Result<()> {
    let issuer = issuer.trim();
    if issuer.is_empty() {
        bail!("OAuth device login requires an authorization server issuer");
    }
    let issuer_url =
        Url::parse(issuer).context("OAuth authorization server issuer must be a valid URL")?;
    if issuer_url.scheme() != "https"
        && issuer_url.host_str() != Some("127.0.0.1")
        && issuer_url.host_str() != Some("localhost")
    {
        bail!("OAuth device login requires a secure authorization server issuer");
    }
    for (name, endpoint) in [
        ("device authorization", device_endpoint),
        ("token", token_endpoint),
    ] {
        let endpoint_url = Url::parse(endpoint)
            .with_context(|| format!("OAuth {name} endpoint must be a valid URL"))?;
        if endpoint_url.scheme() != "https"
            && endpoint_url.host_str() != Some("127.0.0.1")
            && endpoint_url.host_str() != Some("localhost")
        {
            bail!("OAuth {name} endpoint must use HTTPS");
        }
        if !supports_issuer_parameter && endpoint_url.origin() != issuer_url.origin() {
            bail!(
                "OAuth {name} endpoint origin does not match the advertised authorization server issuer"
            );
        }
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
    let request_body = serde_json::to_vec(&request)
        .context("failed to encode OAuth device client registration request")?;
    let response = execute_oauth_request(
        http_client,
        registration_endpoint,
        request_body,
        "application/json",
        DEVICE_HTTP_REQUEST_TIMEOUT,
    )
    .await
    .context("failed to dynamically register OAuth device client")?;
    let status = response.status();
    let body = response.body();
    if !status.is_success() {
        bail!(
            "OAuth dynamic client registration failed with HTTP {status}. Response: {}",
            body_preview(&body)
        );
    }
    let response = serde_json::from_slice::<DeviceClientRegistrationResponse>(body)
        .context("failed to parse OAuth dynamic client registration response")?;
    if response.client_id.trim().is_empty() {
        bail!("OAuth dynamic client registration response did not include a client_id");
    }
    Ok(response.client_id)
}

async fn request_device_authorization_with_pkce_fallback(
    http_client: &OAuthHttpClientAdapter,
    endpoint: &str,
    client_id: &str,
    scopes: &[String],
    resource: Option<&str>,
) -> Result<(DeviceAuthorizationResponse, Option<DevicePkce>)> {
    let pkce = DevicePkce::new_random();
    match request_device_authorization(
        http_client,
        endpoint,
        client_id,
        scopes,
        resource,
        Some(&pkce),
    )
    .await
    {
        Ok(details) => Ok((details, Some(pkce))),
        Err(error) if is_invalid_request_provider_error(&error) => {
            let details = request_device_authorization(http_client, endpoint, client_id, scopes, resource, None)
                .await
                .context("OAuth device authorization rejected PKCE parameters, and retry without PKCE failed")?;
            Ok((details, None))
        }
        Err(error) => Err(error),
    }
}

async fn request_device_authorization(
    http_client: &OAuthHttpClientAdapter,
    endpoint: &str,
    client_id: &str,
    scopes: &[String],
    resource: Option<&str>,
    pkce: Option<&DevicePkce>,
) -> Result<DeviceAuthorizationResponse> {
    let mut form = vec![("client_id", client_id.to_string())];
    if !scopes.is_empty() {
        form.push(("scope", scopes.join(" ")));
    }
    if let Some(resource) = resource.filter(|resource| !resource.trim().is_empty()) {
        form.push(("resource", resource.to_string()));
    }
    if let Some(pkce) = pkce {
        form.push(("code_challenge", pkce.code_challenge.clone()));
        form.push(("code_challenge_method", "S256".to_string()));
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
    let status = response.status();
    let body = response.body();
    if !status.is_success() {
        return Err(provider_error_from_body(
            status,
            body,
            "device authorization",
        ));
    }
    let details = serde_json::from_slice::<DeviceAuthorizationResponse>(body)
        .context("failed to parse OAuth device authorization response")?;
    if details.device_code.trim().is_empty()
        || details.user_code.trim().is_empty()
        || details.verification_uri.trim().is_empty()
    {
        bail!("OAuth device authorization response omitted a required code or verification URI");
    }
    let verification_uri = details
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&details.verification_uri);
    let verification_url = Url::parse(verification_uri)
        .context("OAuth device verification URI must be a valid URL")?;
    if verification_url.scheme() != "https"
        && verification_url.host_str() != Some("127.0.0.1")
        && verification_url.host_str() != Some("localhost")
    {
        bail!("OAuth device verification URI must use HTTPS");
    }
    Ok(details)
}

async fn poll_device_token(
    http_client: &OAuthHttpClientAdapter,
    endpoint: &str,
    client_id: &str,
    resource: Option<&str>,
    details: &DeviceAuthorizationResponse,
    pkce: Option<&DevicePkce>,
) -> Result<OAuthTokenResponse> {
    let expires_in = details
        .expires_in
        .unwrap_or(DEFAULT_DEVICE_EXPIRES_IN_SECS)
        .clamp(1, 86_400);
    let deadline = Instant::now() + Duration::from_secs(expires_in);
    let mut interval = Duration::from_secs(
        details
            .interval
            .unwrap_or(DEFAULT_DEVICE_POLL_INTERVAL_SECS)
            .max(1),
    );
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
        if let Some(pkce) = pkce {
            form.push(("code_verifier", pkce.code_verifier.clone()));
        }
        let request_timeout =
            DEVICE_HTTP_REQUEST_TIMEOUT.min(deadline.saturating_duration_since(Instant::now()));
        let response = execute_oauth_request(
            http_client,
            endpoint,
            encode_form(&form),
            "application/x-www-form-urlencoded",
            request_timeout,
        )
        .await
        .context("failed to poll OAuth device token")?;
        let status = response.status();
        let body = response.body();
        if status.is_success() {
            return serde_json::from_slice::<OAuthTokenResponse>(body)
                .context("failed to parse OAuth device token response");
        }
        let error = parse_provider_error(status, body, "device token")?;
        match error.error.as_str() {
            "authorization_pending" => sleep_until_next_poll(interval, deadline).await?,
            "slow_down" => {
                interval += Duration::from_secs(5);
                sleep_until_next_poll(interval, deadline).await?;
            }
            "expired_token" => bail!("OAuth device code expired before authorization completed"),
            "access_denied" => bail!("OAuth device authorization was denied"),
            _ => {
                return Err(anyhow::Error::new(DeviceProviderError::new(
                    status,
                    "device token",
                    error,
                )));
            }
        }
    }
}

async fn execute_oauth_request(
    http_client: &OAuthHttpClientAdapter,
    endpoint: &str,
    body: Vec<u8>,
    content_type: &str,
    timeout: Duration,
) -> Result<oauth2::HttpResponse> {
    let request = HttpRequest::builder()
        .method(Method::POST)
        .uri(endpoint)
        .header(CONTENT_TYPE, content_type)
        .body(body)
        .context("failed to build OAuth device request")?;
    Ok(http_client
        .execute_request(request, OAuthHttpRedirectPolicy::Stop, Some(timeout))
        .await?)
}

fn encode_form(fields: &[(impl AsRef<str>, String)]) -> Vec<u8> {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in fields {
        serializer.append_pair(name.as_ref(), value);
    }
    serializer.finish().into_bytes()
}

async fn sleep_until_next_poll(interval: Duration, deadline: Instant) -> Result<()> {
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
    serde_json::from_slice::<DeviceError>(body).with_context(|| {
        format!(
            "OAuth {context} failed with HTTP {status}. Response: {}",
            body_preview(body)
        )
    })
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

fn is_invalid_request_provider_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<DeviceProviderError>()
        .is_some_and(|error| error.error.error == "invalid_request")
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
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "OAuth {} failed with HTTP {}: {}{}",
            self.context,
            self.status,
            self.error.error,
            self.error
                .error_description
                .as_deref()
                .map(|description| format!(" ({description})"))
                .unwrap_or_default()
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
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        Self {
            code_challenge: challenge.as_str().to_string(),
            code_verifier: verifier.secret().to_string(),
        }
    }
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
#[path = "perform_oauth_device_login_tests.rs"]
mod tests;
