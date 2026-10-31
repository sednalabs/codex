use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_exec_server::HttpClient;
use codex_exec_server::HttpHeader;
use codex_exec_server::HttpRedirectPolicy;
use codex_exec_server::HttpRequestParams;
use http::HeaderMap;
use http::HeaderValue;
use http::StatusCode;
use http::header::USER_AGENT;
use oauth2::PkceCodeChallenge;
use rmcp::transport::AuthorizationManager;
use rmcp::transport::auth::OAuthTokenResponse;
use serde::Deserialize;
use serde::Serialize;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::timeout_at;

use crate::StoredOAuthTokens;
use crate::WrappedOAuthTokenResponse;
use crate::http_client_adapter::StreamableHttpRedirectMode;
use crate::oauth::compute_expires_at_millis;
use crate::oauth::validate_authorization_server_endpoints;
use crate::oauth_http_client::OAuthHttpClientAdapter;
use crate::oauth_metadata::discover_metadata;
use crate::perform_oauth_login::OAuthProviderError;
use crate::save_oauth_tokens;
use crate::utils::MCP_USER_AGENT;
use crate::utils::build_default_headers;

const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEFAULT_DEVICE_EXPIRES_IN_SECS: u64 = 900;
const MIN_DEVICE_EXPIRES_IN_SECS: u64 = 1;
const MAX_DEVICE_EXPIRES_IN_SECS: u64 = 86_400;
const DEFAULT_DEVICE_POLL_INTERVAL_SECS: u64 = 5;
const MIN_DEVICE_POLL_INTERVAL_SECS: u64 = 1;
const MAX_DEVICE_POLL_INTERVAL_SECS: u64 = 86_400;
const DEVICE_REQUEST_TIMEOUT_MS: u64 = 30_000;
const MAX_DEVICE_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_ERROR_BODY_PREVIEW_CHARS: usize = 500;
static NEXT_DEVICE_REQUEST_ID: AtomicU64 = AtomicU64::new(0);

#[allow(clippy::too_many_arguments)]
pub async fn perform_oauth_device_login(
    server_name: &str,
    server_url: &str,
    http_client: Arc<dyn HttpClient>,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    scopes: &[String],
    oauth_client_id: Option<&str>,
    oauth_resource: Option<&str>,
    issuer: &str,
    device_authorization_endpoint: &str,
    token_endpoint: &str,
    mut show_authorization_prompt: impl FnMut(DeviceAuthorizationPrompt),
) -> Result<()> {
    if issuer.trim().is_empty() {
        bail!("OAuth device login requires issuer-bound authorization metadata");
    }
    validate_oauth_endpoint(device_authorization_endpoint)?;
    validate_oauth_endpoint(token_endpoint)?;
    let has_configured_headers = http_headers
        .as_ref()
        .is_some_and(|headers| !headers.is_empty())
        || env_http_headers
            .as_ref()
            .is_some_and(|headers| !headers.is_empty());
    let default_headers = build_default_headers(http_headers, env_http_headers)?;
    let device_headers =
        oauth_endpoint_headers(server_url, device_authorization_endpoint, &default_headers)?;
    let token_headers = oauth_endpoint_headers(server_url, token_endpoint, &default_headers)?;
    let oauth_client_id = resolve_device_oauth_client_id(
        server_url,
        &http_client,
        &default_headers,
        has_configured_headers,
        scopes,
        oauth_client_id,
        issuer,
    )
    .await?;
    let (details, pkce) = request_device_authorization_with_pkce_fallback(
        http_client.as_ref(),
        &device_headers,
        device_authorization_endpoint,
        &oauth_client_id,
        scopes,
        oauth_resource,
    )
    .await?;

    show_authorization_prompt(DeviceAuthorizationPrompt::new(server_name, &details));

    let token_response = match poll_device_token(
        http_client.as_ref(),
        &token_headers,
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
                http_client.as_ref(),
                &device_headers,
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
                http_client.as_ref(),
                &token_headers,
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

    let stored = StoredOAuthTokens {
        server_name: server_name.to_string(),
        url: server_url.to_string(),
        issuer: Some(issuer.to_string()),
        client_id: oauth_client_id,
        expires_at: compute_expires_at_millis(&token_response),
        token_response: WrappedOAuthTokenResponse(token_response),
    };
    save_oauth_tokens(server_name, &stored, store_mode, keyring_backend_kind).await
}

async fn resolve_device_oauth_client_id(
    server_url: &str,
    http_client: &Arc<dyn HttpClient>,
    default_headers: &HeaderMap,
    has_configured_headers: bool,
    scopes: &[String],
    oauth_client_id: Option<&str>,
    expected_issuer: &str,
) -> Result<String> {
    let adapter = OAuthHttpClientAdapter::new_with_redirect_mode(
        Arc::clone(http_client),
        default_headers.clone(),
        server_url,
        has_configured_headers,
        StreamableHttpRedirectMode::Legacy,
    )?;
    let manager =
        AuthorizationManager::new_with_oauth_http_client(server_url, Arc::new(adapter)).await?;
    let metadata = discover_metadata(&manager).await?;
    if metadata.issuer.as_deref() != Some(expected_issuer) {
        bail!("OAuth authorization server issuer changed during device login");
    }
    validate_authorization_server_endpoints(&metadata)?;
    if let Some(client_id) = oauth_client_id.filter(|client_id| !client_id.trim().is_empty()) {
        return Ok(client_id.trim().to_string());
    }
    let registration_endpoint = metadata
        .registration_endpoint
        .as_deref()
        .filter(|endpoint| !endpoint.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "OAuth device login requires a configured public OAuth client id because the authorization server does not advertise dynamic client registration."
            )
        })?;
    let registration_headers =
        oauth_endpoint_headers(server_url, registration_endpoint, default_headers)?;
    let supports_refresh_token = metadata
        .additional_fields
        .get("grant_types_supported")
        .and_then(serde_json::Value::as_array)
        .is_none_or(|grant_types| {
            grant_types
                .iter()
                .any(|grant_type| grant_type.as_str() == Some("refresh_token"))
        });

    register_device_oauth_client(
        http_client.as_ref(),
        &registration_headers,
        registration_endpoint.trim(),
        scopes,
        supports_refresh_token,
    )
    .await
}

/// Match OAuthHttpClientAdapter's resource-header boundary for direct device
/// and registration POSTs. MCP credentials must not be sent to another origin.
fn oauth_endpoint_headers(
    resource_url: &str,
    endpoint: &str,
    resource_headers: &HeaderMap,
) -> Result<HeaderMap> {
    let resource_origin = url::Url::parse(resource_url)
        .context("MCP resource URL must be valid for OAuth device authorization")?
        .origin();
    let endpoint_origin = validate_oauth_endpoint(endpoint)?.origin();
    if resource_origin == endpoint_origin {
        return Ok(resource_headers.clone());
    }

    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(MCP_USER_AGENT));
    Ok(headers)
}

fn validate_oauth_endpoint(endpoint: &str) -> Result<url::Url> {
    let endpoint = url::Url::parse(endpoint).context("OAuth endpoint must be a valid URL")?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.fragment().is_some()
    {
        bail!("OAuth endpoint must be an HTTP or HTTPS URL without credentials or a fragment");
    }
    Ok(endpoint)
}

async fn register_device_oauth_client(
    http_client: &dyn HttpClient,
    default_headers: &HeaderMap,
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
    let body =
        serde_json::to_vec(&request).context("failed to encode OAuth client registration")?;
    let (status, response_body) = post_bytes(
        http_client,
        default_headers,
        registration_endpoint,
        "application/json",
        body,
    )
    .await
    .context("failed to dynamically register OAuth device client")?;
    if !status.is_success() {
        return Err(anyhow!(
            "OAuth dynamic client registration failed with HTTP {status}. Response: {}",
            body_preview(&response_body)
        ));
    }
    let registration = serde_json::from_slice::<DeviceClientRegistrationResponse>(&response_body)
        .context("failed to parse OAuth dynamic client registration response")?;
    if registration.client_id.trim().is_empty() {
        bail!("OAuth dynamic client registration response did not include a client_id");
    }
    Ok(registration.client_id)
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
    http_client: &dyn HttpClient,
    default_headers: &HeaderMap,
    endpoint: &str,
    client_id: &str,
    scopes: &[String],
    oauth_resource: Option<&str>,
) -> Result<(DeviceAuthorizationResponse, Option<DevicePkce>)> {
    let pkce = Some(DevicePkce::new_random());
    match request_device_authorization(
        http_client,
        default_headers,
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
            let details = request_device_authorization(
                http_client,
                default_headers,
                endpoint,
                client_id,
                scopes,
                oauth_resource,
                /*pkce*/ None,
            )
            .await
            .context("OAuth device authorization rejected PKCE parameters, and retry without PKCE failed")?;
            Ok((details, None))
        }
        Err(err) => Err(err),
    }
}

async fn request_device_authorization(
    http_client: &dyn HttpClient,
    default_headers: &HeaderMap,
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
    let (status, body) = post_form(http_client, default_headers, endpoint, &form).await?;
    if !status.is_success() {
        return Err(provider_error_from_body(
            status,
            &body,
            "device authorization",
        ));
    }
    if body.len() > MAX_DEVICE_RESPONSE_BYTES {
        bail!("OAuth device authorization response exceeded the size limit");
    }
    serde_json::from_slice::<DeviceAuthorizationResponse>(&body)
        .context("failed to parse OAuth device authorization response")
}

async fn poll_device_token(
    http_client: &dyn HttpClient,
    default_headers: &HeaderMap,
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
    .clamp(
        Duration::from_secs(MIN_DEVICE_POLL_INTERVAL_SECS),
        Duration::from_secs(MAX_DEVICE_POLL_INTERVAL_SECS),
    );

    loop {
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "OAuth device code expired before authorization completed"
            ));
        }
        let outcome = timeout_at(
            deadline,
            poll_device_token_once(
                http_client,
                default_headers,
                token_endpoint,
                client_id,
                oauth_resource,
                &details.device_code,
                pkce,
            ),
        )
        .await
        .map_err(|_| anyhow!("OAuth device code expired before authorization completed"))??;
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "OAuth device code expired before authorization completed"
            ));
        }
        match outcome {
            DevicePollOutcome::Authorized(token_response) => return Ok(token_response),
            DevicePollOutcome::Pending => sleep_until_next_poll(interval, deadline).await?,
            DevicePollOutcome::SlowDown => {
                interval = (interval + Duration::from_secs(5))
                    .min(Duration::from_secs(MAX_DEVICE_POLL_INTERVAL_SECS));
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
    http_client: &dyn HttpClient,
    default_headers: &HeaderMap,
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
    let (status, body) = post_form(http_client, default_headers, token_endpoint, &form).await?;
    if status.is_success() {
        if body.len() > MAX_DEVICE_RESPONSE_BYTES {
            bail!("OAuth device token response exceeded the size limit");
        }
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

async fn post_form(
    http_client: &dyn HttpClient,
    default_headers: &HeaderMap,
    endpoint: &str,
    form: &[(&str, String)],
) -> Result<(StatusCode, Vec<u8>)> {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in form {
        serializer.append_pair(key, value);
    }
    post_bytes(
        http_client,
        default_headers,
        endpoint,
        "application/x-www-form-urlencoded",
        serializer.finish().into_bytes(),
    )
    .await
}

async fn post_bytes(
    http_client: &dyn HttpClient,
    default_headers: &HeaderMap,
    endpoint: &str,
    content_type: &str,
    body: Vec<u8>,
) -> Result<(StatusCode, Vec<u8>)> {
    let mut headers = default_headers
        .iter()
        .filter(|(name, _)| name.as_str() != "content-type")
        .map(|(name, value)| {
            Ok(HttpHeader {
                name: name.as_str().to_string(),
                value: value.to_str()?.to_string(),
                value_env_var: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    headers.push(HttpHeader {
        name: "content-type".to_string(),
        value: content_type.to_string(),
        value_env_var: None,
    });
    let deadline = Instant::now() + Duration::from_millis(DEVICE_REQUEST_TIMEOUT_MS);
    let (response, mut response_body) = timeout_at(
        deadline,
        http_client.http_request_stream(HttpRequestParams {
            method: "POST".to_string(),
            url: endpoint.to_string(),
            headers,
            body: Some(body.into()),
            timeout_ms: Some(DEVICE_REQUEST_TIMEOUT_MS),
            redirect_policy: HttpRedirectPolicy::Stop,
            request_id: format!(
                "oauth-device-{}",
                NEXT_DEVICE_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
            ),
            stream_response: true,
        }),
    )
    .await
    .map_err(|_| anyhow!("OAuth device endpoint request timed out"))?
    .context("OAuth device endpoint request failed")?;
    let status = StatusCode::from_u16(response.status).context("invalid OAuth HTTP status")?;
    let body = timeout_at(deadline, async {
        let mut body = Vec::new();
        while let Some(chunk) = response_body.recv().await? {
            if chunk.len() > MAX_DEVICE_RESPONSE_BYTES - body.len() {
                bail!("OAuth device endpoint response exceeded the size limit");
            }
            body.extend_from_slice(&chunk);
        }
        Ok::<_, anyhow::Error>(body)
    })
    .await
    .map_err(|_| anyhow!("OAuth device endpoint request timed out"))??;
    Ok((status, body))
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
#[path = "device_login_boundary_tests.rs"]
mod boundary_tests;
