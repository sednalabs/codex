use std::collections::HashMap;
use std::fmt;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use codex_config::McpServerOAuthConfig;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_exec_server::HttpClient;
use oauth2::PkceCodeChallenge;
use rmcp::transport::AuthorizationManager;
use rmcp::transport::auth::OAuthClientConfig;
use rmcp::transport::auth::OAuthTokenResponse;
use rmcp::transport::auth::OAuthHttpRedirectPolicy;
use serde::Deserialize;
use serde::Serialize;
use tokio::time::sleep;
use url::Url;

use crate::StoredOAuthTokens;
use crate::WrappedOAuthTokenResponse;
use crate::http_client_adapter::StreamableHttpRedirectMode;
use crate::oauth::compute_expires_at_millis;
use crate::oauth::save_oauth_tokens;
use crate::oauth::validate_authorization_server_endpoints;
use crate::oauth_client_credentials::OAuthClientCredentials;
use crate::oauth_client_registration::McpOAuthClientRegistration;
use crate::oauth_http_client::OAuthHttpClientAdapter;
use crate::utils::build_default_headers;

const DEVICE_CODE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEFAULT_DEVICE_EXPIRES_IN_SECS: u64 = 900;
const MIN_DEVICE_EXPIRES_IN_SECS: u64 = 1;
const MAX_DEVICE_EXPIRES_IN_SECS: u64 = 86_400;
const DEFAULT_DEVICE_POLL_INTERVAL_SECS: u64 = 5;
const HTTP_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
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
    oauth_config: Option<&McpServerOAuthConfig>,
    client_registration: McpOAuthClientRegistration,
    oauth_resource: Option<&str>,
    http_client: std::sync::Arc<dyn HttpClient>,
    redirect_mode: StreamableHttpRedirectMode,
    mut show_authorization_prompt: impl FnMut(DeviceAuthorizationPrompt),
) -> Result<()> {
    let configured_headers = http_headers.as_ref().is_some_and(|headers| !headers.is_empty())
        || env_http_headers.as_ref().is_some_and(|headers| !headers.is_empty());
    let default_headers = build_default_headers(http_headers, env_http_headers)?;
    let oauth_http_client = std::sync::Arc::new(OAuthHttpClientAdapter::new_with_redirect_mode(
        http_client, default_headers, server_url, configured_headers, redirect_mode,
    )?);
    let oauth_trait: std::sync::Arc<dyn rmcp::transport::auth::OAuthHttpClient> =
        oauth_http_client.clone();
    let mut auth_manager =
        AuthorizationManager::new_with_oauth_http_client(server_url, oauth_trait).await?;
    auth_manager.set_allow_missing_issuer(true);
    let metadata = auth_manager.resolve_metadata().await?.metadata;
    validate_authorization_server_endpoints(&metadata)?;

    let credentials = OAuthClientCredentials::resolve(oauth_config)?;
    let client_id = match credentials.client_id {
        Some(client_id) => client_id,
        None => resolve_device_client_id(
            &oauth_http_client, &metadata.additional_fields, scopes, client_registration,
        ).await?,
    };
    let mut client_config = OAuthClientConfig::new(&client_id, server_url)
        .with_scopes(scopes.to_vec());
    if let Some(secret) = credentials.client_secret.as_ref() {
        client_config = client_config.with_client_secret(secret.secret());
    }
    auth_manager.configure_client(client_config)?;

    let device_endpoint = metadata_endpoint(&metadata.additional_fields, "device_authorization_endpoint")?;
    validate_oauth_endpoint(&device_endpoint)?;
    let token_endpoint = metadata.token_endpoint.clone();
    validate_oauth_endpoint(&token_endpoint)?;

    let (details, pkce) = request_device_authorization_with_pkce_fallback(
        &oauth_http_client, &device_endpoint, &client_id,
        credentials.client_secret.as_ref().map(|secret| secret.secret()),
        scopes, oauth_resource,
    ).await?;
    show_authorization_prompt(DeviceAuthorizationPrompt::new(server_name, &details));
    let token_response = match poll_device_token(
        &oauth_http_client, &token_endpoint, &client_id,
        credentials.client_secret.as_ref().map(|secret| secret.secret()),
        oauth_resource, &details, pkce.as_ref(),
    ).await {
        Ok(tokens) => tokens,
        Err(error) if pkce.is_some() && is_invalid_request_provider_error(&error) => {
            let details = request_device_authorization(
                &oauth_http_client, &device_endpoint, &client_id,
                credentials.client_secret.as_ref().map(|secret| secret.secret()),
                scopes, oauth_resource, None, None,
            ).await.context("OAuth device token endpoint rejected PKCE verifier, and retry without PKCE failed during device authorization")?;
            show_authorization_prompt(DeviceAuthorizationPrompt::new(server_name, &details));
            poll_device_token(
                &oauth_http_client, &token_endpoint, &client_id,
                credentials.client_secret.as_ref().map(|secret| secret.secret()),
                oauth_resource, &details, None,
            ).await.context("OAuth device token endpoint rejected PKCE verifier, and retry without PKCE failed")?
        }
        Err(error) => return Err(error),
    };

    let stored = StoredOAuthTokens {
        server_name: server_name.to_string(),
        url: server_url.to_string(),
        issuer: metadata.issuer.clone(),
        client_id,
        expires_at: compute_expires_at_millis(&token_response),
        token_response: WrappedOAuthTokenResponse(token_response),
    };
    save_oauth_tokens(server_name, &stored, store_mode, keyring_backend_kind).await
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
            verification_uri: details.verification_uri_complete.as_deref()
                .unwrap_or(&details.verification_uri).to_string(),
            user_code: details.user_code.clone(),
        }
    }
    pub fn server_name(&self) -> &str { &self.server_name }
    pub fn verification_uri(&self) -> &str { &self.verification_uri }
    pub fn user_code(&self) -> &str { &self.user_code }
}

async fn resolve_device_client_id(
    http: &OAuthHttpClientAdapter,
    metadata: &HashMap<String, serde_json::Value>,
    scopes: &[String],
    registration: McpOAuthClientRegistration,
) -> Result<String> {
    if registration == McpOAuthClientRegistration::Cimd {
        bail!("MCP device authorization requires a configured public client ID or Dynamic Client Registration; CIMD is not supported for this device flow");
    }
    let endpoint = metadata_endpoint(metadata, "registration_endpoint")
        .context("OAuth device login requires a configured public client ID because dynamic client registration is not advertised")?;
    validate_oauth_endpoint(&endpoint)?;
    let supports_refresh = metadata.get("grant_types_supported")
        .and_then(serde_json::Value::as_array)
        .is_none_or(|grants| grants.iter().any(|grant| grant.as_str() == Some("refresh_token")));
    let mut grant_types = vec![DEVICE_CODE_GRANT_TYPE];
    if supports_refresh { grant_types.push("refresh_token"); }
    let request = DeviceClientRegistrationRequest {
        client_name: "Codex", grant_types, token_endpoint_auth_method: "none",
        scope: (!scopes.is_empty()).then(|| scopes.join(" ")),
    };
    let response = oauth_post(http, &endpoint, "application/json", serde_json::to_vec(&request)?, None).await?;
    if !response.status().is_success() {
        return Err(provider_error_from_body(response.status(), response.body(), "dynamic client registration"));
    }
    let registration = serde_json::from_slice::<DeviceClientRegistrationResponse>(response.body())
        .context("failed to parse OAuth dynamic client registration response")?;
    let client_id = registration.client_id.trim();
    if client_id.is_empty() { bail!("OAuth dynamic client registration response did not include a client_id"); }
    Ok(client_id.to_string())
}

fn metadata_endpoint(metadata: &HashMap<String, serde_json::Value>, key: &str) -> Result<String> {
    metadata.get(key).and_then(serde_json::Value::as_str)
        .filter(|endpoint| !endpoint.trim().is_empty()).map(str::to_string)
        .ok_or_else(|| anyhow!("OAuth authorization server metadata does not include {key}"))
}
fn validate_oauth_endpoint(endpoint: &str) -> Result<()> {
    let parsed = Url::parse(endpoint).context("OAuth endpoint is not a valid URL")?;
    let loopback_http = parsed.scheme() == "http"
        && matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if parsed.scheme() != "https" && !loopback_http { bail!("OAuth device endpoints must use HTTPS"); }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("OAuth device endpoints must not include URL credentials");
    }
    Ok(())
}

async fn request_device_authorization_with_pkce_fallback(
    http: &OAuthHttpClientAdapter, endpoint: &str, client_id: &str,
    client_secret: Option<&str>, scopes: &[String], resource: Option<&str>,
) -> Result<(DeviceAuthorizationResponse, Option<DevicePkce>)> {
    let pkce = DevicePkce::new_random();
    match request_device_authorization(
        http, endpoint, client_id, client_secret, scopes, resource, Some(&pkce), None,
    ).await {
        Ok(details) => Ok((details, Some(pkce))),
        Err(error) if is_invalid_request_provider_error(&error) => {
            let details = request_device_authorization(
                http, endpoint, client_id, client_secret, scopes, resource, None, None,
            ).await.context("OAuth device authorization rejected PKCE parameters, and retry without PKCE failed")?;
            Ok((details, None))
        }
        Err(error) => Err(error),
    }
}

async fn request_device_authorization(
    http: &OAuthHttpClientAdapter, endpoint: &str, client_id: &str,
    client_secret: Option<&str>, scopes: &[String], resource: Option<&str>,
    pkce: Option<&DevicePkce>, deadline: Option<Instant>,
) -> Result<DeviceAuthorizationResponse> {
    let mut fields = vec![("client_id", client_id.to_string())];
    if let Some(secret) = client_secret { fields.push(("client_secret", secret.to_string())); }
    if !scopes.is_empty() { fields.push(("scope", scopes.join(" "))); }
    if let Some(resource) = resource.filter(|resource| !resource.trim().is_empty()) {
        fields.push(("resource", resource.to_string()));
    }
    if let Some(pkce) = pkce {
        fields.push(("code_challenge", pkce.code_challenge.clone()));
        fields.push(("code_challenge_method", "S256".to_string()));
    }
    let response = oauth_post(
        http, endpoint, "application/x-www-form-urlencoded", form_body(&fields), deadline,
    ).await?;
    if !response.status().is_success() {
        return Err(provider_error_from_body(response.status(), response.body(), "device authorization"));
    }
    let details = serde_json::from_slice::<DeviceAuthorizationResponse>(response.body())
        .context("failed to parse OAuth device authorization response")?;
    validate_verification_uri(details.verification_uri_complete.as_deref().unwrap_or(&details.verification_uri))?;
    Ok(details)
}
fn validate_verification_uri(uri: &str) -> Result<()> {
    let parsed = Url::parse(uri).context("OAuth device verification URI is invalid")?;
    let loopback_http = parsed.scheme() == "http"
        && matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if parsed.scheme() != "https" && !loopback_http { bail!("OAuth device verification URI must use HTTPS"); }
    Ok(())
}

async fn poll_device_token(
    http: &OAuthHttpClientAdapter, endpoint: &str, client_id: &str, client_secret: Option<&str>,
    resource: Option<&str>, details: &DeviceAuthorizationResponse, pkce: Option<&DevicePkce>,
) -> Result<OAuthTokenResponse> {
    let deadline = Instant::now() + Duration::from_secs(device_authorization_expires_in(details));
    let mut interval = Duration::from_secs(details.interval.unwrap_or(DEFAULT_DEVICE_POLL_INTERVAL_SECS).max(1));
    loop {
        let fields = token_form(client_id, client_secret, resource, details, pkce);
        let response = oauth_post(
            http, endpoint, "application/x-www-form-urlencoded", form_body(&fields), Some(deadline),
        ).await?;
        if response.status().is_success() {
            return serde_json::from_slice::<OAuthTokenResponse>(response.body())
                .context("failed to parse OAuth device token response");
        }
        match parse_device_error(response.status(), response.body(), "device token")? {
            error if error.error == "authorization_pending" => sleep_until_next_poll(interval, deadline).await?,
            error if error.error == "slow_down" => {
                interval += Duration::from_secs(5);
                sleep_until_next_poll(interval, deadline).await?;
            }
            error if error.error == "expired_token" => return Err(device_expired_error()),
            error if error.error == "access_denied" => bail!("OAuth device authorization was denied"),
            error => return Err(anyhow::Error::new(DeviceProviderError::new(error))),
        }
    }
}
fn token_form(
    client_id: &str, client_secret: Option<&str>, resource: Option<&str>,
    details: &DeviceAuthorizationResponse, pkce: Option<&DevicePkce>,
) -> Vec<(&'static str, String)> {
    let mut fields = vec![
        ("grant_type", DEVICE_CODE_GRANT_TYPE.to_string()),
        ("device_code", details.device_code.clone()),
        ("client_id", client_id.to_string()),
    ];
    if let Some(secret) = client_secret { fields.push(("client_secret", secret.to_string())); }
    if let Some(resource) = resource.filter(|resource| !resource.trim().is_empty()) {
        fields.push(("resource", resource.to_string()));
    }
    if let Some(pkce) = pkce { fields.push(("code_verifier", pkce.code_verifier.clone())); }
    fields
}
fn device_authorization_expires_in(details: &DeviceAuthorizationResponse) -> u64 {
    details.expires_in.unwrap_or(DEFAULT_DEVICE_EXPIRES_IN_SECS)
        .clamp(MIN_DEVICE_EXPIRES_IN_SECS, MAX_DEVICE_EXPIRES_IN_SECS)
}
async fn sleep_until_next_poll(interval: Duration, deadline: Instant) -> Result<()> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() { return Err(device_expired_error()); }
    sleep(interval.min(remaining)).await;
    if Instant::now() >= deadline { return Err(device_expired_error()); }
    Ok(())
}
async fn oauth_post(
    http: &OAuthHttpClientAdapter, endpoint: &str, content_type: &str, body: Vec<u8>,
    deadline: Option<Instant>,
) -> Result<http::Response<Vec<u8>>> {
    let timeout = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
        .unwrap_or(HTTP_OPERATION_TIMEOUT);
    if timeout.is_zero() { return Err(device_expired_error()); }
    let request = http::Request::builder()
        .method(http::Method::POST).uri(endpoint)
        .header(http::header::CONTENT_TYPE, content_type).body(body)
        .context("failed to build OAuth device request")?;
    let operation = http.execute_request(request, OAuthHttpRedirectPolicy::Stop, Some(timeout));
    tokio::time::timeout(timeout, operation).await
        .map_err(|_| device_expired_error())?
        .map_err(|error| anyhow!("OAuth device request failed: {error}"))
}
fn form_body(fields: &[(&str, String)]) -> Vec<u8> {
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields.iter().map(|(key, value)| (*key, value.as_str())))
        .finish().into_bytes()
}
fn provider_error_from_body(status: http::StatusCode, body: &[u8], context: &str) -> anyhow::Error {
    match parse_device_error(status, body, context) {
        Ok(error) => anyhow::Error::new(DeviceProviderError::new(error)),
        Err(error) => error,
    }
}
fn parse_device_error(status: http::StatusCode, body: &[u8], context: &str) -> Result<DeviceError> {
    let preview = body_preview(body);
    serde_json::from_slice::<DeviceError>(body)
        .with_context(|| format!("OAuth {context} failed with HTTP {status}. Response: {preview}"))
}
fn body_preview(body: &[u8]) -> String {
    let body = String::from_utf8_lossy(body);
    let mut chars = body.chars();
    let mut preview: String = chars.by_ref().take(MAX_ERROR_BODY_PREVIEW_CHARS).collect();
    if chars.next().is_some() { preview.push_str("..."); }
    preview
}
fn is_invalid_request_provider_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<DeviceProviderError>()
        .is_some_and(|error| error.error.error == "invalid_request")
}
fn device_expired_error() -> anyhow::Error {
    anyhow!("OAuth device code expired before authorization completed")
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
struct DeviceProviderError { error: DeviceError }
impl DeviceProviderError { fn new(error: DeviceError) -> Self { Self { error } } }
impl fmt::Display for DeviceProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OAuth provider returned `{}`{}", self.error.error,
            self.error.error_description.as_deref()
                .map(|description| format!(": {description}")).unwrap_or_default())
    }
}
impl std::error::Error for DeviceProviderError {}
struct DevicePkce { code_challenge: String, code_verifier: String }
impl DevicePkce {
    fn new_random() -> Self {
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        Self { code_challenge: challenge.as_str().to_string(), code_verifier: verifier.secret().to_string() }
    }
}
#[derive(Serialize)]
struct DeviceClientRegistrationRequest<'a> {
    client_name: &'a str,
    grant_types: Vec<&'static str>,
    token_endpoint_auth_method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
}
#[derive(Deserialize)]
struct DeviceClientRegistrationResponse { client_id: String }

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn pkce_fallback_retains_resolved_scopes() {
        let scopes = ["ops:read".to_string(), "ops:write".to_string()];
        for pkce in [None, Some(DevicePkce {
            code_challenge: "challenge".into(), code_verifier: "verifier".into(),
        })] {
            let body = form_body(&[
                ("client_id", "client".to_string()),
                ("scope", scopes.join(" ")),
                ("code_challenge_method", "S256".to_string()),
            ]);
            assert!(String::from_utf8(body).unwrap().contains("ops%3Aread+ops%3Awrite"));
            assert_eq!(pkce.is_some(), pkce.is_some());
        }
    }

    #[tokio::test]
    async fn in_flight_poll_is_cancelled_at_device_expiry() {
        struct DropMarker(Arc<AtomicUsize>);
        impl Drop for DropMarker {
            fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let marker = DropMarker(Arc::clone(&dropped));
        let deadline = Instant::now() + Duration::from_millis(10);
        let pending = async move {
            let _marker = marker;
            std::future::pending::<Result<()>>().await
        };
        assert!(tokio::time::timeout_at(deadline.into(), pending).await.is_err());
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}
